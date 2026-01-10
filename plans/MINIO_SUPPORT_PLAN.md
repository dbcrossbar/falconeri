# MinIO Support Plan

> **Status:** Done.

This document outlines how to add MinIO (S3-compatible storage) support to falconeri for local development and CI testing.

## Goals

1. Enable local testing without requiring AWS credentials or internet access
2. Support CI integration tests with a self-contained storage backend
3. Maintain backward compatibility with existing S3/GCS workflows

## Overview

MinIO support requires two main changes:

1. **S3 endpoint configuration**: Allow workers to connect to S3-compatible endpoints other than AWS
2. **Optional MinIO deployment**: Add MinIO to `falconeri deploy` for development environments

Users will be responsible for creating MinIO buckets (via the MinIO console or `mc` CLI).

---

## Part 1: S3 Endpoint URL Support

### 1.1 Add `optional` field to Secret enum

**File:** `falconeri_common/src/secret.rs`

```rust
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum Secret {
    Mount {
        name: String,
        mount_path: String,
    },
    Env {
        name: String,
        key: String,
        env_var: String,
        /// If true, the secret key is optional and the pod will start even if
        /// the key doesn't exist in the secret.
        #[serde(default)]
        optional: bool,
    },
}
```

### 1.2 Update job manifest template

**File:** `falconerid/src/job_manifest.yml.hbs`

Change the secret injection section (lines 65-74):

```yaml
{{#each pipeline_spec.transform.secrets}}
{{#if env_var}}
        - name: "{{env_var}}"
          valueFrom:
            secretKeyRef:
              name: "{{name}}"
              key: "{{key}}"
{{#if optional}}
              optional: true
{{/if}}
{{/if}}
{{/each}}
```

### 1.3 Add endpoint URL support to S3 storage

**File:** `falconeri_common/src/storage/s3.rs`

Update `aws_command()` method:

```rust
fn aws_command(&self) -> Command {
    let mut command = Command::new("aws");
    if let Some(secret_data) = &self.secret_data {
        command.env("AWS_ACCESS_KEY_ID", &secret_data.aws_access_key_id);
        command.env("AWS_SECRET_ACCESS_KEY", &secret_data.aws_secret_access_key);
    }
    // Support custom S3-compatible endpoints (e.g., MinIO)
    if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
        command.args(["--endpoint-url", &endpoint]);
    }
    command
}
```

### 1.4 Usage in pipeline specs

Users can now reference an optional endpoint URL:

```json
{
  "transform": {
    "secrets": [
      {"name": "s3", "key": "AWS_ACCESS_KEY_ID", "env_var": "AWS_ACCESS_KEY_ID"},
      {"name": "s3", "key": "AWS_SECRET_ACCESS_KEY", "env_var": "AWS_SECRET_ACCESS_KEY"},
      {"name": "s3", "key": "AWS_ENDPOINT_URL", "env_var": "AWS_ENDPOINT_URL", "optional": true}
    ]
  }
}
```

For MinIO, the K8s secret would include:
```bash
kubectl create secret generic s3 \
    --from-literal=AWS_ACCESS_KEY_ID=minioadmin \
    --from-literal=AWS_SECRET_ACCESS_KEY=minioadmin \
    --from-literal=AWS_ENDPOINT_URL=http://falconeri-minio:9000
```

For real S3, the secret would omit `AWS_ENDPOINT_URL` and the worker would use AWS defaults.

---

## Part 2: Optional MinIO Deployment

### 2.1 New CLI flags

**File:** `falconeri/src/cmd/deploy.rs`

Add to `Opt` struct:

```rust
/// Deploy MinIO for local S3-compatible storage.
/// Defaults to true for --development, false otherwise.
#[arg(long = "with-minio")]
with_minio: Option<bool>,

/// The amount of disk to allocate for MinIO.
#[arg(long = "minio-storage")]
minio_storage: Option<String>,

/// The amount of RAM to request for MinIO.
#[arg(long = "minio-memory")]
minio_memory: Option<String>,

/// The number of CPUs to request for MinIO.
#[arg(long = "minio-cpu")]
minio_cpu: Option<String>,
```

