//! Job scripts: JavaScript compiled on this computer to the QuickJS-NG
//! bytecode the device loads.
//!
//! The device doesn't parse JavaScript and doesn't verify bytecode, so it
//! must get bytecode from the exact QuickJS-NG release it runs. `sidp-qjsc`
//! builds that release; [`check_device`] compares it with the version the
//! device reports before a script is sent.
use crate::manage::{self, ASSET_NAME_RULE, FIRMWARE_NAME, STAGED_JOB_NAME, proto::DeviceInfo};
use std::path::Path;

pub use sidp_qjsc::Strip;

/// Device file name suffix of compiled scripts.
pub const EXTENSION: &str = "qbc";

/// A script ready to upload.
pub struct Script {
    /// Device asset name, e.g. `stm32wl-rdp.qbc`.
    pub asset: String,
    /// Bytecode container (esp-quickjs-ng `include/qjs_esp.h`).
    pub container: Vec<u8>,
}

/// A script that can't be compiled or sent.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ScriptError(pub String);

/// Compiles `path`, or takes it as it is when it is already a container
/// (`tools/qjs_esp_bc.py` output).
///
/// `asset` defaults to the file name with a `.qbc` extension. Local imports
/// must be bundled into the file first (esbuild, bun build).
pub fn load(path: &Path, asset: Option<&str>, strip: Strip) -> Result<Script, ScriptError> {
    let data = std::fs::read(path).map_err(|e| ScriptError(format!("{}: {e}", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("script.js");
    let container = if sidp_qjsc::is_container(&data) {
        check_container(&data, file_name)?;
        data
    } else {
        let source = std::str::from_utf8(&data)
            .map_err(|_| ScriptError(format!("{file_name}: not UTF-8 JavaScript")))?;
        sidp_qjsc::compile_container(source, file_name, strip)
            .map_err(|e| ScriptError(format!("{file_name} does not compile:\n{e}")))?
    };
    let asset = match asset {
        Some(name) => name.to_owned(),
        None => default_asset_name(path),
    };
    check_asset_name(&asset)?;
    Ok(Script { asset, container })
}

/// SHA-256 of the container, as a job pins it.
pub fn sha256(script: &Script) -> [u8; 32] {
    manage::sha256(&script.container)
}

/// Refuses a device that can't run the script.
pub fn check_device(info: &DeviceInfo, network: bool) -> Result<(), ScriptError> {
    if info.js_version.is_empty() {
        return Err(ScriptError(
            "the device doesn't run job scripts: its firmware is built without them, \
             or they failed to start (see the device log)"
                .into(),
        ));
    }
    if info.js_version != sidp_qjsc::version() {
        return Err(ScriptError(format!(
            "the device runs QuickJS-NG {}, this sidp-agent compiles for {}; \
             update the firmware or sidp-agent so they match",
            info.js_version,
            sidp_qjsc::version()
        )));
    }
    if network && !info.js_network {
        return Err(ScriptError(
            "the script asks for the network, but the firmware is built without \
             CONFIG_SI_JS_NETWORK"
                .into(),
        ));
    }
    Ok(())
}

fn check_container(data: &[u8], file_name: &str) -> Result<(), ScriptError> {
    let ours = sidp_qjsc::compile("", "v.js", Strip::Debug)
        .ok()
        .and_then(|bc| bc.first().copied());
    let theirs = sidp_qjsc::bytecode_version(data);
    if theirs.is_none() || theirs != ours {
        return Err(ScriptError(format!(
            "{file_name} was compiled by a different QuickJS-NG than {}; compile the .js instead",
            sidp_qjsc::version()
        )));
    }
    Ok(())
}

fn default_asset_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    format!("{}.{EXTENSION}", stem.to_ascii_lowercase())
}

fn check_asset_name(name: &str) -> Result<(), ScriptError> {
    if !manage::is_valid_asset_name(name) {
        return Err(ScriptError(format!(
            "script file name '{name}' is not a valid device file name ({ASSET_NAME_RULE}); \
             use --script-name"
        )));
    }
    if name == FIRMWARE_NAME || name == "job.pb" || name == STAGED_JOB_NAME {
        return Err(ScriptError(format!(
            "script file name '{name}' is reserved; use --script-name"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str, network: bool) -> DeviceInfo {
        DeviceInfo {
            js_version: version.into(),
            js_network: network,
            ..Default::default()
        }
    }

    fn temp_file(name: &str, data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sidp-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn compiles_js_and_names_the_asset() {
        let path = temp_file("RDP-Lock.js", b"export function test(ctx) {}\n");
        let script = load(&path, None, Strip::Source).unwrap();
        assert_eq!(script.asset, "rdp-lock.qbc");
        assert!(sidp_qjsc::is_container(&script.container));
    }

    #[test]
    fn takes_a_container_as_it_is() {
        let blob =
            sidp_qjsc::compile_container("export const a = 1;", "a.js", Strip::None).unwrap();
        let path = temp_file("prebuilt.qbc", &blob);
        let script = load(&path, Some("x.qbc"), Strip::Debug).unwrap();
        assert_eq!(script.container, blob);
    }

    #[test]
    fn refuses_another_bytecode_version() {
        let mut blob = sidp_qjsc::compile_container("", "a.js", Strip::None).unwrap();
        blob[sidp_qjsc::HEADER_LEN] ^= 0x40;
        let path = temp_file("old.qbc", &blob);
        let err = load(&path, None, Strip::None).err().unwrap();
        assert!(err.0.contains("different QuickJS-NG"), "{err}");
    }

    #[test]
    fn reports_compile_errors() {
        let path = temp_file("bad.js", b"export function {\n");
        let err = load(&path, None, Strip::Source).err().unwrap();
        assert!(err.0.contains("bad.js does not compile"), "{err}");
        assert!(err.0.contains("SyntaxError"), "{err}");
    }

    #[test]
    fn refuses_reserved_and_invalid_names() {
        let path = temp_file("ok.js", b"");
        assert!(load(&path, Some("firmware.bin"), Strip::Source).is_err());
        assert!(load(&path, Some("Bad Name.qbc"), Strip::Source).is_err());
    }

    #[test]
    fn device_checks() {
        let v = sidp_qjsc::version();
        assert!(check_device(&info(v, false), false).is_ok());
        assert!(check_device(&info("", false), false).is_err());
        assert!(check_device(&info("0.17.0", true), false).is_err());
        assert!(check_device(&info(v, false), true).is_err());
        assert!(check_device(&info(v, true), true).is_ok());
    }
}
