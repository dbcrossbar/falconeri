# Cloud Storage I/O (`object_store`)

> **Status:** Research complete, testing strategy agreed (§2.4), work broken into a pre-beta patch series (§2.5). Five bugs (**CS-1 … CS-5**) are release blockers. Inter-object concurrency is **deferred** to a follow-up. Intra-object (parallel chunk) concurrency is **deferred indefinitely** — our headroom numbers say it can't help at current pod sizes. CS-7 is **parked**: it is probably the intended two-phase reserve-then-upload design, not a bug.

**Basis.** `object_store` 0.14.2 from crates.io (since `bda747e`, "Stop using git version of object-store"), audited against the vendored crate source. Our code: `falconeri_common/src/storage/{mod,s3,gs}.rs`, `falconeri-worker/src/main.rs`, `falconerid/src/inputs.rs`. Vendor numbers from the AWS S3 performance guidelines / quotas and GCS quotas pages (Sept–Oct 2025).

---

## 1. Research: limits and numbers

### 1.1 Hard limits

| | S3 (general purpose) | GCS |
|---|---|---|
| Max object size | 48.8 TiB (= 5 GiB × 10,000) | **5 TiB** ← our real ceiling |
| Multipart parts | max **10,000**, numbered 1–10,000 | max **10,000** (XML API) |
| Part size | 5 MiB – 5 GiB; last part may be smaller | same (min enforced only at *complete*) |
| Max key length | **1,024 bytes UTF-8**, including prefix + `/` | 1,024 bytes (512 + 512 with HNS) |
| Keys per LIST page | 1,000 | 1,000 |
| Bulk delete | `DeleteObjects` ≤ 1,000/request | JSON batch < 10 MiB, ≤ 100 calls |
| Incomplete MPU lifetime | **never expires**; bills until complete/abort | never (resumable sessions: 7 days) |

Corollaries for us:

- Chunk size must be `clamp(ceil(size / 10_000), 5 MiB, 5 GiB)`. With `WriteMultipart`'s **default 5 MiB**, the maximum object is **5 MiB × 10,000 ≈ 48.8 GiB** — the "60GB+" case named in our own doc comment cannot be uploaded today (CS-3).
- If checksums are used, **part numbers must be consecutive from 1** or `CompleteMultipartUpload` returns HTTP 500.
- Both services need an `AbortIncompleteMultipartUpload` lifecycle rule (see CS-6): a crashed worker's parts bill forever, and neither backend can clean up on drop.

### 1.2 Rate limits and throttling

- **S3:** 3,500 PUT/COPY/POST/DELETE and 5,500 GET/HEAD **per second per partitioned prefix**; unlimited prefixes. Scaling to a new request rate is gradual and returns **HTTP 503 Slow Down**, which is a normal, retryable condition. Sustained >5,000 req/s on a *small number of objects* also triggers 503.
- **GCS:** buckets start around **1,000 writes/s and 5,000 reads/s** and auto-scale; bandwidth overage returns **429 `rateLimitExceeded`**. Hard per-object limits: **1 write/s per object name**, 1 metadata update/s.
- GCS **parallel composite uploads** are not our path and are a trap anyway (≤32 components, composite objects have crc32c but **no MD5**, temporary objects must be hard-deleted, incompatible with retention policies). object_store's GCS backend uses **XML API multipart** (`partNumber` + `uploadId`), which Google explicitly recommends instead, and which is order-independent — so parallel parts are safe on both backends.

Design consequence: any concurrency budget must be computed **cluster-aggregate per prefix**, not per pod. Our only high-request-rate path today is `list_subpath_entries`, which is already bounded at 50.

### 1.3 What `object_store` gives us (and doesn't)

