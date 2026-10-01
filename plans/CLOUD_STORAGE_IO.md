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

- Part size must be `clamp(ceil(size / 10_000), FLOOR, 5 GiB)`. **We set FLOOR to 16 MiB**, above the 5 MiB service minimum, for the request-cost reason in §1.2. With `WriteMultipart`'s **default 5 MiB** we inherit the service minimum, so the maximum object is **5 MiB × 10,000 ≈ 48.8 GiB** — the "60GB+" case named in our own doc comment cannot be uploaded today (CS-3).
- If checksums are used, **part numbers must be consecutive from 1** or `CompleteMultipartUpload` returns HTTP 500.
- Both services need an `AbortIncompleteMultipartUpload` lifecycle rule (see CS-6): a crashed worker's parts bill forever, and neither backend can clean up on drop.

### 1.2 Rate limits and throttling

- **S3:** 3,500 PUT/COPY/POST/DELETE and 5,500 GET/HEAD **per second per partitioned prefix**; unlimited prefixes. Scaling to a new request rate is gradual and returns **HTTP 503 Slow Down**, which is a normal, retryable condition. Sustained >5,000 req/s on a *small number of objects* also triggers 503.
- **GCS:** buckets start around **1,000 writes/s and 5,000 reads/s** and auto-scale; bandwidth overage returns **429 `rateLimitExceeded`**. Hard per-object limits: **1 write/s per object name**, 1 metadata update/s.
- GCS **parallel composite uploads** are not our path and are a trap anyway (≤32 components, composite objects have crc32c but **no MD5**, temporary objects must be hard-deleted, incompatible with retention policies). object_store's GCS backend uses **XML API multipart** (`partNumber` + `uploadId`), which Google explicitly recommends instead, and which is order-independent — so parallel parts are safe on both backends.
- **Every part is a write request**, and both services bill it as one (S3 `PUT`; GCS Class A—"Class A operations are performed to create each part"). So part size is a direct lever on request cost and request rate. At ~100 MiB/s per pod (§1.4): 5 MiB parts = 20 requests/s per pod, 16 MiB = 6, 64 MiB = 1.6. A job's pods share one output prefix, so with 5 MiB parts the budget runs out at roughly **175 pods (S3, 3,500 PUT/s per prefix)** or **50 pods (GCS, ~1,000 writes/s initially)**; 16 MiB parts push both about 3× further. Money is noise next to that: a 1 TiB output is ~210k requests at 5 MiB, ~65k at 16 MiB. **Decision: 16 MiB floor**, because the memory cost is fixed and small while the request cost scales with the cluster.
- **GCS "one write per second" applies to writes to the same object *name*, not to parts of one upload.** The quotas page pairs that limit with object immutability, and separately allows "Maximum number of different multipart uploads that can simultaneously occur for an object: Unlimited", which is consistent with Google recommending parallel XML parts. Confidence: medium-high, by inference—Google does not state it outright. The limit *does* matter elsewhere: a retried or duplicate datum rewriting the same output name (see `DUPLICATE_DATUM_ERROR_ANALYSIS.md`).

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

Our own CPU cost is ~1 core to sustain ~1 Gbps (TLS ~0.3–0.5 core/Gbps, checksum ~0.2–0.4, chunk copy + `write_all` ~0.3–0.5), so 2 CPUs is comfortably enough to reach the platform baseline speeds.

**Working assumption for typical sizing: ~100 MiB/s sustained per 2-CPU pod**, single stream. On a default gp3 volume it is usually the *scratch disk* rather than S3 that binds; the worked example below assumes SSD-or-better scratch, provisioned to keep up with uploads.

| object size | single stream at ~100 MiB/s | verdict |
|---|---|---|
| 1 GiB | **~5–10 s** | ignore |
| 3–10 GiB | ~30–90 s | fine perf-wise; this is where CS-1 (30 s timeout) starts killing transfers (~3.7 GiB at 100 MiB/s) |
| 60 GiB | ~8–15 min | only here does parallel chunking pay, and only if scratch storage also improves |
| whole job | scales linearly with pod count (100 pods ≈ 600 GiB/min) | never the bottleneck |

**An envelope, not a wall.** The ~100 MiB/s figure is a baseline estimate for a 2-vCPU pod on a typical instance under normal load, not a hard limit. A pod on an idle or larger instance can draw much more, and our code should let it. Worked example: a 60 GiB upload with `N = 2` in-flight 16 MiB parts, idle instance, burst credits full, fast scratch. Request count never limits it: one in-region TLS stream carries multiple Gbps by itself. CPU binds first, at about 2 Gbps for 2 vCPU (~1 core/Gbps), and two streams reach that. The upload therefore adapts up to ~2.5x baseline with `N = 2`. Filling the 12.5 Gbps burst would need ~4–8 cores of network CPU, so an idle pod leaves burst bandwidth unused at *any* `N`; extra streams would only add contention. The one shape where stream count limits throughput is a high-RTT or lossy link (a cross-region endpoint): per-stream TCP throughput falls with RTT and loss, and concurrency is the only cure. Our buckets are same-region today; revisit `N` if that changes.

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
| **CS-4** | `mod.rs:361` (always `put_multipart`) | every empty or small output costs a 3-request multipart handshake and an incomplete upload that bills for parts if we fail midway | a 0-byte file fills no chunk buffer, so `finish()` calls `complete()` with **zero parts**, which the raw S3 and GCS APIs reject. See the CS-4 note below: `object_store` papers over that, so uploads succeed today |
| **CS-5** | `sync_up_dir` (`mod.rs:889`) | **silent data loss**: outputs never uploaded, datum reported `Done` | `WalkDir::into_iter().filter_map(\|e\| e.ok())` discards permission/IO/`read_dir` errors, and `upload_outputs` marks every recorded `OutputFile` `Done` from the overall result |

