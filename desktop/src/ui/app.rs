//! State and behavior of the main window. Rendering is in the sibling
//! modules; this one decides what happens.
use super::power::{PowerRequest, PowerView};
use crate::{
    describe, platform,
    ports::{self, Port},
    power::Source,
    profile::{DebugMode, Mode, Profile, State},
    snippets::{self, Client, DebugTarget},
    target::{self, BuiltJob, Family, TargetSummary},
    toolchain::{self, Gdb},
    worker::{
        Connection, DebugPlan, DeviceSnapshot, Event, GdbState, Level, NetworkSnapshot, Operation,
        Outcome, Worker,
    },
};
use futures::{StreamExt as _, channel::mpsc};
use gpui_kit::component::{
    IndexPath, Theme, ThemeMode, WindowExt as _,
    input::{InputEvent, InputState},
    notification::Notification,
    select::{SelectEvent, SelectState},
};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, PathPromptOptions, SharedString, Subscription,
    Task, Window,
};
use sidp_agent::listener::Listener;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Keychain entry of a device's password.
fn credential_url(device_id: &str) -> String {
    format!("sidp://{device_id}")
}
/// User name stored with it; the login's user name.
const CREDENTIAL_USER: &str = sidp_agent::login::USERNAME;

/// Lines kept in the activity log.
const LOG_LIMIT: usize = 500;
/// How often USB ports are listed to notice plugging and unplugging.
const PORT_SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// Quiet time after an edit before files are re-checked.
const CHECK_DELAY: Duration = Duration::from_millis(300);
/// Quiet time after an edit before the state file is written.
const SAVE_DELAY: Duration = Duration::from_millis(600);

/// The kind of device operation running, for labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Reading device status.
    Inspect,
    /// Sending the job without running it.
    Push,
    /// Programming once.
    Program,
    /// Collecting the production log.
    PullLog,
    /// Serving GDB.
    Debug,
    /// Recording power from the Soul Injector.
    Power,
    /// Measuring the power profiler's zero offset.
    PowerZero,
    /// Reading or storing network settings.
    Network,
}

/// The operation in progress.
pub struct Running {
    /// What it is.
    pub activity: Activity,
    /// What it does right now.
    pub step: SharedString,
    /// Completion of the step, when known.
    pub progress: Option<f32>,
    /// The user asked it to stop.
    pub stopping: bool,
    /// The job is still being compiled; nothing has touched the device.
    pub preparing: bool,
}

/// Whether the profile's files make a job.
pub enum Readiness {
    /// Not checked yet.
    Checking,
    /// The job compiles.
    Ready(JobInfo),
    /// Something is missing or wrong; the text says what.
    Problem(SharedString),
}

/// What a compiled job contains, for display.
pub struct JobInfo {
    /// Name the device shows.
    pub name: String,
    /// Programming family.
    pub family: Family,
    /// Image names and sizes.
    pub images: Vec<(String, usize)>,
    /// Compiler notes.
    pub warnings: Vec<String>,
}

/// One activity log line.
pub struct LogLine {
    /// Local time, HH:MM:SS.
    pub time: SharedString,
    /// Importance.
    pub level: Level,
    /// Text.
    pub text: SharedString,
}

/// A file field whose path can be chosen with Browse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathField {
    /// target.yaml.
    Target,
    /// Debug symbols.
    Elf,
    /// pre_prog.yaml.
    PreProgram,
    /// post_prog.yaml.
    PostProgram,
    /// The image with this device name.
    Image(String),
    /// The GDB executable.
    Gdb,
}

/// The input for one image file.
pub struct ImageInput {
    /// Device name, e.g. `firmware.bin`.
    pub name: String,
    /// Path input.
    pub input: Entity<InputState>,
    _subscription: Subscription,
}

/// The main window's view.
pub struct AppView {
    pub(super) state: State,
    state_path: Option<PathBuf>,
    save_task: Option<Task<()>>,

    pub(super) name_input: Entity<InputState>,
    pub(super) target_input: Entity<InputState>,
    pub(super) elf_input: Entity<InputState>,
    pub(super) pre_input: Entity<InputState>,
    pub(super) post_input: Entity<InputState>,
    pub(super) job_name_input: Entity<InputState>,
    pub(super) port_input: Entity<InputState>,
    pub(super) image_inputs: Vec<ImageInput>,
    pub(super) variant_select: Entity<SelectState<Vec<SharedString>>>,
    pub(super) gdb_select: Entity<SelectState<Vec<SharedString>>>,
    pub(super) ntp_input: Entity<InputState>,
    pub(super) timeout_input: Entity<InputState>,
    pub(super) settle_input: Entity<InputState>,
    pub(super) ssid_input: Entity<InputState>,
    pub(super) wifi_password_input: Entity<InputState>,
    pub(super) server_input: Entity<InputState>,
    pub(super) device_password_input: Entity<InputState>,
    pub(super) listen_port_input: Entity<InputState>,
    /// The password typed for a device on WiFi.
    pub(super) login_input: Entity<InputState>,
    /// Holds Soul Injectors that connect over WiFi; none in tests.
    listener: Option<Arc<Listener>>,
    /// Passwords of devices on WiFi, by port; also kept in the system keychain.
    passwords: HashMap<String, String>,
    /// Ports whose password was looked up in the keychain this run.
    recalled: HashSet<String>,
    pub(super) network_enabled: bool,
    pub(super) network: Option<NetworkSnapshot>,

    pub(super) summary: Option<Result<TargetSummary, String>>,
    pub(super) readiness: Readiness,
    check_task: Option<Task<()>>,

