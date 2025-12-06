# Linux Setup (x86_64)

This guide covers setting up a local Kubernetes environment on Linux using minikube.

## Prerequisites

### musl toolchain

Install the musl toolchain for building static Linux binaries. On Ubuntu/Debian:

```sh
sudo apt-get install musl-tools musl-dev
```

For other distributions, install the equivalent `musl-tools` or `musl-gcc` package.

### PostgreSQL client

For database access with `falconeri db console`:

```sh
# Ubuntu/Debian
sudo apt-get install postgresql-client

# Fedora/RHEL
sudo dnf install postgresql
```

### MinIO Client

For interacting with MinIO storage in development mode:

```sh
curl https://dl.min.io/client/mc/release/linux-amd64/mc -o ~/.local/bin/mc
chmod +x ~/.local/bin/mc
```

Make sure `~/.local/bin` is in your PATH.

### Docker

Install Docker following the [official instructions](https://docs.docker.com/engine/install/) for your distribution.

### Minikube

Install minikube following the [official instructions](https://minikube.sigs.k8s.io/docs/start/).

## Starting Minikube

```sh
minikube start
```

## Configuring Docker

Point your Docker CLI at minikube's daemon. This must be run in each new terminal session:

```sh
eval $(minikube docker-env)
```

## Minikube Commands

| Command | Description |
|---------|-------------|
| `minikube start` | Start minikube cluster |
| `minikube stop` | Stop minikube cluster |
| `minikube delete` | Delete minikube cluster |
| `minikube status` | Check cluster status |
| `eval $(minikube docker-env)` | Point Docker CLI at minikube |

## Next Steps

Once minikube is running, proceed to [Building and Running](./running.md).