**CS-4 note (found while implementing).** `object_store` 0.14.2 never sends a
zero-part completion to either backend: the AWS client uploads one empty part
first (`aws/client.rs`, `complete_multipart`), and the GCS client aborts the
upload and falls back to a plain `put` (`gcp/client.rs`, `multipart_complete`).
So "0-byte output fails the datum" does not happen on S3 or GCS today, and the
release-blocker framing was wrong. What CS-4 still costs is requests and risk:
three requests per small file, against the per-prefix rates of §1.2, and an
incomplete multipart upload for every empty or small output — the orphaned-parts
case CS-6 worries about. The single-`put` path removes both, and makes us
independent of a workaround we do not control.

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
| part-size properties (proptest): part ≥ service minimum, ≤ service maximum, and `ceil(size / part) ≤ 10,000` | CS-3 | properties, not constants—any legal policy passes, so changing the 16 MiB floor does not break the test. Boundary sizes are drawn directly, not hoped for. **Vacuity trap we found:** the 5 GiB ceiling can only bind above `10,000 × 5 GiB`, so a range limited to sizes we can actually build cannot fail even with the clamp removed. Draw from both sides of that ceiling and guard the part-count property. |
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
3. [x] Round-trip tests: local tree → `sync_up_dir` → `sync_down` → byte-identical, over nasty names; plus the ~2,000–5,000 object variant asserting exact set equality. Lands **before** any transfer-code changes.
4. [x] CS-5: stop swallowing `WalkDir` errors in `sync_up_dir`; include the offending path. Tests: symlink loop (errors even as root) and `chmod 000` (skipped when `uid == 0`). Needs no new machinery, so it goes in ahead of the rest.
5. [x] Test machinery: `ObjectStoreFaultInjector` under the `testing` feature — wraps `InMemory`, fails the Nth call of a kind, records which methods were called. Handed straight to the `sync.rs` functions; no `CloudStorage` adapter needed. Deliberately minimal: a canned fault error rather than arbitrary variants, and only the call kinds the beta tests need.
6. [x] CS-4: `put` for empty and small objects, multipart above the threshold. Test: which branch ran, using the injector's call log — plus the in-memory empty-file round trip, commented as documenting intent rather than proving the fix.
7. [ ] MinIO: `FALCONERI_MINIO_TEST=1` plus a `just` target — empty file, ~30 MiB multipart, and a deep weird-named tree round trip. This is what actually proves CS-4; land it with that patch if convenient. **Caveat from CS-4:** object_store works around zero-part completions on S3 and GCS (see the §2.1 note), so this test now checks that the request shape we choose is legal and round-trips on a real server, not that a previously failing case now passes.
8. [x] CS-3: pure part-size function `clamp(ceil(size / 10_000), FLOOR, 5 GiB)` with **FLOOR = 16 MiB**, a policy choice above the 5 MiB service minimum (rationale in §1.2), property-tested as above and wired into the upload path. The read buffer is one part. Single-`put` uploads use the same threshold: a file that size can only ever produce one part. The function is total and backend-agnostic, so we do not reject an oversized file early: the real ceiling differs by backend (5 TiB on GCS, about 48.8 TiB on S3), and the function has no backend to ask. A clear early error would need a backend-aware layer, and is not scheduled.
9. [x] CS-2: bound in-flight parts with `wait_for_capacity(N)`, `N × chunk_size` inside the pod's memory budget. Untested on purpose (§2.4). **Two findings from CS-3.** The streaming read buffer is now exactly one part, so peak memory is about `(1 + N) × part_size`. And `part_size` grows with the file, up to the 5 GiB service maximum, so a fixed `N` is not a memory budget: we must either pick `N` from the computed part size or cap the part size below the service maximum, which lowers the largest object we can write.
   - **Decision: fixed `N = 2`** (`UPLOAD_PART_CONCURRENCY`), no adaptivity. Neither S3 nor GCS throttles bandwidth per request or per connection today, so concurrency beyond latency-hiding buys little. Even on an idle, burst-fed instance, CPU caps a 2-CPU pod near 2 Gbps, and two in-region streams reach that (worked example in §1.4). One in-flight part already pipelines disk reads over uploads; two survive a slow or retried request.
   - We accept that memory then scales with part size: 48 MiB for outputs up to 160 GiB, but ~1.6 GiB at the 5 TiB GCS maximum. Users of very large outputs allocate more container memory; needs a docs bullet (item 12).
