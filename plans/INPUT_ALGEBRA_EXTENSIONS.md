# Input Algebra Extensions: `/*/path` globs and `Group`

**Status:** Revised 2026-09 after a code-history audit (Appendix A) and the
storage-listing shape decision in C2 (`list` becomes non-recursive). The
implementation plan near the end proposes a patch series, of which this
revision is the first patch (C0). C1 landed 2026-09-03; where it deviated
from this sketch, the sketch has been amended here (see C1).

## 1. Goal

Extend the `Input` algebra (the `"input"` section of a pipeline spec) in two ways:

1. **A new glob option, `"/*/path"`**: select only *some* of the contents of each
   top-level directory entry, instead of the whole entry. The star match is
   non-recursive: one path component, which cannot contain `/`.
2. **A new combinator, `group`**: merge the datums of several inputs which share
   a datum name, so that multiple data sources can contribute to a single
   `/pfs/$REPO/$DATUM_NAME/` directory on a worker.

Both extensions must preserve the character of the existing algebra: a simple,
mathematical structure whose behavior is describable by a handful of laws —
not an ad-hoc feature list. If the laws are clean, we plan to test them with
`proptest` (a pure core with a synthetic bucket listing is the intended seam;
out of scope for this document).

## 2. Input bucket structure and desired worker layout

Input buckets are plain object-store "directories". A repository is a base URI;
its **top-level entries** are the files and directories immediately inside it.

Example bucket contents (base URI `gs://b/data/`):

```
data/
├── alpha/
│   ├── main.txt
│   ├── foo/
│   │   └── ...
│   └── bar/
│       └── ...
├── beta/
│   └── ...
└── notes.txt
```

and a second repository (base URI `gs://b/config/`):

```
config/
├── settings.json
└── ...
```

The worker sees each datum as files materialized under `/pfs`. The layout
rules, per atom `{repo: R, URI, glob}`:

| Glob | Datum name (slot) | Worker path |
|---|---|---|
| `"/"` | `(R, no binding)` — one datum for the whole repo | `/pfs/R/` |
| `"/*"` | `(R, E)` — one datum per top-level entry `E` | `/pfs/R/E` (file or directory) |
| `"/*/p"` | `(R, E)` — one datum per top-level entry `E` that contains `p` | `/pfs/R/E/p` |

`E` is a single path component (top-level entries cannot contain `/`, by
construction of the listing). A top-level *file* entry has no contents, so it
never matches `"/*/p"`. Directory URIs keep the existing trailing-slash
convention; file URIs do not.

The subpath `p` may itself match a file or a directory entry; its row follows
the same convention. (The grouping example below relies on `p` matching
directories.)

**Grouping.** `group` merges datums whose names are equal, where a name is a
tuple of slots (one per atom under crosses; see §3). Concrete example — two
sources that *declare the same repo name* but have different base URIs:

```
gs://b/data-1/alpha/foo/...     gs://b/data-2/alpha/bar/...
```

```json
"group": [
  { "atom": { "repo": "data", "URI": "gs://b/data-1/", "glob": "/*/foo" } },
  { "atom": { "repo": "data", "URI": "gs://b/data-2/", "glob": "/*/bar" } }
]
```

Each child produces a datum named `(data, alpha)`; group merges them into a
single datum whose files materialize as one directory:

```
/pfs/data/alpha/{foo, bar}
```

Note: "one `/pfs/$REPO/$DATUM_NAME/` directory" is a consequence of the
sources sharing the `repo` name. `group` itself matches on names only; children
with different repo names get different names, are not merged, and each keeps
its own `/pfs/<repo>/` tree.

## 3. The algebra

`Input` is a free algebra: atoms are the generators, and `cross`, `union`, and
(now) `group` are the operations. `input_to_datums` is the interpretation
(a homomorphism) of this algebra into sequences of **named datums**.

### 3.1 The carrier

- A **slot** is a pair `(repo, binding)`, where `binding: Option<String>` is
  the star match (`None` for whole-repo).
