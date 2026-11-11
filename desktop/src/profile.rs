//! Saved debug profiles and app settings.
//!
//! A profile is everything needed to program and debug one product: its
//! target description, images and debug symbols, and how to debug it. The
//! whole state is one JSON file in [`crate::platform::config_dir`].
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

/// GDB port used when a profile does not set one.
pub const DEFAULT_GDB_PORT: u16 = 3333;

/// How a debug session starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugMode {
    /// Push the job, program the target once, then halt it at reset.
    #[default]
    ProgramThenDebug,
    /// Attach to whatever the target is already running.
    Attach,
}

/// Debug server options of one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DebugOptions {
    /// How the session starts.
    pub mode: DebugMode,
    /// Local TCP port GDB connects to.
    pub port: u16,
    /// Hold the target in reset while connecting.
    pub connect_under_reset: bool,
    /// Let the target run when GDB detaches cleanly.
    pub resume_on_detach: bool,
    /// Attach again and keep listening after GDB disconnects.
    pub keep_serving: bool,
}

impl Default for DebugOptions {
    fn default() -> Self {
        Self {
            mode: DebugMode::default(),
            port: DEFAULT_GDB_PORT,
            connect_under_reset: false,
            resume_on_detach: false,
            keep_serving: true,
        }
    }
}

/// One product's target, files and debug options.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Name shown in the profile list.
    pub name: String,
    /// target.yaml.
    pub target: Option<PathBuf>,
    /// Variant to use when target.yaml lists several.
    pub variant: Option<String>,
    /// Local file for each device image name, e.g. `firmware.bin`.
    pub images: BTreeMap<String, PathBuf>,
    /// ELF with debug symbols, given to GDB.
    pub elf: Option<PathBuf>,
    /// pre_prog.yaml.
    pub pre_program: Option<PathBuf>,
    /// post_prog.yaml.
    pub post_program: Option<PathBuf>,
    /// Job name the device shows; the variant name when empty.
    pub job_name: Option<String>,
    /// Program whenever a target is plugged in (Programming section only).
    pub auto_run: bool,
    /// Debug server options.
    pub debug: DebugOptions,
}

impl Default for Profile {
    fn default() -> Self {
        Self::named("New target")
    }
}

impl Profile {
    /// An empty profile with a name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            target: None,
            variant: None,
            images: BTreeMap::new(),
            elf: None,
            pre_program: None,
            post_program: None,
            job_name: None,
            auto_run: false,
            debug: DebugOptions::default(),
        }
    }

    /// The folder relative paths and terminals start from: the ELF's or
    /// target.yaml's folder.
    pub fn project_dir(&self) -> Option<&Path> {
        self.elf
            .as_deref()
            .or(self.target.as_deref())
            .and_then(Path::parent)
            .filter(|dir| !dir.as_os_str().is_empty())
    }
}

/// Settings shared by every profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// GDB executable; detected when unset.
    pub gdb: Option<PathBuf>,
    /// NTP server that gives the device its clock.
    pub ntp_server: String,
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Wait after opening the port before the first request, in milliseconds.
    pub settle_ms: u64,
    /// File production log entries are appended to.
    pub log_file: Option<PathBuf>,
    /// Dark (`true`) or light theme; follows the system when unset.
    pub dark: Option<bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gdb: None,
            ntp_server: sidp_agent::ntp::DEFAULT_SERVER.to_owned(),
            timeout_ms: 5000,
            settle_ms: 250,
            log_file: None,
            dark: None,
        }
    }
}

impl Settings {
    /// Where pulled log entries go.
    pub fn log_file(&self) -> PathBuf {
        self.log_file.clone().unwrap_or_else(|| {
            crate::platform::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("production.jsonl")
        })
    }
}

