//! The MCP server against a fake device that answers agent requests.
use prost::Message as _;
use serde_json::{Value, json};
use sidp_agent::{
    agent::{
        self, Allow, SessionEnd,
        proto::{
            ArmRequest, CaptureReadRequest, CaptureReadResponse, EvalRequest, Event, EventNotify,
            EventReadRequest, EventReadResponse, IncidentDeleteRequest, IncidentInfo,
            IncidentListRequest, IncidentListResponse, IncidentReadRequest, IncidentReadResponse,
            OpenRequest, RunRequest, ScriptResult, SessionInfo, TrapStatus, UploadRequest,
        },
    },
    codec::{Frame, LOG, RESPONSE},
    manage::proto::DeviceInfo,
    mcp::{self, Options},
    session::Session,
    transport::Transport,
};
use std::{
    collections::VecDeque,
    io::{self, Cursor, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Device {
    incoming: VecDeque<Frame>,
    opens: Vec<OpenRequest>,
    evals: Vec<Vec<u8>>,
    runs: Vec<RunRequest>,
    upload: Vec<u8>,
    upload_chunks: usize,
    results: VecDeque<ScriptResult>,
    events: Vec<Event>,
    capture: Vec<u8>,
    closes: usize,
    connects: usize,
    fail_event_read: bool,
    trap: Option<ArmRequest>,
    first_result: Option<ScriptResult>,
    incidents: Vec<(u32, Vec<u8>)>,
    status: i32,
}

struct Fake(Arc<Mutex<Device>>);

fn ok(value: &str) -> ScriptResult {
    ScriptResult {
        value: value.into(),
        ..Default::default()
    }
}

impl Device {
    fn answer(&mut self, frame: &Frame) -> io::Result<Vec<u8>> {
        let p = frame.payload.as_slice();
        Ok(match frame.opcode {
            0x0100 => DeviceInfo {
                serial: "SN1".into(),
                js_version: sidp_qjsc::version().into(),
                js_arena_kb: 2048,
                ..Default::default()
            }
            .encode_to_vec(),
            agent::AGENT_OPEN if self.trap.is_some() => {
                self.status = agent::STATUS_TRAP_ARMED;
                Vec::new()
            }
            agent::AGENT_OPEN => {
                self.opens.push(OpenRequest::decode(p).unwrap());
                SessionInfo {
                    js_version: sidp_qjsc::version().into(),
                    cpu_id: 0x410f_c241,
                    profile: 3,
                    newest_event_seq: 3,
                    ..Default::default()
                }
                .encode_to_vec()
            }
            agent::AGENT_CLOSE => {
                self.closes += 1;
                Vec::new()
            }
            agent::SCRIPT_EVAL => {
                let request = EvalRequest::decode(p).unwrap();
                assert!(sidp_qjsc::is_container(&request.bytecode));
                self.evals.push(request.bytecode);
                self.results
                    .pop_front()
                    .unwrap_or_else(|| ok("null"))
                    .encode_to_vec()
            }
            agent::SCRIPT_UPLOAD => {
                let request = UploadRequest::decode(p).unwrap();
                assert_eq!(request.offset as usize, self.upload.len());
                self.upload.extend(request.data);
                self.upload_chunks += 1;
                assert!(self.upload.len() <= request.total as usize);
                Vec::new()
            }
            agent::SCRIPT_RUN => {
                let request = RunRequest::decode(p).unwrap();
                let blob = if request.staged {
                    &self.upload
                } else {
                    &request.bytecode
                };
                assert!(sidp_qjsc::is_container(blob));
                self.runs.push(request);
                ok("").encode_to_vec()
            }
            agent::EVENT_READ => {
                if self.fail_event_read {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let after = EventReadRequest::decode(p).unwrap().after_seq;
                let newest = self.events.last().map_or(0, |e| e.seq);
                EventReadResponse {
                    events: self
                        .events
                        .iter()
                        .filter(|e| e.seq > after)
                        .take(8)
                        .cloned()
                        .collect(),
                    newest_seq: newest,
                    dropped: 0,
                }
                .encode_to_vec()
            }
            agent::CAPTURE_READ => {
                let request = CaptureReadRequest::decode(p).unwrap();
                if request.capture_id != 9 {
                    return Ok(vec![]); // status set by the caller
                }
                let at = request.offset as usize;
                let end = (at + 4096).min(self.capture.len());
                CaptureReadResponse {
                    total: self.capture.len() as u32,
                    data: self.capture[at..end].to_vec(),
                }
                .encode_to_vec()
            }
            agent::TRAP_ARM => {
                let request = ArmRequest::decode(p).unwrap();
                assert!(sidp_qjsc::is_container(&request.bytecode));
                let first = self.first_result.take().unwrap_or_else(|| ok(""));
                let armed = first.outcome == 0 || first.outcome == 2;
                self.trap = armed.then_some(request);
                TrapStatus {
                    first_result: Some(first),
                    ..self.trap_status()
                }
                .encode_to_vec()
            }
            agent::TRAP_STATUS => self.trap_status().encode_to_vec(),
            agent::TRAP_DISARM => {
                self.trap = None;
                Vec::new()
            }
            agent::INCIDENT_LIST => {
                let after = IncidentListRequest::decode(p).unwrap().after_id;
                IncidentListResponse {
                    incidents: self
                        .incidents
                        .iter()
                        .filter(|(id, _)| *id > after)
                        .map(|(id, data)| IncidentInfo {
                            id: *id,
                            size: data.len() as u32,
                            name: "hardfault".into(),
                            trap: "hunt".into(),
                            boot: 7,
                            utc_ms: 1_764_059_400_250,
                            ..Default::default()
                        })
                        .collect(),
                    more: false,
                }
                .encode_to_vec()
            }
            agent::INCIDENT_READ => {
                let request = IncidentReadRequest::decode(p).unwrap();
                let Some((_, data)) = self.incidents.iter().find(|(id, _)| *id == request.id)
                else {
                    self.status = 2;
                    return Ok(Vec::new());
                };
                let at = request.offset as usize;
                let end = (at + 4096).min(data.len());
                IncidentReadResponse {
                    total: data.len() as u32,
                    data: data[at..end].to_vec(),
                }
                .encode_to_vec()
            }
            agent::INCIDENT_DELETE => {
                let id = IncidentDeleteRequest::decode(p).unwrap().id;
                self.incidents.retain(|(i, _)| id != 0 && *i != id);
                Vec::new()
            }
            op => panic!("unexpected opcode 0x{op:04x}"),
        })
    }
}

impl Device {
    fn trap_status(&self) -> TrapStatus {
        TrapStatus {
            config: self.trap.as_ref().and_then(|t| t.config.clone()),
            state: if self.trap.is_some() { 1 } else { 0 },
            incidents: self.incidents.len() as u32,
            ..Default::default()
        }
    }
}

impl Transport for Fake {
    fn send(&mut self, frame: &Frame, _: Duration) -> io::Result<()> {
        let mut d = self.0.lock().unwrap();
        let capture_gone = frame.opcode == agent::CAPTURE_READ
            && CaptureReadRequest::decode(frame.payload.as_slice())
                .unwrap()
                .capture_id
                != 9;
        let body = d.answer(frame)?;
        let status = if capture_gone {
            2
        } else {
            std::mem::take(&mut d.status)
        };
        let mut payload = status.to_le_bytes().to_vec();
        if status == 0 {
            payload.extend(body);
        }
        d.incoming.push_back(Frame {
            kind: RESPONSE,
            opcode: frame.opcode,
            id: frame.id,
            payload,
        });
        Ok(())
    }
    fn receive(&mut self, _: Duration) -> io::Result<Option<Frame>> {
        Ok(self.0.lock().unwrap().incoming.pop_front())
    }
    fn close(&mut self) {}
}

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);
impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn options() -> Options {
    Options {
        allow: Allow {
            write: true,
            ..Default::default()
        },
        session_end: SessionEnd::Resume,
        connect_under_reset: false,
        symbols: None,
    }
}

/// Serves `messages` to the end and returns the replies by id.
fn serve(device: &Arc<Mutex<Device>>, messages: &[Value]) -> Vec<Value> {
    let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
    let out = Out::default();
    let dev = device.clone();
    let connect: mcp::Connect = Box::new(move || {
        dev.lock().unwrap().connects += 1;
        let transport: Box<dyn Transport + Send> = Box::new(Fake(dev.clone()));
        Ok(Session::new(transport, Duration::from_secs(1)))
    });
    mcp::serve(
        Cursor::new(input.into_bytes()),
        out.clone(),
        connect,
        options(),
    )
    .unwrap();
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn init() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
           "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}})
}

fn call(id: u64, tool: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}})
}

