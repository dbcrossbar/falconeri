//! Support for Google Cloud Storage using the native object_store crate.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::{
    ClientConfigKey, ObjectStore,
    gcp::{GoogleCloudStorageBuilder, GoogleConfigKey},
};

use super::{CloudStorage, StorageClientPolicy};
use crate::{
    kubernetes::{base64_encoded_optional_secret_string, kubectl_secret},
    prelude::*,
    secret::Secret,
    storage::parse_cloud_storage_uri,
};

/// A GCS secret fetched from Kubernetes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcsSecretData {
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    #[serde(rename = "GOOGLE_SERVICE_ACCOUNT_KEY")]
    service_account_key: Option<String>,
}

/// Backend for talking to Google Cloud Storage using native Rust (no gsutil).
pub struct GoogleCloudStorage {
    store: Arc<dyn ObjectStore>,
    bucket: String,
}

impl GoogleCloudStorage {
    /// Create a new `GoogleCloudStorage` backend.
    ///
    /// The `bucket_uri` parameter should be any `gs://` URI within the bucket
    /// we want to access. The bucket name is extracted from this URI.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(secrets: &[Secret], bucket_uri: &str) -> Result<Self> {
        let secret = secrets
            .iter()
            .find(|s| matches!(s, Secret::Env { env_var, .. } if env_var == "GOOGLE_SERVICE_ACCOUNT_KEY"));
        let secret_data: Option<GcsSecretData> =
            if let Some(Secret::Env { name, .. }) = secret {
                kubectl_secret(name).await?
            } else {
                None
            };

        Self::build_from_secret(secret_data, bucket_uri)
    }

    fn build_from_secret(
        secret_data: Option<GcsSecretData>,
        bucket_uri: &str,
    ) -> Result<Self> {
        let (_, bucket, _) = parse_cloud_storage_uri(bucket_uri)?;

        let mut builder = GoogleCloudStorageBuilder::from_env()
            .with_bucket_name(bucket)
            .with_storage_timeouts();

        // First try secret_data from Kubernetes (used by falconerid).
        if let Some(ref secret) = secret_data {
            if let Some(ref service_account_key) = secret.service_account_key {
                builder = builder.with_service_account_key(service_account_key);
            }
        } else if let Ok(service_account_key) =
            std::env::var("GOOGLE_SERVICE_ACCOUNT_KEY")
        {
            // Fall back to environment variable (used by worker pods where
            // secrets are mounted as env vars).
            builder = builder.with_service_account_key(&service_account_key);
        }

        let store = builder.build().context("failed to build GCS client")?;

        Ok(GoogleCloudStorage {
            store: Arc::new(store),
            bucket: bucket.to_owned(),
        })
    }
}

impl StorageClientPolicy for GoogleCloudStorageBuilder {
    fn client_option(self, key: ClientConfigKey, value: &'static str) -> Self {
        self.with_config(GoogleConfigKey::Client(key), value)
    }
}

impl fmt::Debug for GoogleCloudStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCloudStorage")
            .field("bucket", &self.bucket)
            .finish()
    }
}

#[async_trait]
impl CloudStorage for GoogleCloudStorage {
    fn scheme(&self) -> &'static str {
        "gs"
    }

    fn store(&self) -> &dyn ObjectStore {
        &self.store
    }
}
