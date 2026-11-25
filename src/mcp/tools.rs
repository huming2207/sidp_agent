//! The MCP tools, and the device connection and agent session behind them.
//!
//! The connection opens on the first tool that needs the target. When it is
//! lost, the next tool opens a new one and says that the old session's
//! runtime, watches and captures are gone.
use super::Cancel;
use crate::{
    agent::{
        self, Allow, Capture, Incident, Outcome, ScriptResult, SessionEnd, SessionInfo, Symbols,
        TrapConfig, TrapMode,
    },
    script,
    session::{self, Session},
    transport::Transport,
};
use serde_json::{Map, Value, json};
use sidp_qjsc::Strip;
use std::time::{Duration, Instant};

/// A connection to the device.
pub type DeviceSession = Session<Box<dyn Transport + Send>>;
/// Opens a fresh connection (and logs in over the network).
pub type Connect = Box<dyn FnMut() -> Result<DeviceSession, String> + Send>;

/// What the person running the server allowed and asked for.
pub struct Options {
    /// Permissions beyond observe, halt and resume.
    pub allow: Allow,
    /// How the target is left when a session ends.
    pub session_end: SessionEnd,
    /// Connect with the target held in reset.
    pub connect_under_reset: bool,
    /// The firmware's symbols, to name code addresses.
    pub symbols: Option<Symbols>,
}

const DEFAULT_WAIT_MS: u64 = 5000;
const MAX_WAIT_MS: u64 = 60_000;
const DEFAULT_EVENT_WAIT_MS: u64 = 10_000;
const MAX_EVENT_WAIT_MS: u64 = 50_000;
const MAX_EVENTS: usize = 64;
const EVENT_RECHECK: Duration = Duration::from_millis(500);
const EVENT_POLL: Duration = Duration::from_millis(50);
const MAX_READ: u32 = 4096;
const READ_CHUNK: u32 = 1024; // 2 hex digits a byte in one eval value

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    read_only: bool,
}

