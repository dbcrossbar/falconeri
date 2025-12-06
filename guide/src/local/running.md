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
