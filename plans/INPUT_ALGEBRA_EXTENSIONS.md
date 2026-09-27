# Input Algebra Extensions: `/*/path` globs and `Group`

**Status:** Revised 2026-09 after a code-history audit (Appendix A) and the
storage-listing shape decision in C2 (`list` becomes non-recursive). The
implementation plan near the end proposes a patch series, of which this
revision is the first patch (C0). C1, C2, and C3 landed 2026-09-03; where
they deviated from this sketch, the sketch has been amended here (see the
"Amended" notes in C1, C2, and C3). **Further revised 2026-09-25:** the
storage probe API was reworked before landing (`list_one_entry` /
`list_subpath_entries` supersede the 2026-09-03 `list_prefix` sketch), and
the proptest harness landed against real storage code; the C3 notes are
superseded wherever they conflict — see the "Revised 2026-09-25" notes.
**Revised 2026-09-26 (C4 planning):** the C4 section was rewritten against
the landed code and harness. C4 has **not** landed; its earlier "Amended
(C4 landed)" notes were stale and have been folded into the C4 section or
dropped, and its still-live decisions are labeled "Decided (C4 planning)".
**Revised 2026-09-26 (laws):** all laws are now stated uniformly *up to
permutation* — neither datum order nor file order within a datum is
semantic (no downstream consumer observes either); determinism remains a
tested implementation property. See §3.3.

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
out of scope for this document). **Revised 2026-09-25:** proptests landed;
the seam changed from synthetic listings to a pure core over the real I/O
phase, run against an in-memory storage backend (see C3 tests).

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
- An `Input` denotes a **sequence of datums**. Sequence order is *not*
  part of the semantics: datum order has no downstream consumer (it is
  not persisted; reservation is unordered), and file order within a datum
  is likewise unconstrained. All laws are stated up to permutation (§3.3).
  Determinism — same input, identical output, order included — remains a
  tested property of the implementation, because reproducibility catches
  real bugs (e.g., hash-iteration order leaking into the output).

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
| `Group([A₁ … Aₙ])` | group-by on the concatenated children's sequence: one datum per distinct name, with all matching files (the concrete expansion uses first-appearance and encounter order — incidental, §3.3) |

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

All laws are stated about the denotations above and hold **up to
permutation**: equality of datum sequences ignoring datum order, and
equality of datums ignoring file order within each datum.

**Restated 2026-09-26:** P1–P3 were previously stated as *exact* sequence
equalities, which committed the API to first-appearance ordering even
though nothing downstream observes datum order — an ordering commitment in
search of a purpose. Uniform permutation-tolerance states what we actually
care about and frees the implementation (e.g., a future parallel merge).
The implementation still produces deterministic first-appearance /
encounter order, which the `determinism` proptest pins; the two-sided
laws compare canonically (datums and rows sorted).

| # | Property |
|---|---|
| P1 | **Group is idempotent**: `G(G(X)) = G(X)`. After the first pass, names are unique. |
| P2 | **Group is bracket-invariant**: `G([A, B, C]) = G([G([A, B]), C])`. `G` depends only on the flat concatenation of its children's sequences. |
| P3 | **Union is a special case of Group**: if every name in the concatenation of the children's sequences is pairwise distinct — no duplicates across children *or within one child* — then `G([A, B]) = U([A, B])`. (Premise strengthened 2026-09-26: the original "no name appears in two different children" was too weak — `G` also merges a child's own internal duplicates, e.g. a union of two same-repo `"/"` atoms, while `U` never merges. Found by proptest.) |
| P4 | **Cross distributes over Union**: `C(A, U(B, C)) ≃ U(C(A, B), C(A, C))`. |
| P5 | **Union is commutative and associative.** |
| P6 | **Subpath refines star**: for the same repo/URI, every name of `/*/p` appears in `/*` (with the same binding). |
| P7 | **Name ⇒ footprint** (soundness): equal names write to the same `/pfs` locations, so group-merged datums are footprint-compatible by construction. Tested as root membership (every file of a datum lives under a `/pfs/<repo>/` root named by one of its slots); the stronger "equal names ⇒ identical rows" is refuted by distinct globs sharing a binding — e.g. the union of `/*` and `/*/p` over one base yields name `(R, E)` with different rows (C3; wording revised 2026-09-25, the earlier "file-plus-directory duality" example did not land). |

