#![deny(unsafe_code)]

// Needed for static linking to work right on Linux.
extern crate openssl_sys;

use falconeri_common::{
    prelude::*,
    rest_api::{Client, OutputFilePatch},
    storage::CloudStorage,
    tracing_support::initialize_tracing,
};
use std::{env, fs, io::ErrorKind, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::RwLock,
};

/// Instructions on how to use this program.
const USAGE: &str = "Usage: falconeri-worker <job id>";

/// Our main entry point.
#[tokio::main]
#[instrument(level = "debug")]
async fn main() -> Result<()> {
    initialize_tracing();
    falconeri_common::init_openssl_probe();

    // Parse our arguments (manually, so we don't need to drag in a ton of
    // libraries).
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 2 {
        eprintln!("{}", USAGE);
        std::process::exit(1);
    }
    if args[1] == "--version" {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    } else if args[1] == "--help" {
        println!("{}", USAGE);
        std::process::exit(0);
    }
    let job_id = args[1].parse::<Uuid>().context("can't parse job ID")?;
    debug!("job ID: {}", job_id);

    // Create a REST client.
    let client = Client::new(ConnectVia::Cluster).await?;

    // Loop until the job is done.
    loop {
        // Fetch our job, and make sure that it's still running.
        let mut job = client.job(job_id).await?;
        trace!("job: {:?}", job);
        if job.status != Status::Running {
            break;
        }

        // Get the next datum and process it.
        if let Some((mut datum, files)) = client.reserve_next_datum(&job).await? {
            // Process our datum, capturing its output.
            let output = Arc::new(RwLock::new(vec![]));
            let result = process_datum(
                &client,
                &job,
                &datum,
                &files,
                &job.command,
                output.clone(),
            )
            .await;
            let output_str =
                String::from_utf8_lossy(&output.read().await).into_owned();

            // Handle the processing results.
            match result {
                Ok(()) => client.mark_datum_as_done(&mut datum, output_str).await?,
                Err(err) => {
                    error!("failed to process datum {}: {:?}", datum.id, err);
                    let error_message = format!("{:?}", err);
                    let backtrace = format!("{}", err.backtrace());
                    client
                        .mark_datum_as_error(
                            &mut datum,
                            output_str,
                            error_message,
                            backtrace,
                        )
                        .await?
                }
            }
        } else {
            debug!("no datums to process right now");

            // Break early if the job is no longer running.
            job = client.job(job_id).await?;
            if job.status != Status::Running {
                break;
            } else {
                // We're still running, so wait a while and check to see if the
                // job finishes or if some datums become available.
                trace!("waiting for job to finish");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }

    // IMPORTANT: Don't exit until all the other workers are ready to exit,
    // because we're normally run as a Kubernetes `Job`, and if so, a 0 exit
    // status would mean that it's safe to start descheduling all other workers.
    // Yes this is weird.
    debug!("all workers have finished");
    Ok(())
}

/// Process a single datum.
#[instrument(skip_all, fields(job = %job.id, datum = %datum.id), level = "trace")]
async fn process_datum(
    client: &Client,
    job: &Job,
    datum: &Datum,
    files: &[InputFile],
    cmd: &[String],
    to_record: Arc<RwLock<Vec<u8>>>,
) -> Result<()> {
    debug!("processing datum {}", datum.id);

    // Download each file.
    reset_work_dirs()?;
    for file in files {
        // We don't pass in any `secrets` here, because those are supposed to
        // be specified in our Kubernetes job when it's created.
        let storage = <dyn CloudStorage>::for_uri(&file.uri, &[]).await?;
        storage
            .sync_down(&file.uri, Path::new(&file.local_path))
            .await?;
    }

    // Run our command.
    if cmd.is_empty() {
        return Err(format_err!("job {} command is empty", job.id));
    }
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {:?}", &cmd[0]))?;

    // Tee stdout and stderr using tokio tasks.
    tee_child(&mut child, to_record).await?;

    let status = child
        .wait()
        .await
        .with_context(|| format!("error running {:?}", &cmd[0]))?;
    if !status.success() {
        return Err(format_err!(
            "command {:?} failed with status {}",
            cmd,
            status
        ));
    }

    // Finish up after the command completes.
    upload_outputs(client, job, datum)
        .await
        .context("could not upload outputs")?;
    reset_work_dirs()?;
    Ok(())
}

/// Copy the stdout and stderr of `child` to either stdout or stderr,
/// respectively, and write a copy to `to_record`.
///
/// This function will panic if `child` does not have a `stdout` or `stderr`.
#[instrument(skip_all, level = "trace")]
async fn tee_child(child: &mut Child, to_record: Arc<RwLock<Vec<u8>>>) -> Result<()> {
    let stdout = child
        .stdout
        .take()
        .expect("child should always have a stdout");
    let stderr = child
        .stderr
        .take()
        .expect("child should always have a stderr");

    let to_record_for_stdout = to_record.clone();
    let to_record_for_stderr = to_record.clone();

    // Spawn tasks to handle stdout and stderr concurrently.
    let stdout_handle = tokio::spawn(async move {
        tee_output(stdout, tokio::io::stdout(), to_record_for_stdout).await
    });
    let stderr_handle = tokio::spawn(async move {
        tee_output(stderr, tokio::io::stderr(), to_record_for_stderr).await
    });

    // Wait for both to complete.
    stdout_handle.await.context("stdout task panicked")??;
    stderr_handle.await.context("stderr task panicked")??;

    Ok(())
}

/// Copy output from `from_child` to `to_console` and `to_record`.
#[instrument(skip_all, level = "trace")]
async fn tee_output<R, W>(
    mut from_child: R,
    mut to_console: W,
    to_record: Arc<RwLock<Vec<u8>>>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Use a small buffer, because I/O performance doesn't matter for reading
    // output to the user.
    let mut buf = vec![0; 4 * 1024];
    loop {
        match from_child.read(&mut buf).await {
            // No more output, so give up.
            Ok(0) => return Ok(()),
            // We have output, so print it.
            Ok(count) => {
                let data = &buf[..count];
                to_console
                    .write_all(data)
                    .await
                    .context("error writing to console")?;
                to_console.flush().await.context("error flushing console")?;
                to_record.write().await.extend_from_slice(data);
            }
            // Retry if reading was interrupted by kernel shenanigans.
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
            // An actual error occurred.
            Err(e) => {
                return Err(e).context("error reading from child process");
            }
        }
    }
}

/// Reset our working directories to a default, clean state.
#[instrument(level = "trace")]
fn reset_work_dirs() -> Result<()> {
    reset_work_dir(Path::new("/pfs/"))?;
    fs::create_dir("/pfs/out").context("cannot create /pfs/out")?;
    reset_work_dir(Path::new("/scratch/"))?;
    Ok(())
}

/// Restore a directory to a default, clean state.
#[instrument(skip_all, fields(work_dir = %work_dir.display()), level = "debug")]
fn reset_work_dir(work_dir: &Path) -> Result<()> {
    // Make sure our work dir still exists.
    if !work_dir.is_dir() {
        return Err(format_err!(
            "the directory {} does not exist, but `falconeri_worker` expects it",
            work_dir.display()
        ));
    }

    // Delete everything in our work dir.
    let entries = work_dir
        .read_dir()
        .with_context(|| format!("error listing directory {}", work_dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| {
                format!("error listing directory {}", work_dir.display())
            })?
            .path();
        trace!("deleting {}", path.display());
        if path.is_dir() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("cannot delete {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("cannot delete {}", path.display()))?;
        }
    }

    // Make sure we haven't deleted our work dir accidentally.
    assert!(work_dir.is_dir());
    Ok(())
}

/// Upload `/pfs/out` to our output bucket.
#[instrument(skip_all, fields(job = %job.id, datum = %datum.id), level = "debug")]
async fn upload_outputs(client: &Client, job: &Job, datum: &Datum) -> Result<()> {
    // Create records describing the files we're going to upload.
    let mut new_output_files = vec![];
    let local_paths = glob::glob("/pfs/out/**/*").context("error listing /pfs/out")?;
    for local_path in local_paths {
        let local_path = local_path.context("error listing /pfs/out")?;
        let _span =
            debug_span!("upload_output", local_path = %local_path.display()).entered();

        // Skip anything we can't upload.
        if local_path.is_dir() {
            continue;
        } else if !local_path.is_file() {
            warn!("can't upload special file {}", local_path.display());
            continue;
        }

        // Get our local path, and strip the prefix.
        let rel_path = local_path.strip_prefix("/pfs/out/")?;
        let rel_path_str = rel_path
            .to_str()
            .ok_or_else(|| format_err!("invalid characters in {:?}", rel_path))?;

        // Build the URI we want to upload to.
        let mut uri = job.egress_uri.clone();
        if !uri.ends_with('/') {
            uri.push('/');
        }
        uri.push_str(rel_path_str);

        // Create a database record for the file we're about to upload.
        new_output_files.push(NewOutputFile {
            datum_id: datum.id,
            job_id: job.id,
            uri: uri.clone(),
        });
    }
    let output_files = client.create_output_files(&new_output_files).await?;

    // Upload all our files in a batch, for maximum performance.
    let storage = <dyn CloudStorage>::for_uri(&job.egress_uri, &[]).await?;
    let result = storage
        .sync_up(Path::new("/pfs/out/"), &job.egress_uri)
        .await;
    let status = match result {
        Ok(()) => Status::Done,
        Err(_) => Status::Error,
    };

    // Record what happened.
    let patches = output_files
        .iter()
        .map(|f| OutputFilePatch { id: f.id, status })
        .collect::<Vec<_>>();
    client.patch_output_files(&patches).await?;

    result
}
