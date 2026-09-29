//! Support for AWS S3 storage using the native object_store crate.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::{
    ClientConfigKey, ObjectStore,
    aws::{AmazonS3Builder, AmazonS3ConfigKey},
};

use super::{CloudStorage, StorageClientPolicy};
use crate::{
    kubernetes::{
        base64_encoded_optional_secret_string, base64_encoded_secret_string,
        kubectl_secret,
    },
    prelude::*,
    secret::Secret,
    storage::parse_cloud_storage_uri,
};

/// An S3 secret fetched from Kubernetes. This can be fetched using
/// `kubectl_secret`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
struct S3SecretData {
    /// Our `AWS_ACCESS_KEY_ID` value.
    #[serde(with = "base64_encoded_secret_string")]
    aws_access_key_id: String,
    /// Our `AWS_SECRET_ACCESS_KEY` value.
    #[serde(with = "base64_encoded_secret_string")]
    aws_secret_access_key: String,
    /// Optional custom endpoint URL for S3-compatible services like MinIO.
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    aws_endpoint_url: Option<String>,
    /// Optional AWS region.
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    aws_region: Option<String>,
}

/// Backend for talking to AWS S3 using native Rust (no external CLI).
pub struct S3Storage {
    store: Arc<dyn ObjectStore>,
    bucket: String,
}

impl S3Storage {
    /// Create a new `S3Storage` backend.
    ///
    /// The `bucket_uri` parameter should be any `s3://` URI within the bucket
    /// we want to access. The bucket name is extracted from this URI.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(secrets: &[Secret], bucket_uri: &str) -> Result<Self> {
        let secret = secrets
            .iter()
            .find(|s| matches!(s, Secret::Env { env_var, .. } if env_var == "AWS_ACCESS_KEY_ID"));
        let secret_data: Option<S3SecretData> =
            if let Some(Secret::Env { name, .. }) = secret {
                Some(kubectl_secret(name).await?)
            } else {
                None
            };

        Self::build_from_secret(secret_data, bucket_uri)
    }

    /// Construct a new `S3Storage` backend using an AWS access key from
    /// the Kubernetes secret `secret_name`.
    ///
    /// The `bucket_uri` parameter should be any `s3://` URI within the bucket
    /// we want to access. The bucket name is extracted from this URI.
    #[instrument(skip_all, fields(secret_name = %secret_name), level = "trace")]
    pub async fn new_with_secret(secret_name: &str, bucket_uri: &str) -> Result<Self> {
        let secret_data: Option<S3SecretData> = kubectl_secret(secret_name).await?;
        Self::build_from_secret(secret_data, bucket_uri)
    }

    fn build_from_secret(
        secret_data: Option<S3SecretData>,
        bucket_uri: &str,
    ) -> Result<Self> {
        let (_scheme, bucket, _path) = parse_cloud_storage_uri(bucket_uri)?;

        // Use from_env() to pick up AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
        // AWS_ENDPOINT_URL, and AWS_REGION from environment variables.
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_storage_timeouts()
            .with_allow_http(true);

        if let Some(ref secret) = secret_data {
            builder = builder
                .with_access_key_id(&secret.aws_access_key_id)
                .with_secret_access_key(&secret.aws_secret_access_key);

            if let Some(ref endpoint_url) = secret.aws_endpoint_url {
                builder = builder.with_endpoint(endpoint_url);
            }

            if let Some(ref region) = secret.aws_region {
                builder = builder.with_region(region);
            }
        }

        let store = builder.build().context("failed to build S3 client")?;

        Ok(S3Storage {
            store: Arc::new(store),
            bucket: bucket.to_owned(),
        })
    }
}

impl StorageClientPolicy for AmazonS3Builder {
    fn client_option(self, key: ClientConfigKey, value: &'static str) -> Self {
        self.with_config(AmazonS3ConfigKey::Client(key), value)
    }
}

impl fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Storage")
            .field("bucket", &self.bucket)
            .finish()
    }
}

#[async_trait]
impl CloudStorage for S3Storage {
    fn scheme(&self) -> &'static str {
        "s3"
    }

    fn store(&self) -> &dyn ObjectStore {
        &self.store
    }
}
