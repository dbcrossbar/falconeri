//! A background process which tries to keep an eye on running jobs.
//!
//! We only store state in Postgres, and we assume that:
//!
//! 1. Any process can fail at any time, and
//! 2. **More than one copy of the babysitter will normally be running.**
//!
//! Using PostgreSQL to store state is one of the simplest ways to build a
//! medium-reliability, small-scale distributed job system.

use std::{panic::AssertUnwindSafe, process, time::Duration};

use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::AsyncConnection;
use falconeri_common::{chrono, db, kubernetes::get_all_job_names, prelude::*};
use futures_util::FutureExt;

/// Spawn a tokio task and run the babysitter in it. This should run indefinitely.
#[instrument(skip_all, level = "trace")]
pub fn start_babysitter(pool: db::AsyncPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // If this task panics, attempt to shut down the entire process, forcing
        // Kubernetes to make noise and restart this `falconerid`. The last thing we
        // want is for the babysitter to silently fail.
        let result = AssertUnwindSafe(run_babysitter(pool)).catch_unwind().await;

        if let Err(err) = result {
            // Extract information about the panic, if it's one of the common types.
            let msg = if let Some(msg) = err.downcast_ref::<&str>() {
                // Created by `panic!("fixed string")`.
                *msg
            } else if let Some(msg) = err.downcast_ref::<String>() {
                // Created by `panic!("format string: {}", "with arguments")`.
                msg
            } else {
                // There's really nothing better we can do here.
                "an unknown panic occurred"
            };

            // Log and print this just in case, so everyone knows what's happening,
            // regardless of whether logs are enabled or where they are sent.
            error!("BABYSITTER PANIC, aborting: {}", msg);
            eprintln!("BABYSITTER PANIC, aborting: {}", msg);
            process::abort();
        }
    })
}

/// Actually run the babysitter.
#[instrument(skip_all, level = "trace")]
async fn run_babysitter(pool: db::AsyncPool) {
    loop {
        // We always want to retry all errors. This way, if PostgreSQL is still
        // starting up, or if someone retarted it, we'll eventually recover.
        if let Err(err) = check_running_jobs(&pool).await {
            error!("error checking running jobs (will retry later): {:?}", err);
        }
        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
    }
}

/// Check our running jobs for various situations we might might need to deal
/// with.
#[instrument(skip_all, level = "debug")]
async fn check_running_jobs(pool: &db::AsyncPool) -> Result<()> {
    let mut conn = pool
        .get()
        .await
        .context("could not get connection from pool")?;
    check_for_finished_and_vanished_jobs(&mut conn).await?;
    check_for_zombie_datums(&mut conn).await?;
    // Note that any datums marked as `Status::Error` by
    // `check_for_zombie_datums` above may then be retried normally by
    // `check_for_datums_which_can_be_rerun` (if they're eligible).
    check_for_datums_which_can_be_rerun(&mut conn).await
}

/// Check for jobs which should already be marked as finished, or which have
/// vanished off the cluster.
#[instrument(skip_all, level = "debug")]
async fn check_for_finished_and_vanished_jobs(
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    let jobs = Job::find_by_status(Status::Running, conn).await?;
    let all_job_names = get_all_job_names().await?;
    for mut job in jobs {
        let all_job_names = &all_job_names;
        conn.transaction(|conn| {
            async move {
                // We may be racing a second copy of the babysitter here, or a
                // request from a worker, so start a transaction, take a lock, and
                // double-check everything before we act on it.
                job.lock_for_update(conn).await?;

                // Check to see if we should have already marked this job as
                // finished. This should normally happen automatically, but if it
                // doesn't, we'll catch it here.
                //
                // This will internally retake the lock and open a nested a
                // transaction, but that should be fine.
                job.update_status_if_done(conn).await?;

                // If the job has been running for a while, but it has no associated
                // Kubernetes job, assume that either the job has exceeded
                // `ttlAfterSecondsFinished`, or was manually deleted by someone.
                let cutoff = Utc::now().naive_utc() - chrono::Duration::minutes(15);
                if job.status == Status::Running
                    && job.created_at < cutoff
                    && !all_job_names.contains(&job.job_name)
                {
                    warn!("job {} is running but has no corresponding Kubernetes job, setting status to 'error'", job.job_name);
                    job.mark_as_error(conn).await?;
                }
                Ok::<_, Error>(())
            }
            .scope_boxed()
        })
        .await?;
    }
    Ok(())
}

