# Local Development

This guide covers setting up a local Kubernetes environment for falconeri development and testing.

## Prerequisites (All Platforms)

Install the Rust musl target for building static Linux binaries:

```sh
rustup target add x86_64-unknown-linux-musl
```

## Platform Setup

Follow the guide for your platform:

- [macOS (Apple Silicon)](./local/mac.md) - Uses Colima with Rosetta for x86_64 emulation
- [Linux (x86_64)](./local/linux.md) - Uses minikube

## Building and Running

After completing platform setup:

- [Building and Running](./local/running.md) - Build images, deploy, and verify
