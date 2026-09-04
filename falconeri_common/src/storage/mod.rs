//! Cloud storage backends.

use std::collections::BTreeSet;

use async_trait::async_trait;
use futures::TryStreamExt;
use lazy_static::lazy_static;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, path::Path as ObjectPath,
};
use regex::Regex;
use tokio::{fs as async_fs, io::AsyncWriteExt};
use walkdir::WalkDir;

use crate::{prelude::*, secret::Secret};

pub mod gs;
pub mod s3;

/// Does this path look like a "prefix" as opposed to a bucket object?
fn path_looks_like_prefix(path: &str) -> bool {
    path.is_empty() || path.ends_with("/")
}

/// An object in a bucket. Never ends in a "/".
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct BucketObject {
    /// Full URL to our object.
    pub uri: String,
    /// The size of the object.
    pub size: u64,
}

impl BucketObject {
    /// Create a new bucket object, rejecting any which have a name ending
    /// in a slash.
    fn new(scheme: &str, bucket: &str, meta: &ObjectMeta) -> Result<Self> {
        let path = meta.location.to_string();
        let uri = format!("{scheme}://{bucket}/{path}");
        if path_looks_like_prefix(&path) {
            // In theory, object_store::path::Path can never end in "/".
            // But we're going to check, anyway.
            Err(format_err!(
                "bucket object {:?} has a zero-length path or ends with '/'",
                uri
            ))
        } else {
            let size = meta.size;
            Ok(Self { uri, size })
        }
    }

    /// (Test only) Create a new bucket object from a URI.
    //#[cfg(test)]
    pub fn from_uri_for_test(uri: &str, size: u64) -> Self {
        assert!(!uri.ends_with('/'));
        Self {
            uri: uri.to_string(),
            size,
        }
    }
}

impl Ord for BucketObject {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.uri.cmp(&other.uri)
    }
}

impl PartialOrd for BucketObject {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A prefix in a bucket. Always ends in "/".
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[non_exhaustive]
pub struct BucketPrefix {
    /// Full URL for our prefix.
    pub uri: String,
}

impl BucketPrefix {
    fn new(scheme: &str, bucket: &str, path: &ObjectPath) -> Result<Self> {
        // `object_store::path::Path` should never have a trailing "/",
        // according to the docs.
        let path = path.to_string();
        assert!(!path.ends_with("/"));
        let uri = format!("{scheme}://{bucket}/{path}/");
        Self::from_uri_and_path_internal(uri, &path)
    }

    /// Construct a [`BucketPrefix`] from a cloud storage URI, which must
    /// end in "/" or be an empty string.
    pub fn from_uri(uri: String) -> Result<Self> {
        let (_, _, path) = parse_cloud_storage_uri(&uri)?;
        Self::from_uri_and_path_internal(uri.to_owned(), path)
    }

    /// Internal constructor helper. Should only be called by other
    /// constructors. `uri` and `path` must match.
    fn from_uri_and_path_internal(uri: String, path: &str) -> Result<Self> {
        assert!(!path.starts_with("/"));
        if !path_looks_like_prefix(path) {
            return Err(format_err!("bucket prefix {uri} does not end with '/'"));
        }
        Ok(Self { uri })
    }
}

/// A entry in a bucket.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum BucketEntry {
    /// An object. Analogous to a file in a filesystem.
    Object(BucketObject),
    /// A prefix. Analogous (more or less) to a directory in a file
    /// system, though it may only exist when there are objects
    /// "under" it.
    Prefix(BucketPrefix),
}

impl BucketEntry {
    /// Get the full URI for this bucket entry.
    pub fn uri(&self) -> &str {
        match self {
            BucketEntry::Object(object) => &object.uri,
            BucketEntry::Prefix(prefix) => &prefix.uri,
        }
    }
}

/// A listing of some portion of a bucket.
///
/// **Invariant:** A bucket listing **always** has "file-system compatible"
/// semantics. It may not contain both an object "a" and a prefix "a/", or an
/// object "a/", because either would prevent it from being mapped to the
/// file system. We need to detect and reject that sort of nonsense early
/// rather than passing it downstream.
#[derive(Clone, Debug)]
pub enum BucketListing {
    /// The listing matched a single object, equivalent to "ls file".
    /// Contains just "file".
    Object(BucketObject),
    /// The listing matched a prefix, equivalent to "ls dir". Does not
    /// include "dir/" itself. Not recursive.
    PrefixEntries(Vec<BucketEntry>),
}

