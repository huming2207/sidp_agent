//! pre_prog.yaml / post_prog.yaml step conversion.
use super::source::{ProcedureFile, StepSource};
use super::{CompileError, Result, check_count, to_u32};
use crate::job::step::Op;
use crate::job::{
    Delay, Empty, MAX_STEPS, Poll32, Procedure, Read32, ReadModWrite32, Step, Write32,
};

/// Converts every step, skipping legacy no-op types with a warning.
pub fn convert(file: &ProcedureFile, what: &str, warnings: &mut Vec<String>) -> Result<Procedure> {
    let mut steps = Vec::with_capacity(file.steps.len());
    for (index, source) in file.steps.iter().enumerate() {
        let step =
            convert_step(source).map_err(|e| CompileError(format!("{what} step {index}: {e}")))?;
        match step {
            Some(step) => steps.push(step),
            None => warnings.push(format!(
                "{what} step {index}: '{}' is not supported, skipped",
                source.kind
            )),
        }
    }
    check_count(steps.len(), MAX_STEPS, &format!("{what} steps"))?;
    Ok(Procedure { steps })
}

/// Returns `None` for the legacy READ_BLOB/WRITE_BLOB types.
fn convert_step(source: &StepSource) -> Result<Option<Step>> {
    let op = match source.kind.as_str() {
        "READ_32" => Op::Read32(Read32 {
            addr: required(source.addr, "addr")?,
        }),
        "WRITE_32" => Op::Write32(Write32 {
            addr: required(source.addr, "addr")?,
            data: required(source.data, "data")?,
        }),
        "READ_MOD_WRITE_32" => Op::ReadModWrite32(ReadModWrite32 {
            addr: required(source.addr, "addr")?,
            mask: required(source.mask, "mask")?,
            data: required(source.data, "data")?,
        }),
        "POLL_32" => Op::Poll32(Poll32 {
            addr: required(source.addr, "addr")?,
            mask: required(source.mask, "mask")?,
            expected: required(source.expected, "expected")?,
            timeout_ms: required(source.timeout_ms, "timeout_ms")?,
        }),
        "DELAY_MS" => Op::Delay(Delay {
            delay_ms: required(source.delay_ms, "delay_ms")?,
        }),
        "SWD_REINIT" => Op::SwdReinit(Empty {}),
        "SWD_RESET_TARGET" => Op::ResetTarget(Empty {}),
        "SWD_HALT_TARGET" => Op::HaltTarget(Empty {}),
        "SWD_WAIT_HALT" => Op::WaitHalt(Empty {}),
        "READ_BLOB" | "WRITE_BLOB" => return Ok(None),
        other => return Err(CompileError(format!("unknown type '{other}'"))),
    };
    Ok(Some(Step {
        ignore_error: source.ignore_error,
        op: Some(op),
    }))
}

fn required(value: Option<u64>, name: &str) -> Result<u32> {
    let value = value.ok_or_else(|| CompileError(format!("missing '{name}'")))?;
    to_u32(value, name)
}
