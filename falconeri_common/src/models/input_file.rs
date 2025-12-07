use diesel_async::RunQueryDsl;
use utoipa::ToSchema;

use crate::{prelude::*, schema::*};

/// An input file which needs to be downloaded to the worker container.
#[derive(
    Associations, Debug, Deserialize, Identifiable, Queryable, Serialize, ToSchema,
)]
#[diesel(belongs_to(Datum, foreign_key = datum_id))]
pub struct InputFile {
    /// The unique ID of this file.
    pub id: Uuid,
    /// When this record was created.
    pub created_at: NaiveDateTime,
    /// The ID of the datum to which this file belongs.
    pub datum_id: Uuid,
    /// The URI from which this file can be downloaded.
    pub uri: String,
    /// The local path to which this file should be downloaded.
    pub local_path: String,
    /// The job to which this input file belongs.
    pub job_id: Uuid,
}

impl InputFile {
    /// Fetch all the input files corresponding to `datums`, returning grouped
    /// in the same order.
    #[instrument(skip_all, level = "trace")]
    pub async fn for_datums(
        datums: &[Datum],
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Vec<InputFile>>> {
        trace!(
            datum_count = datums.len(),
            "fetching input files for datums"
        );
        Ok(InputFile::belonging_to(datums)
            .load(conn)
            .await
            .context("could not load input files belonging to failed datums")?
            .grouped_by(datums))
    }

    /// Generate a sample value for testing.
    pub fn factory(datum: &Datum) -> Self {
        let now = Utc::now().naive_utc();
        InputFile {
            id: Uuid::new_v4(),
            created_at: now,
            datum_id: datum.id,
            uri: "gs://example-bucket/input/file.csv".to_owned(),
            local_path: "/pfs/input/file.csv".to_owned(),
            job_id: datum.job_id,
        }
    }
}

/// Data required to create a new `InputFile`.
#[derive(Debug, Insertable)]
#[diesel(table_name = input_files)]
pub struct NewInputFile {
    /// The ID of the datum to which this file belongs.
    pub datum_id: Uuid,
    /// The URI from which this file can be downloaded.
    pub uri: String,
    /// The local path to which this file should be downloaded.
    pub local_path: String,
    /// The job to which this input file belongs.
    pub job_id: Uuid,
}

impl NewInputFile {
    /// Insert a new job into the database.
    #[instrument(skip_all, level = "trace")]
    pub async fn insert_all(
        input_files: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<()> {
        trace!(
            input_file_count = input_files.len(),
            "inserting input files"
        );
        diesel::insert_into(input_files::table)
            .values(input_files)
            .execute(conn)
            .await
            .context("error inserting input file")?;
        Ok(())
    }
}