/// Check for datums which claim to be running in a pod that no longer exists.
#[instrument(skip_all, level = "debug")]
async fn check_for_zombie_datums(conn: &mut AsyncPgConnection) -> Result<()> {
    let zombies = Datum::zombies(conn).await?;
    for mut zombie in zombies {
        let zombie_id = zombie.id;
        let job_id = zombie.job_id;
        // We may be racing a second copy of the babysitter here, so start a
        // transaction, take a lock, and double-check that our status is still
        // `Status::Running`.
        conn.transaction(|conn| {
            async move {
                zombie.lock_for_update(conn).await?;
                if zombie.status == Status::Running {
                    warn!(
                        "found zombie datum {}, which was supposed to be running on pod {:?}",
                        zombie.id, zombie.pod_name
                    );
                    zombie
                        .mark_as_error(
                            "(did not capture output)",
                            "worker pod disappeared while working on datum",
                            "(no backtrace available)",
                            conn,
                        )
                        .await?;
                } else {
                    warn!("someone beat us to zombie datum {}", zombie.id);
                }
                Ok::<_, Error>(())
            }
            .scope_boxed()
        })
        .await?;
        // If there are no more datums, mark the job as finished (either
        // done or error). We need to look up the job again since `zombie` was
        // moved into the transaction.
        let mut job = Job::find(job_id, conn).await?;
        job.update_status_if_done(conn).await?;
        debug!("finished processing zombie datum {}", zombie_id);
    }
    Ok(())
}

/// Check for datums which are in the error state but which are eligible for
/// retries.
#[instrument(skip_all, level = "debug")]
async fn check_for_datums_which_can_be_rerun(
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    let rerunable_datums = Datum::rerunable(conn).await?;
    for mut datum in rerunable_datums {
        // We may be racing a second copy of the babysitter here, so start a
        // transaction, take a lock, and double-check that we're still eligible
        // for a re-run.
        conn.transaction(|conn| {
            async move {
                // Mark our datum as re-runnable.
                datum.lock_for_update(conn).await?;
                if datum.is_rerunable() {
                    warn!(
                        "rescheduling errored datum {} (previously on try {}/{})",
                        datum.id,
                        datum.attempted_run_count,
                        datum.maximum_allowed_run_count
                    );
                    datum.mark_as_eligible_for_rerun(conn).await?;
                } else {
                    warn!("someone beat us to rerunable datum {}", datum.id);
                }

                // Remove `OutputFile` records for this datum, so we can upload the
                // same output files again.
                //
                // TODO: Unfortunately, there's an issue here. It takes one of two
                // forms:
                //
                // 1. Workers use deterministic file names. In this case, we
                //    _should_ be fine, because we'll just overwrite any files we
                //    did manage to upload.
                // 2. Workers use random filenames. Here, there are two subcases: a.
                //    We have successfully created an `OutputFile` record. b. We
                //    have yet to create an `OutputFile` record.
                //
                // We need to fix (2b) by pre-creating all our `OutputFile` records
                // _before_ uploading, and then updating them later to show that the
                // output succeeded. Which them into case (2a). And then we can fix (2a)
                // by deleting any S3/GCS files corresponding to `OutputFile::uri`.
                OutputFile::delete_for_datum(&datum, conn).await?;
                Ok::<_, Error>(())
            }
            .scope_boxed()
        })
        .await?;
    }
    Ok(())
}
