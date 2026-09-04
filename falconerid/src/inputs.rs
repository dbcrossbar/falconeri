//! Convert JSON `"input"` clauses to datums which will be assigned to workers.
//!
//! The work is split into two phases:
//!
//! - An async **I/O phase** ([`fetch_listings`]) which walks the input tree,
//!   collects every atom base URI (deduplicated, trailing-slash-normalized),
//!   and fetches one listing per base through [`CloudStorage::list`].
//! - A **pure synchronous core** ([`input_to_datums_pure`]) which interprets
//!   the input algebra over those pre-fetched listings. It is a pure,
//!   deterministic function of its inputs, which is what makes the algebra
//!   testable (see the test harness at the bottom of this file) without
//!   touching a real bucket.
//!
//! Listings are non-recursive: each atom base maps to its top-level entries
//! (files and subdirectories), which is what the algebra's `"/*"` glob
//! distributes over.

use std::collections::BTreeMap;

use async_recursion::async_recursion;
use falconeri_common::{
    models::{NewDatum, NewInputFile},
    pipeline::{Glob, Input},
    prelude::*,
    secret::Secret,
    storage::{
        BucketEntry, BucketListing, BucketPrefix, CloudStorageForUri,
        CloudStorageResolver, check_for_bucket_entry_collisions,
    },
};

/// (Local helper type.) The URI of a repository, normalized to end in `/`.
///
/// Repositories are always directories. A URI without a trailing slash lists
/// successfully (as a prefix), but would then fail in [`uri_to_local_path`],
/// which requires one. So the type carries the normalization guarantee: every
/// `BaseUri` ends in `/`, and [`BaseUri::normalize`] is the only way to make
/// one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BaseUri(String);

impl BaseUri {
    /// Normalize a URI to a base URI: append a trailing `/` if missing.
    fn normalize(uri: &str) -> Self {
        if uri.ends_with('/') {
            Self(uri.to_owned())
        } else {
            Self(format!("{uri}/"))
        }
    }

    /// The underlying string, which is guaranteed to end in `/`.
    fn as_str(&self) -> &str {
        &self.0
    }

    /// Strip this base URI from a path.
    fn strip_from<'p>(&self, path: &'p str) -> Result<&'p str> {
        path.strip_prefix(&self.0).ok_or_else(|| {
            format_err!("path {path:?} expected to start with {self:?} but didn't")
        })
    }
}

/// The I/O phase's output: All the information we need from a cloud bucket in
/// order to run the pure phase of our algorithm.
#[derive(Clone, Debug, Default)]
struct Listings {
    /// Mapping from `BaseUri` to listings, for expanding
    /// [`Glob::TopLevelDirectoryEntries`].
    base_uris: BTreeMap<BaseUri, BucketListing>,
}

impl Listings {
    /// I/O phase: Fetch listings from the cloud.
    #[instrument(skip_all, level = "trace")]
    async fn fetch(secrets: &[Secret], input: &Input) -> Result<Listings> {
        debug!("fetching atom listings");
        let mut resolver = CloudStorageResolver::new(secrets.to_owned());
        let mut listings = Listings::default();
        listings.fetch_helper(&mut resolver, input).await?;
        Ok(listings)
    }

