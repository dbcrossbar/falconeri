//! The `proxy` subcommand.

use falconeri_common::{kubernetes, prelude::*};

/// Run our proxy.
#[instrument(level = "trace")]
pub async fn run() -> Result<()> {
    let postgres_handle =
        tokio::spawn(async { forward("svc/falconeri-postgres", "5432:5432").await });
    let falconerid_handle =
        tokio::spawn(async { forward("svc/falconerid", "8089:8089").await });

    // Check if MinIO is deployed and forward its ports if so.
    let minio_exists = kubernetes::resource_exists("svc/falconeri-minio").await?;
    let minio_api_handle = if minio_exists {
        Some(tokio::spawn(async {
            forward("svc/falconeri-minio", "9000:9000").await
        }))
    } else {
        None
    };
    let minio_console_handle = if minio_exists {
        Some(tokio::spawn(async {
            forward("svc/falconeri-minio", "9001:9001").await
        }))
    } else {
        None
    };

    // Wait for any to complete (they run forever until interrupted).
    tokio::select! {
        result = postgres_handle => {
            result.context("postgres proxy task panicked")??;
        }
        result = falconerid_handle => {
            result.context("falconerid proxy task panicked")??;
        }
        result = async { minio_api_handle.unwrap().await }, if minio_api_handle.is_some() => {
            result.context("minio api proxy task panicked")??;
        }
        result = async { minio_console_handle.unwrap().await }, if minio_console_handle.is_some() => {
            result.context("minio console proxy task panicked")??;
        }
    }
    Ok(())
}

#[instrument(level = "debug")]
async fn forward(service: &str, port: &str) -> Result<()> {
    kubernetes::kubectl(&["port-forward", service, port]).await
}
