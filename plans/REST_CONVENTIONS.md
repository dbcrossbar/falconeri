# REST API Convention Migration

> **Status:** Done. I think.

This document describes the migration of falconeri's REST API to follow Rails-style conventions for request and response bodies.

## Goals

1. **Consistency**: All resource endpoints follow the same pattern
2. **Extensibility**: Wrapper objects allow adding metadata without breaking changes
3. **Pod ownership verification**: Worker endpoints include `pod_name` for zombie detection

## References

- [Rails API Guide](https://guides.rubyonrails.org/api_app.html)
- [JSON:API Specification](https://jsonapi.org/format/)
- [ActiveModel::Serializers::JSON](https://api.rubyonrails.org/classes/ActiveModel/Serializers/JSON.html)

---

## Phase 1: Rails Convention Migration

### Convention Summary

| Type | Pattern | Example |
|------|---------|---------|
| Single resource response | `{ "resource": {...} }` | `{ "job": {...} }` |
| Collection response | `{ "resources": [...] }` | `{ "jobs": [...] }` |
| Single resource request | `{ "resource": {...} }` | `{ "job": {...} }` |
| Collection request | `{ "resources": [...] }` | `{ "output_files": [...] }` |

### Changed Endpoints

#### Responses

| Method | Endpoint | Before | After |
|--------|----------|--------|-------|
| GET | `/jobs/list` | `[{...}, {...}]` | `{ "jobs": [{...}, {...}] }` |
| GET | `/jobs/{id}` | `{...}` | `{ "job": {...} }` |
| GET | `/jobs?job_name=` | `{...}` | `{ "job": {...} }` |
| POST | `/jobs` | `{...}` | `{ "job": {...} }` |
| POST | `/jobs/{id}/retry` | `{...}` | `{ "job": {...} }` |
| PATCH | `/datums/{id}` | `{...}` | `{ "datum": {...} }` |
| POST | `/output_files` | `[{...}, ...]` | `{ "output_files": [{...}, ...] }` |

#### Requests

| Method | Endpoint | Before | After |
|--------|----------|--------|-------|
| POST | `/jobs` | `{pipeline...}` | `{ "job": {pipeline...} }` |
| PATCH | `/datums/{id}` | `{status, output, ...}` | `{ "datum": {status, output, ...} }` |
| POST | `/output_files` | `[{job_id, datum_id, uri}, ...]` | `{ "output_files": [{job_id, datum_id, uri}, ...] }` |
| PATCH | `/output_files` | `[{id, status}, ...]` | `{ "output_files": [{id, status}, ...] }` |

### Unchanged Endpoints

These endpoints already follow conventions or have no body:

| Method | Endpoint | Reason |
|--------|----------|--------|
| GET | `/version` | Returns plain string, not a resource |
| POST | `/jobs/{id}/reserve_next_datum` | RPC-style, not a resource CRUD |
| GET | `/jobs/{id}/describe` | Already composite: `{job, datum_status_counts, ...}` |
| GET | `/datums/{id}/describe` | Already composite: `{datum, input_files}` |
| PATCH | `/output_files` | Returns 204 No Content (no body) |

---

## Phase 2: Pod Ownership Verification

Add `pod_name` to worker endpoints for ownership verification and zombie detection.

### Affected Endpoints

| Method | Endpoint | Change |
|--------|----------|--------|
| PATCH | `/datums/{id}` | Add `pod_name` field at top level |
| POST | `/output_files` | Add `pod_name` field at top level |
| PATCH | `/output_files` | Add `pod_name` field at top level |

### Request Format

The `pod_name` field is request metadata (not part of the resource), so it goes at the top level alongside the resource wrapper:

```json
// PATCH /datums/{id}
{
  "pod_name": "worker-abc123",
  "datum": {
    "status": "done",
    "output": "..."
  }
}

// POST /output_files
{
  "pod_name": "worker-abc123",
  "output_files": [
    { "job_id": "...", "datum_id": "...", "uri": "s3://..." }
  ]
}

// PATCH /output_files
{
  "pod_name": "worker-abc123",
  "output_files": [
    { "id": "...", "status": "done" }
  ]
}
```

### Server Behavior

When `pod_name` doesn't match the datum's recorded owner:

1. Log error with structured fields:
   ```
   error!(
     datum = %datum_id,
     pod_name = %request.pod_name,
     conflicting_pod_name = ?datum.pod_name,
     "Pod ownership mismatch - possible zombie worker"
   )
   ```

2. Return HTTP 403 Forbidden:
   ```
   datum {datum_id} is owned by {actual_pod_name}, not {requesting_pod_name}
   ```

This causes zombie workers to fail fast rather than retrying forever.

### Endpoints Already Having pod_name

| Method | Endpoint | Notes |
|--------|----------|-------|
| POST | `/jobs/{id}/reserve_next_datum` | Already receives and stores `pod_name` |

---

## Implementation Files

| File | Changes |
|------|---------|
| `falconeri_common/src/rest_api.rs` | Request/response wrapper types, client methods |
| `falconerid/src/main.rs` | Endpoint handlers with new types |
| `falconeri/src/cmd/*.rs` | CLI commands using updated client |
| `falconeri-worker/src/main.rs` | Worker calls with pod_name |

---

## Migration Notes

- Worker and falconerid are deployed together (same image build), so no backwards compatibility needed
- CLI (`falconeri`) is typically updated alongside server deployments
- All changes can be made atomically in a single release
