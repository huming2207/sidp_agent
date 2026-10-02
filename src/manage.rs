//! Soul Injector management service: device info, asset upload, jobs and
//! the production log.
//!
//! These are SIDP opcodes 0x0100 and up, documented in sidp_client's
//! `docs/sidp-management.md`. Payloads are the protobuf messages in
//! `proto/manage.proto` (a copy of sidp_client's; keep them identical),
//! except ASSET_WRITE, which is a 4-byte offset followed by raw data.
use crate::codec;
use crate::session::{Result, Session};
use crate::transport::Transport;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Generated management service messages.
#[allow(missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/si.manage.rs"));
}

use prost::Message;
use proto::{BootRecord, JobStatus, LogEntry, LogEntryType, RunRecord, RunResult, Trigger};

/// DEVICE_INFO opcode.
pub const DEVICE_INFO: u16 = 0x0100;
/// SET_TIME opcode.
pub const SET_TIME: u16 = 0x0101;
/// ASSET_BEGIN opcode.
pub const ASSET_BEGIN: u16 = 0x0110;
/// ASSET_WRITE opcode.
pub const ASSET_WRITE: u16 = 0x0111;
/// ASSET_COMMIT opcode.
pub const ASSET_COMMIT: u16 = 0x0112;
/// JOB_SET opcode.
pub const JOB_SET: u16 = 0x0120;
/// JOB_GET opcode.
pub const JOB_GET: u16 = 0x0121;
/// JOB_RUN_ONCE opcode.
pub const JOB_RUN_ONCE: u16 = 0x0122;
/// JOB_CANCEL opcode.
pub const JOB_CANCEL: u16 = 0x0123;
/// LOG_READ opcode.
pub const LOG_READ: u16 = 0x0130;
/// LOG_ACK opcode.
pub const LOG_ACK: u16 = 0x0131;

/// Status returned while the production log must be collected before a run.
pub const STATUS_LOG_FULL: i32 = 30;

/// Asset name a new job is uploaded under before JOB_SET activates it.
pub const STAGED_JOB_NAME: &str = "job.pb.new";
/// Asset name the device keeps for the firmware of Cortex-M jobs.
pub const FIRMWARE_NAME: &str = "firmware.bin";
/// Directory the device stores assets in.
pub const DEVICE_DIR: &str = "/data/";
/// Longest asset name the device accepts (nanopb max_size 32).
pub const MAX_ASSET_NAME_LEN: usize = 31;
/// Largest ASSET_WRITE data chunk that fits one SIDP frame.
pub const MAX_CHUNK: usize = codec::MAX_FRAME - codec::HEADER - 4;

/// SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Whether the device accepts `name` as an asset file name.
pub fn is_valid_asset_name(name: &str) -> bool {
    let allowed = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    !name.is_empty()
        && name.len() <= MAX_ASSET_NAME_LEN
        && !name.starts_with('.')
        && allowed
        && !name.ends_with(".part")
        && !name.ends_with(".sha256")
}

/// Uploads `data` as asset `name` unless the device already holds it.
///
/// Returns false when nothing had to be sent. An upload interrupted by a
/// dropped connection resumes where the device left off.
pub fn push_asset<T: Transport>(session: &mut Session<T>, name: &str, data: &[u8]) -> Result<bool> {
    let size = u32::try_from(data.len())
        .map_err(|_| crate::session::Error::State("asset larger than 4 GiB"))?;
    let begin = session.asset_begin(name, size, &sha256(data))?;
    if begin.present {
        return Ok(false);
    }

    let mut offset = begin.offset as usize;
    while offset < data.len() {
        let end = data.len().min(offset + MAX_CHUNK);
        session.asset_write(offset as u32, &data[offset..end])?;
        offset = end;
    }
    session.asset_commit()?;
    Ok(true)
}

