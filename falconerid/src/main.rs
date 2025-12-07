#![deny(unsafe_code)]

use std::env;

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use falconeri_common::{
    db,
    diesel_async::{scoped_futures::ScopedFutureExt, AsyncConnection},
    falconeri_common_version,
    pipeline::PipelineSpec,
    prelude::*,
    rest_api::{
        DatumDescribeResponse, DatumPatch, DatumReservationRequest,
        DatumReservationResponse, JobDescribeResponse, OutputFilePatch,
    },
    tracing_support::initialize_tracing,
};
use serde::Deserialize;
use tower_http::limit::RequestBodyLimitLayer;

mod babysitter;
pub(crate) mod inputs;
mod start_job;
mod util;

use crate::{
    babysitter::start_babysitter,
    start_job::{retry_job, run_job},
    util::{AppState, DbConn, FalconeridError, FalconeridResult, User},
};

/// Initialize the server at startup (run migrations).
#[instrument(level = "debug")]
async fn initialize_server() -> Result<()> {
    // Print our some information about our environment.
    eprintln!("Running in {}", env::current_dir()?.display());

    // Initialize the database and run migrations.
    eprintln!("Connecting to database.");
    let conn = db::async_connect(ConnectVia::Cluster).await?;
    eprintln!("Running any pending migrations.");
    // run_pending_migrations takes ownership and returns the connection.
    // We don't need the connection after migrations, so we can drop it.
    let _conn = db::run_pending_migrations(conn)?;
    eprintln!("Finished migrations.");

    Ok(())
}

/// Return our `falconeri_common` version, which should match the client
/// exactly (for now).
async fn version() -> String {
    falconeri_common_version().to_string()
}

/// Create a new job from a JSON pipeline spec.
async fn post_job(
    _user: User,
    DbConn(mut conn): DbConn,
    Json(pipeline_spec): Json<PipelineSpec>,
) -> FalconeridResult<Json<Job>> {
    let job = run_job(&pipeline_spec, &mut conn).await?;
    Ok(Json(job))
}

/// Query parameters for get_job_by_name.
#[derive(Deserialize)]
struct JobNameQuery {
    job_name: String,
}

/// Look up a job and return it as JSON.
async fn get_job_by_name(
    _user: User,
    DbConn(mut conn): DbConn,
    Query(query): Query<JobNameQuery>,
) -> FalconeridResult<Json<Job>> {
    let job = Job::find_by_job_name(&query.job_name, &mut conn).await?;
    Ok(Json(job))
}

/// List all jobs.
async fn list_jobs(
    _user: User,
    DbConn(mut conn): DbConn,
) -> FalconeridResult<Json<Vec<Job>>> {
    let jobs = Job::list(&mut conn).await?;
    Ok(Json(jobs))
}

