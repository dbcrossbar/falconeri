# `falconeri` Overview

`falconeri` is a Kubernetes-based distributed job runner. It allows you to use Docker images to transform large data files stored in cloud buckets.

**Main components:**

- `falconeri` - CLI client for managing jobs and deployments
- `falconerid` - Backend server that runs on Kubernetes, provides REST API
- `falconeri-worker` - Lightweight worker process that runs in K8s pods

## Source Tree

This is a Cargo workspace with multiple crates:

- `falconeri/` - CLI client
    - `src/cmd/` - Command implementations (deploy, job, proxy, migrate, etc.)
    - `src/cmd/deploy_manifest.yml.hbs` - Kubernetes deployment template
- `falconerid/` - Backend server
    - `src/main.rs` - Axum REST API server
    - `src/babysitter.rs` - Monitors running jobs
    - `src/start_job.rs` - Creates K8s batch jobs from pipeline specs
- `falconeri-worker/` - Worker process
    - `src/main.rs` - Reserves datums, processes work, uploads results
- `falconeri_common/` - Shared library
    - `src/models/` - Database models (Job, Datum, InputFile, OutputFile)
    - `src/kubernetes.rs` - kubectl wrapper and K8s utilities
    - `src/storage/` - Cloud storage backends (S3, GCS)
    - `src/pipeline.rs` - Pipeline specification types
    - `src/db.rs` - PostgreSQL/Diesel database connections
    - `src/rest_api.rs` - HTTP client for falconerid
- `guide/` - mdBook documentation (see "Guide Documentation" below)
- `examples/word-frequencies/` - Example pipeline for testing
- `Justfile` - Development commands
- `deny.toml` - License and policy file for `cargo deny`

## Basic Theory

- **Pipeline specs (JSON)** define batch jobs with input/output URIs, Docker images, and commands.
- **Jobs** contain multiple **datums** (individual work items).
- **Workers** run in K8s pods, communicate with `falconerid` via REST API to reserve and complete datums.
  - Workers are started by making a REST request to `falconerid`, which creates and manages K8s batch jobs.
- **Cloud storage** integration with S3 and Google Cloud Storage for inputs/outputs.
- **Status flow**: `Ready` → `Running` → `Done` | `Error` | `Canceled`.

## Useful Commands

Making sure code is correct:

- `cargo check`: Check syntax quickly. Use after a set of changes.
- `cargo test`: Run unit tests. Use after `cargo check` passes.
- `just check`: Run pre-commit checks (fmt, deny, clippy, test). Use before committing.

Getting docs:

- `cargo run -p falconeri -- --help`: Shows available subcommands
- `cargo run -p falconeri -- job --help`: Shows job subcommand options
- `cargo run -p falconeri -- deploy --help`: Shows deploy options

To get more debug information, set `RUST_LOG` before running:

- `RUST_LOG=falconeri=debug,falconerid=debug,falconeri-worker=debug,falconeri_common=debug,warn` for detailed logging
- `RUST_LOG=falconeri=trace,falconerid=trace,falconeri-worker=trace,falconeri_common=trace,warn` for very verbose output

You can omit the binaries that you're not testing. Also note that getting environment variables to `falconeri-worker` will require going through Kubernetes. (We should document this the first time we figure it out.)

## Docker and Kubernetes

For local development setup (Colima on macOS, minikube on Linux), see the [Local Development](guide/src/local.md) guide.

### Docker context (run before `just image`)

- macOS: `docker context use colima` (persistent, in theory)
- Linux: `eval $(minikube docker-env)` (run in each terminal)

### Key commands

- `just static-bin`: Build static musl binaries to `target/x86_64-unknown-linux-musl/debug/`
- `just image`: Build the Docker image (depends on static-bin).
    - **IMPORTANT:** This creates images on the local Docker daemon with tags that _look_ like remote registry tags. But local deployment uses `imagePullPolicy: Never` to avoid pulling from a registry.
- `cargo run -p falconeri -- deploy --development`: Deploy in development mode
- `cargo run -p falconeri -- proxy`: Create proxy connection to cluster
- `cargo run -p falconeri -- migrate`: Run database schema migrations
- `kubectl get all`: Check cluster status (very important after deploying)
- `cargo run -p falconeri -- job run <pipeline-spec.json>`: Run a job
- `cargo run -p falconeri -- job wait <job-id>`: Wait for job to complete
- `cargo run -p falconeri -- job list`: List recent jobs
- `cargo run -p falconeri -- job describe <job-id>`: Describe a job
    - Lists currrently running and failed datums only, for brevity.
