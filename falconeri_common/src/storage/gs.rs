//! Support for Google Cloud Storage using the native object_store crate.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use lazy_static::lazy_static;
use object_store::{
    gcp::GoogleCloudStorageBuilder, path::Path as ObjectPath, ObjectStore,
};
use regex::Regex;
use tokio::fs as async_fs;
use walkdir::WalkDir;

use super::{stream_download_to_file, stream_upload_from_file, CloudStorage};
use crate::{
    kubernetes::{base64_encoded_optional_secret_string, kubectl_secret},
    prelude::*,
    secret::Secret,
};

/// A GCS secret fetched from Kubernetes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcsSecretData {
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    #[serde(rename = "GOOGLE_SERVICE_ACCOUNT_KEY")]
    service_account_key: Option<String>,
}

/// Parse a GCS URL into (bucket, key).
fn parse_gs_url(url: &str) -> Result<(&str, &str)> {
    lazy_static! {
        static ref RE: Regex = Regex::new("^gs://(?P<bucket>[^/]+)(?:/(?P<key>.*))?$")
            .expect("couldn't parse built-in regex");
    }

    let caps = RE
        .captures(url)
        .ok_or_else(|| format_err!("the URL {:?} could not be parsed", url))?;
    let bucket = caps
        .name("bucket")
        .expect("missing hard-coded capture???")
        .as_str();
    let key = caps.name("key").map(|m| m.as_str()).unwrap_or("");

    Ok((bucket, key))
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
        let (bucket, _) = parse_gs_url(bucket_uri)?;

        let mut builder =
            GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);

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

impl fmt::Debug for GoogleCloudStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCloudStorage")
            .field("bucket", &self.bucket)
            .finish()
    }
}

#[async_trait]
impl CloudStorage for GoogleCloudStorage {
    #[instrument(skip_all, fields(uri = %uri), level = "trace")]
    async fn list(&self, uri: &str) -> Result<Vec<String>> {
        trace!("listing {}", uri);

        let (bucket, key) = parse_gs_url(uri)?;
        let mut prefix = key.to_owned();
        if !key.is_empty() && !key.ends_with('/') {
            prefix.push('/');
        }

        let prefix_path = if prefix.is_empty() {
            None
        } else {
            Some(ObjectPath::from(prefix.as_str()))
        };

        let mut results = Vec::new();
        let mut stream = self.store.list(prefix_path.as_ref());

        while let Some(meta) = stream
            .try_next()
            .await
            .context("error listing GCS objects")?
        {
            let path_str = meta.location.to_string();
            if path_str != prefix {
                results.push(format!("gs://{}/{}", bucket, path_str));
            }
        }

        Ok(results)
    }

    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        trace!("downloading {} to {}", uri, local_path.display());

        let (_, key) = parse_gs_url(uri)?;

        if uri.ends_with('/') {
            // We have a directory. If our source URI ends in `/`, so should our
            // `local_path`, since we generate these ourselves.
            async_fs::create_dir_all(local_path)
                .await
                .context("cannot create local download directory")?;

            let prefix = ObjectPath::from(key);
            let mut stream = self.store.list(Some(&prefix));

            while let Some(meta) = stream
                .try_next()
                .await
                .context("error listing GCS objects")?
            {
                let object_key = meta.location.to_string();
                let relative_path = object_key
                    .strip_prefix(key)
                    .unwrap_or(&object_key)
                    .trim_start_matches('/');

                if relative_path.is_empty() {
                    continue;
                }

                let file_path = local_path.join(relative_path);

                if let Some(parent) = file_path.parent() {
                    async_fs::create_dir_all(parent)
                        .await
                        .context("cannot create local subdirectory")?;
                }

                stream_download_to_file(&self.store, &meta.location, &file_path)
                    .await?;
            }
        } else {
            // We have a file.
            if let Some(parent) = local_path.parent() {
                async_fs::create_dir_all(parent)
                    .await
                    .context("cannot create local download directory")?;
            }

            let object_path = ObjectPath::from(key);
            stream_download_to_file(&self.store, &object_path, local_path).await?;
        }

        Ok(())
    }

    #[instrument(skip_all, fields(local_path = %local_path.display(), uri = %uri), level = "trace")]
    async fn sync_up(&self, local_path: &Path, uri: &str) -> Result<()> {
        trace!("uploading {} to {}", local_path.display(), uri);

        let (_, key) = parse_gs_url(uri)?;
        let base_key = key.trim_end_matches('/');

        for entry in WalkDir::new(local_path).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }

            let file_path = entry.path();
            let relative_path = file_path
                .strip_prefix(local_path)
                .context("failed to compute relative path")?;

            let object_key = if base_key.is_empty() {
                relative_path.to_string_lossy().to_string()
            } else {
                format!("{}/{}", base_key, relative_path.to_string_lossy())
            };

            let object_path = ObjectPath::from(object_key.as_str());
            stream_upload_from_file(&self.store, file_path, &object_path)
                .await
                .with_context(|| format!("error uploading to GCS: {}", object_key))?;
        }

        Ok(())
    }
}

#[test]
fn url_parsing() {
    assert_eq!(parse_gs_url("gs://top-level").unwrap(), ("top-level", ""));
    assert_eq!(parse_gs_url("gs://top-level/").unwrap(), ("top-level", ""));
    assert_eq!(
        parse_gs_url("gs://top-level/path").unwrap(),
        ("top-level", "path")
    );
    assert_eq!(
        parse_gs_url("gs://top-level/path/").unwrap(),
        ("top-level", "path/")
    );
    assert!(parse_gs_url("s3://foo/").is_err());
}
