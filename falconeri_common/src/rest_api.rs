//! The REST API for `falconerid`, including data types and a client.

use serde::de::DeserializeOwned;
use url::Url;
use utoipa::ToSchema;

use crate::{
    db,
    kubernetes::{node_name, pod_name},
    pipeline::PipelineSpec,
    prelude::*,
};

/// Request the reservation of a datum.
#[derive(Debug, Deserialize, Serialize)]
pub struct DatumReservationRequest {
    /// The Kubernetes node name which will process this datum.
    pub node_name: String,
    /// The Kubernetes pod name which will process this datum.
    pub pod_name: String,
}

/// Information about a reserved datum.
#[derive(Debug, Deserialize, Serialize)]
pub struct DatumReservationResponse {
    /// The reserved datum to process.
    pub datum: Datum,
    /// The input files associated with this datum.
    pub input_files: Vec<InputFile>,
}

/// Information about a datum that we can update.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DatumPatch {
    /// The new status for the datum. Must be either `Status::Done` or
    /// `Status::Error`.
    pub status: Status,
    /// The output of procesisng the datum.
    pub output: String,
    /// If and only if `status` is `Status::Error`, this should be the error
    /// message.
    pub error_message: Option<String>,
    /// If and only if `status` is `Status::Error`, this should be the error
    /// backtrace.
    pub backtrace: Option<String>,
}

/// Information about an output file that we can update.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct OutputFilePatch {
    /// The ID of the output file to update.
    pub id: Uuid,
    /// The status of the output file. Must be either `Status::Done` or
    /// `Status::Error`.
    pub status: Status,
}

/// Data for creating an output file via POST.
///
/// The datum_id and job_id are provided via the URL path, so this only
/// contains the URI. Follows the same naming pattern as `DatumPatch` for
/// PATCH operations.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct OutputFilePost {
    /// The URI to which we uploaded this file.
    pub uri: String,
}

/// Response for job describe endpoint.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct JobDescribeResponse {
    /// The job being described.
    pub job: Job,
    /// Counts of datums by status.
    pub datum_status_counts: Vec<DatumStatusCount>,
    /// Currently running datums.
    pub running_datums: Vec<Datum>,
    /// Datums that have errored.
    pub error_datums: Vec<Datum>,
}

/// Response for datum describe endpoint.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DatumDescribeResponse {
    /// The datum being described.
    pub datum: Datum,
    /// The input files for this datum.
    pub input_files: Vec<InputFile>,
}

// ============================================================================
// Rails-style wrapper types for REST API requests and responses.
// ============================================================================

/// Response wrapper for a single job.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct JobResponse {
    /// The job.
    pub job: Job,
}

/// Response wrapper for a list of jobs.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct JobsResponse {
    /// The list of jobs.
    pub jobs: Vec<Job>,
}

/// Response wrapper for a single datum.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DatumResponse {
    /// The datum.
    pub datum: Datum,
}

/// Response wrapper for a list of output files.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct OutputFilesResponse {
    /// The list of output files.
    pub output_files: Vec<OutputFile>,
}

/// Request wrapper for creating a job.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateJobRequest {
    /// The pipeline spec to create the job from.
    pub job: PipelineSpec,
}

/// Request wrapper for updating a datum (worker endpoint).
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct UpdateDatumRequest {
    /// The pod making this request (for ownership verification).
    pub pod_name: String,
    /// The datum patch to apply.
    pub datum: DatumPatch,
}

/// Request wrapper for creating output files (worker endpoint).
///
/// Used with `POST /datums/{datum_id}/output_files`.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateOutputFilesRequest {
    /// The pod making this request (for ownership verification).
    pub pod_name: String,
    /// The output files to create.
    pub output_files: Vec<OutputFilePost>,
}

/// Request wrapper for updating output files (worker endpoint).
///
/// Used with `PATCH /datums/{datum_id}/output_files`.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct UpdateOutputFilesRequest {
    /// The pod making this request (for ownership verification).
    pub pod_name: String,
    /// The output file patches to apply.
    pub output_files: Vec<OutputFilePatch>,
}

/// A client for talking to `falconerid`.
pub struct Client {
    via: ConnectVia,
    url: Url,
    username: String,
    password: String,
    client: reqwest::Client,
}

impl Client {
    /// Create a new client, connecting to `falconerid` as specified.
    #[instrument(level = "trace")]
    pub async fn new(via: ConnectVia) -> Result<Client> {
        // Choose an appropriate URL.
        let url = match via {
            ConnectVia::Cluster => "http://falconerid:8089/",
            ConnectVia::Proxy => "http://localhost:8089/",
        }
        .parse()
        .expect("could not parse URL in source code");

        // Get our credentials. For now, we use our database password for API
        // access, too.
        let username = "falconeri".to_owned();
        let password = db::postgres_password(via).await?;

        // Decide how long to keep connections open.
        let max_idle = match via {
            // If we're running on the cluster, connection startup is cheap but
            // we may have hundreds of inbound connections, so drop connections
            // as fast as possible. This could be improved by putting an async
            // proxy server in front of `falconerid`, if we want that.
            ConnectVia::Cluster => 0,
            // Otherwise allow the maximum possible number of connections.
            ConnectVia::Proxy => usize::MAX,
        };

        // Create our HTTP client.
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(max_idle)
            .build()
            .context("cannot build HTTP client")?;

        Ok(Client {
            via,
            url,
            username,
            password,
            client,
        })
    }