fn reply(replies: &[Value], id: u64) -> &Value {
    replies
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no reply {id}: {replies:?}"))
}

fn tool_json(replies: &[Value], id: u64) -> (Value, bool) {
    let r = &reply(replies, id)["result"];
    let text = r["content"][0]["text"].as_str().unwrap();
    (
        serde_json::from_str(text).unwrap_or(Value::String(text.into())),
        r["isError"] == true,
    )
}

#[test]
fn handshake_lists_and_errors() {
    let device = Arc::new(Mutex::new(Device::default()));
    let replies = serve(
        &device,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            init(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
            call(4, "nope", json!({})),
            json!({"jsonrpc": "2.0", "id": 5, "method": "resources/read", "params": {"uri": "si://guide"}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "resources/read", "params": {"uri": "si://x"}}),
            json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 8, "method": "resources/list"}),
        ],
    );
    assert_eq!(reply(&replies, 1)["error"]["code"], -32600);
    let init = &reply(&replies, 0)["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "sidp-agent");
    let tools = reply(&replies, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    for name in [
        "eval",
        "run_script",
        "wait_event",
        "get_capture",
        "registers",
        "close_session",
    ] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    assert_eq!(reply(&replies, 3)["error"]["code"], -32601);
    assert_eq!(reply(&replies, 4)["error"]["code"], -32602);
    let guide = reply(&replies, 5)["result"]["contents"][0]["text"]
        .as_str()
        .unwrap();
    assert!(guide.contains("## Watches"));
    assert_eq!(reply(&replies, 6)["error"]["code"], -32002);
    assert_eq!(reply(&replies, 7)["result"], json!({}));
    assert_eq!(
        reply(&replies, 8)["result"]["resources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(replies.len(), 9); // nothing for the notification
    assert_eq!(device.lock().unwrap().connects, 0); // nothing needed the device
}

#[test]
fn unknown_protocol_version_gets_the_newest_and_garbage_gets_parse_errors() {
    let device = Arc::new(Mutex::new(Device::default()));
    let out = Out::default();
    let connect: mcp::Connect = Box::new(|| Err("no device".into()));
    let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"1999-01-01\"}}\nnot json\n[1]\n";
    mcp::serve(
        Cursor::new(input.as_bytes().to_vec()),
        out.clone(),
        connect,
        options(),
    )
    .unwrap();
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let replies: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        reply(&replies, 1)["result"]["protocolVersion"],
        mcp::PROTOCOL_VERSIONS[0]
    );
    let errors: Vec<i64> = replies
        .iter()
        .filter_map(|r| r["error"]["code"].as_i64())
        .collect();
    assert_eq!(errors, [-32700, -32600]);
    drop(device);
}

#[test]
fn eval_opens_a_session_with_the_permissions_and_parses_the_value() {
    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().results = VecDeque::from([
        ok("{\"a\":[1,2]}"),
        ScriptResult {
            outcome: agent::Outcome::Error as i32,
            error: "TargetError: write32: NOT_ALLOWED".into(),
            stack: "    at <eval> (eval2.js:1)".into(),
            ..Default::default()
        },
        ScriptResult {
            outcome: agent::Outcome::Pending as i32,
            ..Default::default()
        },
    ]);
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "eval", json!({"code": "({a: [1, 2]})"})),
            call(2, "eval", json!({"code": "target.write32(0, 1)"})),
            call(
                3,
                "eval",
                json!({"code": "await target.delay(9999)", "timeout_ms": 100}),
            ),
            call(4, "eval", json!({"code": "import x from 'y'"})),
            call(5, "eval", json!({"code": "1", "timeout_ms": 70000})),
        ],
    );
    let (v, err) = tool_json(&replies, 1);
    assert!(!err);
    assert_eq!(v, json!({"outcome": "ok", "value": {"a": [1, 2]}}));
    let (v, _) = tool_json(&replies, 2);
    assert_eq!(v["outcome"], "error");
    assert!(v["error"].as_str().unwrap().contains("NOT_ALLOWED"));
    let (v, _) = tool_json(&replies, 3);
    assert_eq!(v["outcome"], "pending");
    let (v, err) = tool_json(&replies, 4);
    assert!(
        err && v.as_str().unwrap().contains("doesn't compile"),
        "{v}"
    );
    let (_, err) = tool_json(&replies, 5);
    assert!(err);

    let d = device.lock().unwrap();
    assert_eq!(d.connects, 1);
    assert_eq!(d.opens.len(), 1);
    let open = &d.opens[0];
    assert_eq!(
        open.permissions,
        Some(agent::Permissions {
            write: true,
            flash: false,
            reset: false
        })
    );
    assert_eq!(open.session_end, SessionEnd::Resume as i32);
    assert_eq!(d.evals.len(), 3);
    assert_eq!(d.closes, 1); // on exit
}

