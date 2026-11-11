//! Finds ARM-capable GDB executables.
use crate::platform;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// GDB executable names that can debug Cortex-M, most specific first.
const GDB_NAMES: &[&str] = &[
    "arm-none-eabi-gdb",
    "gdb-multiarch",
    "arm-none-eabi-gdb-py",
    "arm-zephyr-eabi-gdb",
];

/// One GDB found on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gdb {
    /// Executable.
    pub path: PathBuf,
    /// First line of `--version`, when it answered.
    pub version: Option<String>,
}

/// Every ARM-capable GDB in `PATH` and the usual toolchain folders.
///
/// Runs each candidate with `--version`, so call it off the UI thread.
pub fn find_gdbs() -> Vec<Gdb> {
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in platform::search_dirs() {
        for name in GDB_NAMES {
            let path = dir.join(platform::executable_file_name(name));
            if !platform::is_executable(&path) {
                continue;
            }
            // The same executable is often reachable through symlinks.
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if found.iter().any(|seen| {
                std::fs::canonicalize(seen).unwrap_or_else(|_| seen.clone()) == canonical
            }) {
                continue;
            }
            found.push(path);
        }
    }
    found
        .into_iter()
        .map(|path| Gdb {
            version: version(&path),
            path,
        })
        .collect()
}

/// First line of `gdb --version`, or `None` when it does not answer within
/// a few seconds.
pub fn version(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn reads_the_version_line() {
        let dir = tempfile::tempdir().unwrap();
        let gdb = dir.path().join("arm-none-eabi-gdb");
        std::fs::write(
            &gdb,
            "#!/bin/sh\necho 'GNU gdb (Arm GNU Toolchain) 14.3'\necho more\n",
        )
        .unwrap();
        std::fs::set_permissions(&gdb, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            version(&gdb).as_deref(),
            Some("GNU gdb (Arm GNU Toolchain) 14.3")
        );
        assert_eq!(version(&dir.path().join("missing")), None);
    }
}
