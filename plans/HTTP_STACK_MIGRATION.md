# DONE: HTTP Stack Migration Plan

> **Status:** Done.

This document outlines a three-phase migration from the current Rocket-based HTTP stack to a modern axum-based stack.

## Current State

- **falconerid**: Rocket 0.5.0-rc.3 (pre-release), headers 0.3, axum 0.6 (unused)
- **falconeri_common**: reqwest 0.11 (blocking client)
- **Database**: diesel (sync)

## Goals

1. Remove dependency on Rocket pre-release
2. Align with modern hyper/tokio ecosystem (axum, reqwest 0.12, etc.)
3. Reduce dependency complexity
4. Prepare foundation for future async database migration
5. Eliminate OpenSSL dependencies for simpler builds

---

## Phase 1: Rocket → axum (sync handlers)

Migrate the web framework while keeping all business logic synchronous. This minimizes risk and allows us to verify the HTTP layer works before tackling async database concerns.

### 1.1 Update Dependencies

**falconerid/Cargo.toml changes:**

```toml
# Remove these:
# rocket = { version = "=0.5.0-rc.3", features = ["json", "uuid"] }
# rocket_codegen = "=0.5.0-rc.3"
# rocket_http = "=0.5.0-rc.3"
# axum = { version = "0.6.18", ... }  # unused, remove old version
# headers = "0.3.5"

# Add these:
axum = { version = "0.8", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["trace"] }
```

**falconeri_common/Cargo.toml changes:**

```toml
# Update:
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls-native-roots"] }
```

### 1.2 Migrate Route Handlers (main.rs)

**Before (Rocket):**
```rust
#[get("/jobs/<job_id>")]
fn get_job(
    _user: User,
    mut conn: DbConn,
    job_id: Uuid,
) -> FalconeridResult<Json<Job>> {
    let job = Job::find(job_id, &mut conn)?;
    Ok(Json(job))
}
```

**After (axum with spawn_blocking):**
```rust
async fn get_job(
    _user: User,
    State(pool): State<DbPool>,
    Path(job_id): Path<Uuid>,
) -> FalconeridResult<Json<Job>> {
    let job = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        Job::find(job_id, &mut conn)
    })
    .await
    .context("task panicked")??;
    Ok(Json(job))
}
```

### 1.3 Migrate Extractors (util.rs)

#### DbConn → DbPool State

**Before (Rocket FromRequest):**
```rust
#[rocket::async_trait]
impl<'r> FromRequest<'r> for DbConn {
    type Error = ();
    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        // Get pool from Rocket state, return connection
    }
}
```

**After (axum State extractor):**
```rust
// Just use State<DbPool> directly in handlers
// Pool initialization moves to main()
```

#### User Authentication

**Before (Rocket FromRequest + headers crate):**
```rust
impl<'r> FromRequest<'r> for User {
    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let auth = basic_auth_from_request(request)?;  // uses headers crate
        // validate credentials
    }
}
```

**After (axum extractor, manual base64 parsing):**
```rust
#[async_trait]
impl<S> FromRequestParts<S> for User
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let header = parts.headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "missing auth"))?;

        let (username, password) = parse_basic_auth(header)
            .ok_or((StatusCode::BAD_REQUEST, "invalid auth header"))?;

        // validate credentials against stored password
    }
}

fn parse_basic_auth(header: &str) -> Option<(String, String)> {
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = BASE64_STANDARD.decode(encoded).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (user, pass) = credentials.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}
```

#### FalconeridError

**Before (Rocket Responder):**
```rust
impl<'r, 'o: 'r> Responder<'r, 'o> for FalconeridError {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .sized_body(...)
            .status(Status::InternalServerError)
            .ok()
    }
}
```

**After (axum IntoResponse):**
```rust
impl IntoResponse for FalconeridError {
    fn into_response(self) -> Response {
        error!("{}", self.0.display_causes_without_backtrace());
        let body = format!("{}", self.0.display_causes_without_backtrace());
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}
```

### 1.4 Migrate Server Startup (main.rs)

**Before (Rocket):**
```rust
#[launch]
fn rocket() -> _ {
    initialize_tracing();
    rocket::build()
        .attach(DbConn::fairing())
        .attach(User::fairing())
        .mount("/", routes![...])
}
```

**After (axum):**
```rust
#[tokio::main]
async fn main() -> Result<()> {
    initialize_tracing();
    falconeri_common::init_openssl_probe();

    let pool = initialize_pool()?;
    let admin_password = db::postgres_password(ConnectVia::Cluster)?;

    let app = Router::new()
        .route("/version", get(version))
        .route("/jobs", post(post_job).get(get_job_by_name))
        .route("/jobs/:job_id", get(get_job))
        .route("/jobs/:job_id/retry", post(job_retry))
        .route("/jobs/:job_id/reserve_next_datum", post(job_reserve_next_datum))
        .route("/datums/:datum_id", patch(patch_datum))
        .route("/output_files", post(create_output_files).patch(patch_output_files))
        .with_state(AppState { pool, admin_password });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8089").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
```

### 1.5 Endpoints to Migrate

