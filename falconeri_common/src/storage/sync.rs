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
    ObjectStore, ObjectStoreExt, WriteMultipart, path::Path as ObjectPath,
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

    use super::*;
    use crate::storage::{
        BucketObject, CloudStorage,
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
    /// This documents intent; it is explicitly _not_ proof that
    /// empty-file uploads work against real services. It passes today
    /// only because `InMemory` is too permissive: a zero-byte file
    /// fills no chunk buffer, and `InMemory` happily completes a
    /// multipart upload with zero parts—which real S3 and GCS reject
    /// (CS-4). The env-gated MinIO test planned in
    /// `plans/CLOUD_STORAGE_IO.md` §2.4-D is what will actually prove
    /// that.
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
}
