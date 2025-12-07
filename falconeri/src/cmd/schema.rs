//! The `schema` command.

use falconeri_common::{pipeline::PipelineSpec, prelude::*, schemars, serde_json};

/// Output the JSON Schema for pipeline specs.
pub fn run() -> Result<()> {
    let schema = schemars::schema_for!(PipelineSpec);
    let json = serde_json::to_string_pretty(&schema)?;
    println!("{}", json);
    Ok(())
}
