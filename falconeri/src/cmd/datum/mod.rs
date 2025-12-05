//! The `datum` subcommand.

use falconeri_common::prelude::*;
use clap::Subcommand;

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