impl BucketListing {
    /// Construct a listing for a single object.
    fn object(object: BucketObject) -> Self {
        BucketListing::Object(object)
    }

    /// Construct a listing from a set of objects and prefixes, enforcing our
    /// various invariants.
    pub fn prefix_entries(entries: Vec<BucketEntry>) -> Result<Self> {
        check_for_bucket_entry_collisions(&entries)?;
        Ok(BucketListing::PrefixEntries(entries))
    }
}

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

/// Check for bucket collisions of various sorts.
pub fn check_for_bucket_entry_collisions(entries: &[BucketEntry]) -> Result<()> {
    // Create a table of all our entries.
    let mut entry_set = BTreeSet::<&str>::new();
    for entry in entries {
        if !entry_set.insert(entry.uri()) {
            return Err(format_err!(
                "duplicate bucket entries found pointing to {}",
                entry.uri(),
            ));
        }
    }

    // For each object, make sure it isn't shadowing anything. This
    // is attempting to check the general case of our invariant, rather
    // than relying on a specific precondition about what objects and
    // prefixes we might receive.
    for entry in entries {
        if let BucketEntry::Object(object) = entry {
            let object_as_prefix = format!("{}/", object.uri);
            let range = entry_set.range(&object_as_prefix[..]..);
            for candidate in range {
                // If we've moved past anything that might collide, bail.
                if !candidate.starts_with(&object.uri) {
                    break;
                }

                // Is this candidate shadowed by the object?
                if candidate.starts_with(&object_as_prefix) {
                    return Err(anyhow::anyhow!(
                        "entry {} is shadowed by object {}",
                        candidate,
                        object.uri,
                    ));
                }
            }
        }

        // NOTE: I don't _think_ we want to check for prefix/prefix collisions,
        // especially once we add support for "/*/foo" globs, which might be
        // reasonably overlaid on "/*" globs. _Some_ level of checking will
        // always need to be done later during worker downloads, with the full
        // recursive file list available.
    }

    Ok(())
}

/// Abstract interface to different kinds of cloud storage backends.
#[async_trait]
pub trait CloudStorage: Send + Sync {
    /// The URL scheme supported by this [`CloudStorage`].
    fn scheme(&self) -> &'static str;

    /// Our "store", used to access the bucket.
    fn store(&self) -> &dyn ObjectStore;

