# Use Alpine as a base image, because it's small.
# Alpine 3.21 is current stable LTS (released Nov 2025).
FROM alpine:3.21

# Install minimal dependencies.
# - bash: Used by some scripts
# - ca-certificates: Required for HTTPS connections
RUN apk --no-cache --update add \
        bash \
        ca-certificates

# Install `kubectl` for Kubernetes operations (used by falconerid).
# kubectl 1.31.x is current stable as of Dec 2025.
ARG KUBERNETES_VERSION=1.31.14
ENV KUBERNETES_VERSION=$KUBERNETES_VERSION
ADD https://dl.k8s.io/release/v${KUBERNETES_VERSION}/bin/linux/amd64/kubectl /usr/local/bin/kubectl
RUN chmod +x /usr/local/bin/kubectl

# Run our webserver out of /app.
WORKDIR /app

# Build target and mode.
ARG MODE=debug
ARG MUSL_TARGET=x86_64-unknown-linux-musl

# Copy static executables into container.
# These are statically linked musl binaries - no shared library dependencies.
ADD target/${MUSL_TARGET}/${MODE}/falconerid target/${MUSL_TARGET}/${MODE}/falconeri-worker /usr/local/bin/
