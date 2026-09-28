//! Cloud storage backends.
//!
//! ### This is not a simple, raw API
//!
//! One important detail to keep in mind: [`object_store`] is a non-trivial
//! wrapper over the raw cloud APIs, and **it changes behavior in several
//! critical ways**, especially in prefix listing. When in doubt, do not rely on
//! your intuitions about S3, but instead read the comments carefully and check
//! the docs for `object_store`, rather than assuming that everything works like
//! a transparent S3 wrapper.
//!
//! ## Entries vs prefixes
//!
//! A bucket location is one of two kinds:
//!
//! - An **object** ("file"), whose canonical URI never ends in `/`.
//! - A **prefix** ("directory"), whose canonical URI always ends in `/`.
//!
//! [`BucketObject`] and [`BucketPrefix`] only ever hold URIs in this
//! _canonical_ form, which makes it safe to compare returned URIs as strings.
//!
//! ### We only support bucket layouts which can be synced with file systems
//!
//! We only officially support bucket layouts that can be represented on a
//! standard POSIX filesystem. So "s3://b/a" (the object) and "s3://b/a/file"
//! are assumed to never co-exist, and naming _objects_ things like
//! "s3://b/a/file/" is disallowed even if the underlying bucket service
//! supports it. We will report an error if we _notice_ these shenanigans, but
//! we do not go too far out of our way to detect them (because that amybe
//! require many extra API calls).
//!
//! ### `object_store` is not a raw S3 wrapper!
//!
//! There is also a complication caused by our use of `object_store`:
//!
//! - [`object_store::path::Path`] canonicalizes all paths without a trailing
//!   slash, but when performing a prefix listing, it adds and honors a trailing
//!   slash. See the docs for that type; it has other rules.
//! - Our public API, on the other hand, _does_ use trailing slashes on prefixes,
//!   both in output, and (in some cases) when interpreting input. We try to be
//!   very careful and smart about the semantics of this, but we do have some
//!   older code that should be examined carefully.
//!
//! Finally, [`CloudStorage::list_nonrecursive`] _always_ treats
//! `s3://b/p/` as a prefix, but it may treat `s3://b/e` as either an object or
//! a prefix. See the docs.
//!
//! ### Summary of key semantics
//!
//! - Kinds. Prefix URI canonically ends in /; Object URI never does.
//!   BucketPrefix/BucketObject always hold canonical strings — this is what makes
//!   URI string comparison safe for check_for_bucket_entry_collisions and BaseUri
//!   map keys.
//! - Spelling. Input path empty or ends in / ⇒ Prefix. Some APIs, like
//!   [`CloudStorage::list_nonrecursive`], may probe non-empty paths that do
//!   not end in "/" to see if they're an object or a prefix.
//! - Canonicalize at construction. BucketPrefix::from_uri accepts any legal
//!   spelling, stores canonical form.
//! - Mismatched kinds are hard errors. [`CloudStorage::sync_down`] requires
//!   URI-kind and local-path-kind to agree; [`CloudStorage::sync_up_dir`] only
//!   copies directories. Mismatch ⇒ clear domain error, not leniency and not an
//!   OS error.
//! - Two worlds of text. URI paths and object keys are percent-encoded; local
//!   file names are literal. Crossing the boundary the wrong way corrupts keys:
//!   `object_store`'s `Path::from` _encodes_ literal text (and, as its docs
//!   warn, encodes already-encoded text again), while `Path::parse` accepts
//!   encoded text as-is and `Path::from_url_path` decodes it. So URI → `Path`
//!   always **parses**, local name → key always **encodes**, and `Path` → local
//!   name always **decodes**.
//! - Existence is derived. A prefix exists iff some key is under it (or a
//!   marker object exists). So empty prefix ≡ nonexistent prefix, and an empty
//!   bucket's root probes as None.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt, stream};
use lazy_static::lazy_static;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, path::Path as ObjectPath,
};
use regex::Regex;

use crate::{prelude::*, secret::Secret};

pub mod gs;

/// Testing: in-memory storage backend, simulating bucket storage.
///
/// This is only enabled in `test` builds, or if the `testing` feature is
/// enabled.
#[cfg(any(test, feature = "testing"))]
pub mod mem;
pub mod s3;

/// Streaming file transfers between buckets and the local filesystem, which
/// back [`CloudStorage::sync_down`] and [`CloudStorage::sync_up_dir`].
mod sync;