/// Uploads the job's images and the job itself, then activates it.
///
/// `images` are (asset name, contents) pairs. Images the device already
/// holds and a job that is already active are not sent again.
pub fn push_job<T: Transport>(
    session: &mut Session<T>,
    job: &[u8],
    images: &[(String, Vec<u8>)],
    trigger: Trigger,
) -> Result<()> {
    for (name, data) in images {
        push_asset(session, name, data)?;
    }

    let job_sha256 = sha256(job);
    let status = session.job_get()?;
    if !status.has_job || status.sha256 != job_sha256 {
        push_asset(session, STAGED_JOB_NAME, job)?;
    }
    session.job_set(&job_sha256, trigger)
}

/// Polls JOB_GET until run `run_id` has a result.
pub fn wait_for_run<T: Transport>(
    session: &mut Session<T>,
    run_id: u32,
    interval: Duration,
) -> Result<RunResult> {
    loop {
        let status: JobStatus = session.job_get()?;
        if let Some(result) = status.last_result
            && result.run_id == run_id
        {
            return Ok(result);
        }
        std::thread::sleep(interval);
    }
}

/// Short description of a management status code.
pub fn describe_status(status: i32) -> &'static str {
    match status {
        2 => "rejected by the device (bad argument, missing file or hash mismatch)",
        3 => "not supported by this firmware",
        4 => "device busy: a programming run is in progress",
        STATUS_LOG_FULL => "production log full: collect it with `sidp-agent log pull`",
        _ => "device error (see the device log)",
    }
}

/// Reads every production log entry after `after_id`, oldest first.
pub fn read_log<T: Transport>(
    session: &mut Session<T>,
    mut after_id: u64,
) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    loop {
        let response = session.log_read(after_id)?;
        let Some(last) = response.entries.last() else {
            return Ok(entries);
        };
        after_id = last.id;
        entries.extend(response.entries);
    }
}

/// One log entry as a JSON object, tagged with the device serial number.
pub fn log_entry_json(serial: &str, entry: &LogEntry) -> serde_json::Value {
    let mut json = serde_json::json!({
        "device": serial,
        "id": entry.id,
        "boot": entry.id >> 40,
        "uptime_us": entry.uptime_us,
        "utc_ms": (entry.utc_ms != 0).then_some(entry.utc_ms),
        "type": "unknown",
    });
    match LogEntryType::try_from(entry.r#type) {
        Ok(LogEntryType::LogEntryRun) => {
            if let Ok(run) = RunRecord::decode(entry.record.as_slice()) {
                add_run_fields(&mut json, &run);
            }
        }
        Ok(LogEntryType::LogEntryBoot) => {
            if let Ok(boot) = BootRecord::decode(entry.record.as_slice()) {
                json["type"] = "boot".into();
                json["reset_reason"] = boot.reset_reason.into();
            }
        }
        _ => {}
    }
    json
}

fn add_run_fields(json: &mut serde_json::Value, run: &RunRecord) {
    let result = run.result.unwrap_or_default();
    json["type"] = "run".into();
    json["run_id"] = result.run_id.into();
    json["outcome"] = enum_name(result.outcome, proto::Outcome::as_str_name).into();
    json["failed_stage"] = enum_name(result.failed_stage, proto::Stage::as_str_name).into();
    json["duration_ms"] = result.duration_ms.into();
    json["trigger"] = enum_name(run.trigger, Trigger::as_str_name).into();
    json["job_name"] = run.job_name.clone().into();
    json["job_sha256"] = hex(&run.job_sha256).into();
    json["error_code"] = run.error_code.into();
}

/// Protobuf name of an enum value, or the number when this build does not know it.
fn enum_name<E: TryFrom<i32>>(value: i32, as_str_name: fn(&E) -> &'static str) -> String {
    match E::try_from(value) {
        Ok(known) => as_str_name(&known).to_owned(),
        Err(_) => value.to_string(),
    }
}

/// Lower-case hexadecimal string of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
