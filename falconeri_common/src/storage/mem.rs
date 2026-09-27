//! Testing: In-memory storage backend, simulating bucket storage.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::{
    ObjectStore, ObjectStoreExt, memory::InMemory, path::Path as ObjectPath,
};

use super::{BucketObject, CloudStorage};
use crate::{
    prelude::*,
    storage::{CloudStorageForUri, parse_cloud_storage_uri},
};

/// Keeps track of a set of [`MemoryStorage`] implementations, one
/// for each `memory://` bucket.
///
/// This allows tests to see a persistent storage state across multiple calls to
/// [`MemoryStorageResolver::for_uri`], which is needed for tests.
#[derive(Default)]
pub struct MemoryStorageResolver {
    bucket_storage_map: HashMap<String, Arc<MemoryStorage>>,
}

impl MemoryStorageResolver {
    /// Get the [`MemoryStorage`] for `bucket`, creating it if it doesn't
    /// exist yet.
    fn bucket(&mut self, bucket: &str) -> Arc<MemoryStorage> {
        self.bucket_storage_map
            .entry(bucket.to_owned())
            .or_insert_with(|| Arc::new(MemoryStorage::new(bucket)))
            .to_owned()
    }

    /// Seed our buckets with a set of [`BucketObject`]s, as if they had really
    /// been stored there.
    ///
    /// Objects are routed to the bucket named in their URI, creating bucket
    /// storage on demand. Only objects can be seeded: [`InMemory`] cannot
    /// represent marker objects, so prefixes are _derived_, never stored. A
    /// test that wants a directory should seed a `.keep` object underneath it;
    /// a prefix with no seeded contents simply does not exist. (Prefixes still
    /// flow _out_ of listings, derived from the keys underneath them.)
    ///
    /// Object URIs are already percent-encoded, so paths are _parsed_, never
    /// re-encoded. (This is the opposite of [`MemoryStorage::insert`], which
    /// takes literal names — see the "two worlds of text" note in [`super`].)
    ///
    /// Duplicates are fine: putting the same key twice is a harmless
    /// overwrite, which lets separately generated fragments share buckets.
    pub async fn populate(&mut self, objects: &[BucketObject]) -> Result<()> {
        for object in objects {
            let (scheme, bucket, path) = parse_cloud_storage_uri(&object.uri)?;
            assert_eq!(scheme, "memory");
            let storage = self.bucket(bucket);
            let payload = object_store::PutPayload::from_static(b"x");
            let key = ObjectPath::parse(path).with_context(|| {
                format!("invalid path in entry URI {:?}", object.uri)
            })?;
            storage
                .store
                .put(&key, payload)
                .await
                .with_context(|| format!("cannot populate {}", object.uri))?;
        }
        Ok(())
    }
}

#[async_trait]
impl CloudStorageForUri for MemoryStorageResolver {
    async fn for_uri(&mut self, bucket_uri: &str) -> Result<Arc<dyn CloudStorage>> {
        let (scheme, bucket, _path) = parse_cloud_storage_uri(bucket_uri)?;
        assert_eq!(scheme, "memory");
        let storage = self.bucket(bucket);
        Ok(storage as Arc<dyn CloudStorage>)
    }
}

/// Simulated bucket storage for testing.
///
/// This wraps [`object_store::memory::InMemory`], which implements the
/// listing semantics we depend on: `list` and `list_with_delimiter` both
/// evaluate prefixes _on a path segment basis_, so `a` never matches `ab/c`.
///
/// ### Limitations
///
/// `object_store::path::Path` normalizes away trailing slashes, so this
/// backend _cannot_ store "marker" objects like `a/`, which real buckets use
/// to represent empty directories. Anything involving marker objects must be
/// tested against a real (or emulated) bucket. For the same reason we can't
/// represent `a` and `a/` coexisting, so filesystem-invariant violations are
/// out of reach here as well; those are covered directly by
/// [`super::check_for_bucket_entry_collisions`].
#[derive(Debug)]
pub struct MemoryStorage {
    bucket: String,
    store: InMemory,
}

impl MemoryStorage {
    /// Create a new, empty [`MemoryStorage`].
    pub fn new(bucket: &str) -> Self {
        MemoryStorage {
            bucket: bucket.to_owned(),
            store: InMemory::default(),
        }
    }

    /// Create a [`MemoryStorage`] pre-loaded with objects.
    ///
    /// Each item is a `(path, contents)` pair, where `path` is relative to
    /// the bucket root and has no leading slash.
    pub async fn with_objects<const N: usize>(
        bucket: &str,
        objects: [(&str, &str); N],
    ) -> Result<Self> {
        let storage = Self::new(bucket);
        for (path, contents) in objects {
            storage.insert(path, contents.as_bytes()).await?;
        }
        Ok(storage)
    }

    /// Store a single object at `path`, which is relative to the bucket root
    /// and should not begin or end with a slash.
    ///
    /// `path` is _literal_ text, like a local file name, so it is percent-encoded
    /// on the way in: `"100%.txt"` is stored under the key `100%25.txt`.
    pub async fn insert(&self, path: &str, contents: &[u8]) -> Result<()> {
        let payload = object_store::PutPayload::from(contents.to_vec());
        self.store
            .put(&ObjectPath::from(path), payload)
            .await
            .with_context(|| format!("cannot insert test object {path}"))?;
        Ok(())
    }

    /// Fetch the contents of the object at `path`, for asserting that uploads
    /// landed where we expected.
    ///
    /// Like [`Self::insert`], `path` is literal text and is percent-encoded.
    pub async fn contents(&self, path: &str) -> Result<Vec<u8>> {
        let bytes = self
            .store
            .get(&ObjectPath::from(path))
            .await
            .with_context(|| format!("cannot fetch test object {path}"))?
            .bytes()
            .await
            .with_context(|| format!("cannot read test object {path}"))?;
        Ok(bytes.to_vec())
    }

    /// Build a URI for `path` (or a `…/` prefix) inside our bucket.
    pub fn uri(&self, path: &str) -> String {
        format!("memory://{}/{path}", self.bucket)
    }

    /// The URI of our bucket root, which always ends in "/".
    pub fn bucket_uri(&self) -> String {
        self.uri("")
    }
}

#[async_trait]
impl CloudStorage for MemoryStorage {
    fn scheme(&self) -> &'static str {
        "memory"
    }

    fn store(&self) -> &dyn ObjectStore {
        &self.store
    }
}
