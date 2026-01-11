//! Support for Google Cloud Storage using gcp_auth and reqwest.

use std::{collections::HashSet, fs, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use gcp_auth::TokenProvider;
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use tokio::io::AsyncWriteExt;
use walkdir::WalkDir;

use super::CloudStorage;
use crate::{prelude::*, secret::Secret};

/// OAuth2 scope for Google Cloud Storage read/write access.
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

/// Backend for talking to Google Cloud Storage using gcp_auth and reqwest.
pub struct GoogleCloudStorage {
    client: reqwest::Client,
    token_provider: Arc<dyn TokenProvider>,
}

impl GoogleCloudStorage {
    /// Create a new `GoogleCloudStorage` backend.
    #[allow(clippy::new_ret_no_self)]
    #[instrument(skip_all, level = "trace")]
    pub async fn new(_secrets: &[Secret]) -> Result<Self> {
        // Use gcp_auth's default provider chain which checks:
        // 1. GOOGLE_APPLICATION_CREDENTIALS env var
        // 2. GCE metadata server (for workload identity)
        // 3. Default application credentials
        let token_provider = gcp_auth::provider()
            .await
            .context("failed to get GCP authentication provider")?;

        Ok(GoogleCloudStorage {
            client: reqwest::Client::new(),
            token_provider,
        })
    }

    /// Get an authorization header with a fresh access token.
    async fn auth_headers(&self) -> Result<HeaderMap> {
        let token = self
            .token_provider
            .token(&[GCS_SCOPE])
            .await
            .context("failed to get GCS access token")?;

        let mut headers = HeaderMap::new();
        let auth_value = format!("Bearer {}", token.as_str());
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value)
                .context("invalid authorization header value")?,
        );
        Ok(headers)
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

        // Normalize prefix for directory listing.
        let prefix = if !prefix.is_empty() && !prefix.ends_with('/') {
            format!("{}/", prefix)
        } else {
            prefix.to_string()
        };

        let mut results = HashSet::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "https://storage.googleapis.com/storage/v1/b/{}/o",
                percent_encode(bucket)
            );
            url.push_str(&format!("?prefix={}", percent_encode(&prefix)));
            if let Some(ref token) = page_token {
                url.push_str(&format!("&pageToken={}", percent_encode(token)));
            }

            let headers = self.auth_headers().await?;
            let response = self
                .client
                .get(&url)
                .headers(headers)
                .send()
                .await
                .with_context(|| format!("failed to list {}", uri))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format_err!(
                    "failed to list {}: {} - {}",
                    uri,
                    status,
                    body
                ));
            }

            let list_response: ListObjectsResponse = response
                .json()
                .await
                .context("failed to parse GCS list response")?;

            for item in list_response.items.unwrap_or_default() {
                // Skip the directory prefix itself.
                if item.name != prefix {
                    results.insert(format!("gs://{}/{}", bucket, item.name));
                }
            }

            if let Some(token) = list_response.next_page_token {
                page_token = Some(token);
            } else {
                break;
            }
        }

        Ok(results.into_iter().collect())
    }

    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        let (bucket, key) = parse_gs_url(uri)?;

        if uri.ends_with('/') {
            // Directory sync: list all objects and download each one.
            trace!("syncing {} to {}", uri, local_path.display());
            fs::create_dir_all(local_path)
                .context("cannot create local download directory")?;

            let objects = self.list(uri).await?;
            for object_uri in objects {
                let (_, obj_key) = parse_gs_url(&object_uri)?;
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

impl GoogleCloudStorage {
    /// Download a single file from GCS.
    async fn download_file(
        &self,
        bucket: &str,
        key: &str,
        local_path: &Path,
    ) -> Result<()> {
        trace!(
            "downloading gs://{}/{} to {}",
            bucket,
            key,
            local_path.display()
        );

        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
            percent_encode(bucket),
            percent_encode(key)
        );

        let headers = self.auth_headers().await?;
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("failed to download gs://{}/{}", bucket, key))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format_err!(
                "failed to download gs://{}/{}: {} - {}",
                bucket,
                key,
                status,
                body
            ));
        }

        let body = response
            .bytes()
            .await
            .context("failed to read GCS response body")?;

        let mut file =
            tokio::fs::File::create(local_path).await.with_context(|| {
                format!("failed to create file {}", local_path.display())
            })?;

        file.write_all(&body)
            .await
            .with_context(|| format!("failed to write to {}", local_path.display()))?;

        Ok(())
    }

    /// Upload a single file to GCS.
    async fn upload_file(
        &self,
        local_path: &Path,
        bucket: &str,
        key: &str,
    ) -> Result<()> {
        trace!(
            "uploading {} to gs://{}/{}",
            local_path.display(),
            bucket,
            key
        );

        let body = tokio::fs::read(local_path)
            .await
            .with_context(|| format!("failed to read {}", local_path.display()))?;

        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            percent_encode(bucket),
            percent_encode(key)
        );

        let headers = self.auth_headers().await?;
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(Bytes::from(body))
            .send()
            .await
            .with_context(|| format!("failed to upload to gs://{}/{}", bucket, key))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format_err!(
                "failed to upload to gs://{}/{}: {} - {}",
                bucket,
                key,
                status,
                body
            ));
        }

        Ok(())
    }
}

/// Response from the GCS list objects API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListObjectsResponse {
    #[serde(default)]
    items: Option<Vec<StorageObject>>,
    next_page_token: Option<String>,
}

/// A single object in a GCS bucket.
#[derive(Debug, Deserialize)]
struct StorageObject {
    name: String,
}

/// Parse a `gs://` URL into bucket and object name.
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

/// Percent-encode a string for use in a URL.
fn percent_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[test]
fn gs_url_parsing() {
    assert_eq!(parse_gs_url("gs://bucket").unwrap(), ("bucket", ""));
    assert_eq!(parse_gs_url("gs://bucket/").unwrap(), ("bucket", ""));
    assert_eq!(
        parse_gs_url("gs://bucket/path").unwrap(),
        ("bucket", "path")
    );
    assert_eq!(
        parse_gs_url("gs://bucket/path/").unwrap(),
        ("bucket", "path/")
    );
    assert!(parse_gs_url("s3://foo/").is_err());
}
