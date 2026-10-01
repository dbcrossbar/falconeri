//! Helpers shared by the storage unit tests.
//!
//! Tests for the sync functions live in [`super::sync`], next to the code
//! they test, but still exercise it through the [`CloudStorage`] surface
//! defined in [`super`]. These helpers support both test modules.

use predicates::prelude::*;

use super::{
    BucketEntry, BucketListing, BucketObject, BucketPrefix, CloudStorage,
    mem::MemoryStorage,
};
use crate::prelude::*;

/// A predicate asserting that a file contains exactly `expected`.
pub fn file_contents(expected: &str) -> impl predicates::Predicate<Path> {
    predicate::str::diff(expected.to_owned())
        .from_utf8()
        .from_file_path()
}

/// Our standard fixture. Every object is exactly one byte long, so
/// listings can assert exact [`BucketObject`]s without computing sizes.
///
/// ```text
/// top.txt
/// a0.txt
/// a/e.txt
/// a/b/c.txt
/// a/b/d.txt
/// ab/c         (traps any "a" matched as a raw string prefix)
/// d1/sub/f.txt
/// d2/sub/g.txt
/// d3/other.txt (a top-level directory with no "sub")
/// d4/sub       (a _file_ named "sub" under a top-level directory)
/// ```
pub async fn fixture() -> Result<MemoryStorage> {
    MemoryStorage::with_objects(
        "bucket",
        [
            ("top.txt", "t"),
            ("a0.txt", "0"),
            ("a/e.txt", "e"),
            ("a/b/c.txt", "c"),
            ("a/b/d.txt", "d"),
            ("ab/c", "x"),
            ("d1/sub/f.txt", "f"),
            ("d2/sub/g.txt", "g"),
            ("d3/other.txt", "o"),
            ("d4/sub", "s"),
        ],
    )
    .await
}

/// An expected object entry. All fixture objects are one byte long.
pub fn object(uri: &str) -> BucketEntry {
    BucketEntry::Object(BucketObject::from_uri_for_test(uri, 1))
}

/// An expected prefix entry.
pub fn prefix(uri: &str) -> BucketEntry {
    BucketEntry::Prefix(BucketPrefix::from_uri(uri.to_owned()).unwrap())
}

/// Our listings make no promises about order, which comes from the
/// underlying object store, so we compare entry lists up to permutation.
pub fn sorted(mut entries: Vec<BucketEntry>) -> Vec<BucketEntry> {
    entries.sort();
    entries
}

/// List `uri` and require a `BucketListing::PrefixEntries`, which is what
/// a trailing slash always guarantees.
pub async fn prefix_entries(
    storage: &MemoryStorage,
    uri: &str,
) -> Result<Vec<BucketEntry>> {
    match storage.list_nonrecursive(uri).await? {
        BucketListing::PrefixEntries(entries) => Ok(entries),
        BucketListing::Object(object) => {
            panic!("expected a listing for {uri}, got object {}", object.uri)
        }
    }
}
