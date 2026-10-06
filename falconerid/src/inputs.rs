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

use std::collections::{BTreeMap, HashMap};

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
    /// Matches for `/*/{subpath}`, for expanding [`Glob::Subpath`].
    subpath_matches: BTreeMap<(BaseUri, String), Vec<BucketEntry>>,
}

impl Listings {
    /// I/O phase: Fetch listings from the cloud.
    #[instrument(skip_all, level = "trace")]
    async fn fetch(
        resolver: &mut dyn CloudStorageForUri,
        input: &Input,
    ) -> Result<Listings> {
        debug!("fetching atom listings");
        let mut listings = Listings::default();
        listings.fetch_helper(resolver, input).await?;
        Ok(listings)
    }

    /// Internal fetch helper.
    #[allow(clippy::double_must_use)]
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
                            self.base_uri_insert(base.clone(), listing);
                        }
                    }
                    // Subpaths like "/*/$SUBPATH" are a little tricker.
                    Glob::Subpath(subpath) => {
                        let matches = storage
                            .list_subpath_entries(base.as_str(), subpath)
                            .await?;
                        self.subpath_matches_insert(base.clone(), subpath, matches);
                    }
                    // Nothing to fetch, since we'll just use the whole thing.
                    Glob::WholeRepo => {
                        // Just check to make sure this bucket _exists_, so we
                        // can provide errors earlier.
                        let _ = storage.list_nonrecursive(base.as_str()).await?;
                    }
                }
            }
            // Combining constructs add no new atoms, but recurse through their
            // children.
            Input::Cross(inputs) | Input::Union(inputs) | Input::Group(inputs) => {
                for input in inputs {
                    // Call recursively. We need `boxed_local` so that the impl
                    // Future type created by this function isn't an infinitely
                    // recursive type.
                    self.fetch_helper(resolver, input).await?;
                }
            }
        }
        Ok(())
    }

    /// Insert a listing for `base`, replacing any existing one.
    fn base_uri_insert(&mut self, base: BaseUri, listing: BucketListing) {
        // Listing a normalized `BaseUri` (trailing "/") must always yield
        // directory entries, never a single object. Enforced here so every
        // reader of `base_uris` can rely on it.
        assert!(matches!(listing, BucketListing::PrefixEntries(_)));
        self.base_uris.insert(base, listing);
    }

    /// Insert probe matches for `{base}/*/{subpath}`, keyed by the _raw_
    /// subpath: the slash spelling is preserved, since the storage layer
    /// gives `"p"` and `"p/"` different matching rules (see
    /// [`glob_subpath`], which normalizes).
    fn subpath_matches_insert(
        &mut self,
        base: BaseUri,
        subpath: &str,
        matches: Vec<BucketEntry>,
    ) {
        self.subpath_matches
            .insert((base, subpath.to_owned()), matches);
    }

    /// Look up the listing for `base`.
    fn base_uri_get(&self, base: &BaseUri) -> Option<&BucketListing> {
        self.base_uris.get(base)
    }

    /// Look up the matches for `{base}/*/{subpath}`.
    fn subpath_matches_get(
        &self,
        base: &BaseUri,
        subpath: &str,
    ) -> Option<&Vec<BucketEntry>> {
        self.subpath_matches
            .get(&(base.clone(), subpath.to_string()))
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
    let mut resolver = CloudStorageResolver::new(secrets.to_owned());
    let listings = Listings::fetch(&mut resolver, input).await?;

    // The pure core: interpret the input algebra over those listings.
    let datum_datas = input_to_datums_pure(input, &listings)?;

    // Check for name collisions. We did a version of this when reading
    // individual buckets, but now we need to do it with the full set
    // of paths. It's possible we want to check *more* edge cases than
    // we do here.
    check_datum_collisions(&datum_datas)?;

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

/// Check each datum's input files for bucket entry collisions and local-path
/// clobbers.
///
/// We check each datum separately, because datums get their own `/pfs`
/// filesystems in the worker, and because `cross` normally introduces
/// duplicate entries _across_ datums: the same file can legitimately appear
/// in many datums. Checking the flattened set of all entries across all
/// datums would reject any `cross` with a multi-datum operand.
///
/// The second pass is the clobber check (`plans/INPUT_ALGEBRA_EXTENSIONS.md`
/// §5.1(3)): within one datum, no
/// two _file_ rows (`local_path` without a trailing slash) may share a
/// `local_path` while pointing at different `uri`s, because the worker would
/// download both to one place and leave last-write-wins. It is general, not
/// `group`-scoped: the same-repo `cross` that clobbers this way is deeply
/// dubious, and failing is intended. Written as "same `local_path` ⇒ same
/// `uri`" so it does not depend on the duplicate-entry check running first,
/// even though identical rows are already rejected above.
///
/// Two legal cases worth naming:
///
/// - Two _directory_ rows sharing a local directory are legal — merging two
///   directory trees into one local directory is the whole point of `group`.
///   Documented blind spot: two files of the same name _inside_ two merged
///   directory rows still clobber silently, exactly as with whole-repo rows.
///   Detecting that would require recursive listings, which we avoid.
///
/// - String comparison is exact for file rows: `uri` and `local_path` are
///   both consistently encoded, and file rows keep their encoded form all
///   the way to the worker's disk.
fn check_datum_collisions(datums: &[DatumData]) -> Result<()> {
    for d in datums {
        let entries: Vec<_> = d.input_files.iter().map(|f| f.entry.clone()).collect();
        check_for_bucket_entry_collisions(&entries)
            .with_context(|| format!("collision in datum {:?}", d.name))?;

        // Clobber check: file rows must agree on `uri` per `local_path`.
        let mut uri_by_local: HashMap<&str, &str> = HashMap::new();
        for f in &d.input_files {
            if f.local_path.ends_with('/') {
                continue; // directory row: sharing a directory is legal
            }
            if let Some(prev) =
                uri_by_local.insert(f.local_path.as_str(), f.entry.uri())
                && prev != f.entry.uri()
            {
                return Err(format_err!(
                    "clobber in datum {:?}: {} and {} both download to {}",
                    d.name,
                    prev,
                    f.entry.uri(),
                    f.local_path,
                ));
            }
        }
    }
    Ok(())
}

/// (Pure core.) Interpret an `Input` into a sequence of [`DatumData`], given
/// pre-fetched listings.
///
/// `listings` maps each atom base URI to the top-level entries listed under
/// it. The I/O phase ([`fetch_listings`]) is responsible for fetching a
/// listing for _every_ atom base URI in `input`.
///
/// This is a pure, deterministic function of its two inputs. Its remaining
/// `Err` paths are I/O↔pure contract violations only: user-facing checks
/// for filesystem clashes run outside the pure core, in
/// [`input_to_datums`] (see [`check_datum_collisions`]).
fn input_to_datums_pure(input: &Input, listings: &Listings) -> Result<Vec<DatumData>> {
    match input {
        Input::Atom { uri, repo, glob } => {
            atom_to_datums_pure(uri, repo, glob, listings)
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
        Input::Group(inputs) => group_to_datums_pure(inputs, listings),
    }
}

/// Interpret [`Input::Group`]: merge the datums of our children which share
/// a [`DatumName`].
fn group_to_datums_pure(
    inputs: &[Input],
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    // We preserve first-appearance order mostly because it's well-defined, but
    // our laws won't strictly require it. First-appearance order requires an
    // insertion-ordered result, which we build by indexing into the output
    // `Vec` by name.
    let mut datums: Vec<DatumData> = vec![];
    let mut first_by_name: HashMap<DatumName, usize> = HashMap::new();
    let mut children_datums = 0usize;

    // Iterate over the concatenated children's datums, in child order: one
    // datum per distinct name, kept in first-appearance order, with files
    // concatenated in encounter order. Datums with distinct names pass through
    // untouched, and merging only ever combines rows whose _names_ are equal,
    // which is what makes the merged `/pfs` footprint coherent
    // (plans/INPUT_ALGEBRA_EXTENSIONS.md §3.2-3.3).
    // Like the rest of the pure core, this never rejects its input: collision
    // and clobber checks run outside, in [`input_to_datums`].
    for child in inputs {
        for datum in input_to_datums_pure(child, listings)? {
            children_datums += 1;
            match first_by_name.get(&datum.name) {
                Some(&first) => datums[first].input_files.extend(datum.input_files),
                None => {
                    first_by_name.insert(datum.name.clone(), datums.len());
                    datums.push(datum);
                }
            }
        }
    }

    if datums.len() == children_datums {
        // No names matched, so this behaves exactly like `Union`. That is
        // legal (e.g., children with distinct repo names), but often worth
        // knowing about when debugging a spec.
        debug!("group merged no datums ({} children)", inputs.len());
    }

    Ok(datums)
}

/// Interpret a single `Input::Atom` into a list of datums, given the
/// (non-recursive) listing of its base URI.
fn atom_to_datums_pure(
    uri: &str,
    repo: &str,
    glob: &Glob,
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    let base = BaseUri::normalize(uri);
    match glob {
        Glob::WholeRepo => glob_whole_repo(&base, repo),
        Glob::TopLevelDirectoryEntries => {
            glob_top_level_directory_entries(&base, repo, listings)
        }
        Glob::Subpath(subpath) => glob_subpath(&base, subpath, repo, listings),
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
        .expect("no listing for atom base; the I/O phase must list every /* atom");

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

/// Handle [`Glob::Subpath`] (`/*/$SUBPATH`).
///
/// Our I/O phase has probed the bucket and given us the matches: the
/// top-level directory entries `E` where `E/subpath` exists. We make one
/// datum per match, in the order they are given.
///
/// As with `"/*"`, a match is either a file or a directory. Files map to
/// `/pfs/R/E/subpath`; directories map to `/pfs/R/E/subpath/` (trailing
/// slash), which the worker syncs recursively.
///
/// The datum's name binds the repo to `E`, and deliberately does not
/// include the subpath. That way `/*/foo` and `/*/bar` over the same base
/// produce matching names, which is what lets [`Input::Group`] merge them.
fn glob_subpath(
    base: &BaseUri,
    subpath: &str,
    repo: &str,
    listings: &Listings,
) -> Result<Vec<DatumData>> {
    // Get our entries from the I/O phase. The lookup uses the raw subpath,
    // since that is what the I/O phase stored under. At this point, we still
    // distinguish between "/*/a" (can match objects or prefixes) and "/*/a/"
    // (can only match prefixes). This should have been sorted out by our
    // implementation of Storage; we only need to remember it.
    let matches = listings.subpath_matches_get(base, subpath).expect(
        "no listing for atom base; the I/O phase must list every /*/subpath atom",
    );

    // Now, we need to normalize slash handling. Changes "subpath/" -> "subpath".
    let subpath = subpath.strip_suffix('/').unwrap_or(subpath);
    assert!(!subpath.starts_with('/'));

    // The part of each match's URI we expect after the entry name. Changes "subpath" ->
    // "/subpath".
    let subpath_suffix = format!("/{subpath}");
    let mut datums = vec![];
    for entry in matches {
        // Figure out the name of our datum by stripping the base URI and
        // any trailing slash, then the subpath, giving us just the part
        // that matches the "*" in "/*" (the top-level entry `E`).
        //
        // Start by stripping the slash from _this_ one, too. Changes
        // "s3://bucket/path/datum/subpath/" ->
        // "s3://bucket/path/datum/subpath".
        let slashless_uri = entry.uri().strip_suffix('/').unwrap_or(entry.uri());
        // Remove the leading bit. Changes "s3://bucket/path/repo/datum/subpath" ->
        // "datum/subpath".
        let relative = base.strip_from(slashless_uri)?;
        // Now strip the trailing bit. Changes "datum/subpath" -> "datum".
        let binding = relative.strip_suffix(&subpath_suffix).ok_or_else(|| {
            format_err!(
                "match {:?} for subpath {subpath:?} does not end in {subpath_suffix:?}",
                entry.uri()
            )
        })?;
        // Get the local "/pfs" version of the path. Yields
        // "/pfs/{repo}/{datum}/{subpath}", including any trailing "/".
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
    use std::collections::BTreeSet;

    use falconeri_common::storage::{BucketObject, mem::MemoryStorageResolver};
    use proptest::prelude::*;

    use super::*;

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

    fn group(inputs: Vec<Input>) -> Input {
        Input::Group(inputs)
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

    /// Canonicalize a datum sequence for "up to permutation" comparison:
    /// sorts the files _within_ each datum, then sorts the datums. Neither
    /// datum order nor file order is semantic
    /// (`plans/INPUT_ALGEBRA_EXTENSIONS.md` §3.3); a fully canonical
    /// comparison means no law can accidentally start depending on it.
    /// Deterministic output (order included) is still pinned by the
    /// `determinism` proptest.
    fn canonical(mut datums: Vec<DatumData>) -> Vec<DatumData> {
        for d in &mut datums {
            d.input_files.sort();
        }
        datums.sort();
        datums
    }

    // ---- Pinned current behavior (unit tests) --------------------------------
    //
    // These pin the row shapes and naming rules of the input algebra spec,
    // `plans/INPUT_ALGEBRA_EXTENSIONS.md` §2, which the current algebra
    // implements. See the comment on each test.

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
    /// use it. An unnormalized URI would list successfully (as a prefix)
    /// but then fail in [`uri_to_local_path`].
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
    /// datum (with no files); this pins the existing quirk, which
    /// `plans/INPUT_ALGEBRA_EXTENSIONS.md` §5.1(5) leaves as-is.
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

    /// Regression test: `cross` legitimately places the same file in many
    /// datums, and the collision check must run per-datum, not over the
    /// flattened set of all datums. (The old global check rejected any
    /// `cross` with a multi-datum operand with "duplicate bucket entries
    /// found", and also rejected object/prefix pairs living in _different_
    /// datums, which never share a filesystem.)
    #[test]
    fn cross_repeats_files_across_datums_without_collision() {
        let map = base_uri_listings(&[
            ("gs://b/a/", &["gs://b/a/1.txt", "gs://b/a/2.txt"], &[]),
            ("gs://b/b/", &["gs://b/b/1.txt"], &[]),
        ]);
        let input = cross(vec![
            atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries),
            atom("gs://b/b/", "rb", Glob::TopLevelDirectoryEntries),
        ]);
        let datums = input_to_datums_pure(&input, &map).unwrap();

        // Sanity check: the same file really does appear in multiple datums,
        // so the flattened set of all entries contains duplicates.
        assert_eq!(datums.len(), 2);
        let all_uris: Vec<_> = datums
            .iter()
            .flat_map(|d| d.input_files.iter().map(|f| f.entry.uri().to_owned()))
            .collect();
        assert_eq!(
            all_uris.iter().filter(|u| *u == "gs://b/b/1.txt").count(),
            2
        );

        // The per-datum check accepts this, because each datum gets its own
        // "/pfs" filesystem in the worker.
        check_datum_collisions(&datums).unwrap();

        // ...but the old flattened, whole-job check (the bug) would have
        // rejected it.
        let flat: Vec<_> = datums
            .iter()
            .flat_map(|d| d.input_files.iter().map(|f| f.entry.clone()))
            .collect();
        assert!(check_for_bucket_entry_collisions(&flat).is_err());
    }

    /// Two separate datums may hold an object `x` and a prefix `x/y/`: they
    /// never share a filesystem, so this is not a collision.
    #[test]
    fn shadowed_prefix_in_different_datums_is_ok() {
        let datums = vec![
            datum(&[("r", None)], &[("gs://b/x", "/pfs/r/x")]),
            datum(&[("r", None)], &[("gs://b/x/y/", "/pfs/r/x/y/")]),
        ];
        check_datum_collisions(&datums).unwrap();
    }

    /// Real collisions _within_ a single datum are still detected: an object
    /// `x` and a prefix `x/y/` cannot coexist in one datum's filesystem, and
    /// neither can two copies of the same URI.
    #[test]
    fn within_datum_collisions_are_rejected() {
        let shadow = datum(
            &[("r", None)],
            &[("gs://b/x", "/pfs/r/x"), ("gs://b/x/y/", "/pfs/r/x/y/")],
        );
        assert!(check_datum_collisions(&[shadow]).is_err());

        let dup = datum(
            &[("r", None)],
            &[("gs://b/x", "/pfs/r/x"), ("gs://b/x", "/pfs/r/x")],
        );
        assert!(check_datum_collisions(&[dup]).is_err());
    }

    // ---- Group: merging by datum name (plans/INPUT_ALGEBRA_EXTENSIONS.md §3) --

    /// The group idiom: two atoms declare the same repo name over distinct
    /// bases; `group` merges their same-named datums into one datum whose
    /// rows materialize as one merged `/pfs/<repo>/<binding>/` directory.
    #[test]
    fn group_merges_same_repo_name_over_two_bases() {
        let map = base_uri_listings(&[
            ("gs://b/1/", &[], &["gs://b/1/alpha/"]),
            ("gs://b/2/", &[], &["gs://b/2/alpha/"]),
        ]);
        let input = group(vec![
            atom("gs://b/1/", "r", Glob::TopLevelDirectoryEntries),
            atom("gs://b/2/", "r", Glob::TopLevelDirectoryEntries),
        ]);
        let datums = input_to_datums_pure(&input, &map).unwrap();
        assert_eq!(
            canonical(datums.clone()),
            canonical(vec![datum(
                &[("r", Some("alpha"))],
                &[
                    ("gs://b/1/alpha/", "/pfs/r/alpha/"),
                    ("gs://b/2/alpha/", "/pfs/r/alpha/"),
                ],
            )])
        );
        // The merged datum passes the collision check: two _directory_ rows
        // sharing a local directory is the point of the merge.
        check_datum_collisions(&datums).unwrap();
    }

    /// Distinct repo names never merge, so `group` is a no-op (it logs at
    /// `debug!`).
    #[test]
    fn group_of_distinct_repo_names_is_a_noop() {
        let map = base_uri_listings(&[
            ("gs://b/1/", &[], &["gs://b/1/alpha/"]),
            ("gs://b/2/", &[], &["gs://b/2/alpha/"]),
        ]);
        let input = group(vec![
            atom("gs://b/1/", "r1", Glob::TopLevelDirectoryEntries),
            atom("gs://b/2/", "r2", Glob::TopLevelDirectoryEntries),
        ]);
        assert_eq!(
            canonical(input_to_datums_pure(&input, &map).unwrap()),
            canonical(vec![
                datum(
                    &[("r1", Some("alpha"))],
                    &[("gs://b/1/alpha/", "/pfs/r1/alpha/")],
                ),
                datum(
                    &[("r2", Some("alpha"))],
                    &[("gs://b/2/alpha/", "/pfs/r2/alpha/")],
                ),
            ])
        );
    }

    /// `Group([])` produces zero datums, consistent with `Union([])` and
    /// the `Cross([])` quirk.
    #[test]
    fn group_of_zero_inputs_is_zero_datums() {
        let datums =
            input_to_datums_pure(&group(vec![]), &Listings::default()).unwrap();
        assert!(datums.is_empty());
    }

    /// Documented non-law (`plans/INPUT_ALGEBRA_EXTENSIONS.md` §3.3):
    /// `cross` does _not_ distribute over
    /// `group`. With `B` and `C` declaring the same repo name over two bases
    /// and a common entry `x`, the regrouped RHS contributes `A`'s row
    /// _twice_ to the merged datum. The difference is in row multiplicity,
    /// so it is visible up to permutation. At the `input_to_datums` layer,
    /// the RHS is rejected outright (duplicate entry within one datum), so
    /// such a spec never runs.
    #[test]
    fn cross_does_not_distribute_over_group() {
        let map = base_uri_listings(&[
            ("gs://b/a/", &["gs://b/a/x"], &[]),
            ("gs://b/b1/", &["gs://b/b1/x"], &[]),
            ("gs://b/b2/", &["gs://b/b2/x"], &[]),
        ]);
        let a = || atom("gs://b/a/", "ra", Glob::TopLevelDirectoryEntries);
        let b = || atom("gs://b/b1/", "r", Glob::TopLevelDirectoryEntries);
        let c = || atom("gs://b/b2/", "r", Glob::TopLevelDirectoryEntries);

        let lhs = input_to_datums_pure(&cross(vec![a(), group(vec![b(), c()])]), &map)
            .unwrap();
        let rhs = input_to_datums_pure(
            &group(vec![cross(vec![a(), b()]), cross(vec![a(), c()])]),
            &map,
        )
        .unwrap();

        assert_eq!(
            canonical(lhs.clone()),
            canonical(vec![datum(
                &[("ra", Some("x")), ("r", Some("x"))],
                &[
                    ("gs://b/a/x", "/pfs/ra/x"),
                    ("gs://b/b1/x", "/pfs/r/x"),
                    ("gs://b/b2/x", "/pfs/r/x"),
                ],
            )])
        );
        assert_eq!(
            canonical(rhs.clone()),
            canonical(vec![datum(
                &[("ra", Some("x")), ("r", Some("x"))],
                &[
                    ("gs://b/a/x", "/pfs/ra/x"),
                    ("gs://b/a/x", "/pfs/ra/x"),
                    ("gs://b/b1/x", "/pfs/r/x"),
                    ("gs://b/b2/x", "/pfs/r/x"),
                ],
            )])
        );
        // The duplicated `A` row is a duplicate-entry error downstream.
        assert!(check_datum_collisions(&rhs).is_err());
    }

    // ---- Clobber check (plans/INPUT_ALGEBRA_EXTENSIONS.md §5.1(3)) ------------

    /// Two _file_ rows in one datum, same `local_path`, different `uri`: the
    /// worker would download both to one place, last-write-wins. Rejected.
    /// (Group idiom variant: same repo name, same glob, two bases, matching
    /// top-level _file_ entries.)
    #[test]
    fn clobbered_file_rows_are_rejected() {
        let d = datum(
            &[("r", Some("alpha"))],
            &[
                ("gs://b/1/alpha", "/pfs/r/alpha"),
                ("gs://b/2/alpha", "/pfs/r/alpha"),
            ],
        );
        assert!(check_datum_collisions(&[d]).is_err());
    }

    /// A same-repo-name `cross` over two bases clobbers the same way with no
    /// `group` involved; the check is per-datum and general, and failing is
    /// intended.
    #[test]
    fn cross_of_same_repo_name_over_two_bases_is_rejected() {
        let map = base_uri_listings(&[
            ("gs://b/1/", &["gs://b/1/x"], &[]),
            ("gs://b/2/", &["gs://b/2/x"], &[]),
        ]);
        let input = cross(vec![
            atom("gs://b/1/", "r", Glob::TopLevelDirectoryEntries),
            atom("gs://b/2/", "r", Glob::TopLevelDirectoryEntries),
        ]);
        let datums = input_to_datums_pure(&input, &map).unwrap();
        // The pure core itself does not check...
        assert_eq!(datums.len(), 1);
        // ...but one datum now holds two `x` files with the same local path
        // and different URIs.
        assert!(check_datum_collisions(&datums).is_err());
    }

    /// Legal by design: two _directory_ rows sharing a local directory are
    /// exempt from the clobber check (merging trees is the point of `group`).
    #[test]
    fn merged_directory_rows_may_share_local_path() {
        let d = datum(
            &[("r", Some("alpha"))],
            &[
                ("gs://b/1/alpha/", "/pfs/r/alpha/"),
                ("gs://b/2/alpha/", "/pfs/r/alpha/"),
            ],
        );
        check_datum_collisions(&[d]).unwrap();
    }

    // ---- Glob::Subpath row shapes ---------------------------------------------

    /// Build a `Listings` with pre-probed subpath matches.
    fn subpath_listings(rows: &[(&str, &str, &[&str])]) -> Listings {
        let mut listings = Listings::default();
        for &(base, subpath, matches) in rows {
            let entries = matches
                .iter()
                .map(|&m| {
                    if m.ends_with('/') {
                        BucketEntry::Prefix(
                            BucketPrefix::from_uri(m.to_owned())
                                .expect("invalid bucket prefix"),
                        )
                    } else {
                        BucketEntry::Object(BucketObject::from_uri_for_test(m, 0))
                    }
                })
                .collect();
            listings.subpath_matches_insert(
                BaseUri::normalize(base),
                subpath,
                entries,
            );
        }
        listings
    }

    /// A _file_ match of `/*/p` yields one datum named for the top-level
    /// entry `E` (the subpath is deliberately _not_ part of the name), with
    /// one file row `/pfs/R/E/p` — no trailing slash.
    #[test]
    fn subpath_file_row_shape() {
        let map =
            subpath_listings(&[("gs://b/data/", "out", &["gs://b/data/alpha/out"])]);
        let input = atom("gs://b/data/", "r", Glob::Subpath("out".to_owned()));
        assert_eq!(
            input_to_datums_pure(&input, &map).unwrap(),
            vec![datum(
                &[("r", Some("alpha"))],
                &[("gs://b/data/alpha/out", "/pfs/r/alpha/out")],
            )]
        );
    }

    /// A _directory_ match yields `/pfs/R/E/p/` (trailing slash), which the
    /// worker syncs recursively. A slash-terminated subpath `"p/"` names the
    /// same directory and produces the same row (only prefixes can match
    /// it — the probe layer guarantees this).
    #[test]
    fn subpath_directory_row_shape() {
        let map = subpath_listings(&[
            ("gs://b/data/", "out", &["gs://b/data/alpha/out/"]),
            ("gs://b/data/", "out/", &["gs://b/data/alpha/out/"]),
        ]);
        for subpath in ["out", "out/"] {
            let input = atom("gs://b/data/", "r", Glob::Subpath(subpath.to_owned()));
            assert_eq!(
                input_to_datums_pure(&input, &map).unwrap(),
                vec![datum(
                    &[("r", Some("alpha"))],
                    &[("gs://b/data/alpha/out/", "/pfs/r/alpha/out/")],
                )],
                "subpath {subpath:?}",
            );
        }
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

    // ---- Proptest support -------------------------------------------------
    //
    // Randomized testing using `proptest`. We keep our data generators small.
    // They need to be large enough to hit all the interesting corner cases,
    // but small enough that we _find_ the interesting interactions between
    // multiple generators, and small enough that we can search quickly.
    //
    // The two-sided law tests check the numbered properties P1–P5, and the
    // known non-law, of the input algebra spec:
    // `plans/INPUT_ALGEBRA_EXTENSIONS.md` §3.3. Laws ignore datum order and
    // row order (see `canonical`); determinism is pinned separately.
    //
    // IMPORTANT: At the "pure" layer, we do not worry about duplicate names.
    // These are checked for _outside_ the pure layer. This allows our testing
    // to be much more general and simple.

    prop_compose! {
        /// Generate a single [`BucketEntry`] living inside of `uri`. This is used
        /// to help populate the contents of our various inputs.
        fn child_entry(uri: String)(entry_name in "f[12]|d[12]/(\\.keep)?") -> BucketEntry {
            let mut uri = uri.clone();
            if !uri.ends_with('/') {
                uri.push('/');
            }
            uri.push_str(&entry_name);
            if uri.ends_with('/') {
                BucketEntry::Prefix(BucketPrefix::from_uri(uri).expect("invalid bucket URI"))
            } else {
                BucketEntry::Object(BucketObject::from_uri_for_test(&uri, 0))
            }
        }
    }

    /// Generate multiple child entries for a prefix.
    fn child_entries(uri: String) -> impl Strategy<Value = Vec<BucketEntry>> {
        prop::collection::vec(child_entry(uri), 0..3)
    }

    /// Either generate multiple child entries for a prefix, or (if it doesn't end
    /// in a "/"), possibly return it itself.
    fn child_entries_or_self(uri: String) -> impl Strategy<Value = Vec<BucketEntry>> {
        if uri.ends_with('/') {
            child_entries(uri).boxed()
        } else {
            prop_oneof![
                Just(vec![BucketEntry::Object(BucketObject::from_uri_for_test(
                    &uri, 0
                ))]),
                child_entries(uri),
            ]
            .boxed()
        }
    }

    /// Strings which will match the wildcard portion of glob.
    fn glob_wildcard_value() -> impl Strategy<Value = String> {
        "w[123]"
    }

    /// Entries for a glob.
    fn glob_entries(
        base_uri: BaseUri,
        glob: Glob,
    ) -> impl Strategy<Value = Vec<BucketEntry>> {
        // This involves some moderate proptest shenanigans, almost to the point
        // of looking suspiciously like Haskell or a hand-rolled monad. If
        // you're not familiar with monads think of this more like the
        // pre-`async` days in JavaScript.
        match glob {
            Glob::TopLevelDirectoryEntries => glob_wildcard_value()
                .prop_flat_map(move |wildcard| {
                    child_entries(format!("{}{wildcard}/", base_uri.as_str()))
                })
                .boxed(),
            // We don't _actually_ need real `child_entries` here, but they
            // don't hurt except to add some noise and size to our test cases.
            // This could just be an optional `.keep`, like we do elsewhere.
            Glob::WholeRepo => child_entries(base_uri.as_str().to_owned()).boxed(),
            Glob::Subpath(subpath) => glob_wildcard_value()
                .prop_flat_map(move |wildcard| {
                    // `child_entries_or_self` handles the case where `subpath`
                    // is something like `foo.csv` by at least _allowing_ it to
                    // generate a `BucketObject`.
                    child_entries_or_self(format!(
                        "{}{wildcard}/{subpath}",
                        base_uri.as_str()
                    ))
                })
                .boxed(),
        }
    }

    /// Entries for an [`Input`].
    fn input_entries(input: Input) -> BoxedStrategy<Vec<BucketEntry>> {
        // This is even fancier than `glob_entries`.
        match input {
            // Base case.
            Input::Atom { uri, glob, .. } => {
                let base_uri = BaseUri::normalize(&uri);
                glob_entries(base_uri, glob).boxed()
            }
            // `Cross`, `Union`, and `Group` all regroup the same atoms, so
            // their entries are just the flattened children's entries.
            Input::Cross(inputs) | Input::Union(inputs) | Input::Group(inputs) => {
                // Getting tricky here. First, we build a
                // Vec<BoxedStrategy<Vec<_>>> using the usual tools.
                let entry_strategies: Vec<BoxedStrategy<Vec<BucketEntry>>> =
                    inputs.into_iter().map(input_entries).collect();
                // But a Vec<Strategy<T>> is also a Strategy<Vec<T>>, thanks
                // to one of the standard impls, so we can reinterpret it like
                // this. (This `let` is purely for documentation.)
                let strategy_nested_entries: BoxedStrategy<Vec<Vec<BucketEntry>>> =
                    entry_strategies.boxed();
                // And now we can prop_map and flatten this.
                strategy_nested_entries
                    .prop_map(|nested_entries: Vec<Vec<BucketEntry>>| {
                        nested_entries.into_iter().flatten().collect()
                    })
                    .boxed()
            }
        }
    }

    /// An input and the related entries.
    #[derive(Clone, Debug)]
    struct InputAndEntries {
        input: Input,
        entries: Vec<BucketEntry>,
    }

    /// Generate an input and its entries.
    fn input_and_entries() -> BoxedStrategy<InputAndEntries> {
        any::<Input>()
            .prop_flat_map(|input| {
                input_entries(input.clone()).prop_map(move |entries| InputAndEntries {
                    input: input.clone(),
                    entries,
                })
            })
            .boxed()
    }

    /// Helper function allowing us to make fast async calls to
    /// [`MemoryStorageRevolver`] and [`Listings::fetch`].
    ///
    /// The `MemoryStorageRevolver` code is asyn
    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime")
            .block_on(f)
    }

    /// Build a [`Listings`] object for a group of [`InputAndEntries`] objects.
    fn input_listings(inputs_and_entries: &[&InputAndEntries]) -> Listings {
        let mut inputs = vec![];
        let mut objects = BTreeSet::new();
        for input_and_entries in inputs_and_entries {
            inputs.push(input_and_entries.input.clone());
            // This should force deduplication and ordering. Duplicate names are
            // fine, I _think_, because our `Listings` invariants shouldn't care
            // what's actually in the buckets, and I _think_ shrinking will be
            // OK even if it sometimes removes only 1 out of 2 sources of a
            // BucketEntry. But if something gets weird, keep an eye on this.
            // We only keep the objects, because cloud buckets can't actually
            // store prefixes.
            objects.extend(input_and_entries.entries.iter().cloned().filter_map(
                |e| match e {
                    BucketEntry::Object(bucket_object) => Some(bucket_object),
                    BucketEntry::Prefix(_) => None,
                },
            ));
        }

        // This union could be pretty much any compound type--we're only
        // using the actual Atom values.
        let input_union = Input::Union(inputs);
        let objects = objects.into_iter().collect::<Vec<_>>();

        block_on(async {
            let mut resolver = MemoryStorageResolver::default();
            resolver
                .populate(&objects)
                .await
                .expect("error populating storage");
            Listings::fetch(&mut resolver, &input_union)
                .await
                .expect("error fetching listings")
        })
    }

    /// Run the pure core.
    fn input_to_datums_pure_checked(
        input: &Input,
        listings: &Listings,
    ) -> Vec<DatumData> {
        input_to_datums_pure(input, listings).expect(
            "our generators should not be able to produce invalid bucket contents",
        )
    }

    proptest! {
        /// Determinism: `input_to_datums_pure` is a pure function of its inputs.
        #[test]
        fn determinism(input_a in input_and_entries()) {
            let listings = input_listings(&[&input_a]);
            prop_assert_eq!(
                input_to_datums_pure_checked(&input_a.input, &listings),
                input_to_datums_pure_checked(&input_a.input, &listings)
            );
        }

        /// P1: Group is idempotent. After one merge pass, names are unique,
        /// so a second pass merges nothing.
        #[test]
        fn group_is_idempotent(input_a in input_and_entries()) {
            let listings = input_listings(&[&input_a]);
            let once = group(vec![input_a.input.clone()]);
            let twice = group(vec![once.clone()]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&twice, &listings)),
                canonical(input_to_datums_pure_checked(&once, &listings))
            );
        }

        /// P2: Group is bracket-invariant: `G` depends only on the flat
        /// concatenation of its children's datums.
        #[test]
        fn group_bracket_invariant(
            input_a in input_and_entries(),
            input_b in input_and_entries(),
            input_c in input_and_entries(),
        ) {
            let listings = input_listings(&[&input_a, &input_b, &input_c]);
            let lhs = group(vec![
                input_a.input.clone(),
                input_b.input.clone(),
                input_c.input.clone(),
            ]);
            let rhs = group(vec![
                group(vec![input_a.input, input_b.input]),
                input_c.input,
            ]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&lhs, &listings)),
                canonical(input_to_datums_pure_checked(&rhs, &listings))
            );
        }

        /// P3: Union is a special case of Group: when every datum name in
        /// the _concatenated_ children is pairwise distinct, there is
        /// nothing to merge, and `G([A, B]) = U([A, B])`.
        ///
        /// The premise is about the concatenation, not cross-child
        /// disjointness: a child may contain duplicate names itself (e.g.,
        /// the union of two whole-repo atoms with the same repo name), and
        /// `G` merges those while `U` does not. That merging of in-child
        /// duplicates is exactly what P2's bracket-invariance demands of
        /// `G`.
        ///
        /// The premise is checked at runtime against the fragments' shared
        /// denotations; if rejection ever starves case search, upgrade to
        /// disjoint-by-construction repo alphabets via a parameterized
        /// generator.
        #[test]
        fn union_is_a_special_case_of_group(
            input_a in input_and_entries(),
            input_b in input_and_entries(),
        ) {
            let listings = input_listings(&[&input_a, &input_b]);
            let names = |input: &Input| {
                input_to_datums_pure_checked(input, &listings)
                    .into_iter()
                    .map(|d| d.name)
                    .collect::<Vec<_>>()
            };
            let mut all_names = names(&input_a.input);
            all_names.extend(names(&input_b.input));
            let deduped = all_names.iter().collect::<BTreeSet<_>>();
            prop_assume!(
                deduped.len() == all_names.len(),
                "premise: every datum name in the concatenated children must be unique",
            );
            let grouped = group(vec![input_a.input.clone(), input_b.input.clone()]);
            let unioned = union(vec![input_a.input, input_b.input]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&grouped, &listings)),
                canonical(input_to_datums_pure_checked(&unioned, &listings))
            );
        }

        /// P4: Cross distributes over Union (up to permutation).
        #[test]
        fn cross_distributes_over_union(
            input_a in input_and_entries(),
            input_b in input_and_entries(),
            input_c in input_and_entries(),
        ) {
            let listings = input_listings(&[&input_a, &input_b, &input_c]);
            let lhs = cross(vec![
                input_a.input.clone(),
                union(vec![input_b.input.clone(), input_c.input.clone()]),
            ]);
            let rhs = union(vec![
                cross(vec![input_a.input.clone(), input_b.input.clone()]),
                cross(vec![input_a.input, input_c.input]),
            ]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&lhs, &listings)),
                canonical(input_to_datums_pure_checked(&rhs, &listings))
            );
        }

        /// P5: Union is commutative (up to permutation).
        #[test]
        fn union_commutes(
            input_a in input_and_entries(),
            input_b in input_and_entries(),
        ) {
            // Both sides of the law see the same `Listings`, because they
            // contain the same atoms: `input_listings` fetches against a
            // carrier union, and our laws only regroup atoms. See also
            // `determinism`.
            let listings = input_listings(&[&input_a, &input_b]);
            let ab = union(vec![input_a.input.clone(), input_b.input.clone()]);
            let ba = union(vec![input_b.input, input_a.input]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&ab, &listings)),
                canonical(input_to_datums_pure_checked(&ba, &listings))
            );
        }

        /// P5: Union is associative (up to permutation).
        #[test]
        fn union_associates(
            input_a in input_and_entries(),
            input_b in input_and_entries(),
            input_c in input_and_entries(),
        ) {
            let listings = input_listings(&[&input_a, &input_b, &input_c]);
            let ab_c = union(vec![
                union(vec![input_a.input.clone(), input_b.input.clone()]),
                input_c.input.clone(),
            ]);
            let a_bc = union(vec![
                input_a.input,
                union(vec![input_b.input, input_c.input]),
            ]);
            prop_assert_eq!(
                canonical(input_to_datums_pure_checked(&ab_c, &listings)),
                canonical(input_to_datums_pure_checked(&a_bc, &listings))
            );
        }
    }
}