/// Everything the app remembers between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// File format version.
    pub version: u32,
    /// Saved profiles; never empty once loaded.
    pub profiles: Vec<Profile>,
    /// Index of the profile shown.
    pub selected: usize,
    /// Serial port of the last device used.
    pub port: Option<String>,
    /// Shared settings.
    pub settings: Settings,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: vec![Profile::named("My target")],
            selected: 0,
            port: None,
            settings: Settings::default(),
        }
    }
}

impl State {
    /// The state file in the config folder.
    pub fn default_path() -> Option<PathBuf> {
        crate::platform::config_dir().map(|dir| dir.join("state.json"))
    }

    /// Reads the state, or the default one when the file does not exist.
    ///
    /// A file that cannot be parsed is renamed to `*.bad` so that it is
    /// not overwritten; the error says so.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut state) => {
                state.normalize();
                Ok(state)
            }
            Err(e) => {
                let backup = path.with_extension("json.bad");
                let _ = std::fs::rename(path, &backup);
                Err(format!(
                    "{} is damaged ({e}); it was kept as {} and defaults are used",
                    path.display(),
                    backup.display()
                ))
            }
        }
    }

    /// Writes the state atomically: a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }

    /// Restores the invariants the UI relies on.
    fn normalize(&mut self) {
        if self.profiles.is_empty() {
            self.profiles.push(Profile::named("My target"));
        }
        self.selected = self.selected.min(self.profiles.len() - 1);
        for profile in &mut self.profiles {
            if profile.debug.port == 0 {
                profile.debug.port = DEFAULT_GDB_PORT;
            }
        }
    }

    /// The profile shown.
    pub fn profile(&self) -> &Profile {
        &self.profiles[self.selected]
    }

    /// The profile shown, for editing.
    pub fn profile_mut(&mut self) -> &mut Profile {
        &mut self.profiles[self.selected]
    }

    /// A name no other profile has, based on `base`.
    pub fn unique_name(&self, base: &str) -> String {
        let taken = |name: &str| self.profiles.iter().any(|p| p.name == name);
        if !taken(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base} {n}"))
            .find(|name| !taken(name))
            .expect("unbounded")
    }

    /// Adds a profile and selects it.
    pub fn add_profile(&mut self, profile: Profile) {
        self.profiles.push(profile);
        self.selected = self.profiles.len() - 1;
    }

    /// Removes the selected profile, keeping at least one.
    pub fn remove_selected(&mut self) {
        if self.profiles.len() > 1 {
            self.profiles.remove(self.selected);
        } else {
            self.profiles[0] = Profile::named("My target");
        }
        self.selected = self.selected.min(self.profiles.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.json");
        let mut state = State::default();
        state.profile_mut().target = Some("/work/target.yaml".into());
        state
            .profile_mut()
            .images
            .insert("firmware.bin".into(), "/work/app.bin".into());
        state.add_profile(Profile::named("Second"));
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::load(&dir.path().join("none.json")).unwrap();
        assert_eq!(state.profiles.len(), 1);
    }

    #[test]
    fn damaged_file_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        let error = State::load(&path).unwrap_err();
        assert!(error.contains("damaged"), "{error}");
        assert!(dir.path().join("state.json.bad").exists());
        assert!(!path.exists());
    }

    #[test]
    fn older_files_get_defaults_for_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"profiles":[{"name":"A","debug":{"port":0}}],"selected":7}"#,
        )
        .unwrap();
        let state = State::load(&path).unwrap();
        assert_eq!(state.selected, 0);
        assert_eq!(state.profile().debug.port, DEFAULT_GDB_PORT);
        assert!(state.profile().debug.keep_serving);
        assert_eq!(state.settings.timeout_ms, 5000);
    }

    #[test]
    fn names_and_removal() {
        let mut state = State::default();
        assert_eq!(state.unique_name("My target"), "My target 2");
        state.add_profile(Profile::named("My target 2"));
        assert_eq!(state.unique_name("My target"), "My target 3");
        state.remove_selected();
        assert_eq!(state.profiles.len(), 1);
        state.remove_selected();
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(state.selected, 0);
    }
}
