//! Agent session commands: the MCP server, and eval/run for people.
use crate::commands::{Connection, DeviceSession};
use clap::{Args, ValueEnum};
use sidp_agent::{
    agent::{self, Allow, Outcome, SessionEnd, Symbols},
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
    timeout_ms: u64,
) -> CommandResult {
    let blob = sidp_qjsc::compile_eval_container(code, "eval.js", script::Strip::None)?;
    let mut session = open(connection, options)?;
    let result = session.script_eval(&blob, Duration::from_millis(timeout_ms));
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
    print_result(&session.script_run(&blob, Duration::from_secs(5))?)?;
    let end = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    while end.is_none_or(|end| Instant::now() < end) {
        let read = session.event_read(after)?;
        let more = read.events.last().is_some_and(|e| e.seq < read.newest_seq);
        for e in read.events {
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