fn no_args() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn wait_arg(what: &str) -> Value {
    json!({
        "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS,
        "description": format!("How long to wait for {what}, in ms (default {DEFAULT_WAIT_MS}). \
                                Past it the script carries on and its result comes as a 'result' event."),
    })
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "device_info",
        title: "Device and session",
        description: "The probe (serial, firmware, script engine), this server's permissions and \
                      session settings, and the open session's target (CPU ID, profile, FPU). \
                      Doesn't open a session.",
        schema: no_args,
        read_only: true,
    },
    Tool {
        name: "eval",
        title: "Evaluate JavaScript",
        description: "Runs JavaScript in the session's runtime (starting one if none runs) and \
                      returns the value of its last statement as JSON. Top-level await works; \
                      `var`s and functions persist, and so do globalThis properties a run_script \
                      module set. Globals: target, flash, crypto, agent, console. Writes, flash \
                      and reset need the permissions device_info shows.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "A script, not a module: no import/export." },
                    "timeout_ms": wait_arg("the value"),
                },
                "required": ["code"],
                "additionalProperties": false,
            })
        },
        read_only: false,
    },
    Tool {
        name: "run_script",
        title: "Run a module",
        description: "Replaces the session's runtime with an ES module: the old runtime, its \
                      watches and its globals end first. For watches and code that keeps \
                      running; it may import from si:target, si:flash, si:crypto and si:agent. \
                      Returns once the module's top level finished (or threw).",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "One ES module; bundle local imports into it." },
                    "name": { "type": "string", "description": "File name in stack traces (default agent.js)." },
                    "timeout_ms": wait_arg("the top level"),
                },
                "required": ["source"],
                "additionalProperties": false,
            })
        },
        read_only: false,
    },
    Tool {
        name: "stop_script",
        title: "Stop the runtime",
        description: "Stops the session's runtime; its watches and globals go with it. The \
                      target is left as it is.",
        schema: no_args,
        read_only: false,
    },
    Tool {
        name: "wait_event",
        title: "Wait for events",
        description: "Returns the events after `after_seq` (default: after the last ones this \
                      tool returned), waiting up to timeout_ms for the first. Kinds: watch \
                      (a watch fired; capture_id names its snapshot), stop (the target halted \
                      with no watch on it), console, emit (agent.emit), error (uncaught in a \
                      script), result (a run/eval that finished after its timeout), runtime \
                      (the runtime ended by itself), target (lost, back, reset).",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "after_seq": { "type": "integer", "minimum": 0 },
                    "timeout_ms": {
                        "type": "integer", "minimum": 0, "maximum": MAX_EVENT_WAIT_MS,
                        "description": format!("Default {DEFAULT_EVENT_WAIT_MS}; 0 returns at once."),
                    },
                    "kinds": { "type": "array", "items": { "type": "string" },
                               "description": "Only these kinds; others are skipped." },
                },
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "get_capture",
        title: "Read a capture",
        description: "The snapshot a watch took when it fired: registers, fault registers with \
                      their bits explained, the stacked exception frame, stack and memory bytes. \
                      Only the latest few captures are kept.",
        schema: || {
            json!({
                "type": "object",
                "properties": { "capture_id": { "type": "integer", "minimum": 1 } },
                "required": ["capture_id"],
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "list_watches",
        title: "List watches",
        description: "The watches the running runtime has set (agent.watches()).",
        schema: no_args,
        read_only: true,
    },
    Tool {
        name: "read_memory",
        title: "Read memory",
        description: "Reads target memory as hex, at most 4096 bytes. Works while the target runs.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "address": { "type": ["integer", "string"], "description": "A number or a hex string like 0x20000000." },
                    "length": { "type": "integer", "minimum": 1, "maximum": MAX_READ },
                },
                "required": ["address", "length"],
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "registers",
        title: "Core registers",
        description: "r0-r12, sp, lr, pc, xpsr, msp, psp and the special registers. The target \
                      must be halted.",
        schema: no_args,
        read_only: true,
    },
    Tool {
        name: "halt",
        title: "Halt",
        description: "Halts the target and returns its pc.",
        schema: no_args,
        read_only: false,
    },
    Tool {
        name: "resume",
        title: "Resume",
        description: "Lets a halted target run.",
        schema: no_args,
        read_only: false,
    },
    Tool {
        name: "reset",
        title: "Reset",
        description: "Pulses the target's reset line; it then runs. Needs --allow-reset.",
        schema: no_args,
        read_only: false,
    },
    Tool {
        name: "lookup_symbol",
        title: "Look up a symbol",
        description: "With the firmware's ELF (--elf): the address of a function by `name`, or \
                      the function an `address` is in.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "address": { "type": ["integer", "string"] },
                },
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "arm_trap",
        title: "Arm a trap",
        description: "For bugs that take hours or days to show: leaves an ES module running on \
                      the probe with no computer attached, through disconnects and probe \
                      restarts, and records an incident to the probe's flash each time one of \
                      its watches fires or it calls agent.incident(). A watch handler can return \
                      { action, data } to put its own JSON (say, a history it kept) in the \
                      incident. Replaces the armed trap and ends this session first. The module \
                      gets this server's permissions. It restarts from the top after a probe \
                      restart or a pause, so keep its state small and rebuildable. Read the \
                      si://guide resource's trap section first.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "One ES module; bundle local imports into it." },
                    "trap_name": { "type": "string", "minLength": 1, "maxLength": 23 },
                    "mode": { "type": "string", "enum": ["yield", "exclusive"],
                              "description": "yield (default): GDB attaching or a programming run pauses the trap, \
                                              which restarts after. exclusive: they are refused while it is armed." },
                    "incidents_per_hour": { "type": "integer", "minimum": 0, "maximum": 3600,
                                            "description": "More are only counted; 0 for the probe's default." },
                    "name": { "type": "string", "description": "File name in stack traces (default trap.js)." },
                    "timeout_ms": wait_arg("the top level"),
                },
                "required": ["source", "trap_name"],
                "additionalProperties": false,
            })
        },
        read_only: false,
    },
    Tool {
        name: "trap_status",
        title: "Trap status",
        description: "The armed trap (name, mode, state: running, waiting, paused or failed, and \
                      why), how often it started, and the probe's incident count. While a trap \
                      is armed no session can open: wait_event and get_capture read the trap's \
                      events and captures instead.",
        schema: no_args,
        read_only: true,
    },
    Tool {
        name: "disarm_trap",
        title: "Disarm the trap",
        description: "Stops and forgets the trap; the target is left as it is and the incidents \
                      stay.",
        schema: no_args,
        read_only: false,
    },
    Tool {
        name: "list_incidents",
        title: "List incidents",
        description: "The incidents on the probe, oldest first: id, watch or script, name, trap, \
                      time (UTC when the probe knew it, else boot count and uptime), size.",
        schema: || {
            json!({
                "type": "object",
                "properties": { "after_id": { "type": "integer", "minimum": 0 } },
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "get_incident",
        title: "Read an incident",
        description: "One incident: what happened, the handler's data, what the watch did, the \
                      events before it, and the capture decoded as get_capture does.",
        schema: || {
            json!({
                "type": "object",
                "properties": { "incident_id": { "type": "integer", "minimum": 1 } },
                "required": ["incident_id"],
                "additionalProperties": false,
            })
        },
        read_only: true,
    },
    Tool {
        name: "delete_incident",
        title: "Delete incidents",
        description: "Deletes one incident, or all of them with incident_id 0. The probe also \
                      drops the oldest when its space runs out.",
        schema: || {
            json!({
                "type": "object",
                "properties": { "incident_id": { "type": "integer", "minimum": 0 } },
                "required": ["incident_id"],
                "additionalProperties": false,
            })
        },
        read_only: false,
    },
    Tool {
        name: "close_session",
        title: "Close the session",
        description: "Ends the agent session: watches and vector catch go, then the target is \
                      left as the server's --session-end says, and the probe lets go of it (so \
                      GDB or a job run can have it). The next tool opens a new session.",
        schema: no_args,
        read_only: false,
    },
];

/// The tools, as tools/list returns them.
pub fn list() -> Value {
    TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": { "readOnlyHint": t.read_only, "openWorldHint": false },
            })
        })
        .collect()
}

