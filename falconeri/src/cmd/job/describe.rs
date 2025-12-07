//! The `job describe` subcommand.

use falconeri_common::{prelude::*, rest_api::Client};

use crate::description::render_description;

/// Template for human-readable `describe` output.
const DESCRIBE_TEMPLATE: &str = include_str!("describe.txt.hbs");

/// The `job describe` subcommand.
#[instrument(level = "trace")]
pub async fn run(job_name: &str) -> Result<()> {
    // Load the data we want to display.
    let client = Client::new(ConnectVia::Proxy).await?;
    let job = client.find_job_by_name(job_name).await?;
    let params = client.describe_job(job.id).await?;

    // Print the description.
    print!("{}", render_description(DESCRIBE_TEMPLATE, &params)?);
    Ok(())
}

#[test]
fn render_template() {
    use falconeri_common::rest_api::JobDescribeResponse;

    let job = Job::factory();
    let dsc = |status: Status, count: u64, rerunable_count: u64| DatumStatusCount {
        status,
        count,
        rerunable_count,
    };
    let datum_status_counts = vec![
        dsc(Status::Ready, 1, 0),
        dsc(Status::Running, 1, 0),
        dsc(Status::Error, 2, 1),
    ];
    let mut running_datum = Datum::factory(&job);
    running_datum.status = Status::Running;
    let running_datums = vec![running_datum];
    let mut error_datum = Datum::factory(&job);
    error_datum.status = Status::Error;
    error_datum.error_message = Some("Ooops.".to_owned());
    let error_datums = vec![error_datum];
    let params = JobDescribeResponse {
        job,
        datum_status_counts,
        running_datums,
        error_datums,
    };

    render_description(DESCRIBE_TEMPLATE, &params).expect("could not render template");
}