- **Downloads are stream-only.** `get` / `get_opts` → sequential stream, never buffers the whole object. There is **no download-to-file or parallel-chunk download helper** (upstream issues #274 / #279 ask for one). Options: `get_ranges` (coalesces adjacent ranges, ≤ **10** parallel requests via `OBJECT_STORE_COALESCE_PARALLEL`, returns everything **in RAM**), or manual `get_opts` + `Range` fan-out. `buffered::BufReader` is documented as *slower* than `get` for sequential reads.
- **Uploads are already parallel.** `put_multipart` + `WriteMultipart` spawns each part into a `JoinSet`. It just needs backpressure (`wait_for_capacity`) and a chosen chunk size. The low-level `MultipartStore` trait (`create_multipart` / `put_part` / `multipart_complete` / `abort`) is implemented for S3 and GCS if we ever want resumable, cross-pod uploads.
- **Wrappers we should use:** `object_store::limit::LimitStore::new(store, N)` for a global in-flight cap (also caps multipart parts), `throttle::ThrottledStore` for request-rate limits and for deterministic tests, `delete_stream` for bulk deletes.
- **Client knobs** live in `ClientOptions` / `RetryConfig`: `timeout` (default **30 s, covering the whole response body**), `read_timeout` (resets per successful read; better for long transfers), `connect_timeout` (5 s), HTTP/2 keep-alives, `pool_max_idle_per_host`.
- **Listing quirk:** `list` / `list_with_delimiter` **always append `/` to the prefix**, so prefix matching is segment-scoped (`a` never matches `ab/c`). Good for us — but it also means a raw string prefix can't be listed through the trait.

### 1.4 Throughput model for a ~2-CPU worker pod

2-vCPU instance ceilings (per *instance*, not per pod):

| | m7i.large | m6g.large |
|---|---|---|
| Network | "up to 12.5 Gbps", **baseline ≈ 98 MiB/s** (credits burst 5–60 min) | "up to 10 Gbps", baseline ≈ 75 MiB/s |
| EBS | **baseline 650 Mbps ≈ 81 MiB/s**, 3,600 IOPS | 630 Mbps ≈ 79 MiB/s |
| gp3 volume | 125 MiB/s baseline (so instance-bound anyway) | same |

Our own CPU cost is ~1 core to sustain ~1 Gbps (TLS ~0.3–0.5 core/Gbps, checksum ~0.2–0.4, chunk copy + `write_all` ~0.3–0.5), so 2 CPUs is comfortably enough to hit the platform wall.

**Working assumption: ~100 MiB/s sustained per 2-CPU pod**, single stream, and it is usually the *scratch disk* rather than S3 that binds.

| object size | single stream at ~100 MiB/s | verdict |
|---|---|---|
| 1 GiB | **~5–10 s** | ignore |
| 3–10 GiB | ~30–90 s | fine perf-wise; this is where CS-1 (30 s timeout) starts killing transfers (~3.7 GiB at 100 MiB/s) |
| 60 GiB | ~8–15 min | only here does parallel chunking pay, and only if scratch storage also improves |
| whole job | scales linearly with pod count (100 pods ≈ 600 GiB/min) | never the bottleneck |

Caveat: network I/O credits and EBS bandwidth are **node-level**. A 2-CPU pod sharing an 8-vCPU node with 3 siblings downloading at once gets roughly a quarter of each budget.

Also: **MinIO (local dev) is not representative of any number in this section**, and its tolerance of some edge cases (see CS-4) actively hides bugs.

---

## 2. Bug fixes

### 2.1 Release blockers

| ID | Location | Symptom | Cause |
|---|---|---|---|
| **CS-1** | `s3.rs`, `gs.rs` (no `ClientOptions` set) | any object taking >30 s to transfer fails, then restarts from zero | default `timeout = 30s` applied to the reqwest client, documented as "until the response body has finished" |
| **CS-2** | `stream_upload_from_file` (`mod.rs:361`) | worker pods **OOMKilled** on multi-GB outputs | `WriteMultipart::write` starts parts "regardless of how many outstanding uploads are already in progress"; we never call `wait_for_capacity`, so disk speed outruns the network and the file accumulates in RAM |
| **CS-3** | `mod.rs:374` (`WriteMultipart::new`) | uploads > ~48.8 GiB fail at `CompleteMultipartUpload` | default 5 MiB chunks × the 10,000 part limit |
| **CS-4** | `mod.rs:361` (always `put_multipart`) | **any 0-byte output file fails the datum** | a 0-byte file fills no chunk buffer, so `finish()` calls `complete()` with **zero parts**, which S3 and GCS reject. Invisible in tests: our `MemoryStorage` delegates to `InMemory`, whose `complete()` happily stores an empty object |
| **CS-5** | `sync_up_dir` (`mod.rs:889`) | **silent data loss**: outputs never uploaded, datum reported `Done` | `WalkDir::into_iter().filter_map(\|e\| e.ok())` discards permission/IO/`read_dir` errors, and `upload_outputs` marks every recorded `OutputFile` `Done` from the overall result |

Approach (deliberately loose — details worked out in the working session):

- **CS-5 first** — it's the only silent-wrong-results bug. Propagate walk errors with the offending path in the context.
- **CS-4 + small-file handling** in one change: use `put` below a size threshold and multipart above. This also fixes the "3 requests per tiny file" cost issue, which is the same code decision.
- **CS-1**: `with_timeout_disabled()` + a generous `read_timeout` (stall detection without a transfer cap), and a deliberate `RetryConfig` review. Set these once, ideally where stores are constructed so S3/GCS share the policy.
- **CS-2 + CS-3** together: compute chunk size from the file size, then bound in-flight parts so `N × chunk_size` stays inside the pod's memory budget.
- Ops, independent of code: lifecycle rule to abort incomplete multipart uploads on our buckets.

Tests are in §2.4; the order of work is §2.5. Note that **CS-1 and CS-2 deliberately have no tests**: honest verification needs a real multi-GB transfer over a real link, and asserting their configuration values would only re-test constants we chose. Both live at a single construction site, so they are code-review items.

### 2.2 Triage after release

Not scheduled for the beta, except CS-6, which rides along in §2.5.

| ID | Location | Issue | Notes |
|---|---|---|---|
| CS-6 | `mod.rs:380-390` | on our own error path we drop `WriteMultipart` without `abort()` → orphaned parts billed forever | best-effort `abort()` + log; bucket lifecycle rule regardless |
| CS-7 | `worker/main.rs:291` vs `mod.rs:889` | `/pfs/out` is walked twice by two different walkers | **Probably not a bug** — the design is deliberately two-phase: reserve/record the output files, *then* upload. Worth a separate discussion about what should happen to a file created or deleted between the two walks before treating the second walk as work to do. Not scheduled. |
| CS-8 | `mod.rs:889`, `worker/main.rs:243,291` | `WalkDir`, `std::fs`, `glob` run on the async runtime | a long walk stalls the stdout/stderr tee tasks and can block the pipeline process on a full pipe. `spawn_blocking` |
| CS-9 | `mod.rs:468-475` (+ its TODO at `:470`), `worker/main.rs:126-129` | a fresh client per `for_uri` call; the worker builds a resolver *inside the per-file loop* | no connection/TLS/credential reuse: roughly 0.2–1 s and one auth round trip **per file**. Fix with the concurrency work, not separately — it's the natural place for a single `ClientOptions` and `LimitStore` |
| CS-10 | `sync_up_dir` key building (`mod.rs:903`) | no key-length budget check | our `%XX` encoding inflates non-ASCII names up to 3× against a 1,024-byte limit; today it surfaces as an opaque backend error |
| CS-11 | `local_relative_path` (`mod.rs:164`) | `from_url_path` decodes then parses, so a foreign key `a%2Fb.txt` lands on `a/b.txt` and collides | silent last-write-wins. (`.`/`..` are already rejected.) A duplicate-local-path check per `sync_down` is enough |
| CS-12 | `list_nonrecursive` vs `list_one_entry` | the former logs and ignores non-`NotFound` `head` errors, the latter propagates | an auth/5xx probe error can look like "empty prefix". Pick one policy |
| CS-13 | `stream_download_to_file` | a failed download leaves a truncated file at the final path | **Low priority**: we're usually killing the pod anyway, so the dirs vanish with it. The part that matters is that the *reported error* is correct and specific (tested in §2.4-C). Temp-file-then-rename only if we ever reuse a work dir without `reset_work_dirs` |

### 2.3 What `InMemory` cannot stand in for

Our `MemoryStorage` models the semantics we actually implement — derived existence, segment-scoped prefixes — so it is not a fidelity problem for most of our surface (marker objects are not part of our model; see §2.4 "not tested"). What `InMemory` genuinely gets *wrong* is request legality: it happily accepts a `complete()` with zero parts and undersized parts, which S3 and GCS reject. That is the one gap a real server is needed for, and it is covered by a single MinIO test rather than a suite.

### 2.4 Testing strategy

**Principle: test logic we wrote — arithmetic, branching, error propagation, round-trip identity — never constants we chose.** If a test can only fail when someone edits a constant, delete it. This is why CS-1 and CS-2 go untested rather than tested dishonestly.

Tests below are grouped by *what they'd cover*, not by schedule; the items scheduled for the beta are listed in §2.5.

**Machinery: one piece.** An `ObjectStoreFaultInjector` (~30–50 lines, under the existing `testing` feature) wrapping `InMemory`: fail the Nth call of a given kind with a chosen error, and record which methods were called. Everything else runs on plain `MemoryStorage`, `assert_fs`, and `proptest`, all already in the tree.

**A. Pure logic — no machinery** (~30 lines)

| Test | For | Why it earns its place |
|---|---|---|
| chunk-size policy table: tiny / 1 GiB / 48.8 GiB / 5 TiB / over 5 TiB | CS-3 | `clamp(ceil(size / 10_000), 5 MiB, 5 GiB)` is arithmetic that can be wrong; the boundaries *are* the bug |
| key-length budget produces a clear domain error | CS-10 | our `%XX` encoding inflates names up to 3×; the check is logic |
| decoded-name collision is detected | CS-11 | decode-then-compare logic |
| *(existing)* collision checker — keep as is, add a comment recording the stance | — | **Buckets containing both `foo.txt` and `foo.txt/hah` are out of scope: hard error when we happen to notice, no extra API calls to hunt for it.** The comment is there so nobody later builds fixtures for it |

**B. Filesystem-level — no store machinery** (~60 lines)

| Test | For | Notes |
|---|---|---|
| `sync_up_dir` propagates walk errors | **CS-5** | two variants: an unreadable directory (`chmod 000`, skipped when `uid == 0` — containers run as root), **and a symlink loop**, which makes `WalkDir` yield an error even as root, so CI always exercises it |
| round-trip identity: local tree → `sync_up_dir` → `sync_down` → byte-identical tree, over nasty names (spaces, `%`, `#`, unicode, `.keep`, deep nesting) | the encode/decode boundary | documented exclusions: `.`/`..` segments, over-budget names, empty directories |
| the same round trip at ~2,000–5,000 objects, asserting **exact set equality** | the deferred fan-out rewrite | ~10 lines more than the previous test; "silently dropped one file in 5,000" is precisely the concurrency bug we would ship. This is the test I most want in place before §3 |
| empty file and empty subdirectory round trip **in memory** | documents intent | explicitly *not* the CS-4 test — it passes today because `InMemory` is too permissive; say so in the test comment |

**C. With the injector** (~60 lines)

| Test | For |
|---|---|
| upload fails mid-way → `abort()` reached on the store | **CS-6** — the machinery is worth it for this one |
| download fails mid-stream → `Err` naming the object | CS-13. The pod is probably being killed anyway; what matters is that **the reported error is correct and specific** |
| probe returns 5xx rather than `NotFound` → error propagates instead of becoming "empty prefix" | CS-12 — real branching on error kind |
| *(optional)* empty and small files take the `put` path, large files take multipart | CS-4. Legitimate because it pins which branch ran, not a constant |

**D. MinIO: one env-gated test, plus a manual smoke checklist**

`FALCONERI_MINIO_TEST=1` (a `just` target; skipped by default so `just check` stays fast): **empty file + ~30 MiB multipart + a deep weird-named tree, round trip.** This is the only place a server that enforces request legality is irreplaceable — it is what actually proves CS-4 and our part arithmetic. Everything else real-bucket-shaped (credentials, endpoints, path-vs-virtual-host style, on-the-wire key encoding) belongs on the pre-release manual smoke checklist, not in CI.

**Not tested, on purpose**

| Item | Reason |
|---|---|
| CS-1 timeout | needs a >30 s real transfer; a config assertion would be testing a constant |
| CS-2 backpressure | `InMemory` completes instantly; a latency-decorator version would only prove our own semaphore works |
| CS-8 blocking filesystem calls | code-review item |
| pathological buckets (`foo.txt` beside `foo.txt/hah`) | out of scope by design; the existing pure collision tests are the whole surface |
| marker objects / empty-prefix emulation | not part of our model — existence is derived (see the module docs in `storage/mod.rs`), and a marker's only observable effect is a collision already covered above |
| worker I/O (`upload_outputs`) | hardcodes `/pfs/out`; testable with `TempDir` + `MemoryStorage` once paths are parameterized, pending the CS-7 discussion |

**Sizing.** A + B ≈ 90 lines with no new machinery; C is the injector plus ≈60 lines; D ≈30 lines plus a `just` target. Roughly a dozen tests, all but one inside `just check`.

### 2.5 Patch series

The pre-beta series. One thing per patch: refactor patches change no behavior and must leave existing tests passing untouched, and no patch mixes a refactor with a behavior change. Each patch compiles and passes `just check` on its own. Everything else in §2.2 stays unscheduled for now.

1. [x] Move `stream_download_to_file`, `stream_upload_from_file`, `sync_down`, and `sync_up_dir` out of the `CloudStorage` trait into free async functions in `storage::sync.rs`; the trait methods become one-line delegations and stay as backend override points. Drop the empty `impl dyn CloudStorage {}`, and have `sync.rs` link to the semantics docs in `mod.rs` rather than restate them. No behavior change — `.store()` is only used inside `mod.rs` (8 sites), so this touches no call sites. `fn store(&self) -> &dyn ObjectStore` does **not** need to become `Arc`: `ObjectStore: Send + Sync`, so a borrowed handle is fine for `buffer_unordered` later.
2. [x] Move the worker's per-file download loop into `storage::sync` as `sync_down_all(&mut dyn CloudStorageForUri, &[SyncTarget])` with `SyncTarget { uri, local_path }`. The worker builds **one** resolver per pod (kills the per-file client construction, half of CS-9), maps `InputFile` rows to targets, validates URIs before touching the filesystem, and wraps every failure with the target that caused it. Resolve buckets up front into a `Vec<Arc<dyn CloudStorage>>`, then iterate. Still sequential.
3. [ ] Round-trip tests: local tree → `sync_up_dir` → `sync_down` → byte-identical, over nasty names; plus the ~2,000–5,000 object variant asserting exact set equality. Lands **before** any transfer-code changes.
4. [ ] CS-5: stop swallowing `WalkDir` errors in `sync_up_dir`; include the offending path. Tests: symlink loop (errors even as root) and `chmod 000` (skipped when `uid == 0`). Needs no new machinery, so it goes in ahead of the rest.
5. [ ] Test machinery: `ObjectStoreFaultInjector` under the `testing` feature — wraps `InMemory`, fails the Nth call of a kind, records which methods were called. Handed straight to the `sync.rs` functions; no `CloudStorage` adapter needed.
6. [ ] CS-4: `put` for empty and small objects, multipart above the threshold. Test: which branch ran, using the injector's call log — plus the in-memory empty-file round trip, commented as documenting intent rather than proving the fix.
7. [ ] MinIO: `FALCONERI_MINIO_TEST=1` plus a `just` target — empty file, ~30 MiB multipart, and a deep weird-named tree round trip. This is what actually proves CS-4; land it with that patch if convenient.
8. [ ] CS-3: pure chunk-size function `clamp(ceil(size / 10_000), 5 MiB, 5 GiB)`, table-tested at tiny / 1 GiB / 48.8 GiB / 5 TiB / over, wired into the upload path; align the read buffer to the chunk size.
9. [ ] CS-2: bound in-flight parts with `wait_for_capacity(N)`, `N × chunk_size` inside the pod's memory budget. Untested on purpose (§2.4).
10. [ ] CS-1: set `ClientOptions` at one shared construction site for S3 and GCS — `timeout` disabled plus a `read_timeout` for stall detection, and a deliberate `RetryConfig` review. Untested on purpose (§2.4); the guard is that it exists in exactly one place.
11. [ ] CS-6: best-effort `abort()` on upload error paths, logged; injector test asserting `abort` is reached. Normally §2.2 triage, but it rides along cheaply once the injector exists and real users are pushing data.
12. [ ] Polish & docs:
    - Ops: lifecycle rule aborting incomplete multipart uploads on our buckets (S3 and the GCS equivalent), documented where bucket setup is documented.
    - Two comment edits recording decisions: reword `mem.rs`'s "limitations" note (marker objects are not part of our model — existence is derived), and add the *hard error when noticed, never hunt* comment beside `check_for_bucket_entry_collisions`.

---

## 3. Inter-object I/O concurrency — **postponed**

**Today:** `sync_down` lists and downloads strictly sequentially (see the `TODO` at `mod.rs:816`), `sync_up_dir` uploads one file at a time, and the worker downloads a datum's input files one at a time. The only real parallelism is pod count, plus the bounded probe fan-out in `list_subpath_entries`.

**Why postpone:** the wins here are per-file *latency*, not bandwidth, and they are meaningless while CS-1/CS-2/CS-5 are unfixed. Fixing the bugs first also gives us measurements to size the fan-out instead of guessing.

**Shape when we pick it up** (non-binding):

- Cache `Arc<dyn CloudStorage>` per `(scheme, bucket)` in `CloudStorageResolver` (resolves the TODO at `mod.rs:470`) — one client, one connection pool, one credential, one `ClientOptions`, and one place to wrap the store in `LimitStore` so *aggregate* in-flight requests are capped in a single place instead of scattered `buffer_unordered` calls.
- Bounded `buffer_unordered` over already-listed objects in `sync_down`, and over the file list in `sync_up_dir` / the worker.
- Start from the 2-CPU budget in §1.4: ~2–4 concurrent transfers per pod is all the bandwidth and CPU can use. More is only useful for overlapping latency, not throughput.
- Check the aggregate against §1.2: `pods × N × requests-per-second` vs 3,500 PUT/s and 5,500 GET/s per prefix, and keep output prefixes spread per job rather than one hot prefix.
- Fold in CS-8 and CS-9 while we're in this code.

**Trigger:** a real pipeline whose datums carry many files (order of 20+), or measurements showing per-datum wall time dominated by small-file round trips.

---

## 4. Intra-object I/O concurrency — **postponed, probably not worth it**

Nothing built in (§1.3). If we ever need it: `head` for size → split into ranges → `get_opts(..., with_range)` for streaming ranged GETs → `buffer_unordered` → positional write. Note `tokio::fs::File` has **no `read_at`/`write_at`**, so it's `std::os::unix::fs::FileExt::write_all_at` inside `spawn_blocking`, or one `File` per chunk with its own seek. Memory is `chunk_size × N`.

**Why the numbers argue against it** (§1.4):

- A 2-CPU pod is capped near **100 MiB/s** by network baseline (~98 MiB/s) and EBS baseline (~81 MiB/s) — and usually by scratch disk, not by S3.
- One stream already reaches that wall using about **one core**. Adding streams adds CPU contention inside the pod and against sibling pods without raising the wall.
- So parallel ranges only pay off when a *single* object's transfer time is minutes — call it **≥10 GiB** — and even then the gain (maybe 2–4×) requires also moving scratch to instance-store or provisioned-throughput EBS, otherwise it is still disk-bound.

**Two decisions it would force, if we revisit them:** chunk size becomes a durable format decision (AWS recommends reading objects back aligned to the part sizes used on upload), and it must satisfy the part-number budget `≥ ceil(size / 10,000)`. Pick once, apply to both directions.

**Trigger:** a real ≥10 GiB input or output in an actual pipeline. Not before.

---

## 5. Open questions

- Which scratch storage do worker pods actually get — default gp3, provisioned, or instance store? This single unknown decides §4, and it's worth a measurement in a real pod.
- Do we record content checksums (crc32c / CRC64NVME) on `OutputFile` rows? ETags are useless for multipart objects (checksum-of-checksums), so verification or dedupe requires storing our own.
- Should output publication use a commit marker plus a conditional write (`If-None-Match` on `PutObject` / `CompleteMultipartUpload`, supported on both backends) to make retried datums idempotent? Related to `DUPLICATE_DATUM_ERROR_ANALYSIS.md`.
- Do we want empty directories to survive an upload/download round trip? Currently they don't (object stores have no directories and we don't write markers).
- CS-7: what are the intended semantics of the two-phase reserve-then-upload in the worker? Decide separately before treating the double walk of `/pfs/out` as a defect.

### Measuring for real

Local MinIO numbers are meaningless here. In a real worker pod:

```sh
aws s3 cp s3://bucket/1gb.bin - > /dev/null        # single stream, network only
time aws s3 cp s3://bucket/1gb.bin /work/1gb.bin   # + scratch disk
dd if=/dev/zero of=/work/z bs=1M count=2000 oflag=direct   # scratch write ceiling
```

(`aws s3 cp` uses ~10 concurrent parts by default, so the `-` variant approximates our single stream. Watch ENA/CloudWatch metrics for burst-credit exhaustion.)
