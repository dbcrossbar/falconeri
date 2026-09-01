// ! Code for starting a job on the server.

use std::cmp::min;

use falconeri_common::{
    cast,
    diesel_async::{scoped_futures::ScopedFutureExt, AsyncConnection},
    kubernetes,
    manifest::render_manifest,
    pipeline::*,
    prelude::*,
    serde_json,
};

use crate::inputs::input_to_datums;

/// Run a new job on our cluster.
#[instrument(skip_all, level = "debug")]
pub async fn run_job(
    pipeline_spec: &PipelineSpec,
    conn: &mut AsyncPgConnection,
) -> Result<Job> {
    // Build our job.
    let job_id = Uuid::new_v4();
    let job_name = unique_kubernetes_job_name(&pipeline_spec.pipeline.name);

    // If nobody specified RUST_LOG, default it sensibly.
    let mut transform = pipeline_spec.transform.clone();
    if !transform.env.contains_key("RUST_LOG") {
        transform.env.insert(
            "RUST_LOG".to_owned(),
            "falconeri_common=info,falconeri_worker=info,warning".to_owned(),
        );
    }

    // Store the spec as we are actually going to run it, including the
    // defaults we filled in above, because `retry_job` reparses it to rerun
    // this job.
    let mut pipeline_spec_as_run = pipeline_spec.clone();
    pipeline_spec_as_run.transform = transform;

    let new_job = NewJob {
        id: job_id,
        pipeline_spec: serde_json::to_value(&pipeline_spec_as_run)
            .context("could not serialize pipeline spec")?,
        job_name,
        command: pipeline_spec.transform.cmd.clone(),
        egress_uri: pipeline_spec.egress.uri.clone(),
    };

    // Calculate how many times we're allowed to retry a datum.
    let maximum_allowed_run_count = cast::i32(pipeline_spec.datum_tries.unwrap_or(1))?;

    // Get our datums and input files.
    let (new_datums, new_input_files) = input_to_datums(
        &pipeline_spec.transform.secrets,
        job_id,
        maximum_allowed_run_count,
        &pipeline_spec.input,
    )
    .await?;

    // Insert everthing into the database.
    let job = conn
        .transaction(|conn| {
            async move {
                let job = new_job.insert(conn).await?;
                NewDatum::insert_all(&new_datums, conn).await?;
                NewInputFile::insert_all(&new_input_files, conn).await?;
                Ok::<_, Error>(job)
            }
            .scope_boxed()
        })
        .await?;

    // Launch our batch job on the cluster.
    start_batch_job(pipeline_spec, &job).await?;
    Ok(job)
}

