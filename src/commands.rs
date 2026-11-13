//! Management and job-compile commands of the command-line tool.
use clap::Args;
use prost::Message;
use sidp_agent::compile::{self, ImageHashes, Sources};
use sidp_agent::manage::{
    self, hex, proto::Outcome, proto::RunResult, proto::Stage, proto::Trigger,
};
use sidp_agent::{
    listener::is_device_id,
    ntp,
    session::Session,
    transport::{Transport, UsbCdc, WEBSOCKET_PORT, WebSocket},
};
use std::{
    error::Error,
    fs::OpenOptions,
    io::{BufWriter, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

type CommandResult = Result<(), Box<dyn Error>>;
/// An open connection to a device, over USB or WiFi.
pub type DeviceSession = Session<Box<dyn Transport + Send>>;

/// How often `job run --wait` polls the device.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long to wait for each NTP server address.
const NTP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for a Soul Injector to connect over WiFi.
const DEVICE_WAIT: Duration = Duration::from_secs(30);

/// Connection options shared by every device command.
#[derive(Args)]
pub struct Connection {
    /// USB CDC path, e.g. /dev/ttyACM0 or COM3. Over WiFi:
    /// ws://127.0.0.1/DEVICE_ID uses a device held by `sidp-agent serve` or
    /// the desktop app; ws://0.0.0.0 waits for a device to connect to this
    /// command. Port 80 unless given.
    #[arg(long)]
    port: String,
    /// CDC line coding; does not set SWD speed.
    #[arg(long, default_value_t = 115200)]
    baud: u32,
    /// Per-operation timeout in milliseconds (no automatic retries).
    #[arg(long,default_value_t=5000,value_parser=clap::value_parser!(u64).range(1..=60000))]
    timeout_ms: u64,
    /// Wait after DTR assertion for the firmware to accept the connection.
    #[arg(long,default_value_t=250,value_parser=clap::value_parser!(u64).range(0..=60000))]
    settle_ms: u64,
    /// Skip DTR modem control for PTY testing only; real hardware requires DTR.
    #[arg(long)]
    no_dtr: bool,
    /// NTP server that gives the device its UTC clock.
    #[arg(long, default_value = ntp::DEFAULT_SERVER)]
    ntp_server: String,
    /// Device password, for ws:// ports; otherwise SIDP_PASSWORD, or asked for.
    #[arg(long)]
    password: Option<String>,
}

impl Connection {
    /// Opens a fresh SIDP connection, and logs in over the network.
    pub fn open(&self) -> Result<DeviceSession, Box<dyn Error>> {
        let timeout = Duration::from_millis(self.timeout_ms);
        let Some(address) = self.port.strip_prefix("ws://") else {
            let settle = Duration::from_millis(self.settle_ms);
            let usb = UsbCdc::open(&self.port, self.baud, settle, !self.no_dtr)?;
            return Ok(Session::new(Box::new(usb), timeout));
        };
        let (host, id) = address.split_once('/').unwrap_or((address, ""));
        let host = with_default_port(host);
        // Accept a MAC address written with separators or in lower case.
        let id: String = id
            .chars()
            .filter(|c| !matches!(c, ':' | '-' | '/'))
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if !id.is_empty() && !is_device_id(&id) {
            return Err(format!("--port {}: the device ID is 12 hex digits", self.port).into());
        }
        let password = match &self.password {
            Some(password) => password.clone(),
            None => match std::env::var("SIDP_PASSWORD") {
                Ok(password) => password,
                Err(_) => read_line("Device password (shown as typed): ")?,
            },
        };
        let ws = if id.is_empty() {
            let address: SocketAddr = host
                .parse()
                .map_err(|e| format!("--port {}: {e}; expected ws://ADDRESS[:PORT]", self.port))?;
            eprintln!("Waiting for a Soul Injector to connect to {address}");
            let ws = WebSocket::accept(address, DEVICE_WAIT)?;
            eprintln!("Connected from {}", ws.peer());
            ws
        } else {
            WebSocket::connect(&host, &id)?
        };
        let mut session: DeviceSession = Session::new(Box::new(ws), timeout);
        // Every request crosses WiFi, so cache flash reads within a stop.
        session.set_flash_cache(true);
        session.login(&password)?;
        Ok(session)
    }

    /// Whether this is a USB connection; network settings can only be changed over USB.
    pub fn is_usb(&self) -> bool {
        !self.port.starts_with("ws://")
    }
}

/// `HOST[:PORT]` with port 80, the `ws://` default, when none is given.
fn with_default_port(host: &str) -> String {
    let has_port = host
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    if has_port {
        host.to_owned()
    } else {
        format!("{host}:{WEBSOCKET_PORT}")
    }
}

/// Inputs of one programming job.
#[derive(Args)]
pub struct JobSource {
    /// probe-rs style target description.
    #[arg(long)]
    target: PathBuf,
    /// Variant to select when target.yaml lists several.
    #[arg(long)]
    variant: Option<String>,
    /// Procedure run before target detection (pre_prog.yaml).
    #[arg(long)]
    pre: Option<PathBuf>,
    /// Procedure run after self tests (post_prog.yaml).
    #[arg(long)]
    post: Option<PathBuf>,
    /// Job name shown by the device; defaults to the variant name.
    #[arg(long)]
    name: Option<String>,
    /// Image the job programs, as DEVICE_NAME=FILE (e.g. firmware.bin=build/app.bin).
    #[arg(long = "image", value_name = "NAME=FILE")]
    images: Vec<String>,
}

/// A compiled job with the images it pins.
struct BuiltJob {
    bytes: Vec<u8>,
    name: String,
    images: Vec<(String, Vec<u8>)>,
}

/// Reads one line from standard input after printing `prompt`.
pub fn read_line(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn read_optional(path: Option<&Path>) -> std::io::Result<Option<String>> {
    path.map(std::fs::read_to_string).transpose()
}

fn read_image(spec: &str) -> Result<(String, Vec<u8>), Box<dyn Error>> {
    let Some((name, file)) = spec.split_once('=') else {
        return Err(format!("--image '{spec}' must be NAME=FILE").into());
    };
    if !manage::is_valid_asset_name(name) {
        return Err(format!(
            "--image name '{name}' is not a valid device file name ({})",
            manage::ASSET_NAME_RULE
        )
        .into());
    }
    let data = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
    Ok((name.to_owned(), data))
}

fn build_job(source: &JobSource) -> Result<BuiltJob, Box<dyn Error>> {
    let mut images = Vec::new();
    let mut hashes = ImageHashes::new();
    for spec in &source.images {
        let (name, data) = read_image(spec)?;
        hashes.insert(name.clone(), manage::sha256(&data));
        images.push((name, data));
    }

    let target_text = std::fs::read_to_string(&source.target)?;
    let pre_text = read_optional(source.pre.as_deref())?;
    let post_text = read_optional(source.post.as_deref())?;
    let sources = Sources {
        name: source.name.as_deref(),
        target: &target_text,
        variant: source.variant.as_deref(),
        pre_program: pre_text.as_deref(),
        post_program: post_text.as_deref(),
        images: &hashes,
    };
    let compiled = compile::compile(&sources)?;
    for warning in &compiled.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(BuiltJob {
        bytes: compiled.job.encode_to_vec(),
        name: compiled.job.name,
        images,
    })
}

/// `compile`: writes the job file without talking to a device.
pub fn compile_job(source: &JobSource, output: &Path) -> CommandResult {
    let job = build_job(source)?;
    std::fs::write(output, &job.bytes)?;
    println!(
        "Wrote {} ({} bytes, sha256 {})",
        output.display(),
        job.bytes.len(),
        hex(&manage::sha256(&job.bytes))
    );
    Ok(())
}

/// `device info`.
pub fn device_info(connection: &Connection) -> CommandResult {
    let info = connection.open()?.device_info()?;
    println!("Serial:   {}", info.serial);
    println!("Firmware: {}", info.firmware_version);
    println!(
        "Storage:  {} KiB free of {} KiB",
        info.storage_free_kb, info.storage_total_kb
    );
    if info.log_full {
        println!(
            "Log:      runs refused; `sidp-agent log pull`, or restart the device if it still shows LOG ERROR"
        );
    } else if info.log_newest_id > info.log_acked_id {
        println!("Log:      has entries not yet pulled");
    } else {
        println!("Log:      all entries pulled");
    }
    Ok(())
}

/// Sets the device clock to UTC from the NTP server so log entries get UTC
/// times. This computer's clock is not used: it may be wrong.
///
/// Without NTP, older firmware without SET_TIME, or a refused time, entries
/// only lose their UTC times; a later command can still set the clock.
fn set_device_time(session: &mut DeviceSession, connection: &Connection) {
    let server = &connection.ntp_server;
    let time = match ntp::query(server, NTP_TIMEOUT) {
        Ok(time) => time,
        Err(error) => {
            eprintln!("warning: cannot get the time from {server}: {error}; device clock not set");
            return;
        }
    };
    if let Err(error) = session.set_time(time.now_ms()) {
        eprintln!("warning: cannot set the device clock: {error}");
    }
}

/// `log pull`: appends new production log entries to `output` as JSON
/// Lines, then marks them collected unless `no_ack`.
pub fn log_pull(connection: &Connection, output: &Path, no_ack: bool) -> CommandResult {
    let mut session = connection.open()?;
    set_device_time(&mut session, connection);
    let info = session.device_info()?;
    let entries = manage::read_log(&mut session, info.log_acked_id)?;
    let Some(last) = entries.last() else {
        println!("No new log entries");
        return Ok(());
    };

    // The entries must be on disk before the device may overwrite them.
    let file = OpenOptions::new().create(true).append(true).open(output)?;
    let mut writer = BufWriter::new(file);
    for entry in &entries {
        writeln!(writer, "{}", manage::log_entry_json(&info.serial, entry))?;
    }
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;

    if no_ack {
        println!(
            "Wrote {} entries to {} (not acknowledged)",
            entries.len(),
            output.display()
        );
        return Ok(());
    }
    session.log_ack(last.id)?;
    println!("Wrote {} entries to {}", entries.len(), output.display());
    Ok(())
}

/// `asset push`.
pub fn asset_push(connection: &Connection, file: &Path, name: Option<&str>) -> CommandResult {
    let file_name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let name = name.unwrap_or(file_name);
    if !manage::is_valid_asset_name(name) {
        return Err(format!(
            "'{name}' is not a valid device file name ({}); use --name",
            manage::ASSET_NAME_RULE
        )
        .into());
    }
    let data = std::fs::read(file)?;
    let mut session = connection.open()?;
    if manage::push_asset(&mut session, name, &data)? {
        println!("Uploaded {name} ({} bytes)", data.len());
    } else {
        println!("{name} is already on the device");
    }
    Ok(())
}

/// `job push`: compiles, uploads missing files and activates the job.
pub fn job_push(connection: &Connection, source: &JobSource, auto: bool) -> CommandResult {
    let job = build_job(source)?;
    let trigger = if auto {
        Trigger::AutoOnDetect
    } else {
        Trigger::Manual
    };
    let mut session = connection.open()?;
    set_device_time(&mut session, connection);
    manage::push_job(&mut session, &job.bytes, &job.images, trigger)?;
    println!(
        "Job '{}' active (sha256 {}), trigger {}",
        job.name,
        hex(&manage::sha256(&job.bytes)),
        trigger_name(trigger)
    );
    Ok(())
}

/// `job run`.
pub fn job_run(connection: &Connection, wait: bool) -> CommandResult {
    let mut session = connection.open()?;
    set_device_time(&mut session, connection);
    let run_id = session.job_run_once()?;
    println!("Run {run_id} started");
    if !wait {
        return Ok(());
    }
    wait_for_pass(&mut session, run_id)
}

fn wait_for_pass(session: &mut DeviceSession, run_id: u32) -> CommandResult {
    let result = manage::wait_for_run(session, run_id, RUN_POLL_INTERVAL)?;
    println!("{}", describe_result(&result));
    if result.outcome != Outcome::Pass as i32 {
        return Err(format!("run {run_id} did not pass").into());
    }
    Ok(())
}

/// Compiles and activates the job, programs the target once and returns the
/// open connection so that the caller can attach to it.
pub fn program_target(
    connection: &Connection,
    source: &JobSource,
) -> Result<DeviceSession, Box<dyn Error>> {
    let job = build_job(source)?;
    let mut session = connection.open()?;
    set_device_time(&mut session, connection);
    manage::push_job(&mut session, &job.bytes, &job.images, Trigger::Manual)?;
    println!("Job '{}' active, programming", job.name);
    let run_id = session.job_run_once()?;
    wait_for_pass(&mut session, run_id)?;
    Ok(session)
}

/// `job status`.
pub fn job_status(connection: &Connection) -> CommandResult {
    let status = connection.open()?.job_get()?;
    if status.has_job {
        println!("Job:      {} (sha256 {})", status.name, hex(&status.sha256));
    } else {
        println!("Job:      none");
    }
    let trigger = Trigger::try_from(status.trigger).unwrap_or(Trigger::Manual);
    println!("Trigger:  {}", trigger_name(trigger));
    if status.current_run_id != 0 {
        println!("State:    running (run {})", status.current_run_id);
    } else {
        println!("State:    idle");
    }
    match status.last_result {
        Some(result) if result.run_id != 0 => println!("Last run: {}", describe_result(&result)),
        _ => println!("Last run: none since boot"),
    }
    Ok(())
}

/// `job cancel`.
pub fn job_cancel(connection: &Connection, run_id: u32) -> CommandResult {
    connection.open()?.job_cancel(run_id)?;
    println!("Cancel requested");
    Ok(())
}

fn trigger_name(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Manual => "manual",
        Trigger::AutoOnDetect => "auto on target detect",
    }
}

fn describe_result(result: &RunResult) -> String {
    let seconds = f64::from(result.duration_ms) / 1000.0;
    let outcome = Outcome::try_from(result.outcome).unwrap_or(Outcome::None);
    let stage = Stage::try_from(result.failed_stage).unwrap_or(Stage::None);
    match outcome {
        Outcome::Pass => format!("run {} passed in {seconds:.1} s", result.run_id),
        Outcome::Cancelled => format!(
            "run {} cancelled during {} after {seconds:.1} s",
            result.run_id,
            stage_name(stage)
        ),
        _ => format!(
            "run {} failed at {} after {seconds:.1} s",
            result.run_id,
            stage_name(stage)
        ),
    }
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::None => "an unknown stage",
        Stage::Load => "load (job or images)",
        Stage::PreProgram => "pre-program steps",
        Stage::Detect => "target detect",
        Stage::Erase => "erase",
        Stage::Program => "program",
        Stage::Verify => "verify",
        Stage::SelfTest => "self test",
        Stage::PostProgram => "post-program steps",
        Stage::CurrentTest => "current test",
    }
}
