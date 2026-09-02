//! A background process which tries to keep an eye on running jobs.
//!
//! We only store state in Postgres, and we assume that:
//!
//! 1. Any process can fail at any time, and
//! 2. **More than one copy of the babysitter will normally be running.**
//!
//! Using PostgreSQL to store state is one of the simplest ways to build a
//! medium-reliability, small-scale distributed job system.
//!
//! Basic strategy:
//!
//! 1. We query for lists of relevant jobs _outside_ a transaction. This
//!    includes both database queries and the names of Kubernetes jobs
//!    meeting certain criteria.
//! 2. We then loop over all candidates, open a transaction for each, and
//!    recheck the original query condition immediately. For Kubernetes
//!    query conditions, which we cannot access atomically, we generally
//!    try to design our code to rely on the fact that certain state
//!    transitions are one-way and cannot be reversed.
//!
//! When we are inside a transaction with a row lock, **we "own" that job.**
//! But another copy of the babysitter may be working from a very similar
//! list of jobs, `falconerid` is talking to `falconeri-worker` instances,
//! and Kubernetes is doing its own thing. So when working on this file,
//! **reason about it like a distributed system.** Which means carefully
//! analyzing how all the components interact.

use std::{panic::AssertUnwindSafe, process, time::Duration};

use falconeri_common::{
    chrono, db,
    diesel_async::{scoped_futures::ScopedFutureExt, AsyncConnection},
    futures_util::FutureExt,
    kubernetes::get_job_info,
    prelude::*,
};

const MISSING_KUBERNETES_JOB_ERROR: &str = "No corresponding Kubernetes job was found";

/// How long we're willing to hold a job's database row lock while asking
/// Kubernetes about that job. Generous relative to a healthy API round trip
/// (on the order of 100ms), but bounds the damage a sick API can do to
/// workers waiting on the same row.
const JOB_INFO_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Check for jobs which should already be marked as finished, which have
/// vanished off the cluster, or which K8s has marked as failed.
#[instrument(skip_all, level = "debug")]
async fn check_for_finished_and_vanished_jobs(
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    let jobs = Job::find_by_status(Status::Running, conn).await?;
    for mut job in jobs {
        conn.transaction(|conn| {
            async move {
                // We may be racing a second copy of the babysitter here, or a
                // request from a worker, so start a transaction, take a lock, and
                // double-check everything before we act on it. This reloads `job`.
                //
                // If we're no longer running, we can bail immediately.
                job.lock_for_update(conn).await?;
                if job.status != Status::Running {
                    debug!("job {} is no longer running; skipping", job.job_name);
                    return Ok(());
                }

                // Check to see if we should have already marked this job as
                // finished. This should normally happen automatically, but if it
                // doesn't, we'll catch it here.
                //
                // This will internally retake the lock and open a nested
                // transaction, but that should be fine.
                job.update_status_if_done(conn).await?;
                if job.status != Status::Running {
                    debug!("job {} finished, nothing more to do", job.job_name);
                    return Ok(());
                }

                // Try to fetch k8s job state. This is a per-job API call made
                // while holding the job's database row lock, which is why it
                // gets an explicit timeout.
                let k8s_info_opt = match get_job_info(&job.job_name, JOB_INFO_TIMEOUT).await {
                    Ok(info) => info,
                    Err(e) => {
                        warn!("could not query Kubernetes for job info: {} (is Kubernetes OK?)", e);
                        return Ok(());
                    }
                };

                // Did we see a job with this name?
                if let Some(k8s_info) = k8s_info_opt {
                    // Check whether Kubernetes has marked this job as failed.
                    if let Some(failure) = &k8s_info.failure {
                        warn!(
                            "job {} failed: {}; setting status to 'error'",
                            job.job_name, failure
                        );
                        job.mark_as_error(&failure.to_string(), conn).await?;
                        return Ok(());
                    }
                } else {
                    // We didn't see the job on Kubernetes.
                    debug!(
                        "no Kubernetes job found for job {}; either it hasn't appeared yet or it has disappeared",
                        job.job_name,
                    );

                    // If the job has been running for a while, but it has no associated
                    // Kubernetes job, assume that either the job has exceeded
                    // `ttlAfterSecondsFinished`, or was manually deleted by someone.
                    let cutoff = Utc::now().naive_utc() - chrono::Duration::minutes(15);
                    if job.created_at < cutoff {
                        warn!(
                            "job {} failed: {}; setting status to 'error'",
                            job.job_name, MISSING_KUBERNETES_JOB_ERROR
                        );
                        job.mark_as_error(MISSING_KUBERNETES_JOB_ERROR, conn)
                            .await?;
                        return Ok(());
                    }
                }

                debug!("job {} looks OK; doing nothing", job.job_name);
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
    // `Datum::zombies` returns datums with no matching pod. This includes ones
    // where the _job_ has errored, both for general cleanup tidiness, and also
    // to prevent edge cases in `retry`.
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