/// Whether there is a tool called `name`.
pub fn exists(name: &str) -> bool {
    TOOLS.iter().any(|t| t.name == name)
}

/// The device behind the tools; one at a time.
pub struct Device {
    connect: Connect,
    options: Options,
    conn: Option<DeviceSession>,
    info: Option<SessionInfo>,
    lost: Option<String>, // why the last session ended, until a new one says so
    cursor: u32,
    evals: u32,
    trap_noted: bool,
}

type ToolResult = Result<Value, String>;

fn session_error(e: &session::Error) -> String {
    match e {
        session::Error::Status(s) => format!("{} (status {s})", agent::describe_status(*s)),
        session::Error::Io(e) => format!("connection to the probe lost: {e}"),
        session::Error::State(s) => (*s).to_owned(),
    }
}

fn wait_ms(args: &Value) -> Result<Duration, String> {
    let ms = args.get("timeout_ms").map_or(Ok(DEFAULT_WAIT_MS), |v| {
        v.as_u64()
            .filter(|&ms| ms <= MAX_WAIT_MS)
            .ok_or_else(|| format!("timeout_ms must be 0-{MAX_WAIT_MS}"))
    })?;
    Ok(Duration::from_millis(if ms == 0 {
        DEFAULT_WAIT_MS
    } else {
        ms
    }))
}

fn address(v: &Value) -> Result<u32, String> {
    let bad = || "address must be a 32-bit number or a hex string like 0x20000000".to_owned();
    match v {
        Value::Number(n) => n
            .as_u64()
            .and_then(|a| u32::try_from(a).ok())
            .ok_or_else(bad),
        Value::String(s) => {
            let s = s.trim();
            let digits = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(s);
            u32::from_str_radix(digits, 16).map_err(|_| bad())
        }
        _ => Err(bad()),
    }
}

fn outcome_name(o: Outcome) -> &'static str {
    match o {
        Outcome::Ok => "ok",
        Outcome::Error => "error",
        Outcome::Pending => "pending",
        Outcome::Stopped => "stopped",
    }
}

impl Device {
    /// A device the tools connect to with `connect` when they first need it.
    pub fn new(connect: Connect, options: Options) -> Self {
        Self {
            connect,
            options,
            conn: None,
            info: None,
            lost: None,
            cursor: 0,
            evals: 0,
            trap_noted: false,
        }
    }

    /// Between requests: takes the device's notices off the wire.
    pub fn idle(&mut self) {
        if let Some(conn) = &mut self.conn
            && let Err(e) = conn.poll_ready()
        {
            self.drop_connection(session_error(&e));
        }
    }

