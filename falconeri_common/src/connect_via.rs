//! How should we connect to PostgreSQL and `falconerid`?

use backon::{BlockingRetryable, ExponentialBuilder, Retryable};
use std::future::Future;
use std::time::Duration;

use crate::prelude::*;

/// How should we connect to the database?
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectVia {
    /// Assume we're connecting via a `kubectl proxy`.
    Proxy,
    /// Assume we're connecting via internal cluster networking and DNS.
    Cluster,
}

impl ConnectVia {
    /// Should we retry failed connections?
    #[tracing::instrument(level = "trace")]
    pub fn should_retry_by_default(self) -> bool {
        match self {
            // When we're connected via a proxy from outside the cluster, it's
            // generally better to just pass errors straight through
            // immediately.
            ConnectVia::Proxy => false,
            // When we're running on the cluster, we want to retry network
            // operations by default, because:
            //
            // 1. Kubernetes cluster DNS is extremely flaky, and
            // 2. Cluster operations may involve 1000+ worker-hours. At this
            //    scale, something will inevitably break.
            ConnectVia::Cluster => true,
        }
    }

    /// Create a backoff configuration matching our previous behavior.
    fn backoff_config() -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(500))
            .with_jitter()
            .without_max_times()
    }

    /// Run the function `f`. If `self.should_retry_by_default()` is true, retry
    /// failures using exponential backoff. Return either the result or the final
    /// final failure.
    #[tracing::instrument(skip(f), level = "trace")]
    pub fn retry_if_appropriate<F, T>(self, f: F) -> Result<T>
    where
        F: FnMut() -> Result<T>,
    {
        f.retry(Self::backoff_config())
            .when(|_| self.should_retry_by_default())
            .notify(|err, _dur| error!("retrying after error: {}", err))
            .call()
    }

    /// Async version of `retry_if_appropriate` for use with async HTTP clients.
    #[tracing::instrument(skip(f), level = "trace")]
    pub async fn retry_if_appropriate_async<F, Fut, T>(self, f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        f.retry(Self::backoff_config())
            .when(|_| self.should_retry_by_default())
            .notify(|err, _dur| error!("retrying after error: {}", err))
            .await
    }
}
