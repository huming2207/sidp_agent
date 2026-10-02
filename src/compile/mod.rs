//! Compiles target.yaml and procedure YAML files into a programming job.
//!
//! The firmware no longer parses YAML. Every check it used to make is done
//! here instead, together with the nanopb size limits from [`crate::job`].
mod procedure;
mod source;
mod target;

use crate::job::{Job, Procedure, SchemaVersion};
use serde::de::DeserializeOwned;

/// A compile error describing which YAML input or field is wrong.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CompileError(pub String);

type Result<T> = std::result::Result<T, CompileError>;

/// YAML text for one job.
pub struct Sources<'a> {
    /// target.yaml contents.
    pub target: &'a str,
    /// Variant to select when target.yaml lists several.
    pub variant: Option<&'a str>,
    /// pre_prog.yaml contents, if any.
    pub pre_program: Option<&'a str>,
    /// post_prog.yaml contents, if any.
    pub post_program: Option<&'a str>,
}

/// A compiled job plus non-fatal notes such as skipped legacy steps.
pub struct Compiled {
    /// The job to encode and copy to the device.
    pub job: Job,
    /// Human-readable warnings.
    pub warnings: Vec<String>,
}

/// Compiles the YAML sources into a job the firmware can decode.
pub fn compile(sources: &Sources) -> Result<Compiled> {
    let mut warnings = Vec::new();
    let file: source::TargetFile = parse_yaml(sources.target, "target")?;
    let target = target::convert(&file, sources.variant, &mut warnings)?;
    let pre_program = optional_procedure(sources.pre_program, "pre-program", &mut warnings)?;
    let post_program = optional_procedure(sources.post_program, "post-program", &mut warnings)?;
    let job = Job {
        schema_version: SchemaVersion::Current as i32,
        target: Some(target),
        pre_program,
        post_program,
    };
    Ok(Compiled { job, warnings })
}

fn optional_procedure(
    text: Option<&str>,
    what: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<Procedure>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let file: source::ProcedureFile = parse_yaml(text, what)?;
    procedure::convert(&file, what, warnings).map(Some)
}

fn parse_yaml<T: DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    yaml_serde::from_str(text).map_err(|e| CompileError(format!("{what} YAML: {e}")))
}

/// Narrows a YAML integer to the 32-bit range the firmware uses.
fn to_u32(value: u64, what: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| CompileError(format!("{what} 0x{value:x} does not fit in 32 bits")))
}

/// Narrows an optional YAML integer to 32 bits.
fn optional_u32(value: Option<u64>, what: &str) -> Result<Option<u32>> {
    value.map(|v| to_u32(v, what)).transpose()
}

/// Rejects strings longer than the firmware's fixed buffer.
fn check_len(value: &str, max: usize, what: &str) -> Result<()> {
    if value.len() > max {
        return Err(CompileError(format!(
            "{what} '{value}' is longer than {max} bytes"
        )));
    }
    Ok(())
}

/// Rejects lists longer than the firmware's fixed array.
fn check_count(count: usize, max: usize, what: &str) -> Result<()> {
    if count > max {
        return Err(CompileError(format!(
            "{count} {what} exceed the limit of {max}"
        )));
    }
    Ok(())
}
