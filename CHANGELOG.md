# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added GCS (Google Cloud Storage) authentication support with three options:
  - **GKE Workload Identity / metadata service**: Automatic authentication when running on GKE with workload identity configured
  - **`GOOGLE_APPLICATION_CREDENTIALS`**: Standard environment variable pointing to a service account key file path (used by `object_store` library)
  - **`GOOGLE_SERVICE_ACCOUNT_KEY`**: Inline JSON content of service account key (for Kubernetes secrets mounted as env vars in worker pods). This is the standard env var used by the `object_store` crate.
- Added GCS end-to-end test example in `examples/word-frequencies/`
- Fork support: Added `--image` flag to `falconeri deploy` to specify a custom container image, enabling deployment from forked repositories. This flag is for production deployments only (mutually exclusive with `--development`).
- Fork support: CI workflow now uses `github.repository_owner` variable, so forks automatically build and push images to their own container registry.

### Changed

- Replaced `gsutil` and `aws` CLIs with native Rust `object_store` crate for cloud storage operations. This means smaller worker images and better error handling.
- Removed Google Cloud SDK and `aws-cli` from Docker image, reducing image size by ~500MB. Cloud storage is now handled natively by the Rust binaries.

## [2.0.0-alpha.4] - 2026-01-11

### Fixed

- falconerid: Fixed old container repository URL in the job start code. Good catch! We missed this because we hadn't tested a standalone release yet, just the local development deploy.

## [2.0.0-alpha.3] - 2026-01-10

### Fixed

- Fixed `falconeri proxy` to cleanup properly on Ctrl-C.
- Fixed `falconeri proxy` to restart proxy processes when `falconerid` restarts, mostly for developer convenience.

## [2.0.0-alpha.2] - 2026-01-10

### Added

- Added `falconeri` binaries for Apple Silicon (aarch64-apple-darwin). These are unsigned for now.

## [2.0.0-alpha.1] - 2026-01-10

### Added

- A new `falconeri schema` command outputs JSON Schema for pipeline specification files, enabling IDE autocompletion and validation.
- OpenAPI documentation is available at the `/api-docs/openapi.json` endpoint.
- Added `AWS_ENDPOINT_URL` environment variable support in pipeline specification files for S3-compatible storage backends.
- Added `--storage-class-name` flag for `falconeri deploy` to configure Kubernetes storage class.
- Developer: Added MinIO support for fully offline local development.
- Developer: Added local development guide for macOS with Colima (see `guide/src/local/mac.md`).

### Changed

- **BREAKING:** We now do `falconeri-worker` injection via a Kubernetes init container: User Docker images no longer need to include `falconeri-worker`. The worker binary is automatically injected from the `falconerid` image at job startup, ensuring version consistency.
- **BREAKING (internal API):** Private REST API endpoints used by `falconeri-worker` have changed. The new init container injection ensures your `falconeri-worker` version always matches.
- **BREAKING:** The Docker image registry moved from `faraday/falconeri` to `ghcr.io/dbcrossbar/falconeri`.
- **BREAKING:** The default storage class changed from hardcoded `standard` to cluster default. GKE users should add `--storage-class-name=standard`. EKS 1.30+ users may need to specify a storage class explicitly.
- **BREAKING:** GitHub repository moved from `faradayio` to `dbcrossbar`.
- Internal: Migrated from Rocket to axum, from sync diesel to diesel-async, and from sync Rust to async Rust.

### Removed

- Removed requirement to include `falconeri-worker` in user Docker images (now injected automatically).
- Internal: Removed `Rocket.toml` configuration file, plus the `ROCKET_ENV` and `ROCKET_CONFIG` environment variables. This is probably mostly internal.
- Internal: Replaced `ekidd/rust-musl-builder` with standard Rust musl toolchain for builds.
- Internal: Completely eliminated OpenSSL and libpq dependencies. Builds now use pure-Rust rustls for TLS and tokio-postgres for database connections.

### Fixed

- Added pod ownership verification and enhanced logging to help diagnose the rare "duplicate key" error during datum uploads. If you've encountered this error, please watch for improved diagnostics in logs.

## [1.0.0-beta.12] - 2022-12-14

### Fixed

