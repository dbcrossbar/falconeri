//! The `deploy` subcommand.

use std::iter;

use clap::Args;
use falconeri_common::{
    base64::{prelude::BASE64_STANDARD, Engine},
    kubernetes,
    manifest::render_manifest,
    prelude::*,
    rand::{distr::Alphanumeric, rngs::StdRng, Rng, SeedableRng},
};

/// The manifest defining secrets for `falconeri`.
const SECRET_MANIFEST: &str = include_str!("secret_manifest.yml.hbs");

/// The manifest we use to deploy `falconeri`.
const DEPLOY_MANIFEST: &str = include_str!("deploy_manifest.yml.hbs");

/// Parameters used to generate a secret manifest.
#[derive(Serialize)]
struct SecretManifestParams {
    /// Include the main falconeri secret (postgres password).
    include_falconeri: bool,
    /// The base64-encoded postgres password.
    postgres_password: String,
    /// Include the MinIO server secret.
    include_minio: bool,
    /// Include the S3 client secret for workers.
    include_s3: bool,
    /// The base64-encoded MinIO root user (also used as AWS_ACCESS_KEY_ID).
    minio_root_user: String,
    /// The base64-encoded MinIO root password (also used as AWS_SECRET_ACCESS_KEY).
    minio_root_password: String,
    /// The MinIO endpoint URL for the s3 secret.
    minio_endpoint_url: String,
}

/// Per-environment configuration.
#[derive(Serialize)]
struct Config {
    /// The name of the environment. Should be `development` or `production`.
    env: String,
    /// The storage class name for PostgreSQL PVC. If unspecified, uses cluster
    /// default. For AWS EKS with gp3 volumes, this should be `gp3`.
    storage_class_name: Option<String>,
    /// The version of PostgreSQL to deploy.
    postgres_version: String,
    /// The amount of disk to allocate for PostgreSQL.
    postgres_storage: String,
    /// The amount of RAM to request for PostgreSQL.
    postgres_memory: String,
    /// The number of CPUs to request for PostgreSQL.
    postgres_cpu: String,
    /// The number of copies of `falconerid` to run.
    falconerid_replicas: u16,
    /// The amount of RAM to request for `falconerid`.
    falconerid_memory: String,
    /// The number of CPUs to request for `falconerid`.
    falconerid_cpu: String,
    /// The RUST_LOG value to pass to `falconerid`.
    falconerid_log_level: String,
    /// The database connection pool size for `falconerid`.
    falconerid_pool_size: u16,
    /// Should we get our `falconeri` image from `minikube`'s internal Docker
    /// daemon?
    use_local_image: bool,
    /// The version of `falconeri`.
    version: String,
    /// Whether to deploy MinIO for local S3-compatible storage.
    enable_minio: bool,
    /// The amount of disk to allocate for MinIO.
    minio_storage: String,
    /// The amount of RAM to request for MinIO.
    minio_memory: String,
    /// The number of CPUs to request for MinIO.
    minio_cpu: String,
    /// The full container image reference for falconeri.
    image: String,
}

/// Parameters used to generate a deploy manifest.
#[derive(Serialize)]
struct DeployManifestParams {
    all: bool,
    config: Config,
}

/// Commands for interacting with the database.
#[derive(Debug, Args)]
#[command(name = "deploy", about = "Commands for interacting with the database.")]
pub struct Opt {
    /// Just print out the manifest without deploying it.
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Don't include secrets in the manifest.
    #[arg(long = "skip-secrets", visible_alias = "skip-secret")]
    skip_secrets: bool,

    /// Deploy a development server (for minikube/colima).
    #[arg(long = "development")]
    development: bool,

    /// The storage class name for the PostgreSQL PVC. If not specified, uses
    /// the cluster's default storage class.
    #[arg(long = "storage-class-name")]
    storage_class_name: Option<String>,

    /// The version of PostgreSQL to deploy. It's generally OK to specify just
    /// the major version, like "14".
    #[arg(long = "postgres-version", default_value = "14")]
    postgres_version: String,

    /// The amount of disk to allocate for PostgreSQL.
    #[arg(long = "postgres-storage")]
    postgres_storage: Option<String>,

