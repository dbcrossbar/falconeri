//! `db` subcommand for interaction with the database.

use clap::Subcommand;
use falconeri_common::{db, prelude::*};
use std::process;

/// Commands for interacting with the database.
#[derive(Debug, Subcommand)]
pub enum Opt {
    /// Access the database console.
    #[command(name = "console")]
    Console,
    /// Print our a URL for connecting to the database.
    #[command(name = "url")]
    Url,
}

/// Run the `db` subcommand.
///
/// These commands are async because we need to fetch the database URL via kubectl,
/// but the actual psql execution stays sync (it's interactive with inherited stdio).
#[instrument(skip_all, level = "trace")]
pub async fn run(opt: &Opt) -> Result<()> {
    match opt {
        Opt::Console => run_console().await,
        Opt::Url => run_url().await,
    }
}

/// Connect to the database console.
#[instrument(level = "debug")]
async fn run_console() -> Result<()> {
    let url = db::database_url(ConnectVia::Proxy).await?;
    // Use std::process::Command (sync) because psql is interactive
    // and needs inherited stdio. There may be a way to do this using async
    // but we haven't looked that hard for it yet.
    let status = process::Command::new("psql")
        .arg(&url)
        .status()
        .context("error starting psql")?;
    if !status.success() {
        return Err(format_err!("error running psql with {:?}", url));
    }
    Ok(())
}

/// Print out the database URL.
#[instrument(level = "trace")]
async fn run_url() -> Result<()> {
    let url = db::database_url(ConnectVia::Proxy).await?;
    println!("{}", url);
    Ok(())
}
