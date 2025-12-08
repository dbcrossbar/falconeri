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
        CreateJobRequest, CreateOutputFilesRequest, DatumDescribeResponse, DatumPatch,
        DatumReservationRequest, DatumReservationResponse, DatumResponse,
        JobDescribeResponse, JobResponse, JobsResponse, OutputFilesResponse,
        UpdateDatumRequest, UpdateOutputFilesRequest,
    },
    tracing_support::initialize_tracing,
};
use serde::Deserialize;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use utoipa::OpenApi;

mod babysitter;
pub(crate) mod inputs;
mod start_job;
mod util;

use crate::{
    babysitter::start_babysitter,
    start_job::{retry_job, run_job},
    util::{AppState, DbConn, FalconeridError, FalconeridResult, User},
};

/// OpenAPI specification for CLI-facing endpoints.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Falconerid API",
        version = "2.0.0-alpha",
        description = "REST API for the Falconeri distributed job runner"
    ),
    paths(
        version,
        post_job,
        get_job_by_name,
        list_jobs,
        get_job,
        describe_job,
        job_retry,
        describe_datum,
    ),
    components(schemas(
        Job,
        Datum,
        DatumStatusCount,
        InputFile,
        Status,
        JobDescribeResponse,
        DatumDescribeResponse,
        PipelineSpec,
        falconeri_common::pipeline::Pipeline,
        falconeri_common::pipeline::Transform,
        falconeri_common::pipeline::ParallelismSpec,
        falconeri_common::pipeline::ResourceRequests,
        falconeri_common::pipeline::Input,
        falconeri_common::pipeline::Glob,
        falconeri_common::pipeline::Egress,
        falconeri_common::secret::Secret,
    ))
)]
struct ApiDoc;

/// Return the OpenAPI JSON specification.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

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
///
/// Used by: CLI, Worker
#[utoipa::path(
    get,
    path = "/version",
    responses(
        (status = 200, description = "Server version", body = String)
    )
)]
async fn version() -> String {
    falconeri_common_version().to_string()
}

/// Create a new job from a JSON pipeline spec.
///
/// Used by: CLI (job run)
#[utoipa::path(
    post,
    path = "/jobs",
    request_body = CreateJobRequest,
    responses(
        (status = 200, description = "Job created successfully", body = JobResponse)
    )
)]
async fn post_job(
    _user: User,
    DbConn(mut conn): DbConn,
    Json(request): Json<CreateJobRequest>,
) -> FalconeridResult<Json<JobResponse>> {
    let job = run_job(&request.job, &mut conn).await?;
    Ok(Json(JobResponse { job }))
}

/// Query parameters for get_job_by_name.
#[derive(Deserialize, utoipa::IntoParams)]
struct JobNameQuery {
    /// The Kubernetes job name to look up.
    job_name: String,
}

/// Look up a job by name and return it as JSON.
///
/// Used by: CLI (job describe, wait, retry)
#[utoipa::path(
    get,
    path = "/jobs",
    params(JobNameQuery),
    responses(
        (status = 200, description = "Job found", body = JobResponse)
    )
)]
async fn get_job_by_name(
    _user: User,
    DbConn(mut conn): DbConn,
    Query(query): Query<JobNameQuery>,
) -> FalconeridResult<Json<JobResponse>> {
    let job = Job::find_by_job_name(&query.job_name, &mut conn).await?;
    Ok(Json(JobResponse { job }))
}

/// List all jobs.
///
/// Used by: CLI (job list)
#[utoipa::path(
    get,
    path = "/jobs/list",
    responses(
        (status = 200, description = "List of all jobs", body = JobsResponse)
    )
)]
async fn list_jobs(
    _user: User,
    DbConn(mut conn): DbConn,
) -> FalconeridResult<Json<JobsResponse>> {
    let jobs = Job::list(&mut conn).await?;
    Ok(Json(JobsResponse { jobs }))
}

