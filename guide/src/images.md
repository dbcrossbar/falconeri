# Creating Docker images

In order to transform your data, you will need to create a Docker image.

## Inputs and outputs

If your pipeline JSON contains the following input section:

```json
"input": {
  "atom": {
    "URI": "gs://example-bucket/books/",
    "repo": "books",
    "glob": "/*"
  }
}
```

...you will find one or more input files from your bucket in the directory `/pfs/books`. You should place your input files in `/pfs/out`, using output names that are unique across all workers.

## Required executables

As of falconeri 2.0, your Docker image only needs to contain your data processing tools. The `falconeri-worker` binary is automatically injected into your container at job startup via a Kubernetes init container, ensuring version consistency with the server.

Cloud storage operations (S3, GCS) are handled natively by `falconeri-worker` using the Rust `object_store` crate—no need to install `gsutil`, `aws`, or other CLI tools.

A minimal worker image needs only:

- `ca-certificates` for HTTPS connections (if your base image doesn't include them)
- Your data processing script or binary

### Example Dockerfile

```Dockerfile
FROM ubuntu:20.04

RUN apt-get update && \
    apt-get install -y ca-certificates && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

ADD my-processing-script.sh /usr/local/bin/
```

See the [word-frequencies example](https://github.com/dbcrossbar/falconeri/tree/main/examples/word-frequencies) for a complete working example.
