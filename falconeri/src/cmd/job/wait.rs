//! The `job wait` subcommand.

use std::time::Duration;

use falconeri_common::{prelude::*, rest_api::Client};

/// The `job wait` subcommand.
pub async fn run(job_name: &str) -> Result<()> {
    let client = Client::new(ConnectVia::Proxy).await?;
    let mut job = client.find_job_by_name(job_name).await?;
    while !job.status.has_finished() {
        tokio::time::sleep(Duration::from_secs(30)).await;
        job = client.job(job.id).await?;
    }
    println!("{}", job.status);
    Ok(())
}
