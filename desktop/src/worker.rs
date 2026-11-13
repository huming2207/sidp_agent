//! Device operations, run one at a time on a background thread.
//!
//! The UI starts an [`Operation`] with [`Worker::start`] and receives
//! [`Event`]s while it runs. Every operation opens its own connection, over
//! USB or through the app's listener over WiFi, and closes it when done, like
//! one command-line invocation. Nothing is
//! retried on the same connection: a failed request ends the operation.
use crate::{
    describe,
    power::{CaptureEvent, DeviceFeed},
    target::BuiltJob,
};
use futures::channel::mpsc::UnboundedSender;
use sidp_agent::{
    manage::{
        self,
        proto::{DeviceInfo, JobStatus, NetConfig, NetStatus, RunResult, Trigger},
    },
    ntp,
    power::Zero,
    session::{self, ResetKind, Session},
    transport::{Transport, UsbCdc, WebSocket},
};
use std::{
    fs::OpenOptions,
    io::{self, BufWriter, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

mod bridge;
mod power;

/// An open connection to a Soul Injector, over USB or WiFi.
pub type DeviceSession = Session<Box<dyn Transport + Send>>;

/// How often a programming run is polled.
const RUN_POLL: Duration = Duration::from_millis(500);
/// How long to wait for each NTP server address.
const NTP_TIMEOUT: Duration = Duration::from_secs(2);
/// USB line coding; the CDC link ignores it, the CLI uses the same value.
const BAUD: u32 = 115_200;

/// How to reach the device.
#[derive(Clone, Debug)]
pub struct Connection {
    /// Serial port path, or `ws://127.0.0.1:PORT/DEVICE_ID` for a device
    /// connected to the app's listener.
    pub port: String,
    /// Device password, for a device on WiFi.
    pub password: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Wait after opening before the first request.
    pub settle: Duration,
    /// NTP server for the device clock.
    pub ntp_server: String,
    /// Toggle DTR on open and close. Real Soul Injectors require it; only
    /// tests over a pseudo-terminal turn it off.
    pub dtr: bool,
}

impl Connection {
    fn open(&self, r: &Reporter) -> Result<DeviceSession, Stop> {
        let Some((host, id)) = self
            .port
            .strip_prefix("ws://")
            .and_then(|rest| rest.split_once('/'))
        else {
            let usb = UsbCdc::open(&self.port, BAUD, self.settle, self.dtr)
                .map_err(|e| Stop::Failed(describe::open_error(&self.port, &e)))?;
            return Ok(Session::new(Box::new(usb), self.timeout));
        };
        let ws = WebSocket::connect(host, id).map_err(|e| Stop::Failed(e.to_string()))?;
        let mut session: DeviceSession = Session::new(Box::new(ws), self.timeout);
        // Every request crosses WiFi, so cache flash reads within a stop.
        session.set_flash_cache(true);
        if let Err(error) = session.login(self.password.as_deref().unwrap_or_default()) {
            if describe::status_code(&error) == Some(manage::STATUS_UNAUTHORIZED) {
                r.send(Event::LoginRefused);
            }
            return Err(error.into());
        }
        Ok(session)
    }
}

/// What a debug session does.
#[derive(Clone)]
pub struct DebugPlan {
    /// Program this job before debugging; attach only when `None`.
    pub program: Option<BuiltJob>,
    /// Local port GDB connects to.
    pub port: u16,
    /// Hold the target in reset while connecting.
    pub connect_under_reset: bool,
    /// Let the target run when GDB detaches cleanly.
    pub resume_on_detach: bool,
    /// Keep listening for the next GDB session after one ends.
    pub keep_serving: bool,
    /// Measure power while connected to the device.
    pub power: Option<DeviceFeed>,
}

/// A device operation.
#[derive(Clone)]
pub enum Operation {
    /// Read device and job status.
    Inspect,
    /// Make the job the device's job without running it.
    PushJob {
        /// The job.
        job: BuiltJob,
        /// Also run whenever a target is plugged in.
        auto: bool,
    },
    /// Push the job and program the target once.
    Program {
        /// The job.
        job: BuiltJob,
        /// Also run whenever a target is plugged in.
        auto: bool,
    },
    /// Append new production log entries to a file and mark them collected.
    PullLog {
        /// JSON Lines file.
        output: PathBuf,
    },
    /// Serve GDB.
    Debug(DebugPlan),
    /// Record power into a view.
    Power(DeviceFeed),
    /// Measure the power profiler's zero offset; nothing may be on VPP_OUTPUT.
    PowerZero,
    /// Read the network settings and connection state.
    ReadNetwork,
    /// Store network settings; the device uses them after a restart.
    SetNetwork(NetConfig),
}

/// Importance of a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Progress information.
    Info,
    /// Something finished well.
    Success,
    /// Something the user may want to act on.
    Warning,
    /// Something failed.
    Error,
}

/// Device and job status.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceSnapshot {
    /// Serial port it was read from.
    pub port: String,
    /// Serial number, firmware and storage.
    pub info: DeviceInfo,
    /// Job, trigger and last result.
    pub job: JobStatus,
}

