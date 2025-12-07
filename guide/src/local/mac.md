# macOS Setup (Apple Silicon)

This guide covers setting up a local Kubernetes environment on Apple Silicon Macs using Colima.

## Prerequisites

### musl-cross toolchain

Some crates (like `ring`) require a C compiler for cross-compilation. Install the musl-cross toolchain:

```sh
brew install filosottile/musl-cross/musl-cross
```

The `.cargo/config.toml` file is already configured to use this linker.

### PostgreSQL client

For database access with `falconeri db console`:

```sh
brew install libpq
```

Add libpq to your PATH (add to your shell profile):

```sh
export PATH="/opt/homebrew/opt/libpq/bin:$PATH"
```

Alternatively, you can install the full PostgreSQL package:

```sh
brew install postgresql
```

### MinIO Client

For interacting with MinIO storage in development mode:

```sh
brew install minio/stable/mc
```

### Colima

```sh
brew install colima
```

## Starting Colima with Kubernetes

```sh
colima start --kubernetes --arch x86_64 --vm-type=vz --vz-rosetta
```

Flags explained:

- `--kubernetes` - Enables the k3s Kubernetes distribution
- `--arch x86_64` - Uses x86_64 architecture to match production
- `--vm-type=vz` - Uses Apple's Virtualization Framework (faster than QEMU)
- `--vz-rosetta` - Enables Rosetta 2 for x86_64 translation (much faster than full emulation)

## Configuring Docker

Point your Docker CLI at Colima's daemon:

```sh
docker context use colima
```

This setting persists across terminal sessions.

## Colima Commands

| Command | Description |
|---------|-------------|
| `colima start --kubernetes ...` | Start Colima with Kubernetes |
| `colima stop` | Stop Colima |
| `colima delete` | Delete Colima VM (start fresh) |
| `colima status` | Check Colima status |
| `docker context use colima` | Point Docker CLI at Colima |
| `docker context use default` | Switch back to default Docker |

## Troubleshooting

### Image not found in Kubernetes

Make sure you're using Colima's Docker context before building:

```sh
docker context use colima
just image
docker images | grep falconeri
```

### Port connectivity issues

Some Colima users report networking issues with x86 emulation. If exposed ports aren't accessible, try recreating the Colima VM:

```sh
colima delete
colima start --kubernetes --arch x86_64 --vm-type=vz --vz-rosetta
```

## Next Steps

Once Colima is running, proceed to the Building and Running section.