    /// Internal fetch helper.
    #[async_recursion]
    async fn fetch_helper(
        &mut self,
        resolver: &mut dyn CloudStorageForUri,
        input: &Input,
    ) -> Result<()> {
        match input {
            Input::Atom { uri, glob, .. } => {
                let base = BaseUri::normalize(uri);
                let storage = resolver.for_uri(base.as_str()).await?;
                match glob {
                    // We need a listing to handle "/*", so fetch it.
                    Glob::TopLevelDirectoryEntries => {
                        // Don't look it up if we already have it.
                        if !self.base_uris.contains_key(&base) {
                            let listing =
                                storage.list_nonrecursive(base.as_str()).await?;
                            // BaseUri::normalize should force the URI to end in
                            // "/", which should in turn force
                            // `BucketListing::PrefixEntries`.
                            assert!(matches!(
                                listing,
                                BucketListing::PrefixEntries(_),
                            ));
                            self.base_uris.insert(base.clone(), listing);
                        }
                    }
                    // Nothing to fetch, since we'll just use the whole thing.
                    Glob::WholeRepo => {
                        // Just check to make sure this bucket _exists_, so we
                        // can provide errors earlier.
                        let _ = storage.list_nonrecursive(base.as_str()).await?;
                    }
                }
            }
            Input::Cross(inputs) | Input::Union(inputs) => {
                for input in inputs {
                    // Call recursively. We need `boxed_local` so that the impl
                    // Future type created by this function isn't an infinitely
                    // recursive type.
                    self.fetch_helper(resolver, input).await?;
                }
            }
        }
        Ok::<_, Error>(())
    }

    /// (Test only.) Insert a listing for `base` into the listings.
    #[cfg(test)]
    fn base_uri_insert(&mut self, base: BaseUri, listing: BucketListing) {
        self.base_uris.insert(base, listing);
    }

    /// Look up the listing for `base`.
    fn base_uri_get(&self, base: &BaseUri) -> Option<&BucketListing> {
        self.base_uris.get(base)
    }
}

/// One slot of a datum name: the repo where the atom's files land, and the
/// star binding (the top-level entry, `None` for whole-repo atoms).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Slot {
    repo: String,
    binding: Option<String>,
}

/// The name of a datum: the tuple of slots under crosses, in order.
///
/// Two datums with equal names write to the same `/pfs` locations.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DatumName(Vec<Slot>);

/// (Local helper type.) This is essentially just a `NewDatum` and a
/// `Vec<NewInputFile>`, but in a more convenient format that works better with
/// the algorithm in this file, so we don't need to carry around UUIDs
/// everywhere.
///
/// `name` is bookkeeping for the algebra (see [`DatumName`]); it is dropped
/// when converting to database models.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DatumData {
    name: DatumName,
    input_files: Vec<InputFileData>,
}

impl DatumData {
    /// Convert this into an actual `NewDatum` and a `Vec<NewInputFile>`.
    fn into_new_datum_and_input_files(
        self,
        job_id: Uuid,
        maximum_allowed_run_count: i32,
    ) -> (NewDatum, Vec<NewInputFile>) {
        let datum_id = Uuid::new_v4();
        let datum = NewDatum {
            id: datum_id,
            job_id,
            maximum_allowed_run_count,
        };
        let input_files = self
            .input_files
            .into_iter()
            .map(|f| f.into_new_input_file(job_id, datum_id))
            .collect();
        (datum, input_files)
    }
}

/// (Local helper type.) This is essentially a `NewInputFile`, but in a more
/// convenient format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InputFileData {
    entry: BucketEntry,
    local_path: String,
}

impl InputFileData {
    /// Convert this into an actual `NewInputFile`.
    fn into_new_input_file(self, job_id: Uuid, datum_id: Uuid) -> NewInputFile {
        NewInputFile {
            job_id,
            datum_id,
            uri: self.entry.uri().to_owned(),
            local_path: self.local_path,
        }
    }
}