/// Network settings and connection state.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkSnapshot {
    /// Stored settings; the WiFi password is never returned.
    pub config: NetConfig,
    /// WiFi and server connection.
    pub status: NetStatus,
}

/// State of the GDB server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GdbState {
    /// Not serving.
    Idle,
    /// Waiting for GDB on this address.
    Listening(SocketAddr),
    /// GDB is connected from this address.
    Connected(SocketAddr),
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It did what was asked; the message may be empty.
    Done(String),
    /// The user stopped it.
    Cancelled,
    /// It failed; the message explains why.
    Failed(String),
}

/// Something that happened during an operation.
#[derive(Clone, Debug)]
pub enum Event {
    /// A line for the activity log.
    Log(Level, String),
    /// What the operation is doing now.
    Step(String),
    /// Completion of the current step, when known.
    Progress(Option<f32>),
    /// Fresh device status.
    Device(DeviceSnapshot),
    /// The GDB server changed state.
    Gdb(GdbState),
    /// A zero offset was measured for the device with this serial number.
    PowerZero {
        /// Device serial number.
        serial: String,
        /// The correction.
        zero: Zero,
    },
    /// Fresh network settings and state.
    Network(NetworkSnapshot),
    /// The device refused the password.
    LoginRefused,
    /// The operation ended; no events follow.
    Finished(Outcome),
}

/// Why an operation stopped early.
#[derive(Debug)]
enum Stop {
    Cancelled,
    Failed(String),
}

impl From<session::Error> for Stop {
    fn from(error: session::Error) -> Self {
        Stop::Failed(describe::error(&error))
    }
}

type Step<T> = Result<T, Stop>;