#[test]
fn big_modules_are_uploaded_then_run() {
    let device = Arc::new(Mutex::new(Device::default()));
    let mut source = String::from("export const table = [");
    for i in 0..3000 {
        source += &format!("{i},");
    }
    source += "];\n";
    let small = "import { watch } from 'si:target';\nwatch({ on: 'fault' });\n";
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "run_script", json!({"source": source})),
            call(2, "run_script", json!({"source": small, "name": "w.js"})),
        ],
    );
    assert_eq!(tool_json(&replies, 1).0["outcome"], "ok");
    assert_eq!(tool_json(&replies, 2).0["outcome"], "ok");
    let d = device.lock().unwrap();
    assert!(d.upload_chunks >= 2, "{}", d.upload_chunks);
    assert!(d.upload.len() > agent::MAX_INLINE);
    assert!(d.runs[0].staged && d.runs[0].bytecode.is_empty());
    assert!(!d.runs[1].staged && !d.runs[1].bytecode.is_empty());
}

fn event(seq: u32, kind: &str, json: &str, capture_id: u32) -> Event {
    Event {
        seq,
        uptime_us: seq as u64 * 1000,
        kind: kind.into(),
        json: json.into(),
        capture_id,
    }
}

#[test]
fn wait_event_reads_after_the_cursor_and_reports_gaps() {
    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().events = vec![
        event(1, "console", r#"{"level":"log","text":"old"}"#, 0),
        event(4, "console", r#"{"level":"log","text":"new"}"#, 0),
        event(
            5,
            "watch",
            r#"{"watch":1,"on":"fault","pc":"0x08000124"}"#,
            9,
        ),
        event(6, "emit", r#"{"name":"x"}"#, 0),
    ];
    let replies = serve(
        &device,
        &[
            init(),
            call(
                1,
                "wait_event",
                json!({"timeout_ms": 0, "kinds": ["watch", "emit"]}),
            ),
            call(2, "wait_event", json!({"timeout_ms": 0})),
            call(3, "wait_event", json!({"after_seq": 0, "timeout_ms": 0})),
        ],
    );
    let (v, _) = tool_json(&replies, 1);
    let kinds: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["watch", "emit"]); // after seq 3, filtered
    assert_eq!(v["events"][0]["capture_id"], 9);
    assert_eq!(v["events"][0]["data"]["pc"], "0x08000124");
    assert_eq!(v["next_after_seq"], 6);
    assert!(v.get("missed").is_none());
    let (v, _) = tool_json(&replies, 2);
    assert_eq!(v["events"], json!([])); // nothing after the cursor
    let (v, _) = tool_json(&replies, 3);
    assert_eq!(v["events"].as_array().unwrap().len(), 4);
    assert_eq!(v["missed"], 2); // 2 and 3 were overwritten
}

#[test]
fn cancelled_wait_gets_no_reply() {
    let device = Arc::new(Mutex::new(Device::default()));
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "wait_event", json!({"timeout_ms": 50000})),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
        ],
    );
    assert!(replies.iter().all(|r| r["id"] != 1), "{replies:?}");
    assert_eq!(reply(&replies, 2)["result"], json!({}));
}