**Known non-law.** Cross does *not* distribute over Group:

```
C(A, G(B, C))  ≠  G(C(A, B), C(A, C))
```

whenever some datum name is produced by *both* `B` and `C` — e.g., the group
idiom, the same repo name declared over two bases, with a common entry. In
the right-hand side, `A`'s files are contributed once *per cross child* that
feeds the merged datum, so they appear twice (identical `uri`/`local_path`
rows). Concretely, with `A = {repo a, base U_a, "/*"}`, `B = {repo r, base
U_1, "/*"}`, `C = {repo r, base U_2, "/*"}` — `B` and `C` declare the same
repo name over two bases — and a common file entry `x` in all three:

- LHS: `G(B, C)` merges `B`'s and `C`'s datums `(r, x)` into one, with files
  `U_1/x`, `U_2/x`; crossing with `A` gives the datum `((a,x),(r,x))` with
  files `U_a/x`, `U_1/x`, `U_2/x`.
- RHS: `C(A, B)` gives `((a,x),(r,x))` with files `U_a/x`, `U_1/x`, and
  `C(A, C)` the same name with files `U_a/x`, `U_2/x`; `G` merges them into
  `U_a/x`, `U_1/x`, `U_a/x`, `U_2/x` — `A`'s files appear twice.

This is a consequence of the intended semantics ("each child of `group`
contributes its datums wholesale"), not a defect. It holds as an equality at
the "set of files per name" level, after duplicate removal — a mathematical
footnote only: the duplicated row makes the RHS a clobber/duplicate-entry
error at `input_to_datums` (see §5.1(3)), so that spec never actually runs.
C4 pins the denotation difference as a pure-core unit test, with a note on
the runtime rejection. (The difference is in row *multiplicity* — `U_a/x`
appearing twice — so it is visible up to permutation, like everything
else.) **Decided (C4 planning):** an earlier draft of this
example gave `B` and `C` distinct repo names; with distinct repo names the
two sides never merge and the equation holds, so that example was wrong.

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
    /// match is non-recursive: one path component. `path` may
    /// be multi-segment.                                       // "/*/path"
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
   **Decided (C4 planning):** no user-facing diagnostic for now — the CLI
   has no reliable info channel (open question 4); a group which merges no
   datums is logged at `debug!`. Revisit. (The deferred `group_key`
   sketch gives this case its real fix: it turns "nothing happened" into
   an expressible diagonal join, and would let any future diagnostic
   suggest "did you mean a shared `group_key`?")
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
   **Decided (C4 planning):** the check is per-datum and general, not
   `group`-scoped: a `cross` of two same-repo atoms over distinct bases can
   clobber the same way, and failing there is the right answer too.
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
   DB boundary). A schema change; probably later. If the deferred
   `group_key` lands, slots should persist `group_key` *and* `repo`:
   with the two decoupled, the name alone no longer addresses the
   footprint, and debugging wants both.
4. **Validation policy.** How much of §5.1(2)–(3) do we check at `job run`
   time (spec-level errors/warnings) versus leaving to the worker? Two
   refinements settled during planning: the clobber check (§5.1(3)) applies to
   file paths only, and the `group` idiom of several atoms declaring the same
   repo name over distinct base URIs should surface as a spec-time *info*
   message so it is discoverable rather than magical.
   - We will probably want to catch as much up front as we can, before we start
     spinning up worker nodes.
   - **Decided (C4 planning):** the clobber check (§5.1(3)) runs at `job
     run` time, server-side, as a per-datum pass in `input_to_datums`
     beside `check_datum_collisions` (outside the pure core) — before any
     worker exists — as a hard error, the one channel the CLI reliably
     surfaces. The spec-time *info* message for the idiom is
     dropped: the CLI has no info channel (stdout is the job name; tracing
     is `RUST_LOG`-gated, defaulting to error), so the idiom is documented
     in the guide instead, and a zero-merge group logs at `debug!`. The
     diagnostic question for §5.1(2) stays open; note the deferred
     `group_key` sketch changes what a good answer looks like (suggesting
     the shared-key join rather than merely flagging the no-op).
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
  **Amended 2026-09-03 (C3 landed):** the serde impl is fully manual
  (a derived string enum cannot select a variant on a prefix pattern),
  and `p` is validated as one or more non-empty path segments.
