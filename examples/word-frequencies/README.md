# Word Frequencies Example

This example processes text files and outputs word frequency counts using falconeri's distributed job processing.

## Prerequisites

- Local Kubernetes cluster running (Colima on macOS, minikube on Linux)
- Falconeri deployed: `cargo run -p falconeri -- deploy --development`
- Proxy running: `just proxy`

## S3/MinIO (Local Development)

One-time setup:

```sh
just mc-alias    # Configure MinIO CLI
```

Running:

```sh
just image       # Build Docker image (run after changes)
just test        # Upload, run job, show results
```

## GCS (Google Cloud Storage)

Requires `GOOGLE_APPLICATION_CREDENTIALS` env var and `gsutil`.

One-time setup:

```sh
just -f Justfile.gcs create-secret  # Create K8s secret from credentials
```

Running:

```sh
just -f Justfile.gcs image                       # Build Docker image (run after changes)
just -f Justfile.gcs GCS_BUCKET=my-bucket test   # Upload, run job, show results
```

## Files

- `word-frequencies.s3.json` / `word-frequencies.gcs.json` - Pipeline specs
- `Justfile` / `Justfile.gcs` - Test commands
- `texts/` - Sample input texts
- `word-frequencies.sh` - Worker script
- `Dockerfile` - Worker image
