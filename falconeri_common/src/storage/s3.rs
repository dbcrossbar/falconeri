//! Support for AWS S3 storage using the native AWS SDK.

use std::fs;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_smithy_types::byte_stream::ByteStream;
use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::AsyncWriteExt;

use super::CloudStorage;
use crate::{prelude::*, secret::Secret};

/// Backend for talking to AWS S3 using the native AWS SDK.
///
/// Credentials are read from the environment via the standard AWS credential
/// chain: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, and
/// optionally `AWS_ENDPOINT_URL` for S3-compatible services like MinIO.
pub struct S3Storage {
    client: Client,
}

impl S3Storage {
    /// Create a new `S3Storage` backend.
    ///
    /// The SDK automatically picks up credentials from environment variables
    /// (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY) and custom endpoints
    /// (AWS_ENDPOINT_URL for MinIO).
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(_secrets: &[Secret]) -> Result<Self> {
        // The SDK automatically reads AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
        // AWS_REGION, and AWS_ENDPOINT_URL from the environment.
        let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
        let client = Client::new(&config);
        Ok(S3Storage { client })
    }

    /// Construct a new `S3Storage` backend.
    ///
    /// This is a simplified constructor that ignores the secret_name parameter
    /// since credentials are now read from the environment.
    #[instrument(skip_all, fields(secret_name = %_secret_name), level = "trace")]
    pub async fn new_with_secret(_secret_name: &str) -> Result<Self> {
        Self::new(&[]).await
    }

    /// Download a single file from S3 to a local path.
    #[instrument(skip_all, fields(bucket = %bucket, key = %key, local_path = %local_path.display()), level = "trace")]
    async fn download_file(
        &self,
        bucket: &str,
        key: &str,
        local_path: &Path,
    ) -> Result<()> {
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("failed to get object s3://{bucket}/{key}"))?;

        let mut file =
            tokio::fs::File::create(local_path).await.with_context(|| {
                format!("failed to create local file {}", local_path.display())
            })?;

        let mut stream = response.body.into_async_read();
        tokio::io::copy(&mut stream, &mut file)
            .await
            .with_context(|| {
                format!(
                    "failed to write s3://{bucket}/{key} to {}",
                    local_path.display()
                )
            })?;

        file.flush().await?;
        Ok(())
    }

    /// Upload a single file from a local path to S3.
    #[instrument(skip_all, fields(local_path = %local_path.display(), bucket = %bucket, key = %key), level = "trace")]
    async fn upload_file(
        &self,
        local_path: &Path,
        bucket: &str,
        key: &str,
    ) -> Result<()> {
        let body = ByteStream::from_path(local_path).await.with_context(|| {
            format!("failed to read local file {}", local_path.display())
        })?;

        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to upload {} to s3://{bucket}/{key}",
                    local_path.display()
                )
            })?;

        Ok(())
    }
}

impl fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
                .with_context(|| format!("failed to list objects in {uri}"))?;

            if let Some(contents) = response.contents {
                for obj in contents {
                    if let Some(obj_key) = obj.key {
                        // Skip the directory itself.
                        if obj_key != prefix {
                            results.push(format!("s3://{bucket}/{obj_key}"));
                        }
                    }
                }
            }

            // Check if there are more results to fetch.
            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
                if continuation_token.is_none() {
                    break;
                }
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
            // Directory sync: list all objects with the prefix and download each.
            fs::create_dir_all(local_path)
                .context("cannot create local download directory")?;

            let objects = self.list(uri).await?;
            for obj_uri in objects {
                let (_, obj_key) = parse_s3_url(&obj_uri)?;
                // Calculate the relative path from the prefix.
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

        if local_path.is_dir() {
            // Directory sync: walk the directory and upload each file.
            for entry in walkdir::WalkDir::new(local_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                let file_path = entry.path();
                let relative_path = file_path
                    .strip_prefix(local_path)
                    .context("failed to compute relative path")?;
                let obj_key = if key.is_empty() {
                    relative_path.to_string_lossy().to_string()
                } else {
                    format!(
                        "{}/{}",
                        key.trim_end_matches('/'),
                        relative_path.to_string_lossy()
                    )
                };

                self.upload_file(file_path, bucket, &obj_key).await?;
            }
        } else {
            // Single file upload.
            self.upload_file(local_path, bucket, key).await?;
        }

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