#[test]
fn capture_is_read_in_pieces_and_decoded() {
    let mut capture = b"SICP".to_vec();
    capture.extend([1, 1, 2, 0]);
    capture.extend(9u32.to_le_bytes());
    capture.extend(0u32.to_le_bytes());
    capture.extend([3, 1, 0, 0]); // a 6000-byte mem range
    capture.extend((8 + 6000u32).to_le_bytes());
    capture.extend(0x2000_0000u32.to_le_bytes());
    capture.extend(6000u32.to_le_bytes());
    capture.extend(vec![0xab; 6000]);
    let len = capture.len() as u32;
    capture[12..16].copy_from_slice(&len.to_le_bytes());

    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().capture = capture;
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "get_capture", json!({"capture_id": 9})),
            call(2, "get_capture", json!({"capture_id": 8})),
        ],
    );
    let (v, err) = tool_json(&replies, 1);
    assert!(!err, "{v}");
    assert_eq!(v["on"], "halt");
    assert_eq!(v["watch"], 2);
    assert_eq!(v["mem"][0]["hex"].as_str().unwrap().len(), 12000);
    let (v, err) = tool_json(&replies, 2);
    assert!(err && v.as_str().unwrap().contains("gone"), "{v}");
}

#[test]
fn a_lost_connection_is_reopened_and_said_so() {
    let device = Arc::new(Mutex::new(Device::default()));
    let dev = device.clone();
    let replies = {
        let input: String = [
            init(),
            call(1, "eval", json!({"code": "1"})),
            call(2, "wait_event", json!({"timeout_ms": 0})),
            call(3, "eval", json!({"code": "1"})),
        ]
        .iter()
        .map(|m| format!("{m}\n"))
        .collect();
        let out = Out::default();
        let connect: mcp::Connect = Box::new(move || {
            let mut d = dev.lock().unwrap();
            d.connects += 1;
            d.fail_event_read = d.connects == 1; // the first connection breaks on EVENT_READ
            let transport: Box<dyn Transport + Send> = Box::new(Fake(dev.clone()));
            Ok(Session::new(transport, Duration::from_secs(1)))
        });
        mcp::serve(
            Cursor::new(input.into_bytes()),
            out.clone(),
            connect,
            options(),
        )
        .unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect::<Vec<_>>()
    };
    let (v, err) = tool_json(&replies, 2);
    assert!(err && v.as_str().unwrap().contains("lost"), "{v}");
    let (v, err) = tool_json(&replies, 3);
    assert!(!err);
    assert!(
        v["notes"][0]
            .as_str()
            .unwrap()
            .contains("previous agent session ended"),
        "{v}"
    );
    let d = device.lock().unwrap();
    assert_eq!((d.connects, d.opens.len()), (2, 2));
}