    /// The amount of RAM to request for PostgreSQL.
    #[arg(long = "postgres-memory")]
    postgres_memory: Option<String>,

    /// The number of CPUs to request for PostgreSQL.
    #[arg(long = "postgres-cpu")]
    postgres_cpu: Option<String>,

    /// The number of copies of `falconerid` to run.
    #[arg(long = "falconerid-replicas")]
    falconerid_replicas: Option<u16>,

    /// The amount of RAM to request for `falconerid`.
    #[arg(long = "falconerid-memory")]
    falconerid_memory: Option<String>,

    /// The number of CPUs to request for `falconerid`.
    #[arg(long = "falconerid-cpu")]
    falconerid_cpu: Option<String>,

    /// Set the log level to be used for `falconerid`. This uses the same format
    /// as `RUST_LOG`. Example: `falconeri_common=debug,falconerid=debug,warn`.
    #[arg(long = "falconerid-log-level")]
    falconerid_log_level: Option<String>,

    /// Deploy MinIO for local S3-compatible storage. Defaults to true for
    /// --development, false otherwise.
    #[arg(long = "with-minio")]
    with_minio: Option<bool>,

    /// The amount of disk to allocate for MinIO.
    #[arg(long = "minio-storage")]
    minio_storage: Option<String>,

    /// The amount of RAM to request for MinIO.
    #[arg(long = "minio-memory")]
    minio_memory: Option<String>,

    /// The number of CPUs to request for MinIO.
    #[arg(long = "minio-cpu")]
    minio_cpu: Option<String>,

    /// Custom container image for falconeri (production only).
    /// Use this to deploy from a forked repository's CI-built image.
    /// Example: ghcr.io/myorg/falconeri:v2.0.0
    #[arg(long = "image", conflicts_with = "development")]
    image: Option<String>,
}

/// Deploy `falconeri` to the current Kubernetes cluster.
pub async fn run(opt: &Opt) -> Result<()> {
    // Generate passwords using the system's "secure" random number generator.
    let mut rng = StdRng::from_os_rng();
    let postgres_password: Vec<u8> = iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .take(32)
        .collect();
    let minio_root_password: Vec<u8> = iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .take(32)
        .collect();

    // Figure out our configuration.
    let mut config = default_config(opt.development);
    if let Some(storage_class_name) = &opt.storage_class_name {
        config.storage_class_name = Some(storage_class_name.to_owned());
    }
    config.postgres_version = opt.postgres_version.clone();
    if let Some(postgres_storage) = &opt.postgres_storage {
        config.postgres_storage = postgres_storage.to_owned();
    }
    if let Some(postgres_memory) = &opt.postgres_memory {
        config.postgres_memory = postgres_memory.to_owned();
    }
    if let Some(postgres_cpu) = &opt.postgres_cpu {
        config.postgres_cpu = postgres_cpu.to_owned();
    }
    if let Some(falconerid_replicas) = opt.falconerid_replicas {
        config.falconerid_replicas = falconerid_replicas;
    }
    if let Some(falconerid_memory) = &opt.falconerid_memory {
        config.falconerid_memory = falconerid_memory.to_owned();
    }
    if let Some(falconerid_cpu) = &opt.falconerid_cpu {
        config.falconerid_cpu = falconerid_cpu.to_owned();
    }
    if let Some(falconerid_log_level) = &opt.falconerid_log_level {
        config.falconerid_log_level = falconerid_log_level.to_owned();
    }
    // Handle --with-minio flag (defaults based on development mode).
    if let Some(with_minio) = opt.with_minio {
        config.enable_minio = with_minio;
    }
    if let Some(minio_storage) = &opt.minio_storage {
        config.minio_storage = minio_storage.to_owned();
    }
    if let Some(minio_memory) = &opt.minio_memory {
        config.minio_memory = minio_memory.to_owned();
    }
    if let Some(minio_cpu) = &opt.minio_cpu {
        config.minio_cpu = minio_cpu.to_owned();
    }
    if let Some(image) = &opt.image {
        config.image = image.to_owned();
    }

    // Check which secrets need to be created (only if they don't already exist).
    let include_falconeri =
        !opt.skip_secrets && !kubernetes::resource_exists("secret/falconeri").await?;
    let include_minio = config.enable_minio
        && !opt.skip_secrets
        && !kubernetes::resource_exists("secret/falconeri-minio").await?;
    let include_s3 = config.enable_minio
        && !opt.skip_secrets
        && !kubernetes::resource_exists("secret/s3").await?;

    // Generate our secret manifest.
    let secret_params = SecretManifestParams {
        include_falconeri,
        postgres_password: BASE64_STANDARD.encode(&postgres_password[..]),
        include_minio,
        include_s3,
        minio_root_user: BASE64_STANDARD.encode("minioadmin"),
        minio_root_password: BASE64_STANDARD.encode(&minio_root_password[..]),
        minio_endpoint_url: "http://falconeri-minio:9000".to_string(),
    };
    let secret_manifest = render_manifest(SECRET_MANIFEST, &secret_params)?;

    // Generate our deploy manifest.
    let deploy_params = DeployManifestParams { all: true, config };
    let deploy_manifest = render_manifest(DEPLOY_MANIFEST, &deploy_params)?;

    // Combine our manifests.
    let mut manifest = String::new();
    manifest.push_str(&secret_manifest);
    manifest.push_str(&deploy_manifest);

    if opt.dry_run {
        // Print out our manifests.
        print!("{}", manifest);
    } else {
        kubernetes::deploy(&manifest).await?;
    }
    Ok(())
}