- A **datum name** is a tuple of slots.
- A **file** is a pair `(uri, local_path)`.
- A **datum** is a pair `(name, files)`, where `files` is a sequence of files.
- An `Input` denotes a **sequence of datums** (order = deterministic
  expansion order; not part of the semantics except via the laws below).

### 3.2 Interpretation

Let `L(base)` be the listing of top-level entries of `base`. (How this is
obtained from the storage layer is an implementation matter; see the
implementation plan, C2.)

| Construct | Denotation |
|---|---|
| `Atom(R, base, "/")` | `[( (R, ∅), [(base, /pfs/R/)] )]` |
| `Atom(R, base, "/*")` | `[( (R, E), [(u_E, /pfs/R/E)] )]` for each entry `E` of `L(base)` |
| `Atom(R, base, "/*/p")` | `[( (R, E), [(u_{E/p}, /pfs/R/E/p)] )]` for each entry `E` of `L(base)` with `E/p` existing |
| `Union([A₁ … Aₙ])` | sequence concatenation of the children |
| `Cross([A₁ … Aₙ])` | all pairings: names are slot-tuple concatenations, files are concatenations (nested loops, left to right) |
| `Group([A₁ … Aₙ])` | group-by on the concatenated children's sequence: one datum per distinct name, in first-appearance order, with files concatenated in encounter order |

Two design invariants:

- **The name determines the footprint.** A datum's files all live under the
  `/pfs/<repo>/` roots named by its slots, and two datums with equal names
  write to the same locations. `Group` merges exactly the datums whose names
  are equal.
- **The subpath is not part of the name.** `/*/foo` and `/*/bar` must match,
  and they share `(R, E)` but not the subpath. The name is *the directory the
  datum writes into*; the subpath (encoded in `local_path`) is *which file
  within it*.

### 3.3 Properties

All laws are stated about the denotations above. "Exact" means equality as
sequences; "up to permutation" means equality once the order of the datum
sequence is ignored (loop order is an implementation detail).

| # | Property | Status |
|---|---|---|
| P1 | **Group is idempotent**: `G(G(X)) = G(X)`. After the first pass, names are unique. | exact |
| P2 | **Group is bracket-invariant**: `G([A, B, C]) = G([G([A, B]), C])`. `G` depends only on the flat concatenation of its children's sequences. | exact |
| P3 | **Union is a special case of Group**: if no name appears in two different children, `G([A, B]) = U([A, B])`. | exact |
| P4 | **Cross distributes over Union**: `C(A, U(B, C)) ≃ U(C(A, B), C(A, C))`. | up to permutation |
| P5 | **Union is commutative and associative.** | up to permutation |
| P6 | **Subpath refines star**: for the same repo/URI, every name of `/*/p` appears in `/*` (with the same binding). | exact |
| P7 | **Name ⇒ footprint** (soundness): equal names write to the same `/pfs` locations, so group-merged datums are footprint-compatible by construction. | exact (structural) |

**Known non-law.** Cross does *not* distribute over Group:

```
C(A, G(B, C))  ≠  G(C(A, B), C(A, C))
```

whenever a binding of `B` equals a binding of `C`. In the right-hand side,
`A`'s files are contributed once *per cross child* that feeds the merged
datum, so they appear twice (identical `uri`/`local_path` rows). Concretely,
with `A = {repo a, "/*"}`, `B = {repo b, "/*"}`, `C = {repo c, "/*"}` and a
common entry `x`:

- LHS datum `(a,x),(b,x),(c,x)`: files of `a/x`, then `b/x`, then `c/x`.
- RHS: same datum, but `a/x`'s files appear twice — once after `b/x`, once
  after `c/x`.

