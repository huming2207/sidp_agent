//! Everything that differs between operating systems: where settings live,
//! how executables are found and how a terminal is opened.
//!
//! Linux and macOS are supported. Windows has working fallbacks for the
//! locations and executable search; opening a terminal reports that it is
//! not supported yet. Keep OS checks inside this module.
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// Name shown in the window title and used for settings folders.
pub const APP_NAME: &str = "Soul Agent";

/// Folder for the app's settings file.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home().map(|home| home.join("Library/Application Support").join(APP_NAME))
    } else if cfg!(windows) {
        env_dir("APPDATA").map(|dir| dir.join(APP_NAME))
    } else {
        env_dir("XDG_CONFIG_HOME")
            .or_else(|| home().map(|home| home.join(".config")))
            .map(|dir| dir.join("soul-agent"))
    }
}

/// Folder for files the app writes on the user's behalf, such as the
/// production log.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        config_dir()
    } else if cfg!(windows) {
        env_dir("LOCALAPPDATA").map(|dir| dir.join(APP_NAME))
    } else {
        env_dir("XDG_DATA_HOME")
            .or_else(|| home().map(|home| home.join(".local/share")))
            .map(|dir| dir.join("soul-agent"))
    }
}

fn home() -> Option<PathBuf> {
    env_dir(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
}

/// An environment variable holding an absolute directory; relative values
/// are ignored, as the XDG specification requires.
fn env_dir(name: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(name)?);
    dir.is_absolute().then_some(dir)
}

/// Folders searched for executables besides `PATH`.
///
/// Apps started from a desktop launcher or the macOS Dock do not get the
/// shell's `PATH`, so common toolchain locations are listed here.
pub fn extra_tool_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        return dirs;
    }
    if let Some(home) = home() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join("bin"));
    }
    dirs.push("/usr/local/bin".into());
    dirs.push("/usr/bin".into());
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".into());
        // Arm's installer puts each release in its own folder.
        dirs.extend(glob_dirs(
            Path::new("/Applications/ArmGNUToolchain"),
            "arm-none-eabi/bin",
        ));
    } else {
        dirs.extend(glob_dirs(Path::new("/opt"), "bin"));
        dirs.extend(glob_dirs(Path::new("/opt"), "arm-none-eabi/bin"));
    }
    dirs
}

/// `<parent>/*/<child>` for every directory directly in `parent`, newest
/// name last.
fn glob_dirs(parent: &Path, child: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join(child))
        .filter(|dir| dir.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Finds `name` (without extension) in `PATH` and [`extra_tool_dirs`].
pub fn find_executable(name: &str) -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .map(|dir| dir.join(executable_file_name(name)))
        .find(|path| is_executable(path))
}

/// `PATH` followed by [`extra_tool_dirs`], without duplicates.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for dir in extra_tool_dirs() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// `name` with the platform's executable extension.
pub fn executable_file_name(name: &str) -> OsString {
    let mut file = OsString::from(name);
    file.push(std::env::consts::EXE_SUFFIX);
    file
}

/// Whether `path` is a file this user may run.
pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Quotes one argument for the shell a user pastes commands into.
pub fn shell_quote(arg: &str) -> String {
    if cfg!(windows) {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"', '&', '^', '|', '<', '>']) {
            return arg.to_owned();
        }
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        let plain = |c: char| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c);
        if !arg.is_empty() && arg.chars().all(plain) {
            return arg.to_owned();
        }
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// A command line built from quoted arguments.
pub fn command_line<S: AsRef<str>>(args: &[S]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `args` in a new terminal window, starting in `cwd`.
///
/// The window stays open after the command exits so its output can be read.
pub fn open_in_terminal(cwd: Option<&Path>, args: &[String]) -> io::Result<()> {
    if args.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command"));
    }
    #[cfg(unix)]
    {
        unix_terminal::open(cwd, args)
    }
    #[cfg(not(unix))]
    {
        let _ = cwd;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opening a terminal is not supported on this system yet; copy the command instead",
        ))
    }
}

#[cfg(unix)]
mod unix_terminal {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Terminal emulators tried in order, with the arguments that make them
    /// run a script. `x-terminal-emulator` is the Debian/Ubuntu default.
    const LINUX_TERMINALS: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e"]),
        ("ptyxis", &["--new-window", "--"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xfce4-terminal", &["-x"]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("foot", &[]),
        ("xterm", &["-e"]),
    ];

    pub fn open(cwd: Option<&Path>, args: &[String]) -> io::Result<()> {
        let script = write_script(cwd, args)?;
        let mut command = terminal_command(&script)?;
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        // Reap the launcher so it does not linger as a zombie.
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    fn write_script(cwd: Option<&Path>, args: &[String]) -> io::Result<PathBuf> {
        let extension = if cfg!(target_os = "macos") {
            // Terminal.app runs .command files.
            "command"
        } else {
            "sh"
        };
        let path = std::env::temp_dir().join(format!(
            "soul-agent-{}-{}.{extension}",
            std::process::id(),
            unique()
        ));
        let mut text = String::from("#!/bin/sh\n");
        if let Some(cwd) = cwd {
            text += &format!("cd {} || exit 1\n", shell_quote(&cwd.to_string_lossy()));
        }
        text += &format!("rm -f {}\n", shell_quote(&path.to_string_lossy()));
        text += &command_line(args);
        text += "\nstatus=$?\necho\necho \"[exited with status $status; press Enter to close]\"\nread _\n";
        std::fs::write(&path, text)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(path)
    }

    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    fn terminal_command(script: &Path) -> io::Result<Command> {
        if cfg!(target_os = "macos") {
            let mut command = Command::new("open");
            command.args(["-a", "Terminal"]).arg(script);
            return Ok(command);
        }
        let preferred = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty());
        if let Some(terminal) = preferred.and_then(|t| find_executable(&t)) {
            let mut command = Command::new(terminal);
            command.arg("-e").arg(script);
            return Ok(command);
        }
        for (name, args) in LINUX_TERMINALS {
            if let Some(path) = find_executable(name) {
                let mut command = Command::new(path);
                command.args(*args).arg(script);
                return Ok(command);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no terminal emulator found; copy the command instead",
        ))
    }
}

/// Opens a folder or file with the system's default application.
pub fn reveal(path: &Path) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(opener).arg(path).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn quotes_for_posix_shells() {
        assert_eq!(shell_quote("arm-none-eabi-gdb"), "arm-none-eabi-gdb");
        assert_eq!(shell_quote("/tmp/my app.elf"), "'/tmp/my app.elf'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            command_line(&["gdb", "-ex", "target remote 127.0.0.1:3333"]),
            "gdb -ex 'target remote 127.0.0.1:3333'"
        );
    }

    #[test]
    fn executable_names_get_the_platform_suffix() {
        let name = executable_file_name("gdb");
        assert_eq!(
            name,
            OsString::from(format!("gdb{}", std::env::consts::EXE_SUFFIX))
        );
    }
}
