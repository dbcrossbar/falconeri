use std::fmt;

use diesel_async::RunQueryDsl;
use utoipa::ToSchema;

use crate::{kubernetes, prelude::*, schema::*};

/// Error type for datum ownership verification.
#[derive(Debug)]
pub enum DatumOwnershipError {
    /// The datum was not found.
    NotFound(Uuid),
    /// The pod does not own the datum (possible zombie worker).
    NotOwned {
        /// The datum ID.
        datum_id: Uuid,
        /// The pod that claimed ownership.
        expected_pod: String,
        /// The pod that actually owns the datum (if any).
        actual_pod: Option<String>,
    },
}

impl fmt::Display for DatumOwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatumOwnershipError::NotFound(id) => {
                write!(f, "datum {} not found", id)
            }
            DatumOwnershipError::NotOwned {
                datum_id,
                expected_pod,
                actual_pod,
            } => {
                write!(
                    f,
                    "datum {} is owned by {:?}, not {}",
                    datum_id, actual_pod, expected_pod
                )
            }
        }
    }
}

impl std::error::Error for DatumOwnershipError {}

/// A single chunk of work, consisting of one or more files.
#[derive(
    Associations, Debug, Deserialize, Identifiable, Queryable, Serialize, ToSchema,
)]
#[diesel(belongs_to(Job, foreign_key = job_id))]
pub struct Datum {
    /// The unique ID of this datum.
    pub id: Uuid,
    /// When this datum was created.
    pub created_at: NaiveDateTime,
    /// When this job was last updated.
    pub updated_at: NaiveDateTime,
    /// The current status of this datum.
    pub status: Status,
    /// The job to which this datum belongs.
    pub job_id: Uuid,
    /// An error message associated with this datum, if any.
    pub error_message: Option<String>,
    /// The Kubernetes node on which this job is running / was run.
    pub node_name: Option<String>,
    /// The Kubernetes pod which is running / ran this job.
    pub pod_name: Option<String>,
    /// The backtrace associated with `error_message`, if any.
    pub backtrace: Option<String>,
    /// Combined stdout and stderr of the code which processed the datum.
    pub output: Option<String>,
    /// How many times have we tried to process this datum (counting attempts in
    /// progress)?
    pub attempted_run_count: i32,
    /// How many times are we allowed to attempt to process this datum before
    /// failing for good?
    ///
    /// We store this on the `datum`, not the `job`, because (1) it simplifies
    /// several queries, and (2) it gives us the option of allowing extra
    /// retries on a particular datum someday.
    pub maximum_allowed_run_count: i32,
}

impl Datum {
    /// Find a datum by ID.
    #[instrument(skip_all, fields(id = %id), level = "trace")]
    pub async fn find(id: Uuid, conn: &mut AsyncPgConnection) -> Result<Datum> {
        datums::table
            .find(id)
            .first(conn)
            .await
            .with_context(|| format!("could not load datum {}", id))
    }

