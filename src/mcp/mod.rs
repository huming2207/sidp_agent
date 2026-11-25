//! Model Context Protocol server over stdio (`sidp-agent mcp`): lets an LLM
//! agent drive a Soul Injector agent session.
//!
//! JSON-RPC 2.0, one message per line on stdin and stdout; logs go to stderr.
//! A reader thread takes cancellations as they arrive; one worker owns the
//! device and answers requests in order. See [`tools`] for what the agent can
//! do and `guide.md` / `api.d.ts` for what it reads first.
pub mod tools;

use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
pub use tools::{Connect, Options};

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const RESOURCE_NOT_FOUND: i64 = -32002;

/// How often an idle worker drains the device's notices.
const IDLE_POLL: Duration = Duration::from_millis(200);

const GUIDE: &str = include_str!("guide.md");
const API: &str = include_str!("api.d.ts");

const INSTRUCTIONS: &str = "Drives a Cortex-M target through a Soul Injector debug probe by running \
JavaScript on the probe. Read the resources si://guide and si://api.d.ts before the first script. \
Use eval for quick looks and actions, run_script to install watches and long-lived code, and \
wait_event to see what the target and your scripts report.";

/// The request the worker is on, so a cancellation can reach it.
#[derive(Default)]
pub struct Cancel {
    current: Mutex<Option<Value>>,
    queued: Mutex<Vec<Value>>, // cancelled before the worker got to them
    flag: AtomicBool,
}

impl Cancel {
    /// Whether the client cancelled the request being served.
    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn begin(&self, id: &Value) {
        let mut current = self.current.lock().unwrap();
        *current = Some(id.clone());
        let mut queued = self.queued.lock().unwrap();
        let was = queued
            .iter()
            .position(|q| q == id)
            .map(|i| queued.remove(i));
        self.flag.store(was.is_some(), Ordering::Relaxed);
    }

    fn end(&self) -> bool {
        *self.current.lock().unwrap() = None;
        self.flag.swap(false, Ordering::Relaxed)
    }

    fn cancel(&self, id: &Value) {
        let current = self.current.lock().unwrap();
        if current.as_ref() == Some(id) {
            self.flag.store(true, Ordering::Relaxed);
            return;
        }
        let mut queued = self.queued.lock().unwrap();
        if queued.len() >= 64 {
            queued.remove(0); // ids of requests that were already answered
        }
        queued.push(id.clone());
    }
}

type Output = Arc<Mutex<Box<dyn Write + Send>>>;

fn send(out: &Output, message: &Value) {
    let mut out = out.lock().unwrap();
    // A client that went away ends the reader too; nothing to do here.
    let _ = writeln!(out, "{message}").and_then(|_| out.flush());
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Serves MCP on `input`/`output` until the client closes `input`.
pub fn serve(
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
    connect: Connect,
    options: Options,
) -> io::Result<()> {
    let out: Output = Arc::new(Mutex::new(Box::new(output)));
    let cancel = Arc::new(Cancel::default());
    let (tx, rx) = mpsc::channel::<(Value, Value)>();

    let reader = {
        let out = out.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || read_messages(input, &out, &cancel, &tx))
    };

    let mut device = tools::Device::new(connect, options);
    let mut initialized = false;
    loop {
        let (id, message) = match rx.recv_timeout(IDLE_POLL) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                device.idle();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let method = message["method"].as_str().unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        cancel.begin(&id);
        if cancel.requested() {
            cancel.end();
            continue;
        }
        let reply = match method {
            "initialize" => {
                initialized = true;
                Ok(initialize(&params))
            }
            "ping" => Ok(json!({})),
            _ if !initialized => Err((INVALID_REQUEST, "initialize first".to_owned())),
            "tools/list" => Ok(json!({ "tools": tools::list() })),
            "tools/call" => call_tool(&mut device, &params, &cancel),
            "resources/list" => Ok(resources()),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => read_resource(&params),
            _ => Err((METHOD_NOT_FOUND, format!("no method {method}"))),
        };
        if cancel.end() {
            continue; // cancelled: no reply
        }
        send(
            &out,
            &match reply {
                Ok(r) => result(&id, r),
                Err((code, text)) => error(&id, code, &text),
            },
        );
    }
    device.shutdown();
    reader.join().unwrap_or(Ok(()))
}

fn read_messages(
    input: impl BufRead,
    out: &Output,
    cancel: &Cancel,
    tx: &mpsc::Sender<(Value, Value)>,
) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                send(out, &error(&Value::Null, PARSE_ERROR, &e.to_string()));
                continue;
            }
        };
        if message.is_array() {
            send(
                out,
                &error(&Value::Null, INVALID_REQUEST, "batches are not supported"),
            );
            continue;
        }
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some("notifications/cancelled"), None) => {
                if let Some(id) = message.pointer("/params/requestId") {
                    cancel.cancel(id);
                }
            }
            (Some(_), None) => {} // other notifications need nothing
            (Some(_), Some(id)) if id.is_string() || id.is_number() => {
                if tx.send((id, message)).is_err() {
                    break;
                }
            }
            (Some(_), Some(id)) => send(
                out,
                &error(&id, INVALID_REQUEST, "id must be a string or a number"),
            ),
            (None, Some(_)) => {} // a response; this server sends no requests
            (None, None) => send(
                out,
                &error(&Value::Null, INVALID_REQUEST, "not a JSON-RPC message"),
            ),
        }
    }
    Ok(())
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": false },
        },
        "serverInfo": {
            "name": "sidp-agent",
            "title": "Soul Injector",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn call_tool(
    device: &mut tools::Device,
    params: &Value,
    cancel: &Cancel,
) -> Result<Value, (i64, String)> {
    let Some(name) = params["name"].as_str() else {
        return Err((INVALID_PARAMS, "tools/call needs a name".into()));
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(a) if a.is_object() => a.clone(),
        Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".into())),
    };
    if !tools::exists(name) {
        return Err((INVALID_PARAMS, format!("no tool {name}")));
    }
    let (text, is_error) = match device.call(name, &args, cancel) {
        Ok(v) => (serde_json::to_string_pretty(&v).unwrap(), false),
        Err(e) => (e, true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
}

fn resources() -> Value {
    json!({ "resources": [
        {
            "uri": "si://guide",
            "name": "guide",
            "title": "How to drive the target",
            "description": "Sessions, permissions, watches, events and captures. Read first.",
            "mimeType": "text/markdown",
        },
        {
            "uri": "si://api.d.ts",
            "name": "api.d.ts",
            "title": "Script API",
            "description": "TypeScript declarations of what scripts can call.",
            "mimeType": "text/plain",
        },
    ]})
}

fn read_resource(params: &Value) -> Result<Value, (i64, String)> {
    let uri = params["uri"].as_str().unwrap_or_default();
    let (text, mime) = match uri {
        "si://guide" => (GUIDE, "text/markdown"),
        "si://api.d.ts" => (API, "text/plain"),
        _ => return Err((RESOURCE_NOT_FOUND, format!("no resource {uri}"))),
    };
    Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
}
