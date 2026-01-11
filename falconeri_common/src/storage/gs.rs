//! Support for Google Cloud Storage using the native SDK.

use std::{collections::HashSet, fs};

use async_trait::async_trait;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_storage::client::{Storage, StorageControl};
use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::AsyncWriteExt;

use super::CloudStorage;
use crate::{prelude::*, secret::Secret};

/// Backend for talking to Google Cloud Storage using the native SDK.
///
/// Credentials are read from the environment via Application Default Credentials
/// or the `GOOGLE_APPLICATION_CREDENTIALS` environment variable.
pub struct GoogleCloudStorage {
    storage: Storage,
    control: StorageControl,
}

impl GoogleCloudStorage {
    /// Create a new `GoogleCloudStorage` backend.
    ///
    /// The SDK automatically uses Application Default Credentials or
    /// GOOGLE_APPLICATION_CREDENTIALS from the environment.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(_secrets: &[Secret]) -> Result<Self> {
        let storage = Storage::builder()
            .build()
            .await
            .context("failed to create GCS Storage client")?;
        let control = StorageControl::builder()
            .build()
            .await
            .context("failed to create GCS StorageControl client")?;
        Ok(GoogleCloudStorage { storage, control })
    }

    /// Download a single file from GCS to a local path.
    #[instrument(skip_all, fields(bucket = %bucket, object = %object, local_path = %local_path.display()), level = "trace")]
    async fn download_file(
        &self,
        bucket: &str,
        object: &str,
        local_path: &Path,
    ) -> Result<()> {
        let bucket_path = format!("projects/_/buckets/{bucket}");
        let mut reader = self
            .storage
            .read_object(&bucket_path, object)
            .send()
            .await
            .with_context(|| format!("failed to read gs://{bucket}/{object}"))?;

        let mut file =
            tokio::fs::File::create(local_path).await.with_context(|| {
                format!("failed to create local file {}", local_path.display())
            })?;

        while let Some(chunk) = reader.next().await.transpose()? {
            file.write_all(&chunk).await.with_context(|| {
                format!(
                    "failed to write gs://{bucket}/{object} to {}",
                    local_path.display()
                )
            })?;
        }

        file.flush().await?;
        Ok(())
    }

    /// Upload a single file from a local path to GCS.
    #[instrument(skip_all, fields(local_path = %local_path.display(), bucket = %bucket, object = %object), level = "trace")]
    async fn upload_file(
        &self,
        local_path: &Path,
        bucket: &str,
        object: &str,
    ) -> Result<()> {
        let bucket_path = format!("projects/_/buckets/{bucket}");
        let file = tokio::fs::File::open(local_path).await.with_context(|| {
            format!("failed to open local file {}", local_path.display())
        })?;

        self.storage
            .write_object(&bucket_path, object, file)
            .send_unbuffered()
            .await
            .with_context(|| {
                format!(
                    "failed to upload {} to gs://{bucket}/{object}",
                    local_path.display()
                )
            })?;

        Ok(())
    }
}

impl fmt::Debug for GoogleCloudStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCloudStorage").finish()
    }
}

#[async_trait]
impl CloudStorage for GoogleCloudStorage {
    #[instrument(skip_all, fields(uri = %uri), level = "trace")]
    async fn list(&self, uri: &str) -> Result<Vec<String>> {
        trace!("listing {}", uri);

        let (bucket, prefix) = parse_gs_url(uri)?;
        let bucket_path = format!("projects/_/buckets/{bucket}");

        let mut results = HashSet::new();
        let mut objects = self
            .control
            .list_objects()
            .set_parent(&bucket_path)
            .set_prefix(prefix)
            .by_item();

        while let Some(object) = objects.next().await.transpose()? {
            let name = &object.name;
            // Skip the prefix itself if it's a "directory".
            if name != prefix && !name.is_empty() {
                results.insert(format!("gs://{bucket}/{name}"));
            }
        }

        Ok(results.into_iter().collect())
    }

    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        let (bucket, key) = parse_gs_url(uri)?;

        if uri.ends_with('/') {
            // Directory sync: list all objects with the prefix and download each.
            trace!("syncing {} to {}", uri, local_path.display());
            fs::create_dir_all(local_path)
                .context("cannot create local download directory")?;

            let objects = self.list(uri).await?;
            for obj_uri in objects {
                let (_, obj_key) = parse_gs_url(&obj_uri)?;
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
            trace!("downloading {} to {}", uri, local_path.display());
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

        let (bucket, key) = parse_gs_url(uri)?;

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

/// Parse a GCS URL (gs://bucket/key).
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

#[test]
fn url_parsing() {
    assert_eq!(parse_gs_url("gs://my-bucket").unwrap(), ("my-bucket", ""));
    assert_eq!(parse_gs_url("gs://my-bucket/").unwrap(), ("my-bucket", ""));
    assert_eq!(
        parse_gs_url("gs://my-bucket/path").unwrap(),
        ("my-bucket", "path")
    );
    assert_eq!(
        parse_gs_url("gs://my-bucket/path/").unwrap(),
        ("my-bucket", "path/")
    );
    assert!(parse_gs_url("s3://foo/").is_err());
}
