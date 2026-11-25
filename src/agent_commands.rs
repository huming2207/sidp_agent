//! Agent session commands: the MCP server, and eval/run for people.
use crate::commands::{Connection, DeviceSession};
use clap::{Args, ValueEnum};
use sidp_agent::{
    agent::{self, Allow, Incident, Outcome, SessionEnd, Symbols, TrapConfig, TrapMode},
    mcp, script,
};
use std::{
    error::Error,
    path::Path,
    time::{Duration, Instant},
};

type CommandResult = Result<(), Box<dyn Error>>;

/// How the target is left when an agent session ends.
#[derive(Clone, Copy, ValueEnum)]
pub enum SessionEndArg {
    /// A running target keeps running, a halted one stays halted.
    Asis,
    /// The target runs.
    Resume,
    /// The target is reset and runs.
    Reset,
}

/// Permissions and target handling of an agent session.
#[derive(Args)]
pub struct AgentOptions {
    /// Let scripts write target memory and core registers, and call target functions.
    #[arg(long)]
    allow_write: bool,
    /// Let scripts erase and write target flash.
    #[arg(long)]
    allow_flash: bool,
    /// Let scripts reset the target (reset(), reinit(), 'reset' reflexes and handler results).
    #[arg(long)]
    allow_reset: bool,
    /// How to leave the target when the session ends. Watches, vector catch and
    /// breakpoints are always removed first.
    #[arg(long, value_enum, default_value_t = SessionEndArg::Asis)]
    session_end: SessionEndArg,
    /// Hold the target in reset while connecting; it then starts halted.
    #[arg(long)]
    connect_under_reset: bool,
}

impl AgentOptions {
    fn allow(&self) -> Allow {
        Allow {
            write: self.allow_write,
            flash: self.allow_flash,
            reset: self.allow_reset,
        }
    }

    fn session_end(&self) -> SessionEnd {
        match self.session_end {
            SessionEndArg::Asis => SessionEnd::AsIs,
            SessionEndArg::Resume => SessionEnd::Resume,
            SessionEndArg::Reset => SessionEnd::Reset,
        }
    }

    fn open_request(&self) -> agent::OpenRequest {
        agent::OpenRequest {
            permissions: Some(self.allow().into()),
            session_end: self.session_end() as i32,
            connect_under_reset: self.connect_under_reset,
        }
    }
}

fn load_symbols(elf: Option<&Path>) -> Result<Option<Symbols>, Box<dyn Error>> {
    let Some(path) = elf else { return Ok(None) };
    let symbols = Symbols::load(path)?;
    eprintln!("{}: {} functions", path.display(), symbols.len());
    Ok(Some(symbols))
}

/// Serves MCP on stdin/stdout until the client closes stdin.
pub fn mcp(connection: &Connection, options: &AgentOptions, elf: Option<&Path>) -> CommandResult {
    if connection.would_ask_password() {
        return Err(
            "stdin carries MCP: give the device password with --password or SIDP_PASSWORD".into(),
        );
    }
    let connection = connection.clone();
    let connect: mcp::Connect = Box::new(move || connection.open().map_err(|e| e.to_string()));
    let options = mcp::Options {
        allow: options.allow(),
        session_end: options.session_end(),
        connect_under_reset: options.connect_under_reset,
        symbols: load_symbols(elf)?,
    };
    eprintln!("sidp-agent MCP server on stdio");
    mcp::serve(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        connect,
        options,
    )?;
    Ok(())
}

fn open(connection: &Connection, options: &AgentOptions) -> Result<DeviceSession, Box<dyn Error>> {
    let mut session = connection.open()?;
    script::check_device(&session.device_info()?, false)?;
    let info = session.agent_open(&options.open_request())?;
    eprintln!(
        "Agent session open: CPU ID 0x{:08x}, QuickJS-NG {}, {} KB for scripts",
        info.cpu_id, info.js_version, info.arena_kb
    );
    Ok(session)
}