/// The `job retry` subcommand.
///
/// This is explicitly a last-ditch, emergency retry for patching up very
/// expensive failed jobs that _almost_ succeeded. Let's say you've spent a
/// CPU-year processing 900 datums, and 3 failed because they either needed
/// slightly more memory, or they contained edge-case data that broke the
/// Docker image you used to do the processing. So maybe you've edited the
/// job spec JSON recorded in the database by hand, or you've built and pushed
/// new images. The `retry` command creates a _new_ job, a "chimera" mixing
/// successful results from the old job with fresh datums that will be re-run.
///
/// We **want** this to work and to be correct. Just because it's a last-ditch
/// measure doesn't mean we _like_ this code to have issues. But if an operator
/// is running this, a heightened level of awareness is likely required. Making
/// chimeras can sometimes go badly.
///
/// TODO: _Strengthen_ our guarantees here as much as we can, rather than
/// letting this rot. Or at least please don't make it worse.
///
/// TODO: We likely want to look carefully at _output_ file handling here.
/// There are some corner cases we may not handle well.
#[instrument(skip_all, fields(job = %job.id), level = "debug")]
pub async fn retry_job(job: &Job, conn: &mut AsyncPgConnection) -> Result<Job> {
    // Load the original job, failed datums, and input files.
    if job.status != Status::Error {
        return Err(format_err!("can only retry jobs with status 'error'"));
    }

    let job_pipeline_spec = job.pipeline_spec.clone();
    let job_command = job.command.clone();
    let job_egress_uri = job.egress_uri.clone();

    let (pipeline_spec, new_job) = conn
        .transaction(|conn| {
            async move {
                // First, fetch our datum status counts, to make sure that this
                // job has actually finished quieting down. We do this carefully,
                // with a `match`, to make sure we think about every possible status.
                let datum_status_counts = job.datum_status_counts(conn).await?;
                debug!("datum status counts: {:?}", datum_status_counts);
                for datum_status_count in &datum_status_counts {
                    match datum_status_count.status {
                        // If we have some datums that have never been run in
                        // the first place, we need to be super cautions,
                        // because we haven't thought through whether they might
                        // get assigned workers on the _old_ job when we're
                        // trying to make a new one. Earlier versions of retry
                        // silently ignored these datums, losing any output they
                        // would have produced. We merely error.
                        Status::Ready if datum_status_count.count > 0 => {
                            return Err(format_err!(
                                "old job has {} never-run datums with status=\"ready\", and we don't know how to retry this safely",
                                datum_status_count.count,
                            ));
                        }
                        Status::Ready => {}

                        // There are edge cases where some datums may still be marked as
                        // Running, which the babysitter will try to sort out as part of
                        // zombie-datum checking.
                        Status::Running if datum_status_count.count > 0 => {
                            return Err(format_err!(
                                "original failed job still has {} running datums; wait to see if the babysitter resolves them in a few minutes",
                                datum_status_count.count,
                            ));
                        }
                        Status::Running => {}

                        // Huh, never even considered these cases.
                        Status::Canceled if datum_status_count.count > 0 => {
                            return Err(format_err!(
                                "cannot retry job with cancelled datums"
                            ));
                        }
                        Status::Canceled => {}

                        // These are in tidy terminal states, and we'll handle them below.
                        Status::Done | Status::Error => {}
                    }
                }

                // Get our error datums to retry, and the associated files.
                let error_datums = job.datums_with_status(Status::Error, conn).await?;
                if error_datums.is_empty() {
                    return Err(format_err!(
                        "no error datums to retry (the output data may be complete)",
                    ));
                }
                let input_files = InputFile::for_datums(&error_datums, conn).await?;

                // Recover the original pipeline specification.
                let mut pipeline_spec: PipelineSpec =
                    serde_json::from_value(job_pipeline_spec.clone())
                        .context("could not parse original pipeline spec")?;
                pipeline_spec.parallelism_spec.constant = min(
                    pipeline_spec.parallelism_spec.constant,
                    cast::u32(error_datums.len())?,
                );

                // Create a new job record.
                let job_name =
                    unique_kubernetes_job_name(&pipeline_spec.pipeline.name);
                let new_job = NewJob {
                    id: Uuid::new_v4(),
                    pipeline_spec: job_pipeline_spec.clone(),
                    job_name,
                    command: job_command.clone(),
                    egress_uri: job_egress_uri.clone(),
                }
                .insert(conn)
                .await?;

                // Create new datums and input files.
                let mut new_datums = vec![];
                let mut new_input_files = vec![];
                for (old_datum, input_files) in
                    error_datums.into_iter().zip(input_files)
                {
                    let datum_id = Uuid::new_v4();
                    new_datums.push(NewDatum {
                        id: datum_id,
                        job_id: new_job.id,
                        // I guess we'll give this the same number of retries it was
                        // allowed before?
                        maximum_allowed_run_count: old_datum.maximum_allowed_run_count,
                    });
                    for input_file in input_files {
                        new_input_files.push(NewInputFile {
                            datum_id,
                            uri: input_file.uri.clone(),
                            local_path: input_file.local_path.clone(),
                            job_id: new_job.id,
                        });
                    }
                }
                NewDatum::insert_all(&new_datums, conn).await?;
                NewInputFile::insert_all(&new_input_files, conn).await?;

                Ok::<_, Error>((pipeline_spec, new_job))
            }
            .scope_boxed()
        })
        .await?;

    // Start a new batch job.
    start_batch_job(&pipeline_spec, &new_job).await?;
    Ok(new_job)
}

/// Generate a unique name for our job. To keep Kubernetes happy, this
/// must be a legal DNS name component (but we have a database constraint
/// to enforce that).
pub fn unique_kubernetes_job_name(pipeline_name: &str) -> String {
    let tag = kubernetes::resource_tag();
    format!("{}-{}", pipeline_name, tag)
        .replace('_', "-")
        .to_lowercase()
}

/// The manifest to use to run a job.
const RUN_MANIFEST_TEMPLATE: &str = include_str!("job_manifest.yml.hbs");

/// Parameters used to render `MANIFEST_TEMPLATE`.
#[derive(Serialize)]
struct JobParams<'a> {
    pipeline_spec: &'a PipelineSpec,
    /// The Kubernetes Job `backoffLimit`.
    kubernetes_backoff_limit: u32,
    /// The Kubernetes Job `activeDeadlineSeconds`.
    job_timeout_seconds: u64,
    job: &'a Job,
    /// The falconeri image to use for init containers (e.g., "ghcr.io/dbcrossbar/falconeri:2.0.0").
    falconeri_image: String,
    /// Whether to use `imagePullPolicy: Never` for the init container (for local dev).
    use_local_image: bool,
}

