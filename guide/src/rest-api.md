# REST API

All major `falconeri` functions are available via REST API calls to `falconerid`. This allows integration with other systems and automation tools.

## OpenAPI Documentation

With `falconeri proxy` running, you can fetch the OpenAPI specification:

```sh
curl http://localhost:8089/api-docs/openapi.json
```

To generate a PDF version of the API documentation:

```sh
# Requires npm
npx apibake openapi/falconeri-openapi.json --out openapi/falconeri-restapi.pdf
```

## Base URL

- **Local development**: `http://localhost:8089` (via `falconeri proxy`)
- **In-cluster**: `http://falconerid:8089` (Kubernetes service DNS)
- **External**: Depends on your ingress configuration (see [HTTP Ingress](./installation.md#setting-up-an-http-ingress))

## Authentication

Most API endpoints require HTTP Basic Authentication:

- **Username**: `falconeri`
- **Password**: The Postgres password from the `falconeri` Kubernetes secret

The CLI and worker automatically use these credentials. For manual API calls:

```sh
# Get the password from the cluster
PASSWORD=$(kubectl get secret falconeri -o jsonpath='{.data.POSTGRES_PASSWORD}' | base64 -d)

# Make an authenticated request
curl -u "falconeri:$PASSWORD" http://localhost:8089/jobs/list
```

**Unauthenticated endpoints** (public):
- `/version` - Server version
- `/api-docs/openapi.json` - OpenAPI specification

If exposing externally, you should also set up HTTPS via your ingress/load balancer. But see the warnings about that configuration in the [installation guide](./installation.md#setting-up-an-http-ingress).
