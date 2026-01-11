# Use Alpine as a base image, because it's small.
# Alpine 3.21 is current stable LTS (released Nov 2025).
FROM alpine:3.21

# Install minimal dependencies.
# - libc6-compat: Required for musl static binaries
# - ca-certificates: Required for HTTPS connections to cloud APIs
# Note: gsutil and aws-cli are no longer needed - we use native Rust SDKs.
RUN apk --no-cache --update add \
        libc6-compat \
        ca-certificates

# Install `kubectl`.
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
ADD target/${MUSL_TARGET}/${MODE}/falconerid target/${MUSL_TARGET}/${MODE}/falconeri-worker /usr/local/bin/
