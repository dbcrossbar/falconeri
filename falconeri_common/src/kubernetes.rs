//! Tools for talking to Kubernetes.

use std::{collections::HashSet, env, fmt, iter, process::Stdio, time::Duration};

use rand::{distr::Alphanumeric, rng, Rng};
use serde::de::{Deserialize, DeserializeOwned};
use serde_json;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::prelude::*;

/// Run `kubectl`, passing any output through to the console.
#[instrument(level = "trace")]
pub async fn kubectl(args: &[&str]) -> Result<()> {
    let status = Command::new("kubectl")
        .args(args)
        .status()
        .await
        .with_context(|| format!("error starting kubectl with {:?}", args))?;
    if !status.success() {
        return Err(format_err!("error running kubectl with {:?}", args));
    }
    Ok(())
}

/// Run `kubectl`, capture output as JSON, and parse it using the
/// specified type.
#[instrument(level = "trace")]
pub async fn kubectl_parse_json<T: DeserializeOwned>(args: &[&str]) -> Result<T> {
    let output = Command::new("kubectl")
        .args(args)
        // Pass `stderr` through on console instead of capturing.
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("error starting kubectl with {:?}", args))?;
    if !output.status.success() {
        return Err(format_err!("error running kubectl with {:?}", args));
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("error parsing output of kubectl {:?}", args))
}

/// Run `kubectl` with the specified input.
#[instrument(skip(input), level = "trace")]
pub async fn kubectl_with_input(args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new("kubectl")
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("error starting kubectl with {:?}", args))?;
    let mut stdin = child.stdin.take().expect("child stdin is missing");
    stdin
        .write_all(input.as_bytes())
        .await
        .with_context(|| format!("error writing input to kubectl {:?}", args))?;
    drop(stdin); // Close stdin so kubectl knows we're done
    let status = child
        .wait()
        .await
        .with_context(|| format!("error running kubectl with {:?}", args))?;
    if !status.success() {
        return Err(format_err!("error running kubectl with {:?}", args));
    }
    Ok(())
}

/// Does `kubectl` exit successfully when called with the specified arguments?
#[instrument(level = "trace")]
pub async fn kubectl_succeeds(args: &[&str]) -> Result<bool> {
    let output = Command::new("kubectl").args(args).output().await?;
    Ok(output.status.success())
}

/// A Kubernetes secret (missing lots of fields).
#[derive(Debug, Deserialize)]
struct Secret<T> {
    /// Our secret data.
    ///
    /// We use some [serde magic][] to deserialize a parameterized type.
    ///
    /// [serde magic]: https://serde.rs/attr-bound.html
    #[serde(bound(deserialize = "T: Deserialize<'de>"))]
    data: T,
}

/// Custom `serde` (de)serialization module for Base64-encoded strings. Use
/// with `#[serde(with = "base64_encoded_secret_string")]` to automatically
/// decode Base64-encoded fields.
pub mod base64_encoded_secret_string {
    use std::result;

    use base64::{prelude::BASE64_STANDARD, Engine};
    use serde::de::{Deserialize, Deserializer, Error as DeError};

    /// Deserialize a secret represented as a Base64-encoded UTF-8 string.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> result::Result<String, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let bytes = BASE64_STANDARD.decode(&encoded[..]).map_err(|err| {
            D::Error::custom(format!("could not base64-decode secret: {}", err))
        })?;
        let decoded = String::from_utf8(bytes).map_err(|err| {
            D::Error::custom(format!("could not UTF-8-decode secret: {}", err))
        })?;
        Ok(decoded)
    }
}

/// Custom `serde` (de)serialization module for optional Base64-encoded strings.
/// Use with `#[serde(default, with = "base64_encoded_optional_secret_string")]`
/// to automatically decode optional Base64-encoded fields.
pub mod base64_encoded_optional_secret_string {
    use std::result;

    use base64::{prelude::BASE64_STANDARD, Engine};
    use serde::de::{Deserializer, Error as DeError};

    /// Deserialize an optional secret represented as a Base64-encoded UTF-8 string.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> result::Result<Option<String>, D::Error> {
        // Use Option::deserialize to handle missing fields
        let maybe_encoded: Option<String> =
            serde::de::Deserialize::deserialize(deserializer)?;
        match maybe_encoded {
            None => Ok(None),
            Some(encoded) => {
                let bytes = BASE64_STANDARD.decode(&encoded[..]).map_err(|err| {
                    D::Error::custom(format!(
                        "could not base64-decode secret: {}",
                        err
                    ))
                })?;
                let decoded = String::from_utf8(bytes).map_err(|err| {
                    D::Error::custom(format!("could not UTF-8-decode secret: {}", err))
                })?;
                Ok(Some(decoded))
            }
        }
    }
}

/// Fetch a secret and deserialize it as the specified type.
#[instrument(level = "trace")]
pub async fn kubectl_secret<T: DeserializeOwned>(secret: &str) -> Result<T> {
    let secret: Secret<T> =
        kubectl_parse_json(&["get", "secret", secret, "-o", "json"]).await?;
    Ok(secret.data)
}

