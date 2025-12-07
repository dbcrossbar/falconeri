//! Database utilities.

use std::{env, fs::read_to_string};

use anyhow::anyhow;
pub use diesel_async::{
    pooled_connection::deadpool::{
        Object as PooledConnection, Pool as AsyncPoolInner,
    },
    AsyncPgConnection,
};
use diesel_async::{
    pooled_connection::AsyncDieselConnectionManager, AsyncConnection,
    AsyncMigrationHarness,
};
use diesel_migrations::MigrationHarness;

use crate::{
    kubernetes::{base64_encoded_secret_string, kubectl_secret},
    prelude::*,
};

/// Embed our migrations directly into the executable. We use a
/// submodule so we can configure warnings.
#[allow(unused_imports)]
mod migrations {
    use diesel_migrations::{embed_migrations, EmbeddedMigrations};
    pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations");
}

/// The data we store in our secret.
#[derive(Debug, Deserialize)]
struct FalconeriSecretData {
    #[serde(with = "base64_encoded_secret_string", rename = "POSTGRES_PASSWORD")]
    postgres_password: String,
}

/// Look up our PostgreSQL password in our cluster's `falconeri` secret.
#[instrument(level = "trace")]
pub async fn postgres_password(via: ConnectVia) -> Result<String> {
    match via {
        ConnectVia::Proxy => {
            trace!("Fetching POSTGRES_PASSWORD from secret `falconeri`");
            // We implement the following as Rust:
            //
            // kubectl get secret falconeri -o json |
            //     jq -r .data.POSTGRES_PASSWORD |
            //     base64 --decode
            let secret_data: FalconeriSecretData = kubectl_secret("falconeri").await?;
            Ok(secret_data.postgres_password)
        }
        ConnectVia::Cluster => {
            // This should be mounted into our container.
            Ok(read_to_string("/etc/falconeri/secrets/POSTGRES_PASSWORD")
                .context("could not read /etc/falconeri/secrets/POSTGRES_PASSWORD")?)
        }
    }
}

/// Get an appropriate database URL.
#[instrument(level = "trace")]
pub async fn database_url(via: ConnectVia) -> Result<String> {
    // Check the environment first, so it can be overridden for testing outside
    // of a full Kubernetes setup.
    if let Ok(database_url) = env::var("DATABASE_URL") {
        return Ok(database_url);
    }

    // Build a URL.
    let password = postgres_password(via).await?;
    match via {
        ConnectVia::Proxy => {
            let host = env::var("FALCONERI_PROXY_HOST")
                .unwrap_or_else(|_| "localhost".to_string());
            Ok(format!("postgres://postgres:{}@{}:5432/", password, host))
        }
        ConnectVia::Cluster => Ok(format!(
            "postgres://postgres:{}@falconeri-postgres:5432/",
            password,
        )),
    }
}

/// An async database connection pool using deadpool.
pub type AsyncPool = AsyncPoolInner<AsyncPgConnection>;

/// A pooled async database connection.
pub type AsyncPooledConn = PooledConnection<AsyncPgConnection>;

/// Create an async connection pool using the specified parameters.
#[instrument(level = "trace")]
pub async fn async_pool(pool_size: usize, via: ConnectVia) -> Result<AsyncPool> {
    let database_url = database_url(via).await?;
    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
    AsyncPoolInner::builder(config)
        .max_size(pool_size)
        .build()
        .context("could not create async database pool")
}

/// Establish a direct async connection to the database.
///
/// This is used for migrations where we need a raw connection rather than
/// a pooled one.
#[instrument(level = "trace")]
pub async fn async_connect(via: ConnectVia) -> Result<AsyncPgConnection> {
    let url = database_url(via).await?;
    via.retry_if_appropriate_async(|| async {
        AsyncPgConnection::establish(&url)
            .await
            .context("Error connecting to database")
    })
    .await
}

/// Run any pending migrations.
///
/// Uses `AsyncMigrationHarness` which internally uses `block_in_place` to run
/// diesel's sync migration infrastructure without blocking the async runtime.
#[instrument(skip_all, level = "trace")]
pub fn run_pending_migrations(conn: AsyncPgConnection) -> Result<AsyncPgConnection> {
    debug!("Running pending migrations");
    let mut harness = AsyncMigrationHarness::new(conn);
    harness
        .run_pending_migrations(migrations::MIGRATIONS)
        .map_err(|e| anyhow!("could not run migrations: {}", e))?;
    Ok(harness.into_inner())
}