This is a consequence of the intended semantics ("each child of `group`
contributes its datums wholesale"), not a defect. It holds as an equality at
the "set of files per name" level, after duplicate removal. It should be
pinned down as documented behavior in the test suite.

## 4. Type declarations

### 4.1 The algebra (signature) — `falconeri_common/src/pipeline.rs`

```rust
/// How to distribute files from an input across workers.
pub enum Glob {
    /// Put the entire repo in a single datum.                    // "/"
    WholeRepo,

    /// Put each top-level directory entry (file, subdir) in its
    /// own datum.                                                // "/*"
    TopLevelDirectoryEntries,

    /// Put the subpath `path` inside each top-level directory
    /// entry in its own datum, named for the entry. The star
    /// match is non-recursive: one path component.              // "/*/path"
    Subpath(String),
}

/// Specify our input data.
pub enum Input {
    /// Input from a cloud storage bucket.
    Atom {
        uri: String,
        /// The repo name: names the `/pfs/$repo/` directory and, together
        /// with the star binding, the datum's name.
        repo: String,
        glob: Glob,
    },

    /// Cross product of other inputs, producing every possible combination.
    Cross(Vec<Input>),

    /// Union of other inputs.
    Union(Vec<Input>),

    /// Merge the datums of our children which share a datum name (a tuple of
    /// (repo, star-binding) slots). Files are concatenated in child order;
    /// names keep first-appearance order.
    Group(Vec<Input>),
}
```

(How `Glob` round-trips through its string forms — `"/"`, `"/*"`,
`"/*/path"` — is a serde implementation detail deliberately left out of this
draft.)

### 4.2 The element type (values) — `falconerid/src/inputs.rs`

Local helper types. The algebra is operations on `Vec<DatumData>`.

```rust
/// One slot of a datum name: the repo where the atom's files land, and the
/// star binding (`None` for whole-repo).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Slot {
    repo: String,
    binding: Option<String>,
}

/// The name of a datum: the tuple of slots under crosses, in order.
///
/// Two datums with equal names write to the same `/pfs` locations, and
/// `Input::Group` merges exactly those.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DatumName(Vec<Slot>);

/// A file to download for a datum. (Existing type; unchanged.)
#[derive(Clone, Debug)]
struct InputFileData {
    uri: String,
    local_path: String,   // = /pfs/<repo>/<binding>[/<subpath>][/]
}

/// One datum: its name, and the files to download for it.
/// (Existing type; gains `name`.)
#[derive(Clone, Debug)]
struct DatumData {
    name: DatumName,
    input_files: Vec<InputFileData>,
}
```

How the operations touch these values:

| Operation | On values |
|---|---|
| `Atom "/"` | one datum, name `[(R, None)]`, one file |
| `Atom "/*"` | one datum per entry `E`, name `[(R, Some(E))]`, one file |
| `Atom "/*/p"` | one datum per matching entry `E`, name `[(R, Some(E))]`, one file at `E/p` |
| `Union` | sequence concatenation (names untouched) |
| `Cross` | pairwise; `name = name_0 ++ name_1` (slot-tuple concatenation); files concatenated |
| `Group` | group-by on `DatumName` (hence `Eq + Hash`); files concatenated in child order; first-appearance name order |

Names are bookkeeping for the algebra: they are dropped at the boundary into
`NewDatum`/`NewInputFile` (the database models are unchanged).

`InputFileData` gains *no* repo field: the repo lives in the name (once per
cross slot), and `local_path` already embeds it.

## 5. Consequences and open questions

### Consequences (expected, but worth noting)

1. **The failed distributivity of §3.3** is the one genuinely surprising
   equation: `G(C(A,B), C(A,C))` duplicates `A`'s files. Accepted as
   documented semantics; pin with a test.
2. **Cross-repo `group` is a silent no-op.** `{repo a, "/*"}` and
   `{repo b, "/*"}` in one group do not merge — no error, no multi-`/pfs`-tree
   datum. Predictable, but a user who expected merging will see "nothing
   happened". Candidate for a warning or spec-level validation later.
3. **Clobber hazard.** Two children with equal names whose files resolve to
   the *same* `local_path` but different `uri`s (e.g. `"/*"` from `U1` and
   `"/*"` from `U2`, same repo name) make the worker download both to one
   place, last write wins. With `uri` and `local_path` explicit on each file
   row, "same `local_path`, different `uri`, within one datum" is a trivial
   check we can turn into a validation error. Note the interaction with
   `Group`: merged directory-prefix rows *legitimately* share a local
   directory across children — that is the point of the merge (see the §2
   example) — so the check must apply only to file paths (no trailing
   slash).
4. **Whole-repo is no longer the cross-unit of names** (it contributes
   `(R, None)`, not nothing). Cosmetic; every load-bearing law survives.
5. **Pre-existing quirk, untouched:** `Cross([])` yields zero datums, where
   the empty product "should" be one. Left as-is.

### Open questions

1. **May `p` in `"/*/p"` be multi-segment** (e.g. `"/*/a/b"`)? The algebra is
   unaffected either way; only the per-entry probing cost changes (one listing
   per top-level directory per path level). Single-segment is the simpler v1
   and matches the "non-recursive" wording; multi-segment is a strict
   generalization.
   - Yes, multisegments are allowed.
2. **Cost of `/*/p` listing.** *Resolved during planning (revised 2026-09):*
   `list` is now non-recursive (top-level entries), so `/*/p` uses a prefix
   probe per **directory** entry: list `base/E/p` with a delimiter, filter
   to exact-or-under (see C3). Each probe is ~1 page, probes are
   independent (parallelizable), and each probe's cost is bounded by *p's*
   fan-out, not E's — a directory holding a million files still costs 1–2
   pages. At falconeri's scale profile (top-level entries in the low
   thousands, files up to ~10⁶), that is ~10³ small calls, the same order
   as the gsutil-era cost structure. Recursive-listing-plus-pruning is the
   documented fallback if entry counts grow far beyond that; it is not the
   default, because a recursive `/*` would page through the entire repo
   (~1000 sequential pages at 10⁶ files) to derive just its top level.
   (An earlier draft resolved this the other way, assuming a recursive
   `list`.)
