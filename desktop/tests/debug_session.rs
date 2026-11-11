//! Debug sessions through the worker against Soul Injector's real C++ SIDP
//! session, with this test acting as GDB.
//!
//! Needs the mock endpoint the CLI's end-to-end runner builds. From this
//! crate's folder:
//!
//! ```sh
//! C=../../soul-injector/components/sidp_client
//! c++ -std=gnu++20 -I$C/include -I$C/test/host_stubs ../tests/e2e/mock_endpoint.cpp \
//!     $C/sidp_session.cpp $C/sidp_transport.cpp -o /tmp/mock-endpoint
//! SIDP_MOCK_ENDPOINT=/tmp/mock-endpoint cargo test -- --include-ignored
//! ```
#![cfg(unix)]

use futures::channel::mpsc::{TryRecvError, UnboundedReceiver, unbounded};
use serialport::{SerialPort, TTYPort};
use sidp_desktop::worker::{Connection, DebugPlan, Event, GdbState, Operation, Outcome, Worker};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(15);

/// The C++ session behind a pseudo-terminal, as the CLI's e2e runner wires it.
struct Endpoint {
    child: Child,
    stop: Arc<AtomicBool>,
    port: String,
    _slave: TTYPort,
}

impl Endpoint {
    fn start(path: &str) -> Self {
        let (master, slave) = TTYPort::pair().expect("pty pair");
        let port = slave.name().expect("pty name");
        let mut child = Command::new(path)
            .args(["3", "fpu"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mock endpoint");
        let stop = Arc::new(AtomicBool::new(false));

        let mut to_endpoint = child.stdin.take().unwrap();
        let mut from_pty = master.try_clone_native().unwrap();
        from_pty.set_timeout(Duration::from_millis(50)).unwrap();
        let running = stop.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while !running.load(Ordering::SeqCst) {
                match from_pty.read(&mut buffer) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                    Ok(n) => {
                        if to_endpoint.write_all(&buffer[..n]).is_err() {
                            return;
                        }
                        let _ = to_endpoint.flush();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let mut from_endpoint = child.stdout.take().unwrap();
        let mut to_pty = master;
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(n) = from_endpoint.read(&mut buffer) {
                if n == 0 || to_pty.write_all(&buffer[..n]).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stop,
            port,
            _slave: slave,
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Collects events and waits for specific ones.
struct Events {
    receiver: UnboundedReceiver<Event>,
    seen: Vec<Event>,
}

impl Events {
    fn wait_for<T>(&mut self, what: &str, mut matches: impl FnMut(&Event) -> Option<T>) -> T {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            match self.receiver.try_recv() {
                Ok(event) => {
                    let found = matches(&event);
                    self.seen.push(event);
                    if let Some(found) = found {
                        return found;
                    }
                }
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("no {what}; events: {:#?}", self.seen);
    }

    fn listening(&mut self) -> SocketAddr {
        self.wait_for("listening", |e| match e {
            Event::Gdb(GdbState::Listening(address)) => Some(*address),
            _ => None,
        })
    }

    fn log_containing(&mut self, text: &str) {
        self.wait_for(text, |e| match e {
            Event::Log(_, line) if line.contains(text) => Some(()),
            _ => None,
        })
    }

    /// Sends a GDB command; on failure shows what the worker reported.
    fn command(&mut self, gdb: &mut Gdb, payload: &str) -> String {
        match gdb.try_command(payload) {
            Ok(reply) => reply,
            Err(error) => {
                std::thread::sleep(Duration::from_millis(500));
                while let Ok(event) = self.receiver.try_recv() {
                    self.seen.push(event);
                }
                panic!(
                    "GDB command {payload:?} failed: {error}; events: {:#?}",
                    self.seen
                )
            }
        }
    }

    fn finished(&mut self) -> Outcome {
        self.wait_for("finish", |e| match e {
            Event::Finished(outcome) => Some(outcome.clone()),
            _ => None,
        })
    }
}

/// The smallest GDB: framed packets with acknowledgements.
struct Gdb(TcpStream);

impl Gdb {
    fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).expect("connect to the debug server");
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        Self(stream)
    }

    fn byte(&mut self) -> std::io::Result<u8> {
        let mut byte = [0];
        self.0.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn try_command(&mut self, payload: &str) -> std::io::Result<String> {
        let sum = payload.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        write!(self.0, "${payload}#{sum:02x}")?;
        // Skip acknowledgements, then read one packet.
        while self.byte()? != b'$' {}
        let mut reply = Vec::new();
        loop {
            match self.byte()? {
                b'#' => break,
                byte => reply.push(byte),
            }
        }
        self.byte()?;
        self.byte()?;
        self.0.write_all(b"+")?;
        Ok(expand_runs(&reply))
    }
}

/// Undoes RSP run-length encoding: `X*n` repeats X another `n - 29` times.
fn expand_runs(packet: &[u8]) -> String {
    let mut text = String::new();
    let mut bytes = packet.iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'*' {
            let count = usize::from(*bytes.next().expect("run length")) - 29;
            let last = text.chars().last().expect("run follows a character");
            text.extend(std::iter::repeat_n(last, count));
        } else {
            text.push(char::from(byte));
        }
    }
    text
}

fn endpoint_path() -> String {
    std::env::var("SIDP_MOCK_ENDPOINT")
        .expect("set SIDP_MOCK_ENDPOINT to the mock endpoint; see this file's header")
}

fn connection(endpoint: &Endpoint) -> Connection {
    Connection {
        port: endpoint.port.clone(),
        timeout: Duration::from_secs(5),
        settle: Duration::ZERO,
        ntp_server: String::new(),
        dtr: false,
    }
}

fn attach_plan(keep_serving: bool) -> Operation {
    Operation::Debug(DebugPlan {
        program: None,
        // Any free port; the Listening event tells which.
        port: 0,
        connect_under_reset: false,
        resume_on_detach: false,
        keep_serving,
    })
}

fn start(endpoint: &Endpoint, operation: Operation) -> (Worker, Events) {
    let (sender, receiver) = unbounded();
    let worker = Worker::start(connection(endpoint), operation, sender);
    let events = Events {
        receiver,
        seen: Vec::new(),
    };
    (worker, events)
}

#[test]
#[ignore = "needs SIDP_MOCK_ENDPOINT"]
fn serves_detaches_reattaches_and_stops() {
    let endpoint = Endpoint::start(&endpoint_path());
    let (worker, mut events) = start(&endpoint, attach_plan(true));

    // First session: attached before GDB connects; a clean detach.
    events.log_containing("Attached: Cortex-M4/M7 with FPU");
    let address = events.listening();
    let mut gdb = Gdb::connect(address);
    let stop = events.command(&mut gdb, "?");
    assert!(stop.starts_with('S') || stop.starts_with('T'), "{stop}");
    let registers = events.command(&mut gdb, "g");
    assert!(registers.len() >= 17 * 8, "{registers}");
    assert!(registers.starts_with("00000000"), "r0 is 0: {registers}");
    assert_eq!(events.command(&mut gdb, "D"), "OK");
    events.log_containing("GDB detached; the target stays halted");

    // Second session: attaches when GDB connects, on a new USB connection.
    let address = events.listening();
    let mut gdb = Gdb::connect(address);
    let stop = events.command(&mut gdb, "?");
    assert!(stop.starts_with('S') || stop.starts_with('T'), "{stop}");
    events.log_containing("Attached");
    events.wait_for("connected", |e| {
        matches!(e, Event::Gdb(GdbState::Connected(_))).then_some(())
    });

    // The user stops the server while GDB is connected; GDB sees the close.
    worker.cancel();
    assert_eq!(
        events.finished(),
        Outcome::Done("Debug server stopped".into())
    );
    let mut rest = Vec::new();
    let _ = gdb.0.read_to_end(&mut rest);
}

#[test]
#[ignore = "needs SIDP_MOCK_ENDPOINT"]
fn notices_gdb_leaving_without_detach() {
    let endpoint = Endpoint::start(&endpoint_path());
    let (worker, mut events) = start(&endpoint, attach_plan(true));
    let address = events.listening();
    let mut gdb = Gdb::connect(address);
    events.command(&mut gdb, "?");
    drop(gdb);
    events.log_containing("GDB disconnected without detaching");
    // Keeps serving; stopping while nobody is connected ends cleanly.
    events.listening();
    worker.cancel();
    assert_eq!(
        events.finished(),
        Outcome::Done("Debug server stopped".into())
    );
}

#[test]
#[ignore = "needs SIDP_MOCK_ENDPOINT"]
fn ends_after_one_session_unless_keep_serving() {
    let endpoint = Endpoint::start(&endpoint_path());
    let (_worker, mut events) = start(&endpoint, attach_plan(false));
    let address = events.listening();
    let mut gdb = Gdb::connect(address);
    events.command(&mut gdb, "?");
    assert_eq!(events.command(&mut gdb, "D"), "OK");
    assert_eq!(
        events.finished(),
        Outcome::Done("Debug session ended".into())
    );
}

#[test]
#[ignore = "needs SIDP_MOCK_ENDPOINT"]
fn stops_cleanly_before_gdb_connects() {
    let endpoint = Endpoint::start(&endpoint_path());
    let (worker, mut events) = start(&endpoint, attach_plan(false));
    let address = events.listening();
    worker.cancel();
    assert_eq!(
        events.finished(),
        Outcome::Done("Debug server stopped".into())
    );
    // The port is free again for the next session.
    TcpListener::bind(address).expect("port released");
}
