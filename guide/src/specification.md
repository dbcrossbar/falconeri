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
- For now, `input.atom` is the only supported input type.
- `egress.URI` is mandatory.

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