    pub(super) ports: Vec<Port>,
    pub(super) device: Option<DeviceSnapshot>,
    pub(super) device_error: Option<SharedString>,
    pub(super) gdbs: Vec<Gdb>,
    pub(super) gdbs_scanned: bool,

    pub(super) power: Entity<PowerView>,
    /// The power capture was started with the debug session.
    power_with_debug: bool,
    worker: Option<Worker>,
    pub(super) running: Option<Running>,
    pub(super) gdb_state: GdbState,
    pub(super) log: VecDeque<LogLine>,
    pub(super) log_scroll: gpui_kit::ScrollHandle,
    pub(super) page_scroll: gpui_kit::ScrollHandle,
    pub(super) show_log: bool,
    pub(super) show_more: bool,
    pub(super) client: Client,
    events: mpsc::UnboundedSender<Event>,

    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn text_input(
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<AppView>,
) -> Entity<InputState> {
    let placeholder = placeholder.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

fn number_input(window: &mut Window, cx: &mut Context<AppView>) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx).pattern(regex::Regex::new(r"^\d{0,6}$").expect("valid regex"))
    })
}

fn path_text(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn non_empty_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

impl AppView {
    /// Builds the view with the user's saved state and starts watching USB
    /// ports for Soul Injectors.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::open(State::default_path(), window, cx);
        this.start_listener();
        this.watch_ports(window, cx);
        this
    }

    /// Listens for Soul Injectors on WiFi. Failing leaves USB working.
    fn start_listener(&mut self) {
        let port = self.state.settings.listen_port;
        let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
        match Listener::start(address, |_| {}) {
            Ok(listener) => self.listener = Some(Arc::new(listener)),
            Err(error) => self.add_log(
                Level::Warning,
                format!("Soul Injectors on WiFi cannot connect: {error}"),
            ),
        }
    }

    /// Builds the view with the state saved in `state_path` (none: nothing is
    /// loaded or saved) and looks for GDB. It does not watch USB ports, so
    /// tests built on it never open a Soul Injector plugged into the computer.
    pub fn open(state_path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (state, load_error) = match state_path.as_deref().map(State::load) {
            Some(Ok(state)) => (state, None),
            Some(Err(error)) => (State::default(), Some(error)),
            None => (State::default(), None),
        };
        let (events, receiver) = mpsc::unbounded();

        let variant_select =
            cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));
        let gdb_select =
            cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));

        let mut this = Self {
            name_input: text_input("Profile name", window, cx),
            target_input: text_input("Path to target.yaml", window, cx),
            elf_input: text_input("Path to the firmware ELF (optional)", window, cx),
            pre_input: text_input("Optional pre_prog.yaml", window, cx),
            post_input: text_input("Optional post_prog.yaml", window, cx),
            job_name_input: text_input("Defaults to the variant name", window, cx),
            port_input: number_input(window, cx),
            ntp_input: text_input(sidp_agent::ntp::DEFAULT_SERVER, window, cx),
            timeout_input: number_input(window, cx),
            settle_input: number_input(window, cx),
            ssid_input: text_input("Network name", window, cx),
            wifi_password_input: cx.new(|cx| InputState::new(window, cx).masked(true)),
            server_input: text_input("ws://ADDRESS/", window, cx),
            device_password_input: cx.new(|cx| InputState::new(window, cx).masked(true)),
            listen_port_input: number_input(window, cx),
            login_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Device password")
            }),
            listener: None,
            passwords: HashMap::new(),
            recalled: HashSet::new(),
            network_enabled: true,
            network: None,
            image_inputs: Vec::new(),
            variant_select,
            gdb_select,
            state,
            state_path,
            save_task: None,
            summary: None,
            readiness: Readiness::Checking,
            check_task: None,
            ports: Vec::new(),
            device: None,
            device_error: None,
            gdbs: Vec::new(),
            gdbs_scanned: false,
            power: cx.new(PowerView::new),
            power_with_debug: false,
            worker: None,
            running: None,
            gdb_state: GdbState::Idle,
            log: VecDeque::new(),
            log_scroll: gpui_kit::ScrollHandle::new(),
            page_scroll: gpui_kit::ScrollHandle::new(),
            show_log: false,
            show_more: false,
            client: Client::Gdb,
            events,
            _tasks: Vec::new(),
            _subscriptions: Vec::new(),
        };

        let compact = this.state.mode == Mode::Debug;
        this.power
            .update(cx, |power, cx| power.set_compact(compact, cx));
        // The mode bar shows whether power is being measured.
        let mut capturing = false;
        this._subscriptions
            .push(cx.observe(&this.power, move |_, power, cx| {
                let now = power.read(cx).is_capturing();
                if now != capturing {
                    capturing = now;
                    cx.notify();
                }
            }));
        this._subscriptions.push(
            cx.subscribe(&this.power, |this, _, request: &PowerRequest, cx| {
                this.on_power_request(*request, cx)
            }),
        );
        this.subscribe(window, cx);
        this.load_profile(window, cx);
        this.load_settings(window, cx);
        this.apply_theme(window, cx);

        let events_task = cx.spawn_in(window, async move |this, cx| {
            let mut receiver = receiver;
            while let Some(event) = receiver.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.on_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        this._tasks.push(events_task);
        this.scan_gdbs(window, cx);

        this._subscriptions.push(cx.on_app_quit(|this, cx| {
            this.shut_down(cx);
            async {}
        }));
        this._subscriptions
            .push(cx.observe_window_appearance(window, |this, window, cx| {
                if this.state.settings.dark.is_none() {
                    this.apply_theme(window, cx);
                }
            }));
        if let Some(error) = load_error {
            this.add_log(Level::Warning, error);
        }
        this.add_log(Level::Info, format!("{} started", platform::APP_NAME));
        this
    }

    // ---- Read access, for the views and tests ---------------------------------

    /// Profiles and settings as they are now.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Whether the shown profile makes a job.
    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Device names of the image fields shown.
    pub fn image_names(&self) -> Vec<&str> {
        self.image_inputs.iter().map(|i| i.name.as_str()).collect()
    }

    /// The page shown.
    pub fn mode(&self) -> Mode {
        self.state.mode
    }

    /// The power measurement.
    pub fn power(&self) -> &Entity<PowerView> {
        &self.power
    }

    /// Shows another page.
    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.state.mode == mode {
            return;
        }
        self.state.mode = mode;
        self.power
            .update(cx, |power, cx| power.set_compact(mode == Mode::Debug, cx));
        self.save_soon(cx);
        cx.notify();
    }

    /// The debugger whose settings are shown.
    pub fn client(&self) -> Client {
        self.client
    }

    /// The activity log, oldest first.
    pub fn log_lines(&self) -> impl Iterator<Item = &LogLine> {
        self.log.iter()
    }

    // ---- Inputs ------------------------------------------------------------

    fn subscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let mut on_change =
            |input: &Entity<InputState>,
             apply: fn(&mut Self, &str, &mut Window, &mut Context<Self>)| {
                subscriptions.push(cx.subscribe_in(
                    input,
                    window,
                    move |this, input, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::Change) {
                            let value = input.read(cx).value().to_string();
                            apply(this, &value, window, cx);
                        }
                    },
                ));
            };

        on_change(&self.name_input, |this, value, _, cx| {
            let name = value.trim();
            if !name.is_empty() && this.state.profile().name != name {
                this.state.profile_mut().name = name.to_owned();
                this.save_soon(cx);
                cx.notify();
            }
        });
        on_change(&self.target_input, |this, value, window, cx| {
            this.edit_profile(window, cx, |p| p.target = non_empty_path(value));
        });
        on_change(&self.elf_input, |this, value, window, cx| {
            this.edit_profile(window, cx, |p| p.elf = non_empty_path(value));
        });
        on_change(&self.pre_input, |this, value, window, cx| {
            this.edit_profile(window, cx, |p| p.pre_program = non_empty_path(value));
        });
        on_change(&self.post_input, |this, value, window, cx| {
            this.edit_profile(window, cx, |p| p.post_program = non_empty_path(value));
        });
        on_change(&self.job_name_input, |this, value, window, cx| {
            let name = value.trim();
            this.edit_profile(window, cx, |p| {
                p.job_name = (!name.is_empty()).then(|| name.to_owned());
            });
        });
        on_change(&self.port_input, |this, value, _, cx| {
            if let Ok(port) = value.parse::<u16>()
                && port != 0
                && this.state.profile().debug.port != port
            {
                this.state.profile_mut().debug.port = port;
                this.save_soon(cx);
                cx.notify();
            }
        });
        on_change(&self.ntp_input, |this, value, _, cx| {
            this.state.settings.ntp_server = value.trim().to_owned();
            this.save_soon(cx);
        });
        on_change(&self.timeout_input, |this, value, _, cx| {
            if let Ok(ms) = value.parse::<u64>()
                && (100..=60_000).contains(&ms)
            {
                this.state.settings.timeout_ms = ms;
                this.save_soon(cx);
            }
        });
        on_change(&self.listen_port_input, |this, value, _, cx| {
            if let Ok(port) = value.parse::<u16>()
                && port != 0
            {
                this.state.settings.listen_port = port;
                this.save_soon(cx);
            }
        });
        on_change(&self.settle_input, |this, value, _, cx| {
            if let Ok(ms) = value.parse::<u64>()
                && ms <= 10_000
            {
                this.state.settings.settle_ms = ms;
                this.save_soon(cx);
            }
        });

        subscriptions.push(cx.subscribe_in(
            &self.variant_select,
            window,
            |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                let SelectEvent::Confirm(value) = event;
                let variant = value.as_ref().map(|v| v.to_string());
                this.edit_profile(window, cx, |p| p.variant = variant);
                this.refresh_target_fields(window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &self.gdb_select,
            window,
            |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                this.state.settings.gdb = Some(PathBuf::from(value.as_ref()));
                this.save_soon(cx);
                cx.notify();
            },
        ));
        self._subscriptions.extend(subscriptions);
    }

    /// Applies an edit to the shown profile, then saves and re-checks it.
    fn edit_profile(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut Profile),
    ) {
        let before = self.state.profile().clone();
        edit(self.state.profile_mut());
        if *self.state.profile() != before {
            self.save_soon(cx);
            self.schedule_check(window, cx);
            cx.notify();
        }
    }

    /// Fills every input from the selected profile.
    fn load_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.state.profile().clone();
        let set = |input: &Entity<InputState>,
                   value: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name_input, profile.name.clone(), window, cx);
        set(
            &self.target_input,
            path_text(profile.target.as_deref()),
            window,
            cx,
        );
        set(
            &self.elf_input,
            path_text(profile.elf.as_deref()),
            window,
            cx,
        );
        set(
            &self.pre_input,
            path_text(profile.pre_program.as_deref()),
            window,
            cx,
        );
        set(
            &self.post_input,
            path_text(profile.post_program.as_deref()),
            window,
            cx,
        );
        set(
            &self.job_name_input,
            profile.job_name.clone().unwrap_or_default(),
            window,
            cx,
        );
        set(&self.port_input, profile.debug.port.to_string(), window, cx);
        self.show_more = profile.pre_program.is_some()
            || profile.post_program.is_some()
            || profile.job_name.is_some();
        self.summary = None;
        self.image_inputs.clear();
        self.readiness = Readiness::Checking;
        self.refresh_target_fields(window, cx);
        self.schedule_check(window, cx);
        cx.notify();
    }

    fn load_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.state.settings.clone();
        self.ntp_input.update(cx, |input, cx| {
            input.set_value(settings.ntp_server, window, cx)
        });
        self.timeout_input.update(cx, |input, cx| {
            input.set_value(settings.timeout_ms.to_string(), window, cx)
        });
        self.settle_input.update(cx, |input, cx| {
            input.set_value(settings.settle_ms.to_string(), window, cx)
        });
        self.listen_port_input.update(cx, |input, cx| {
            input.set_value(settings.listen_port.to_string(), window, cx)
        });
    }

    /// Rebuilds the variant choice and the image inputs from the summary.
    fn refresh_target_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.state.profile().clone();
        let summary = self.summary.as_ref().and_then(|s| s.as_ref().ok());

        let variants: Vec<SharedString> = summary
            .map(|s| {
                s.variants
                    .iter()
                    .filter(|v| !v.name.is_empty())
                    .map(|v| SharedString::from(v.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let selected = profile
            .variant
            .as_deref()
            .and_then(|name| variants.iter().position(|v| v.as_ref() == name))
            .map(IndexPath::new);
        self.variant_select.update(cx, |select, cx| {
            select.set_items(variants, window, cx);
            select.set_selected_index(selected, window, cx);
        });

        // Without a usable target.yaml, offer the Cortex-M firmware image,
        // which is what most profiles need.
        let wanted: Vec<String> = match self.summary.as_ref() {
            None => vec![sidp_agent::manage::FIRMWARE_NAME.to_owned()],
            Some(Ok(summary)) => summary
                .variant(profile.variant.as_deref())
                .map(|v| v.images.clone())
                .unwrap_or_default(),
            Some(Err(_)) => self.image_inputs.iter().map(|i| i.name.clone()).collect(),
        };
        if wanted.iter().eq(self.image_inputs.iter().map(|i| &i.name)) {
            return;
        }
        let mut old: Vec<ImageInput> = std::mem::take(&mut self.image_inputs);
        for name in wanted {
            if let Some(ix) = old.iter().position(|i| i.name == name) {
                self.image_inputs.push(old.remove(ix));
                continue;
            }
            let placeholder = describe::capitalize(&target::image_description(&name));
            let input = text_input(&placeholder, window, cx);
            let value = path_text(profile.images.get(&name).map(PathBuf::as_path));
            input.update(cx, |input, cx| input.set_value(value, window, cx));
            let key = name.clone();
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let value = input.read(cx).value().to_string();
                    let key = key.clone();
                    this.edit_profile(window, cx, |p| match non_empty_path(&value) {
                        Some(path) => {
                            p.images.insert(key, path);
                        }
                        None => {
                            p.images.remove(&key);
                        }
                    });
                },
            );
            self.image_inputs.push(ImageInput {
                name,
                input,
                _subscription: subscription,
            });
        }
        cx.notify();
    }

    // ---- Checking and saving ------------------------------------------------

    /// Re-reads the profile's files shortly after the last edit.
    fn schedule_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.state.profile().clone();
        self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(CHECK_DELAY).await;
            let (summary, job) = cx
                .background_executor()
                .spawn(async move {
                    let summary = profile.target.as_deref().map(TargetSummary::load);
                    (summary, target::build_job(&profile))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.on_checked(summary, job, window, cx)
            });
        }));
    }

    fn on_checked(
        &mut self,
        summary: Option<Result<TargetSummary, String>>,
        job: Result<BuiltJob, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.summary = summary;
        self.refresh_target_fields(window, cx);
        self.readiness = match job {
            Ok(job) => Readiness::Ready(JobInfo {
                name: job.name.clone(),
                family: job.family,
                images: job
                    .images
                    .iter()
                    .map(|(name, data)| (name.clone(), data.len()))
                    .collect(),
                warnings: job.warnings.clone(),
            }),
            Err(problem) => Readiness::Problem(problem.into()),
        };
        cx.notify();
    }

    /// Writes the state file shortly after the last change.
    fn save_soon(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, _| this.save_now());
        }));
    }

    fn save_now(&mut self) {
        self.save_task = None;
        let Some(path) = self.state_path.clone() else {
            return;
        };
        if let Err(error) = self.state.save(&path) {
            self.add_log(
                Level::Error,
                format!("Cannot save settings to {}: {error}", path.display()),
            );
        }
    }

    // ---- Profiles ------------------------------------------------------------

    /// Shows another profile.
    pub fn select_profile(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.state.selected || index >= self.state.profiles.len() || self.is_busy() {
            return;
        }
        self.state.selected = index;
        self.save_soon(cx);
        self.load_profile(window, cx);
    }

    /// Adds an empty profile and shows it.
    pub fn new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let name = self.state.unique_name("New target");
        self.state.add_profile(Profile::named(name));
        self.save_soon(cx);
        self.load_profile(window, cx);
        self.name_input
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Copies the shown profile and shows the copy.
    pub fn duplicate_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let mut copy = self.state.profile().clone();
        copy.name = self.state.unique_name(&format!("{} copy", copy.name));
        self.state.add_profile(copy);
        self.save_soon(cx);
        self.load_profile(window, cx);
    }

    /// Asks before deleting the shown profile.
    pub fn confirm_delete_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let view = cx.entity();
        let name = self.state.profile().name.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .confirm()
                .title("Delete this profile?")
                .description(format!(
                    "\"{name}\" will be removed from {}. Your files are not changed.",
                    platform::APP_NAME
                ))
                .ok_text("Delete")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.delete_profile(window, cx));
                    true
                })
        });
    }

    fn delete_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.state.remove_selected();
        self.save_soon(cx);
        self.load_profile(window, cx);
    }

    // ---- Files -----------------------------------------------------------------

    /// Lets the user pick a file for `field`.
    pub fn browse(&mut self, field: PathField, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = match &field {
            PathField::Target => "Choose target.yaml".to_owned(),
            PathField::Elf => "Choose the firmware ELF".to_owned(),
            PathField::PreProgram => "Choose pre_prog.yaml".to_owned(),
            PathField::PostProgram => "Choose post_prog.yaml".to_owned(),
            PathField::Image(name) => format!("Choose the {}", target::image_description(name)),
            PathField::Gdb => "Choose a GDB executable".to_owned(),
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.add_log(
                            Level::Error,
                            format!("Cannot open the file chooser: {error}"),
                        );
                        cx.notify();
                    });
                    None
                }
                _ => None,
            };
            if let Some(path) = path {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.set_path(field, path, window, cx)
                });
            }
        })
        .detach();
    }

    fn set_path(
        &mut self,
        field: PathField,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = path_text(Some(&path));
        let input = match &field {
            PathField::Target => Some(self.target_input.clone()),
            PathField::Elf => Some(self.elf_input.clone()),
            PathField::PreProgram => Some(self.pre_input.clone()),
            PathField::PostProgram => Some(self.post_input.clone()),
            PathField::Image(name) => self
                .image_inputs
                .iter()
                .find(|i| &i.name == name)
                .map(|i| i.input.clone()),
            PathField::Gdb => None,
        };
        if let Some(input) = input {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        // `set_value` emits no change event, so the profile is updated here.
        match field {
            PathField::Target => self.edit_profile(window, cx, |p| p.target = Some(path)),
            PathField::Elf => self.edit_profile(window, cx, |p| p.elf = Some(path)),
            PathField::PreProgram => self.edit_profile(window, cx, |p| p.pre_program = Some(path)),
            PathField::PostProgram => {
                self.edit_profile(window, cx, |p| p.post_program = Some(path))
            }
            PathField::Image(name) => self.edit_profile(window, cx, |p| {
                p.images.insert(name, path);
            }),
            PathField::Gdb => {
                self.state.settings.gdb = Some(path);
                self.save_soon(cx);
                self.refresh_gdb_select(window, cx);
                cx.notify();
            }
        }
    }

    /// Chooses where pulled log entries are appended.
    pub fn choose_log_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.state.settings.log_file();
        let directory = current
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&directory);
        let name = current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let receiver = cx.prompt_for_new_path(&directory, name.as_deref());
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                let _ = this.update(cx, |this, cx| {
                    this.state.settings.log_file = Some(path);
                    this.save_soon(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ---- Toolchain ---------------------------------------------------------------

    /// Looks for ARM GDBs in the background.
    pub fn scan_gdbs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.spawn_in(window, async move |this, cx| {
            let gdbs = cx
                .background_executor()
                .spawn(async { toolchain::find_gdbs() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.gdbs = gdbs;
                this.gdbs_scanned = true;
                this.refresh_gdb_select(window, cx);
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    fn refresh_gdb_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut items: Vec<SharedString> = self
            .gdbs
            .iter()
            .map(|gdb| gdb.path.to_string_lossy().into_owned().into())
            .collect();
        if let Some(custom) = &self.state.settings.gdb {
            let custom: SharedString = custom.to_string_lossy().into_owned().into();
            if !items.contains(&custom) {
                items.insert(0, custom);
            }
        }
        let selected = self
            .gdb_path()
            .map(|p| SharedString::from(p.to_string_lossy().into_owned()))
            .and_then(|p| items.iter().position(|i| *i == p))
            .map(IndexPath::new);
        self.gdb_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(selected, window, cx);
        });
    }

    /// The GDB snippets and the terminal use.
    pub fn gdb_path(&self) -> Option<PathBuf> {
        self.state
            .settings
            .gdb
            .clone()
            .or_else(|| self.gdbs.first().map(|gdb| gdb.path.clone()))
    }

    /// Version line of the chosen GDB, when it was found by the scan.
    pub fn gdb_version(&self) -> Option<&str> {
        let path = self.gdb_path()?;
        self.gdbs
            .iter()
            .find(|gdb| gdb.path == path)
            .and_then(|gdb| gdb.version.as_deref())
    }

    // ---- Devices ---------------------------------------------------------------

    /// Lists USB ports, and devices connected over WiFi, every few seconds to
    /// notice them coming and going. USB devices come first.
    fn watch_ports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listener = self.listener.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                let listener = listener.clone();
                let ports = cx
                    .background_executor()
                    .spawn(async move {
                        let mut ports = ports::list()?;
                        if let Some(listener) = &listener {
                            ports.extend(ports::network(listener));
                        }
                        Ok(ports)
                    })
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.on_ports(ports, window, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(PORT_SCAN_INTERVAL).await;
            }
        });
        self._tasks.push(task);
    }

    fn on_ports(
        &mut self,
        ports: Result<Vec<Port>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ports = match ports {
            Ok(ports) => ports,
            Err(error) => {
                if !self.ports.is_empty() {
                    self.add_log(Level::Warning, format!("Cannot list USB ports: {error}"));
                }
                Vec::new()
            }
        };
        if ports == self.ports {
            return;
        }
        let was_present = self.selected_port().is_some();
        self.ports = ports;

        // Keep the remembered device; otherwise use the first Soul Injector.
        if self.selected_port().is_none()
            && let Some(port) = self.ports.iter().find(|p| p.soul_injector)
        {
            self.state.port = Some(port.path.clone());
            self.save_soon(cx);
        }
        match self.selected_port() {
            Some(port) if !was_present => {
                self.add_log(
                    Level::Info,
                    format!("{} connected on {}", port.product, port.path),
                );
                self.inspect(window, cx);
            }
            None if was_present => {
                self.add_log(Level::Warning, "The Soul Injector was disconnected");
                self.device = None;
                self.device_error = None;
            }
            _ => {}
        }
        cx.notify();
    }

    /// The chosen device, when it is plugged in.
    pub fn selected_port(&self) -> Option<&Port> {
        let path = self.state.port.as_deref()?;
        self.ports.iter().find(|p| p.path == path)
    }

    /// Chooses the device on `path`.
    pub fn select_port(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if self.state.port.as_deref() != Some(path.as_str()) {
            self.state.port = Some(path);
            self.device = None;
            self.device_error = None;
            self.save_soon(cx);
        }
        self.inspect(window, cx);
    }

    /// Device status for the chosen device, when it is current.
    pub fn device_snapshot(&self) -> Option<&DeviceSnapshot> {
        let port = self.selected_port()?;
        self.device.as_ref().filter(|d| d.port == port.path)
    }

    // ---- Operations --------------------------------------------------------------

    /// Whether an operation is running or being prepared.
    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    fn connection(&self, port: String) -> Connection {
        let settings = &self.state.settings;
        Connection {
            password: self.passwords.get(&port).cloned(),
            port,
            timeout: Duration::from_millis(settings.timeout_ms),
            settle: Duration::from_millis(settings.settle_ms),
            ntp_server: settings.ntp_server.clone(),
            dtr: true,
        }
    }

    /// The GDB port as typed, when it is a usable port number.
    pub fn typed_port(&self, cx: &gpui_kit::App) -> Option<u16> {
        self.port_input
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
    }

    /// Why device actions are unavailable, if they are.
    pub fn device_blocker(&self) -> Option<&'static str> {
        match self.selected_port() {
            None => Some("Connect a Soul Injector with a USB cable."),
            Some(port) if self.needs_password(&port.path) => {
                Some("Enter the device password to use this Soul Injector on WiFi.")
            }
            Some(_) => None,
        }
    }

    /// A device on WiFi whose password has not been typed yet.
    pub fn needs_password(&self, path: &str) -> bool {
        ports::is_network(path) && !self.passwords.contains_key(path)
    }

    /// Remembers the typed password for the chosen device and reads its status.
    pub fn log_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected_port().map(|p| p.path.clone()) else {
            return;
        };
        let password = self.login_input.read(cx).value().to_string();
        if password.is_empty() {
            return;
        }
        self.login_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if let Some(id) = ports::device_id(&path) {
            let write =
                cx.write_credentials(&credential_url(id), CREDENTIAL_USER, password.as_bytes());
            cx.spawn(async move |this, cx| {
                if let Err(error) = write.await {
                    let _ = this.update(cx, |this, cx| {
                        this.add_log(
                            Level::Warning,
                            format!(
                                "The password could not be saved in the system keychain: {error}"
                            ),
                        );
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        self.passwords.insert(path, password);
        self.inspect(window, cx);
    }

    /// Looks the device's password up in the system keychain, once per run,
    /// and reads the device's status when one is found.
    fn recall_password(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = ports::device_id(&path) else {
            return;
        };
        if !self.recalled.insert(path.clone()) {
            return;
        }
        let read = cx.read_credentials(&credential_url(id));
        cx.spawn_in(window, async move |this, cx| {
            let stored = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match stored {
                    Ok(Some((_, secret))) => {
                        if let Ok(password) = String::from_utf8(secret) {
                            this.passwords.insert(path, password);
                            this.inspect(window, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.add_log(
                        Level::Warning,
                        format!("The system keychain could not be read: {error}"),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The kind of operation running, if any.
    pub fn running_activity(&self) -> Option<Activity> {
        self.running.as_ref().map(|r| r.activity)
    }

    pub(super) fn start(
        &mut self,
        activity: Activity,
        operation: Operation,
        cx: &mut Context<Self>,
    ) {
        if self.worker.is_some() {
            return;
        }
        let Some(port) = self.selected_port().map(|p| p.path.clone()) else {
            self.running = None;
            self.end_power_with_debug(cx);
            self.add_log(Level::Error, "No Soul Injector is connected");
            cx.notify();
            return;
        };
        if activity == Activity::Inspect {
            self.device_error = None;
        }
        self.running = Some(Running {
            activity,
            step: "Starting".into(),
            progress: None,
            stopping: false,
            preparing: false,
        });
        self.worker = Some(Worker::start(
            self.connection(port),
            operation,
            self.events.clone(),
        ));
        cx.notify();
    }

    /// Compiles the profile's job off the UI thread, then starts the
    /// operation `make` builds from it.
    fn start_with_job(
        &mut self,
        activity: Activity,
        make: impl FnOnce(BuiltJob) -> Operation + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        self.running = Some(Running {
            activity,
            step: "Preparing the job".into(),
            progress: None,
            stopping: false,
            preparing: true,
        });
        let profile = self.state.profile().clone();
        cx.spawn_in(window, async move |this, cx| {
            let job = cx
                .background_executor()
                .spawn(async move { target::build_job(&profile) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let cancelled = this.running.as_ref().is_some_and(|r| r.stopping);
                this.running = None;
                match job {
                    Ok(_) if cancelled => {
                        this.end_power_with_debug(cx);
                        cx.notify();
                    }
                    Ok(job) => this.start(activity, make(job), cx),
                    Err(problem) => {
                        this.end_power_with_debug(cx);
                        this.report_failure(activity, problem, window, cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Reads device and job status.
    pub fn inspect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self
            .selected_port()
            .map(|p| p.path.clone())
            .filter(|path| self.needs_password(path))
        {
            self.recall_password(path, window, cx);
            return;
        }
        if !self.is_busy() && self.device_blocker().is_none() {
            self.start(Activity::Inspect, Operation::Inspect, cx);
        }
    }

    /// Starts or stops the debug server.
    pub fn toggle_debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .running
            .as_ref()
            .is_some_and(|r| r.activity == Activity::Debug)
        {
            self.stop(cx);
            return;
        }
        let options = self.state.profile().debug.clone();
        // A recording the user started is theirs: the session neither
        // restarts it nor stops it. Soul Injector samples come over the debug
        // connection itself; the simulated preview runs alongside.
        let mut feed = None;
        if options.measure_power && !self.is_busy() && !self.power.read(cx).is_capturing() {
            if self.power.read(cx).source() == Source::SoulInjector {
                let zero = self.power_zero();
                feed = self
                    .power
                    .update(cx, |power, cx| power.begin_device(zero, cx));
            } else {
                self.power.update(cx, |power, cx| power.start(cx));
            }
            self.power_with_debug = true;
        }
        let plan = move |program: Option<BuiltJob>| DebugPlan {
            program,
            port: options.port,
            connect_under_reset: options.connect_under_reset,
            resume_on_detach: options.resume_on_detach,
            keep_serving: options.keep_serving,
            power: feed,
        };
        match self.state.profile().debug.mode {
            DebugMode::ProgramThenDebug => self.start_with_job(
                Activity::Debug,
                move |job| Operation::Debug(plan(Some(job))),
                window,
                cx,
            ),
            DebugMode::Attach => {
                if !self.is_busy() {
                    self.start(Activity::Debug, Operation::Debug(plan(None)), cx);
                }
            }
        }
    }

    /// Pushes the job and programs the target once.
    pub fn program(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let auto = self.state.profile().auto_run;
        self.start_with_job(
            Activity::Program,
            move |job| Operation::Program { job, auto },
            window,
            cx,
        );
    }

    /// Pushes the job without running it.
    pub fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let auto = self.state.profile().auto_run;
        self.start_with_job(
            Activity::Push,
            move |job| Operation::PushJob { job, auto },
            window,
            cx,
        );
    }

    /// Collects the production log.
    pub fn pull_log(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_busy() {
            let output = self.state.settings.log_file();
            self.start(Activity::PullLog, Operation::PullLog { output }, cx);
        }
    }

    /// Asks the running operation to stop.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(running) = &mut self.running {
            running.stopping = true;
            running.step = "Stopping".into();
        }
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        cx.notify();
    }

    fn on_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Log(level, text) => self.add_log(level, text),
            Event::Step(step) => {
                if let Some(running) = &mut self.running
                    && !running.stopping
                {
                    running.step = step.into();
                }
            }
            Event::Progress(progress) => {
                if let Some(running) = &mut self.running {
                    running.progress = progress;
                }
            }
            Event::Device(snapshot) => {
                self.device = Some(snapshot);
                self.device_error = None;
            }
            Event::Gdb(state) => self.gdb_state = state,
            Event::Network(snapshot) => self.show_network(snapshot, window, cx),
            // Ask again; the failure itself explains why.
            Event::LoginRefused => {
                if let Some(path) = self.selected_port().map(|p| p.path.clone()) {
                    self.passwords.remove(&path);
                    if let Some(id) = ports::device_id(&path) {
                        cx.delete_credentials(&credential_url(id)).detach();
                    }
                }
            }
            Event::PowerZero { serial, zero } => {
                self.state.settings.power_zero.insert(serial, zero);
                self.save_soon(cx);
            }
            Event::Finished(outcome) => self.on_finished(outcome, window, cx),
        }
        cx.notify();
    }

    fn on_finished(&mut self, outcome: Outcome, window: &mut Window, cx: &mut Context<Self>) {
        let activity = self.running.take().map(|r| r.activity);
        self.worker = None;
        self.gdb_state = GdbState::Idle;
        if activity == Some(Activity::Debug) {
            self.end_power_with_debug(cx);
        }
        let Some(activity) = activity else {
            return;
        };
        match outcome {
            Outcome::Done(message) if message.is_empty() => {}
            Outcome::Done(message) => {
                self.add_log(Level::Success, message.clone());
                if !matches!(activity, Activity::Debug | Activity::Power) {
                    window.push_notification(Notification::success(message), cx);
                }
            }
            Outcome::Cancelled => self.add_log(Level::Info, "Stopped"),
            Outcome::Failed(message) => self.report_failure(activity, message, window, cx),
        }
    }

    fn report_failure(
        &mut self,
        activity: Activity,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_log(Level::Error, message.clone());
        if activity == Activity::Inspect {
            // Shown in the device panel; a popup would be noise on plug-in.
            self.device_error = Some(message.into());
        } else {
            window.push_notification(Notification::error(message), cx);
        }
    }

    /// Asks for what the power view cannot do on its own.
    fn on_power_request(&mut self, request: PowerRequest, cx: &mut Context<Self>) {
        let blocker = if self.is_busy() {
            Some("The Soul Injector is busy. Wait for it to finish, or stop what it is doing.")
        } else {
            self.device_blocker()
        };
        if let Some(blocker) = blocker {
            self.power.update(cx, |power, cx| power.fail(blocker, cx));
            return;
        }
        match request {
            PowerRequest::StartDevice => {
                let zero = self.power_zero();
                if let Some(feed) = self
                    .power
                    .update(cx, |power, cx| power.begin_device(zero, cx))
                {
                    self.start(Activity::Power, Operation::Power(feed), cx);
                }
            }
            PowerRequest::MeasureZero => {
                self.add_log(
                    Level::Info,
                    "Measuring the zero offset; nothing may be connected to VPP_OUTPUT (PROG pin 9)",
                );
                self.start(Activity::PowerZero, Operation::PowerZero, cx);
            }
        }
    }

    /// The zero correction measured for the connected Soul Injector.
    fn power_zero(&self) -> sidp_agent::power::Zero {
        self.device_snapshot()
            .and_then(|device| self.state.settings.power_zero.get(&device.info.serial))
            .copied()
            .unwrap_or_default()
    }

    /// Stops a power capture that was started with the debug session.
    fn end_power_with_debug(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.power_with_debug) {
            self.power.update(cx, |power, cx| power.stop(cx));
        }
    }

    /// Stops the running operation and saves, before the app exits.
    fn shut_down(&mut self, cx: &mut Context<Self>) {
        self.power.update(cx, |power, cx| power.stop(cx));
        if let Some(worker) = self.worker.take() {
            worker.shut_down(Duration::from_secs(3));
        }
        self.save_now();
    }

    // ---- Log -------------------------------------------------------------------

    /// Adds a line to the activity log.
    pub fn add_log(&mut self, level: Level, text: impl Into<SharedString>) {
        if level == Level::Error {
            self.show_log = true;
        }
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push_back(LogLine {
            time: time.into(),
            level,
            text: text.into(),
        });
        while self.log.len() > LOG_LIMIT {
            self.log.pop_front();
        }
        self.log_scroll.scroll_to_bottom();
    }

    /// Empties the activity log.
    pub fn clear_log(&mut self, cx: &mut Context<Self>) {
        self.log.clear();
        cx.notify();
    }

    // ---- Debugger ----------------------------------------------------------------

    /// What the snippets describe.
    pub fn snippet(&self, client: Client) -> String {
        let profile = self.state.profile();
        let gdb = self.gdb_path();
        client.snippet(&DebugTarget {
            name: &profile.name,
            gdb: gdb.as_deref(),
            elf: profile.elf.as_deref(),
            port: profile.debug.port,
        })
    }

    /// Copies the shown snippet.
    pub fn copy_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.snippet(self.client);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(
            Notification::info(format!("{} settings copied", self.client.label())),
            cx,
        );
    }

    /// Opens GDB connected to the server in a terminal window.
    pub fn open_gdb_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.state.profile();
        let gdb = self
            .gdb_path()
            .unwrap_or_else(|| PathBuf::from(snippets::FALLBACK_GDB));
        let args = snippets::gdb_args(&DebugTarget {
            name: &profile.name,
            gdb: Some(&gdb),
            elf: profile.elf.as_deref(),
            port: profile.debug.port,
        });
        match platform::open_in_terminal(profile.project_dir(), &args) {
            Ok(()) => self.add_log(Level::Info, "Opened GDB in a terminal"),
            Err(error) => {
                let message = format!("Cannot open a terminal: {error}");
                self.add_log(Level::Error, message.clone());
                window.push_notification(Notification::error(message), cx);
            }
        }
        cx.notify();
    }

    // ---- Appearance ----------------------------------------------------------------

    fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.state.settings.dark {
            Some(true) => Theme::change(ThemeMode::Dark, Some(window), cx),
            Some(false) => Theme::change(ThemeMode::Light, Some(window), cx),
            None => Theme::sync_system_appearance(Some(window), cx),
        }
        // Changing the theme restores its sizes. A slightly denser scale
        // than the library default suits a form-heavy tool; rem-based
        // sizes follow it.
        let theme = Theme::global_mut(cx);
        theme.font_size = gpui_kit::px(14.);
        theme.mono_font_size = gpui_kit::px(12.5);
        window.refresh();
    }

    /// Switches between light and dark.
    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = !Theme::global(cx).is_dark();
        self.state.settings.dark = Some(dark);
        self.apply_theme(window, cx);
        self.save_soon(cx);
        cx.notify();
    }

    /// Changes a debug option of the shown profile.
    pub fn set_debug_option(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut crate::profile::DebugOptions),
    ) {
        edit(&mut self.state.profile_mut().debug);
        self.save_soon(cx);
        cx.notify();
    }

    /// Changes whether the device programs on its own.
    pub fn set_auto_run(&mut self, auto: bool, cx: &mut Context<Self>) {
        self.state.profile_mut().auto_run = auto;
        self.save_soon(cx);
        cx.notify();
    }
}