/// Undeploy `falconeri`, removing it from the cluster.
pub async fn run_undeploy(all: bool) -> Result<()> {
    // Clean up things declared by our regular manifest. Use development config
    // to ensure MinIO resources are included in the manifest for deletion.
    let params = DeployManifestParams {
        all,
        config: default_config(true),
    };
    let manifest = render_manifest(DEPLOY_MANIFEST, &params)?;
    kubernetes::undeploy(&manifest).await?;

    // Clean up our secrets manually instead of rendering a new manifest.
    if all {
        kubernetes::delete("secret/falconeri").await?;
        kubernetes::delete("secret/falconeri-minio").await?;
        kubernetes::delete("secret/s3").await?;
    }

    Ok(())
}

/// Our current default Postgres version.
const POSTGRES_VERSION: &str = "14";

/// Get our default deployment config.
fn default_config(development: bool) -> Config {
    if development {
        Config {
            env: "development".to_string(),
            storage_class_name: None,
            postgres_version: POSTGRES_VERSION.to_string(),
            postgres_storage: "100Mi".to_string(),
            postgres_memory: "256Mi".to_string(),
            postgres_cpu: "100m".to_string(),
            falconerid_replicas: 1,
            // 256Mi needed for Python-based aws-cli in Alpine 3.21+
            falconerid_memory: "256Mi".to_string(),
            falconerid_cpu: "100m".to_string(),
            falconerid_log_level: "falconeri_common=debug,falconerid=debug,warn"
                .to_string(),
            falconerid_pool_size: 4,
            use_local_image: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            enable_minio: true,
            minio_storage: "256Mi".to_string(),
            minio_memory: "256Mi".to_string(),
            minio_cpu: "100m".to_string(),
            image: format!(
                "ghcr.io/dbcrossbar/falconeri:{}",
                env!("CARGO_PKG_VERSION")
            ),
        }
    } else {
        Config {
            env: "production".to_string(),
            storage_class_name: None,
            postgres_version: POSTGRES_VERSION.to_string(),
            postgres_storage: "10Gi".to_string(),
            postgres_memory: "1Gi".to_string(),
            postgres_cpu: "500m".to_string(),
            falconerid_replicas: 2,
            falconerid_memory: "256Mi".to_string(),
            falconerid_cpu: "450m".to_string(),
            falconerid_log_level: "warn".to_string(),
            falconerid_pool_size: 32,
            use_local_image: false,
            version: env!("CARGO_PKG_VERSION").to_string(),
            enable_minio: false,
            minio_storage: "10Gi".to_string(),
            minio_memory: "512Mi".to_string(),
            minio_cpu: "250m".to_string(),
            image: format!(
                "ghcr.io/dbcrossbar/falconeri:{}",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }
}
