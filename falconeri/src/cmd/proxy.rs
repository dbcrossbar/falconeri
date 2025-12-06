//! The `proxy` subcommand.

use falconeri_common::{kubernetes, prelude::*};

/// Run our proxy.
#[instrument(level = "trace")]
pub async fn run() -> Result<()> {
    let postgres_handle =
        tokio::spawn(async { forward("svc/falconeri-postgres", "5432:5432").await });
    let falconerid_handle =
        tokio::spawn(async { forward("svc/falconerid", "8089:8089").await });

    // Wait for either to complete (they run forever until interrupted).
    tokio::select! {
        result = postgres_handle => {
            result.context("postgres proxy task panicked")??;
        }
        result = falconerid_handle => {
            result.context("falconerid proxy task panicked")??;
        }
    }
    Ok(())
}

#[instrument(level = "debug")]
async fn forward(service: &str, port: &str) -> Result<()> {
    kubernetes::kubectl(&["port-forward", service, port]).await
}
