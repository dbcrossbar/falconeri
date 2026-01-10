# Duplicate Output File Error Analysis

> **Status:** We have implemented logging and locks to try to prevent this, and to better report any future failures.

## The Error

```
ERROR: could not upload outputs
  caused by: unexpected HTTP status 500 Internal Server Error for http://falconerid:8089/output_files:
ERROR: error inserting datums
  caused by: duplicate key value violates unique constraint "output_files_job_id_uri_key"
```

---

# Executive Summary

This error occurs when a worker tries to create output_file records that already exist. After extensive analysis, **Kubernetes-level split-brain (zombie workers)** is the leading hypothesis that explains failures even with `datum_tries >= 2`.

## Production Evidence

From gist (2025-12-06):
- **Datum ID:** `52b6db42-a8b0-4de1-bc30-0630b09baab4`
- **Job ID:** `ccee0990-7ed1-4bed-a1a7-3140d67bab4b`
- **attempted_run_count:** 2
- **maximum_allowed_run_count:** 2
- **Node:** `gke-prod-gke-persistent3-6826b181-wd2m`
- **Pod:** `geocode-requested-ahlxrdqj85-lfdhf`

This confirms the bug occurs even with retries enabled (`datum_tries=2`).

---

# Kubernetes Split-Brain: Research Findings

## It's a Real Phenomenon

Kubernetes split-brain is a documented issue where pods continue running on nodes that have become unreachable to the API server. Key findings:

### Default Timing (from [Kubernetes Nodes docs](https://kubernetes.io/docs/concepts/architecture/nodes/), [AWS docs](https://docs.aws.amazon.com/prescriptive-guidance/latest/ha-resiliency-amazon-eks-apps/pod-eviction-time.html))

| Event | Default Timing |
|-------|---------------|
| Node marked `ConditionUnknown` | 40 seconds |
| Pod eviction timeout (default toleration) | 5 minutes |
| **Total before eviction attempt** | **~5-6 minutes** |

### Critical Insight