/// Given an `Input` from a JSON pipeline spec, convert to an actual set of
/// "datums" (work chunks) to be assigned to a worker.
///
/// Returns the datums and associated input files in a form well-suited to bulk
/// database insert.
#[instrument(skip_all, fields(job_id = %job_id), level = "trace")]
pub async fn input_to_datums(
    secrets: &[Secret],
    job_id: Uuid,
    maximum_allowed_run_count: i32,
    input: &Input,
) -> Result<(Vec<NewDatum>, Vec<NewInputFile>)> {
    // The I/O phase: fetch the listings that the pure core needs. This also
    // verifies that every atom is listable _before_ spinning up a big cluster
    // job.
    let listings = Listings::fetch(secrets, input).await?;

    // The pure core: interpret the input algebra over those listings.
    let datum_datas = input_to_datums_pure(input, &listings)?;

    // Check for name collisions. We did a version of this when reading
    // individual buckets, but now we need to do it with the full set
    // of paths. It's possible we want to check *more* edge cases than
    // we do here.
    let mut all_entries = vec![];
    for d in &datum_datas {
        for f in &d.input_files {
            all_entries.push(f.entry.clone());
        }
    }
    check_for_bucket_entry_collisions(&all_entries)?;

    let mut all_datums = vec![];
    let mut all_input_files = vec![];
    for datum_data in datum_datas {
        let (datum, input_files) = datum_data
            .into_new_datum_and_input_files(job_id, maximum_allowed_run_count);
        all_datums.push(datum);
        all_input_files.extend(input_files);
    }
    Ok((all_datums, all_input_files))
}

/// (Pure core.) Interpret an `Input` into a sequence of [`DatumData`], given
/// pre-fetched listings.
///
/// `listings` maps each atom base URI to the top-level entries listed under
/// it. The I/O phase ([`fetch_listings`]) is responsible for fetching a
/// listing for _every_ atom base URI in `input`.
///
/// This is a pure, deterministic function of its two inputs. It fails if the
/// input would produce rows that clash in the worker's local file system
/// (see [`verify_local_paths`]).
fn input_to_datums_pure(input: &Input, listings: &Listings) -> Result<Vec<DatumData>> {
    match input {
        Input::Atom { uri, repo, glob } => {
            atom_to_datums_pure(uri, repo, *glob, listings)
        }
        Input::Union(inputs) => {
            // Merge all our inputs, in child order. We only do this
            // inline without a helper because it's the simplest case.
            let mut datums = vec![];
            for child in inputs {
                datums.extend(input_to_datums_pure(child, listings)?);
            }
            Ok(datums)
        }
        Input::Cross(inputs) => cross_to_datums_pure(inputs, listings),
    }
}

/// Interpret a single `Input::Atom` into a list of datums, given the
/// (non-recursive) listing of its base URI.
fn atom_to_datums_pure(
    uri: &str,
    repo: &str,
    glob: Glob,
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    let base = BaseUri::normalize(uri);
    match glob {
        Glob::WholeRepo => glob_whole_repo(&base, repo),
        Glob::TopLevelDirectoryEntries => {
            glob_top_level_directory_entries(&base, repo, listings)
        }
    }
}

/// Handle [`Glob::WholeRepo`]. This is simple, because we don't need to inspect
/// what's in it. Our input file is just the entire repo, as a directory.
fn glob_whole_repo(base: &BaseUri, repo: &str) -> Result<Vec<DatumData>> {
    Ok(vec![DatumData {
        name: DatumName(vec![Slot {
            repo: repo.to_owned(),
            binding: None,
        }]),
        input_files: vec![InputFileData {
            entry: BucketEntry::Prefix(BucketPrefix::from_uri(
                base.as_str().to_owned(),
            )?),
            local_path: format!("/pfs/{}/", repo),
        }],
    }])
}

/// Handle [`Glob::TopLevelDirectoryEntries`]. One datum per top-level entry
/// (file or directory), in the order the listing provides them. File entries
/// map to `/pfs/R/E`; directory entries map to `/pfs/R/E/` (trailing slash),
/// which the worker syncs recursively. The datum's name binds the repo to the
/// entry `E`.
fn glob_top_level_directory_entries(
    base: &BaseUri,
    repo: &str,
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    // The I/O phase fetched a listing for every atom base, so this
    // can only fail if `input_to_datums_pure` was called with an
    // incomplete map (a programmer error).
    let listing = listings
        .base_uri_get(base)
        .expect("no listing for atom base; the I/O phase must list every atom base");

    let mut datums = vec![];
    if let BucketListing::PrefixEntries(entries) = listing {
        for entry in entries {
            // Figure out the name of our datum by stripping the base
            // URI and any trailing slash, giving us just the part that
            // matches the "*" in "/*".
            let binding =
                base.strip_from(entry.uri().strip_suffix("/").unwrap_or(entry.uri()))?;
            // Get the local "/pfs" version of the path.
            let local_path = uri_to_local_path(base, entry.uri(), repo)?;
            datums.push(DatumData {
                name: DatumName(vec![Slot {
                    repo: repo.to_owned(),
                    binding: Some(binding.to_owned()),
                }]),
                input_files: vec![InputFileData {
                    entry: entry.to_owned(),
                    local_path,
                }],
            });
        }
    }
    Ok(datums)
}

