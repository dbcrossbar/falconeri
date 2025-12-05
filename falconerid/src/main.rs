#![deny(unsafe_code)]

// Needed for static linking to work right on Linux.
extern crate openssl_sys;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use falconeri_common::{
    db, falconeri_common_version,
    pipeline::PipelineSpec,
    prelude::*,
    rest_api::{
        DatumPatch, DatumReservationRequest, DatumReservationResponse, OutputFilePatch,
    },
    tracing_support::initialize_tracing,
};
use serde::Deserialize;
use std::{convert::TryFrom, env, process::exit};

mod babysitter;
pub(crate) mod inputs;
mod start_job;
mod util;

use crate::babysitter::start_babysitter;
use crate::start_job::{retry_job, run_job};
use crate::util::{AppState, FalconeridError, FalconeridResult, User};

/// initialize the server at startup.
fn initialize_server() -> Result<()> {
    // Print our some information about our environment.
    eprintln!("Running in {}", env::current_dir()?.display());

    // Initialize the database.
    eprintln!("Connecting to database.");
    let mut conn = db::connect(ConnectVia::Cluster)?;
    eprintln!("Running any pending migrations.");
    db::run_pending_migrations(&mut conn)?;
    eprintln!("Finished migrations.");

    eprintln!("Starting babysitter thread to monitor jobs.");
    start_babysitter()?;
    eprintln!("Babysitter started.");

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
    State(state): State<AppState>,
    Json(pipeline_spec): Json<PipelineSpec>,
) -> FalconeridResult<Json<Job>> {
    let pool = state.pool.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        run_job(&pipeline_spec, &mut conn)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
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
    State(state): State<AppState>,
    Query(query): Query<JobNameQuery>,
) -> FalconeridResult<Json<Job>> {
    let pool = state.pool.clone();
    let job_name = query.job_name;
    let job = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        Job::find_by_job_name(&job_name, &mut conn)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(job))
}

/// Look up a job and return it as JSON.
async fn get_job(
    _user: User,
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<Job>> {
    let pool = state.pool.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        Job::find(job_id, &mut conn)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(job))
}

/// Retry a job, and return the new job as JSON.
async fn job_retry(
    _user: User,
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<Job>> {
    let pool = state.pool.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        let job = Job::find(job_id, &mut conn)?;
        retry_job(&job, &mut conn)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(job))
}

/// Reserve the next available datum for a job, and return it along with a list
/// of input files.
async fn job_reserve_next_datum(
    _user: User,
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    Json(request): Json<DatumReservationRequest>,
) -> FalconeridResult<Json<Option<DatumReservationResponse>>> {
    let pool = state.pool.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        let job = Job::find(job_id, &mut conn)?;
        let reserved =
            job.reserve_next_datum(&request.node_name, &request.pod_name, &mut conn)?;
        Ok::<_, Error>(
            reserved.map(|(datum, input_files)| DatumReservationResponse {
                datum,
                input_files,
            }),
        )
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(result))
}

/// Update a datum when it's done.
async fn patch_datum(
    _user: User,
    State(state): State<AppState>,
    Path(datum_id): Path<Uuid>,
    Json(patch): Json<DatumPatch>,
) -> FalconeridResult<Json<Datum>> {
    let pool = state.pool.clone();
    let datum = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        let mut datum = Datum::find(datum_id, &mut conn)?;

        // We only support a few very specific types of patches.
        match &patch {
            // Set status to `Status::Done`.
            DatumPatch {
                status: Status::Done,
                output,
                error_message: None,
                backtrace: None,
            } => {
                datum.mark_as_done(output, &mut conn)?;
            }

            // Set status to `Status::Error`.
            DatumPatch {
                status: Status::Error,
                output,
                error_message: Some(error_message),
                backtrace: Some(backtrace),
            } => {
                datum.mark_as_error(output, error_message, backtrace, &mut conn)?;
            }

            // All other combinations are forbidden.
            other => {
                return Err(format_err!("cannot update datum with {:?}", other));
            }
        }

        // If there are no more datums, mark the job as finished (either done or
        // error).
        datum.update_job_status_if_done(&mut conn)?;

        Ok::<_, Error>(datum)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(datum))
}

/// Create a batch of output files.
///
/// TODO: These include `job_id` and `datum_id` values that might be nicer to
/// move to our URL at some point.
async fn create_output_files(
    _user: User,
    State(state): State<AppState>,
    Json(new_output_files): Json<Vec<NewOutputFile>>,
) -> FalconeridResult<Json<Vec<OutputFile>>> {
    let pool = state.pool.clone();
    let created = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        NewOutputFile::insert_all(&new_output_files, &mut conn)
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(Json(created))
}

/// Update a batch of output files.
async fn patch_output_files(
    _user: User,
    State(state): State<AppState>,
    Json(output_file_patches): Json<Vec<OutputFilePatch>>,
) -> FalconeridResult<StatusCode> {
    let pool = state.pool.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;

        // Separate patches by status.
        let mut done_ids = vec![];
        let mut error_ids = vec![];
        for patch in output_file_patches {
            match patch.status {
                Status::Done => done_ids.push(patch.id),
                Status::Error => error_ids.push(patch.id),
                _ => {
                    return Err(format_err!(
                        "cannot patch output file with {:?}",
                        patch
                    ));
                }
            }
        }

        // Apply our updates.
        conn.transaction(|conn| -> Result<()> {
            OutputFile::mark_ids_as_done(&done_ids, conn)?;
            OutputFile::mark_ids_as_error(&error_ids, conn)?;
            Ok(())
        })?;

        Ok::<_, Error>(())
    })
    .await
    .map_err(|e| FalconeridError(format_err!("task panicked: {}", e)))??;
    Ok(StatusCode::NO_CONTENT)
}

#[tokio::main]
async fn main() -> Result<()> {
    initialize_tracing();
    falconeri_common::init_openssl_probe();

    if let Err(err) = initialize_server() {
        eprintln!(
            "Failed to initialize server:\n{}",
            err.display_causes_and_backtrace()
        );
        exit(1);
    }

    // Set up application state. Use 2x CPU count for pool size to match
    // Rocket's default worker count, which was tested under heavy load.
    let pool = db::pool(
        u32::try_from(num_cpus::get() * 2).unwrap_or(8),
        ConnectVia::Cluster,
    )?;
    let admin_password = db::postgres_password(ConnectVia::Cluster)?;
    let state = AppState {
        pool,
        admin_password,
    };

    // Build our router.
    let app = Router::new()
        .route("/version", get(version))
        .route("/jobs", post(post_job).get(get_job_by_name))
        .route("/jobs/{job_id}", get(get_job))
        .route("/jobs/{job_id}/retry", post(job_retry))
        .route(
            "/jobs/{job_id}/reserve_next_datum",
            post(job_reserve_next_datum),
        )
        .route("/datums/{datum_id}", patch(patch_datum))
        .route(
            "/output_files",
            post(create_output_files).patch(patch_output_files),
        )
        .with_state(state);

    // Start the server.
    eprintln!("Will listen on 0.0.0.0:8089.");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8089").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
