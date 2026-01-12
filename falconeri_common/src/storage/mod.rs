//! Cloud storage backends.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use object_store::{path::Path as ObjectPath, ObjectStore, ObjectStoreExt};
use tokio::{fs as async_fs, io::AsyncWriteExt};

use crate::{prelude::*, secret::Secret};

pub mod gs;
pub mod s3;

/// Stream a download from the object store to a local file.
///
/// This streams the data in chunks to avoid loading entire files (which may
/// be 60GB+) into memory.
pub(crate) async fn stream_download_to_file(
    store: &Arc<dyn ObjectStore>,
    object_path: &ObjectPath,
    local_path: &Path,
) -> Result<()> {
    let get_result = store
        .get(object_path)
        .await
        .with_context(|| format!("error fetching object: {}", object_path))?;

    let mut stream = get_result.into_stream();
    let mut file = async_fs::File::create(local_path).await.with_context(|| {
        format!("cannot create local file: {}", local_path.display())
    })?;

    while let Some(chunk) = stream
        .try_next()
        .await
        .with_context(|| format!("error streaming object: {}", object_path))?
    {
        file.write_all(&chunk).await.with_context(|| {
            format!("error writing to file: {}", local_path.display())
        })?;
    }

    file.flush()
        .await
        .with_context(|| format!("error flushing file: {}", local_path.display()))?;

    Ok(())
}

/// Stream an upload from a local file to the object store.
///
/// This uses multipart upload to stream the data in chunks to avoid loading
/// entire files (which may be 60GB+) into memory.
pub(crate) async fn stream_upload_from_file(
    store: &Arc<dyn ObjectStore>,
    local_path: &Path,
    object_path: &ObjectPath,
) -> Result<()> {
    let file = async_fs::File::open(local_path).await.with_context(|| {
        format!("cannot open local file: {}", local_path.display())
    })?;

    let upload = store.put_multipart(object_path).await.with_context(|| {
        format!("error starting multipart upload: {}", object_path)
    })?;

    let mut write = object_store::WriteMultipart::new(upload);

    let mut reader = tokio::io::BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut buf = vec![0u8; 8 * 1024 * 1024];

    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut buf)
            .await
            .with_context(|| {
                format!("error reading file: {}", local_path.display())
            })?;

        if n == 0 {
            break;
        }

        write.write(&buf[..n]);
    }

    write.finish().await.with_context(|| {
        format!("error completing multipart upload: {}", object_path)
    })?;

    Ok(())
}

/// Abstract interface to different kinds of cloud storage backends.
#[async_trait]
pub trait CloudStorage: Send + Sync {
    /// List all the files and subdirectories immediately present in `uri` if
    /// `uri` is a directory, or just return `uri` if it points to a file.
    async fn list(&self, uri: &str) -> Result<Vec<String>>;

    /// Synchronize `uri` down to `local_path` recursively. Does not delete any
    /// existing destination files. The contents of `uri` should be exactly
    /// represented in `local_path`, without the trailing subdirectory name
    /// being inserted—this is a straight directory-to-directory sync.
    ///
    /// To sync down a file, neither `uri` nor `local_path` should end in `/`.
    /// To sync down a directory, _both_ `uri` and `local_path` must end in `/`.
    /// Any other combination is allowed to fail or panic at the discretion of
    /// the implementation.
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()>;

    /// Synchronize `local_path` to `uri` recursively. Does not delete any
    /// existing destination files. The contents of `local_path` should be
    /// exactly represented in `uri`, without the trailing subdirectory name
    /// being inserted—this is a straight directory-to-directory sync.
    async fn sync_up(&self, local_path: &Path, uri: &str) -> Result<()>;
}

impl dyn CloudStorage {
    /// Get the storage backend for the specified URI.
    ///
    /// The `bucket_uri` is used to determine both the storage backend type
    /// (based on the URI scheme like `gs://` or `s3://`) and the bucket name.
    /// It can be any URI within the bucket we want to access.
    ///
    /// If we know about any secrets, we can pass them as the `secrets` array,
    /// and the storage driver can check to see if there are any secrets it can
    /// use to authenticate.
    pub async fn for_uri(
        bucket_uri: &str,
        secrets: &[Secret],
    ) -> Result<Box<dyn CloudStorage>> {
        if bucket_uri.starts_with("gs://") {
            Ok(Box::new(
                gs::GoogleCloudStorage::new(secrets, bucket_uri).await?,
            ))
        } else if bucket_uri.starts_with("s3://") {
            Ok(Box::new(s3::S3Storage::new(secrets, bucket_uri).await?))
        } else {
            Err(format_err!(
                "cannot find storage backend for {}",
                bucket_uri
            ))
        }
    }
}