#[test]
fn agent_notices_are_kept_for_the_next_wait() {
    let device = Arc::new(Mutex::new(Device::default()));
    let mut session = Session::new(Fake(device.clone()), Duration::from_secs(1));
    device.lock().unwrap().incoming.push_back(Frame {
        kind: LOG,
        opcode: agent::AGENT_EVENT,
        id: 0,
        payload: EventNotify { newest_seq: 12 }.encode_to_vec(),
    });
    session.poll(Duration::ZERO).unwrap();
    assert_eq!(session.take_agent_notice(), Some(12));
    assert_eq!(session.take_agent_notice(), None);
    assert!(!session.is_closed());
}

fn incident_bytes(id: u32) -> Vec<u8> {
    let mut b = b"SIIN".to_vec();
    b.extend([1, 0, 2, 0]);
    b.extend(id.to_le_bytes());
    b.extend(0u32.to_le_bytes());
    b.extend(7u32.to_le_bytes());
    b.extend(0u32.to_le_bytes());
    b.extend(5_000_000u64.to_le_bytes());
    b.extend(1_764_059_400_250u64.to_le_bytes());
    let mut name = [0u8; 24];
    name[..9].copy_from_slice(b"hardfault");
    b.extend(name);
    let mut trap = [0u8; 24];
    trap[..4].copy_from_slice(b"hunt");
    b.extend(trap);
    b.extend([0u8; 8]);
    assert_eq!(b.len(), 96);
    for (ty, body) in [
        (
            1u8,
            &br#"{"kind":"watch","on":"fault","cause":"HardFault","pc":"0x08000123"}"#[..],
        ),
        (2, br#"{"history":[1,2,3]}"#),
        (
            4,
            br#"[{"seq":5,"us":1,"kind":"emit","capture":0,"data":{"name":"x"}}]"#,
        ),
        (3, br#"{"action":"resume","handler":"ok"}"#),
    ] {
        b.extend([ty, 0, 0, 0]);
        b.extend((body.len() as u32).to_le_bytes());
        b.extend(body);
        b.resize(b.len().div_ceil(4) * 4, 0);
    }
    let len = b.len() as u32;
    b[12..16].copy_from_slice(&len.to_le_bytes());
    b
}

#[test]
fn a_trap_is_armed_inspected_and_disarmed() {
    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().events = (1..=3)
        .map(|seq| Event {
            seq,
            kind: "emit".into(),
            json: format!(r#"{{"name":"n{seq}"}}"#),
            ..Default::default()
        })
        .collect();
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "eval", json!({"code": "1"})),
            call(
                2,
                "arm_trap",
                json!({"source": "export const a = 1;", "trap_name": "hunt", "incidents_per_hour": 30}),
            ),
            call(3, "eval", json!({"code": "1"})),
            call(4, "wait_event", json!({"timeout_ms": 0})),
            call(5, "trap_status", json!({})),
            call(6, "disarm_trap", json!({})),
            call(7, "trap_status", json!({})),
        ],
    );
    let (v, err) = tool_json(&replies, 2);
    assert!(!err, "{v}");
    assert_eq!(
        (v["armed"].clone(), v["state"].clone(), v["mode"].clone()),
        (json!(true), json!("running"), json!("yield"))
    );
    assert_eq!(v["first_run"]["outcome"], "ok");
    assert!(v["notes"][0].as_str().unwrap().contains("closed"), "{v}");
    let (v, err) = tool_json(&replies, 3);
    assert!(err && v.as_str().unwrap().contains("trap is armed"), "{v}");
    let (v, err) = tool_json(&replies, 4);
    assert!(!err, "{v}");
    assert_eq!(v["events"].as_array().unwrap().len(), 3);
    assert!(
        v["notes"][0]
            .as_str()
            .unwrap()
            .contains("Trap 'hunt' is armed"),
        "{v}"
    );
    let (v, _) = tool_json(&replies, 5);
    assert_eq!(v["name"], "hunt");
    assert_eq!(v["incidents_per_hour"], 30);
    assert_eq!(v["throttle_hold_ms"], "default");
    assert_eq!(v["permissions"]["write"], true);
    assert_eq!(tool_json(&replies, 7).0["armed"], false);
    let d = device.lock().unwrap();
    assert_eq!((d.opens.len(), d.closes), (1, 1));
}