- Pure-core arm: one datum per directory entry `E` with `E/p` present
  (a file entry has no contents and never matches), decidable from
  per-entry prefix probes (open question 2): list `base/E/p` with a
  delimiter, and keep only results `== base/E/p` or under `base/E/p/`
  (the bare prefix would also match siblings like `pfoo/`). Probe results
  slot into the same `prefix -> (files, dirs)` seam. `p` matches a file or
  a directory (a marker-only `p/` counts as an empty directory).
  **Amended 2026-09-03 (C3 landed):** `p` is multi-segment (per open
  question 1) at no cost change: the probe is a single call at the exact
  prefix `base/E/p`, whose cost is independent of `p`'s depth. The probe
  is a new `CloudStorage::list_prefix` method, because
  `list_nonrecursive` normalizes its argument to a directory (appending
  `/`), which would hide the exact-match object. The seam's keys became
  exact prefixes (plain strings, not normalized bases), so the map type
  was renamed `Listings`. A probe can match both the exact object and a
  subtree (the flat key space allows both `E/p` and keys under `E/p/`);
  that yields **two datums with the same name** `(R, E)`, one per row
  shape, mirroring the top-level file-plus-directory duality. Combined
  into one datum (e.g. crossing the atom with itself) it is a local
  file-system clash, rejected by `verify_local_paths`. `entries_from_listing`
  is deliberately not reused for the probe: its base-marker rule would
  misclassify the exact `p` file. Probes are fetched a few at a time
  (`buffer_unordered(8)`), independently of each other.
  **Revised 2026-09-25:** the probe API differs from the above. There is no
  `list_prefix`: a probe is `CloudStorage::list_one_entry(uri)`, returning
  the entry at `uri` — `Object`, `Prefix` (derived from any key under
  `uri/`, no marker required), or `None` — and the full `/*/p` walk is
  `CloudStorage::list_subpath_entries(base_uri, subpath)`: one
  non-recursive listing of the base, then bounded-concurrent (50-way,
  with perf rationale in the method docs) probes of each top-level
  _directory_ entry, dropping misses. Consequences: each probe yields at
  most one datum per entry — the "two datums with the same name" duality
  never landed (a probe returns whichever kind it finds first); a bucket
  with both `E/p` and keys under `E/p/` violates our filesystem invariant
  and is deliberately not hunted at that depth. There is also no sibling
  filter step: `object_store`'s segment-basis prefix semantics exclude
  `pfoo/` directly. `verify_local_paths` never existed either: collision
  rejection is `check_for_bucket_entry_collisions`
  (`falconeri_common::storage`), run at listing construction
  (`BucketListing::prefix_entries`) and over the full post-cross entry set
  in `input_to_datums` — both _outside_ the pure core, so
  `input_to_datums_pure` has no reachable user-facing error path (its
  remaining `Err`s are I/O↔pure contract violations). The seam's subpath
  keys are `(normalized base, raw subpath)` in a `Listings` struct with
  separate `base_uris`/`subpath_matches` maps, not "exact prefix" strings.