fn print_result(r: &agent::ScriptResult) -> CommandResult {
    match Outcome::try_from(r.outcome).unwrap_or(Outcome::Error) {
        Outcome::Ok => {
            if !r.value.is_empty() {
                println!(
                    "{}{}",
                    r.value,
                    if r.truncated { " (truncated)" } else { "" }
                );
            }
            Ok(())
        }
        Outcome::Pending => {
            eprintln!("still running after the timeout");
            Ok(())
        }
        Outcome::Error => Err(if r.stack.is_empty() {
            r.error.clone()
        } else {
            format!("{}\n{}", r.error, r.stack)
        }
        .into()),
        Outcome::Stopped => Err(format!("stopped: {}", r.error).into()),
    }
}

/// Evaluates `code` in a new session and prints its value.
pub fn eval(
    connection: &Connection,
    options: &AgentOptions,
    code: &str,
    wait_ms: u64,
) -> CommandResult {
    let blob = sidp_qjsc::compile_eval_container(code, "eval.js", script::Strip::None)?;
    let mut session = open(connection, options)?;
    let result = session.script_eval(&blob, Duration::from_millis(wait_ms));
    let printed = result
        .map_err(Box::<dyn Error>::from)
        .and_then(|r| print_result(&r));
    session.agent_close()?;
    printed
}

/// Runs a module in a new session and prints its events as JSON lines.
pub fn run(
    connection: &Connection,
    options: &AgentOptions,
    file: &Path,
    seconds: f64,
    elf: Option<&Path>,
) -> CommandResult {
    let symbols = load_symbols(elf)?;
    let source = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent.js");
    let blob = sidp_qjsc::compile_container(&source, name, script::Strip::None)?;
    let mut session = open(connection, options)?;
    let mut after = session.agent_info()?.newest_event_seq;
    // Events aren't read while this waits, and the device keeps only the last
    // 64: a module that logs from its first line loses them after a few
    // seconds. One second holds a full console/emit burst (40) plus a second
    // of the steady rate; a later outcome comes as a `result` event.
    print_result(&session.script_run(&blob, Duration::from_secs(1))?)?;
    let end = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    while end.is_none_or(|end| Instant::now() < end) {
        let read = session.event_read(after)?;
        let more = read.events.last().is_some_and(|e| e.seq < read.newest_seq);
        for e in read.events {
            let missed = e.seq.saturating_sub(after.saturating_add(1));
            if missed > 0 {
                println!(
                    "{}",
                    serde_json::json!({ "kind": "missed", "count": missed })
                );
            }
            after = e.seq;
            let mut data: serde_json::Value =
                serde_json::from_str(&e.json).unwrap_or(serde_json::Value::String(e.json));
            if let (Some(syms), Some(pc)) = (&symbols, data["pc"].as_str())
                && let Ok(pc) = u32::from_str_radix(pc.trim_start_matches("0x"), 16)
                && let Some(name) = syms.describe(pc)
            {
                data["pc_symbol"] = name.into();
            }
            let line = serde_json::json!({
                "seq": e.seq, "kind": e.kind, "uptime_us": e.uptime_us,
                "capture_id": e.capture_id, "data": data,
            });
            println!("{line}");
        }
        if !more {
            session.poll(Duration::from_millis(200))?;
        }
    }
    session.agent_close()?;
    Ok(())
}

/// What a trap gets; see `sidp-agent trap arm --help`.
#[derive(Args)]
pub struct TrapOptions {
    /// The trap's name, shown in its status and incidents.
    #[arg(long, value_parser = trap_name)]
    name: String,
    /// What happens when GDB attaches or a programming run starts: `yield`
    /// pauses the trap and restarts it after, `exclusive` refuses them.
    #[arg(long, value_enum, default_value_t = TrapModeArg::Yield)]
    mode: TrapModeArg,
    /// Incidents recorded per hour; more are only counted. 0 for the device's default.
    #[arg(long, default_value_t = 0)]
    incidents_per_hour: u32,
    /// Let the module write target memory and core registers, and call target functions.
    #[arg(long)]
    allow_write: bool,
    /// Let the module erase and write target flash.
    #[arg(long)]
    allow_flash: bool,
    /// Let the module reset the target.
    #[arg(long)]
    allow_reset: bool,
    /// Hold the target in reset while connecting; it then starts halted.
    #[arg(long)]
    connect_under_reset: bool,
}