3. **Persist the datum name?** A `names` column on `datums` would make
   `job describe`/debugging much easier (names are currently dropped at the
   DB boundary). A schema change; probably later.
4. **Validation policy.** How much of §5.1(2)–(3) do we check at `job run`
   time (spec-level errors/warnings) versus leaving to the worker? Two
   refinements settled during planning: the clobber check (§5.1(3)) applies to
   file paths only, and the `group` idiom of several atoms declaring the same
   repo name over distinct base URIs should surface as a spec-time *info*
   message so it is discoverable rather than magical.
   - We will probably want to catch as much up front as we can, before we start
     spinning up worker nodes.
5. **Empty `Group` / empty `Union`.** Both should yield zero datums,
   consistently with `Cross([])`; just confirming the convention.

## References

- `falconerid/src/inputs.rs` — `input_to_datums` and the local value types
  (`DatumData`, `InputFileData`); home of the functional core.
- `falconeri_common/src/pipeline.rs` — `Input` and `Glob`: the algebra's
  signature as seen from the pipeline spec.

## Implementation plan (patch series)

One `jj` commit per chunk on a single bookmark (`feat/input-algebra`); each
chunk is independently reviewable, leaves `just check` (fmt, deny, clippy,
test) green, and includes its own tests. This revision of the plan is patch
**C0**, and any changes to what is planned will be initially made by using
`jj edit` to edit C0. (This is a bit of an experimental workflow.)

**C1 — Make the algebra testable (behavior-preserving).**

- `falconerid/src/inputs.rs`: split `input_to_datums` into (a) an async I/O
  phase that collects every atom base URI (deduped, trailing-slash-
  normalized) and fetches the listings through the existing
  `CloudStorage::list`, and (b) a pure synchronous core `expand(&Input,
  &BTreeMap<String, Vec<String>>) -> Result<Vec<DatumData>>` (base URI →
  objects). Public signature unchanged; `start_job.rs` untouched. (C2
  changes both `list` and this map's value type — see below; C1 pins what
  exists today.) **Amended 2026-09-03 (C1 landed):** the core returns a
  `Result`, not the bare `Vec` of the original sketch — the only reachable
  error is a listing containing the base marker object itself (a 0-byte
  `base/`), which `uri_to_local_path` rejects; C1 pins that error, and it
  disappears in C2 when `entries_from_listing` drops the marker.
