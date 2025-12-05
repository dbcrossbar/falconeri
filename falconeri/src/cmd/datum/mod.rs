//! The `datum` subcommand.

use clap::Subcommand;
use falconeri_common::prelude::*;

mod describe;

/// `datum` options.
#[derive(Debug, Subcommand)]
pub enum Opt {
    /// Describe a specific job.
    #[command(name = "describe")]
    Describe {
        /// The UUID of the datum to describe.
        id: Uuid,
    },
}

/// Run the `job` subcommand.
pub async fn run(opt: &Opt) -> Result<()> {
    match opt {
        Opt::Describe { id } => describe::run(*id).await,
    }
}
