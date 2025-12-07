//! The `datum describe` subcommand.

use falconeri_common::{prelude::*, rest_api::Client};

use crate::description::render_description;

/// Template for human-readable `describe` output.
const DESCRIBE_TEMPLATE: &str = include_str!("describe.txt.hbs");

/// Run the `datum describe` subcommand.
pub async fn run(id: Uuid) -> Result<()> {
    // Look up our data via the REST API.
    let client = Client::new(ConnectVia::Proxy).await?;
    let params = client.describe_datum(id).await?;

    // Print the description.
    print!("{}", render_description(DESCRIBE_TEMPLATE, &params)?);
    Ok(())
}

#[test]
fn render_template() {
    use falconeri_common::rest_api::DatumDescribeResponse;

    let job = Job::factory();
    let datum = Datum::factory(&job);
    let input_file = InputFile::factory(&datum);
    let input_files = vec![input_file];
    let params = DatumDescribeResponse { datum, input_files };
    render_description(DESCRIBE_TEMPLATE, &params).expect("could not render template");
}