    /// List `uri` non-recursively.
    ///
    /// If called with a URI ending in "/", this will always return a
    /// `BucketListing::PrefixEntries`. If called without a trailing "/", this
    /// may return either a `BucketListing::Object` or a
    /// `BucketListing::PrefixEntries`. A `BucketListing::PrefixEntries` should
    /// exclude the path you passed. If nothing exists under that prefix, it
    /// should return empty. Note that if we're asked to list "a", and both "a"
    /// and "a/" exist, then we will return one or the other, or perhaps an
    /// error—specific behavior isn't guaranteed. Since Falconeri does support
    /// buckets with object/prefix name clashes, our behavior here is not
    /// guaranteed to be consistent in the future.
    ///
    /// If the listed contents of bucket cannot be represented on a filesystem,
    /// perhaps because of colliding "a" objects and "a/" prefixes, or because
    /// of "a/" objects with a trailing slash, it will return an error.
    /// See [`BucketListing`] for more details on this invariant.
    ///
    /// The underlying library we're using provides this guarantee on prefix
    /// handling:
    ///
    /// > Prefixes are evaluated on a path segment basis, i.e. foo/bar is a
    /// > prefix of foo/bar/x but not of foo/bar_baz/x. List is not recursive,
    /// > i.e. foo/bar/more/x will not be included.
    ///
    /// Also see [`object_storage::path::Path`], which imposes some additional
    /// constraints: No leading or trailing slashes, no . or .., etc.
    #[instrument(skip_all, fields(uri = %uri), level = "debug")]
    async fn list_nonrecursive(&self, uri: &str) -> Result<BucketListing> {
        // Parse our URI.
        let (scheme, bucket, path) = parse_cloud_storage_uri(uri)?;
        assert_eq!(scheme, self.scheme());
        let is_potential_object = !path_looks_like_prefix(path);
        trace!(scheme, bucket, path, ?is_potential_object, "listing");

        // Convert our path to something our library can use.
        let object_path = if path.is_empty() {
            None
        } else {
            Some(ObjectPath::from(path))
        };

        if is_potential_object && let Some(object_path) = &object_path {
            // We have a URL without a trailing "/", which may point to an object
            // in a bucket. So we need to check that first.
            trace!("checking for object at {object_path} (because no trailing slash)");
            let meta_result = self.store().head(object_path).await;
            match meta_result {
                Ok(meta) => {
                    return Ok(BucketListing::object(BucketObject::new(
                        scheme, bucket, &meta,
                    )?));
                }
                Err(object_store::Error::NotFound { .. }) => {
                    trace!("no object found at {uri}");
                }
                Err(e) => {
                    error!("error trying to fetch metadata for {uri}: {e}");
                }
            }
        }

        // Do our actual listing. See the note above about how the
        // library handles "foo/bar" and "foo/bar_baz".
        trace!("listing immediate child entries of {uri}");
        let list_result = self
            .store()
            .list_with_delimiter(object_path.as_ref())
            .await
            .with_context(|| format!("error listing {scheme} objects"))?;
        trace!(?list_result, "list result");

        // Build our raw objects and prefixes.
        let mut entries = vec![];
        for object in &list_result.objects {
            entries.push(BucketEntry::Object(BucketObject::new(
                scheme, bucket, object,
            )?));
        }
        for prefix in &list_result.common_prefixes {
            entries.push(BucketEntry::Prefix(BucketPrefix::new(
                scheme, bucket, prefix,
            )?));
        }

        // This will enforce our invariants.
        BucketListing::prefix_entries(entries)
    }

    /// Synchronize `uri` down to `local_path` recursively. Does not delete any
    /// existing destination files. The contents of `uri` should be exactly
    /// represented in `local_path`, without the trailing subdirectory name
    /// being inserted—this is a straight directory-to-directory sync.
    ///
    /// To sync down a file, neither `uri` nor `local_path` should end in `/`.
    /// To sync down a directory, _both_ `uri` and `local_path` must end in `/`.
    /// Any other combination is allowed to fail or panic at the discretion of
    /// the implementation.
    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        trace!("downloading {} to {}", uri, local_path.display());

        let (_, _, key) = parse_cloud_storage_uri(uri)?;

        if path_looks_like_prefix(key) {
            // We have a directory. If our source URI ends in `/`, so should our
            // `local_path`, since we generate these ourselves.
            async_fs::create_dir_all(local_path)
                .await
                .context("cannot create local download directory")?;

            let prefix = ObjectPath::from(key);
            let mut stream = self.store().list(Some(&prefix));

            // TODO: This has _massively_ insufficient parallelism for many use cases.
            // We need to do something with buffer_unordered and specified concurrency.
            while let Some(meta) = stream
                .try_next()
                .await
                .context("error listing bucket objects")?
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

                stream_download_to_file(self.store(), &meta.location, &file_path)
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
            stream_download_to_file(self.store(), &object_path, local_path).await?;
        }

        Ok(())
    }

    /// Synchronize `local_path` to `uri` recursively. Does not delete any
    /// existing destination files. The contents of `local_path` should be
    /// exactly represented in `uri`, without the trailing subdirectory name
    /// being inserted—this is a straight directory-to-directory sync.
    #[instrument(skip_all, fields(local_path = %local_path.display(), uri = %uri), level = "trace")]
    async fn sync_up(&self, local_path: &Path, uri: &str) -> Result<()> {
        trace!("uploading {} to {}", local_path.display(), uri);

        let (_, _, key) = parse_cloud_storage_uri(uri)?;
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
            stream_upload_from_file(self.store(), file_path, &object_path)
                .await
                .with_context(|| {
                    format!("error uploading to cloud bucket: {}", object_key)
                })?;
        }

        Ok(())
    }
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

