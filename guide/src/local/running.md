# Building and Running

Once you have your local Kubernetes cluster running (via [Colima](./mac.md) or [minikube](./linux.md)), follow these steps to build and deploy falconeri.

## Building Images

Build static musl binaries and the Docker image:

```sh
just image
```

This command:
1. Compiles the Rust binaries for `x86_64-unknown-linux-musl`
2. Builds a Docker image containing `falconerid`

Images built this way are immediately available to Kubernetes pods without pushing to a registry (uses `imagePullPolicy: Never`).

## Deploying Falconeri

Deploy falconeri in development mode:

```sh
cargo run -p falconeri -- deploy --development
```

The `--development` flag:
- Uses fewer replicas
- Configures for local images (no registry pull)

## Creating a Proxy Connection

In another terminal, create a proxy connection to the cluster:

```sh
cargo run -p falconeri -- proxy
```

This forwards the falconeri API to your local machine.

## Running Migrations

Run database schema migrations:

```sh
cargo run -p falconeri -- migrate
```

## Verifying Your Setup

Check the cluster status:

```sh
kubectl get all
```

You should see the falconeri deployment, service, and pods running.

## Summary of Commands

| Command | Description |
|---------|-------------|
| `just image` | Build static binaries and Docker image |
| `cargo run -p falconeri -- deploy --development` | Deploy in development mode |
| `cargo run -p falconeri -- proxy` | Create proxy connection |
| `cargo run -p falconeri -- migrate` | Run database migrations |
| `kubectl get all` | Check cluster status |

## Running the Word-Frequencies Example

The `examples/word-frequencies/` directory contains a complete example pipeline that processes text files and outputs word frequency counts.

### Prerequisites

- Falconeri deployed with `--development` (includes MinIO)
- Proxy running in another terminal
- MinIO client (`mc`) installed (see [macOS](./mac.md) or [Linux](./linux.md) setup)

### Building the Example Image

From the `examples/word-frequencies/` directory:

```sh
just image
```

### Setting Up MinIO

Configure the MinIO CLI with credentials from the cluster:

```sh
just mc-alias
```

Upload test data to MinIO:

```sh
just upload
```

### Running the Job

```sh
just run
```

### Viewing Results

```sh
just results
```

This shows the top 20 word frequencies from the processed text.

### Re-running Tests

To verify you're seeing fresh output (not stale results), delete the previous results first:

```sh
just delete-results
just run
just results
```

### Word-Frequencies Commands

| Command | Description |
|---------|-------------|
| `just image` | Build the word-frequencies Docker image |
| `just mc-alias` | Configure MinIO CLI credentials |
| `just upload` | Create bucket and upload test texts |
| `just run` | Submit the word-frequencies job |
| `just results` | Display word frequency output |
| `just delete-results` | Delete results (for clean re-runs) |
