# Input Algebra Extensions: `/*/path` globs and `Group`

**Status:** Preliminary design draft. No code has been written yet.

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

Let `L(base)` be the listing of top-level entries of `base`.

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
   check we can turn into a validation error.
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
2. **Cost of `/*/p` listing.** The implementation needs `1 + N` listings (the
   base plus each top-level directory), where `N` is the number of top-level
   entries. Fine for reasonable `N`; note it if we ever see huge flat repos.
3. **Persist the datum name?** A `names` column on `datums` would make
   `job describe`/debugging much easier (names are currently dropped at the
   DB boundary). A schema change; probably later.
4. **Validation policy.** How much of §5.1(2)–(3) do we check at `job run`
   time (spec-level errors/warnings) versus leaving to the worker?
5. **Empty `Group` / empty `Union`.** Both should yield zero datums,
   consistently with `Cross([])`; just confirming the convention.

## References

- `falconerid/src/inputs.rs` — `input_to_datums` and the local value types
  (`DatumData`, `InputFileData`); home of the functional core.
- `falconeri_common/src/pipeline.rs` — `Input` and `Glob`: the algebra's
  signature as seen from the pipeline spec.
