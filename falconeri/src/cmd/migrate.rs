//! The `migrate` subcommand.

use falconeri_common::{db, prelude::*};

/// Run the `migrate` subcommand.
#[instrument(level = "trace")]
pub async fn run() -> Result<()> {
    let conn = db::async_connect(ConnectVia::Proxy).await?;
    db::run_pending_migrations(conn)?;
    Ok(())
}