/// Parse a cloud storage URL into (bucket, key).
fn parse_cloud_storage_uri(url: &str) -> Result<(&str, &str, &str)> {
    lazy_static! {
        static ref RE: Regex = Regex::new(
            "^(?P<scheme>[a-z][a-z0-9]*)://(?P<bucket>[^/]+)(?:/(?P<path>.*))?$"
        )
        .expect("couldn't parse built-in regex");
    }

    let caps = RE
        .captures(url)
        .ok_or_else(|| format_err!("the URL {:?} could not be parsed", url))?;
    let scheme = caps
        .name("scheme")
        .expect("missing hard-coded capture???")
        .as_str();
    let bucket = caps
        .name("bucket")
        .expect("missing hard-coded capture???")
        .as_str();
    let path = caps.name("path").map(|m| m.as_str()).unwrap_or("");
    assert!(!path.starts_with("/"));
    Ok((scheme, bucket, path))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_cloud_storage_uri("gs://top-level").unwrap(),
            ("gs", "top-level", "")
        );
        assert_eq!(
            parse_cloud_storage_uri("gs://top-level/").unwrap(),
            ("gs", "top-level", "")
        );
        assert_eq!(
            parse_cloud_storage_uri("s3://top-level/path").unwrap(),
            ("s3", "top-level", "path")
        );
        assert_eq!(
            parse_cloud_storage_uri("s3://top-level/path/").unwrap(),
            ("s3", "top-level", "path/")
        );
    }

    /// A bucket listing must be mappable to a file system, so entries that
    /// cannot coexist there are rejected: a file "foo" alongside a
    /// directory "foo/" (the file would shadow the directory and anything
    /// under it), a file shadowing another file stored "under" it, and
    /// duplicated entries.
    #[test]
    fn clashing_bucket_entries_are_rejected() {
        // A file and a directory of the same name.
        let entries = vec![
            BucketEntry::Object(BucketObject::from_uri_for_test("gs://b/a/foo", 0)),
            BucketEntry::Prefix(
                BucketPrefix::from_uri("gs://b/a/foo/".to_owned()).unwrap(),
            ),
        ];
        assert!(check_for_bucket_entry_collisions(&entries).is_err());

        // A file shadowing another file stored under it.
        let entries = vec![
            BucketEntry::Object(BucketObject::from_uri_for_test("gs://b/a/foo", 0)),
            BucketEntry::Object(BucketObject::from_uri_for_test(
                "gs://b/a/foo/bar",
                0,
            )),
        ];
        assert!(check_for_bucket_entry_collisions(&entries).is_err());

        // Duplicate entries.
        let entries = vec![
            BucketEntry::Prefix(
                BucketPrefix::from_uri("gs://b/a/foo/".to_owned()).unwrap(),
            ),
            BucketEntry::Prefix(
                BucketPrefix::from_uri("gs://b/a/foo/".to_owned()).unwrap(),
            ),
        ];
        assert!(check_for_bucket_entry_collisions(&entries).is_err());
    }

    /// File-system-compatible combinations of objects and prefixes are
    /// allowed: an object nested under a prefix, and names that merely
    /// start with the same characters ("a0" is not under "a/").
    #[test]
    fn compatible_bucket_entries_are_allowed() {
        let entries = vec![
            BucketEntry::Object(BucketObject::from_uri_for_test("gs://b/a/foo", 0)),
            BucketEntry::Prefix(
                BucketPrefix::from_uri("gs://b/a/a/".to_owned()).unwrap(),
            ),
            BucketEntry::Object(BucketObject::from_uri_for_test("gs://b/a/a0", 0)),
            BucketEntry::Prefix(
                BucketPrefix::from_uri("gs://b/a/bar/".to_owned()).unwrap(),
            ),
            BucketEntry::Object(BucketObject::from_uri_for_test(
                "gs://b/a/bar/baz",
                0,
            )),
        ];
        assert!(check_for_bucket_entry_collisions(&entries).is_ok());
    }
}
