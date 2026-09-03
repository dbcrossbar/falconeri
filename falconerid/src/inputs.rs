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

use std::collections::{BTreeMap, BTreeSet};

use falconeri_common::{
    models::{NewDatum, NewInputFile},
    pipeline::{Glob, Input},
    prelude::*,
    secret::Secret,
    storage::CloudStorage,
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
/// the objects listed under it.
#[derive(Clone, Debug, Default)]
struct BaseUriListings(BTreeMap<BaseUri, Vec<String>>);

impl BaseUriListings {
    fn new() -> Self {
        Self::default()
    }

    /// Record the listing fetched for `base`.
    fn insert(&mut self, base: BaseUri, objects: Vec<String>) {
        self.0.insert(base, objects);
    }

    /// Look up the listing for `base`.
    fn get(&self, base: &BaseUri) -> Option<&Vec<String>> {
        self.0.get(base)
    }
}

/// (Local helper type.) This is essentially just a `NewDatum` and a
/// `Vec<NewInputFile>`, but in a more convenient format that works better with
/// the algorithm in this file, so we don't need to carry around UUIDs
/// everywhere.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DatumData {
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

/// (I/O phase.) Fetch one listing per atom base URI in `input`.
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
        listings.insert(base.clone(), storage.list(base.as_str()).await?);
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
/// `listings` maps each atom base URI to the objects listed under it. The I/O
/// phase ([`fetch_listings`]) is responsible for fetching a listing for
/// _every_ atom base URI in `input`.
///
/// This is a pure, deterministic function of its two inputs.
fn input_to_datums_pure(
    input: &Input,
    listings: &BaseUriListings,
) -> Result<Vec<DatumData>> {
    match input {
        Input::Atom { uri, repo, glob } => {
            let base = BaseUri::normalize(uri);
            // The I/O phase fetched a listing for every atom base, so this
            // can only fail if `input_to_datums_pure` was called with an
            // incomplete map (a programmer error).
            let file_uris = listings.get(&base).expect(
                "no listing for atom base; the I/O phase must list every atom base",
            );
            atom_to_datums_pure(&base, repo, *glob, file_uris)
        }
        Input::Union(inputs) => {
            // Merge all our inputs. We could do this cleverly using `flat_map`
            // and `collect` to manage the errors, but it's clearer with a `for`
            // loop.
            let mut datums = vec![];
            for child in inputs {
                datums.extend(input_to_datums_pure(child, listings)?);
            }
            Ok(datums)
        }
        Input::Cross(inputs) => cross_to_datums_pure(inputs, listings),
    }
}

/// Interpret a single `Input::Atom` into a list of datums, given the listing
/// of its base URI.
fn atom_to_datums_pure(
    base: &BaseUri,
    repo: &str,
    glob: Glob,
    file_uris: &[String],
) -> Result<Vec<DatumData>> {
    match glob {
        // Our input file is just the entire repo, as a directory.
        Glob::WholeRepo => Ok(vec![DatumData {
            input_files: vec![InputFileData {
                uri: base.as_str().to_owned(),
                local_path: format!("/pfs/{}/", repo),
            }],
        }]),

        // One datum per object in the listing, in listing order.
        //
        // KNOWN DEVIATION (pinned by tests below): this is one datum per
        // _object_, not per _top-level entry_. That is what the current
        // `object_store`-era storage listing produces. See
        // `plans/INPUT_ALGEBRA_EXTENSIONS.md`, Appendix A: GCS was
        // per-top-level-entry from 2019-01 (gsutil `ls` without `-r`) through
        // the 2026-01 migration to `object_store`, which made GCS per-object
        // to match S3's recursive listing. Chunk C2 restores the
        // per-top-level-entry semantics.
        Glob::TopLevelDirectoryEntries => {
            let mut datums = vec![];
            for file_uri in file_uris {
                let local_path = uri_to_local_path(base, file_uri, repo)?;
                datums.push(DatumData {
                    input_files: vec![InputFileData {
                        uri: file_uri.clone(),
                        local_path,
                    }],
                });
            }
            Ok(datums)
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
            // local `datums_1`.
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
                    output.push(DatumData {
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

    /// Build a `BaseUriListings` from `(base, objects)` pairs.
    fn listing_map(pairs: &[(&str, &[&str])]) -> BaseUriListings {
        let mut listings = BaseUriListings::new();
        for (base, objects) in pairs {
            listings.insert(
                BaseUri::normalize(base),
                objects.iter().map(|o| o.to_string()).collect(),
            );
        }
        listings
    }

    /// Build a `DatumData` from `(uri, local_path)` pairs.
    fn datum(files: &[(&str, &str)]) -> DatumData {
        DatumData {
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

    /// `"/"` (WholeRepo) produces exactly one datum with exactly one file: the
    /// repo itself, as a directory (trailing slash on both `uri` and
    /// `local_path`). The listing is irrelevant to the row (but the I/O phase
    /// still fetches it, to verify listability).
    #[test]
    fn whole_repo_row_shape() {
        let input = atom("gs://b/data/", "r", Glob::WholeRepo);
        let map = listing_map(&[("gs://b/data/", &["gs://b/data/a.txt"])]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("gs://b/data/", "/pfs/r/")])]
        );
    }

    /// `"/*"` produces one datum per _object_ in the listing, in listing
    /// order.
    ///
    /// KNOWN DEVIATION from the plan's §2 semantics (one datum per top-level
    /// _entry_). This is the `object_store`-era behavior; see Appendix A of
    /// `plans/INPUT_ALGEBRA_EXTENSIONS.md` for the history. Chunk C2 restores
    /// the per-entry semantics.
    ///
    /// Note the trailing-slash conventions pinned here: a file URI has no
    /// trailing slash, and a directory (marker) URI keeps it, in both `uri`
    /// and `local_path`.
    #[test]
    fn star_is_per_object_known_deviation() {
        let input = atom("gs://b/data/", "r", Glob::TopLevelDirectoryEntries);
        let map = listing_map(&[(
            "gs://b/data/",
            &[
                "gs://b/data/alpha/",
                "gs://b/data/alpha/main.txt",
                "gs://b/data/notes.txt",
            ],
        )]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
                datum(&[("gs://b/data/alpha/", "/pfs/r/alpha/")]),
                datum(&[("gs://b/data/alpha/main.txt", "/pfs/r/alpha/main.txt")]),
                datum(&[("gs://b/data/notes.txt", "/pfs/r/notes.txt")]),
            ]
        );
    }

    /// A listing that contains the base marker object itself (a 0-byte
    /// `base/`) makes the whole conversion fail, because
    /// [`uri_to_local_path`] rejects a URI with no base-relative portion.
    /// (C2 handles marker objects explicitly, in `entries_from_listing`.)
    #[test]
    fn star_base_marker_errors() {
        let input = atom("gs://b/data/", "r", Glob::TopLevelDirectoryEntries);
        let map =
            listing_map(&[("gs://b/data/", &["gs://b/data/", "gs://b/data/a.txt"])]);
        assert!(input_to_datums_pure(&input, &map).is_err());
    }

    /// An atom URI without a trailing `/` is normalized (to end in `/`)
    /// throughout: the pure core looks up the normalized base, and the rows
    /// use it. Before the C1 fix, such a URI listed successfully but then
    /// failed in [`uri_to_local_path`].
    #[test]
    fn atom_uri_without_trailing_slash_is_normalized() {
        let map = listing_map(&[("gs://b/data/", &["gs://b/data/a.txt"])]);

        let input = atom("gs://b/data", "r", Glob::TopLevelDirectoryEntries);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("gs://b/data/a.txt", "/pfs/r/a.txt")])]
        );

        // `WholeRepo` rows use the normalized URI, as before.
        let input = atom("gs://b/data", "r", Glob::WholeRepo);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(&[("gs://b/data/", "/pfs/r/")])]
        );
    }

    /// `Union` concatenates its children's datums, in child order.
    #[test]
    fn union_concatenates_in_child_order() {
        let map = listing_map(&[
            ("gs://b/a/", &["gs://b/a/x.txt"]),
            ("gs://b/b/", &["gs://b/b/y.txt"]),
        ]);
        let input = union(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::WholeRepo),
        ]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
                datum(&[("gs://b/a/x.txt", "/pfs/ra/x.txt")]),
                datum(&[("gs://b/b/", "/pfs/rb/")]),
            ]
        );
    }

    /// `Cross` builds nested loops, left to right: for each datum of the
    /// first input, for each datum of the second, one combined datum whose
    /// files are concatenated in the same order.
    #[test]
    fn cross_nests_left_to_right() {
        let map = listing_map(&[
            ("gs://b/a/", &["gs://b/a/1.txt", "gs://b/a/2.txt"]),
            ("gs://b/b/", &["gs://b/b/1.txt", "gs://b/b/2.txt"]),
        ]);
        let input = cross(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::TopLevelDirectoryEntries),
        ]);
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![
                datum(&[
                    ("gs://b/a/1.txt", "/pfs/ra/1.txt"),
                    ("gs://b/b/1.txt", "/pfs/rb/1.txt"),
                ]),
                datum(&[
                    ("gs://b/a/1.txt", "/pfs/ra/1.txt"),
                    ("gs://b/b/2.txt", "/pfs/rb/2.txt"),
                ]),
                datum(&[
                    ("gs://b/a/2.txt", "/pfs/ra/2.txt"),
                    ("gs://b/b/1.txt", "/pfs/rb/1.txt"),
                ]),
                datum(&[
                    ("gs://b/a/2.txt", "/pfs/ra/2.txt"),
                    ("gs://b/b/2.txt", "/pfs/rb/2.txt"),
                ]),
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
        let map = listing_map(&[("gs://b/a/", &["gs://b/a/x.txt"])]);
        let input = atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries);
        assert_eq!(
            input_to_datums_pure(&cross(vec![input.clone()]), &map).unwrap(),
            input_to_datums_pure(&input, &map).unwrap()
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
    // interesting cases (repo-name collisions, nested trees, slash-less atom
    // URIs, nested objects), small enough to avoid combinatorial explosion
    // that would dilute the tests with boring cases.

    /// Repo names for atom `repo` fields. A deliberately tiny alphabet, so
    /// that distinct atoms frequently declare the same repo name.
    const REPO_NAMES: &[&str] = &["a", "b"];

    /// Base URIs for atom `uri` fields. The listing map always contains both
    /// bases (with possibly-empty listings), so an atom may reference either.
    const BASES: &[&str] = &["gs://b/ra/", "gs://b/rb/"];

    /// The two glob forms.
    const GLOBS: &[Glob] = &[Glob::WholeRepo, Glob::TopLevelDirectoryEntries];

    /// Candidate base-relative object paths: files, a directory marker, and
    /// nested files (the current listing is recursive, so nested objects
    /// appear in it).
    const REL_PATHS: &[&str] = &["f1", "f2", "d1/", "d1/f1", "d1/f2"];

    /// A synthetic listing map: both bases, each with a small subset (0–4 of
    /// the 5 candidates) of the candidate objects, in candidate order.
    fn gen_listing_map() -> impl Strategy<Value = BaseUriListings> {
        let ra_paths: Vec<String> = REL_PATHS
            .iter()
            .map(|p| format!("gs://b/ra/{}", p))
            .collect();
        let rb_paths: Vec<String> = REL_PATHS
            .iter()
            .map(|p| format!("gs://b/rb/{}", p))
            .collect();
        (
            prop::sample::subsequence(ra_paths, 0..=4),
            prop::sample::subsequence(rb_paths, 0..=4),
        )
            .prop_map(|(ra, rb)| {
                let mut listings = BaseUriListings::new();
                listings.insert(BaseUri::normalize("gs://b/ra/"), ra);
                listings.insert(BaseUri::normalize("gs://b/rb/"), rb);
                listings
            })
    }

    /// A random atom, referencing one of the listed bases. Roughly half the
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

    /// Run the pure core, stringifying any error so results are comparable.
    fn expand_str(
        input: &Input,
        listings: &BaseUriListings,
    ) -> Result<Vec<DatumData>, String> {
        input_to_datums_pure(input, listings).map_err(|e| e.to_string())
    }

    /// Normalize an expansion for "up to permutation" comparison. The
    /// generators never produce base marker objects, so `input_to_datums_pure` should not
    /// error; if it does, the error string is compared and the test fails.
    fn canon(
        result: Result<Vec<DatumData>, String>,
    ) -> Result<Vec<DatumData>, String> {
        result.map(canonical)
    }

    proptest! {
        /// Determinism: `input_to_datums_pure` is a pure function of its inputs.
        #[test]
        fn determinism(
            input in gen_input(2),
            listings in gen_listing_map(),
        ) {
            prop_assert_eq!(expand_str(&input, &listings), expand_str(&input, &listings));
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
                canon(expand_str(&ab, &listings)),
                canon(expand_str(&ba, &listings))
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
                canon(expand_str(&ab_c, &listings)),
                canon(expand_str(&a_bc, &listings))
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
                canon(expand_str(&lhs, &listings)),
                canon(expand_str(&rhs, &listings))
            );
        }
    }
}
