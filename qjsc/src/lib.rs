//! Compiles Soul Injector job scripts to QuickJS-NG bytecode on the host.
//!
//! The device doesn't parse JavaScript; it loads bytecode containers made
//! here (or by esp-quickjs-ng's `tools/qjs_esp_bc.py`, which gives the same
//! bytes). QuickJS-NG doesn't verify bytecode and only loads its own
//! bytecode version, so this crate builds the exact release the firmware
//! uses, from the `quickjs/` submodule, and [`version`] must equal the
//! device's `DeviceInfo.js_version` before a script is sent.
use std::ffi::{CStr, CString, c_char, c_int, c_void};

unsafe extern "C" {
    fn sidp_qjsc_version() -> *const c_char;
    fn sidp_qjsc_compile(
        src: *const c_char,
        len: usize,
        name: *const c_char,
        strip: c_int,
        out: *mut *mut u8,
        out_len: *mut usize,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sidp_qjsc_free(p: *mut c_void);
}

/// Container magic, see esp-quickjs-ng `include/qjs_esp.h`.
pub const MAGIC: &[u8; 4] = b"QJBC";
/// Container format this crate writes.
pub const FORMAT: u8 = 1;
/// Container header length.
pub const HEADER_LEN: usize = 12;

/// What to leave out of the bytecode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strip {
    /// Keep the source text and debug info.
    None,
    /// Drop the source text; line numbers in stack traces remain (the default).
    #[default]
    Source,
    /// Drop the source text and debug info.
    Debug,
}

/// A script that doesn't compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// QuickJS-NG version this crate compiles for, e.g. "0.18.0".
pub fn version() -> &'static str {
    // SAFETY: JS_GetVersion returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(sidp_qjsc_version()) }
        .to_str()
        .unwrap_or("")
}

/// Compiles one ES module to raw bytecode.
///
/// `name` is the file name stack traces show. Imports with a colon
/// (`si:target`, `qjs:os`...) are left to the device; any other import is an
/// error, since local files must be bundled into the module first.
pub fn compile(source: &str, name: &str, strip: Strip) -> Result<Vec<u8>, Error> {
    let name = CString::new(name).map_err(|_| Error("script name contains NUL".into()))?;
    let mut text = Vec::with_capacity(source.len() + 1);
    text.extend_from_slice(source.as_bytes());
    text.push(0);
    let strip = match strip {
        Strip::None => 0,
        Strip::Source => 1,
        Strip::Debug => 2,
    };
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut out_len = 0usize;
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: text is NUL-terminated at source.len(); name is a C string;
    // the out pointers are valid for writes.
    let ret = unsafe {
        sidp_qjsc_compile(
            text.as_ptr().cast(),
            source.len(),
            name.as_ptr(),
            strip,
            &mut out,
            &mut out_len,
            &mut err,
        )
    };
    if ret == 0 && !out.is_null() {
        // SAFETY: on success out holds out_len bytes from malloc, freed once here.
        let bytecode = unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec();
        unsafe { sidp_qjsc_free(out.cast()) };
        return Ok(bytecode);
    }
    let message = if err.is_null() {
        "out of memory compiling the script".to_owned()
    } else {
        // SAFETY: err is a NUL-terminated string from malloc, freed once here.
        let text = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { sidp_qjsc_free(err.cast()) };
        text
    };
    Err(Error(message.trim_end().to_owned()))
}

/// Wraps bytecode in the container the device loads.
pub fn container(bytecode: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(HEADER_LEN + bytecode.len());
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&[FORMAT, 0, 0, 0]);
    blob.extend_from_slice(&(bytecode.len() as u32).to_le_bytes());
    blob.extend_from_slice(bytecode);
    blob
}

/// Compiles a module straight to a container.
pub fn compile_container(source: &str, name: &str, strip: Strip) -> Result<Vec<u8>, Error> {
    compile(source, name, strip).map(|bc| container(&bc))
}

/// Whether `data` is a bytecode container rather than JavaScript source.
pub fn is_container(data: &[u8]) -> bool {
    data.len() >= HEADER_LEN
        && &data[..4] == MAGIC
        && u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize
            == data.len() - HEADER_LEN
}

/// QuickJS-NG bytecode version of a container (the first bytecode byte).
pub fn bytecode_version(container: &[u8]) -> Option<u8> {
    is_container(container)
        .then(|| container.get(HEADER_LEN).copied())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_firmware_release() {
        assert_eq!(version(), "0.18.0");
    }

    #[test]
    fn compiles_a_job_script() {
        let src = "import * as target from 'si:target';\n\
                   export async function preProgram(ctx) { await target.halt(); }\n";
        let blob = compile_container(src, "job.js", Strip::Source).unwrap();
        assert!(is_container(&blob));
        assert_eq!(bytecode_version(&blob), Some(29));
    }

    #[test]
    fn reports_syntax_errors_with_the_line() {
        let err = compile("let x = 1;\nlet = ;\n", "bad.js", Strip::Source).unwrap_err();
        assert!(err.0.contains("SyntaxError"), "{err}");
        assert!(err.0.contains("bad.js:2"), "{err}");
    }

    #[test]
    fn refuses_local_imports() {
        let err = compile("import { x } from './lib.js';\n", "a.js", Strip::Source).unwrap_err();
        assert!(err.0.contains("bundle local files"), "{err}");
    }

    #[test]
    fn strip_levels_shrink_the_bytecode() {
        let src = "export function test(ctx) {\n  return 1 + 2;\n}\n";
        let none = compile(src, "s.js", Strip::None).unwrap().len();
        let source = compile(src, "s.js", Strip::Source).unwrap().len();
        let debug = compile(src, "s.js", Strip::Debug).unwrap().len();
        assert!(none > source && source > debug, "{none} {source} {debug}");
    }

    #[test]
    fn container_header() {
        let blob = container(&[29, 1, 2]);
        assert_eq!(&blob[..12], b"QJBC\x01\x00\x00\x00\x03\x00\x00\x00");
        assert!(!is_container(&blob[..14]));
    }
}
