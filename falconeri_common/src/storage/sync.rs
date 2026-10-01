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

use std::{path::PathBuf, sync::Arc};

use futures::TryStreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart, path::Path as ObjectPath,
};
use tokio::{
    fs as async_fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use walkdir::WalkDir;

use super::{
    CloudStorage, CloudStorageForUri, check_sync_down_kinds, local_path_is_dir,
    local_relative_path, object_path_from_uri_path, parse_cloud_storage_uri,
    path_looks_like_prefix,
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

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Largest part that S3 and GCS accept in a multipart upload.
const MAX_PART_SIZE: u64 = 5 * GIB;

/// Most parts that S3 and GCS accept in one multipart upload.
const MAX_PARTS: u64 = 10_000;

/// Part size we start with, before a file is large enough to force bigger
/// parts. S3 and GCS accept parts of 5 MiB or more, so this is our choice, not
/// a service limit.
///
/// Every part is a billable write request, and a job uploads through one
/// output prefix, so part count costs us money and request-rate headroom. At
/// about 100 MiB/s per pod, 5 MiB parts would take 20 requests per second per
/// pod; 16 MiB parts take 6. The price is one 16 MiB buffer per in-flight
/// part, which is small next to the pod memory budget, and the part window
/// only has to cover bandwidth times round trip time, about 3 MiB.
const DEFAULT_PART_SIZE: u64 = 16 * MIB;

/// Largest local file we upload with a single `put` request.
///
/// A file this size or smaller can never produce more than one multipart part,
/// so a single `put` does the same work for two fewer requests. Larger files
/// go to [`stream_upload_from_file`], which streams them without buffering.
const PUT_MAX_SIZE: u64 = DEFAULT_PART_SIZE;

/// Part size to use for a multipart upload of an object `size` bytes long.
/// `object_store` calls this the chunk size.
///
/// S3 and GCS allow at most [`MAX_PARTS`] parts, each of at most
/// [`MAX_PART_SIZE`], so a big file needs `ceil(size / MAX_PARTS)`. We take
/// that, clamped to at least [`DEFAULT_PART_SIZE`] and at most
/// [`MAX_PART_SIZE`].
///
/// Above `MAX_PARTS * MAX_PART_SIZE`, about 48.8 TiB, no legal part size
/// exists. We still answer with the ceiling, and the service rejects the
/// upload. GCS caps objects at 5 TiB, which is lower, but that limit belongs
/// to the backend, and this function does not know which backend it feeds.
fn multipart_part_size(size: u64) -> u64 {
    size.div_ceil(MAX_PARTS)
        .clamp(DEFAULT_PART_SIZE, MAX_PART_SIZE)
}

/// Upload a local file to the object store, choosing the request shape by
/// file size.
///
/// Files at or below [`PUT_MAX_SIZE`], empty files included, are uploaded
/// whole with a single `put`. Everything larger is streamed as a multipart
/// upload.
pub(crate) async fn upload_file(
    store: &dyn ObjectStore,
    local_path: &Path,
    object_path: &ObjectPath,
) -> Result<()> {
    let mut file = async_fs::File::open(local_path).await.with_context(|| {
        format!("cannot open local file: {}", local_path.display())
    })?;
    let size = file
        .metadata()
        .await
        .with_context(|| format!("cannot stat local file: {}", local_path.display()))?
        .len();

    if size > PUT_MAX_SIZE {
        return stream_upload_from_file(store, file, size, local_path, object_path)
            .await;
    }

    // We know the file is small enough to hold in memory (see
    // PUT_MAX_SIZE). A file that grows between the stat above and the read
    // below is simply uploaded whole, which is still correct.
    let capacity = usize::try_from(size)
        .with_context(|| format!("cannot buffer {size} bytes into memory"))?;
    let mut buf = Vec::with_capacity(capacity);
    file.read_to_end(&mut buf)
        .await
        .with_context(|| format!("error reading file: {}", local_path.display()))?;

    store
        .put(object_path, PutPayload::from(buf))
        .await
        .map(|_result| ())
        .with_context(|| format!("error putting object: {}", object_path.as_ref()))
}

/// Stream an upload from an open local file, as a multipart upload.
///
/// `size` is the file size reported by the caller. We stream the data in
/// parts, so an object of any size uploads without loading it whole into
/// memory. Callers should go through [`upload_file`], which picks between this
/// and a single `put`.
pub(crate) async fn stream_upload_from_file(
    store: &dyn ObjectStore,
    mut file: async_fs::File,
    size: u64,
    local_path: &Path,
    object_path: &ObjectPath,
) -> Result<()> {
    // `object_store` takes a part size in bytes and buffers parts in memory.
    // A 32-bit platform cannot buffer a part larger than 4 GiB, so we check.
    let part_size = usize::try_from(multipart_part_size(size)).with_context(|| {
        format!(
            "multipart parts for a {} byte file do not fit in memory on this platform",
            size
        )
    })?;

    let upload = store.put_multipart(object_path).await.with_context(|| {
        format!("error starting multipart upload: {}", object_path)
    })?;

    let mut write = WriteMultipart::new_with_chunk_size(upload, part_size);
    let mut buf = vec![0u8; part_size];

    // Read one part at a time, so each write hands `WriteMultipart` exactly
    // one part and it never has to hold a partial one.
    loop {
        let filled = read_part(&mut file, &mut buf).await.with_context(|| {
            format!("error reading file: {}", local_path.display())
        })?;

        if filled == 0 {
            break;
        }

        write.write(&buf[..filled]);
    }

    write.finish().await.with_context(|| {
        format!("error completing multipart upload: {}", object_path)
    })?;

    Ok(())
}

/// Read into `buf` until it is full or the file ends, and report how many
/// bytes it holds. A count below `buf.len()` only happens at end of file, so
/// the caller can treat that read as the tail. Errors carry no context: the
/// caller knows which file this is.
async fn read_part(file: &mut async_fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;

    while filled < buf.len() {
        let read = file.read(&mut buf[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }

    Ok(filled)
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

    // Walk errors must not be swallowed: upstream `upload_outputs` marks
    // every recorded output `Done` based on this call's overall result,
    // so skipping an unreadable or vanished directory silently loses
    // data (CS-5). `walkdir::Error`'s Display names the path it failed
    // on; our context names the tree we were walking.
    for entry in WalkDir::new(local_path) {
        let entry = entry.with_context(|| {
            format!("error walking local directory {}", local_path.display())
        })?;

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

        upload_file(store, file_path, &object_path)
            .await
            .with_context(|| {
                format!("error uploading to cloud bucket: {}", object_path.as_ref())
            })?;
    }

    Ok(())
}

/// One download in a [`sync_down_all`] batch.
#[derive(Clone, Debug)]
pub struct SyncTarget {
    /// The cloud storage URI to download. The kind-matching rules of
    /// [`CloudStorage::sync_down`](crate::storage::CloudStorage::sync_down)
    /// apply: object URIs and prefix URIs must be paired with matching
    /// local paths.
    pub uri: String,

    /// Where to write it locally.
    pub local_path: PathBuf,
}

/// Download a batch of targets, using a single resolver.
///
/// This is the worker's input-download path, lifted out of the worker so
/// that a pod can build **one** resolver for its lifetime instead of one
/// per file. Behavior notes:
///
/// - Every target is validated (URI parse plus kind match) before
///   anything touches the filesystem, so a bad late target leaves the
///   work dir untouched rather than half-populated.
/// - Storage is resolved once per distinct bucket, up front, so
///   credential problems also surface before we start writing.
/// - Downloads are still sequential; inter-object concurrency is
///   deliberately postponed (see `plans/CLOUD_STORAGE_IO.md` §3).
/// - Every failure is wrapped with the target that caused it.
pub async fn sync_down_all<R: CloudStorageForUri + ?Sized>(
    resolver: &mut R,
    targets: &[SyncTarget],
) -> Result<()> {
    // Validate every target before touching the filesystem. `sync_down`
    // checks these too, but only per-target and only after earlier
    // targets have already been downloaded.
    for target in targets {
        let (_, _, key) = parse_cloud_storage_uri(&target.uri)
            .with_context(|| format!("invalid sync target URI {}", target.uri))?;
        check_sync_down_kinds(
            &target.uri,
            path_looks_like_prefix(key),
            &target.local_path,
        )?;
    }

    // Resolve one storage backend per distinct bucket, up front.
    let mut stores: HashMap<String, Arc<dyn CloudStorage>> = HashMap::new();
    for target in targets {
        let (_, bucket, _) = parse_cloud_storage_uri(&target.uri)?;
        if !stores.contains_key(bucket) {
            let storage = resolver.for_uri(&target.uri).await.with_context(|| {
                format!("cannot resolve storage backend for {}", target.uri)
            })?;
            stores.insert(bucket.to_owned(), storage);
        }
    }

    // Download, sequentially for now (see the note above).
    for target in targets {
        let (_, bucket, _) = parse_cloud_storage_uri(&target.uri)?;
        let storage = stores
            .get(bucket)
            .expect("every target bucket was resolved above");
        sync_down(storage.store(), &target.uri, &target.local_path)
            .await
            .with_context(|| {
                format!(
                    "error downloading {} to {}",
                    target.uri,
                    target.local_path.display(),
                )
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use assert_fs::{TempDir, prelude::*};
    use predicates::prelude::*;
    use proptest::prelude::*;

    use object_store::memory::InMemory;

    use super::*;
    use crate::storage::{
        BucketObject, CloudStorage,
        injector::{CallKind, ObjectStoreFaultInjector},
        mem::{MemoryStorage, MemoryStorageResolver},
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

    /// `sync_down_all` downloads every target through one resolver,
    /// creating parent directories just like `sync_down`.
    #[tokio::test]
    async fn test_sync_down_all() -> Result<()> {
        let mut resolver = MemoryStorageResolver::default();
        resolver
            .populate(&[
                BucketObject::from_uri_for_test("memory://bucket/a.txt", 1),
                BucketObject::from_uri_for_test("memory://bucket/deep/b.txt", 1),
            ])
            .await?;
        let dir = TempDir::new()?;

        sync_down_all(
            &mut resolver,
            &[
                SyncTarget {
                    uri: "memory://bucket/a.txt".to_owned(),
                    local_path: dir.path().join("a.txt"),
                },
                SyncTarget {
                    uri: "memory://bucket/deep/b.txt".to_owned(),
                    local_path: dir.path().join("nested/b.txt"),
                },
            ],
        )
        .await?;

        dir.child("a.txt").assert(file_contents("x"));
        dir.child("nested/b.txt").assert(file_contents("x"));

        Ok(())
    }

    /// A bad target is rejected before *anything* is written, so the
    /// work dir is never left half-populated, and the failure names the
    /// target that caused it.
    #[tokio::test]
    async fn test_sync_down_all_fails_before_writing() -> Result<()> {
        let mut resolver = MemoryStorageResolver::default();
        resolver
            .populate(&[BucketObject::from_uri_for_test("memory://bucket/a.txt", 1)])
            .await?;
        let dir = TempDir::new()?;

        // The second target violates the kind rules (a prefix URI with a
        // non-directory local path), which must be caught before the
        // first target is downloaded.
        let err = sync_down_all(
            &mut resolver,
            &[
                SyncTarget {
                    uri: "memory://bucket/a.txt".to_owned(),
                    local_path: dir.path().join("a.txt"),
                },
                SyncTarget {
                    uri: "memory://bucket/deep/".to_owned(),
                    local_path: dir.path().join("no-slash"),
                },
            ],
        )
        .await
        .expect_err("a kind mismatch late in the batch must fail the batch");
        assert!(
            format!("{err:#}").contains("memory://bucket/deep/"),
            "the error should name the offending target, got {err:#}"
        );
        dir.child("a.txt").assert(predicate::path::missing());

        // A failed download also names its target.
        let err = sync_down_all(
            &mut resolver,
            &[SyncTarget {
                uri: "memory://bucket/gone.txt".to_owned(),
                local_path: dir.path().join("gone.txt"),
            }],
        )
        .await
        .expect_err("a missing object must fail the batch");
        assert!(
            format!("{err:#}").contains("memory://bucket/gone.txt"),
            "the error should name the offending target, got {err:#}"
        );

        Ok(())
    }

    /// `multipart_part_size` must always return a part size that is legal and
    /// also big enough to fit the object.
    ///
    /// The properties are the shape of the bug: a part below the 5 MiB service
    /// minimum, a part above the 5 GiB service maximum, or a part count above
    /// the 10,000-part limit. Each one makes an upload impossible.
    ///
    /// We do not pin the part size itself. `DEFAULT_PART_SIZE` is a constant we
    /// chose, and a test that can only fail when someone edits a constant tells
    /// us nothing. Any legal policy passes here.
    ///
    /// The two ranges need separate handling. Above `MAX_PARTS * MAX_PART_SIZE`
    /// no legal answer exists, so the part-count property cannot hold there.
    /// That range is also the only place the 5 GiB ceiling can bind, so testing
    /// only below it would leave that property unable to fail. We draw from
    /// both sides, and guard the part count.
    #[test]
    fn test_multipart_part_size() {
        // The largest object we can upload at all.
        const MAX_OBJECT_SIZE: u64 = MAX_PARTS * MAX_PART_SIZE;
        // Smallest part S3 and GCS accept. We deliberately pick a larger
        // policy floor, so the test states the service limit, not our choice.
        const MIN_PART_SIZE: u64 = 5 * MIB;

        // Boundaries are where mistakes live, so we test them directly rather
        // than hope a random draw lands on one.
        let edges = [
            0,
            1,
            MIN_PART_SIZE,
            MIN_PART_SIZE + 1,
            DEFAULT_PART_SIZE,
            DEFAULT_PART_SIZE + 1,
            MAX_PART_SIZE,
            MAX_PART_SIZE + 1,
            MAX_OBJECT_SIZE - 1,
            MAX_OBJECT_SIZE,
            MAX_OBJECT_SIZE + 1,
            u64::MAX,
        ];
        let sizes = prop_oneof![
            proptest::sample::select(edges.to_vec()),
            // Below the ceiling, where the parts have to cover the object.
            0..=MAX_OBJECT_SIZE,
            // Above it, where only the service limits still apply.
            MAX_OBJECT_SIZE + 1..=u64::MAX,
        ];

        proptest!(|(size in sizes)| {
            let part_size = multipart_part_size(size);
            prop_assert!(
                part_size >= MIN_PART_SIZE,
                "{size}: part size {part_size} is below the service minimum {MIN_PART_SIZE}",
            );
            prop_assert!(
                part_size <= MAX_PART_SIZE,
                "{size}: part size {part_size} is above the service maximum {MAX_PART_SIZE}",
            );
            // A part count that fits is only possible up to the ceiling.
            if size <= MAX_OBJECT_SIZE {
                let parts = size.div_ceil(part_size);
                prop_assert!(
                    parts <= MAX_PARTS,
                    "{size}: needs {parts} parts of {part_size} bytes, over the limit of {MAX_PARTS}",
                );
            }
        });
    }

    /// Above the largest object we can build, there is no legal part size. We
    /// answer with the service ceiling and let the service reject the upload,
    /// rather than inventing a part size or refusing locally.
    #[test]
    fn test_multipart_part_size_above_the_ceiling() {
        let size = MAX_PARTS * MAX_PART_SIZE + 1;
        assert_eq!(multipart_part_size(size), MAX_PART_SIZE);
    }

    /// Make sure we use the right kind of request for our file size.
    ///
    /// The reasoning behind this is a little complicated:
    ///
    /// - Some cloud stores will fail if we try to upload 0-byte files
    ///   via a streaming API.
    /// - [`object_store`] will work around this transparently, but may
    ///   need a bunch of API calls to do so. This costs network bandwidth
    ///   and time, and may count against throughput quota.
    /// - So if we _can_ upload a small file in a single `put`, we should
    ///   do so. Streaming should be reserved for larger files.
    ///
    /// So we use our call injector to verify our actual call sequence,
    /// to make sure we're doing this the efficient way, and not resorting
    /// to more expensive fallback paths.
    #[tokio::test]
    async fn test_sync_up_dir_picks_upload_path_by_size() -> Result<()> {
        let one_part =
            usize::try_from(PUT_MAX_SIZE).expect("chunk size fits in usize");
        for (name, len, expect_put) in [
            ("empty.bin", 0, true),
            ("small.bin", 1024, true),
            // Exactly one part's worth, so a single `put` still does all
            // the work multipart could have done.
            ("one-part.bin", one_part, true),
            // One byte more cannot fit in a single part.
            ("two-parts.bin", one_part + 1, false),
        ] {
            let src = TempDir::new()?;
            src.child("tree/f.bin")
                .write_binary(&vec![0xAB; len])
                .with_context(|| format!("{name}: cannot write fixture"))?;

            let injector = ObjectStoreFaultInjector::new(InMemory::default());
            sync_up_dir(&injector, &src.path().join("tree/"), "memory://bucket/up/")
                .await
                .with_context(|| format!("{name}: upload failed"))?;

            let calls = injector.calls();
            if expect_put {
                assert_eq!(
                    calls,
                    vec![CallKind::Put],
                    "{name}: expected exactly one put and no multipart handshake, got {calls:?}",
                );
            } else {
                assert_eq!(
                    calls.first(),
                    Some(&CallKind::PutMultipart),
                    "{name}: expected a multipart upload, got {calls:?}",
                );
                assert_eq!(
                    calls.last(),
                    Some(&CallKind::Complete),
                    "{name}: expected the multipart upload to be completed, got {calls:?}",
                );
                assert!(
                    calls.iter().all(|c| {
                        matches!(
                            c,
                            CallKind::PutMultipart
                                | CallKind::PutPart
                                | CallKind::Complete
                        )
                    }),
                    "{name}: expected only multipart calls, got {calls:?}",
                );
                assert!(
                    injector.called(CallKind::PutPart),
                    "{name}: expected at least one part, got {calls:?}",
                );
            }

            // Whichever branch ran, the object must hold exactly the bytes
            // we wrote, under the key we expect. Reading through `inner()`
            // is not recorded, so it leaves the log above alone.
            let stored = injector
                .inner()
                .get(&ObjectPath::from("up/f.bin"))
                .await
                .with_context(|| format!("{name}: object is missing"))?
                .bytes()
                .await?;
            assert!(
                stored.len() == len && stored.iter().all(|b| *b == 0xAB),
                "{name}: stored object should have the same length as the source file",
            );
        }

        Ok(())
    }

    /// Collect every regular file under `root` as `(relative path,
    /// bytes)`, for byte-for-byte tree comparisons.
    fn collect_tree(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let mut files = BTreeMap::new();
        for entry in WalkDir::new(root) {
            let entry =
                entry.with_context(|| format!("error walking {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry.path().strip_prefix(root).with_context(|| {
                format!("{} is not under {}", entry.path().display(), root.display(),)
            })?;
            files.insert(
                relative.to_path_buf(),
                std::fs::read(entry.path()).with_context(|| {
                    format!("cannot read {}", entry.path().display())
                })?,
            );
        }
        Ok(files)
    }

    /// A local tree, uploaded with `sync_up_dir` and downloaded again
    /// with `sync_down`, must come back byte-identical—including the
    /// names. This is the round trip over the "two worlds of text"
    /// boundary (literal local names vs percent-encoded keys, see the
    /// [`super`] module docs), so it covers every name that needs
    /// encoding or could be double-encoded: spaces, `%`, `#`, text that
    /// already looks escaped, unicode, dot-files, and deep nesting.
    #[tokio::test]
    async fn test_sync_round_trip_nasty_names() -> Result<()> {
        let src = TempDir::new()?;
        let files: &[(&str, &str)] = &[
            ("plain.txt", "plain"),
            ("with spaces.txt", "spaces"),
            ("100%.txt", "percent"),
            ("hash#tag.txt", "hash"),
            // Looks like an encoded '/' but is literal text; encoding
            // must escape the '%' and decoding must restore it exactly.
            ("pre%2Fescaped.txt", "already escaped"),
            ("日本語/第1階層/ファイル.txt", "unicode"),
            ("emoji-🎉.txt", "emoji"),
            (".keep", "root dotfile"),
            ("dotted/.keep", "directory dotfile"),
            ("deep/a/b/c/d/e/f/g/leaf.txt", "deep"),
        ];
        for (name, contents) in files {
            src.child(format!("tree/{name}")).write_str(contents)?;
        }

        let storage = MemoryStorage::new("bucket");
        storage
            .sync_up_dir(&src.path().join("tree/"), &storage.bucket_uri())
            .await?;

        let down = TempDir::new()?;
        storage
            .sync_down(&storage.bucket_uri(), &down.path().join("tree/"))
            .await?;

        let down_tree = collect_tree(&down.path().join("tree"))?;
        assert_eq!(
            down_tree.len(),
            files.len(),
            "the download should hold exactly one file per source file, not zero",
        );
        assert_eq!(
            collect_tree(&src.path().join("tree"))?,
            down_tree,
            "the round trip should be byte-identical, names and contents",
        );

        Ok(())
    }

    /// The same round trip at scale, asserting **exact set equality**.
    /// "Silently dropped one file in 5,000" is precisely the class of
    /// bug we risk shipping when the concurrent fan-out lands (see
    /// `plans/CLOUD_STORAGE_IO.md` §3), so this test is deliberately in
    /// place before any transfer-code changes.
    #[tokio::test]
    async fn test_sync_round_trip_many_objects() -> Result<()> {
        const DIRS: u32 = 25;
        const SUBDIRS: u32 = 10;
        const FILES: u32 = 10; // 2,500 files total.

        let src = TempDir::new()?;
        for dir in 0..DIRS {
            for subdir in 0..SUBDIRS {
                for file in 0..FILES {
                    let name = format!("tree/d{dir:02}/s{subdir}/f{file:02}.dat");
                    // Each file's contents name its own path, so a
                    // misfiled object cannot accidentally match.
                    src.child(&name).write_str(&name)?;
                }
            }
        }

        let storage = MemoryStorage::new("bucket");
        storage
            .sync_up_dir(&src.path().join("tree/"), &storage.bucket_uri())
            .await?;

        let down = TempDir::new()?;
        storage
            .sync_down(&storage.bucket_uri(), &down.path().join("tree/"))
            .await?;

        let expected = collect_tree(&src.path().join("tree"))?;
        let actual = collect_tree(&down.path().join("tree"))?;
        assert_eq!(
            expected.len(),
            (DIRS * SUBDIRS * FILES) as usize,
            "fixture sanity: the source tree itself should hold every generated file",
        );
        assert_eq!(
            expected, actual,
            "exact set equality (paths and bytes), not just 'a lot of files made it'",
        );

        Ok(())
    }

    /// Empty files and "empty" directories (which only survive as a
    /// `.keep` object, since object stores have no directories) round
    /// trip through memory.
    ///
    /// This documents intent; it is explicitly _not_ proof that empty-file
    /// uploads are legal against real services. `InMemory` accepts any
    /// request, including a multipart completion with zero parts, which the
    /// S3 and GCS APIs reject. Request legality is what the env-gated MinIO
    /// test in `plans/CLOUD_STORAGE_IO.md` §2.4-D is for.
    #[tokio::test]
    async fn test_sync_round_trip_empty_file_and_dir() -> Result<()> {
        let src = TempDir::new()?;
        src.child("tree/empty.bin").write_str("")?;
        src.child("tree/emptish/.keep").write_str("")?;

        let storage = MemoryStorage::new("bucket");
        storage
            .sync_up_dir(&src.path().join("tree/"), &storage.bucket_uri())
            .await?;

        let down = TempDir::new()?;
        storage
            .sync_down(&storage.bucket_uri(), &down.path().join("tree/"))
            .await?;

        let down_tree = collect_tree(&down.path().join("tree"))?;
        assert_eq!(
            down_tree.len(),
            2,
            "both the empty file and the directory's .keep should come back",
        );
        assert_eq!(
            collect_tree(&src.path().join("tree"))?,
            down_tree,
            "empty files and dotfile-backed directories should round trip",
        );

        Ok(())
    }

    /// A walk failure must abort `sync_up_dir` rather than be skipped.
    /// Upstream, `upload_outputs` marks every recorded output `Done`
    /// based on this call's overall result, so a swallowed walk error
    /// means outputs silently never land (CS-5).
    ///
    /// A self-referential symlink as the walk root fails with ELOOP on
    /// every lookup—even for root, which ignores mode bits—so this test
    /// always exercises the error path. (A symlink loop _inside_ the
    /// tree would not: `WalkDir` does not follow symlinks by default and
    /// simply yields them as non-file entries.)
    #[cfg(unix)]
    #[tokio::test]
    async fn test_sync_up_dir_propagates_walk_errors() -> Result<()> {
        let dir = TempDir::new()?;
        // The link `<dir>/loop` points at "loop", i.e. at itself.
        std::os::unix::fs::symlink("loop", dir.path().join("loop"))?;

        let storage = MemoryStorage::new("bucket");
        let err = storage
            .sync_up_dir(
                Path::new(&format!("{}/loop/", dir.display())),
                &storage.uri("up/"),
            )
            .await
            .expect_err(
                "a walk error in the source tree must fail the upload, not be skipped",
            );
        assert!(
            format!("{err:#}").contains("loop"),
            "the error should name the offending path, got {err:#}",
        );

        Ok(())
    }

    /// An unreadable subdirectory must also fail the upload (CS-5), and
    /// name the directory that broke. Unlike the symlink-loop test,
    /// this one is skipped when the process can read mode-000
    /// directories—root or CAP_DAC_OVERRIDE, i.e. most containers—which
    /// is exactly why the loop test above is the always-on one.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_sync_up_dir_propagates_unreadable_dir() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new()?;
        dir.child("src/ok.txt").write_str("ok")?;
        dir.child("src/locked/hidden.txt").write_str("h")?;

        let locked = dir.path().join("src/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;

        // Probe whether we can defeat the mode bits, rather than
        // checking the euid (no unsafe libc calls, and it tracks the
        // actual capability set).
        let readable_despite_mode = std::fs::read_dir(&locked).is_ok();

        let storage = MemoryStorage::new("bucket");
        let result = storage
            .sync_up_dir(&dir.path().join("src/"), &storage.uri("up/"))
            .await;

        // Restore permissions before anything that can fail, so the
        // TempDir can always clean up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;

        if readable_despite_mode {
            eprintln!(
                "skipping: this process can read mode-000 directories \
                 (running as root or with DAC override)",
            );
            return Ok(());
        }

        let err = result.expect_err(
            "an unreadable subdirectory must fail the upload, not be \
             silently skipped (CS-5)",
        );
        assert!(
            format!("{err:#}").contains("locked"),
            "the error should name the offending path, got {err:#}",
        );

        Ok(())
    }
}