/// Look up a job by ID and return it as JSON.
///
/// Used by: CLI (job wait), Worker
#[utoipa::path(
    get,
    path = "/jobs/{job_id}",
    params(
        ("job_id" = Uuid, Path, description = "The job UUID")
    ),
    responses(
        (status = 200, description = "Job found", body = JobResponse)
    )
)]
async fn get_job(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<JobResponse>> {
    let job = Job::find(job_id, &mut conn).await?;
    Ok(Json(JobResponse { job }))
}

/// Get detailed job information for display.
///
/// Used by: CLI (job describe)
#[utoipa::path(
    get,
    path = "/jobs/{job_id}/describe",
    params(
        ("job_id" = Uuid, Path, description = "The job UUID")
    ),
    responses(
        (status = 200, description = "Job description", body = JobDescribeResponse)
    )
)]
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
///
/// Used by: CLI (job retry)
#[utoipa::path(
    post,
    path = "/jobs/{job_id}/retry",
    params(
        ("job_id" = Uuid, Path, description = "The job UUID to retry")
    ),
    responses(
        (status = 200, description = "New job created from retry", body = JobResponse)
    )
)]
async fn job_retry(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<JobResponse>> {
    let job = Job::find(job_id, &mut conn).await?;
    let new_job = retry_job(&job, &mut conn).await?;
    Ok(Json(JobResponse { job: new_job }))
}

/// Reserve the next available datum for a job, and return it along with a list
/// of input files.
///
/// Used by: Worker
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
///
/// Used by: Worker
async fn patch_datum(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
    Json(request): Json<UpdateDatumRequest>,
) -> FalconeridResult<Json<DatumResponse>> {
    let patch = request.datum.clone();

    // Wrap everything in a transaction for the ownership lock.
    let datum = conn
        .transaction(|conn| {
            async move {
                // Lock datum and verify ownership (returns 403 if mismatch).
                let mut datum =
                    Datum::lock_and_verify_owner(datum_id, &request.pod_name, conn)
                        .await
                        .map_err(FalconeridError::from)?;

                // We only support a few very specific types of patches.
                match &patch {
                    // Set status to `Status::Done`.
                    DatumPatch {
                        status: Status::Done,
                        output,
                        error_message: None,
                        backtrace: None,
                    } => {
                        datum.mark_as_done(output, conn).await?;
                    }

                    // Set status to `Status::Error`.
                    DatumPatch {
                        status: Status::Error,
                        output,
                        error_message: Some(error_message),
                        backtrace: Some(backtrace),
                    } => {
                        datum
                            .mark_as_error(output, error_message, backtrace, conn)
                            .await?;
                    }

                    // All other combinations are forbidden.
                    other => {
                        return Err(FalconeridError::Internal(format_err!(
                            "cannot update datum with {:?}",
                            other
                        )));
                    }
                }

                // If there are no more datums, mark the job as finished (either
                // done or error).
                datum.update_job_status_if_done(conn).await?;

                Ok::<_, FalconeridError>(datum)
            }
            .scope_boxed()
        })
        .await?;

    Ok(Json(DatumResponse { datum }))
}

/// Get detailed datum information for display.
///
/// Used by: CLI (datum describe)
#[utoipa::path(
    get,
    path = "/datums/{datum_id}/describe",
    params(
        ("datum_id" = Uuid, Path, description = "The datum UUID")
    ),
    responses(
        (status = 200, description = "Datum description", body = DatumDescribeResponse)
    )
)]
async fn describe_datum(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
) -> FalconeridResult<Json<DatumDescribeResponse>> {
    let datum = Datum::find(datum_id, &mut conn).await?;
    let input_files = datum.input_files(&mut conn).await?;
    Ok(Json(DatumDescribeResponse { datum, input_files }))
}

/// Create a batch of output files for a datum.
///
/// Used by: Worker
async fn create_output_files(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
    Json(request): Json<CreateOutputFilesRequest>,
) -> FalconeridResult<Json<OutputFilesResponse>> {
    let output_files = conn
        .transaction(|conn| {
            async move {
                // Lock datum and verify ownership (returns 403 if mismatch).
                let datum =
                    Datum::lock_and_verify_owner(datum_id, &request.pod_name, conn)
                        .await
                        .map_err(FalconeridError::from)?;

                // Build NewOutputFile with job_id from datum.
                let new_files: Vec<NewOutputFile> = request
                    .output_files
                    .iter()
                    .map(|f| NewOutputFile {
                        job_id: datum.job_id,
                        datum_id: datum.id,
                        uri: f.uri.clone(),
                    })
                    .collect();

                let output_files = NewOutputFile::insert_all(&new_files, conn).await?;
                Ok::<_, FalconeridError>(output_files)
            }
            .scope_boxed()
        })
        .await?;
    Ok(Json(OutputFilesResponse { output_files }))
}

/// Update a batch of output files for a datum.
///
/// Used by: Worker
async fn patch_output_files(
    _user: User,
    DbConn(mut conn): DbConn,
    Path(datum_id): Path<Uuid>,
    Json(request): Json<UpdateOutputFilesRequest>,
) -> FalconeridResult<StatusCode> {
    // Separate patches by status.
    let mut done_ids = vec![];
    let mut error_ids = vec![];
    for patch in &request.output_files {
        match patch.status {
            Status::Done => done_ids.push(patch.id),
            Status::Error => error_ids.push(patch.id),
            _ => {
                return Err(FalconeridError::Internal(format_err!(
                    "cannot patch output file with {:?}",
                    patch
                )));
            }
        }
    }

    // Apply our updates within a transaction that verifies ownership.
    conn.transaction(|conn| {
        async move {
            // Lock datum and verify ownership (returns 403 if mismatch).
            let _datum =
                Datum::lock_and_verify_owner(datum_id, &request.pod_name, conn)
                    .await
                    .map_err(FalconeridError::from)?;

            OutputFile::mark_ids_as_done(&done_ids, conn).await?;
            OutputFile::mark_ids_as_error(&error_ids, conn).await?;
            Ok::<_, FalconeridError>(())
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
            "/datums/{datum_id}/output_files",
            post(create_output_files).patch(patch_output_files),
        )
        // OpenAPI JSON endpoint for CLI-facing API documentation.
        .route("/api-docs/openapi.json", get(openapi_json))
        // HTTP request/response tracing for debugging.
        .layer(TraceLayer::new_for_http())
        // 50 MB limit to match previous Rocket.toml configuration
        .layer(RequestBodyLimitLayer::new(52_428_800))
        .with_state(state);

    // Start the server.
    eprintln!("Will listen on 0.0.0.0:8089.");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8089").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
