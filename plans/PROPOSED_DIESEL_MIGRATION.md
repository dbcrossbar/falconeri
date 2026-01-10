# DONE: Proposed Migration: diesel → diesel-async

> **Status:** Done.

## Motivation

The current diesel setup requires `libpq` (the PostgreSQL C client library), which in turn pulls in `libopenssl`. This creates several problems:

1. **Development setup friction**: Developers must install `libpq` via their system package manager (`brew install libpq` on macOS)
2. **Cross-compilation complexity**: Building static Linux binaries requires cross-compiling C libraries
3. **Security surface**: C dependencies add potential memory safety issues
4. **Build reproducibility**: System library versions can vary

**diesel-async** uses `tokio-postgres` under the hood, which is a pure-Rust PostgreSQL implementation with no C dependencies.

## Scope of Changes

### Files Requiring Modification

| File | Changes Required | Complexity |
|------|-----------------|------------|
| `falconeri_common/src/db.rs` | Async pool, async connect, async migrations | High |
| `falconeri_common/src/models/job.rs` | Add async/await to all queries | Medium |
| `falconeri_common/src/models/datum.rs` | Async transactions, locking queries | High |
| `falconeri_common/src/models/input_file.rs` | Async batch inserts | Low |
| `falconeri_common/src/models/output_file.rs` | Async batch updates | Low |
| `falconerid/src/main.rs` | Async Rocket handlers | Medium |
| `falconerid/src/util.rs` | Async DbConn extractor | High |
| `falconerid/src/start_job.rs` | Async transactions | Medium |
| `falconerid/src/babysitter.rs` | Convert thread → tokio task | High |

### What Stays the Same

- All diesel schema definitions (`schema.rs`)
- All model struct definitions
- All query builder DSL (just add `.await`)
- Database migrations SQL files

## Dependency Changes

```toml
# BEFORE (falconeri_common/Cargo.toml)
diesel = { version = "2.0.4", features = ["chrono", "postgres", "r2d2", "serde_json", "uuid"] }
diesel_migrations = "2.0.0"
r2d2 = "0.8.4"

# AFTER
diesel = { version = "2.2", default-features = false, features = ["chrono", "postgres_backend", "serde_json", "uuid"] }
diesel-async = { version = "0.5", features = ["postgres", "deadpool"] }
diesel_migrations = "2.2"
# Remove: r2d2, pq-sys (transitive)
```

Note: `postgres_backend` feature enables diesel's PostgreSQL types without requiring `libpq`.

## Migration Phases

### Phase 1: Preparation (non-breaking)
1. Upgrade diesel to 2.2.x (latest)
2. Ensure all tests pass with current sync implementation
3. Add `tokio` runtime to `falconerid` if not already present

### Phase 2: Add diesel-async alongside diesel
1. Add `diesel-async` dependency
2. Create parallel async versions of `db.rs` functions
3. Keep sync versions working during transition

### Phase 3: Convert falconeri_common models
1. Convert model methods to async one file at a time
2. Start with simpler models (`input_file.rs`, `output_file.rs`)
3. Then convert complex models (`job.rs`, `datum.rs`)

### Phase 4: Convert falconerid
1. Update `DbConn` to use async pool
2. Convert Rocket handlers to async
3. Convert babysitter from thread to tokio task

### Phase 5: Cleanup
1. Remove sync connection code
2. Remove `r2d2` dependency
3. Update documentation
4. Verify `cargo test` works without `libpq` installed

## Code Change Examples

### Connection Pool (db.rs)

```rust
// BEFORE
pub type Pool = r2d2::Pool<DieselConnectionManager<PgConnection>>;

pub fn pool(connect_via: ConnectVia) -> Result<Pool> {
    let manager = DieselConnectionManager::new(database_url);
    r2d2::Pool::builder().max_size(pool_size).build(manager)
}

// AFTER
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::AsyncPgConnection;

pub type Pool = deadpool::Pool<AsyncPgConnection>;

pub async fn pool(connect_via: ConnectVia) -> Result<Pool> {
    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
    Pool::builder(config).max_size(pool_size).build()
}
```

### Model Query (job.rs)

```rust
// BEFORE
pub fn find(id: Uuid, conn: &mut PgConnection) -> Result<Job> {
    jobs::table.find(id).first(conn)
        .with_context(|| format!("could not load job {}", id))
}

// AFTER
pub async fn find(id: Uuid, conn: &mut AsyncPgConnection) -> Result<Job> {
    jobs::table.find(id).first(conn).await
        .with_context(|| format!("could not load job {}", id))
}
```

### Transaction (datum.rs)

```rust
// BEFORE
conn.transaction(|conn| {
    // ... queries ...
    Ok(result)
})

// AFTER
conn.transaction(|conn| async move {
    // ... queries with .await ...
    Ok(result)
}.scope_boxed()).await
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Rocket 0.5 async compatibility | Medium | High | Test thoroughly; may need Rocket upgrade |
| Transaction semantics differ | Low | High | Careful testing of locking behavior |
| Performance regression | Low | Medium | Benchmark critical paths |
| Migration runtime issues | Low | High | Test migrations in staging first |

## Open Questions

1. **Rocket version**: Should we upgrade Rocket to a newer version with better async support?
2. **Connection pool**: deadpool vs bb8 for async pooling?
3. **Migration timing**: Run migrations sync at startup, or convert to async?

## Success Criteria

- [ ] `cargo build` succeeds without `libpq` installed
- [ ] `cargo test` passes (may need test database setup)
- [ ] All existing functionality works unchanged
- [ ] No performance regression in critical paths (datum reservation, job status updates)

## References

- [diesel-async documentation](https://docs.rs/diesel-async/latest/diesel_async/)
- [diesel-async GitHub](https://github.com/weiznich/diesel_async)
- [tokio-postgres](https://docs.rs/tokio-postgres/latest/tokio_postgres/)
- [Diesel compare page](https://diesel.rs/compare_diesel.html)