/// Testing: helpers shared by the storage unit tests.
#[cfg(test)]
mod test_util;

/// Does this path look like a "prefix" as opposed to a bucket object?
fn path_looks_like_prefix(path: &str) -> bool {
    path.is_empty() || path.ends_with("/")
}

/// Interpret `uri` as a prefix, adding a trailing slash if the caller left it
/// off.
///
/// Use this at API boundaries, where a URI is _known_ to name a prefix but may
/// not have been spelled canonically (an egress URI from an older pipeline
/// spec, for example). When the kind of a URI is unknown, use
/// [`BucketPrefix::from_uri`] instead, which rejects object URIs.
pub fn to_prefix_uri(uri: &str) -> String {
    if uri.ends_with('/') {
        uri.to_owned()
    } else {
        format!("{uri}/")
    }
}

/// Does `local_path` name a directory? By convention, if and only if it ends
/// in `/`, matching our URI conventions.
fn local_path_is_dir(local_path: &Path) -> bool {
    local_path.to_string_lossy().ends_with('/')
}

/// Check that both ends of [`CloudStorage::sync_down`] agree about whether we
/// are copying a file or a directory. We refuse to guess: guessing has meant
/// silently writing a file over a prefix, or failing with an obscure OS error.
fn check_sync_down_kinds(
    uri: &str,
    uri_is_prefix: bool,
    local_path: &Path,
) -> Result<()> {
    let local_is_dir = local_path_is_dir(local_path);
    match (uri_is_prefix, local_is_dir) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(format_err!(
            "cannot sync directory {uri} to local path {}: a directory needs a trailing '/'",
            local_path.display(),
        )),
        (false, true) => Err(format_err!(
            "cannot sync object {uri} to local path {}: a file must not end with '/'",
            local_path.display(),
        )),
    }
}

/// Turn the path portion of a cloud storage URI into an [`ObjectPath`].
///
/// URI paths are already percent-encoded, so we must _parse_ them. Using
/// `ObjectPath::from` here would encode them a second time, which means the
/// URIs we hand back could not be fetched again: the key `100%.txt` is listed
/// as `…/100%25.txt`, and re-encoding that would look up `…/100%2525.txt`.
fn object_path_from_uri_path(path: &str) -> Result<ObjectPath> {
    ObjectPath::parse(path)
        .with_context(|| format!("invalid object path in URI: {path:?}"))
}