/// A running operation.
pub struct Worker {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts `operation` on its own thread; events go to `events`.
    pub fn start(
        connection: Connection,
        operation: Operation,
        events: UnboundedSender<Event>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let reporter = Reporter {
            events,
            cancel: cancel.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("device".into())
            .spawn(move || {
                let outcome = match run(&connection, operation, &reporter) {
                    Ok(message) => Outcome::Done(message),
                    Err(Stop::Cancelled) => Outcome::Cancelled,
                    Err(Stop::Failed(message)) => Outcome::Failed(message),
                };
                reporter.send(Event::Gdb(GdbState::Idle));
                reporter.send(Event::Finished(outcome));
            })
            .expect("spawn device thread");
        Self {
            cancel,
            thread: Some(thread),
        }
    }

    /// Asks the operation to stop as soon as it safely can.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Whether the operation has ended.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Cancels and waits up to `limit` for the operation to end, so that
    /// the device is released before the app exits.
    pub fn shut_down(mut self, limit: Duration) {
        self.cancel();
        let deadline = Instant::now() + limit;
        while !self.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.is_finished()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Sends events and answers whether the user asked to stop.
struct Reporter {
    events: UnboundedSender<Event>,
    cancel: Arc<AtomicBool>,
}

impl Reporter {
    fn send(&self, event: Event) {
        // The UI may already be gone while the app quits.
        let _ = self.events.unbounded_send(event);
    }
    fn log(&self, level: Level, text: impl Into<String>) {
        self.send(Event::Log(level, text.into()));
    }
    fn info(&self, text: impl Into<String>) {
        self.log(Level::Info, text);
    }
    fn warn(&self, text: impl Into<String>) {
        self.log(Level::Warning, text);
    }
    fn step(&self, text: impl Into<String>) {
        self.send(Event::Step(text.into()));
        self.send(Event::Progress(None));
    }
    fn progress(&self, fraction: f32) {
        self.send(Event::Progress(Some(fraction.clamp(0.0, 1.0))));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    fn check(&self) -> Step<()> {
        if self.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }
    /// Sleeps for `duration`, waking early when cancelled.
    fn pause(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20).min(duration));
        }
    }
}

fn run(connection: &Connection, operation: Operation, r: &Reporter) -> Step<String> {
    match operation {
        Operation::Inspect => {
            let mut session = open(connection, r)?;
            snapshot(&mut session, connection, r)?;
            Ok(String::new())
        }
        Operation::PushJob { job, auto } => {
            let mut session = open(connection, r)?;
            set_device_time(&mut session, connection, r);
            push_job(&mut session, &job, trigger(auto), r)?;
            snapshot(&mut session, connection, r)?;
            Ok(format!(
                "Job '{}' is on the Soul Injector{}",
                job.name,
                if auto {
                    " and runs whenever a target is plugged in"
                } else {
                    ""
                }
            ))
        }
        Operation::Program { job, auto } => {
            let mut session = open(connection, r)?;
            set_device_time(&mut session, connection, r);
            push_job(&mut session, &job, trigger(auto), r)?;
            let result = run_once(&mut session, r)?;
            snapshot(&mut session, connection, r)?;
            finish_run(&result)
        }
        Operation::PullLog { output } => pull_log(connection, &output, r),
        Operation::Debug(plan) => {
            let result = debug(connection, &plan, r);
            if let Some(feed) = &plan.power {
                end_feed(feed, &result);
            }
            result
        }
        Operation::Power(feed) => {
            let result =
                open(connection, r).and_then(|mut session| power::record(&mut session, &feed, r));
            end_feed(&feed, &result);
            result
        }
        Operation::PowerZero => {
            let mut session = open(connection, r)?;
            let serial = session.device_info()?.serial;
            let zero = power::measure_zero(&mut session, r)?;
            r.send(Event::PowerZero { serial, zero });
            let [low, mid, high] = zero.amps_per_volt.map(|a| a * 3.3e6);
            Ok(format!(
                "Zero offset at 3.3 V: LOW {low:.2} µA, MID {mid:.2} µA, HIGH {high:.1} µA"
            ))
        }
        Operation::ReadNetwork => {
            let mut session = open(connection, r)?;
            read_network(&mut session, r)?;
            Ok(String::new())
        }
        Operation::SetNetwork(config) => {
            let mut session = open(connection, r)?;
            r.step("Storing network settings");
            session.net_config_set(&config)?;
            read_network(&mut session, r)?;
            Ok("Network settings stored. Restart the Soul Injector to use them.".into())
        }
    }
}

fn read_network(session: &mut DeviceSession, r: &Reporter) -> Step<()> {
    let config = session.net_config_get()?;
    let status = session.net_status()?;
    r.send(Event::Network(NetworkSnapshot { config, status }));
    Ok(())
}

/// Tells the view its recording ended; a second end is ignored there.
fn end_feed<T>(feed: &DeviceFeed, result: &Step<T>) {
    let ended = match result {
        Err(Stop::Failed(message)) => Err(message.clone()),
        _ => Ok(()),
    };
    let _ = feed.events.send(CaptureEvent::Ended(ended));
}

/// Starts power measurement on a debug connection. A failure only costs
/// the measurement, not the debug session.
fn start_debug_power(
    session: &mut DeviceSession,
    plan: &DebugPlan,
    timeline: Option<&Arc<std::sync::Mutex<power::Timeline>>>,
    r: &Reporter,
) {
    let (Some(feed), Some(timeline)) = (&plan.power, timeline) else {
        return;
    };
    if feed.stopped() {
        return;
    }
    if let Err(Stop::Failed(message)) = power::start(session, feed, timeline, r) {
        r.warn(message.clone());
        let _ = feed.events.send(CaptureEvent::Ended(Err(message)));
    }
}

fn trigger(auto: bool) -> Trigger {
    if auto {
        Trigger::AutoOnDetect
    } else {
        Trigger::Manual
    }
}

fn open(connection: &Connection, r: &Reporter) -> Step<DeviceSession> {
    r.check()?;
    r.step(format!("Connecting to {}", connection.port));
    connection.open(r)
}

fn snapshot(session: &mut DeviceSession, connection: &Connection, r: &Reporter) -> Step<()> {
    let info = session.device_info()?;
    let job = session.job_get()?;
    r.send(Event::Device(DeviceSnapshot {
        port: connection.port.clone(),
        info,
        job,
    }));
    Ok(())
}

/// Sets the device clock from NTP, as the CLI does, so log entries get UTC
/// times. Failing only costs the times, so it is a warning.
fn set_device_time(session: &mut DeviceSession, connection: &Connection, r: &Reporter) {
    let server = &connection.ntp_server;
    if server.trim().is_empty() {
        return;
    }
    match ntp::query(server, NTP_TIMEOUT) {
        Ok(time) => {
            if let Err(error) = session.set_time(time.now_ms()) {
                r.warn(format!(
                    "Could not set the Soul Injector's clock: {}",
                    describe::error(&error)
                ));
            }
        }
        Err(error) => r.warn(format!(
            "No time from {server} ({error}); log entries get no UTC time"
        )),
    }
}

/// Uploads one file unless the device already has it, reporting progress.
/// An interrupted upload resumes where the device left off. `label` names
/// the file in messages.
fn upload(
    session: &mut DeviceSession,
    name: &str,
    label: &str,
    data: &[u8],
    r: &Reporter,
) -> Step<()> {
    let size = u32::try_from(data.len()).map_err(|_| {
        Stop::Failed(format!(
            "{} is larger than 4 GiB",
            describe::capitalize(label)
        ))
    })?;
    let begin = session.asset_begin(name, size, &manage::sha256(data))?;
    if begin.present {
        r.info(format!(
            "{} is already on the Soul Injector",
            describe::capitalize(label)
        ));
        return Ok(());
    }
    r.step(format!("Uploading {label}"));
    let mut offset = begin.offset as usize;
    if offset > 0 {
        r.info(format!("Resuming {label} at {} KiB", offset / 1024));
    }
    while offset < data.len() {
        r.check()?;
        let end = data.len().min(offset + manage::MAX_CHUNK);
        session.asset_write(offset as u32, &data[offset..end])?;
        offset = end;
        r.progress(offset as f32 / data.len() as f32);
    }
    session.asset_commit()?;
    r.info(format!("Uploaded {label} ({})", size_text(data.len())));
    Ok(())
}

fn size_text(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

/// Uploads what the device lacks and activates the job, like
/// `sidp_agent::manage::push_job` but with progress and cancellation.
fn push_job(
    session: &mut DeviceSession,
    job: &BuiltJob,
    trigger: Trigger,
    r: &Reporter,
) -> Step<()> {
    for warning in &job.warnings {
        r.warn(warning.clone());
    }
    for (name, data) in &job.images {
        upload(session, name, name, data, r)?;
    }
    let sha256 = manage::sha256(&job.bytes);
    let status = session.job_get()?;
    if !status.has_job || status.sha256 != sha256 {
        upload(session, manage::STAGED_JOB_NAME, "the job", &job.bytes, r)?;
    }
    r.check()?;
    session.job_set(&sha256, trigger)?;
    r.info(format!("Job '{}' is active", job.name));
    Ok(())
}

/// Runs the active job once and waits for its result. Cancelling asks the
/// device to stop the run and still waits for it to end.
fn run_once(session: &mut DeviceSession, r: &Reporter) -> Step<RunResult> {
    r.step("Programming the target");
    let run_id = session.job_run_once()?;
    let started = Instant::now();
    let mut cancel_sent = false;
    loop {
        if r.cancelled() && !cancel_sent {
            session.job_cancel(run_id)?;
            cancel_sent = true;
            r.info("Stopping the run…");
        }
        let status = session.job_get()?;
        // The result appears before the device has written its log entry.
        if status.current_run_id == 0
            && let Some(result) = status.last_result
            && result.run_id == run_id
        {
            return Ok(result);
        }
        r.send(Event::Step(format!(
            "Programming the target ({} s)",
            started.elapsed().as_secs()
        )));
        std::thread::sleep(RUN_POLL);
    }
}

fn finish_run(result: &RunResult) -> Step<String> {
    let text = describe::run_result(result);
    if describe::passed(result) {
        Ok(text)
    } else if result.outcome == manage::proto::Outcome::Cancelled as i32 {
        Err(Stop::Cancelled)
    } else {
        Err(Stop::Failed(text))
    }
}

fn pull_log(connection: &Connection, output: &PathBuf, r: &Reporter) -> Step<String> {
    let mut session = open(connection, r)?;
    set_device_time(&mut session, connection, r);
    r.step("Reading the production log");
    let info = session.device_info()?;
    let entries = manage::read_log(&mut session, info.log_acked_id)?;
    let Some(last) = entries.last() else {
        snapshot(&mut session, connection, r)?;
        return Ok("No new log entries".into());
    };

    // The entries must be on disk before the device may overwrite them.
    let write = || -> io::Result<()> {
        if let Some(dir) = output.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(output)?;
        let mut writer = BufWriter::new(file);
        for entry in &entries {
            writeln!(writer, "{}", manage::log_entry_json(&info.serial, entry))?;
        }
        writer.into_inner().map_err(|e| e.into_error())?.sync_all()
    };
    write().map_err(|e| Stop::Failed(format!("Cannot write {}: {e}", output.display())))?;
    session.log_ack(last.id)?;
    snapshot(&mut session, connection, r)?;
    Ok(format!(
        "Saved {} log entr{} to {}",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" },
        output.display()
    ))
}

/// Serves GDB until the user stops it, or until one GDB session ends when
/// `keep_serving` is off.
///
/// The first attach happens before GDB connects, so wiring problems show
/// up at once. Later sessions attach only when GDB connects, so the target
/// runs undisturbed in between when it was resumed.
fn debug(connection: &Connection, plan: &DebugPlan, r: &Reporter) -> Step<String> {
    let listener = bind(plan.port)?;
    let address = listener
        .local_addr()
        .map_err(|e| Stop::Failed(e.to_string()))?;

    let mut session = open(connection, r)?;
    if let Some(job) = &plan.program {
        set_device_time(&mut session, connection, r);
        // A job that runs on target detection would program again under GDB.
        push_job(&mut session, job, Trigger::Manual, r)?;
        let result = run_once(&mut session, r)?;
        let text = finish_run(&result)?;
        r.log(Level::Success, text);
    }
    let mut session = attach(session, plan, r)?;
    let timeline = plan.power.as_ref().map(power::Timeline::new);
    start_debug_power(&mut session, plan, timeline.as_ref(), r);
    let mut session = Some(session);

    loop {
        r.send(Event::Gdb(GdbState::Listening(address)));
        r.step(format!("Waiting for GDB on {address}"));
        r.log(Level::Success, format!("Debug server ready on {address}"));
        let Some((client, peer)) = wait_for_gdb(&listener, session.as_mut(), r)? else {
            if let Some(mut session) = session {
                // Release the target the same way a GDB detach would.
                session.detach(plan.resume_on_detach)?;
            }
            return Ok("Debug server stopped".into());
        };
        let session = match session.take() {
            Some(session) => session,
            None => match open(connection, r).and_then(|s| attach(s, plan, r)) {
                Ok(mut session) => {
                    start_debug_power(&mut session, plan, timeline.as_ref(), r);
                    session
                }
                Err(stop) => {
                    // GDB sees the connection close.
                    let _ = client.shutdown(std::net::Shutdown::Both);
                    return Err(stop);
                }
            },
        };
        r.send(Event::Gdb(GdbState::Connected(peer)));
        r.step(format!("GDB connected from {peer}"));
        r.info("GDB connected");
        match bridge::serve(client, session, plan.resume_on_detach, r) {
            bridge::End::Detached { resumed } => r.info(if resumed {
                "GDB detached; the target is running"
            } else {
                "GDB detached; the target stays halted"
            }),
            bridge::End::GdbClosed => {
                r.info("GDB disconnected without detaching; the target stays halted")
            }
            bridge::End::Stopped => {
                if plan.resume_on_detach {
                    resume_after_stop(connection, r);
                }
                return Ok("Debug server stopped".into());
            }
            bridge::End::Failed(message) => return Err(Stop::Failed(message)),
        }
        r.send(Event::Gdb(GdbState::Idle));
        if !plan.keep_serving || r.cancelled() {
            return Ok("Debug session ended".into());
        }
    }
}

fn bind(port: u16) -> Step<TcpListener> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
        Stop::Failed(match e.kind() {
            io::ErrorKind::AddrInUse => format!(
                "Port {port} is already in use, probably by another debug server. Stop it or choose another port."
            ),
            _ => format!("Cannot listen on port {port}: {e}"),
        })
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| Stop::Failed(e.to_string()))?;
    Ok(listener)
}