From [Akka Management Issue #156](https://github.com/akka/akka-management/issues/156):

> "Replicas that are left on unreachable nodes will keep running, until there is a connection with API Server, and then kubelet on that node would stop evicted containers."

The eviction decision **cannot be communicated** to the kubelet if the node is partitioned. Pods continue running indefinitely until the node reconnects. If it never reconnects, pods must be manually deleted.

### How This Affects Falconeri

Our babysitter checks every 2 minutes. If a node becomes unreachable:

1. **0-5 min**: K8s still thinks pod might be okay
2. **~5 min**: K8s decides to evict, but can't reach node
3. **Meanwhile**: Babysitter sees pod missing from `kubectl get pods`, marks datum as error
4. **If `datum_tries >= 2`**: Babysitter deletes output_files, marks datum ready
5. **New worker (Worker B) reserves datum, starts processing**
6. **Original worker (Worker A, still running on partitioned node!)** eventually tries to create output_files
7. **DUPLICATE KEY** - Worker A or Worker B hits the constraint violation

This timeline is plausible for the bug and explains why it occurs even with `datum_tries=2`.

### References

- [Akka Management Issue #156 - Split-brain scenario](https://github.com/akka/akka-management/issues/156)
- [AWS - Configure pod eviction time](https://docs.aws.amazon.com/prescriptive-guidance/latest/ha-resiliency-amazon-eks-apps/pod-eviction-time.html)
- [Kubernetes - Nodes documentation](https://kubernetes.io/docs/concepts/architecture/nodes/)
- [DEV Community - How Kubernetes handles offline nodes](https://dev.to/duske/how-kubernetes-handles-offline-nodes-53b5)

---

# Confirmed Issues

## Issue 1: No Pod Ownership Verification

**Location:** `falconerid/src/main.rs` (output_file endpoints), worker code

**Problem:** When workers call `create_output_files` or `patch_output_files`, the server doesn't verify that the requesting pod still owns the datum. A zombie worker can interfere with a new worker's datum.

**Current worker flow (no ownership checks):**
```
1. Reserve datum              - updates pod_name in DB
2. Process locally            - no DB check
3. create_output_files        - no datum lock, no ownership check
4. Upload to S3/GCS           - no DB check
5. patch_output_files         - no datum lock, no ownership check
6. Mark datum done            - no ownership check
```

**Fix required:** New protocol for all datum/output_file operations (see Recommended Fixes).

## Issue 2: No `pod_name` in `output_files` Table

**Location:** `migrations/2018-07-06-112750_create_job_and_datum/up.sql:44-55`

The `output_files` table has no `pod_name` column, making it impossible to:
- Determine which worker created a record
- Detect split-brain scenarios
- Debug failed babysitter cleanups
- Provide informative error messages

**Fix required:** Add `pod_name` column to `output_files`.

## Issue 3: No Datum Locking During Output File Operations

**Location:** `falconerid/src/main.rs:354-361` and `falconerid/src/main.rs:366-399`

The `create_output_files` and `patch_output_files` endpoints don't lock the datum row. This means:
- Babysitter could delete output_files while worker is in the middle of operations
- Multiple workers (in a split-brain scenario) could race on the same datum's output_files

**Fix required:** Lock the datum row during all output_file operations.

## Issue 4: `create_output_files` Not Idempotent

**Location:** `falconeri_common/src/models/output_file.rs:136-153`

The insert uses a plain `INSERT` without `ON CONFLICT`, causing duplicate key errors on retry.

**Fix required:** Use `INSERT ... ON CONFLICT` with pod_name verification.

## Issue 5: Retry Timeout Regression (NEW CODE ONLY)

**Location:** `connect_via.rs:38-43`

**What happened:** During migration from `backoff` to `backon` crate (commit `18d1e26`), the implicit 15-minute timeout was lost.

| Aspect | Old (`backoff` 0.4.0) | New (`backon`) |
|--------|----------------------|----------------|
| Timeout | 15 minutes (implicit default) | **None** (`.without_max_times()`) |
| Effect | Eventually gives up | Retries forever |

**Impact:** Workers hitting any permanent error will spin forever instead of failing after 15 minutes.

**Fix required:** Restore timeout behavior in new code.

## Issue 6: Insufficient Logging for Debugging

**Location:** `falconerid/src/main.rs`

Current issues:
- No HTTP request tracing (no `tower_http::TraceLayer`)
- No structured logging with datum/job/pod_name fields on output_file operations
- Cannot correlate events across workers and server

**Fix required:** Add comprehensive tracing with `datum=`, `job=`, `pod_name=`, `conflicting_pod_name=` fields.

---

# Working Hypotheses

## Hypothesis A: Kubernetes Split-Brain Zombie Worker (LEADING)

**Probability:** High
**Compatible with datum_tries=2:** Yes

**Mechanism:**
1. Worker A processing datum (attempt 1)
2. Worker A's node becomes unreachable (network partition, node issue)
3. K8s can't communicate eviction to kubelet - Worker A keeps running
4. Babysitter sees pod missing, marks datum as error
5. Babysitter deletes output_files, marks datum ready for retry
6. Worker B reserves datum (attempt 2)
7. Worker B creates output_files successfully
8. Worker A (still running!) finally tries to create its output_files
9. **DUPLICATE KEY** - Worker B's records exist
10. Worker A keeps retrying until timeout, logging errors

**Why this fits:**
- Explains failures with `datum_tries=2`
- Consistent with K8s behavior during network partitions
- Would not be detected by current code (no ownership verification)

**Evidence needed:**
- Babysitter logs showing zombie detection for a datum
- Logs showing two different pod_names attempting operations on same datum
- K8s events showing node unreachable around failure time

## Hypothesis B: HTTP Retry on Lost Response

**Probability:** Medium
**Compatible with datum_tries=2:** Partially (would need to happen on both attempts)

**Mechanism:**
1. Worker calls `create_output_files`
2. Server inserts successfully
3. HTTP response lost (network glitch)
4. Worker retries → DUPLICATE KEY
5. All retries fail with same error
6. After timeout, datum marked as error

**Analysis:** Could explain a single failure, but unlikely to hit both attempts of the same datum.

## Hypothesis C: Worker OOM Mid-Upload

**Probability:** Medium
**Compatible with datum_tries=2:** Yes, if cleanup race exists

**Mechanism:**
1. Worker creates output_file records
2. Worker starts uploading to S3/GCS (gcloud CLI can use 5GB RAM!)
3. Worker OOM-killed mid-upload
4. Output_file records exist but uploads incomplete
5. Babysitter marks datum as error
6. Race: new worker starts before babysitter deletes output_files
7. **DUPLICATE KEY**

**Evidence needed:**
- Memory usage graphs showing OOM around failure time
- Timeline correlation between worker death and retry

---

# Schema & Design Context

## output_files Table

```sql
CREATE TABLE output_files (
    id uuid PRIMARY KEY,
    created_at timestamp NOT NULL DEFAULT now(),
    updated_at timestamp NOT NULL DEFAULT now(),
    status status NOT NULL DEFAULT 'running',
    job_id uuid NOT NULL REFERENCES jobs(id),
    datum_id uuid NOT NULL REFERENCES datums(id),
    uri text NOT NULL,
    -- NOTE: No pod_name column!
    UNIQUE (job_id, uri)  -- Prevents clobbering between datums
);
```

## Worker Upload Flow (Current - No Ownership Checks)

```
1. create_output_files  →  Insert records with status='running' (placeholder)
2. sync_up to S3/GCS    →  Actually upload files (uses gcloud CLI - can OOM!)
3. patch_output_files   →  Update status to 'done' or 'error'
```

**Note:** No datum lock is held during steps 1-3. No ownership verification.

## Babysitter Cleanup Flow

```
Phase 1: check_for_zombie_datums (every 2 min)
  - Finds datums with status='running' but pod disappeared
  - Marks datum as 'error'
  - Does NOT delete output_file records

Phase 2: check_for_datums_which_can_be_rerun (every 2 min)
  - Finds datums with status='error' AND attempted_run_count < maximum_allowed_run_count
  - Locks datum row (SELECT FOR UPDATE)
  - Deletes output_file records for datum
  - Marks datum as 'ready' for retry
```

---

# Recommended Fixes

## Priority 1: New Protocol for Datum/Output File Operations

All operations that modify datums or output_files must follow this protocol:

### Server-Side (falconerid)

1. **Always lock the datum row** using `SELECT FOR UPDATE`
2. **Verify pod_name matches** the expected worker
3. **If pod_name doesn't match:** Log with `error!()` including:
   - `datum=` (the datum ID)
   - `pod_name=` (the expected pod from the request)
   - `conflicting_pod_name=` (the actual pod_name in the database)
4. **Return appropriate error** to the worker

### REST API Changes

Modify worker-facing endpoints to require `pod_name` parameter:

```rust
// New request type for output file operations
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateOutputFilesRequest {
    pub pod_name: String,
    pub output_files: Vec<NewOutputFile>,
}

// Updated endpoint
async fn create_output_files(
    _user: User,
    DbConn(mut conn): DbConn,
    Json(request): Json<CreateOutputFilesRequest>,
) -> FalconeridResult<Json<Vec<OutputFile>>> {
    let datum_id = request.output_files.first()
        .map(|f| f.datum_id)
        .ok_or_else(|| format_err!("empty output_files list"))?;

    conn.transaction(|conn| {
        async move {
            // 1. Lock the datum
            let mut datum = Datum::find(datum_id, conn).await?;
            datum.lock_for_update(conn).await?;

            // 2. Verify ownership
            if datum.pod_name.as_deref() != Some(&request.pod_name) {
                error!(
                    datum = %datum_id,
                    pod_name = %request.pod_name,
                    conflicting_pod_name = ?datum.pod_name,
                    "Pod ownership mismatch - possible zombie worker"
                );
                return Err(format_err!(
                    "datum {} is owned by {:?}, not {}",
                    datum_id, datum.pod_name, request.pod_name
                ));
            }

            // 3. Proceed with insert
            let created = NewOutputFile::insert_all(&request.output_files, conn).await?;
            Ok(created)
        }
        .scope_boxed()
    }).await
}
```

### Apply to All Worker Endpoints

This pattern should apply to:
- `POST /output_files` (create_output_files)
- `PATCH /output_files` (patch_output_files)
- `PATCH /datums/{datum_id}` (patch_datum / mark_as_done / mark_as_error)

## Priority 2: Add `pod_name` to `output_files` Table

```sql
ALTER TABLE output_files ADD COLUMN pod_name text;
CREATE INDEX output_files_pod_name ON output_files (pod_name);
```

Update `NewOutputFile` to include `pod_name`. While not strictly necessary for correctness (the datum lock provides that), it provides:
- Audit trail for debugging
- Ability to detect historical issues
- Extra safety layer

## Priority 3: Add Comprehensive Tracing

### HTTP Request Tracing

Add `tower_http::TraceLayer` to the Axum router:

```rust
use tower_http::trace::TraceLayer;

let app = Router::new()
    // ... routes ...
    .layer(TraceLayer::new_for_http())
    .layer(RequestBodyLimitLayer::new(52_428_800))
    .with_state(state);
```

### Structured Logging

Add `#[instrument]` with relevant fields to all critical operations:

```rust
#[instrument(skip_all, fields(
    datum = %datum_id,
    pod_name = %request.pod_name,
    file_count = request.output_files.len()
), level = "info")]
async fn create_output_files(...) { ... }
```

Key fields to always include:
- `datum=` - datum UUID
- `job=` - job UUID
- `pod_name=` - requesting pod
- `conflicting_pod_name=` - when ownership mismatch detected

This enables querying logs by datum ID to see all related events.

## Priority 4: Make `create_output_files` Idempotent

Once we have the datum lock and pod_name verification, we can also make the insert idempotent for extra safety:

```sql
INSERT INTO output_files (job_id, datum_id, uri, pod_name)
VALUES ($1, $2, $3, $4)
ON CONFLICT (job_id, uri) DO UPDATE SET
    updated_at = now()
WHERE output_files.pod_name = EXCLUDED.pod_name
RETURNING *;
```

If conflict with different `pod_name`, the `WHERE` clause prevents the update and we can detect this.

## Priority 5: Restore Retry Timeout (NEW CODE)

```rust
fn backoff_config() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(500))
        .with_jitter()
        .with_max_times(20)  // ~15 min with exponential backoff
}
```

---

# Evidence That Would Help

## To confirm split-brain hypothesis:
- [ ] Babysitter logs showing zombie detection for a specific datum
- [ ] Logs showing two different pods operating on the same datum
- [ ] K8s events showing node unreachable around failure time
- [ ] Timeline: zombie detection → output_file deletion → new worker start → error

## Database queries:

```sql
-- Find orphaned output_files (datum is error/done but output_file status is running)
SELECT of.*, d.status as datum_status, d.attempted_run_count, d.maximum_allowed_run_count
FROM output_files of
JOIN datums d ON of.datum_id = d.id
WHERE of.status = 'running' AND d.status IN ('error', 'done');

-- Find datums that hit the duplicate key error (have output_files but status=error)
SELECT d.*, COUNT(of.id) as output_file_count
FROM datums d
JOIN output_files of ON of.datum_id = d.id
WHERE d.status = 'error'
GROUP BY d.id;

-- Check specific datum from production error
SELECT * FROM datums WHERE id = '52b6db42-a8b0-4de1-bc30-0630b09baab4';
SELECT * FROM output_files WHERE datum_id = '52b6db42-a8b0-4de1-bc30-0630b09baab4';
```

---

# Key Code Locations

| Component | Location |
|-----------|----------|
| Unique constraint | `migrations/2018-07-06-112750_create_job_and_datum/up.sql:54` |
| Output file insertion | `falconeri_common/src/models/output_file.rs:136-153` |
| REST endpoint (create) | `falconerid/src/main.rs:354-361` |
| REST endpoint (patch) | `falconerid/src/main.rs:366-399` |
| REST client retry | `falconeri_common/src/rest_api.rs:371-393` |
| Retry configuration | `falconeri_common/src/connect_via.rs:38-43` |
| Babysitter zombie detection | `falconerid/src/babysitter.rs:126-168` |
| Babysitter datum re-run | `falconerid/src/babysitter.rs:172-222` |
| Babysitter output_file delete | `falconerid/src/babysitter.rs:214` |
| OutputFile::delete_for_datum | `falconeri_common/src/models/output_file.rs:41-50` |
| Rerunable check | `falconeri_common/src/models/datum.rs:97-120` |
| Default datum_tries | `falconerid/src/start_job.rs:54` |

---

# Historical Reference

Old code (v1.0.0-beta.12) preserved in `.old-code-reference/` for comparison.

## Old vs New Retry Behavior

| Aspect | Old (`backoff` 0.4.0) | New (`backon`) |
|--------|----------------------|----------------|
| Crate | `backoff` | `backon` |
| Timeout | 15 min (implicit) | None (regression) |
| Error classification | Partial (unused) | None |
| Exponential backoff | Yes | Yes |
| Jitter | Yes | Yes |

The regression was introduced in commit `18d1e26` ("UNTESTED: Migrate from backoff to backon").

---

# Summary: Root Cause Theory

The most likely explanation for the duplicate key error, especially with `datum_tries=2`:

1. **Kubernetes network partition** causes a worker's node to become unreachable
2. **Babysitter detects "zombie"** (pod not visible via kubectl) and marks datum as error
3. **Babysitter cleans up** output_files and marks datum ready for retry
4. **New worker reserves datum** and begins processing
5. **Original worker is still running** (K8s can't deliver eviction notice)
6. **Both workers race** to create output_files
7. **Duplicate key violation** occurs

The fix is to:
1. **Lock the datum** during all output_file operations
2. **Verify pod ownership** before allowing modifications
3. **Log conflicts clearly** with both pod names for debugging