/// Interpret a cross product into a list of datums.
///
/// SECURITY: This assumes it runs on reasonably trusted and plausible inputs.
/// You can cause a denial-of-service by calculating the cross product of
/// enormous repos, or by passing in so many repos that the stack overflows. But
/// since our input comes from a local user, this is fine for now.
fn cross_to_datums_pure(
    inputs: &[Input],
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    match inputs.len() {
        // Base cases.
        0 => Ok(vec![]),
        1 => input_to_datums_pure(&inputs[0], listings),

        // Recursive case.
        n => {
            // Recursively calculate the cross product of all but our last input.
            let datums_0 = cross_to_datums_pure(&inputs[0..n - 1], listings)?;

            // Process our last input.
            let datums_1 = input_to_datums_pure(&inputs[n - 1], listings)?;

            // Build our cross product between the recursive `datums_0` and our
            // local `datums_1`. Names (slot tuples) and files are both
            // concatenated in the same order.
            let mut output = vec![];
            for datum_0 in &datums_0 {
                for datum_1 in &datums_1 {
                    let input_files_0 = &datum_0.input_files;
                    let input_files_1 = &datum_1.input_files;
                    let len_0 = input_files_0.len();
                    let len_1 = input_files_1.len();
                    let mut combined = Vec::with_capacity(len_0 + len_1);
                    combined.extend(input_files_0.iter().cloned());
                    combined.extend(input_files_1.iter().cloned());
                    let mut name = datum_0.name.0.clone();
                    name.extend(datum_1.name.0.iter().cloned());
                    output.push(DatumData {
                        name: DatumName(name),
                        input_files: combined,
                    })
                }
            }
            Ok(output)
        }
    }
}

