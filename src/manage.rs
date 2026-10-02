//! Soul Injector management service: device info, asset upload and jobs.
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

use proto::{JobStatus, RunResult, Trigger};

/// DEVICE_INFO opcode.
pub const DEVICE_INFO: u16 = 0x0100;
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
        _ => "device error (see the device log)",
    }
}
