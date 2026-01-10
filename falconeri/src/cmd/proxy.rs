//! The `proxy` subcommand.

use std::time::{Duration, Instant};

use falconeri_common::{
    futures_util::future::join_all, kubernetes, prelude::*, tokio,
};
use tokio::{
    process::{Child, Command},
    sync::broadcast,
};

/// A single port-forward connection with automatic reconnection.
struct PortForward {
    /// Kubernetes resource (e.g., "svc/falconeri-postgres").
    service: String,
    /// Port mapping (e.g., "5432:5432").
    port_mapping: String,
    /// Human-readable name for logging.
    name: String,
}

impl PortForward {
    fn new(service: &str, port_mapping: &str, name: &str) -> Self {
        Self {
            service: service.to_owned(),
            port_mapping: port_mapping.to_owned(),
            name: name.to_owned(),
        }
    }

    /// Run the port-forward with automatic reconnection until shutdown.
    ///
    /// We use a hand-rolled retry loop instead of `backon` because our use
    /// case is unusual: we want to run indefinitely and restart on exit,
    /// rather than retry until success. The backoff is for repeated failures,
    /// but "success" means running forever until shutdown.
    #[instrument(level = "debug", skip_all, fields(name = %self.name))]
    async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        const STABLE_THRESHOLD: Duration = Duration::from_secs(10);

        let mut backoff = INITIAL_BACKOFF;

        loop {
            let start = Instant::now();
            info!("Starting port-forward for {}", self.name);

            match self.run_once(&mut shutdown_rx).await {
                Ok(ExitReason::ShutdownRequested) => {
                    debug!("Shutdown requested, stopping {}", self.name);
                    return;
                }
                Ok(ExitReason::ProcessExited) => {
                    if start.elapsed() > STABLE_THRESHOLD {
                        backoff = INITIAL_BACKOFF;
                    }
                    warn!("{} disconnected, reconnecting in {:?}", self.name, backoff);
                }
                Err(e) => {
                    warn!("{} error: {:#}, retrying in {:?}", self.name, e, backoff);
                }
            }

            // Wait before reconnecting, but respond to shutdown.
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.recv() => {
                    debug!("Shutdown during backoff for {}", self.name);
                    return;
                }
            }

            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Run a single port-forward attempt.
    #[instrument(level = "trace", skip_all)]
    async fn run_once(
        &self,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) -> Result<ExitReason> {
        let mut child: Child = Command::new("kubectl")
            .args(["port-forward", &self.service, &self.port_mapping])
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("failed to start port-forward for {}", self.name)
            })?;

        tokio::select! {
            status = child.wait() => {
                let status = status.context("failed to wait for kubectl")?;
                if status.success() {
                    Ok(ExitReason::ProcessExited)
                } else {
                    Err(format_err!("kubectl exited with status: {}", status))
                }
            }
            _ = shutdown_rx.recv() => {
                // Child will be killed on drop due to kill_on_drop(true).
                Ok(ExitReason::ShutdownRequested)
            }
        }
    }
}

/// Why did run_once complete?
enum ExitReason {
    /// The kubectl process exited (needs reconnection).
    ProcessExited,
    /// Shutdown was requested (stop reconnecting).
    ShutdownRequested,
}

/// Run the proxy command.
#[instrument(level = "trace")]
pub async fn run() -> Result<()> {
    // Build list of port-forwards to create.
    let mut forwards = vec![
        PortForward::new("svc/falconeri-postgres", "5432:5432", "postgres"),
        PortForward::new("svc/falconerid", "8089:8089", "falconerid"),
    ];

    // Check for optional MinIO.
    if kubernetes::resource_exists("svc/falconeri-minio").await? {
        forwards.push(PortForward::new(
            "svc/falconeri-minio",
            "9000:9000",
            "minio-api",
        ));
        forwards.push(PortForward::new(
            "svc/falconeri-minio",
            "9001:9001",
            "minio-console",
        ));
    }

    info!(
        "Starting proxy for {} service(s): {}",
        forwards.len(),
        forwards
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Create shutdown channel.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Spawn port-forward tasks.
    let mut handles = Vec::new();
    for forward in forwards {
        let shutdown_rx = shutdown_tx.subscribe();
        let handle = tokio::spawn(async move {
            forward.run(shutdown_rx).await;
        });
        handles.push(handle);
    }

    // Wait for shutdown signal.
    shutdown_signal().await;
    info!("Shutdown signal received, stopping all port-forwards...");

    // Signal all tasks to stop.
    let _ = shutdown_tx.send(());

    // Wait for all tasks to complete (with timeout).
    let shutdown_timeout = Duration::from_secs(5);
    match tokio::time::timeout(shutdown_timeout, join_all(handles)).await {
        Ok(_) => info!("All port-forwards stopped cleanly"),
        Err(_) => warn!("Shutdown timed out, some processes may still be running"),
    }

    Ok(())
}

/// Wait for a shutdown signal (Ctrl-C or SIGTERM).
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            debug!("Received Ctrl-C");
        }
        _ = terminate => {
            debug!("Received SIGTERM");
        }
    }
}