/// Look up a job and return it as JSON.
async fn get_job(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<Job>> {
    let job = Job::find(job_id, &mut conn).await?;
    Ok(Json(job))
}

/// Get detailed job information for display.
async fn describe_job(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<JobDescribeResponse>> {
    let job = Job::find(job_id, &mut conn).await?;
    let datum_status_counts = job.datum_status_counts(&mut conn).await?;
    let running_datums = job.datums_with_status(Status::Running, &mut conn).await?;
    let error_datums = job.datums_with_status(Status::Error, &mut conn).await?;
    Ok(Json(JobDescribeResponse {
        job,
        datum_status_counts,
        running_datums,
        error_datums,
    }))
}

/// Retry a job, and return the new job as JSON.
async fn job_retry(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<Job>> {
    let job = Job::find(job_id, &mut conn).await?;
    let new_job = retry_job(&job, &mut conn).await?;
    Ok(Json(new_job))
}

/// Reserve the next available datum for a job, and return it along with a list
/// of input files.
async fn job_reserve_next_datum(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
    Json(request): Json<DatumReservationRequest>,
) -> FalconeridResult<Json<Option<DatumReservationResponse>>> {
    let job = Job::find(job_id, &mut conn).await?;
    let reserved = job
        .reserve_next_datum(&request.node_name, &request.pod_name, &mut conn)
        .await?;
    let result = reserved
        .map(|(datum, input_files)| DatumReservationResponse { datum, input_files });
    Ok(Json(result))
}

/// Update a datum when it's done.
async fn patch_datum(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
    Json(patch): Json<DatumPatch>,
) -> FalconeridResult<Json<Datum>> {
    let mut datum = Datum::find(datum_id, &mut conn).await?;

    // We only support a few very specific types of patches.
    match &patch {
        // Set status to `Status::Done`.
        DatumPatch {
            status: Status::Done,
            output,
            error_message: None,
            backtrace: None,
        } => {
            datum.mark_as_done(output, &mut conn).await?;
        }

        // Set status to `Status::Error`.
        DatumPatch {
            status: Status::Error,
            output,
            error_message: Some(error_message),
            backtrace: Some(backtrace),
        } => {
            datum
                .mark_as_error(output, error_message, backtrace, &mut conn)
                .await?;
        }

        // All other combinations are forbidden.
        other => {
            return Err(FalconeridError(format_err!(
                "cannot update datum with {:?}",
                other
            )));
        }
    }

    // If there are no more datums, mark the job as finished (either done or
    // error).
    datum.update_job_status_if_done(&mut conn).await?;

    Ok(Json(datum))
}

/// Get detailed datum information for display.
async fn describe_datum(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
) -> FalconeridResult<Json<DatumDescribeResponse>> {
    let datum = Datum::find(datum_id, &mut conn).await?;
    let input_files = datum.input_files(&mut conn).await?;
    Ok(Json(DatumDescribeResponse { datum, input_files }))
}

/// Create a batch of output files.
///
/// TODO: These include `job_id` and `datum_id` values that might be nicer to
/// move to our URL at some point.
async fn create_output_files(
    _user: User,
    DbConn(mut conn): DbConn,
    Json(new_output_files): Json<Vec<NewOutputFile>>,
) -> FalconeridResult<Json<Vec<OutputFile>>> {
    let created = NewOutputFile::insert_all(&new_output_files, &mut conn).await?;
    Ok(Json(created))
}

/// Update a batch of output files.
async fn patch_output_files(
    _user: User,
    DbConn(mut conn): DbConn,
    Json(output_file_patches): Json<Vec<OutputFilePatch>>,
) -> FalconeridResult<StatusCode> {
    // Separate patches by status.
    let mut done_ids = vec![];
    let mut error_ids = vec![];
    for patch in output_file_patches {
        match patch.status {
            Status::Done => done_ids.push(patch.id),
            Status::Error => error_ids.push(patch.id),
            _ => {
                return Err(FalconeridError(format_err!(
                    "cannot patch output file with {:?}",
                    patch
                )));
            }
        }
    }

    // Apply our updates.
    conn.transaction(|conn| {
        async move {
            OutputFile::mark_ids_as_done(&done_ids, conn).await?;
            OutputFile::mark_ids_as_error(&error_ids, conn).await?;
            Ok::<_, Error>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[tokio::main]
#[instrument(level = "debug")]
async fn main() -> Result<()> {
    initialize_tracing();
    initialize_server()
        .await
        .context("Failed to initialize server")?;

    // Set up application state. Pool size is configured via environment variable,
    // with defaults matching historical Rocket configuration (32 for production).
    let pool_size: usize = env::var("FALCONERID_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let pool = db::async_pool(pool_size, ConnectVia::Cluster).await?;
    let admin_password = db::postgres_password(ConnectVia::Cluster).await?;

    // Start babysitter tokio task to monitor jobs. Give it its own pool so it
    // can't be starved by heavy API traffic - the babysitter is critical
    // infrastructure for detecting failed jobs and zombie datums.
    //
    // _babysitter_handle must be left in scope as long as this process is running,
    // because a failed babysitter means we need to abort() the whole process.
    eprintln!("Starting babysitter task to monitor jobs.");
    let babysitter_pool = db::async_pool(1, ConnectVia::Cluster).await?;
    let _babysitter_handle = start_babysitter(babysitter_pool);
    eprintln!("Babysitter started.");

    let state = AppState {
        pool,
        admin_password,
    };

    // Build our router.
    let app = Router::new()
        .route("/version", get(version))
        .route("/jobs", post(post_job).get(get_job_by_name))
        .route("/jobs/list", get(list_jobs))
        .route("/jobs/{job_id}", get(get_job))
        .route("/jobs/{job_id}/describe", get(describe_job))
        .route("/jobs/{job_id}/retry", post(job_retry))
        .route(
            "/jobs/{job_id}/reserve_next_datum",
            post(job_reserve_next_datum),
        )
        .route("/datums/{datum_id}", patch(patch_datum))
        .route("/datums/{datum_id}/describe", get(describe_datum))
        .route(
            "/output_files",
            post(create_output_files).patch(patch_output_files),
        )
        // 50 MB limit to match previous Rocket.toml configuration
        .layer(RequestBodyLimitLayer::new(52_428_800))
        .with_state(state);

    // Start the server.
    eprintln!("Will listen on 0.0.0.0:8089.");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8089").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
