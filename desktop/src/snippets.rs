//! Ready-to-paste debugger settings for the running GDB server.
use crate::platform;
use std::path::Path;

/// What the snippets point at.
pub struct DebugTarget<'a> {
    /// Profile name, used to name launch configurations.
    pub name: &'a str,
    /// GDB executable; a generic name is used when unknown.
    pub gdb: Option<&'a Path>,
    /// ELF with debug symbols.
    pub elf: Option<&'a Path>,
    /// GDB server port on localhost.
    pub port: u16,
}

/// Debuggers the app writes settings for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// GDB in a terminal.
    Gdb,
    /// VS Code with the Microsoft C/C++ extension.
    VsCode,
    /// VS Code with the Cortex-Debug extension.
    CortexDebug,
    /// CLion's Remote Debug configuration.
    CLion,
}

impl Client {
    /// All clients, in tab order.
    pub const ALL: [Client; 4] = [
        Client::Gdb,
        Client::VsCode,
        Client::CortexDebug,
        Client::CLion,
    ];

    /// Tab label.
    pub fn label(self) -> &'static str {
        match self {
            Client::Gdb => "GDB",
            Client::VsCode => "VS Code",
            Client::CortexDebug => "Cortex-Debug",
            Client::CLion => "CLion",
        }
    }

    /// One line on where the snippet goes.
    pub fn hint(self) -> &'static str {
        match self {
            Client::Gdb => "Run this in a terminal while the debug server is running.",
            Client::VsCode => {
                "Add this to \"configurations\" in .vscode/launch.json (Microsoft C/C++ extension)."
            }
            Client::CortexDebug => {
                "Add this to \"configurations\" in .vscode/launch.json (Cortex-Debug extension)."
            }
            Client::CLion => {
                "Run › Edit Configurations › + › Remote Debug, then fill in these fields."
            }
        }
    }

    /// The snippet text.
    pub fn snippet(self, target: &DebugTarget) -> String {
        match self {
            Client::Gdb => platform::command_line(&gdb_args(target)),
            Client::VsCode => vscode(target),
            Client::CortexDebug => cortex_debug(target),
            Client::CLion => clion(target),
        }
    }
}

/// Default GDB name when none was found.
pub const FALLBACK_GDB: &str = "arm-none-eabi-gdb";

fn gdb_name(target: &DebugTarget) -> String {
    target
        .gdb
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| FALLBACK_GDB.to_owned())
}

fn elf_name(target: &DebugTarget) -> String {
    target
        .elf
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "${workspaceFolder}/build/firmware.elf".to_owned())
}

fn address(target: &DebugTarget) -> String {
    format!("127.0.0.1:{}", target.port)
}

/// GDB's arguments: the ELF when known, then connect.
pub fn gdb_args(target: &DebugTarget) -> Vec<String> {
    let mut args = vec![gdb_name(target)];
    if let Some(elf) = target.elf {
        args.push(elf.to_string_lossy().into_owned());
    }
    args.push("-ex".to_owned());
    args.push(format!("target remote {}", address(target)));
    args
}

fn json(value: serde_json::Value) -> String {
    serde_json::to_string_pretty(&value).expect("JSON value")
}

fn vscode(target: &DebugTarget) -> String {
    let cwd = target
        .elf
        .and_then(Path::parent)
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|| "${workspaceFolder}".to_owned());
    json(serde_json::json!({
        "name": format!("Soul Injector: {}", target.name),
        "type": "cppdbg",
        "request": "launch",
        "program": elf_name(target),
        "cwd": cwd,
        "MIMode": "gdb",
        "miDebuggerPath": gdb_name(target),
        "miDebuggerServerAddress": address(target),
        "stopAtEntry": false,
    }))
}

fn cortex_debug(target: &DebugTarget) -> String {
    json(serde_json::json!({
        "name": format!("Soul Injector: {}", target.name),
        "type": "cortex-debug",
        // "attach" because the Soul Injector programs the target itself;
        // "launch" would ask GDB to load the ELF, which is not supported.
        "request": "attach",
        "servertype": "external",
        "gdbTarget": address(target),
        "executable": elf_name(target),
        "gdbPath": gdb_name(target),
    }))
}

fn clion(target: &DebugTarget) -> String {
    format!(
        "Debugger:                 {}\n\
         'target remote' args:     {}\n\
         Symbol file:              {}",
        gdb_name(target),
        address(target),
        elf_name(target)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target<'a>(gdb: Option<&'a Path>, elf: Option<&'a Path>) -> DebugTarget<'a> {
        DebugTarget {
            name: "Board",
            gdb,
            elf,
            port: 4444,
        }
    }

    #[cfg(unix)]
    #[test]
    fn gdb_command_quotes_paths() {
        let elf = Path::new("/work/my app/fw.elf");
        let text = Client::Gdb.snippet(&target(None, Some(elf)));
        assert_eq!(
            text,
            "arm-none-eabi-gdb '/work/my app/fw.elf' -ex 'target remote 127.0.0.1:4444'"
        );
        let text = Client::Gdb.snippet(&target(None, None));
        assert_eq!(text, "arm-none-eabi-gdb -ex 'target remote 127.0.0.1:4444'");
    }

    #[test]
    fn vscode_configs_are_valid_json() {
        let gdb = Path::new("/opt/arm/bin/arm-none-eabi-gdb");
        let elf = Path::new("/work/fw.elf");
        for client in [Client::VsCode, Client::CortexDebug] {
            let text = client.snippet(&target(Some(gdb), Some(elf)));
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["name"], "Soul Injector: Board");
            assert!(text.contains("127.0.0.1:4444"), "{text}");
            assert!(text.contains("/work/fw.elf"), "{text}");
        }
        let text = Client::VsCode.snippet(&target(Some(gdb), Some(elf)));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["miDebuggerPath"], "/opt/arm/bin/arm-none-eabi-gdb");
        assert_eq!(value["cwd"], "/work");
    }
}