/// Turn an object key, or the suffix of one, into literal text for use as a
/// relative path on the local filesystem.
///
/// Keys are percent-encoded, local names are not. Decoding via
/// [`ObjectPath::from_url_path`] also rejects `.` and `..` segments, so a bucket
/// cannot escape the directory it is being downloaded into.
fn local_relative_path(encoded_key: &str) -> Result<String> {
    Ok(ObjectPath::from_url_path(encoded_key)
        .with_context(|| format!("cannot decode object key {encoded_key:?}"))?
        .to_string())
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
    ///
    /// This is public, and compiled into normal builds, because `#[cfg(test)]`
    /// items are not visible across crate boundaries, and [`BucketObject`] is
    /// `#[non_exhaustive]`, so other crates' tests cannot build one any other
    /// way.
    #[cfg(any(test, feature = "testing"))]
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
        Self::from_uri(uri)
    }

    /// Construct a [`BucketPrefix`] from a cloud storage URI, which must name
    /// a prefix rather than an object.
    ///
    /// The bucket root is a valid prefix, spelled either `"gs://bucket/"` or
    /// as the bare `"gs://bucket"`; the resulting [`BucketPrefix`] always holds
    /// a canonical URI ending in `/`.
    pub fn from_uri(uri: String) -> Result<Self> {
        // Check that this is a well-formed cloud storage URI which names a
        // prefix rather than an object.
        let (_, _, path) = parse_cloud_storage_uri(&uri)?;
        if !path_looks_like_prefix(path) {
            return Err(format_err!("bucket prefix {uri} does not end with '/'"));
        }

        // Canonicalize the one case our parser cannot tell us about: the bare
        // bucket "gs://bucket" has an empty path, just like the root prefix
        // "gs://bucket/", but our invariant is that a prefix URI always ends
        // in "/", because callers concatenate prefixes with subpaths.
        Ok(Self {
            uri: to_prefix_uri(&uri),
        })
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

/// Given a URL, return a [`CloudStorage`] implementation which can resolve it.
///
/// We don't use many factories in this code base, but this is the easiest way
/// to generalize over the test and non-test cases.
#[async_trait]
pub trait CloudStorageForUri: Send + Sync {
    /// Get the storage backend for the specified URI.
    ///
    /// The `bucket_uri` is used to determine both the storage backend type
    /// (based on the URI scheme like `gs://` or `s3://`) and the bucket name.
    /// It can be any URI within the bucket we want to access.
    ///
    /// If we know about any secrets, we can pass them as the `secrets` array,
    /// and the storage driver can check to see if there are any secrets it can
    /// use to authenticate.
    async fn for_uri(&mut self, bucket_uri: &str) -> Result<Arc<dyn CloudStorage>>;
}

/// Given a URL, return a real, network-backed [`CloudStorage`] implementation
/// which can resolve it.
pub struct CloudStorageResolver {
    secrets: Vec<Secret>,
    // TODO: Do we want to cache resolvers? I think we generally only need 1 or
    // 2 anyway, so maybe it isn't essential.
}

impl CloudStorageResolver {
    /// Create a new resolver with the specified secrets.
    pub fn new(secrets: Vec<Secret>) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl CloudStorageForUri for CloudStorageResolver {
    async fn for_uri(&mut self, bucket_uri: &str) -> Result<Arc<dyn CloudStorage>> {
        if bucket_uri.starts_with("gs://") {
            Ok(Arc::new(
                gs::GoogleCloudStorage::new(&self.secrets, bucket_uri).await?,
            ))
        } else if bucket_uri.starts_with("s3://") {
            Ok(Arc::new(
                s3::S3Storage::new(&self.secrets, bucket_uri).await?,
            ))
        } else {
            Err(format_err!(
                "cannot find storage backend for {}",
                bucket_uri
            ))
        }
    }
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
            Some(object_path_from_uri_path(path)?)
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

    /// Probe `uri`, and see if it contains an object, a prefix or neither.
    ///
    /// Note that an _empty_ prefix probes as `None`, because a prefix exists
    /// only if some key lies under it. See the module docs on derived
    /// existence.
    ///
    /// This will potentially be called once per datum (so N=1,000, which is
    /// the recommended reasonable number of datums) concurrently. So it needs
    /// to be reasonably efficient and a good citizen.
    ///
    /// Performance: The default version requires up to 2 API calls but should
    /// work with any backend. It should also be possible to override this for
    /// backends that support [`object_store::list::PaginatedListStore`] to test
    /// both the object _and_ prefix cases in a single call, probably with
    /// `max_keys: 2`.
    #[instrument(skip_all, fields(uri = %uri), level = "trace")]
    async fn list_one_entry(&self, uri: &str) -> Result<Option<BucketEntry>> {
        // Parse our URI.
        let (scheme, bucket, path) = parse_cloud_storage_uri(uri)?;
        assert_eq!(scheme, self.scheme());

        // Convert our path to something our library can use.
        let object_path = if path.is_empty() {
            None
        } else {
            Some(object_path_from_uri_path(path)?)
        };

        // First, check for an exact object at `uri`. Only `NotFound`
        // means "no file"; any other error (which our client has already
        // retried with backoff) is a real failure.
        //
        // We skip this check if the path ends in "/" or otherwise looks
        // like a prefix, in which case it only matches directories.
        if !path_looks_like_prefix(path)
            && let Some(object_path) = &object_path
        {
            match self.store().head(object_path).await {
                Ok(meta) => {
                    return Ok(Some(BucketEntry::Object(BucketObject::new(
                        scheme, bucket, &meta,
                    )?)));
                }
                Err(object_store::Error::NotFound { .. }) => {
                    trace!("no object found at {uri}");
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("error fetching metadata for {uri}")
                    });
                }
            }
        }

        // Next, check for a directory. `list` appends a trailing
        // delimiter itself, so this enumerates keys at or under `{uri}/`.
        // We only need the first entry: it exists if and only if the
        // directory does (a marker-only directory yields its marker as
        // the first key). Dropping the stream cancels any further pages.
        let mut stream = self.store().list(object_path.as_ref());
        match stream
            .try_next()
            .await
            .with_context(|| format!("error listing {scheme} objects under {uri}/"))?
        {
            Some(_) => {
                let dir_uri = if uri.ends_with('/') {
                    uri.to_owned()
                } else {
                    format!("{uri}/")
                };
                trace!("directory found at {dir_uri}");
                Ok(Some(BucketEntry::Prefix(BucketPrefix::from_uri(dir_uri)?)))
            }
            None => {
                trace!("nothing found at {uri}");
                Ok(None)
            }
        }
    }

    /// Perform the partially-recursive listing required by
    /// [`crate::pipeline::Glob::Subpath`] (`/*/$SUBPATH`) as efficiently as
    /// possible.
    ///
    /// For each top-level _directory_ entry `E` of `base_uri`, return the
    /// entry `E/subpath` if it exists: as an object (a file) or a prefix
    /// (a directory, including a marker-only "empty" one). Top-level file
    /// entries never match — a file with contents under it is a
    /// file+directory collision, which the base listing already rejects.
    /// Absent subpaths contribute nothing.
    ///
    /// If both `E/subpath` and `E/subpath/` exist, the bucket lacks
    /// filesystem semantics and is invalid input. We do not commit to
    /// detecting this: each probe returns whichever half it found first
    /// (see [`list_one_entry`]).
    ///
    /// ### Performance
    ///
    /// This may represent a few thousand network calls, so it has been
    /// carefully designed and included directly in the storage layer. Key
    /// factors and numbers that drove this design:
    ///
    /// **Why a dedicated API?** [`Self::list_nonrecursive`] could be used to
    /// build this, but to detect "a/" prefixes, it would need to list all their
    /// children, even if there are thousands. Having a dedicated path allow us
    /// to optimize.
    ///
    /// **Why per-entry probes, rather than one recursive listing?** Qwen3.8
    /// 27B did the math, and showed that (given Falcoenri's preference for
    /// <= 1,000 datums but aspirational support for larger numbers of files
    /// inside a datum) selectively probing comes out ahead:
    ///
    /// > A recursive listing of `base_uri` is N/1,000 *sequential* pages (a
    /// > continuation-token chain that cannot be parallelized), with cost
    /// > scaling with the repo's total size. Reported numbers: ~20 min to
    /// > list 1M S3 objects single-threaded (2023); on a bucket with 10M
    /// > versioned delete markers, a single page averaged ~536 ms with 30+ s
    /// > cold starts, while a narrow prefix listed in ~8 ms (2025) — S3's
    /// > per-page cost scales with what it must scan; and `gsutil du`, which
    /// > lists in parallel, "sometimes takes hours" on buckets with millions
    /// > of objects (GCS). Per-entry probes instead issue O(K) independent,
    /// > bounded, parallelizable calls (K = number of top-level directory
    /// > entries), with cost set by the glob's top-level fan-out rather than
    /// > the repo's depth or size; a subpath holding 1,000+ entries still
    /// > costs a single page. A recursive listing only wins for shallow,
    /// > dense repos (total objects a small multiple of K), which is not the
    /// > profile we target (top-level entries in the low thousands, files up
    /// > to ~10⁶), where probes win by minutes versus seconds.
    /// >
    /// > **Concurrency and rate limits.** The probe fan-out is bounded (50
    /// > concurrent). At ~50–150 ms per request that is ~300–1,000 QPS:
    /// > ~7–20% of GCS's initial per-bucket read capacity (~5,000 read
    /// > requests/s, listing included; it auto-scales above that, and the
    /// > ramp-up guidance only applies past the threshold). S3's rate
    /// > scaling is per-prefix, and these probes present thousands of
    /// > distinct prefixes — the best case for that model. This runs inside
    /// > falconerid's job-creation handler, where seconds matter and minutes
    /// > do not: at the sane scale (K ≈ 1,000 — splitting beyond ~1,000
    /// > datums is suspicious, since falconeri prefers large datums) the
    /// > burst is ~4–12 s; the K = 10,000 outlier takes ~40–60 s.
    ///
    /// **Failure policy.** We rely heavily on [`object_store`]'s support
    /// for rate-limiting and exponential backoff in case we hit limits.
    #[instrument(skip_all, fields(base_uri = %base_uri, subpath = %subpath), level = "debug")]
    async fn list_subpath_entries(
        &self,
        base_uri: &str,
        subpath: &str,
    ) -> Result<Vec<BucketEntry>> {
        // Get the top-level listing of our base.
        let listing = self.list_nonrecursive(base_uri).await?;

        // Only directory entries can contain subpaths: a file entry with
        // contents under it would be a file+directory collision, which
        // the listing above already rejected. (`prefix.uri` ends in "/"
        // and `subpath` has no leading or trailing slash, so the
        // concatenation is well-formed.)
        let probe_uris: Vec<String> = match listing {
            // Our base is a file, so it has no entries at all.
            BucketListing::Object(_) => return Ok(vec![]),
            BucketListing::PrefixEntries(entries) => entries
                .iter()
                .filter_map(|entry| match entry {
                    BucketEntry::Prefix(prefix) => {
                        Some(format!("{}{}", prefix.uri, subpath))
                    }
                    BucketEntry::Object(_) => None,
                })
                .collect(),
        };

        // Probe each directory entry concurrently. `buffer_unordered`
        // bounds our in-flight requests (see the rate-limiting notes
        // above); if we hit a limit, our client's backoff handles it.
        const PROBE_CONCURRENCY: usize = 50;
        let probed = stream::iter(probe_uris)
            .map(|probe_uri| async move { self.list_one_entry(&probe_uri).await })
            .buffer_unordered(PROBE_CONCURRENCY)
            .try_collect::<Vec<Option<BucketEntry>>>()
            .await?;

        // Absent subpaths contribute nothing.
        Ok(probed.into_iter().flatten().collect())
    }

    /// Synchronize `uri` down to `local_path` recursively. Does not delete any
    /// existing destination files. The contents of `uri` should be exactly
    /// represented in `local_path`, without the trailing subdirectory name
    /// being inserted—this is a straight directory-to-directory sync.
    ///
    /// To sync down a file, neither `uri` nor `local_path` should end in `/`.
    /// To sync down a directory, _both_ `uri` and `local_path` must end in `/`.
    /// Any other combination is an error: we will not guess which one you
    /// meant.
    ///
    /// Syncing down a non-existant prefix will create an empty directory.
    ///
    /// Local names are the percent-decoded form of each object key, so a key of
    /// `100%25.txt` is stored locally as `100%.txt`.
    #[instrument(skip_all, fields(uri = %uri, local_path = %local_path.display()), level = "trace")]
    async fn sync_down(&self, uri: &str, local_path: &Path) -> Result<()> {
        sync::sync_down(self.store(), uri, local_path).await
    }

    /// Synchronize the local directory `local_path` up to the bucket prefix
    /// `uri`. Does not delete any existing destination files. The contents of
    /// `local_path` should be exactly represented in `uri`, without the trailing
    /// subdirectory name being inserted—this is a straight directory-to-
    /// directory copy.
    ///
    /// Unlike [`Self::sync_down`], this only copies directories: `local_path`
    /// must end in `/`, and so must `uri`. There is no symmetric "upload one
    /// file" case, because we do not need that.
    ///
    /// Local names are literal text and are percent-encoded as they become
    /// object keys, so `100%.txt` is stored under the key `100%25.txt`.
    #[instrument(skip_all, fields(local_path = %local_path.display(), uri = %uri), level = "trace")]
    async fn sync_up_dir(&self, local_path: &Path, uri: &str) -> Result<()> {
        sync::sync_up_dir(self.store(), local_path, uri).await
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
    use assert_fs::{TempDir, prelude::*};

    use super::test_util::{
        file_contents, fixture, object, prefix, prefix_entries, sorted,
    };
    use super::{mem::MemoryStorage, *};

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

    /// A prefix URI always ends in `/`, but the bucket root may be spelled
    /// without one, so both `gs://b` and `gs://b/` name the root prefix and
    /// canonicalize to the same thing.
    #[test]
    fn bucket_prefix_uri_canonicalization() {
        let root = BucketPrefix::from_uri("gs://b".to_owned()).unwrap();
        assert_eq!(root.uri, "gs://b/");
        assert_eq!(BucketPrefix::from_uri("gs://b/".to_owned()).unwrap(), root);
        assert_eq!(
            BucketPrefix::from_uri("gs://b/a/".to_owned()).unwrap().uri,
            "gs://b/a/",
        );

        // An object URI is not a prefix.
        assert!(BucketPrefix::from_uri("gs://b/a".to_owned()).is_err());
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

    /// Listing the bucket root yields top-level objects and prefixes, and
    /// nothing from deeper levels.
    #[tokio::test]
    async fn test_list_nonrecursive_root() -> Result<()> {
        let storage = fixture().await?;

        let entries = prefix_entries(&storage, &storage.bucket_uri()).await?;
        assert_eq!(
            sorted(entries),
            sorted(vec![
                object(&storage.uri("a0.txt")),
                object(&storage.uri("top.txt")),
                prefix(&storage.uri("a/")),
                prefix(&storage.uri("ab/")),
                prefix(&storage.uri("d1/")),
                prefix(&storage.uri("d2/")),
                prefix(&storage.uri("d3/")),
                prefix(&storage.uri("d4/")),
            ]),
        );

        Ok(())
    }

    /// A URI without a trailing slash which names an object lists as that
    /// single object, including its size.
    #[tokio::test]
    async fn test_list_nonrecursive_object() -> Result<()> {
        let storage = fixture().await?;

        match storage.list_nonrecursive(&storage.uri("a/e.txt")).await? {
            BucketListing::Object(object) => assert_eq!(
                object,
                BucketObject::from_uri_for_test("memory://bucket/a/e.txt", 1)
            ),
            BucketListing::PrefixEntries(entries) => {
                panic!("expected an object, got {} entries", entries.len())
            }
        }

        Ok(())
    }

    /// Listing a prefix returns only its immediate children: not `a/b/c.txt`
    /// (no recursion), not `a/` itself, and not `a0.txt`, which merely shares
    /// a raw string prefix with `a`.
    #[tokio::test]
    async fn test_list_nonrecursive_prefix() -> Result<()> {
        let storage = fixture().await?;

        let entries = prefix_entries(&storage, &storage.uri("a/")).await?;
        assert_eq!(
            sorted(entries),
            sorted(vec![
                object(&storage.uri("a/e.txt")),
                prefix(&storage.uri("a/b/")),
            ]),
        );

        Ok(())
    }

    /// Missing paths produce empty listings rather than errors, whether or
    /// not they end in a slash.
    #[tokio::test]
    async fn test_list_nonrecursive_missing_is_empty() -> Result<()> {
        let storage = fixture().await?;

        assert!(
            prefix_entries(&storage, &storage.uri("nope/"))
                .await?
                .is_empty()
        );
        assert!(
            prefix_entries(&storage, &storage.uri("nope"))
                .await?
                .is_empty()
        );

        Ok(())
    }

    /// Probing an object finds the object.
    #[tokio::test]
    async fn test_list_one_entry_object() -> Result<()> {
        let storage = fixture().await?;

        let entry = storage.list_one_entry(&storage.uri("a/b/c.txt")).await?;
        assert_eq!(
            entry,
            Some(object("memory://bucket/a/b/c.txt")),
            "should find the object itself"
        );

        Ok(())
    }

    /// Probing a directory finds a prefix, with exactly one trailing slash
    /// whether or not the caller supplied one.
    #[tokio::test]
    async fn test_list_one_entry_prefix() -> Result<()> {
        let storage = fixture().await?;

        assert_eq!(
            storage.list_one_entry(&storage.uri("a")).await?,
            Some(prefix("memory://bucket/a/")),
            "a bare directory path should gain a trailing slash",
        );
        assert_eq!(
            storage.list_one_entry(&storage.uri("a/")).await?,
            Some(prefix("memory://bucket/a/")),
            "a trailing slash should not be doubled",
        );

        Ok(())
    }

    /// Probing a path that exists neither as an object nor as a directory
    /// returns `None`—including when a sibling merely shares a raw string
    /// prefix, which the `object_store` segment-basis contract rules out.
    #[tokio::test]
    async fn test_list_one_entry_missing() -> Result<()> {
        let storage = fixture().await?;

        assert_eq!(storage.list_one_entry(&storage.uri("nope")).await?, None);
        assert_eq!(storage.list_one_entry(&storage.uri("nope/")).await?, None);

        // "a" must not match "ab/c", which would make it look like a
        // directory exists at "a/".
        let only_sibling =
            MemoryStorage::with_objects("bucket", [("ab/c", "x")]).await?;
        assert_eq!(
            only_sibling.list_one_entry(&only_sibling.uri("a")).await?,
            None
        );

        Ok(())
    }

    /// `/*/$SUBPATH` finds subpaths under top-level directories, whether they
    /// are directories or files, and skips top-level files and directories
    /// which lack the subpath.
    #[tokio::test]
    async fn test_list_subpath_entries() -> Result<()> {
        let storage = fixture().await?;

        let entries = storage
            .list_subpath_entries(&storage.bucket_uri(), "sub")
            .await?;
        assert_eq!(
            sorted(entries),
            sorted(vec![
                object(&storage.uri("d4/sub")),
                prefix(&storage.uri("d1/sub/")),
                prefix(&storage.uri("d2/sub/")),
            ]),
        );

        // A subpath nobody has yields nothing at all.
        assert!(
            storage
                .list_subpath_entries(&storage.bucket_uri(), "nope")
                .await?
                .is_empty()
        );

        Ok(())
    }

    /// A base URI which names a file has no entries, so no subpaths either.
    #[tokio::test]
    async fn test_list_subpath_entries_base_is_file() -> Result<()> {
        let storage = fixture().await?;

        assert!(
            storage
                .list_subpath_entries(&storage.uri("top.txt"), "sub")
                .await?
                .is_empty()
        );

        Ok(())
    }

    /// Probing is concurrent and bounded, so make sure a fan-out larger than
    /// our concurrency limit neither drops nor duplicates entries.
    #[tokio::test]
    async fn test_list_subpath_entries_beyond_concurrency_limit() -> Result<()> {
        let storage = MemoryStorage::new("bucket");
        let mut expected = vec![];
        for i in 0..60 {
            let name = format!("dir{i:02}");
            storage.insert(&format!("{name}/sub/f.txt"), b"f").await?;
            expected.push(prefix(&storage.uri(&format!("{name}/sub/"))));
        }

        let entries = storage
            .list_subpath_entries(&storage.bucket_uri(), "sub")
            .await?;
        assert_eq!(sorted(entries), sorted(expected));

        Ok(())
    }

    /// Object keys are percent-encoded text, local names are literal text, and
    /// every crossing of that boundary has to convert in the right direction.
    #[tokio::test]
    async fn test_escaped_names_cross_the_boundary() -> Result<()> {
        let storage = MemoryStorage::new("bucket");
        storage.insert("esc/100%.txt", b"x").await?;

        // Listings give us the encoded form, as any URI must.
        assert_eq!(
            prefix_entries(&storage, &storage.uri("esc/")).await?,
            vec![object("memory://bucket/esc/100%25.txt")],
        );

        // And the URI we hand back must still be addressable by our own API,
        // which means parsing it rather than encoding it a second time.
        assert_eq!(
            storage
                .list_one_entry("memory://bucket/esc/100%25.txt")
                .await?,
            Some(object("memory://bucket/esc/100%25.txt")),
        );
        let dir = TempDir::new()?;
        storage
            .sync_down("memory://bucket/esc/", &dir.path().join("out/"))
            .await?;
        dir.child("out/100%.txt").assert(file_contents("x"));

        // Uploading encodes literal local names on the way in.
        let up = TempDir::new()?;
        up.child("src/100%.txt").write_str("y")?;
        up.child("src/sub/z.txt").write_str("z")?;
        storage
            .sync_up_dir(&up.path().join("src/"), &storage.uri("esc/"))
            .await?;
        assert_eq!(storage.contents("esc/100%.txt").await?, b"y");

        // A prefix arriving as URI text is parsed, never encoded a second time.
        storage
            .sync_up_dir(&up.path().join("src/"), "memory://bucket/a%20b/")
            .await?;
        let root = prefix_entries(&storage, &storage.bucket_uri()).await?;
        assert!(
            root.contains(&prefix("memory://bucket/a%20b/")),
            "we should keep the escape we were given, got {root:?}",
        );
        assert!(
            !root.contains(&prefix("memory://bucket/a%2520b/")),
            "the prefix must not be double-encoded, got {root:?}",
        );

        // And downloading decodes back to literal names, through a prefix which
        // itself needed an escape.
        let round = TempDir::new()?;
        storage
            .sync_down("memory://bucket/a%20b/", &round.path().join("out/"))
            .await?;
        round.child("out/100%.txt").assert(file_contents("y"));
        round.child("out/sub/z.txt").assert(file_contents("z"));

        Ok(())
    }
}