- Tests: unit (file match, directory match, multi-segment, `E`-is-file
  never matches, missing `p` yields no datum, probe sibling exclusion,
  duality, duality-cross rejection), serde round-trip and malformed-form
  rejection, proptest P6 and P7 (root membership).
  **Amended 2026-09-03 (C3 landed):** the proptest listing generator is
  deliberately free — it emits probes with any outcome, including duality,
  and does not avoid the input shapes `input_to_datums_pure` rejects. The
  two-sided laws compare the _partial_ denotation (same defined value, or
  both rejected — the regroupings only regroup rows, so rejection is in
  lockstep across the law's sides); the one-sided P7 filters rejected
  inputs, which have no denotation. The rejection behavior itself is
  pinned by unit tests.
  **Revised 2026-09-25 (harness landed; the partial-denotation design above
  is void — it became unnecessary).** What actually landed:
  - `falconeri_common::storage::mem::MemoryStorage`, an `object_store`
    `InMemory`-backed test backend, reached through a new factory trait
    `CloudStorageForUri` (`CloudStorageResolver` in production,
    `MemoryStorageResolver` per test — no global state, isolation by object
    lifetime, feature `testing`).
  - Generators promoted into `falconeri_common` test support: `Arbitrary
    for Input` draws atoms over a small `memory://` bucket/path pool, so
    whether fragments share buckets is itself generated data; an
    `input_entries` strategy seeds storage guaranteed to match each atom's
    glob, with zero-match draws deliberately generated. Only objects are
    seeded (prefixes are derived, never stored; a desired directory is a
    `.keep` object).
  - Four laws — determinism, P5 commutes and associates, P4
    cross-distributes-over-union — running the real I/O phase
    (`Listings::fetch`) against a fresh `MemoryStorageResolver`, sharing a
    single `Listings` across a law via a "carrier" `Union` of the law's
    atoms. This is sound because `fetch` depends only on the atom multiset
    and bucket contents, and every law regroups a fixed atom multiset; if
    a future law ever varies the atom multiset across sides, fetch per
    side instead.
  - No rejection filtering: the pure core cannot reject generated input
    (collision checks are outside the law seam), so both sides are always
    defined. Rejection/collision behavior is pinned at the storage layer
    instead (`check_for_bucket_entry_collisions`, listing tests in
    `storage/mod.rs`).
  - Not landed: proptest P6/P7, and pinned pure-core unit tests for
    `Glob::Subpath` row shapes (coverage currently rides on the
    `list_subpath_entries` storage tests plus the soak-clean laws; worth
    adding unit pins when touching C4). Marker-object behavior is out of
    reach of `InMemory` and stays pinned by unit-level collision tests.
- Guide: document `"/*/path"`.

**C4 — `Input::Group`.** *(Rewritten 2026-09-26 against the landed C1–C3
code; the stale "C4 landed" notes this replaces are recovered in git
history if ever needed.)*

- `pipeline.rs`: `Group(Vec<Input>)` variant (`snake_case` derives
  `"group"`; `#[schema(no_recursion)]` like `Cross`/`Union`). Add a `Group`
  arm to `Arbitrary for Input`; the existing round-trip and
  schema-validation proptests then cover serde for free.
- `Listings::fetch_helper`: recurse through `Group` like `Cross`/`Union`
  (`group` regroups, never adds atoms — no new fetching).
- Pure-core arm in `input_to_datums_pure`: group-by on `DatumName` over
  the concatenated children — one datum per distinct name, first-appearance
  order, files concatenated in child order. `DatumName` already derives
  `Eq + Hash`. No new error path: the pure core must keep its invariant of
  never rejecting generated input (collision and clobber checks live
  outside it).
- Clobber validation (§5.1(3)), folded into `check_datum_collisions` as a
  second per-datum pass (first pass: bucket-entry URIs; second: file rows
  keyed by `local_path`) — **not** in the pure core,
  where the law harness's generators produce clobbers routinely. Within one
  datum, no two *file* rows (no trailing slash) may share a `local_path`
  with different `uris`; directory rows sharing a `local_path` are legal —
  merging trees is the point. General, not `group`-scoped (same-repo
  `cross`es fail the same check; that is intended). Documented blind spot:
  two files of the same name *inside* two merged directory rows still
  clobber silently, exactly as with whole-repo rows.
- Tests, on the landed harness (see the C3 "Revised 2026-09-25" notes):
  - Unit pins: two base URIs sharing a repo name merge into one datum
    holding both rows; distinct repo names are a no-op (no merge);
    first-appearance order and child-order file concatenation;
    `Group([])` → zero datums (open question 5); the §3.3 non-law pinned
    at the pure-core level (RHS shows the duplicated `A` row — note in the
    test that such a spec is rejected at runtime by the checks above).
    While touching C4, also add the `Glob::Subpath` row-shape pins C3
    noted as missing.
  - proptest P1 (idempotent), P2 (bracket-invariant), P3 (union-as-group,
    with an explicit name-disjointness `prop_assume!`), using
    `input_and_entries()` + the `input_listings()` shared-listings carrier:
    all three laws regroup a fixed atom multiset, so one `fetch` is sound.
    Adding `Group` to `Arbitrary for Input` also exercises groups inside
    existing P4/P5 fragments (they hold: `group` is a function of its
    children's denotation, and the laws only regroup). Shared repo names
    arise naturally from the `r[123]` alphabet; cross-fragment bucket
    sharing is reachable via the small 6-bucket pool but random — add a
    knob to force same-bucket fragments if merge coverage turns out thin.
- A migration note ships with this chunk (guide + code comment): for users
  coming from Pachyderm's `group` input, falconeri's merge key is the
  datum name (repo + star binding), not a `groupBy` pattern, and the merged
  datum materializes as one `/pfs/<repo>/<binding>/` directory rather than
  per-repo `/pfs` trees. Merging across distinct base URIs therefore
  requires declaring the same repo name over those URIs.
- Guide: document `group`, including the migration note.

**Related fix (landed 2026-09-26).** The whole-job collision check used to
run once over the entries of *all* datums flattened together, which
rejected any `cross` with a multi-datum operand (crosses legitimately
repeat one entry URI across datums). Now fixed: `check_datum_collisions`
runs the URI-keyed check per-datum — one datum, one worker filesystem view
— with regression tests for all three cases (cross repeats OK; object/prefix
pair in *different* datums OK; within-datum collisions rejected). C4's
clobber check lands folded into the same function, as a second per-datum
pass keyed on `local_path`. Consequence for C4: exact-duplicate rows in
one merged datum (two `group` children with the same base + glob) are a
per-datum "duplicate bucket entries" error, which is the intended
behavior.

**Deferred (after the algebra lands).**

- Spec-level validation at `job run` per §5.1(2) (the §5.1(3) clobber check
  lands in C4).
- Persisting datum names (a `names` column; schema change; open question 3;
  should store `repo` and `group_key` if the latter lands).

### The biggest open algebra question: `group_key` (sketched 2026-09-26)

Our merge key `(repo, binding)` conflates two concepts: `repo` is both
_placement_ (the `/pfs/<repo>/` root) and _identity_ (what `group` matches
on). The proposed generalization adds an optional per-atom `group_key`,
defaulting to `repo`, and makes the merge key (and the name slot)
`(group_key, binding)`. Defaulted, it is _exactly_ today's semantics — the
same-repo idiom is just a shared default key — so C4 is the default
instantiation of the extension, not a legacy case to migrate. Setting a
shared `group_key` across _different_ repo names enables the diagonal join
Pachyderm-style group provides (a worker sees `/pfs/a/E/` and `/pfs/b/E/`
as one datum), while keeping merging typed intent: coincidental star-match
equality stays inert, which was our main objection to binding-only names.
The key extension argument is that it is clash-conservative: the new
merges span different `/pfs` roots and therefore cannot introduce
file-level clashes by construction, so `check_datum_collisions` and the
clobber check stay complete without change.

Adopting it later would require restatement, not repair: P1–P3 are
unaffected (keys ride in name tuples through `cross`; the P3 premise is
about name distinctness, whatever slots hold), while P7's "the name is the
directory the datum writes into" weakens to "the name is the work item's
identity; clashes are possible only among same-root rows" — an explanation
of why safety is free, rather than a safety mechanism itself. The docs'
mental model becomes one sentence — "`repo` says where your data lands;
`group_key` says when two sources are the same thing" — and the
migration note gets simpler (declare a shared `group_key`, instead of
reusing a repo name and accepting tree overlay). Open sub-decisions if it
lands: naming (avoid confusion with the `group` combinator), whether
whole-repo `"/"` atoms with a shared key should merge (likely yes), and
rejecting a never-merge-unless-declared default, which would kill the
overlay idiom. Nothing shipped prevents this; if we got the fork wrong,
this is the door back.

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
