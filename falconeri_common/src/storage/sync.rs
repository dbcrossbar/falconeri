//! Streaming transfers between cloud storage and the local filesystem.
//!
//! These are the implementations behind [`crate::storage::CloudStorage::sync_down`]
//! and [`crate::storage::CloudStorage::sync_up_dir`]. The trait methods
//! remain as one-line delegations, so backends keep the option to override
//! them, but the real code lives here.
//!
//! This module deliberately does _not_ restate the semantics of these
//! operations: entries vs prefixes, kind matching between URI and local
//! path, crossing the percent-encoding boundary, and derived existence are
//! documented on the [`CloudStorage`](crate::storage::CloudStorage) trait
//! and in the [`super`] module docs. Read those first.

use futures::TryStreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, WriteMultipart, path::Path as ObjectPath,
};
use tokio::{
    fs as async_fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use walkdir::WalkDir;

use super::{
    check_sync_down_kinds, local_path_is_dir, local_relative_path,
    object_path_from_uri_path, parse_cloud_storage_uri, path_looks_like_prefix,
};
use crate::prelude::*;

/// Stream a download from the object store to a local file.
///
/// This streams the data in chunks to avoid loading entire files (which may
/// be 60GB+) into memory.
pub(crate) async fn stream_download_to_file(
    store: &dyn ObjectStore,
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
    store: &dyn ObjectStore,
    local_path: &Path,
    object_path: &ObjectPath,
) -> Result<()> {
    let file = async_fs::File::open(local_path).await.with_context(|| {
        format!("cannot open local file: {}", local_path.display())
    })?;

    let upload = store.put_multipart(object_path).await.with_context(|| {
        format!("error starting multipart upload: {}", object_path)
    })?;

    let mut write = WriteMultipart::new(upload);

    let mut reader = tokio::io::BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut buf = vec![0u8; 8 * 1024 * 1024];

    loop {
        let n = reader.read(&mut buf).await.with_context(|| {
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

/// Implementation of [`CloudStorage::sync_down`](crate::storage::CloudStorage::sync_down);
/// see the trait method for the contract this implements and the [`super`]
/// docs for the semantics it depends on.
pub(crate) async fn sync_down(
    store: &dyn ObjectStore,
    uri: &str,
    local_path: &Path,
) -> Result<()> {
    trace!("downloading {} to {}", uri, local_path.display());

    let (_, _, key) = parse_cloud_storage_uri(uri)?;
    check_sync_down_kinds(uri, path_looks_like_prefix(key), local_path)?;

    if path_looks_like_prefix(key) {
        // We have a directory. If our source URI ends in `/`, so should our
        // `local_path`, since we generate these ourselves.
        async_fs::create_dir_all(local_path)
            .await
            .context("cannot create local download directory")?;

        let prefix = object_path_from_uri_path(key)?;
        let mut stream = store.list(Some(&prefix));

        // TODO: This has _massively_ insufficient parallelism for many use cases.
        // We need to do something with buffer_unordered and specified concurrency.
        while let Some(meta) = stream
            .try_next()
            .await
            .context("error listing bucket objects")?
        {
            // Both sides of this comparison are encoded key text, so we
            // strip first and decode afterwards.
            let object_key = meta.location.to_string();
            let relative_key = object_key
                .strip_prefix(key)
                .unwrap_or(&object_key)
                .trim_start_matches('/');

            if relative_key.is_empty() {
                continue;
            }

            let file_path = local_path.join(local_relative_path(relative_key)?);

            if let Some(parent) = file_path.parent() {
                async_fs::create_dir_all(parent)
                    .await
                    .context("cannot create local subdirectory")?;
            }

            stream_download_to_file(store, &meta.location, &file_path).await?;
        }
    } else {
        // We have a file.
        if let Some(parent) = local_path.parent() {
            async_fs::create_dir_all(parent)
                .await
                .context("cannot create local download directory")?;
        }

        let object_path = object_path_from_uri_path(key)?;
        stream_download_to_file(store, &object_path, local_path).await?;
    }

    Ok(())
}

/// Implementation of [`CloudStorage::sync_up_dir`](crate::storage::CloudStorage::sync_up_dir);
/// see the trait method for the contract this implements and the [`super`]
/// docs for the semantics it depends on.
pub(crate) async fn sync_up_dir(
    store: &dyn ObjectStore,
    local_path: &Path,
    uri: &str,
) -> Result<()> {
    trace!("uploading {} to {}", local_path.display(), uri);

    let (_, _, key) = parse_cloud_storage_uri(uri)?;
    if !local_path_is_dir(local_path) || !path_looks_like_prefix(key) {
        return Err(format_err!(
            "sync_up_dir copies a local directory (trailing '/') to a bucket \
             prefix (trailing '/'), but got local path {} and URI {uri}",
            local_path.display(),
        ));
    }

    // Our prefix arrives as URI text, which is already percent-encoded.
    let base = object_path_from_uri_path(key)?;

    for entry in WalkDir::new(local_path).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }

        let file_path = entry.path();
        let relative_path = file_path
            .strip_prefix(local_path)
            .context("failed to compute relative path")?;

        // Build our key one segment at a time. Each local name is literal
        // text, which `PathPart::from` encodes for us; concatenating our
        // encoded prefix with literal names and encoding the whole thing
        // would encode the prefix twice.
        let mut object_path = base.clone();
        for component in relative_path.components() {
            // `strip_prefix` should leave us with plain names, and nothing
            // else is safe to encode into a key.
            let std::path::Component::Normal(name) = component else {
                return Err(format_err!(
                    "unexpected path component {component:?} in local file {}",
                    file_path.display(),
                ));
            };
            let name = name.to_str().with_context(|| {
                format!("local path {} is not valid UTF-8", file_path.display())
            })?;
            object_path = object_path.join(object_store::path::PathPart::from(name));
        }

        stream_upload_from_file(store, file_path, &object_path)
            .await
            .with_context(|| {
                format!("error uploading to cloud bucket: {}", object_path.as_ref())
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use assert_fs::{TempDir, prelude::*};
    use predicates::prelude::*;

    use super::*;
    use crate::storage::{
        CloudStorage,
        mem::MemoryStorage,
        test_util::{file_contents, fixture, object, prefix, prefix_entries, sorted},
    };

    // These tests exercise the sync functions through the
    // [`CloudStorage::sync_down`](crate::storage::CloudStorage::sync_down)
    // and [`CloudStorage::sync_up_dir`](crate::storage::CloudStorage::sync_up_dir)
    // trait methods defined in the parent module, which delegate here. That
    // is the surface callers actually use.

    /// Syncing down a single file writes the file and creates parent
    /// directories as needed.
    #[tokio::test]
    async fn test_sync_down_file() -> Result<()> {
        let storage = fixture().await?;
        let dir = TempDir::new()?;

        storage
            .sync_down(
                &storage.uri("a/e.txt"),
                &dir.path().join("deep/nested/e.txt"),
            )
            .await?;

        dir.child("deep/nested/e.txt").assert(file_contents("e"));

        Ok(())
    }

    /// Syncing down a prefix recreates the tree with paths relative to that
    /// prefix, and does not pick up the `ab/c` sibling.
    #[tokio::test]
    async fn test_sync_down_prefix() -> Result<()> {
        let storage = fixture().await?;
        let dir = TempDir::new()?;

        storage
            .sync_down(&storage.uri("a/"), &dir.path().join("out/"))
            .await?;

        dir.child("out/e.txt").assert(file_contents("e"));
        dir.child("out/b/c.txt").assert(file_contents("c"));
        dir.child("out/b/d.txt").assert(file_contents("d"));
        dir.child("out/a0.txt").assert(predicate::path::missing());
        dir.child("out/c.txt").assert(predicate::path::missing());

        Ok(())
    }

    /// Syncing down the bucket root, which is how whole-repo inputs are
    /// downloaded, preserves the full key hierarchy.
    #[tokio::test]
    async fn test_sync_down_bucket_root() -> Result<()> {
        let storage = fixture().await?;
        let dir = TempDir::new()?;

        storage
            .sync_down(&storage.bucket_uri(), &dir.path().join("repo/"))
            .await?;

        dir.child("repo/top.txt").assert(file_contents("t"));
        dir.child("repo/a/b/c.txt").assert(file_contents("c"));
        dir.child("repo/ab/c").assert(file_contents("x"));
        dir.child("repo/d4/sub").assert(file_contents("s"));

        Ok(())
    }

    /// Syncing up a directory uploads each file under the target prefix,
    /// creating deeper keys for subdirectories and no marker objects.
    #[tokio::test]
    async fn test_sync_up_dir() -> Result<()> {
        let storage = MemoryStorage::new("bucket");
        let dir = TempDir::new()?;
        dir.child("src/x.txt").write_str("x")?;
        dir.child("src/nested/y.txt").write_str("y")?;

        storage
            .sync_up_dir(&dir.path().join("src/"), &storage.uri("up/"))
            .await?;

        assert_eq!(storage.contents("up/x.txt").await?, b"x");
        assert_eq!(storage.contents("up/nested/y.txt").await?, b"y");
        assert_eq!(
            sorted(prefix_entries(&storage, &storage.uri("up/")).await?),
            sorted(vec![
                object(&storage.uri("up/x.txt")),
                prefix(&storage.uri("up/nested/")),
            ]),
        );

        Ok(())
    }

    /// Syncing requires both ends to agree about kind, because guessing has
    /// historically meant silently writing a file over a prefix.
    #[tokio::test]
    async fn test_sync_kinds_must_agree() -> Result<()> {
        let storage = fixture().await?;
        let dir = TempDir::new()?;

        // `sync_down` of a directory needs a local directory...
        assert!(
            storage
                .sync_down(&storage.uri("a/"), &dir.path().join("no-slash"))
                .await
                .is_err(),
            "a prefix URI needs a trailing '/' on the local path"
        );
        // ...and an object needs a local file.
        assert!(
            storage
                .sync_down(&storage.uri("a/e.txt"), &dir.path().join("with-slash/"))
                .await
                .is_err(),
            "an object URI must not get a local directory"
        );

        // `sync_up_dir` copies directories only, in both directions.
        dir.child("f.txt").write_str("f")?;
        assert!(
            storage
                .sync_up_dir(&dir.path().join("f.txt"), &storage.uri("a/"))
                .await
                .is_err(),
            "uploading a file over the prefix 'a/' must be refused"
        );
        assert!(
            storage.contents("a").await.is_err(),
            "nothing should have been stored at 'a'"
        );
        assert!(
            storage
                .sync_up_dir(&dir.path().join("src/"), &storage.uri("results"))
                .await
                .is_err(),
            "an object URI is not a prefix to upload into"
        );

        Ok(())
    }
}
