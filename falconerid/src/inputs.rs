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

use std::collections::{BTreeMap, BTreeSet};

use falconeri_common::{
    models::{NewDatum, NewInputFile},
    pipeline::{Glob, Input},
    prelude::*,
    secret::Secret,
    storage::{CloudStorage, Listing},
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
}

/// (Local helper type.) The I/O phase's output: each atom base URI mapped to
/// the top-level entries listed under it.
#[derive(Clone, Debug, Default)]
struct BaseUriListings(BTreeMap<BaseUri, Listing>);

impl BaseUriListings {
    fn new() -> Self {
        Self::default()
    }

    /// Record the listing fetched for `base`.
    fn insert(&mut self, base: BaseUri, listing: Listing) {
        self.0.insert(base, listing);
    }

    /// Look up the listing for `base`.
    fn get(&self, base: &BaseUri) -> Option<&Listing> {
        self.0.get(base)
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
    uri: String,
    local_path: String,
}

impl InputFileData {
    /// Convert this into an actual `NewInputFile`.
    fn into_new_input_file(self, job_id: Uuid, datum_id: Uuid) -> NewInputFile {
        NewInputFile {
            job_id,
            datum_id,
            uri: self.uri,
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
    let listings = fetch_listings(secrets, input).await?;

    // The pure core: interpret the input algebra over those listings.
    let datum_datas = input_to_datums_pure(input, &listings)?;

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

/// (I/O phase.) Fetch one (non-recursive) listing per atom base URI in
/// `input`.
///
/// We list even `Glob::WholeRepo` repos, because we want to verify that we can
/// actually list the contents of a `Glob::WholeRepo` _before_ spinning up a
/// big cluster job.
#[instrument(skip_all, level = "trace")]
async fn fetch_listings(secrets: &[Secret], input: &Input) -> Result<BaseUriListings> {
    let base_uris = collect_atom_base_uris(input);
    debug!(num_base_uris = %base_uris.len(), "fetching atom listings");
    let mut listings = BaseUriListings::new();
    for base in &base_uris {
        let storage = <dyn CloudStorage>::for_uri(base.as_str(), secrets).await?;
        listings.insert(
            base.clone(),
            storage.list_nonrecursive(base.as_str()).await?,
        );
    }
    Ok(listings)
}

/// Collect the base URI of every atom in `input`, deduplicated.
fn collect_atom_base_uris(input: &Input) -> BTreeSet<BaseUri> {
    let mut base_uris = BTreeSet::new();
    collect_atom_base_uris_helper(input, &mut base_uris);
    base_uris
}

fn collect_atom_base_uris_helper(input: &Input, base_uris: &mut BTreeSet<BaseUri>) {
    match input {
        Input::Atom { uri, .. } => {
            base_uris.insert(BaseUri::normalize(uri));
        }
        Input::Cross(inputs) | Input::Union(inputs) => {
            for child in inputs {
                collect_atom_base_uris_helper(child, base_uris);
            }
        }
    }
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
fn input_to_datums_pure(
    input: &Input,
    listings: &BaseUriListings,
) -> Result<Vec<DatumData>> {
    let datums = match input {
        Input::Atom { uri, repo, glob } => {
            let base = BaseUri::normalize(uri);
            // The I/O phase fetched a listing for every atom base, so this
            // can only fail if `input_to_datums_pure` was called with an
            // incomplete map (a programmer error).
            let listing = listings.get(&base).expect(
                "no listing for atom base; the I/O phase must list every atom base",
            );
            atom_to_datums_pure(&base, repo, *glob, listing)
        }
        Input::Union(inputs) => {
            // Merge all our inputs, in child order.
            let mut datums = vec![];
            for child in inputs {
                datums.extend(input_to_datums_pure(child, listings)?);
            }
            datums
        }
        Input::Cross(inputs) => cross_to_datums_pure(inputs, listings)?,
    };
    verify_local_paths(&datums)?;
    Ok(datums)
}

/// Verify that each datum's rows can coexist in a single local file system
/// tree: no row may live at or under a path that another row of the same
/// datum occupies as a file. For example, `/pfs/R/foo` as a file clashes
/// with `/pfs/R/foo/` or `/pfs/R/foo/bar`, because the worker downloads all
/// of a datum's rows into one clean `/pfs`, where a path cannot be both a
/// file and a directory.
///
/// This checks the rows themselves; it cannot see inside the recursive
/// download of a whole-repo row.
fn verify_local_paths(datums: &[DatumData]) -> Result<()> {
    for datum in datums {
        let paths: BTreeSet<&str> = datum
            .input_files
            .iter()
            .map(|f| f.local_path.as_str())
            .collect();
        for file in &paths {
            if file.ends_with('/') {
                continue;
            }
            // The rule: no other row may start with `file/`, because such a
            // row would need `file` to be a directory. Paths sharing a
            // prefix are contiguous in sorted order, so the smallest member
            // >= `file/` starts with it if and only if any member does.
            let prefix = format!("{file}/");
            if let Some(under) = paths.range(prefix.as_str()..).next() {
                if under.starts_with(prefix.as_str()) {
                    return Err(format_err!(
                        "datum rows {file} (a file) and {under} (under it) \
                         would clash in the worker's local file system"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// A top-level entry of a repository: a file or a directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    /// A file entry. `uri` does not end in `/`.
    File { uri: String },
    /// A directory entry. `uri` ends in `/`.
    Dir { uri: String },
}

impl Entry {
    fn uri(&self) -> &str {
        match self {
            Entry::File { uri } | Entry::Dir { uri } => uri,
        }
    }
}

/// Turn the raw non-recursive listing of `base` into its top-level entries.
///
/// `listing.files` are the file objects directly under `base`; the raw
/// listing may include marker objects: a 0-byte `base/` for the base
/// directory itself, and 0-byte `E/` objects for directories.
/// `listing.dirs` are the subdirectories directly under `base`, each ending
/// in `/`.
///
/// The base marker object is dropped. A directory marker object is a
/// directory entry, winning the tie-break against its counterpart in
/// `listing.dirs`; a marker object with no counterpart in `listing.dirs`
/// names an _empty_ directory, which is still a directory entry.
///
/// Entries are returned in name order, files and directories interleaved.
fn entries_from_listing(base: &BaseUri, listing: &Listing) -> Vec<Entry> {
    let mut emitted_dirs: BTreeSet<&str> = BTreeSet::new();
    let mut entries: Vec<Entry> =
        Vec::with_capacity(listing.files.len() + listing.dirs.len());
    for uri in &listing.files {
        if uri.as_str() == base.as_str() {
            // The base marker object: not an entry.
            continue;
        }
        if uri.ends_with('/') {
            // A directory marker object: a directory, possibly empty.
            if emitted_dirs.insert(uri.as_str()) {
                entries.push(Entry::Dir { uri: uri.clone() });
            }
        } else {
            entries.push(Entry::File { uri: uri.clone() });
        }
    }
    for uri in &listing.dirs {
        // Skip directories already emitted from their marker object.
        if emitted_dirs.insert(uri.as_str()) {
            entries.push(Entry::Dir { uri: uri.clone() });
        }
    }
    entries.sort_by(|entry1, entry2| entry1.uri().cmp(entry2.uri()));
    entries
}

/// Interpret a single `Input::Atom` into a list of datums, given the
/// (non-recursive) listing of its base URI.
fn atom_to_datums_pure(
    base: &BaseUri,
    repo: &str,
    glob: Glob,
    listing: &Listing,
) -> Vec<DatumData> {
    match glob {
        // Our input file is just the entire repo, as a directory.
        Glob::WholeRepo => vec![DatumData {
            name: DatumName(vec![Slot {
                repo: repo.to_owned(),
                binding: None,
            }]),
            input_files: vec![InputFileData {
                uri: base.as_str().to_owned(),
                local_path: format!("/pfs/{}/", repo),
            }],
        }],

        // One datum per top-level entry (file or directory), in name order.
        // File entries land at `/pfs/R/E`; directory entries land at
        // `/pfs/R/E/` (trailing slash), which the worker syncs recursively.
        // The datum's name binds the repo to the entry `E`.
        Glob::TopLevelDirectoryEntries => {
            let base_len = base.as_str().len();
            entries_from_listing(base, listing)
                .into_iter()
                .map(|entry| {
                    // The entry name `E` (without any trailing slash) is the
                    // datum's star binding.
                    let binding = match &entry {
                        Entry::File { uri } => uri[base_len..].to_owned(),
                        Entry::Dir { uri } => uri[base_len..uri.len() - 1].to_owned(),
                    };
                    // Listing entries are under `base` (with a non-empty
                    // base-relative portion) by construction.
                    let local_path = uri_to_local_path(base, entry.uri(), repo)
                        .expect("a listing entry should be under its base URI");
                    DatumData {
                        name: DatumName(vec![Slot {
                            repo: repo.to_owned(),
                            binding: Some(binding),
                        }]),
                        input_files: vec![InputFileData {
                            uri: entry.uri().to_owned(),
                            local_path,
                        }],
                    }
                })
                .collect()
        }
    }
}

/// Interpret a cross product into a list of datums.
///
/// SECURITY: This assumes it runs on reasonably trusted and plausible inputs.
/// You can cause a denial-of-service by calculating the cross product of
/// enormous repos, or by passing in so many repos that the stack overflows. But
/// since our input comes from a local user, this is fine for now.
fn cross_to_datums_pure(
    inputs: &[Input],
    listings: &BaseUriListings,
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
    fn listing_map(pairs: &[(&str, &[&str], &[&str])]) -> BaseUriListings {
        let mut listings = BaseUriListings::new();
        for &(base, files, dirs) in pairs {
            listings.insert(
                BaseUri::normalize(base),
                Listing {
                    files: files.iter().map(|s| s.to_string()).collect(),
                    dirs: dirs.iter().map(|s| s.to_string()).collect(),
                },
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
                    uri: uri.to_owned(),
                    local_path: local_path.to_owned(),
                })
                .collect(),
        }
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
        let map = listing_map(&[("gs://b/data/", &["gs://b/data/a.txt"], &[])]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("r", None)], &[("gs://b/data/", "/pfs/r/")])]
        );
    }

    /// `"/*"` produces one datum per _top-level entry_ (file or directory),
    /// in name order, nested objects excluded (the listing is
    /// non-recursive).
    ///
    /// The listing here exercises the marker rules end to end: the base
    /// marker object (`gs://b/data/`) is dropped, and the directory marker
    /// object (`gs://b/data/alpha/`, also a common prefix) yields a single
    /// directory entry.
    ///
    /// Note the trailing-slash conventions pinned here: a file URI has no
    /// trailing slash, and a directory URI keeps it, in both `uri` and
    /// `local_path`. The datum name's binding is the entry name `E`.
    #[test]
    fn star_is_per_entry() {
        let input = atom("gs://b/data/", "r", Glob::TopLevelDirectoryEntries);
        let map = listing_map(&[(
            "gs://b/data/",
            &[
                "gs://b/data/",
                "gs://b/data/alpha/",
                "gs://b/data/notes.txt",
            ],
            &["gs://b/data/alpha/"],
        )]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
                datum(
                    &[("r", Some("alpha"))],
                    &[("gs://b/data/alpha/", "/pfs/r/alpha/")]
                ),
                datum(
                    &[("r", Some("notes.txt"))],
                    &[("gs://b/data/notes.txt", "/pfs/r/notes.txt")]
                ),
            ]
        );
    }

    /// Marker handling in [`entries_from_listing`]: the base marker object
    /// is dropped; a directory marker object that is also in `listing.dirs`
    /// yields a single directory entry (the directory wins the tie-break);
    /// a directory marker object with _no_ counterpart in `listing.dirs`
    /// names an empty directory and is still a directory entry. The result
    /// is in name order, files and directories interleaved.
    #[test]
    fn entries_from_listing_marker_rules() {
        let base = BaseUri::normalize("gs://b/data/");
        let listing = Listing {
            files: vec![
                "gs://b/data/".to_owned(),       // base marker: dropped
                "gs://b/data/alpha/".to_owned(), // marker + dir entry: one dir
                "gs://b/data/empty/".to_owned(), // marker only: empty dir
                "gs://b/data/notes.txt".to_owned(),
            ],
            dirs: vec!["gs://b/data/alpha/".to_owned()],
        };
        assert_eq!(
            entries_from_listing(&base, &listing),
            vec![
                Entry::Dir {
                    uri: "gs://b/data/alpha/".to_owned(),
                },
                Entry::Dir {
                    uri: "gs://b/data/empty/".to_owned(),
                },
                Entry::File {
                    uri: "gs://b/data/notes.txt".to_owned(),
                },
            ]
        );
    }

    /// An atom URI without a trailing `/` is normalized (to end in `/`)
    /// throughout: the pure core looks up the normalized base, and the rows
    /// use it. Before the C1 fix, such a URI listed successfully but then
    /// failed in [`uri_to_local_path`].
    #[test]
    fn atom_uri_without_trailing_slash_is_normalized() {
        let map = listing_map(&[("gs://b/data/", &["gs://b/data/a.txt"], &[])]);

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

    /// `Union` concatenates its children's datums, in child order.
    #[test]
    fn union_concatenates_in_child_order() {
        let map = listing_map(&[
            ("gs://b/a/", &["gs://b/a/x.txt"], &[]),
            ("gs://b/b/", &[], &[]),
        ]);
        let input = union(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::WholeRepo),
        ]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
                datum(
                    &[("ra", Some("x.txt"))],
                    &[("gs://b/a/x.txt", "/pfs/ra/x.txt")]
                ),
                datum(&[("rb", None)], &[("gs://b/b/", "/pfs/rb/")]),
            ]
        );
    }

    /// `Cross` builds nested loops, left to right: for each datum of the
    /// first input, for each datum of the second, one combined datum whose
    /// name slots and files are concatenated in the same order.
    #[test]
    fn cross_nests_left_to_right() {
        let map = listing_map(&[
            ("gs://b/a/", &["gs://b/a/1.txt", "gs://b/a/2.txt"], &[]),
            ("gs://b/b/", &["gs://b/b/1.txt", "gs://b/b/2.txt"], &[]),
        ]);
        let input = cross(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::TopLevelDirectoryEntries),
        ]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
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
            ]
        );
    }