/// A trap's ownership mode.
#[derive(Clone, Copy, ValueEnum)]
pub enum TrapModeArg {
    /// GDB and programming runs pause the trap.
    Yield,
    /// GDB and programming runs are refused while the trap is armed.
    Exclusive,
}

fn trap_name(s: &str) -> Result<String, String> {
    if s.is_empty() || s.len() > 23 {
        return Err("1-23 bytes".into());
    }
    Ok(s.to_owned())
}

impl TrapOptions {
    fn config(&self) -> TrapConfig {
        TrapConfig {
            name: self.name.clone(),
            mode: match self.mode {
                TrapModeArg::Yield => TrapMode::Yield,
                TrapModeArg::Exclusive => TrapMode::Exclusive,
            } as i32,
            permissions: Some(
                Allow {
                    write: self.allow_write,
                    flash: self.allow_flash,
                    reset: self.allow_reset,
                }
                .into(),
            ),
            connect_under_reset: self.connect_under_reset,
            incidents_per_hour: self.incidents_per_hour,
        }
    }
}

fn open_plain(connection: &Connection) -> Result<DeviceSession, Box<dyn Error>> {
    let mut session = connection.open()?;
    script::check_device(&session.device_info()?, false)?;
    Ok(session)
}

/// Arms `file` as the device's trap.
pub fn trap_arm(connection: &Connection, options: &TrapOptions, file: &Path) -> CommandResult {
    let source = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("trap.js");
    let blob = sidp_qjsc::compile_container(&source, name, script::Strip::None)?;
    let mut session = open_plain(connection)?;
    let status = session.trap_arm(options.config(), &blob, Duration::from_secs(5))?;
    if status.config.is_none() {
        let first = status.first_result.unwrap_or_default();
        print_result(&first)?;
        return Err("the trap was not armed".into());
    }
    println!("{:#}", agent::trap_status_json(&status));
    Ok(())
}

/// Prints the trap's status as JSON.
pub fn trap_status(connection: &Connection) -> CommandResult {
    let status = open_plain(connection)?.trap_status()?;
    println!("{:#}", agent::trap_status_json(&status));
    Ok(())
}

/// Disarms the trap.
pub fn trap_disarm(connection: &Connection) -> CommandResult {
    open_plain(connection)?.trap_disarm()?;
    eprintln!("Trap disarmed");
    Ok(())
}

/// Lists the device's incidents.
pub fn incident_list(connection: &Connection) -> CommandResult {
    let list = open_plain(connection)?.incident_list(0)?;
    if list.is_empty() {
        eprintln!("No incidents");
    }
    for i in list {
        let when = if i.utc_ms != 0 {
            agent::utc_text(i.utc_ms)
        } else {
            format!("boot {} +{:.1} s", i.boot, i.uptime_us as f64 / 1e6)
        };
        println!(
            "{:>6}  {:<6}  {:<23}  {:<23}  {:>6} B  {when}",
            i.id,
            if i.kind == 0 { "watch" } else { "script" },
            i.name,
            i.trap,
            i.size
        );
    }
    Ok(())
}

/// Prints an incident as JSON.
pub fn incident_show(connection: &Connection, id: u32, elf: Option<&Path>) -> CommandResult {
    let symbols = load_symbols(elf)?;
    let data = open_plain(connection)?.incident_read(id)?;
    let incident = Incident::parse(&data)?;
    println!("{:#}", incident.to_json(symbols.as_ref()));
    Ok(())
}

/// Deletes an incident, or all of them for 0.
pub fn incident_rm(connection: &Connection, id: u32) -> CommandResult {
    open_plain(connection)?.incident_delete(id)?;
    Ok(())
}
