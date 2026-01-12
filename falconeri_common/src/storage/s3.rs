//! Support for AWS S3 storage using the native object_store crate.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use lazy_static::lazy_static;
use object_store::{
    aws::AmazonS3Builder, path::Path as ObjectPath, ObjectStore, ObjectStoreExt,
    PutPayload,
};
use regex::Regex;
use tokio::{fs as async_fs, io::AsyncWriteExt};
use walkdir::WalkDir;

use super::CloudStorage;
use crate::{
    kubernetes::{
        base64_encoded_optional_secret_string, base64_encoded_secret_string,
        kubectl_secret,
    },
    prelude::*,
    secret::Secret,
};

/// An S3 secret fetched from Kubernetes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
struct S3SecretData {
    #[serde(with = "base64_encoded_secret_string")]
    aws_access_key_id: String,
    #[serde(with = "base64_encoded_secret_string")]
    aws_secret_access_key: String,
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    aws_endpoint_url: Option<String>,
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    aws_region: Option<String>,
}

/// Parse an S3 URL into (bucket, key).
fn parse_s3_url(url: &str) -> Result<(&str, &str)> {
    lazy_static! {
        static ref RE: Regex = Regex::new("^s3://(?P<bucket>[^/]+)(?:/(?P<key>.*))?$")
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

/// Backend for talking to AWS S3 using native Rust (no external CLI).
pub struct S3Storage {
    store: Arc<dyn ObjectStore>,
    bucket: String,
}

impl S3Storage {
    /// Create a new `S3Storage` backend.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(secrets: &[Secret], uri: &str) -> Result<Self> {
        let secret = secrets
            .iter()
            .find(|s| matches!(s, Secret::Env { env_var, .. } if env_var == "AWS_ACCESS_KEY_ID"));
        let secret_data: Option<S3SecretData> =
            if let Some(Secret::Env { name, .. }) = secret {
                Some(kubectl_secret(name).await?)
            } else {
                None
            };

        Self::build_from_secret(secret_data, uri)
    }

    /// Construct a new `S3Storage` backend using an AWS access key from
    /// the Kubernetes secret `secret_name`.
    #[instrument(skip_all, fields(secret_name = %secret_name), level = "trace")]
    pub async fn new_with_secret(secret_name: &str, uri: &str) -> Result<Self> {
        let secret_data: Option<S3SecretData> = kubectl_secret(secret_name).await?;
        Self::build_from_secret(secret_data, uri)
    }

    fn build_from_secret(
        secret_data: Option<S3SecretData>,
        uri: &str,
    ) -> Result<Self> {
        let (bucket, _) = parse_s3_url(uri)?;

        // Use from_env() to pick up AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
        // AWS_ENDPOINT_URL, and AWS_REGION from environment variables.
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
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

impl fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Storage")
            .field("bucket", &self.bucket)
            .finish()
    }
}

#[async_trait]
impl CloudStorage for S3Storage {
    #[instrument(skip_all, fields(uri = %uri), level = "trace")]
    async fn list(&self, uri: &str) -> Result<Vec<String>> {
        trace!("listing {}", uri);

        let (bucket, key) = parse_s3_url(uri)?;
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
            .context("error listing S3 objects")?
        {
            let path_str = meta.location.to_string();
            if path_str != prefix {
                results.push(format!("s3://{}/{}", bucket, path_str));
            }
        }

        Ok(results)
    }

    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        trace!("downloading {} to {}", uri, local_path.display());

        let (_, key) = parse_s3_url(uri)?;

        if uri.ends_with('/') {
            async_fs::create_dir_all(local_path)
                .await
                .context("cannot create local download directory")?;

            let prefix = ObjectPath::from(key);
            let mut stream = self.store.list(Some(&prefix));

            while let Some(meta) = stream
                .try_next()
                .await
                .context("error listing S3 objects")?
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

                let data = self
                    .store
                    .get(&meta.location)
                    .await
                    .context("error fetching S3 object")?
                    .bytes()
                    .await
                    .context("error reading S3 object bytes")?;

                let mut file = async_fs::File::create(&file_path)
                    .await
                    .context("cannot create local file")?;
                file.write_all(&data)
                    .await
                    .context("cannot write to local file")?;
            }
        } else {
            if let Some(parent) = local_path.parent() {
                async_fs::create_dir_all(parent)
                    .await
                    .context("cannot create local download directory")?;
            }

            let object_path = ObjectPath::from(key);
            let data = self
                .store
                .get(&object_path)
                .await
                .context("error fetching S3 object")?
                .bytes()
                .await
                .context("error reading S3 object bytes")?;

            let mut file = async_fs::File::create(local_path)
                .await
                .context("cannot create local file")?;
            file.write_all(&data)
                .await
                .context("cannot write to local file")?;
        }

        Ok(())
    }

    #[instrument(skip_all, fields(local_path = %local_path.display(), uri = %uri), level = "trace")]
    async fn sync_up(&self, local_path: &Path, uri: &str) -> Result<()> {
        trace!("uploading {} to {}", local_path.display(), uri);

        let (_, key) = parse_s3_url(uri)?;
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

            let data = async_fs::read(file_path)
                .await
                .with_context(|| format!("cannot read local file {:?}", file_path))?;

            let object_path = ObjectPath::from(object_key.as_str());
            self.store
                .put(&object_path, PutPayload::from(Bytes::from(data)))
                .await
                .with_context(|| format!("error uploading to S3: {}", object_key))?;
        }

        Ok(())
    }
}

#[test]
fn url_parsing() {
    assert_eq!(parse_s3_url("s3://top-level").unwrap(), ("top-level", ""));
    assert_eq!(parse_s3_url("s3://top-level/").unwrap(), ("top-level", ""));
    assert_eq!(
        parse_s3_url("s3://top-level/path").unwrap(),
        ("top-level", "path")
    );
    assert_eq!(
        parse_s3_url("s3://top-level/path/").unwrap(),
        ("top-level", "path/")
    );
    assert!(parse_s3_url("gs://foo/").is_err());
}