### 2.2 New Config fields

Add to `Config` struct:

```rust
/// Whether to deploy MinIO.
enable_minio: bool,
/// The amount of disk to allocate for MinIO.
minio_storage: String,
/// The amount of RAM to request for MinIO.
minio_memory: String,
/// The number of CPUs to request for MinIO.
minio_cpu: String,
```

### 2.3 Default values

| Setting | Development | Production |
|---------|-------------|------------|
| enable_minio | true | false |
| minio_storage | 1Gi | 10Gi |
| minio_memory | 256Mi | 512Mi |
| minio_cpu | 100m | 250m |

### 2.4 Secret generation

**File:** `falconeri/src/cmd/deploy.rs`

Generate MinIO credentials alongside postgres password:

```rust
let minio_root_user = "minioadmin".to_string();  // Or generate random
let minio_root_password = iter::repeat(())
    .map(|()| rng.sample(Alphanumeric))
    .take(32)
    .collect::<Vec<u8>>();
```

### 2.5 Update secret manifest

**File:** `falconeri/src/cmd/secret_manifest.yml.hbs`

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: falconeri
type: Opaque
data:
  POSTGRES_PASSWORD: "{{postgres_password}}"
{{#if enable_minio}}
  MINIO_ROOT_USER: "{{minio_root_user}}"
  MINIO_ROOT_PASSWORD: "{{minio_root_password}}"
{{/if}}
```

### 2.6 Deploy manifest additions

**File:** `falconeri/src/cmd/deploy_manifest.yml.hbs`

Add after PostgreSQL resources:

```yaml
{{#if config.enable_minio}}
---
# MinIO volume: Stores object data.
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: falconeri-minio
spec:
  accessModes:
    - ReadWriteOnce
  volumeMode: Filesystem
  resources:
    requests:
      storage: "{{config.minio_storage}}"
  storageClassName: standard

---
# MinIO deployment: S3-compatible object storage.
apiVersion: apps/v1
kind: Deployment
metadata:
  name: falconeri-minio
  labels:
    app: falconeri-minio
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: falconeri-minio
  template:
    metadata:
      labels:
        app: falconeri-minio
    spec:
      containers:
      - name: minio
        image: minio/minio:latest
        args: ["server", "/data", "--console-address", ":9001"]
        resources:
          requests:
            cpu: "{{config.minio_cpu}}"
            memory: "{{config.minio_memory}}"
        volumeMounts:
        - name: data
          mountPath: /data
        env:
        - name: MINIO_ROOT_USER
          valueFrom:
            secretKeyRef:
              name: falconeri
              key: MINIO_ROOT_USER
        - name: MINIO_ROOT_PASSWORD
          valueFrom:
            secretKeyRef:
              name: falconeri
              key: MINIO_ROOT_PASSWORD
        ports:
        - containerPort: 9000
          name: api
        - containerPort: 9001
          name: console
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: falconeri-minio

---
# MinIO service: Provides DNS lookup for MinIO.
kind: Service
apiVersion: v1
metadata:
  name: falconeri-minio
spec:
  selector:
    app: falconeri-minio
  ports:
  - name: api
    port: 9000
  - name: console
    port: 9001
{{/if}}
```

### 2.7 Update proxy command

**File:** `falconeri/src/cmd/proxy.rs`

Add MinIO port forwarding alongside existing postgres and falconerid:

```rust
// If MinIO is deployed, also forward its ports
let minio_api = kubectl_port_forward("svc/falconeri-minio", 9000);
let minio_console = kubectl_port_forward("svc/falconeri-minio", 9001);
```

Print connection info:
```
MinIO API:     http://localhost:9000
MinIO Console: http://localhost:9001
```

### 2.8 Undeploy support

Update `run_undeploy()` to also remove MinIO resources when present.

---

## Part 3: User Workflow

### Local development with MinIO

```bash
# 1. Start minikube
minikube start
eval $(minikube docker-env)

# 2. Build and deploy with MinIO (default for --development)
just image
cargo run -p falconeri -- deploy --development

# 3. Start proxy (includes MinIO ports)
cargo run -p falconeri -- proxy
# Output:
#   falconerid:    http://localhost:8089
#   PostgreSQL:    localhost:5432
#   MinIO API:     http://localhost:9000
#   MinIO Console: http://localhost:9001

# 4. Create bucket via MinIO console
#    - Open http://localhost:9001
#    - Login with minioadmin / <generated-password>
#    - Create bucket named "test"
#    - Upload test files to test/texts/

# 5. Create S3 secret for workers
kubectl create secret generic s3 \
    --from-literal=AWS_ACCESS_KEY_ID=minioadmin \
    --from-literal=AWS_SECRET_ACCESS_KEY=<password-from-falconeri-secret> \
    --from-literal=AWS_ENDPOINT_URL=http://falconeri-minio:9000

# 6. Update pipeline spec to use MinIO bucket
#    Change: s3://fdy-falconeri-test/texts/ → s3://test/texts/

# 7. Build worker image and run job
cd examples/word-frequencies
just image
just run
```

### Production (no MinIO)

```bash
# Deploy without MinIO (default for production)
cargo run -p falconeri -- deploy

# Or explicitly disable:
cargo run -p falconeri -- deploy --development --with-minio=false
```

---

## Part 4: CI Integration Testing

### GitHub Actions workflow sketch

```yaml
jobs:
  integration-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Start minikube
        uses: medyagh/setup-minikube@latest

      - name: Build images
        run: |
          eval $(minikube docker-env)
          just image
          cd examples/word-frequencies && just image

      - name: Deploy falconeri with MinIO
        run: cargo run -p falconeri -- deploy --development

      - name: Wait for pods
        run: kubectl wait --for=condition=ready pod -l app=falconerid --timeout=120s

      - name: Setup MinIO bucket
        run: |
          # Port-forward MinIO
          kubectl port-forward svc/falconeri-minio 9000:9000 &
          sleep 5

          # Install mc (MinIO client)
          curl -O https://dl.min.io/client/mc/release/linux-amd64/mc
          chmod +x mc

          # Configure and create bucket
          ./mc alias set local http://localhost:9000 minioadmin $MINIO_PASSWORD
          ./mc mb local/test
          ./mc cp examples/word-frequencies/texts/* local/test/texts/

      - name: Create S3 secret
        run: |
          kubectl create secret generic s3 \
            --from-literal=AWS_ACCESS_KEY_ID=minioadmin \
            --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASSWORD \
            --from-literal=AWS_ENDPOINT_URL=http://falconeri-minio:9000

      - name: Run test job
        run: |
          kubectl port-forward svc/falconerid 8089:8089 &
          sleep 5
          cargo run -p falconeri -- job run examples/word-frequencies/word-frequencies-minio.json

      - name: Verify results
        run: |
          # Check job completed successfully
          # Download and verify output files
```

---

## Files Changed Summary

| File | Changes |
|------|---------|
| `falconeri_common/src/secret.rs` | Add `optional` field to `Secret::Env` |
| `falconeri_common/src/storage/s3.rs` | Check `AWS_ENDPOINT_URL` in `aws_command()` |
| `falconerid/src/job_manifest.yml.hbs` | Add `optional: true` support |
| `falconeri/src/cmd/deploy.rs` | Add MinIO config fields and CLI flags |
| `falconeri/src/cmd/deploy_manifest.yml.hbs` | Add MinIO PVC, Deployment, Service |
| `falconeri/src/cmd/secret_manifest.yml.hbs` | Add MinIO credentials |
| `falconeri/src/cmd/proxy.rs` | Add MinIO port forwarding |
| `examples/word-frequencies/word-frequencies-minio.json` | New example for MinIO testing |
| `guide/src/specification.md` | Document optional secrets and endpoint URL |

---

## Future Enhancements (Out of Scope)

- Automatic bucket creation via falconerid
- MinIO multi-node deployment for production use
- TLS support for MinIO
- Integration with external MinIO/S3-compatible services
