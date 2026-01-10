# `falconeri`: Run batch data-processing jobs on Kubernetes

Falconeri runs on a pre-existing Kubernetes cluster, and it allows you to use Docker images to transform large data files stored in cloud buckets.

For detailed instructions, see the [Falconeri guide][guide].

Setup is simple:

```sh
falconeri deploy
falconeri proxy
falconeri migrate
```

Running is similarly simple:

```sh
falconeri job run my-job.json
```

[guide]: https://github.com/faradayio/falconeri/blob/master/guide/src/SUMMARY.md

## REST API

The `falconerid` server provides a complete REST API. You don't need to use the `falconeri` CLI during normal operations. See the [REST API documentation](guide/src/rest-api.md) for authentication details and OpenAPI specs.

## Contributing to `falconeri`

First, you'll need to set up some development tools:

```sh
cargo install just
cargo install cargo-deny
cargo install cargo-edit

# If you want to change the SQL schema, you'll also need the `diesel` CLI. This
# may also require installing some C development libraries.
cargo install diesel_cli
```

Next, check out the available tasks in the `justfile`:

```sh
just --list
```

For local development, you'll need a local Kubernetes cluster. See the detailed setup guides:

- **macOS (Apple Silicon)**: [Colima setup](guide/src/local/mac.md)
- **Linux (x86_64)**: [Minikube setup](guide/src/local/linux.md)

Both guides cover required prerequisites (musl toolchain, PostgreSQL client, MinIO client).

Once your cluster is running, build and deploy:

```sh
just image
cargo run -p falconeri -- deploy --development
```

Check to see if your cluster comes up:

```sh
kubectl get all

# Or if you have `watch`, try:
watch -n 5 kubectl get all
```

### Running the example program

Running the example program is necessary to make sure `falconeri` works. The `--development` deployment includes a MinIO server for local S3-compatible storage.

In another terminal, start the proxy (this also forwards MinIO ports 9000 and 9001):

```sh
cargo run -p falconeri -- proxy
```

Set up MinIO and upload test data (one-time):

```sh
cd examples/word-frequencies
just mc-alias   # Configure MinIO CLI (reads credentials from K8s)
just upload     # Create bucket and upload test texts
```

Run the example:

```sh
just run        # Start the job
just results    # View output (once job completes)
```

For re-runs, clean up previous results first:

```sh
just delete-results
just run
```

From here, you can use `falconeri job describe $ID` and `kubectl` normally. See the [guide][] for more details.

### Releasing a new `falconeri`

For now, this process should only be done by Eric, because there are some semver issues that we haven't fully thought out yet.

First, edit the `CHANGELOG.md` file to describe the release. Next, bump the version:

```sh
just set-version $MY_NEW_VERSION
```

Commit your changes with a subject like:

```sh
$MY_NEW_VERSION: Short description
```

You should be able to make a release by running:

```sh
just release
```

Once the the binaries have built, you can find them at https://github.com/dbcrossbar/falconeri/releases. The `CHANGELOG.md` entry should be automatically converted to release notes.

### Changing the database schema

We use [`diesel`][diesel] as our ORM. This has complex tradeoffs, and we've been considering whether to move to `sqlx` or `tokio-postgres` in the future. See above for instructions on install `diesel_cli`.

[diesel]: https://diesel.rs/

To create a new migration, run:

```sh
cd falconeri_common
diesel migration generate add_some_table_or_columns
```

This will generate a new `up.sql` and `down.sql` file which you can edit as needed. These work like Rails migrations: `up.sql` makes the necessary changes to the database, and `down.sql` reverts those changes. But in this case, migrations are written using SQL.

You can show a list of migrations using:

```sh
diesel migration list
```

To apply pending migrations, run:

```sh
diesel migration run

# Test the `down.sql` file as well.
diesel migration revert
diesel migration run
```

After doing this, edit `falconeri_common/src/schema.rs` and revert any changes which break the schema, and any which introduce warnings. You will probably also need to update any corresponding files in `falconeri_common/src/models/`.

Migrations will be compiled into the server and run on deploys, as well.