    /// List all jobs.
    ///
    /// `GET /jobs/list`
    #[instrument(level = "trace", skip_all)]
    pub async fn list_jobs(&self) -> Result<Vec<Job>> {
        let url = self.url.join("jobs/list")?;
        let response: JobsResponse = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .get(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .send()
                    .await
                    .with_context(|| format!("error getting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        Ok(response.jobs)
    }

    /// Create a job. This does not automatically retry on network failure,
    /// because it's very expensive and not idempotent (and only called by
    /// `falconeri` and never `falconeri-worker`).
    ///
    /// `POST /jobs`
    #[instrument(skip_all, level = "trace")]
    pub async fn new_job(&self, pipeline_spec: &PipelineSpec) -> Result<Job> {
        let url = self.url.join("jobs")?;
        let request = CreateJobRequest {
            job: pipeline_spec.clone(),
        };
        let resp = self
            .client
            .post(url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .json(&request)
            .send()
            .await
            .with_context(|| format!("error posting {}", url))?;
        let response: JobResponse = self.handle_json_response(&url, resp).await?;
        Ok(response.job)
    }

    /// Fetch a job by ID.
    ///
    /// `GET /jobs/<job_id>`
    #[instrument(skip_all, fields(id = %id), level = "trace")]
    pub async fn job(&self, id: Uuid) -> Result<Job> {
        let url = self.url.join(&format!("jobs/{}", id))?;
        let response: JobResponse = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .get(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .send()
                    .await
                    .with_context(|| format!("error getting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        Ok(response.job)
    }

    /// Fetch a job by name.
    ///
    /// `GET /jobs?job_name=$NAME`
    #[instrument(skip_all, fields(job_name = %job_name), level = "trace")]
    pub async fn find_job_by_name(&self, job_name: &str) -> Result<Job> {
        let mut url = self.url.join("jobs")?;
        url.query_pairs_mut()
            .append_pair("job_name", job_name)
            .finish();
        let response: JobResponse = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .get(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .send()
                    .await
                    .with_context(|| format!("error getting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        Ok(response.job)
    }

    /// Get detailed job information for display.
    ///
    /// `GET /jobs/{job_id}/describe`
    #[instrument(skip_all, fields(job_id = %job_id), level = "trace")]
    pub async fn describe_job(&self, job_id: Uuid) -> Result<JobDescribeResponse> {
        let url = self.url.join(&format!("jobs/{}/describe", job_id))?;
        self.via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .get(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .send()
                    .await
                    .with_context(|| format!("error getting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await
    }

    /// Retry a job by ID.
    ///
    /// Not idempotent because it's expensive and only called by `falconeri`.
    ///
    /// `POST /jobs/<job_id>/retry`
    #[instrument(skip_all, fields(job = %job.id), level = "trace")]
    pub async fn retry_job(&self, job: &Job) -> Result<Job> {
        let url = self.url.join(&format!("jobs/{}/retry", job.id))?;
        let resp = self
            .client
            .post(url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .with_context(|| format!("error posting {}", url))?;
        let response: JobResponse = self.handle_json_response(&url, resp).await?;
        Ok(response.job)
    }

    /// Reserve the next available datum to process, and return it along with
    /// the corresponding input files. This can only be called from inside a
    /// pod.
    ///
    /// `POST /jobs/<job_id>/reserve_next_datum`
    #[instrument(skip_all, fields(job = %job.id), level = "trace")]
    pub async fn reserve_next_datum(
        &self,
        job: &Job,
    ) -> Result<Option<(Datum, Vec<InputFile>)>> {
        let url = self
            .url
            .join(&format!("jobs/{}/reserve_next_datum", job.id))?;
        let resv_resp: Option<DatumReservationResponse> = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .post(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .json(&DatumReservationRequest {
                        node_name: node_name()?,
                        pod_name: pod_name()?,
                    })
                    .send()
                    .await
                    .with_context(|| format!("error posting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        Ok(resv_resp.map(|r| (r.datum, r.input_files)))
    }

    /// Mark `datum` as done, and record the output of the commands we ran.
    #[instrument(skip_all, fields(datum_id = %datum.id), level = "trace")]
    pub async fn mark_datum_as_done(
        &self,
        datum: &mut Datum,
        output: String,
    ) -> Result<()> {
        let patch = DatumPatch {
            status: Status::Done,
            output,
            error_message: None,
            backtrace: None,
        };
        self.patch_datum(datum, &patch).await
    }

    /// Mark `datum` as having failed, and record the output and error
    /// information.
    #[instrument(skip_all, fields(datum = %datum.id), level = "trace")]
    pub async fn mark_datum_as_error(
        &self,
        datum: &mut Datum,
        output: String,
        error_message: String,
        backtrace: String,
    ) -> Result<()> {
        let patch = DatumPatch {
            status: Status::Error,
            output,
            error_message: Some(error_message),
            backtrace: Some(backtrace),
        };
        self.patch_datum(datum, &patch).await
    }

    /// Apply `patch` to `datum`.
    ///
    /// `PATCH /datums/<datum_id>`
    #[instrument(skip_all, fields(datum = %datum.id), level = "trace")]
    async fn patch_datum(&self, datum: &mut Datum, patch: &DatumPatch) -> Result<()> {
        let url = self.url.join(&format!("datums/{}", datum.id))?;
        let request = UpdateDatumRequest {
            pod_name: pod_name()?,
            datum: patch.clone(),
        };
        let response: DatumResponse = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .patch(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .json(&request)
                    .send()
                    .await
                    .with_context(|| format!("error patching {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        *datum = response.datum;
        Ok(())
    }

    /// Get detailed datum information for display.
    ///
    /// `GET /datums/{datum_id}/describe`
    #[instrument(skip_all, fields(datum_id = %datum_id), level = "trace")]
    pub async fn describe_datum(
        &self,
        datum_id: Uuid,
    ) -> Result<DatumDescribeResponse> {
        let url = self.url.join(&format!("datums/{}/describe", datum_id))?;
        self.via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .get(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .send()
                    .await
                    .with_context(|| format!("error getting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await
    }

    /// Create new output files for a datum.
    ///
    /// `POST /datums/{datum_id}/output_files`
    #[instrument(level = "trace", skip_all, fields(datum = %datum.id))]
    pub async fn create_output_files(
        &self,
        datum: &Datum,
        output_files: &[OutputFilePost],
    ) -> Result<Vec<OutputFile>> {
        let url = self
            .url
            .join(&format!("datums/{}/output_files", datum.id))?;
        let request = CreateOutputFilesRequest {
            pod_name: pod_name()?,
            output_files: output_files.to_vec(),
        };
        // TODO: We might want finer-grained retry here? This isn't remotely
        // idempotent. Though I suppose if we encounter a "double create", all
        // the retries should just fail until we give up, then we'll eventually
        // fail the datum, allowing it to be retried.
        let response: OutputFilesResponse = self
            .via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .post(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .json(&request)
                    .send()
                    .await
                    .with_context(|| format!("error posting {}", url))?;
                self.handle_json_response(&url, resp).await
            })
            .await?;
        Ok(response.output_files)
    }

    /// Update the status of existing output files for a datum.
    ///
    /// `PATCH /datums/{datum_id}/output_files`
    #[instrument(level = "trace", skip_all, fields(datum = %datum.id))]
    pub async fn patch_output_files(
        &self,
        datum: &Datum,
        patches: &[OutputFilePatch],
    ) -> Result<()> {
        let url = self
            .url
            .join(&format!("datums/{}/output_files", datum.id))?;
        let request = UpdateOutputFilesRequest {
            pod_name: pod_name()?,
            output_files: patches.to_vec(),
        };
        self.via
            .retry_if_appropriate_async(|| async {
                let resp = self
                    .client
                    .patch(url.clone())
                    .basic_auth(&self.username, Some(&self.password))
                    .json(&request)
                    .send()
                    .await
                    .with_context(|| format!("error patching {}", url))?;
                self.handle_empty_response(&url, resp).await
            })
            .await
    }

    /// Check the HTTP status code and parse a JSON response.
    #[instrument(level = "trace", skip_all, fields(url = %url))]
    async fn handle_json_response<T>(
        &self,
        url: &Url,
        resp: reqwest::Response,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        if resp.status().is_success() {
            let value = resp
                .json()
                .await
                .with_context(|| format!("error parsing {}", url))?;
            Ok(value)
        } else {
            Err(self.handle_error_response(url, resp).await)
        }
    }

    /// Check the HTTP status code and parse a JSON response.
    #[instrument(level = "trace", skip_all, fields(url = %url))]
    async fn handle_empty_response(
        &self,
        url: &Url,
        resp: reqwest::Response,
    ) -> Result<()> {
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(self.handle_error_response(url, resp).await)
        }
    }

    /// Extract an error from an HTTP respone payload.
    #[instrument(level = "trace", skip_all, fields(url = %url, status = %resp.status()))]
    async fn handle_error_response(
        &self,
        url: &Url,
        resp: reqwest::Response,
    ) -> Error {
        let status = resp.status();
        match resp.text().await {
            Ok(body) => {
                format_err!("unexpected HTTP status {} for {}:\n{}", status, url, body,)
            }
            Err(err) => err.into(),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("via", &self.via)
            .field("url", &self.url)
            .field("username", &self.username)
            // We don't need these for debugging.
            //
            // .field("password", &self.password)
            // .field("client", &self.client)
            .finish()
    }
}
