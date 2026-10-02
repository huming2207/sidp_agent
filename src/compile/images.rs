//! Pins the SHA-256 of every image a job programs.
//!
//! The device refuses to run a job unless each image it stores under the
//! pinned name has exactly the pinned hash.
use super::{CompileError, Result};
use crate::job::Job;
use crate::manage::{DEVICE_DIR, FIRMWARE_NAME, STAGED_JOB_NAME, is_valid_asset_name};
use std::collections::BTreeMap;

/// Image hashes by device asset name, e.g. `firmware.bin`.
pub type ImageHashes = BTreeMap<String, [u8; 32]>;

/// Fills in the image hashes of a compiled job.
pub fn pin(job: &mut Job, hashes: &ImageHashes, warnings: &mut Vec<String>) -> Result<()> {
    let Some(target) = job.target.as_mut() else {
        return Err(CompileError("job has no target".into()));
    };

    let mut used = Vec::new();
    if let Some(cortex_m) = target.cortex_m.as_mut() {
        cortex_m.firmware_sha256 = lookup(hashes, FIRMWARE_NAME)?;
        used.push(FIRMWARE_NAME);
    }
    if let Some(esp32) = target.esp32.as_mut() {
        for image in &mut esp32.images {
            let name = asset_name(&image.path)?;
            image.sha256 = lookup(hashes, name)?;
            used.push(name);
        }
    }

    for name in hashes.keys() {
        if !used.contains(&name.as_str()) {
            warnings.push(format!("image '{name}' is not used by this job"));
        }
    }
    Ok(())
}

/// Device image paths must be `/data/<asset name>`.
fn asset_name(path: &str) -> Result<&str> {
    let name = path.strip_prefix(DEVICE_DIR).unwrap_or("");
    if !is_valid_asset_name(name) || name == "job.pb" || name == STAGED_JOB_NAME {
        return Err(CompileError(format!(
            "image path '{path}' must be {DEVICE_DIR}<name> with a plain file name"
        )));
    }
    Ok(name)
}

fn lookup(hashes: &ImageHashes, name: &str) -> Result<Vec<u8>> {
    match hashes.get(name) {
        Some(hash) => Ok(hash.to_vec()),
        None => Err(CompileError(format!(
            "the job programs '{name}'; pass it with --image {name}=<file>"
        ))),
    }
}