/// Given a URI and a repo name, construct a local path starting with "/pfs"
/// pointing to where we should download the file.
fn uri_to_local_path(base: &BaseUri, uri: &str, repo: &str) -> Result<String> {
    // Check a precondition. This could probably be an assertion; other code
    // should ensure it is always true.
    if !uri.starts_with(base.as_str()) {
        return Err(format_err!("expected {} to be in {}", uri, base.as_str()));
    }

    // Extract just the local portion of `uri` not included in `base`.
    let base = base.as_str();
    let rel_uri = &uri[base.len()..];
    if rel_uri.is_empty() {
        Err(format_err!("{:?} ends with '/'", uri))
    } else {
        Ok(format!("/pfs/{}/{}", repo, rel_uri))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use falconeri_common::storage::BucketObject;
    use proptest::prelude::*;

    // ---- Test helpers -------------------------------------------------------

    /// Build an `Input::Atom`.
    fn atom(uri: &str, repo: &str, glob: Glob) -> Input {
        Input::Atom {
            uri: uri.to_owned(),
            repo: repo.to_owned(),
            glob,
        }
    }

    fn union(inputs: Vec<Input>) -> Input {
        Input::Union(inputs)
    }

    fn cross(inputs: Vec<Input>) -> Input {
        Input::Cross(inputs)
    }

    /// Build a `BaseUriListings` from `(base, files, dirs)` triples.
    fn base_uri_listings(pairs: &[(&str, &[&str], &[&str])]) -> Listings {
        let mut listings = Listings::default();
        for &(base, files, dirs) in pairs {
            let mut entries = files
                .iter()
                .map(|&f| BucketEntry::Object(BucketObject::from_uri_for_test(f, 0)))
                .collect::<Vec<_>>();
            entries.extend(
                dirs.iter()
                    .map(|&d| {
                        Ok(BucketEntry::Prefix(BucketPrefix::from_uri(d.to_owned())?))
                    })
                    .collect::<Result<Vec<_>>>()
                    .expect("invalid bucket prefix"),
            );
            listings.base_uri_insert(
                BaseUri::normalize(base),
                BucketListing::prefix_entries(entries)
                    .expect("invalid bucket entries"),
            );
        }
        listings
    }

    /// Build a `DatumData` from `(repo, binding)` name slots and
    /// `(uri, local_path)` file pairs.
    fn datum(name: &[(&str, Option<&str>)], files: &[(&str, &str)]) -> DatumData {
        DatumData {
            name: DatumName(
                name.iter()
                    .map(|&(repo, binding)| Slot {
                        repo: repo.to_owned(),
                        binding: binding.map(|b| b.to_owned()),
                    })
                    .collect(),
            ),
            input_files: files
                .iter()
                .map(|&(uri, local_path)| InputFileData {
                    entry: if uri.ends_with('/') {
                        BucketEntry::Prefix(
                            BucketPrefix::from_uri(uri.to_owned())
                                .expect("invalid bucket prefix"),
                        )
                    } else {
                        BucketEntry::Object(BucketObject::from_uri_for_test(uri, 0))
                    },
                    local_path: local_path.to_owned(),
                })
                .collect(),
        }
    }

    /// Sort a datum sequence, to compare "up to permutation" (datum order
    /// is an implementation detail).
    fn canonical(datums: Vec<DatumData>) -> Vec<DatumData> {
        let mut v = datums;
        v.sort();
        v
    }

    // ---- Pinned current behavior (unit tests) --------------------------------
    //
    // These pin the behavior of the _current_ algebra, which (in the case of
    // `"/*"`) is not the target semantics of `plans/INPUT_ALGEBRA_EXTENSIONS.md`
    // §2. See the comment on each test, and on [`atom_to_datums_pure`].

    /// `"/"` (WholeRepo) produces exactly one datum, named `(repo, no
    /// binding)`, with exactly one file: the repo itself, as a directory
    /// (trailing slash on both `uri` and `local_path`). The listing is
    /// irrelevant to the row (but the I/O phase still fetches it, to verify
    /// listability).
    #[test]
    fn whole_repo_row_shape() {
        let input = atom("gs://b/data/", "r", Glob::WholeRepo);
        let map = base_uri_listings(&[("gs://b/data/", &["gs://b/data/a.txt"], &[])]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("r", None)], &[("gs://b/data/", "/pfs/r/")])]
        );
    }

    /// `"/*"` produces one datum per _top-level entry_ (file or directory),
    /// nested objects excluded (the listing is non-recursive). Compared up
    /// to permutation: the order of the datums follows the object store's
    /// listing order, which is an implementation detail.
    ///
    /// Note the trailing-slash conventions pinned here: a file URI has no
    /// trailing slash, and a directory URI keeps it, in both `uri` and
    /// `local_path`. The datum name's binding is the entry name `E`.
    #[test]
    fn star_is_per_entry() {
        let input = atom("gs://b/data/", "r", Glob::TopLevelDirectoryEntries);
        let map = base_uri_listings(&[(
            "gs://b/data/",
            &["gs://b/data/notes.txt"],
            &["gs://b/data/alpha/"],
        )]);
        assert_eq!(
            canonical(input_to_datums_pure(&input, &map).unwrap()),
            canonical(vec![
                datum(
                    &[("r", Some("alpha"))],
                    &[("gs://b/data/alpha/", "/pfs/r/alpha/")]
                ),
                datum(
                    &[("r", Some("notes.txt"))],
                    &[("gs://b/data/notes.txt", "/pfs/r/notes.txt")]
                ),
            ])
        );
    }

    /// An atom URI without a trailing `/` is normalized (to end in `/`)
    /// throughout: the pure core looks up the normalized base, and the rows
    /// use it. Before the C1 fix, such a URI listed successfully but then
    /// failed in [`uri_to_local_path`].
    #[test]
    fn atom_uri_without_trailing_slash_is_normalized() {
        let map = base_uri_listings(&[("gs://b/data/", &["gs://b/data/a.txt"], &[])]);

        let input = atom("gs://b/data", "r", Glob::TopLevelDirectoryEntries);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(
                &[("r", Some("a.txt"))],
                &[("gs://b/data/a.txt", "/pfs/r/a.txt")]
            )]
        );

        // `WholeRepo` rows use the normalized URI, as before.
        let input = atom("gs://b/data", "r", Glob::WholeRepo);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("r", None)], &[("gs://b/data/", "/pfs/r/")])]
        );
    }

    /// `Union` contains exactly the union of its children's datums, and
    /// nothing else. Compared up to permutation: datum order is an
    /// implementation detail (datum IDs are random UUIDs, and neither
    /// reservation nor display depends on insertion order).
    #[test]
    fn union_contains_exactly_the_childrens_datums() {
        let map = base_uri_listings(&[
            ("gs://b/a/", &["gs://b/a/x.txt"], &[]),
            ("gs://b/b/", &[], &[]),
        ]);
        let input = union(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::WholeRepo),
        ]);
        assert_eq!(
            canonical(input_to_datums_pure(&input, &map).unwrap()),
            canonical(vec![
                datum(
                    &[("ra", Some("x.txt"))],
                    &[("gs://b/a/x.txt", "/pfs/ra/x.txt")]
                ),
                datum(&[("rb", None)], &[("gs://b/b/", "/pfs/rb/")]),
            ])
        );
    }

    /// `Cross` builds nested loops, left to right: for each datum of the
    /// first input, for each datum of the second, one combined datum whose
    /// name slots and files are concatenated in the same order. Compared up
    /// to permutation: the order of the combined datums follows the
    /// children's listing order, which is an implementation detail.
    #[test]
    fn cross_nests_left_to_right() {
        let map = base_uri_listings(&[
            ("gs://b/a/", &["gs://b/a/1.txt", "gs://b/a/2.txt"], &[]),
            ("gs://b/b/", &["gs://b/b/1.txt", "gs://b/b/2.txt"], &[]),
        ]);
        let input = cross(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::TopLevelDirectoryEntries),
        ]);
        assert_eq!(
            canonical(input_to_datums_pure(&input, &map).unwrap()),
            canonical(vec![
                datum(
                    &[("ra", Some("1.txt")), ("rb", Some("1.txt"))],
                    &[
                        ("gs://b/a/1.txt", "/pfs/ra/1.txt"),
                        ("gs://b/b/1.txt", "/pfs/rb/1.txt"),
                    ]
                ),
                datum(
                    &[("ra", Some("1.txt")), ("rb", Some("2.txt"))],
                    &[
                        ("gs://b/a/1.txt", "/pfs/ra/1.txt"),
                        ("gs://b/b/2.txt", "/pfs/rb/2.txt"),
                    ]
                ),
                datum(
                    &[("ra", Some("2.txt")), ("rb", Some("1.txt"))],
                    &[
                        ("gs://b/a/2.txt", "/pfs/ra/2.txt"),
                        ("gs://b/b/1.txt", "/pfs/rb/1.txt"),
                    ]
                ),
                datum(
                    &[("ra", Some("2.txt")), ("rb", Some("2.txt"))],
                    &[
                        ("gs://b/a/2.txt", "/pfs/ra/2.txt"),
                        ("gs://b/b/2.txt", "/pfs/rb/2.txt"),
                    ]
                ),
            ])
        );
    }

    /// `Cross([])` produces zero datums. The empty product "should" be one
    /// datum (with no files); this pins the existing quirk. (Plan §5.1(5):
    /// left as-is.)
    #[test]
    fn cross_of_zero_inputs_is_zero_datums() {
        let datums =
            input_to_datums_pure(&cross(vec![]), &Listings::default()).unwrap();
        assert!(datums.is_empty());
    }

    /// `Cross` of a single input is that input.
    #[test]
    fn cross_of_one_input_is_identity() {
        let map = base_uri_listings(&[("gs://b/a/", &["gs://b/a/x.txt"], &[])]);
        let input = atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries);
        assert_eq!(
            input_to_datums_pure(&cross(vec![input.clone()]), &map).unwrap(),
            input_to_datums_pure(&input, &map).unwrap()
        );
    }

    /// Given a URI and a repo name, construct a local path starting with
    /// "/pfs" pointing to where we should download the file.
    #[test]
    fn uri_to_local_path_works() {
        let base = BaseUri::normalize("gs://bucket/path/");
        let path =
            uri_to_local_path(&base, "gs://bucket/path/data1.csv", "myrepo").unwrap();
        assert_eq!(path, "/pfs/myrepo/data1.csv");

        // Directories use this convention for now?
        let dpath =
            uri_to_local_path(&base, "gs://bucket/path/data1/", "myrepo").unwrap();
        assert_eq!(dpath, "/pfs/myrepo/data1/");
    }

    // ---- Proptest harness -----------------------------------------------------
    //
    // The generators are deliberately small: large enough to hit the
    // interesting cases (repo-name collisions, directory markers, slash-less
    // atom URIs), small enough to avoid combinatorial explosion that would
    // dilute the tests with boring cases.

    /// Repo names for atom `repo` fields. A deliberately tiny alphabet, so
    /// that distinct atoms frequently declare the same repo name.
    const REPO_NAMES: &[&str] = &["a", "b"];

    /// Base URIs for atom `uri` fields. The listing map always contains both
    /// base URIs (with possibly-empty listings), so an atom may reference either.
    const BASES: &[&str] = &["gs://b/ra/", "gs://b/rb/"];

    /// The two glob forms.
    const GLOBS: &[Glob] = &[Glob::WholeRepo, Glob::TopLevelDirectoryEntries];

    /// Candidate top-level file names (base-relative).
    const REL_FILES: &[&str] = &["f1", "f2"];

    /// Candidate top-level directory names (base-relative).
    const REL_DIRS: &[&str] = &["d1"];

    /// A synthetic non-recursive listing of `base`: a random subset of the
    /// candidate files and directories.
    fn gen_listing(base: &'static str) -> impl Strategy<Value = BucketListing> {
        let files: Vec<String> =
            REL_FILES.iter().map(|p| format!("{base}{p}")).collect();
        let dirs: Vec<String> =
            REL_DIRS.iter().map(|p| format!("{base}{p}/")).collect();
        (
            prop::sample::subsequence(files, 0..=REL_FILES.len()),
            prop::sample::subsequence(dirs, 0..=REL_DIRS.len()),
        )
            .prop_map(move |(files, dirs)| {
                let mut entries = files
                    .iter()
                    .map(|p| {
                        BucketEntry::Object(BucketObject::from_uri_for_test(p, 0))
                    })
                    .collect::<Vec<_>>();
                entries.extend(dirs.iter().map(|p| {
                    BucketEntry::Prefix(
                        BucketPrefix::from_uri(p.to_owned())
                            .expect("invalid bucket prefix"),
                    )
                }));
                BucketListing::prefix_entries(entries).expect("invalid bucket entries")
            })
    }

    /// A synthetic listing map: both base URIs, each with possibly-empty
    /// listings.
    fn gen_listing_map() -> impl Strategy<Value = Listings> {
        (gen_listing("gs://b/ra/"), gen_listing("gs://b/rb/")).prop_map(|(ra, rb)| {
            let mut listings = Listings::default();
            listings.base_uri_insert(BaseUri::normalize("gs://b/ra/"), ra);
            listings.base_uri_insert(BaseUri::normalize("gs://b/rb/"), rb);
            listings
        })
    }

    /// A random atom, referencing one of the listed base URIs. Roughly half the
    /// time the URI is given without its trailing slash, to exercise
    /// normalization.
    fn gen_atom() -> impl Strategy<Value = Input> {
        (
            prop::sample::select(BASES),
            prop::sample::select(REPO_NAMES),
            prop::sample::select(GLOBS),
            any::<bool>(),
        )
            .prop_map(|(uri, repo, glob, slashless)| {
                let uri = if slashless {
                    &uri[..uri.len() - 1]
                } else {
                    uri
                };
                atom(uri, repo, glob)
            })
    }

    /// A random atom/cross/union tree, with nesting bounded by `depth`: at
    /// depth 0 only atoms are generated. Trees therefore contain at most a
    /// handful of atoms, keeping the datum counts small.
    fn gen_input(depth: usize) -> BoxedStrategy<Input> {
        if depth == 0 {
            return gen_atom().boxed();
        }
        prop_oneof![
            3 => gen_atom().boxed(),
            2 => prop::collection::vec(gen_input(depth - 1), 1..=2)
                .prop_map(union)
                .boxed(),
            2 => prop::collection::vec(gen_input(depth - 1), 1..=2)
                .prop_map(cross)
                .boxed(),
        ]
        .boxed()
    }

    /// Run the pure core. The generators cannot produce clashing local paths
    /// (no candidate name is both a file and a directory), so this cannot
    /// fail.
    fn expand_ok(input: &Input, listings: &Listings) -> Vec<DatumData> {
        input_to_datums_pure(input, listings)
            .expect("the generators cannot produce clashing local paths")
    }

    proptest! {
        /// Determinism: `input_to_datums_pure` is a pure function of its inputs.
        #[test]
        fn determinism(
            input in gen_input(2),
            listings in gen_listing_map(),
        ) {
            prop_assert_eq!(
                expand_ok(&input, &listings),
                expand_ok(&input, &listings)
            );
        }

        /// P5: Union is commutative (up to permutation).
        #[test]
        fn union_commutes(
            a in gen_input(1),
            b in gen_input(1),
            listings in gen_listing_map(),
        ) {
            let ab = union(vec![a.clone(), b.clone()]);
            let ba = union(vec![b, a]);
            prop_assert_eq!(
                canonical(expand_ok(&ab, &listings)),
                canonical(expand_ok(&ba, &listings))
            );
        }

        /// P5: Union is associative (up to permutation).
        #[test]
        fn union_associates(
            a in gen_input(1),
            b in gen_input(1),
            c in gen_input(1),
            listings in gen_listing_map(),
        ) {
            let ab_c = union(vec![union(vec![a.clone(), b.clone()]), c.clone()]);
            let a_bc = union(vec![a, union(vec![b, c])]);
            prop_assert_eq!(
                canonical(expand_ok(&ab_c, &listings)),
                canonical(expand_ok(&a_bc, &listings))
            );
        }

        /// P4: Cross distributes over Union (up to permutation).
        #[test]
        fn cross_distributes_over_union(
            a in gen_input(1),
            b in gen_input(1),
            c in gen_input(1),
            listings in gen_listing_map(),
        ) {
            let lhs = cross(vec![a.clone(), union(vec![b.clone(), c.clone()])]);
            let rhs = union(vec![
                cross(vec![a.clone(), b]),
                cross(vec![a, c]),
            ]);
            prop_assert_eq!(
                canonical(expand_ok(&lhs, &listings)),
                canonical(expand_ok(&rhs, &listings))
            );
        }
    }
}