| Endpoint | Method | Handler | Notes |
|----------|--------|---------|-------|
| `/version` | GET | `version()` | Simple, no extractors |
| `/jobs` | POST | `post_job()` | JSON body |
| `/jobs` | GET | `get_job_by_name()` | Query param `?job_name=` |
| `/jobs/:job_id` | GET | `get_job()` | Path param |
| `/jobs/:job_id/retry` | POST | `job_retry()` | Path param |
| `/jobs/:job_id/reserve_next_datum` | POST | `job_reserve_next_datum()` | Path + JSON body |
| `/datums/:datum_id` | PATCH | `patch_datum()` | Path + JSON body |
| `/output_files` | POST | `create_output_files()` | JSON body |
| `/output_files` | PATCH | `patch_output_files()` | JSON body |

### 1.6 Testing Phase 1

1. `cargo check` - Verify compilation
2. `cargo test` - Run unit tests
3. `just check` - Full pre-commit validation
4. Manual testing with minikube:
   - Deploy and run proxy
   - Submit a test job
   - Verify worker communication
   - Check error handling

### 1.7 Phase 1 Success Criteria

- [ ] All Rocket code removed
- [ ] axum 0.8 serving all endpoints
- [ ] reqwest 0.12 working for client
- [ ] headers crate removed
- [ ] All existing tests pass
- [ ] Manual end-to-end test passes

---

## Phase 2: Async Database (Future)

After Phase 1 is stable, consider migrating to fully async database access. This is covered in `PROPOSED_DIESEL_MIGRATION.md`.

### 2.1 Why Wait?

- Phase 1 proves the HTTP layer works
- spawn_blocking is a valid long-term solution for sync DB
- diesel-async is a larger change affecting more code
- Can be done incrementally if needed

### 2.2 Phase 2 Scope (When Ready)

1. Add diesel-async dependency
2. Migrate connection pool to async pool
3. Remove spawn_blocking wrappers from handlers
4. Update model methods to async
5. Migrate reqwest from blocking to async client

### 2.3 Phase 2 Benefits

- No thread pool overhead for DB operations
- Better resource utilization under load
- Cleaner async/await code throughout
- Align with async ecosystem best practices

---

## Phase 3: Eliminate OpenSSL

After the HTTP stack is modernized, remove all OpenSSL dependencies in favor of pure-Rust TLS implementations.

### 3.1 Current OpenSSL Usage

Several crates currently depend on OpenSSL:

- `openssl-sys` - Explicit dependency in multiple Cargo.toml files
- Some dependencies may pull in OpenSSL transitively

One crate looks like it depends on OpenSSL, but might not:

- `openssl-probe` - Used to find system CA certificates. THIS SHOULD STAY. It doesn't actually link OpenSSL, and I believe that it works with rustls when doing musl builds.

### 3.2 Migration Steps

1. **Audit current OpenSSL usage:**
   ```bash
   cargo tree -i openssl-sys
   cargo tree -i openssl
   ```

2. **Switch to rustls everywhere:**
   - reqwest: Already using `rustls-tls-native-roots` feature
   - Verify no other crates pull in OpenSSL
   - Remove explicit `openssl-sys` dependencies from Cargo.toml files
   - Can we modify openssl-probe usage to stop setting env vars and to instead correctly initialize all rustls callers correctly?

3. **Update deny.toml to ban OpenSSL:**
   ```toml
   [bans]
   deny = [
       # Prefer pure-Rust TLS implementations
       { name = "openssl" },
       { name = "openssl-sys" },
   ]
   ```

4. **See if we can replace `ekidd/rust-musl-builder`** with either ordinary target MUSL builds, or `cargo cross` if necessary.
   - `ekidd/rust-musl-builder` is heavily deprecated, and is only used to work around OpenSSL and libpq issues.

### 3.3 Benefits

- **Simpler builds:** No need to install OpenSSL dev libraries
- **Easier cross-compilation:** Pure Rust compiles anywhere
- **Smaller attack surface:** Fewer C dependencies
- **Faster CI:** No OpenSSL installation step
- **Better musl/static builds:** No dynamic linking issues

### 3.4 Phase 3 Success Criteria

- [x] `cargo tree -i openssl-sys` returns nothing
- [x] `cargo tree -i openssl` returns nothing
- [x] deny.toml bans OpenSSL crates
- [x] `cargo deny check` passes
- [ ] Static musl builds work without OpenSSL (needs testing)

---

## Rollback Plan

If any phase encounters blocking issues:

1. Keep changes on a feature branch
2. Document specific blockers
3. Consider alternative approaches:
   - Phase 1: Upgrade Rocket to 0.5.1 stable instead
   - Phase 3: Keep OpenSSL if a dependency requires it

---

## Files Changed Summary

### Phase 1
| File | Changes |
|------|---------|
| `falconerid/Cargo.toml` | Remove rocket/headers, add axum/tokio/tower |
| `falconerid/src/main.rs` | Router setup, handler signatures, startup |
| `falconerid/src/util.rs` | Extractors, error handling |
| `falconeri_common/Cargo.toml` | reqwest 0.11 → 0.12 |

### Phase 2
| File | Changes |
|------|---------|
| `falconeri_common/Cargo.toml` | Add diesel-async |
| `falconeri_common/src/db.rs` | Async pool |
| `falconeri_common/src/models/*.rs` | Async methods |
| `falconerid/src/main.rs` | Remove spawn_blocking |

### Phase 3
| File | Changes |
|------|---------|
| `*/Cargo.toml` | Remove openssl-sys |
| `falconeri_common/src/lib.rs` | Remove init_openssl_probe() |
| `deny.toml` | Add OpenSSL to ban list |