- Add `proptest` (workspace dependency + dev-dependency of `falconerid`;
  MIT-licensed, passes `cargo deny` as configured).
- Test rig: a synthetic listing map, plus generators (small random bucket
  trees; random atom/cross/union inputs drawn over a small repo-name alphabet
  to force name collisions).
- Pin current behavior: per-object `/*` (labeled a *known deviation* from §2,
  with the Appendix A history in a comment), `WholeRepo` row shape,
  union/cross ordering, the `Cross([])` → zero-datums quirk, the
  file/directory trailing-slash conventions, and the base-marker-listing
  error.
- First proptest laws (already true of the current algebra): P4, P5, and
  determinism.
- Also fix a latent quirk: an atom URI without a trailing `/` lists
  successfully but then fails in `uri_to_local_path`; normalize `base`
  throughout.

**C2 — `"/*"` matches top-level entries (the §2 semantics).**

- Storage change: `CloudStorage::list` becomes **non-recursive** — the
  top-level entries of `uri` (files plus subdirectory prefixes) via
  `object_store`'s `list_with_delimiter` (paginated internally; GCS and S3
  symmetric). This restores the trait's 2018-documented contract ("files and
  subdirectories immediately present") and fixes the stale docstring that
  survived the `object_store` migration; the recursive behavior being
  replaced is recorded in Appendix A. No `list_recursive` or mode enum is
  added; `sync_down` (the only other listing consumer) talks to
  `object_store` directly and is untouched.
- Entry construction: a pure, unit-tested `entries_from_listing(base,
  objects, common_prefixes) -> Vec<Entry>` drops the base marker object
  (a 0-byte `base/`), applies the directory-wins marker tie-break (a 0-byte
  `base/E/` marker with contents appears both as an object and as a common
  prefix), and yields file entries and directory entries in name order,
  files and directories interleaved. A marker object with no matching
  common prefix names an empty directory and is still a directory entry.
  **Amended 2026-09-03 (C2 landed):** the entry order and the empty-
  directory rule were added to the sketch as C2 was implemented.