    /// Before exiting: ends the session the way --session-end says.
    pub fn shutdown(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            if self.info.is_some() {
                let _ = conn.agent_close();
            }
            conn.close();
        }
        self.info = None;
    }

    fn drop_connection(&mut self, why: String) {
        if self.info.is_some() {
            self.lost = Some(why);
        }
        if let Some(mut conn) = self.conn.take() {
            conn.close();
        }
        self.info = None;
    }

    fn connection(&mut self) -> Result<&mut DeviceSession, String> {
        if self.conn.as_ref().is_some_and(|c| c.is_closed()) {
            self.drop_connection("the connection to the probe closed".into());
        }
        if self.conn.is_none() {
            let mut conn = (self.connect)()?;
            let info = conn.device_info().map_err(|e| session_error(&e))?;
            script::check_device(&info, false).map_err(|e| e.0)?;
            self.conn = Some(conn);
        }
        Ok(self.conn.as_mut().unwrap())
    }

    /// The connection with a session open; `notes` gets what the agent should know about it.
    fn session(&mut self, notes: &mut Vec<String>) -> Result<&mut DeviceSession, String> {
        self.connection()?;
        if self.info.is_none() {
            let request = agent::OpenRequest {
                permissions: Some(self.options.allow.into()),
                session_end: self.options.session_end as i32,
                connect_under_reset: self.options.connect_under_reset,
            };
            let conn = self.conn.as_mut().unwrap();
            let info = match conn.agent_open(&request) {
                Ok(info) => info,
                Err(e) => {
                    let text = session_error(&e);
                    if conn.is_closed() {
                        self.drop_connection(text.clone());
                    }
                    return Err(text);
                }
            };
            if let Some(why) = self.lost.take() {
                notes.push(format!(
                    "The previous agent session ended ({why}). Its runtime, watches and captures \
                     are gone; this is a new session."
                ));
            }
            self.cursor = info.newest_event_seq;
            self.info = Some(info);
        }
        Ok(self.conn.as_mut().unwrap())
    }

    /// Runs `f` on the session, dropping the connection when it broke.
    fn with_session<R>(
        &mut self,
        notes: &mut Vec<String>,
        f: impl FnOnce(&mut DeviceSession) -> session::Result<R>,
    ) -> Result<R, String> {
        let conn = self.session(notes)?;
        let r = f(conn);
        match r {
            Ok(v) => Ok(v),
            Err(e) => {
                let text = session_error(&e);
                let closed = self.conn.as_ref().is_none_or(|c| c.is_closed());
                if closed || matches!(e, session::Error::Status(agent::STATUS_NO_SESSION)) {
                    self.drop_connection(text.clone());
                }
                Err(text)
            }
        }
    }

    /// Runs the tool `name`.
    pub fn call(&mut self, name: &str, args: &Value, cancel: &Cancel) -> ToolResult {
        let mut notes = Vec::new();
        let result = match name {
            "device_info" => self.device_info(),
            "eval" => self.eval(args, &mut notes),
            "run_script" => self.run_script(args, &mut notes),
            "stop_script" => self
                .with_session(&mut notes, |c| c.script_stop())
                .map(|_| json!({ "stopped": true })),
            "wait_event" => self.wait_event(args, cancel, &mut notes),
            "get_capture" => self.get_capture(args, &mut notes),
            "list_watches" => self
                .quick_eval("agent.watches()", &mut notes)
                .map(|v| json!({ "watches": v })),
            "read_memory" => self.read_memory(args, &mut notes),
            "registers" => self.registers(&mut notes),
            "halt" => self.halt(&mut notes),
            "resume" => self
                .quick_eval("target.resume(); true", &mut notes)
                .map(|_| json!({ "running": true })),
            "reset" => self
                .quick_eval("await target.reset(); true", &mut notes)
                .map(|_| json!({ "reset": true, "running": true })),
            "lookup_symbol" => self.lookup_symbol(args),
            "close_session" => self.close_session(),
            "arm_trap" => self.arm_trap(args, &mut notes),
            "trap_status" => self
                .with_connection(|c| c.trap_status())
                .map(|st| agent::trap_status_json(&st)),
            "disarm_trap" => self.with_connection(|c| c.trap_disarm()).map(|_| {
                self.trap_noted = false;
                json!({ "disarmed": true })
            }),
            "list_incidents" => self.list_incidents(args),
            "get_incident" => self.get_incident(args),
            "delete_incident" => self.delete_incident(args),
            _ => Err(format!("no tool {name}")),
        };
        let mut value = result?;
        if !notes.is_empty() {
            if let Some(map) = value.as_object_mut() {
                map.insert("notes".into(), notes.into());
            } else {
                value = json!({ "result": value, "notes": notes });
            }
        }
        Ok(value)
    }

    fn device_info(&mut self) -> ToolResult {
        let allow = self.options.allow;
        let mut out = json!({
            "server": {
                "compiler": sidp_qjsc::version(),
                "permissions": { "write": allow.write, "flash": allow.flash, "reset": allow.reset },
                "session_end": match self.options.session_end {
                    SessionEnd::AsIs => "asis",
                    SessionEnd::Resume => "resume",
                    SessionEnd::Reset => "reset",
                },
                "connect_under_reset": self.options.connect_under_reset,
                "elf_functions": self.options.symbols.as_ref().map_or(0, Symbols::len),
            },
        });
        let conn = self.connection()?;
        let info = conn.device_info().map_err(|e| session_error(&e))?;
        out["probe"] = json!({
            "serial": info.serial,
            "firmware": info.firmware_version,
            "js_version": info.js_version,
            "js_arena_kb": info.js_arena_kb,
        });
        out["session"] = match &self.info {
            Some(s) => json!({
                "open": true,
                "cpu_id": agent::hex(s.cpu_id),
                "profile": match s.profile { 1 => "ARMv6-M", 2 => "ARMv7-M", 3 => "ARMv7E-M", _ => "?" },
                "fpu": s.fpu,
                "arena_kb": s.arena_kb,
            }),
            None => json!({ "open": false }),
        };
        Ok(out)
    }

    fn script_result(&self, r: ScriptResult) -> Value {
        let outcome = Outcome::try_from(r.outcome).unwrap_or(Outcome::Error);
        let mut out = json!({ "outcome": outcome_name(outcome) });
        if outcome == Outcome::Ok && !r.value.is_empty() {
            out["value"] = if r.truncated || r.value == "undefined" {
                Value::String(r.value)
            } else {
                serde_json::from_str(&r.value).unwrap_or(Value::String(r.value))
            };
            if r.truncated {
                out["truncated"] = true.into();
                out["note"] = format!(
                    "the value's JSON was cut at {} bytes; return less, or emit parts",
                    agent::MAX_VALUE
                )
                .into();
            }
        }
        if !r.error.is_empty() {
            out["error"] = r.error.into();
        }
        if !r.stack.is_empty() {
            out["stack"] = r.stack.into();
        }
        if outcome == Outcome::Pending {
            out["note"] = "still running; its result will be a 'result' event (wait_event)".into();
        }
        out
    }

    fn eval_code(
        &mut self,
        code: &str,
        wait: Duration,
        notes: &mut Vec<String>,
    ) -> Result<ScriptResult, String> {
        self.evals += 1;
        let name = format!("eval{}.js", self.evals);
        let blob = sidp_qjsc::compile_eval_container(code, &name, Strip::None)
            .map_err(|e| format!("doesn't compile: {e}"))?;
        self.with_session(notes, |c| c.script_eval(&blob, wait))
    }

    fn eval(&mut self, args: &Value, notes: &mut Vec<String>) -> ToolResult {
        let code = args["code"].as_str().ok_or("eval needs code")?;
        let r = self.eval_code(code, wait_ms(args)?, notes)?;
        Ok(self.script_result(r))
    }

    /// The value of a short eval of our own, or its error.
    fn quick_eval(&mut self, code: &str, notes: &mut Vec<String>) -> ToolResult {
        let r = self.eval_code(code, Duration::from_millis(DEFAULT_WAIT_MS), notes)?;
        let outcome = Outcome::try_from(r.outcome).unwrap_or(Outcome::Error);
        if outcome != Outcome::Ok {
            let why = if r.error.is_empty() {
                outcome_name(outcome).to_owned()
            } else {
                r.error
            };
            return Err(why);
        }
        if r.value == "undefined" {
            return Ok(Value::Null);
        }
        serde_json::from_str(&r.value).map_err(|_| format!("unexpected value {}", r.value))
    }

    fn run_script(&mut self, args: &Value, notes: &mut Vec<String>) -> ToolResult {
        let source = args["source"].as_str().ok_or("run_script needs source")?;
        let name = args["name"].as_str().unwrap_or("agent.js");
        let blob = sidp_qjsc::compile_container(source, name, Strip::None)
            .map_err(|e| format!("doesn't compile: {e}"))?;
        let wait = wait_ms(args)?;
        let r = self.with_session(notes, |c| c.script_run(&blob, wait))?;
        Ok(self.script_result(r))
    }

    fn event_json(&self, e: agent::Event) -> Value {
        let mut v = serde_json::from_str::<Value>(&e.json)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "raw": e.json }));
        if let (Some(syms), Some(pc)) = (
            &self.options.symbols,
            v.get("pc").and_then(|p| address(p).ok()),
        ) && let Some(name) = syms.describe(pc)
        {
            v["pc_symbol"] = name.into();
        }
        let mut out = Map::new();
        out.insert("seq".into(), e.seq.into());
        out.insert("kind".into(), e.kind.into());
        out.insert("uptime_us".into(), e.uptime_us.into());
        if e.capture_id != 0 {
            out.insert("capture_id".into(), e.capture_id.into());
        }
        out.insert("data".into(), v);
        out.into()
    }

    fn wait_event(&mut self, args: &Value, cancel: &Cancel, notes: &mut Vec<String>) -> ToolResult {
        let timeout = args
            .get("timeout_ms")
            .map_or(Ok(DEFAULT_EVENT_WAIT_MS), |v| {
                v.as_u64()
                    .filter(|&ms| ms <= MAX_EVENT_WAIT_MS)
                    .ok_or_else(|| format!("timeout_ms must be 0-{MAX_EVENT_WAIT_MS}"))
            })?;
        let kinds: Option<Vec<String>> = match args.get("kinds") {
            None => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|k| k.as_str().map(str::to_owned).ok_or("kinds must be strings"))
                    .collect::<Result<_, _>>()?,
            ),
            Some(_) => return Err("kinds must be an array of strings".into()),
        };
        let via_trap = self.trap_instead(notes)?;
        if !via_trap {
            self.session(notes)?;
        }
        let mut after = match args.get("after_seq") {
            None => self.cursor,
            Some(v) => v
                .as_u64()
                .and_then(|s| u32::try_from(s).ok())
                .ok_or("after_seq must be a 32-bit number")?,
        };
        let deadline = Instant::now() + Duration::from_millis(timeout);
        let mut events = Vec::new();
        let mut missed = 0u32;
        let (mut dropped, mut newest);
        loop {
            let mut next_read = Instant::now() + EVENT_RECHECK;
            loop {
                let read = self.on_device(via_trap, notes, |c| c.event_read(after))?;
                dropped = read.dropped;
                newest = read.newest_seq;
                if after > newest {
                    notes.push(format!(
                        "after_seq {after} is past the newest event, {newest}; reading from there"
                    ));
                    after = newest;
                }
                let Some(last) = read.events.last().map(|e| e.seq) else {
                    break;
                };
                for e in read.events {
                    missed += e.seq.saturating_sub(after.saturating_add(1));
                    after = e.seq;
                    if kinds.as_ref().is_none_or(|k| k.contains(&e.kind)) {
                        events.push(self.event_json(e));
                    }
                }
                if events.len() >= MAX_EVENTS || last >= newest {
                    break;
                }
            }
            if !events.is_empty() || Instant::now() >= deadline || events.len() >= MAX_EVENTS {
                break;
            }
            while Instant::now() < deadline.min(next_read) {
                if cancel.requested() {
                    return Err("cancelled".into());
                }
                let notice = self.on_device(via_trap, notes, |c| {
                    c.poll(EVENT_POLL)?;
                    Ok(c.take_agent_notice())
                })?;
                if notice.is_some_and(|n| n > after) {
                    next_read = Instant::now();
                }
            }
        }
        if args.get("after_seq").is_none() || after > self.cursor {
            self.cursor = after;
        }
        let mut out = json!({ "events": events, "next_after_seq": after, "newest_seq": newest });
        if missed > 0 {
            out["missed"] = missed.into();
            notes.push(format!(
                "{missed} event(s) were overwritten on the probe before they were read; \
                 wait_event more often or emit less"
            ));
        }
        if dropped > 0 {
            out["rate_limited_this_session"] = dropped.into();
        }
        Ok(out)
    }

    fn get_capture(&mut self, args: &Value, notes: &mut Vec<String>) -> ToolResult {
        let id = args["capture_id"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .filter(|&i| i != 0)
            .ok_or("capture_id must be a positive number")?;
        let via_trap = self.trap_instead(notes)?;
        let data = self
            .on_device(via_trap, notes, |c| c.capture_read(id))
            .map_err(|e| {
                if e.contains("status 2") {
                    format!("capture {id} is gone: only the latest few are kept")
                } else {
                    e
                }
            })?;
        let capture = Capture::parse(&data).map_err(|e| e.to_string())?;
        Ok(capture.to_json(self.options.symbols.as_ref()))
    }

    fn read_memory(&mut self, args: &Value, notes: &mut Vec<String>) -> ToolResult {
        let addr = address(&args["address"])?;
        let len = args["length"]
            .as_u64()
            .and_then(|l| u32::try_from(l).ok())
            .filter(|&l| (1..=MAX_READ).contains(&l))
            .ok_or(format!("length must be 1-{MAX_READ}"))?;
        addr.checked_add(len - 1)
            .ok_or("the range wraps past 0xffffffff")?;
        let mut hex = String::with_capacity(len as usize * 2);
        let mut at = 0;
        while at < len {
            let n = READ_CHUNK.min(len - at);
            let code = format!(
                "(() => {{ const b = target.readMem({}, {n}); let s = ''; \
                 for (const x of b) s += (x < 16 ? '0' : '') + x.toString(16); return s; }})()",
                addr + at
            );
            match self.quick_eval(&code, notes)? {
                Value::String(s) if s.len() == n as usize * 2 => hex.push_str(&s),
                other => return Err(format!("unexpected readMem result {other}")),
            }
            at += n;
        }
        Ok(json!({ "address": agent::hex(addr), "length": len, "hex": hex }))
    }

    fn registers(&mut self, notes: &mut Vec<String>) -> ToolResult {
        const CODE: &str = "(() => { const h = (v) => '0x' + (v >>> 0).toString(16).padStart(8, '0'); \
            const r = {}; \
            for (const n of ['r0','r1','r2','r3','r4','r5','r6','r7','r8','r9','r10','r11','r12', \
                             'sp','lr','pc','xpsr','msp','psp']) r[n] = h(target.readReg(n)); \
            const s = target.readReg(20) >>> 0; \
            Object.assign(r, { primask: s & 0xff, basepri: (s >>> 8) & 0xff, \
                               faultmask: (s >>> 16) & 0xff, control: s >>> 24 }); \
            return r; })()";
        let mut regs = self.quick_eval(CODE, notes).map_err(|e| {
            if e.contains("NOT_HALTED") {
                "the target is running; halt it first".to_owned()
            } else {
                e
            }
        })?;
        if let Some(syms) = &self.options.symbols {
            for (reg, key) in [("pc", "pc_symbol"), ("lr", "lr_symbol")] {
                if let Some(name) = address(&regs[reg]).ok().and_then(|a| syms.describe(a)) {
                    regs[key] = name.into();
                }
            }
        }
        if let Some(ipsr) = address(&regs["xpsr"])
            .ok()
            .map(|x| x & 0x1ff)
            .filter(|&i| i != 0)
        {
            regs["exception"] = agent::exception_name(ipsr).into();
        }
        Ok(regs)
    }

    fn halt(&mut self, notes: &mut Vec<String>) -> ToolResult {
        let pc = self.quick_eval("target.halt(); target.readReg('pc') >>> 0", notes)?;
        let pc = pc
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .ok_or("unexpected pc")?;
        let mut out = json!({ "halted": true, "pc": agent::hex(pc) });
        if let Some(name) = self.options.symbols.as_ref().and_then(|s| s.describe(pc)) {
            out["pc_symbol"] = name.into();
        }
        Ok(out)
    }

    fn lookup_symbol(&self, args: &Value) -> ToolResult {
        let syms = self
            .options
            .symbols
            .as_ref()
            .ok_or("no symbols: start the server with --elf FIRMWARE.elf")?;
        if let Some(name) = args["name"].as_str() {
            let addr = syms
                .address_of(name)
                .ok_or_else(|| format!("no function {name}"))?;
            return Ok(
                json!({ "name": name, "address": agent::hex(addr), "thumb_address": agent::hex(addr | 1) }),
            );
        }
        let addr = address(args.get("address").ok_or("give a name or an address")?)?;
        Ok(json!({ "address": agent::hex(addr), "symbol": syms.describe(addr) }))
    }

    /// Runs `f` on the connection without a session.
    fn with_connection<R>(
        &mut self,
        f: impl FnOnce(&mut DeviceSession) -> session::Result<R>,
    ) -> Result<R, String> {
        let conn = self.connection()?;
        f(conn).map_err(|e| {
            let text = session_error(&e);
            if self.conn.as_ref().is_none_or(|c| c.is_closed()) {
                self.drop_connection(text.clone());
            }
            text
        })
    }

    /// With no session open and a trap armed, the trap's events and captures are read instead.
    fn trap_instead(&mut self, notes: &mut Vec<String>) -> Result<bool, String> {
        if self.info.is_some() {
            return Ok(false);
        }
        let status = self.with_connection(|c| c.trap_status())?;
        let Some(cfg) = status.config else {
            return Ok(false);
        };
        if !self.trap_noted {
            notes.push(format!(
                "Trap '{}' is armed, so no session opens: these are the trap's events and \
                 captures. Its incidents are kept on the probe (list_incidents).",
                cfg.name
            ));
            self.trap_noted = true;
            self.cursor = 0;
        }
        Ok(true)
    }

    fn on_device<R>(
        &mut self,
        via_trap: bool,
        notes: &mut Vec<String>,
        f: impl FnOnce(&mut DeviceSession) -> session::Result<R>,
    ) -> Result<R, String> {
        if via_trap {
            self.with_connection(f)
        } else {
            self.with_session(notes, f)
        }
    }

    fn arm_trap(&mut self, args: &Value, notes: &mut Vec<String>) -> ToolResult {
        let source = args["source"].as_str().ok_or("arm_trap needs source")?;
        let trap_name = args["trap_name"]
            .as_str()
            .filter(|n| !n.is_empty() && n.len() <= 23)
            .ok_or("trap_name must be 1-23 bytes")?;
        let mode = match args.get("mode").and_then(Value::as_str) {
            None | Some("yield") => TrapMode::Yield,
            Some("exclusive") => TrapMode::Exclusive,
            Some(m) => return Err(format!("mode {m}: yield or exclusive")),
        };
        let per_hour = args.get("incidents_per_hour").map_or(Ok(0), |v| {
            v.as_u64()
                .filter(|&n| n <= 3600)
                .map(|n| n as u32)
                .ok_or("incidents_per_hour must be 0-3600")
        })?;
        let name = args["name"].as_str().unwrap_or("trap.js");
        let blob = sidp_qjsc::compile_container(source, name, Strip::None)
            .map_err(|e| format!("doesn't compile: {e}"))?;
        let wait = wait_ms(args)?;
        if self.info.is_some() {
            self.with_connection(|c| c.agent_close())?;
            self.info = None;
            notes.push("The agent session was closed to arm the trap.".into());
        }
        let config = TrapConfig {
            name: trap_name.to_owned(),
            mode: mode as i32,
            permissions: Some(self.options.allow.into()),
            connect_under_reset: self.options.connect_under_reset,
            incidents_per_hour: per_hour,
        };
        let status = self.with_connection(|c| c.trap_arm(config, &blob, wait))?;
        let first = self.script_result(status.first_result.clone().unwrap_or_default());
        let mut out = agent::trap_status_json(&status);
        out["first_run"] = first;
        if status.config.is_none() {
            out["note"] = "not armed: the module's top level failed, see first_run".into();
        }
        self.trap_noted = false;
        Ok(out)
    }

    fn list_incidents(&mut self, args: &Value) -> ToolResult {
        let after = args.get("after_id").map_or(Ok(0), |v| {
            v.as_u64()
                .and_then(|i| u32::try_from(i).ok())
                .ok_or("after_id must be a 32-bit number")
        })?;
        let list = self.with_connection(|c| c.incident_list(after))?;
        let items: Vec<Value> = list
            .into_iter()
            .map(|i| {
                let mut v = json!({
                    "id": i.id,
                    "kind": if i.kind == 0 { "watch" } else { "script" },
                    "name": i.name,
                    "trap": i.trap,
                    "boot": i.boot,
                    "uptime_s": i.uptime_us as f64 / 1e6,
                    "size": i.size,
                });
                if i.utc_ms != 0 {
                    v["utc"] = agent::utc_text(i.utc_ms).into();
                }
                v
            })
            .collect();
        Ok(json!({ "incidents": items }))
    }

    fn get_incident(&mut self, args: &Value) -> ToolResult {
        let id = args["incident_id"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .filter(|&i| i != 0)
            .ok_or("incident_id must be a positive number")?;
        let data = self.with_connection(|c| c.incident_read(id)).map_err(|e| {
            if e.contains("status 2") {
                format!("there is no incident {id}")
            } else {
                e
            }
        })?;
        let incident = Incident::parse(&data).map_err(|e| e.to_string())?;
        Ok(incident.to_json(self.options.symbols.as_ref()))
    }

    fn delete_incident(&mut self, args: &Value) -> ToolResult {
        let id = args["incident_id"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .ok_or("incident_id must be a number")?;
        self.with_connection(|c| c.incident_delete(id))
            .map_err(|e| {
                if e.contains("status 2") {
                    format!("there is no incident {id}")
                } else {
                    e
                }
            })?;
        Ok(json!({ "deleted": id }))
    }

    fn close_session(&mut self) -> ToolResult {
        let Some(conn) = &mut self.conn else {
            return Ok(json!({ "closed": false, "note": "no session was open" }));
        };
        let result = if self.info.is_some() {
            conn.agent_close()
        } else {
            Ok(())
        };
        if let Some(mut conn) = self.conn.take() {
            conn.close();
        }
        let was_open = self.info.take().is_some();
        self.lost = None;
        result.map_err(|e| session_error(&e))?;
        Ok(json!({ "closed": was_open }))
    }
}