impl<'a> JobParams<'a> {
    fn new(pipeline_spec: &'a PipelineSpec, job: &'a Job) -> JobParams<'a> {
        let falconeri_image = std::env::var("FALCONERI_IMAGE").unwrap_or_else(|_| {
            format!("ghcr.io/dbcrossbar/falconeri:{}", env!("CARGO_PKG_VERSION"))
        });
        let use_local_image = kubernetes::use_local_image();
        Self {
            pipeline_spec,
            kubernetes_backoff_limit: pipeline_spec
                .maximum_counted_pod_failures()
                .kubernetes_backoff_limit(),
            job_timeout_seconds: pipeline_spec.job_timeout.as_secs(),
            job,
            falconeri_image,
            use_local_image,
        }
    }

    fn render(&self) -> Result<String> {
        render_manifest(RUN_MANIFEST_TEMPLATE, self)
            .context("error rendering job template")
    }
}

/// Start a new batch job running.
#[instrument(skip_all, fields(job = %job.id), level = "debug")]
pub async fn start_batch_job(pipeline_spec: &PipelineSpec, job: &Job) -> Result<()> {
    debug!("starting batch job on cluster");

    kubernetes::deploy(&JobParams::new(pipeline_spec, job).render()?).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RenderedJobManifest(serde_json::Value);

    impl RenderedJobManifest {
        fn for_pipeline_spec(pipeline_spec: &PipelineSpec) -> Self {
            Self(
                serde_yaml::from_str(
                    &JobParams::new(pipeline_spec, &Job::factory())
                        .render()
                        .expect("job manifest should render"),
                )
                .expect("rendered job manifest should be valid YAML"),
            )
        }

        fn backoff_limit(&self) -> u64 {
            self.0["spec"]["backoffLimit"]
                .as_u64()
                .expect("backoffLimit should be an integer")
        }

        fn active_deadline_seconds(&self) -> u64 {
            self.0["spec"]["activeDeadlineSeconds"]
                .as_u64()
                .expect("activeDeadlineSeconds should be an integer")
        }

        fn pod_failure_policy_rules(&self) -> &[serde_json::Value] {
            self.0["spec"]["podFailurePolicy"]["rules"]
                .as_array()
                .expect("pod failure policy rules should be an array")
        }
    }

    fn example_pipeline_spec_json() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../falconeri_common/src/example_pipeline_spec.json"
        ))
        .expect("example pipeline spec should parse as JSON")
    }

    fn example_pipeline_spec() -> PipelineSpec {
        serde_json::from_value(example_pipeline_spec_json())
            .expect("example pipeline spec should parse")
    }

    #[test]
    fn renders_valid_job_manifest() {
        RenderedJobManifest::for_pipeline_spec(&example_pipeline_spec());
    }

    #[test]
    fn default_failure_budget_allows_two_failures_per_worker() {
        for (parallelism, expected_backoff_limit) in [(1, 4), (2, 4), (4, 8), (16, 32)]
        {
            let mut pipeline_spec = example_pipeline_spec();
            pipeline_spec.parallelism_spec.constant = parallelism;

            assert_eq!(
                RenderedJobManifest::for_pipeline_spec(&pipeline_spec).backoff_limit(),
                expected_backoff_limit,
                "wrong backoffLimit for parallelism {}",
                parallelism,
            );
        }
    }

    #[test]
    fn disruptions_do_not_consume_the_failure_budget() {
        let rendered_job_manifest =
            RenderedJobManifest::for_pipeline_spec(&example_pipeline_spec());
        let rules = rendered_job_manifest.pod_failure_policy_rules();

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["action"], "Ignore");
        assert_eq!(rules[0]["onPodConditions"][0]["type"], "DisruptionTarget");
        // Kubernetes rejects the whole Job if we leave this out.
        assert_eq!(rules[0]["onPodConditions"][0]["status"], "True");
    }

    #[test]
    fn explicit_worker_failure_budget_overrides_default() {
        let mut pipeline_spec_json = example_pipeline_spec_json();
        pipeline_spec_json["worker_failure_policy"] = serde_json::json!({
            "maximum_counted_pod_failures": 40
        });

        assert_eq!(
            RenderedJobManifest::for_pipeline_spec(
                &serde_json::from_value(pipeline_spec_json)
                    .expect("worker failure policy should parse")
            )
            .backoff_limit(),
            40
        );
    }

    #[test]
    fn job_timeout_becomes_the_active_deadline() {
        assert_eq!(
            RenderedJobManifest::for_pipeline_spec(&example_pipeline_spec())
                .active_deadline_seconds(),
            300
        );
    }

    #[test]
    fn jobs_without_a_timeout_get_the_three_day_default() {
        let mut pipeline_spec_json = example_pipeline_spec_json();
        pipeline_spec_json
            .as_object_mut()
            .expect("example pipeline spec should be a JSON object")
            .remove("job_timeout");

        assert_eq!(
            RenderedJobManifest::for_pipeline_spec(
                &serde_json::from_value(pipeline_spec_json)
                    .expect("pipeline spec without a job timeout should parse")
            )
            .active_deadline_seconds(),
            3 * 24 * 60 * 60
        );
    }
}