    /// Find all datums with the specified status that belong to a running job.
    #[instrument(skip_all, fields(status = %status), level = "trace")]
    pub async fn active_with_status(
        status: Status,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Datum>> {
        let datums = datums::table
            .inner_join(jobs::table)
            .filter(jobs::status.eq(Status::Running))
            .filter(datums::status.eq(status))
            .select(datums::all_columns)
            .load::<Datum>(conn)
            .await
            .with_context(|| {
                format!("could not load datums with status {}", status)
            })?;
        Ok(datums)
    }

    /// Find datums which claim to be running, but whose `pod_name` points to a
    /// non-existant pod.
    #[instrument(skip_all, level = "trace")]
    pub async fn zombies(conn: &mut AsyncPgConnection) -> Result<Vec<Datum>> {
        let running = Self::active_with_status(Status::Running, conn).await?;
        trace!("running datums: {:?}", running);
        let running_pod_names = kubernetes::get_running_pod_names().await?;
        Ok(running
            .into_iter()
            .filter(|datum| match &datum.pod_name {
                Some(pod_name) => !running_pod_names.contains(pod_name),
                None => {
                    warn!("datum {} has status=\"running\" but no pod_name", datum.id);
                    true
                }
            })
            .collect::<Vec<_>>())
    }

    /// Find all datums which have errored, but that we can re-run.
    ///
    /// This will only return datums associated with running jobs.
    #[instrument(skip_all, level = "trace")]
    pub async fn rerunable(conn: &mut AsyncPgConnection) -> Result<Vec<Datum>> {
        let datums = datums::table
            .inner_join(jobs::table)
            .filter(jobs::status.eq(Status::Running))
            .filter(datums::status.eq(Status::Error))
            .filter(datums::attempted_run_count.lt(datums::maximum_allowed_run_count))
            .select(datums::all_columns)
            .load::<Datum>(conn)
            .await
            .context("could not load rerunable datums")?;
        debug!("found {} re-runable jobs", datums.len());
        Ok(datums)
    }

    /// Is this datum re-runable, assuming it belongs to a running job?
    ///
    /// The logic here should mirror [`Datum::rerunnable`] above, except we
    /// don't check the job status. We use this to double-check the results of
    /// `Self::rerunnable` _after_ loading them and locking an individual
    /// `Datum`. We do this to prevent holding locks on more than one `Datum`.
    pub fn is_rerunable(&self) -> bool {
        self.status == Status::Error
            && self.attempted_run_count < self.maximum_allowed_run_count
    }

    /// Get the input files for this datum.
    #[instrument(skip_all, fields(datum = %self.id), level = "trace")]
    pub async fn input_files(
        &self,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<InputFile>> {
        InputFile::belonging_to(self)
            .order_by(input_files::created_at)
            .load(conn)
            .await
            .context("could not load input file")
    }

    /// Lock the underying database row using `SELECT FOR UPDATE`. Must be
    /// called from within a transaction.
    #[instrument(skip_all, fields(datum = %self.id), level = "trace")]
    pub async fn lock_for_update(
        &mut self,
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        *self = datums::table
            .find(self.id)
            .for_update()
            .first(conn)
            .await
            .with_context(|| format!("could not load datum {}", self.id))?;
        Ok(())
    }

    /// Lock this datum and verify the requesting pod owns it.
    ///
    /// Returns the locked datum if ownership matches, or an error if:
    /// - The datum doesn't exist
    /// - The pod_name doesn't match (zombie worker detected)
    ///
    /// Must be called within a transaction.
    #[instrument(skip_all, fields(datum = %id, pod_name = %pod_name), level = "trace")]
    pub async fn lock_and_verify_owner(
        id: Uuid,
        pod_name: &str,
        conn: &mut AsyncPgConnection,
    ) -> Result<Datum, DatumOwnershipError> {
        let datum: Datum = datums::table
            .find(id)
            .for_update()
            .first(conn)
            .await
            .map_err(|_| DatumOwnershipError::NotFound(id))?;

        if datum.pod_name.as_deref() != Some(pod_name) {
            error!(
                datum = %id,
                pod_name = %pod_name,
                conflicting_pod_name = ?datum.pod_name,
                "Pod ownership mismatch - possible zombie worker"
            );
            return Err(DatumOwnershipError::NotOwned {
                datum_id: id,
                expected_pod: pod_name.to_string(),
                actual_pod: datum.pod_name.clone(),
            });
        }

        Ok(datum)
    }

    /// Mark this datum as having been successfully processed.
    #[instrument(skip_all, fields(datum = %self.id), level = "trace")]
    pub async fn mark_as_done(
        &mut self,
        output: &str,
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        let now = Utc::now().naive_utc();
        *self = diesel::update(datums::table.filter(datums::id.eq(&self.id)))
            .set((
                datums::updated_at.eq(now),
                datums::status.eq(&Status::Done),
                datums::output.eq(output),
            ))
            .get_result(conn)
            .await
            .context("can't mark datum as done")?;
        Ok(())
    }

    /// Mark this datum as having been unsuccessfully processed.
    #[instrument(skip_all, fields(datum = %self.id), level = "trace")]
    pub async fn mark_as_error(
        &mut self,
        output: &str,
        error_message: &str,
        backtrace: &str,
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        let now = Utc::now().naive_utc();
        *self = diesel::update(datums::table.filter(datums::id.eq(&self.id)))
            .set((
                datums::updated_at.eq(now),
                datums::status.eq(&Status::Error),
                datums::output.eq(output),
                datums::error_message.eq(&error_message),
                datums::backtrace.eq(&backtrace),
            ))
            .get_result(conn)
            .await
            .context("can't mark datum as having failed")?;
        Ok(())
    }

    /// Mark this datum as eligible to be re-run another time.
    ///
    /// We assume that the datum's row is locked by `lock_for_update` when we
    /// are called.
    #[instrument(skip_all, fields(datum = %self.id), level = "trace")]
    pub async fn mark_as_eligible_for_rerun(
        &mut self,
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        let now = Utc::now().naive_utc();
        *self = diesel::update(datums::table.filter(datums::id.eq(&self.id)))
            .set((
                datums::updated_at.eq(now),
                datums::status.eq(&Status::Ready),
                // Don't do this here! This is done when we start running in
                // `actually_reserve_next_datum`.
                //
                // datums::attempted_run_count.eq(self.attempted_run_count + 1),
            ))
            .get_result(conn)
            .await
            .context("can't mark datum as eligible")?;
        Ok(())
    }

    /// Update the status of our associate job, if it has finished.
    ///
    /// This calls [`Job::update_status_if_done`].
    #[instrument(skip_all, fields(datum = %self.id, job = %self.job_id), level = "trace")]
    pub async fn update_job_status_if_done(
        &self,
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        let mut job = Job::find(self.job_id, conn).await?;
        job.update_status_if_done(conn).await
    }

    /// Generate a sample value for testing.
    pub fn factory(job: &Job) -> Self {
        let now = Utc::now().naive_utc();
        Datum {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            status: Status::Running,
            job_id: job.id,
            error_message: None,
            node_name: None,
            pod_name: None,
            backtrace: None,
            output: None,
            attempted_run_count: 0,
            maximum_allowed_run_count: 1,
        }
    }
}

/// Data required to create a new `Datum`.
#[derive(Debug, Insertable)]
#[diesel(table_name = datums)]
pub struct NewDatum {
    /// The unique ID of this datum. This must be generated by the caller and
    /// supplied at creation time so that it can be immediately used for the
    /// associated `InputFiles` without first needing to insert this record and
    /// pay round-trip costs.
    pub id: Uuid,
    /// The job to which this datum belongs.
    pub job_id: Uuid,
    /// How many times are we allowed to attempt to process this datum before
    /// failing for good?
    pub maximum_allowed_run_count: i32,
}

impl NewDatum {
    /// Insert new datums into the database.
    #[instrument(skip_all, level = "trace")]
    pub async fn insert_all(
        datums: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        trace!(datum_count = datums.len(), "inserting datums");
        diesel::insert_into(datums::table)
            .values(datums)
            .execute(conn)
            .await
            .context("error inserting datums")?;
        Ok(())
    }
}