- `cargo run -p falconeri -- datum describe <datum-id>`: Describe a datum

## End-to-End Testing

Once local Kubernetes is set up and `falconeri` has been deployed to it (see [Local Development](guide/src/local.md) and [Building and Running](guide/src/local/running.md)), you can run end-to-end tests using the word-frequencies example:

### Quick Test (from repo root)

In a separate terminal, or as a background task, start the proxy (keep running):

```sh
cargo run -p falconeri -- proxy
```

The `word-frequencies` example needs one-time setup in its directory:

```sh
cd examples/word-frequencies         # Make sure you're in the example dir
just mc-alias                        # Configure MinIO CLI (one-time)
just upload                          # Upload test data (one-time)
```

If you modify `falconerid` code, you can run the following at the top level to restart a previously deployed `falconerid` with the new images:

```sh
just image                                     # Rebuild Docker image
kubectl rollout restart deployment/falconerid  # Redeploy to pick up changes
kubectl rollout status deployment/falconerid   # Wait for restart to complete
```

After a rollout restart, the proxy will automatically reconnect to the new pods (you'll see reconnection messages in the logs).

If you have modified `falconeri-worker` code, you need to rebuild the static binaries and the worker image (starting at the top level):

```sh
just static-bin               # Rebuild static binaries
cd examples/word-frequencies  # Make sure you're in the example dir
just image                    # Rebuild word-frequencies Docker image
```

Then, from the `examples/word-frequencies/` directory, you can run the job:

```sh
just delete-results                  # Clean up previous results
just run                             # Run the job (jobs run once, so you'll always need a fresh one to test)
just results                         # View output
```

The test passes when `just results` shows word frequency counts (e.g., "the 42", "and 28"). For re-runs, use `just delete-results` first.

## Guide Documentation

The `guide/` directory contains mdBook documentation covering:

- **Installation**: Kubernetes cluster setup, authentication, autoscaling, for both production and local environments
- **Specification**: Pipeline spec format, resource requests, S3/GCS authentication
- **Images**: Creating worker Docker images, input/output handling
- **Commands**: CLI reference (proxy, job run/list/describe/retry, db)

To view: Read the markdown files directly in `guide/src/`. The table of contents is in `guide/src/SUMMARY.md`.

## Secret Management

Secrets are generated during the initial deploy, and stored in Kubernetes secrets. Most of them are mounted into appropriate containers normally by the deployment specification. But bucket access credentials are a bit special: They are declared by a specific pipeline spec, and accessed as follows:

- `falconerid` reads the pipeline spec, gets the secret names, and access the secrets from Kubernetes at runtime. This allows it to access newly created secrets without redeploying.
- `falconerid` sets up the worker job spec to mount the secrets as environment variables at runtime, so `falconeri-worker` can find them automatically.

**Development:** The `cargo run -p falconeri -- deploy --development` command sets up a credential named `s3` containing MinIO access keys for the local deployment. 

**Production:** You should **never** need to manually access or configure production credentials. If you encounter errors related to production credentials, immediately stop and ask the user to help.

## Rust Coding Style

NOTE: This section is aspirational, and we may need to review and migrate existing code over time.

All code will be run through `cargo fmt` to enforce style.

### Centralized Dependencies

A number of dependencies are re-exported from `falconeri_common` to crate to ensure consistent versions. Before adding a new dependency to a `Cargo.toml` file, check to see if it is already available from `falconeri_common`. Other dependencies which can't be re-exported for some reason may still be available via the workspace `Cargo.toml`.

### Error-handling

We use `anyhow::Error` and `anyhow::Result`. Our `prelude` module automatically includes `anyhow::Result` as `Result`, replacing Rust's standard `Result`. Instead of writing `Result<T, anyhow::Error>`, you should write `Result<T>`.

### Avoiding `unwrap` and `expect`

IMPORTANT: Never use `unwrap` or `expect` for regular error-handling.

You may use `expect` or `unwrap` ONLY to:

- Represent "can't happen" behavior that indicates a programmer mistake, not a user or runtime error. `expect` is strongly preferred here.
- Inside unit tests.

### Logging

We use `tracing`. You may use `debug!` and `trace!`. Use `#[instrument(level = ...)]` for all functions that call external network services or CLI commands, with a level of `"trace"` or `"debug"`.

### Philosophy

We strongly encourage correctness.

Avoid using `as` when there's a better alternative. Always use `TYPE::from` or `TYPE::try_from` to convert numeric types. **Never** use `std::mem::transmute`. It's a sign something has gone horribly wrong in the code.