- Log much less from `falconeri_worker` by default, and make it configurable. This fixes an issue where the newer tracing code was causing the worker to log far too much.

## [1.0.0-beta.11] - 2022-12-14 [YANKED]

### Fixed

- This version hard-coded a very low logging level. It was yanked because the low logging level would have made it impossible to debug falconeri issues discovered in the field, and because it was never fully released.

## [1.0.0-beta.10] - 2022-12-02

### Fixed

- Prevent key constraint error when retrying failed datums ([Issue #33](https://github.com/faradayio/falconeri/issues/33)). But see [Issue #36](https://github.com/faradayio/falconeri/issues/36); we still don't do the right thing when output files are randomly named.
- Reduce odds of birthday paradox collision when naming jobs ([Issue #35](https://github.com/faradayio/falconeri/issues/35)).

## [1.0.0-beta.9] - 2022-10-24

### Fixed

- Hard-code PostgreSQL version to prevent it from getting accidentally upgraded by Kubernetes.

## [1.0.0-beta.8] - 2022-05-19

### Fixed

- Use correct file name to upload release assets (again).

## [1.0.0-beta.7] - 2022-05-19

### Fixed

- Use correct file name to upload release assets.

## [1.0.0-beta.6] - 2022-05-19

### Fixed

- Attempted to fix binary builds on Linux (yet again).

## [1.0.0-beta.5] - 2022-05-19

### Fixed

- Attempted to fix binary builds on Linux (again).

## [1.0.0-beta.4] - 2022-05-19

### Fixed

- Attempted to fix binary builds on Linux. Not even trying on the Mac.

## [1.0.0-beta.3] - 2022-05-17

### Fixed

- Work around issue where `--field-selector` didn't find all running pods, resulting in accidental worker terminations.

## [1.0.0-beta.2] - 2021-12-02

### Fixed

- Fix `job_timeout` conversion to `ttlActiveSeconds` in the Kubernetes YAML.

## [1.0.0-beta.1] - 2021-11-24

This release adds a "babysitter" process inside each `falconerid`. We use this to monitor jobs and datums, and detect and/or recover from various types of errors. Updating an existing cluster _should_ be fine, but it's likely to spend a minute or two detecting and marking problems with old jobs. So please exercise appropriate caution.

We plan to stabilize a `falconeri` 1.0 with approximately this feature set. It has been in production for years, and the babysitter was the last missing critical feature.

### Added

- If worker pod disappears off the cluster while processing a datum, detect this and set the datum to `status = Status::Error`. This is handled automatically by a "babysitter" thread in `falconerid`.
- Add support for `datum_tries` in the pipeline JSON. Set this to 2, 3, etc., to automatically retry failed datums. This is also handled by the babysitter.
- Periodically check to see whether a job has finished without being correctly marked as such. This is mostly intended to clean up existing clusters.
- Periodically check to see whether a Kubernetes job has unexpectedly disappeared, and mark the corresponding `falconeri` job as having failed.
- Add trace spans for most low-level database access.

### Fixed

- We now correctly update `updated_at` on all tables that have it.

## [0.2.13] - 2021-11-23

### Added

- Wrote some basic developer documentation to supplement the `justfile`s.
- Allow specifying `--falconerid-log-level` for `falconeri deploy`. This uses standard `RUST_LOG` syntax, as described in the CLI help. 

### Fixed

- Cleaned up tracing output a bit.
- Switched to using `rustls` for HTTPS. Database connections still indirectly require OpenSSL thanks to `libpq`.

## [0.2.12] - 2021-11-22

### Fixed

- Attempt to fix TravisCI binary releases.

## [0.2.11] - 2021-11-22

### Added

- Don't show interactive progress bar when uploading outputs.
- Support `job_timeout` in pipeline schemas. This allows you to specify when an entire job should be stopped, even if it isn't done. Values include "300s", "2h", "2d", etc.
- Add much better tracing support when `RUST_LOG=trace` is passed.

### Changed

- We update most of our dependencies, including Rust libraries and our Docker base images. But this shouldn't affect normal use.

### Fixed

- Set `ttlSecondsAfterFinished` to 1 day so that old jobs don't hang around forever on the backplane wasting storage.