    /// `Cross([])` produces zero datums. The empty product "should" be one
    /// datum (with no files); this pins the existing quirk. (Plan §5.1(5):
    /// left as-is.)
    #[test]
    fn cross_of_zero_inputs_is_zero_datums() {
        let datums =
            input_to_datums_pure(&cross(vec![]), &BaseUriListings::new()).unwrap();
        assert!(datums.is_empty());
    }

    /// `Cross` of a single input is that input.
    #[test]
    fn cross_of_one_input_is_identity() {
        let map = listing_map(&[("gs://b/a/", &["gs://b/a/x.txt"], &[])]);
        let input = atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries);
        assert_eq!(
            input_to_datums_pure(&cross(vec![input.clone()]), &map).unwrap(),
            input_to_datums_pure(&input, &map).unwrap()
        );
    }

    /// A datum whose rows would clash in the worker's local file system is
    /// rejected. The bucket contains both a file `foo` and a file `foo/bar`,
    /// so the top-level entries include a file `foo` and a directory `foo/`,
    /// and `Cross` combines both into a single datum, where `/pfs/ra/foo`
    /// would have to be both a file and a directory.
    #[test]
    fn clashing_local_paths_are_rejected() {
        let map = listing_map(&[("gs://b/a/", &["gs://b/a/foo"], &["gs://b/a/foo/"])]);
        let input = atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries);
        assert!(
            input_to_datums_pure(&cross(vec![input.clone(), input]), &map).is_err()
        );
    }

    /// The I/O phase's base collection dedupes and normalizes: an atom URI
    /// without a trailing slash is the same base as one with it, and repeated
    /// base URIs are listed only once.
    #[test]
    fn collect_atom_base_uris_dedupes_and_normalizes() {
        let input = cross(vec![
            atom("gs://b/data", "r1", Glob::WholeRepo),
            union(vec![
                atom("gs://b/data/", "r2", Glob::TopLevelDirectoryEntries),
                atom("gs://b/other/", "r3", Glob::WholeRepo),
            ]),
        ]);
        let base_uris = collect_atom_base_uris(&input);
        assert_eq!(
            base_uris,
            ["gs://b/data/", "gs://b/other/"]
                .iter()
                .map(|uri| BaseUri::normalize(uri))
                .collect::<BTreeSet<BaseUri>>()
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

    /// A synthetic non-recursive listing of `base`: a small subset of the
    /// candidate files and directories, plus the marker objects (the base
    /// marker, and each selected directory's marker) to exercise the drop
    /// and tie-break rules.
    fn gen_listing(base: &'static str) -> impl Strategy<Value = Listing> {
        let files: Vec<String> =
            REL_FILES.iter().map(|p| format!("{base}{p}")).collect();
        let dirs: Vec<String> =
            REL_DIRS.iter().map(|p| format!("{base}{p}/")).collect();
        (
            prop::sample::subsequence(files, 0..=2),
            prop::sample::subsequence(dirs, 0..=1),
        )
            .prop_map(move |(files, dirs)| {
                let mut all_files = files;
                all_files.push(base.to_owned());
                all_files.extend(dirs.iter().cloned());
                Listing {
                    files: all_files,
                    dirs,
                }
            })
    }

    /// A synthetic listing map: both base URIs, each with possibly-empty
    /// listings.
    fn gen_listing_map() -> impl Strategy<Value = BaseUriListings> {
        (gen_listing("gs://b/ra/"), gen_listing("gs://b/rb/")).prop_map(|(ra, rb)| {
            let mut listings = BaseUriListings::new();
            listings.insert(BaseUri::normalize("gs://b/ra/"), ra);
            listings.insert(BaseUri::normalize("gs://b/rb/"), rb);
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
                .prop_map(|inputs| union(inputs))
                .boxed(),
            2 => prop::collection::vec(gen_input(depth - 1), 1..=2)
                .prop_map(|inputs| cross(inputs))
                .boxed(),
        ]
        .boxed()
    }

    /// Sort a datum sequence, to compare "up to permutation" (loop order is
    /// an implementation detail).
    fn canonical(datums: Vec<DatumData>) -> Vec<DatumData> {
        let mut v = datums;
        v.sort();
        v
    }

    /// Run the pure core. The generators cannot produce clashing local paths
    /// (no candidate name is both a file and a directory), so this cannot
    /// fail.
    fn expand_ok(input: &Input, listings: &BaseUriListings) -> Vec<DatumData> {
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