/// A list of items returned by Kubernetes.
#[derive(Deserialize)]
struct ItemsJson<T> {
    items: Vec<T>,
}

/// JSON describing resource metadata.
#[derive(Deserialize)]
struct MetadataJson {
    /// Resource name.
    name: Option<String>,
}

impl MetadataJson {
    /// Get the resource name, if any.
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// JSON describing a pod.
#[derive(Deserialize)]
struct PodJson {
    /// Kubernetes resource metadata.
    metadata: Option<MetadataJson>,
    /// Kubernetes pod status.
    status: Option<PodStatusJson>,
}

impl PodJson {
    /// Get the name of this pod, if any.
    fn name(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.name())
    }

    /// Is this pod running?
    fn is_running(&self) -> bool {
        let phase = self.status.as_ref().and_then(|s| s.phase.as_deref());
        phase == Some("Running")
    }
}

/// JSON describing pod status.
#[derive(Deserialize)]
struct PodStatusJson {
    /// Execution phase (e.g., "Pending", "Running").
    phase: Option<String>,
}

/// JSON describing a job.
#[derive(Deserialize)]
struct JobJson {
    /// Kubernetes job status.
    status: Option<JobStatusJson>,
}

impl JobJson {
    /// Return the failure reported by Kubernetes, if any.
    fn job_failure(&self) -> Option<K8sJobFailure> {
        let conditions = self.status.as_ref()?.conditions.as_ref()?;
        for condition in conditions {
            if condition.condition_type == "Failed" && condition.status == "True" {
                return Some(K8sJobFailure {
                    reason: condition.reason.clone(),
                    message: condition.message.clone(),
                });
            }
        }
        None
    }
}

/// JSON describing job status.
#[derive(Deserialize)]
struct JobStatusJson {
    /// Status conditions.
    conditions: Option<Vec<ConditionJson>>,
}

/// JSON describing a status condition (used by K8s jobs).
#[derive(Deserialize)]
struct ConditionJson {
    /// The type of condition (e.g., "Complete", "Failed").
    #[serde(rename = "type")]
    condition_type: String,
    /// Whether the condition is "True", "False", or "Unknown".
    status: String,
    /// Machine-readable reason for the condition (e.g., "BackoffLimitExceeded").
    reason: Option<String>,
    /// Human-readable detail about the condition.
    message: Option<String>,
}

/// Get a set of currently running pod names.
#[instrument(level = "trace")]
pub async fn get_running_pod_names() -> Result<HashSet<String>> {
    let pods = kubectl_parse_json::<ItemsJson<PodJson>>(&[
        "get",
        "pods",
        // If we pass this, output seems to be limited to 50 records, even if
        // more exist.
        //
        // "--field-selector",
        // "status.phase=Running",
        "--output=json",
    ])
    .await?;

    let mut names = HashSet::new();
    for pod in &pods.items {
        // This replaces the `--field-selector status.phase=Running` argument,
        // which doesn't work. Checking this way seems to see all running pods.
        if !pod.is_running() {
            continue;
        }

        if let Some(name) = pod.name() {
            names.insert(name.to_owned());
        } else {
            warn!("found nameless pod");
        }
    }
    debug!("found {} running pods", names.len());
    trace!("running pods: {:?}", names);
    Ok(names)
}

/// Information about a K8s job's status.
#[derive(Debug, Clone)]
pub struct K8sJobInfo {
    /// The job name.
    pub name: String,
    /// The terminal failure reported by Kubernetes.
    pub failure: Option<K8sJobFailure>,
}

/// A terminal failure reported for a Kubernetes job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct K8sJobFailure {
    /// The machine-readable reason, such as `BackoffLimitExceeded`.
    pub reason: Option<String>,
    /// Human-readable detail supplied by Kubernetes.
    pub message: Option<String>,
}

impl fmt::Display for K8sJobFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.reason, &self.message) {
            (Some(reason), Some(message)) => write!(
                formatter,
                "Kubernetes marked the job as failed ({reason}): {message}"
            ),
            (Some(reason), None) => {
                write!(formatter, "Kubernetes marked the job as failed ({reason})")
            }
            (None, Some(message)) => {
                write!(formatter, "Kubernetes marked the job as failed: {message}")
            }
            (None, None) => formatter.write_str("Kubernetes marked the job as failed"),
        }
    }
}

