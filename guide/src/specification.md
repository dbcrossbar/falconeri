# Job specification

Here is a sample job specification:

```json
{{#include ../../examples/word-frequencies/word-frequencies.s3.json}}
```

Some notes:

- `parallelism_spec` only accepts `constant`, not `coefficient`. We don't scale the job to fit the cluster; we scale the cluster to fit the job.
- `datum_tries` limits attempts for each datum. A worker pod that disappears while processing a datum uses one of that datum's attempts.
- `job_timeout` is optional and defaults to three days. Values look like `"300s"`, `"2h"` or `"3d"`. Kubernetes stops the whole job once it has run this long, whatever state its datums are in.
- `worker_failure_policy` controls the separate Kubernetes worker pod failure budget. See below.
- `resource_requests` is mandatory.
- The `resource_requests.memory` value is used as both a request and as a hard limit. This is because we've seen too many problems caused by worker nodes that consume unexpectedly large amounts of RAM, forcing other workers (or cluster infrastructure) to be evicted from the node.
- `node_selector` is optional. When present, it allows you to limit which nodes will be used for workers. This also integrates with Kubernetes cluster autoscaling. The autoscaler will look for a node pool with matching tags, and create as many nodes as required to satisfy the `resource_requests`.
- `service_account` is optional. This may be used to specify a Kubernetes service account name, allowing access to the Kubernetes API or to third-party integrations such as credentials from Vault.
- `input` declares which data each worker receives. See [Inputs](#inputs) below.
- `egress.URI` is mandatory.

## Inputs

The `input` section of a pipeline specification declares the data each worker receives. Data is materialized under `/pfs` in the worker container, in a directory per `repo` name.

```json
"input": {
    "atom": {
        "repo": "my-data",
        "URI": "gs://my-bucket/inputs/",
        "glob": "/*"
    }
}
```

- `repo` is the name of the directory the input lands in: `/pfs/my-data/`.
- `URI` is a cloud URI to the input "repo", a directory in your bucket.
- `glob` controls how the repo's contents are distributed across datums:
  - `"/"` puts the entire repo in a single datum, at `/pfs/<repo>/`.
  - `"/*"` puts each top-level entry — every file and subdirectory immediately inside the repo — in its own datum, at `/pfs/<repo>/<entry>` (file entries have no trailing slash; subdirectories keep one). A matched subdirectory is delivered whole, as a single unit of work on one worker.
  - `"/*/path"` puts the subpath `path` of each top-level directory entry in its own datum, at `/pfs/<repo>/<entry>/<path>` (a matched subdirectory keeps its trailing slash and is delivered whole). `path` may be multi-segment (for example `"/*/a/b"`). Top-level file entries have no contents and never match, and an entry that does not contain `path` produces no datum.

Use `"/*"` when each top-level entry is one unit of work. The classic case is a worker function that operates on one subdirectory at a time — say, merging each subdirectory's files into a single output file: for that to be correct under parallel workers, every file of each subdirectory must land in a single datum, which is exactly what `"/*"` provides.

Use `"/*/path"` when each top-level entry is one unit of work but only part of it should be downloaded — say, each entry holds the data alongside caches or intermediates the worker does not need. Whether an entry contains `path` is decided by a single listing probe per entry, so the cost of the glob does not grow with the depth of `path` or with the size of the entries.

On S3, `"/*"` produces one datum per top-level entry on both flat and nested repos. Older falconeri versions produced one datum per S3 object at any depth, so a nested S3 repo now produces fewer, larger datums than before; flat repos (files only, no subdirectories) are unaffected.

## Combining inputs

The value of `"input"` can be a single `atom`, or any of three *combinators* — `union`, `cross`, and `group` — nested to any depth. Every input evaluates to a set of datums (units of work), and the combinators operate on those datums.

Every datum has a *name*: the contributing atom's `repo`, plus the top-level entry matched by the glob's `*` (a `"/*"` or `"/*/path"` datum for entry `shard7` is named for `shard7`). The combinators combine and match datums by these names. At runtime, all of a datum's files are materialized together on one worker under `/pfs/<repo>/`.

### Union

`union` runs every datum of every child; each child contributes its own datums, unchanged.

```json
"input": {
    "union": [
        { "atom": { "repo": "train", "URI": "gs://b/train-2025/", "glob": "/*" } },
        { "atom": { "repo": "train", "URI": "gs://b/train-2026/", "glob": "/*" } }
    ]
}
```

Each top-level entry in either bucket becomes one datum. If children share a `repo` name, the entries matched by `*` should be distinct across children — otherwise two children produce datums with the same name, which you usually want to `group` together instead.

### Cross

`cross` produces every pairing of its children's datums, and the worker sees each side of the pairing in its own `/pfs` directory.

```json
"input": {
    "cross": [
        { "atom": { "repo": "subject", "URI": "gs://b/subjects/", "glob": "/*" } },
        { "atom": { "repo": "model", "URI": "gs://b/models/", "glob": "/*" } }
    ]
}
```

One hundred subjects and three models give 300 datums, each holding `/pfs/subject/<s>` and `/pfs/model/<m>`. Cross multiplies work; keep the product in mind when adding a child.

### Group

`group` merges datums with the *same name* into a single datum, so several sources materialize as one `/pfs` directory. Names are per `repo`, so the standard idiom is to declare *the same repo name over different URIs*:

```json
"input": {
    "group": [
        { "atom": { "repo": "data", "URI": "gs://b/data-1/", "glob": "/*/foo" } },
        { "atom": { "repo": "data", "URI": "gs://b/data-2/", "glob": "/*/bar" } }
    ]
}
```

Both children use the repo name `data` and produce one datum per matching top-level entry. Where both have an entry `alpha`, the two `alpha` datums merge into one whose directory contains both trees:

```text
gs://b/data-1/alpha/foo/...   and   gs://b/data-2/alpha/bar/...
    → /pfs/data/alpha/{foo,bar}
```

Merging directory trees is the interesting case: two trees land in one directory, as if they had been stored together. The rules:

- **Matching is by name, not URI.** Children with different `repo` names never merge — such a `group` is a legal no-op, but probably not what you meant. Merging across buckets requires declaring one repo name over several URIs.
- **File clashes are rejected before the job starts.** If two files in one datum would download different URIs to the same local path, `job run` fails immediately rather than letting workers race. Two *directory* trees may share a local directory — that is the point of the merge — but a file named `x` in each tree clashes inside the merged result. Falconeri does not go looking for that case (it would mean listing everything recursively); treat it as last-write-wins and avoid it.
- An entry matched by only one child still produces a datum, containing just that child's files.

Coming from Pachyderm: falconeri's `group` merges by datum name (repo plus the `*` match), not by a `groupBy` pattern over file paths, and a merged datum materializes as one `/pfs/<repo>/...` tree rather than per-repo `/pfs` trees. To merge the same logical entry across URIs, declare those URIs under one `repo` name.

## The worker pod failure budget

Kubernetes counts the worker pods that fail, and fails the whole job once the count reaches a budget. This budget is separate from `datum_tries`: `datum_tries` limits the attempts for one datum, while the budget covers every worker pod in the job. A pod that dies mid-datum normally costs one counted pod failure and one of that datum's attempts.

By default, Falconeri sets the budget to the greater of four failed pods or twice `parallelism_spec.constant`. A job with many workers is more likely to see unrelated one-off pod failures, and those failures shouldn't kill work that Falconeri is willing to retry. The budget stays finite so that a fault hitting every worker, such as an image that starts and then crashes, stops the job after roughly two worker pools instead of running to `job_timeout`.

Pods carrying the Kubernetes `DisruptionTarget` condition, such as those lost to preemption, eviction or a node drain, do not count against the budget. Falconeri retries their datums instead.

To set the budget yourself, add:

```json
"worker_failure_policy": {
  "maximum_counted_pod_failures": 40
}
```

`maximum_counted_pod_failures` is a number of failed worker pods, from 1 through 2,147,483,647. Kubernetes may report a final failed-pod count larger than the budget, because it terminates the remaining active pods once the budget is spent.

Some failures never produce a failed pod and so never spend the budget. An image stuck in `ImagePullBackOff` is the common one. Those jobs end at `job_timeout`.

## S3 authentication

In order to authenticate with S3, you will need to create a secret, and add a `transform.secrets` section to your pipeline specification. This should look like the following, although you may replace the secret name with something other than `"s3"`. For now, the `"key"` values must be as specified below for the S3 backend to work.

```json
"secrets": [
  {
    "name": "s3",
    "key": "AWS_ACCESS_KEY_ID",
    "env_var": "AWS_ACCESS_KEY_ID"
  },
  {
    "name": "s3",
    "key": "AWS_SECRET_ACCESS_KEY",
    "env_var": "AWS_SECRET_ACCESS_KEY"
  }
]
```

## GCS authentication

For Google Cloud Storage, create a Kubernetes secret containing your service account key JSON, then reference it in your pipeline specification.

First, create the secret from your service account key file:

```bash
kubectl create secret generic gcs \
    --from-file=GOOGLE_SERVICE_ACCOUNT_KEY=/path/to/service-account-key.json
```

Then add this to your pipeline specification:

```json
"secrets": [
  {
    "name": "gcs",
    "key": "GOOGLE_SERVICE_ACCOUNT_KEY",
    "env_var": "GOOGLE_SERVICE_ACCOUNT_KEY"
  }
]
```

Your input and egress URIs should use the `gs://` scheme:

```json
"input": {
    "atom": {
        "repo": "my-data",
        "URI": "gs://my-bucket/inputs/",
        "glob": "/*"
    }
},
"egress": {
    "URI": "gs://my-bucket/outputs/"
}
```
