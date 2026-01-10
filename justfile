# This is a `justfile`, which is sort of like a less crufty makefile.
# It's processed using https://github.com/casey/just, which you can
# install using `cargo install -f just`.
#
# To see a list of available commands, run `just --list`.
#
# To make an alpha release:
#
# 1. Run `just set-version 0.x.y-alpha.z`, where `0.x.y` will be the next
#    release.
# 2. Push a tag (e.g., `git tag v0.x.y-alpha.z && git push --tags`) to trigger
#    CI, which will publish the Docker image to ghcr.io/dbcrossbar/falconeri.

# This should be either "debug" or "release". You can pass `mode=release` on
# the command line to perform a release build.
MODE := "debug"

# Target triple for static Linux builds.
MUSL_TARGET := "x86_64-unknown-linux-musl"

# Look up our CLI version (which should match our other package versions).
VERSION := `cargo metadata --format-version 1 | jq -r '.packages[] | select(.name == "falconeri") | .version'`

# Print the current version.
version:
    @echo "{{VERSION}}"

# Update all versions. Usage:
#
#     just set-version 0.2.1
#
# TEMPORARY: This will have to be improved before we can make crate releases,
# because it doesn't update inter-crate dependencies. We need something like
# this. See https://github.com/killercup/cargo-edit/issues/426.
set-version NEW_VERSION:
    # If this fails, run `cargo install cargo-edit`.
    cargo set-version --workspace {{NEW_VERSION}}

# Build static musl binaries.
#
# Prerequisites: rustup target add x86_64-unknown-linux-musl
static-bin:
    cargo build --target {{MUSL_TARGET}} {{ if MODE == "release" { "--release" } else { "" } }}

# Build the guide (HTML + PDF).
guide:
    cd guide && mdbook build
    mv guide/book/pdf/output.pdf guide/book/pdf/falconeri-guide.pdf

# Create a `gh-pages` directory with our "GitHub pages" documentation.
gh-pages: guide
    rm -rf gh-pages
    mv guide/book/html gh-pages
    cp guide/book/pdf/falconeri-guide.pdf gh-pages/

# Our `falconeri` Docker image (for local development/testing).
image: static-bin
    docker build \
        --build-arg MODE={{MODE}} \
        --build-arg MUSL_TARGET={{MUSL_TARGET}} \
        -t ghcr.io/dbcrossbar/falconeri:{{VERSION}} .

# Check to make sure that we're in releasable shape.
check:
    cargo fmt -- --check
    cargo deny check
    cargo clippy -- -D warnings
    cargo test --all

# Check to make sure our working copy is clean.
check-clean:
    git diff-index --quiet HEAD --

# Sort and group imports using nightly rustfmt.
# This groups imports into std/external/local sections and merges imports from the same crate.
sort-imports:
    cargo +nightly fmt

# PLEASE DO NOT RUN WITHOUT SIGN-OFF FROM emk. This is not a complete set of
# things that need to be done for a valid release. Some other things:
#
# 1. The top-most commit must be a valid release commit in a consistent
#    format.
# 2. We must follow an as-yet-incomplete semver policy.
#
# For internal testing releases, use:
#
#     just set-version x.y.z-alpha.n
#     # Commit the version bump, then push a tag to trigger CI
#
release: check check-clean
    git tag v{{VERSION}}
    git push
    git push --tags

# Generate REST API PDF documentation from the running server.
# Requires: proxy running, npm installed
rest-docs:
    curl -s http://localhost:8089/api-docs/openapi.json -o openapi/falconeri-openapi.json
    npx apibake openapi/falconeri-openapi.json --out openapi/falconeri-restapi.pdf