#[test]
fn a_failing_trap_is_not_armed() {
    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().first_result = Some(ScriptResult {
        outcome: 1,
        error: "ReferenceError: x is not defined".into(),
        ..Default::default()
    });
    let replies = serve(
        &device,
        &[
            init(),
            call(
                1,
                "arm_trap",
                json!({"source": "x;", "trap_name": "t", "mode": "exclusive"}),
            ),
        ],
    );
    let (v, err) = tool_json(&replies, 1);
    assert!(!err, "{v}");
    assert_eq!(v["armed"], false);
    assert_eq!(v["first_run"]["outcome"], "error");
    assert!(device.lock().unwrap().trap.is_none());
}

#[test]
fn incidents_are_listed_read_and_deleted() {
    let device = Arc::new(Mutex::new(Device::default()));
    device.lock().unwrap().incidents = vec![(4, incident_bytes(4)), (5, incident_bytes(5))];
    let replies = serve(
        &device,
        &[
            init(),
            call(1, "list_incidents", json!({})),
            call(2, "get_incident", json!({"incident_id": 5})),
            call(3, "get_incident", json!({"incident_id": 6})),
            call(4, "delete_incident", json!({"incident_id": 4})),
            call(5, "list_incidents", json!({})),
        ],
    );
    let (v, _) = tool_json(&replies, 1);
    assert_eq!(v["incidents"].as_array().unwrap().len(), 2);
    assert_eq!(v["incidents"][0]["utc"], "2025-11-25T08:30:00.250Z");
    let (v, err) = tool_json(&replies, 2);
    assert!(!err, "{v}");
    assert_eq!(v["id"], 5);
    assert_eq!(v["trap"], "hunt");
    assert_eq!(v["cause"]["cause"], "HardFault");
    assert_eq!(v["data"]["history"], json!([1, 2, 3]));
    assert_eq!(v["outcome"]["action"], "resume");
    assert_eq!(v["events_before"][0]["data"]["name"], "x");
    let (v, err) = tool_json(&replies, 3);
    assert!(err && v.as_str().unwrap().contains("no incident 6"), "{v}");
    let (v, _) = tool_json(&replies, 5);
    assert_eq!(v["incidents"][0]["id"], 5);
}
