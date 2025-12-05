//! The `job run` subcommand.

use falconeri_common::{pipeline::*, prelude::*, rest_api::Client};

/// The `job run` subcommand.
pub async fn run(pipeline_spec: &PipelineSpec) -> Result<()> {
    let client = Client::new(ConnectVia::Proxy).await?;
    let job = client.new_job(pipeline_spec).await?;
    println!("{}", job.job_name);
    Ok(())
}
