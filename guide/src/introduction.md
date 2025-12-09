# Introduction

Falconeri is lightweight tool for running distributed batch jobs on a Kubernetes cluster. You can specify your processing code as a Docker image that reads files as input, and produces other files as output. This allows you to use virtually any programming language.

Falconeri will read files from cloud buckets, distribute them among multiple copies of your worker image, and collect the output into a another cloud bucket.

## Architecture

```mermaid
flowchart TB
    cli[falconeri CLI]

    subgraph k8s[Kubernetes]
        direction TB
        other[Other Job-Running Programs]
        falconerid[falconerid servers]
        db[(PostgreSQL)]
        w1[falconeri-worker #1]
        w2[falconeri-worker #2]
        wn[falconeri-worker #N]
    end

    s3[(S3)]

    cli <--> falconerid
    other <--> falconerid
    falconerid <--> db
    falconerid <--> s3
    falconerid <--> w1
    falconerid <--> w2
    falconerid <--> wn
    w1 <--> s3
    w2 <--> s3
    wn <--> s3

    classDef client fill:#e8f5e9,stroke:#000,color:#000
    classDef server fill:#fff3e0,stroke:#000,color:#000
    classDef worker fill:#e3f2fd,stroke:#000,color:#000
    classDef storage fill:#fce4ec,stroke:#000,color:#000

    class cli,other client
    class falconerid server
    class w1,w2,wn worker
    class db,s3 storage

    linkStyle default stroke:#000
    style k8s fill:#fafafa,stroke:#000,color:#000
```

- **falconeri CLI**: Command-line tool for submitting jobs, monitoring progress, and managing deployments.
- **Other Job-Running Programs**: Any program that speaks the falconerid REST API can submit and manage jobs.
- **falconerid**: The central servers that coordinate job execution, manage worker assignments, and track job state. Multiple server instances provide high availability.
- **PostgreSQL**: Stores all job metadata, datum status, and file references as the authoritative source of truth. Using PostgreSQL probably limits us to a few thousand workers per cluster, but it simplifies the architecture tremendously.
- **falconeri-worker**: Runs your Docker image to process individual datums, downloading inputs from S3 and uploading outputs.
- **S3**: Cloud storage (S3, Google Cloud Storage or S3-compatible) for input files and output results.

Falconeri is inspired by the open source [Pachyderm][], which offers a considerably richer set of tools for batch-processing on a Kubernetes cluster, plus a `git`-like file system for tracking multiple versions of data and recording the provenance.

[Pachyderm]: http://www.pachyderm.io/