/// Get information about a single job on the cluster, including its
/// failure status.
///
/// The Kubernetes API call is bounded by `timeout`, rounded down to whole
/// seconds. Callers that hold a database lock while awaiting this function
/// should pass a short value.
///
/// Returns `Ok(None)` if the job does not exist on the cluster. Returns
/// `Err` if we could not reach the Kubernetes API at all; in that case the
/// job's existence is not established, and callers must not treat the job
/// as absent.
#[instrument(level = "trace")]
pub async fn get_job_info(
    name: &str,
    timeout: Duration,
) -> Result<Option<K8sJobInfo>> {
    let request_timeout = format!("--request-timeout={}s", timeout.as_secs());
    let args: &[&str] = &[
        "get",
        "job",
        name,
        "--output=json",
        // Exit successfully with empty output if the job does not exist, so
        // that "absent" is distinguishable from "could not ask the API".
        "--ignore-not-found",
        request_timeout.as_str(),
    ];
    // TODO: Consider a tokio timeout on the shell call, just in case
    // Kubernetes ignores our timeout argument.
    let output = Command::new("kubectl")
        .args(args)
        .output()
        .await
        .with_context(|| format!("error starting kubectl with {:?}", args))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format_err!(
            "error running kubectl with {:?}: {}",
            args,
            stderr.trim()
        ));
    }
    if output.stdout.is_empty() {
        trace!("no kubernetes job named {}", name);
        return Ok(None);
    }
    let job: JobJson = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("error parsing output of kubectl {:?}", args))?;
    let info = K8sJobInfo {
        name: name.to_owned(),
        failure: job.job_failure(),
    };
    trace!("kubernetes job info: {:?}", info);
    Ok(Some(info))
}

/// Deploy a manifest to our Kubernetes cluster.
pub async fn deploy(manifest: &str) -> Result<()> {
    kubectl_with_input(&["apply", "-f", "-"], manifest).await
}

/// Delete all resources specified in the manifest from our Kubernetes cluster.
pub async fn undeploy(manifest: &str) -> Result<()> {
    kubectl_with_input(&["delete", "-f", "-"], manifest).await
}

/// Does the specified resource exist?
pub async fn resource_exists(resource_id: &str) -> Result<bool> {
    kubectl_succeeds(&["get", resource_id]).await
}

/// Delete the specified Kubernetes resource.
pub async fn delete(resource_id: &str) -> Result<()> {
    kubectl(&["delete", resource_id]).await
}

/// Generate a hopefully unique tag for a Kubernetes resource. To keep
/// Kubernetes happy, this must be a legal DNS name component (but we have a
/// database constraint to enforce that).
pub fn resource_tag() -> String {
    let mut rng = rng();
    let bytes = iter::repeat(())
        // Note that this random distribution is biased, because we generate
        // both upper and lowercase letters and then convert to lowercase
        // later. This isn't a big deal for now.
        .map(|()| rng.sample(Alphanumeric))
        // This needs to be large enough to avoid getting bit by
        // https://en.wikipedia.org/wiki/Birthday_problem.
        .take(10)
        .collect::<Vec<u8>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Get the name of the current Kubernetes node.
pub fn node_name() -> Result<String> {
    env::var("FALCONERI_NODE_NAME").context("couldn't get FALCONERI_NODE_NAME")
}

/// Get the name of the current Kubernetes pod.
pub fn pod_name() -> Result<String> {
    env::var("FALCONERI_POD_NAME").context("couldn't get FALCONERI_POD_NAME")
}

/// Check if we should use local (never-pull) images for init containers.
/// Set via FALCONERI_USE_LOCAL_IMAGE environment variable during deployment.
pub fn use_local_image() -> bool {
    env::var("FALCONERI_USE_LOCAL_IMAGE")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_failed_job_condition() {
        let job: JobJson = serde_json::from_value(serde_json::json!({
            "status": {
                "conditions": [{
                    "type": "Failed",
                    "status": "True",
                    "reason": "BackoffLimitExceeded",
                    "message": "Job has reached the specified backoff limit"
                }]
            }
        }))
        .expect("job JSON should parse");

        assert_eq!(
            job.job_failure(),
            Some(K8sJobFailure {
                reason: Some("BackoffLimitExceeded".to_owned()),
                message: Some(
                    "Job has reached the specified backoff limit".to_owned()
                ),
            })
        );
    }

    #[test]
    fn ignores_false_failed_job_condition() {
        let job: JobJson = serde_json::from_value(serde_json::json!({
            "status": {
                "conditions": [{
                    "type": "Failed",
                    "status": "False",
                    "reason": "BackoffLimitExceeded"
                }]
            }
        }))
        .expect("job JSON should parse");

        assert_eq!(job.job_failure(), None);
    }

    #[test]
    fn handles_job_without_conditions() {
        let job: JobJson = serde_json::from_value(serde_json::json!({
            "status": {}
        }))
        .expect("job JSON should parse");

        assert_eq!(job.job_failure(), None);
    }

    #[test]
    fn handles_failed_job_without_reason() {
        let job: JobJson = serde_json::from_value(serde_json::json!({
            "status": {
                "conditions": [{
                    "type": "Failed",
                    "status": "True",
                    "message": "The job failed"
                }]
            }
        }))
        .expect("job JSON should parse");

        assert_eq!(
            job.job_failure(),
            Some(K8sJobFailure {
                reason: None,
                message: Some("The job failed".to_owned()),
            })
        );
    }
}
