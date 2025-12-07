//! The `job list` subcommand.

use falconeri_common::{prelude::*, rest_api::Client};
use prettytable::{format::consts::FORMAT_CLEAN, row, Table};

/// The `job list` subcommand.
#[instrument(level = "trace")]
pub async fn run() -> Result<()> {
    // Look up the information to display.
    let client = Client::new(ConnectVia::Proxy).await?;
    let jobs = client.list_jobs().await?;

    // Create a new table. This library makes some rather unusual API choices,
    // but it does the job well enough.
    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.add_row(row!["JOB_NAME", "STATUS", "CREATED_AT"]);

    // Print information about each job.
    for job in jobs {
        table.add_row(row![&job.job_name, job.status, job.created_at]);
    }

    table.printstd();
    Ok(())
}
