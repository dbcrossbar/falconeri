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

- macOS: `docker context use colima`
- Linux: `eval $(minikube docker-env)` (run in each terminal)

### Key commands

- `just static-bin`: Build static musl binaries to `target/x86_64-unknown-linux-musl/debug/`
- `just image`: Build the Docker image (depends on static-bin)
- `cargo run -p falconeri -- deploy --development`: Deploy in development mode
- `cargo run -p falconeri -- proxy`: Create proxy connection to cluster
- `cargo run -p falconeri -- migrate`: Run database schema migrations
- `kubectl get all`: Check cluster status

## Running the Example Pipeline

The `examples/word-frequencies/` directory contains a complete example pipeline:

1. Set up minikube and deploy falconeri (see above)
2. Create an S3 bucket with `*.txt` files in a `texts/` prefix
3. Create a K8s secret with AWS credentials:
   ```sh
   kubectl create secret generic s3 \
       --from-file=AWS_ACCESS_KEY_ID \
       --from-file=AWS_SECRET_ACCESS_KEY
   ```
4. Edit `word-frequencies.json` to point at your bucket
5. Build the worker image: `just image` (in examples/word-frequencies/)
6. Start a proxy: `just proxy`
7. Run the job: `just run`

See the guide for complete details.

## Guide Documentation

The `guide/` directory contains mdBook documentation covering:

- **Installation**: Kubernetes cluster setup, authentication, autoscaling
- **Specification**: Pipeline spec format, resource requests, S3/GCS authentication
- **Images**: Creating worker Docker images, input/output handling
- **Commands**: CLI reference (proxy, job run/list/describe/retry, db)

To view: Read the markdown files directly in `guide/src/`. The table of contents is in `guide/src/SUMMARY.md`.

## Environment Configuration

You should **never** need to manually configure Kubernetes or cloud storage credentials for development. These are handled via:

- `kubectl` context for Kubernetes access
- K8s secrets for cloud storage credentials (S3, GCS)

If you encounter credential-related errors, immediately stop and ask the user to help.

## Rust Coding Style

NOTE: This section is aspirational, and we may need to review and migrate existing code over time.

All code will be run through `cargo fmt` to enforce style.

### Error-handling

We use `anyhow::Error` and `anyhow::Result`. Our `prelude` module automatically includes `anyhow::Result` as `Result`, replacing Rust's standard `Result`. Instead of writing `Result<T, anyhow::Error>`, you should write `Result<T>`.

### Avoiding `unwrap` and `expect`

IMPORTANT: Never use `unwrap` or `expect` for regular error-handling.

You may use `expect` or `unwrap` ONLY to:

- Represent "can't happen" behavior that indicates a programmer mistake, not a user or runtime error.
- Inside unit tests.

### Logging

We use `tracing`. You may use `debug!` and `trace!`. Use `#[instrument(level = ...)]` for all functions that call external network services or CLI commands, with a level of `"trace"` or `"debug"`.

### Philosophy

We strongly encourage correctness.

Avoid using `as` when there's a better alternative. Always use `TYPE::from` or `TYPE::try_from` to convert numeric types. **Never** use `std::mem::transmute`. It's a sign something has gone horribly wrong in the code.