- Seam change: the pure-core map becomes `prefix -> Listing` entries, where
  `Listing { files, dirs }` (a named struct in `falconeri_common::storage`,
  replacing C1's `base -> objects`) is also what `CloudStorage::list` now
  returns; the I/O phase fills it straight from the non-recursive listing —
  1–2 list pages per atom at our scale, instead of listing every object in
  the repo.
- Per-entry datums: file entries as `(base/E, /pfs/R/E)` rows; directory
  entries as prefix rows `(base/E/, /pfs/R/E/)` that the worker already
  knows how to sync recursively — no worker changes. This is the historic
  2019–2025 GCS row shape (Appendix A): one `InputFile` row per top-level
  entry, so the database stores directories, not their contents.
- Introduce the §3.1/§4.2 carrier: `Slot`, `DatumName`, `DatumData.name`;
  cross concatenates names; names are dropped at the `NewDatum`/`NewInputFile`
  boundary (no schema change; `retry_job` unaffected).
- Provable no-op for flat repos, where per-file and per-entry coincide; C1's
  flat-repo tests pass unchanged, and the word-frequencies e2e is unchanged.
  **Amended 2026-09-03 (C2 landed):** with markers handled explicitly in
  `entries_from_listing`, `expand` has no reachable error path and returns
  `Vec<DatumData>` (the original sketch signature, restored).
- Guide: document `"/*"`; fix the stale "for now, `input.atom` is the only
  supported input type" line; document the use case from Appendix A; note
  that on S3, `"/*"` on nested repos changes from per-object (the 2019–2025
  aws-CLI-era behavior) to per-top-level-entry, matching GCS — flat repos
  are unaffected.

**C3 — `Glob::Subpath` (`"/*/path"`).**

- `pipeline.rs`: new variant; custom serde for the string forms (`/`, `/*`,
  `/*/p`); schema described as a pattern'd string. `Glob` gains a `String`
  payload and loses `Copy` (small mechanical ripple).
- Pure-core arm: one datum per directory entry `E` with `E/p` present
  (a file entry has no contents and never matches), decidable from
  per-entry prefix probes (open question 2): list `base/E/p` with a
  delimiter, and keep only results `== base/E/p` or under `base/E/p/`
  (the bare prefix would also match siblings like `pfoo/`). Probe results
  slot into the same `prefix -> (files, dirs)` seam. `p` matches a file or
  a directory (a marker-only `p/` counts as an empty directory); v1 is
  single-segment `p`.
- Tests: unit (file match, directory match, `E`-is-file never matches,
  missing `p` yields no datum), serde round-trip, proptest P6 and P7.
- Guide: document `"/*/path"`.

**C4 — `Input::Group`.**

- `pipeline.rs`: `Group(Vec<Input>)` variant (snake_case `"group"`,
  `no_recursion` schema).
- Pure-core arm: group-by on `DatumName` over the concatenated children,
  first-appearance order, files concatenated in child order.
- Tests: unit (two base URIs sharing a repo name merge into one
  `/pfs/R/<binding>/` directory; distinct repo names are a no-op), proptest
  P1, P2, P3, and the §3.3 non-law pinned as documented behavior.
- Design notes to land with this chunk:
  - A migration note for users coming from Pachyderm's `group` input:
    falconeri's merge key is the datum name (repo + star binding), not a
    `groupBy` pattern, and the merged datum materializes as a single
    `/pfs/<repo>/<binding>/` directory rather than per-repo `/pfs` trees.
    Merging across distinct base URIs therefore requires declaring the same
    repo name over those URIs.
  - The spec-time *info* message for that idiom (open question 4).
  - The clobber validation of §5.1(3), file paths only.
- Guide: document `group`, including the migration note.

**Deferred (after the algebra lands).**

- Spec-level validation at `job run` per §5.1(2)–(3).
- Persisting datum names (a `names` column; schema change; open question 3).
- Possibly a `groupBy`-style pattern merge as a follow-up to `group`.

## Appendix A: the `"/*"` history (resolved)

**Conclusion:** the §2 semantics — one datum per top-level entry, a matched
directory delivered whole — **was** the Google Cloud behavior from 2019-01
through the 2026-01 migration to `object_store`. The 2019-era
production job (below) ran on exactly that behavior; the question this
appendix once left open is resolved. S3 was the exception: its listing was
truly recursive, so S3 `"/*"` was one datum per object at any depth.

### How `"/*"` worked on Google Cloud (2019 → 2025)

- **One datum per listing entry.** `atom_to_datums_helper`
  (`falconeri/src/inputs.rs` from `8e4ecd7`, 2019-01-21; then
  `falconerid/src/inputs.rs` from `90d5ecd`, 2019-06-03) turned each entry of
  `storage.list(uri)` into a one-row datum — the code comment reads "Each
  top-level file or directory in `base` should be translated into a separate
  datum". The semantics of `"/*"` were entirely delegated to the listing
  tool.
- **The GCS listing was non-recursive.** GCS `list()` shelled out to
  `gsutil ls <uri>` with no `-r` (verified in `storage/gs.rs` across the
  whole window). Per the 2019 gsutil documentation, `ls` without `-r` lists
  "only the objects and names of subdirectories it contains"; subdirectories
  print with a trailing `/`. For a repo at `gs://b/data/`, the output was
  exactly the top-level entries, e.g.
  `gs://b/data/alpha/`, `gs://b/data/beta/`, `gs://b/data/notes.txt`.
  (Mechanics, in case anyone re-audits: gsutil treats the URL as an object
  name — a fast-path metadata probe that 404s on a real directory — then
  lists with `prefix=data, delimiter=/` under an **exact-match** filter that
  drops all deeper prefixes, then expands exactly one level (`data/*`),
  printing nested subdirectory names without descending. The logic is
  identical in gsutil v4.28 (2018) and v4.34 (2019), and matches gsutil's
  own `test_subdir`.)
- **Directory entries became whole-datums.** As of `3d1ebc9` (2019-01-31),
  `uri_to_local_path` required the spec URI to end in `/` (a `"/*"` job with
  a slash-less URI failed at `job run`), mapped a directory entry
  `gs://b/data/alpha/` to `/pfs/<repo>/alpha/` (trailing slash), and
  `sync_down` gained a recursive `gsutil -m rsync` branch for
  trailing-slash URIs. The worker downloads each datum's `input_files` rows
  verbatim, per row, with no regrouping — so each top-level directory was
  materialized whole, on a single worker.

### Timeline (corrected)

- **2018-07-06:** the first commits create one datum per listing entry; the
  GCS listing was already non-recursive.
- **2018-08-02** (`f8387cc`): a guard rejects any `"/*"` job whose listing
  contains a directory entry (any `/` after the base) — "we cannot handle
  directory inputs yet". Only flat repos could run. The guard itself is
  evidence that directory entries did appear in `gsutil ls` output.
- **2019-01-21** (`8e4ecd7`): the input logic is rewritten (adding `/`,
  `union`, `cross`); the guard is removed, and nested GCS repos became
  per-entry datums. (`c417918`, 2019-01-24, fixes a local-path bug the
  rewrite introduced.)
- **2019-01-31** (`3d1ebc9`): the per-entry local-path layout and recursive
  `rsync` download described above. Before this, directory entries mapped to
  the repo root (a trailing-slash basename quirk) and were downloaded with
  `gsutil cp -r`.
- **S3, same years:** `list()` = `aws s3api list-objects-v2 --prefix` with
  **no delimiter** — a truly recursive listing (verified at `39030fd`,
  2019-04) — so S3 `"/*"` was one datum per object at any depth. The
  backends were asymmetric; the 2018 guard's message ("we don't handle these
  correctly yet for S3") reflects that.
- **2026-01** (`130ecd8`, 2026-01-11): migration to the `object_store`
  crate, whose `list(prefix)` is a recursive prefix listing for **both**
  backends. From then on, `"/*"` is per-object on GCS as well — this is the
  "current behavior" C1 pins down. For GCS this was a **regression** against
  the gsutil-era behavior: `storage/gs.rs` now calls `ObjectStore::list`
  (whose docs state "List is recursive"; the pinned revision sends no
  `delimiter` to GCS), while the one-datum-per-line logic was left
  unchanged. S3's per-object behavior is unchanged.

### The 2019-era production job (not for republication)

A set of 2019-era falconeri production pipeline configurations, provided by
Faraday for reference (not for republication), contains a step whose worker
function iterates the top-level subdirectories of its input repository and
merges each subdirectory's files into a single output file. For that step to be
correct under parallel workers, every file of each top-level subdirectory must
be delivered to a single datum — i.e., it requires the §2 semantics.

That is exactly what the mechanism above provides, so the job ran as
recorded; no alternative explanation (e.g., an earlier stack) is needed.

### Consequence for this plan

- C2's §2 semantics is a **restoration** of the long-standing GCS behavior
  (and a unification of S3 onto it), implemented natively via a
  non-recursive `list_with_delimiter` listing plus a pure entry-
  construction step, instead of parsing `gsutil` stdout. It remains a
  provable no-op for flat repositories on both backends.
- C1's "known deviation" label on the current per-object `"/*"` refers to
  the `object_store` era only; the 2019–2025 GCS behavior already was §2.
- A prior audit claimed `gsutil ls <uri>` was "a recursive prefix listing".
  It was not (no `-r` was ever passed); that misreading created the apparent
  paradox. Recorded here so the gsutil semantics are not re-misread.
