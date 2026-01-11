//! Support for AWS S3 storage using the official AWS SDK.

use std::fs;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::{config::Credentials, Client};
use bytes::Bytes;
use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::AsyncWriteExt;
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

/// An S3 secret fetched from Kubernetes. This can be fetched using
/// `kubernetes_secret`.
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
    /// Optional AWS region (defaults to us-east-1 for MinIO compatibility).
    #[serde(default, with = "base64_encoded_optional_secret_string")]
    aws_default_region: Option<String>,
}

/// Backend for talking to AWS S3 using the official AWS SDK.
pub struct S3Storage {
    client: Client,
}

impl S3Storage {
    /// Create a new `S3Storage` backend.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(secrets: &[Secret]) -> Result<Self> {
        let secret = secrets.iter().find(|s| {
            matches!(s, Secret::Env { env_var, .. } if env_var == "AWS_ACCESS_KEY_ID")
        });
        let secret_data: Option<S3SecretData> =
            if let Some(Secret::Env { name, .. }) = secret {
                Some(kubectl_secret(name).await?)
            } else {
                None
            };
        Self::new_with_secret_data(secret_data).await
    }

    /// Construct a new `S3Storage` backend, using an AWS access key from
    /// the Kubernetes secret `secret_name`.
    #[instrument(skip_all, fields(secret_name = %secret_name), level = "trace")]
    pub async fn new_with_secret(secret_name: &str) -> Result<Self> {
        let secret_data: Option<S3SecretData> = kubectl_secret(secret_name).await?;
        Self::new_with_secret_data(secret_data).await
    }

    /// Internal constructor that builds the AWS SDK client.
    async fn new_with_secret_data(secret_data: Option<S3SecretData>) -> Result<Self> {
        let client = match secret_data {
            Some(ref data) => {
                let credentials = Credentials::new(
                    &data.aws_access_key_id,
                    &data.aws_secret_access_key,
                    None, // session token
                    None, // expiry
                    "falconeri",
                );
                let region = data
                    .aws_default_region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string());

                let mut config_builder = aws_sdk_s3::Config::builder()
                    .behavior_version(BehaviorVersion::latest())
                    .credentials_provider(credentials)
                    .region(aws_sdk_s3::config::Region::new(region));

                if let Some(endpoint_url) = &data.aws_endpoint_url {
                    config_builder = config_builder
                        .endpoint_url(endpoint_url)
                        .force_path_style(true);
                }

                Client::from_conf(config_builder.build())
            }
            None => {
                // Fall back to default credential chain (env vars, ~/.aws/credentials, etc.)
                let sdk_config =
                    aws_config::load_defaults(BehaviorVersion::latest()).await;
                Client::new(&sdk_config)
            }
        };
        Ok(S3Storage { client })
    }
}

impl fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Don't include secrets in the debug output, for trace mode.
        f.debug_struct("S3Storage").finish()
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

        let mut results = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request =
                self.client.list_objects_v2().bucket(bucket).prefix(&prefix);

            if let Some(token) = continuation_token.take() {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("failed to list objects in {}", uri))?;

            if let Some(contents) = response.contents {
                for obj in contents {
                    if let Some(obj_key) = obj.key {
                        // Remove the directory itself.
                        if obj_key != prefix {
                            results.push(format!("s3://{}/{}", bucket, obj_key));
                        }
                    }
                }
            }

            if response.is_truncated == Some(true) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(results)
    }

    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        trace!("downloading {} to {}", uri, local_path.display());
        let (bucket, key) = parse_s3_url(uri)?;

        if uri.ends_with('/') {
            // Directory sync: list all objects and download each one.
            fs::create_dir_all(local_path)
                .context("cannot create local download directory")?;

            let objects = self.list(uri).await?;
            for object_uri in objects {
                let (_, obj_key) = parse_s3_url(&object_uri)?;
                // Calculate relative path from the prefix.
                let relative_path = obj_key
                    .strip_prefix(key)
                    .unwrap_or(obj_key)
                    .trim_start_matches('/');
                let dest_path = local_path.join(relative_path);

                // Create parent directories if needed.
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)
                        .context("cannot create local download directory")?;
                }

                self.download_file(bucket, obj_key, &dest_path).await?;
            }
        } else {
            // Single file download.
            if let Some(parent) = local_path.parent() {
                fs::create_dir_all(parent)
                    .context("cannot create local download directory")?;
            }
            self.download_file(bucket, key, local_path).await?;
        }
        Ok(())
    }

    #[instrument(skip_all, fields(local_path = %local_path.display(), uri = %uri), level = "trace")]
    async fn sync_up(&self, local_path: &Path, uri: &str) -> Result<()> {
        trace!("uploading {} to {}", local_path.display(), uri);
        let (bucket, key) = parse_s3_url(uri)?;

        // Walk the local directory and upload each file.
        for entry in WalkDir::new(local_path) {
            let entry = entry.context("error walking local directory")?;
            if entry.file_type().is_file() {
                let relative_path = entry
                    .path()
                    .strip_prefix(local_path)
                    .context("failed to compute relative path")?;
                let dest_key = if key.is_empty() {
                    relative_path.to_string_lossy().to_string()
                } else {
                    format!(
                        "{}/{}",
                        key.trim_end_matches('/'),
                        relative_path.to_string_lossy()
                    )
                };

                self.upload_file(entry.path(), bucket, &dest_key).await?;
            }
        }
        Ok(())
    }
}

impl S3Storage {
    /// Download a single file from S3.
    async fn download_file(
        &self,
        bucket: &str,
        key: &str,
        local_path: &Path,
    ) -> Result<()> {
        trace!(
            "downloading s3://{}/{} to {}",
            bucket,
            key,
            local_path.display()
        );

        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("failed to download s3://{}/{}", bucket, key))?;

        let body = response
            .body
            .collect()
            .await
            .context("failed to read S3 response body")?;

        let mut file =
            tokio::fs::File::create(local_path).await.with_context(|| {
                format!("failed to create file {}", local_path.display())
            })?;

        file.write_all(&body.into_bytes())
            .await
            .with_context(|| format!("failed to write to {}", local_path.display()))?;

        Ok(())
    }

    /// Upload a single file to S3.
    async fn upload_file(
        &self,
        local_path: &Path,
        bucket: &str,
        key: &str,
    ) -> Result<()> {
        trace!(
            "uploading {} to s3://{}/{}",
            local_path.display(),
            bucket,
            key
        );

        let body = tokio::fs::read(local_path)
            .await
            .with_context(|| format!("failed to read {}", local_path.display()))?;

        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(Bytes::from(body).into())
            .send()
            .await
            .with_context(|| format!("failed to upload to s3://{}/{}", bucket, key))?;

        Ok(())
    }
}

/// Parse an S3 URL.
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