/// Attaches and halts the target. After programming, the target is reset
/// so debugging starts at its first instruction.
fn attach(mut session: DeviceSession, plan: &DebugPlan, r: &Reporter) -> Step<DeviceSession> {
    r.check()?;
    if plan.connect_under_reset {
        r.step("Attaching to the target under reset");
        session.attach_under_reset()?;
    } else {
        r.step("Attaching to the target");
        session.attach()?;
        if plan.program.is_some() {
            session.reset_halt(ResetKind::System)?;
        }
    }
    let info = session
        .info()
        .map(describe::target)
        .unwrap_or_else(|| "Cortex-M".into());
    let state = if plan.connect_under_reset || plan.program.is_some() {
        "halted at reset"
    } else {
        "halted"
    };
    r.info(format!("Attached: {info}; {state}"));
    Ok(session)
}

/// Lets the target run after the server was stopped with GDB connected, as
/// stopping before GDB connects does. Stopping closes the USB link, after
/// which the Soul Injector keeps the target halted, so this attaches again
/// and detaches with resume. The user already asked to stop, so this skips
/// the cancellation checks; a failure only costs the resume.
fn resume_after_stop(connection: &Connection, r: &Reporter) {
    r.step("Letting the target run");
    let resumed = connection.open(r).and_then(|mut session| {
        session.attach()?;
        session.detach(true)?;
        Ok(())
    });
    match resumed {
        Ok(()) => r.info("The target is running"),
        Err(Stop::Failed(message)) => r.warn(format!("The target stays halted: {message}")),
        Err(Stop::Cancelled) => {}
    }
}

/// Waits for GDB to connect, keeping the USB link serviced. `None` when
/// the user stopped the server first.
fn wait_for_gdb(
    listener: &TcpListener,
    mut session: Option<&mut DeviceSession>,
    r: &Reporter,
) -> Step<Option<(TcpStream, SocketAddr)>> {
    loop {
        if r.cancelled() {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                // macOS hands out sockets that inherit non-blocking mode.
                stream
                    .set_nonblocking(false)
                    .map_err(|e| Stop::Failed(e.to_string()))?;
                return Ok(Some((stream, peer)));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => match session.as_deref_mut() {
                Some(session) => session.poll(Duration::from_millis(50))?,
                None => r.pause(Duration::from_millis(50)),
            },
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Stop::Failed(format!("GDB connection failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_text(512), "512 bytes");
        assert_eq!(size_text(2048), "2.0 KiB");
        assert_eq!(size_text(3 * 1024 * 1024 / 2), "1.5 MiB");
    }

    #[test]
    fn busy_ports_are_explained() {
        let first = bind(0).unwrap();
        let port = first.local_addr().unwrap().port();
        match bind(port) {
            Err(Stop::Failed(message)) => assert!(message.contains("already in use"), "{message}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