10. [ ] CS-1: set `ClientOptions` at one shared construction site for S3 and GCS — `timeout` disabled plus a `read_timeout` for stall detection, and a deliberate `RetryConfig` review. Untested on purpose (§2.4); the guard is that it exists in exactly one place.
11. [ ] CS-6: best-effort `abort()` on upload error paths, logged; injector test asserting `abort` is reached. Normally §2.2 triage, but it rides along cheaply once the injector exists and real users are pushing data. Caveat: `WriteMultipart::finish()` already calls `abort()` itself when a part or `complete()` fails, so the injector test pins that upstream safety net rather than our own error handling. The part of CS-6 a store test cannot see—dropping the upload without calling `finish()` when the read loop errors—stays a code-review item.
12. [ ] Polish & docs:
    - Ops: lifecycle rule aborting incomplete multipart uploads on our buckets (S3 and the GCS equivalent), documented where bucket setup is documented.
    - Document worker memory sizing for large outputs: peak upload buffering is `3 × multipart_part_size(output size)` — 48 MiB up to 160 GiB, ~1.6 GiB at the 5 TiB GCS maximum.
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

- A typical 2-CPU pod sees an envelope near **100 MiB/s**: network baseline ~98 MiB/s, EBS baseline ~81 MiB/s — and on default gp3, scratch disk binds before S3. Idle or larger instances go higher, but CPU still caps a 2-CPU pod near 2 Gbps (§1.4 worked example).
- One stream already reaches that envelope using about **one core**. Adding streams adds CPU contention inside the pod and against sibling pods without raising the underlying baselines.
- So parallel ranges only pay off when a *single* object's transfer time is minutes — call it **≥10 GiB** — and even then the gain (maybe 2–4×) requires also moving scratch to instance-store or provisioned-throughput EBS, otherwise it is still disk-bound.

**Two decisions it would force, if we revisit them:** chunk size becomes a durable format decision (AWS recommends reading objects back aligned to the part sizes used on upload), and it must satisfy the part-number budget `≥ ceil(size / 10,000)`. Pick once, apply to both directions. We have now picked on the upload side: `multipart_part_size` gives 16 MiB up to 160 GiB, then `ceil(size / 10_000)` up to the 5 GiB ceiling. The part size is therefore a function of object size, not a constant, and an aligned reader must recompute the same function.

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

## 6. Patch workflow

This section records how this series is being implemented, so future
sessions (and eventually a project skill) keep the same process.

**Patches.** Linux-kernel-style: one focused thing per patch. Refactor
patches change no behavior and leave existing tests passing untouched;
behavior changes never mix with refactors. Each patch compiles and
passes `just check` on its own. Test-only patches that lock in current
behavior land *before* the patches that change it.

**jj mechanics.** Start each patch with `jj new`; finish with
`jj describe`—never `jj commit`, because the working copy stays
available for review-driven edits. The human may amend any commit
(message or content), particularly during their conversation turn.
The harness will not say so, so use `jj log`/`jj diff` if something
seems to have changed unexpectedly.

**Commit messages.** Use Simple-Technical-English (STE) prose, written for a
reviewer seeing the patch in isolation. The goal is to speed up human
comprehension and review, which is our more significant productivity
bottleneck.

General style rules for commit messages:

- Start with clear motivation—why the change exists—then describe what
  the patch does, concisely.
- Explain the goal. Do not enumerate every individual change.
  A reviewer should understand the reasoning, not just the diff.
- No incidental history: no process narration, no test-run logs, no
  “per review” bookkeeping, no record of dead-end designs.
- Stay succinct. If a paragraph is explaining your work on the patch
  rather than the patch itself, cut it.
- End with an `Assisted-by: LLM` trailer.

Here is a rough summary of some key rules from the STE standard, to
help you. STE was designed for aerospace manuals used in a multilingual
environment. These are not absolute limits, but guidelines to point you
towards a prose style that will be easy to read:

- Choose simple, unambiguous vocabulary.
- Use one instruction per sentence, and one topic per paragraph.
- Use a maximum of around 25 words per sentence.
- Use a maximum of 6 sentences per paragraph.
- Use numbered or vertical lists for complex sequences.
- Do not omit verbs, subjects or articles to shorten a sentence.
- Use active voice.
- Permitted verb forms: infinitive, imperative, simple present, simple
  past, simple future. Past participle may be used as an adjective.

**Plan updates.** Each patch checks off its §2.5 item in the same
commit. Caveats and findings that affect *later* patches go into the
plan, attached to the item they will affect—not in a commit message,
where a future implementer won't look.

**Test discipline.** Test logic we wrote—arithmetic, branching, error
propagation, round-trip identity—never constants we chose. Keep tests
non-vacuous: verify a test actually fails when the behavior breaks
(mutation-check it) before trusting it. Untested fixes are acceptable
when honest testing would cost more than it proves; say so in the plan.

**Scope.** Ruthlessly simplify toward the minimum the next patch needs;
extend later when a real need appears. When a design decision is
genuinely hard, use the collaborative-design process: explore options,
surface constraints and findings early, and leave the choices to the
human.
