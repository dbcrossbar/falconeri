//! The `job list` subcommand.

use falconeri_common::{db, prelude::*};
use prettytable::{format::consts::FORMAT_CLEAN, row, Table};

/// The `job list` subcommand.
pub async fn run() -> Result<()> {
    // Look up the information to display.
    let pool = db::async_pool(1, ConnectVia::Proxy)?;
    let mut conn = pool.get().await.context("could not get db connection")?;
    let jobs = Job::list(&mut conn).await?;

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
