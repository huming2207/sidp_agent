//! The agent's listener for Soul Injectors on the LAN.
//!
//! Devices connect in and stay connected; each names itself with an
//! `X-SIDP-Device` header holding its device ID (the factory WiFi MAC, 12 hex
//! digits). Programs on this computer use a device by connecting to
//! `ws://127.0.0.1[:PORT]/DEVICE_ID`, and the listener forwards every message
//! between them unchanged: the session messages, the login and the SIDP frames.
//! It never logs in itself, so each user needs the device's password, and one
//! user at a time may use a device.
//!
//! One thread per connection reads from it. Writes go straight to the other
//! side's socket, one whole WebSocket frame at a time under that socket's lock,
//! so forwarding adds no polling delay.
use crate::transport::bind;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
};

/// Header a Soul Injector names itself with.
pub const DEVICE_HEADER: &str = "X-SIDP-Device";
/// Time allowed for the WebSocket opening handshake, and for one write.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A device pings every 10 s; one silent this long is gone.
const DEVICE_SILENCE: Duration = Duration::from_secs(30);

/// What the listener reports as devices come and go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A device connected, or connected again.
    Connected {
        /// The device's ID.
        id: String,
        /// Where it connected from.
        peer: SocketAddr,
    },
    /// A device disconnected or stopped answering.
    Disconnected {
        /// The device's ID.
        id: String,
    },
}

/// A device connected to the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The device's ID: its factory WiFi MAC, 12 uppercase hex digits.
    pub id: String,
    /// Where it connected from.
    pub peer: SocketAddr,
    /// A user on this computer has it.
    pub in_use: bool,
}

/// Listens for Soul Injectors and lets programs on this computer use them.
/// Runs until the process ends.
pub struct Listener {
    address: SocketAddr,
    state: Arc<State>,
}

type EventSink = Box<dyn Fn(Event) + Send + Sync>;

struct State {
    devices: Mutex<HashMap<String, Slot>>,
    next_id: AtomicU64,
    events: EventSink,
}

/// A connected device, and the user who has it, if any.
struct Slot {
    connection: u64,
    peer: SocketAddr,
    writer: Arc<Writer>,
    user: Option<User>,
}

struct User {
    connection: u64,
    writer: Arc<Writer>,
    /// The device answered "ready" and has not sent "closed".
    session: bool,
}

impl Listener {
    /// Listens on `address`; `events` is called, from the listener's threads,
    /// as devices connect and disconnect.
    pub fn start(
        address: SocketAddr,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = bind(address)?;
        let address = listener.local_addr()?;
        let state = Arc::new(State {
            devices: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events: Box::new(events),
        });
        let shared = state.clone();
        thread::Builder::new()
            .name("sidp-listener".into())
            .spawn(move || accept_loop(listener, shared))?;
        Ok(Self { address, state })
    }

    /// The address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The connected devices, sorted by ID.
    pub fn devices(&self) -> Vec<Device> {
        let devices = self.state.devices.lock().unwrap();
        let mut list: Vec<Device> = devices
            .iter()
            .map(|(id, slot)| Device {
                id: id.clone(),
                peer: slot.peer,
                in_use: slot.user.is_some(),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }
}

/// A device ID: 12 uppercase hex digits.
pub fn is_device_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F'))
}

fn accept_loop(listener: TcpListener, state: Arc<State>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        let _ = thread::Builder::new()
            .name("sidp-connection".into())
            .spawn(move || {
                let _ = serve(&state, stream);
            });
    }
}

/// Who connected, as the opening handshake told.
enum Peer {
    Device(String),
    User(String),
}

// tungstenite's handshake callback returns its large error response.
#[allow(clippy::result_large_err)]
fn serve(state: &State, stream: TcpStream) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let writer = Arc::new(Writer(Mutex::new(stream.try_clone()?)));
    let connection = state.next_id.fetch_add(1, Ordering::Relaxed);
    let duplex = Duplex {
        read: stream,
        writer: writer.clone(),
    };

    let mut who = None;
    let check = |request: &Request, response: Response| {
        let refuse = |status, text: &str| {
            let mut error = ErrorResponse::new(Some(text.to_owned()));
            *error.status_mut() = status;
            Err(error)
        };
        if let Some(id) = request.headers().get(DEVICE_HEADER) {
            let id = id.to_str().unwrap_or_default();
            if !is_device_id(id) {
                return refuse(StatusCode::BAD_REQUEST, "bad device ID");
            }
            who = Some(Peer::Device(id.to_owned()));
            return Ok(response);
        }
        // Users must be on this computer: the listener does not log in, and
        // nothing on the network should reach a device through it.
        if !peer.ip().is_loopback() {
            return refuse(
                StatusCode::FORBIDDEN,
                "only this computer can use devices here",
            );
        }
        let id = request.uri().path().trim_matches('/');
        let mut devices = state.devices.lock().unwrap();
        let Some(slot) = devices.get_mut(id) else {
            return refuse(StatusCode::NOT_FOUND, "no such device connected");
        };
        if slot.user.is_some() {
            return refuse(StatusCode::CONFLICT, "the device is in use");
        }
        slot.user = Some(User {
            connection,
            writer: writer.clone(),
            session: false,
        });
        who = Some(Peer::User(id.to_owned()));
        Ok(response)
    };
    let socket = match tungstenite::accept_hdr(duplex, check) {
        Ok(socket) => socket,
        Err(_) => {
            // A user attached during the handshake, which then failed.
            if let Some(Peer::User(id)) = &who {
                detach(state, id, connection);
            }
            writer.shutdown();
            return Ok(());
        }
    };
    match who {
        Some(Peer::Device(id)) => serve_device(state, socket, id, connection, peer, writer),
        Some(Peer::User(id)) => serve_user(state, socket, id, connection, writer),
        None => Ok(()),
    }
}

fn serve_device(
    state: &State,
    mut socket: tungstenite::WebSocket<Duplex>,
    id: String,
    connection: u64,
    peer: SocketAddr,
    writer: Arc<Writer>,
) -> io::Result<()> {
    socket
        .get_mut()
        .read
        .set_read_timeout(Some(DEVICE_SILENCE))?;
    let replaced = state.devices.lock().unwrap().insert(
        id.clone(),
        Slot {
            connection,
            peer,
            writer: writer.clone(),
            user: None,
        },
    );
    // The same device connecting again: the old connection is dead.
    if let Some(old) = replaced {
        old.writer.shutdown();
        if let Some(user) = old.user {
            user.writer.close();
        }
    }
    (state.events)(Event::Connected {
        id: id.clone(),
        peer,
    });

    while let Ok(message) = socket.read() {
        let (opcode, data) = match &message {
            Message::Text(text) => (OP_TEXT, text.as_bytes()),
            Message::Binary(data) => (OP_BINARY, data.as_ref()),
            Message::Close(_) => break,
            _ => continue,
        };
        let mut devices = state.devices.lock().unwrap();
        let Some(slot) = devices.get_mut(&id).filter(|s| s.connection == connection) else {
            break;
        };
        let Some(user) = slot.user.as_mut() else {
            // The user left before the device accepted: end that session.
            if data == b"ready" {
                let device = slot.writer.clone();
                drop(devices);
                let _ = device.send(OP_TEXT, b"close");
            }
            continue;
        };
        match data {
            b"ready" => user.session = true,
            b"closed" => user.session = false,
            _ => {}
        }
        let user = user.writer.clone();
        drop(devices);
        // A failed write ends that connection; its own thread then cleans up.
        let _ = user.send(opcode, data);
    }

    writer.shutdown();
    let mut devices = state.devices.lock().unwrap();
    if devices.get(&id).is_some_and(|s| s.connection == connection) {
        let user = devices.remove(&id).and_then(|s| s.user);
        drop(devices);
        if let Some(user) = user {
            user.writer.close();
        }
        (state.events)(Event::Disconnected { id });
    }
    Ok(())
}

fn serve_user(
    state: &State,
    mut socket: tungstenite::WebSocket<Duplex>,
    id: String,
    connection: u64,
    writer: Arc<Writer>,
) -> io::Result<()> {
    // A debugger may sit idle for as long as it likes.
    socket.get_mut().read.set_read_timeout(None)?;
    while let Ok(message) = socket.read() {
        let (opcode, data) = match &message {
            Message::Text(text) => (OP_TEXT, text.as_bytes()),
            Message::Binary(data) => (OP_BINARY, data.as_ref()),
            Message::Close(_) => break,
            _ => continue,
        };
        let device = {
            let devices = state.devices.lock().unwrap();
            match devices.get(&id) {
                Some(slot)
                    if slot
                        .user
                        .as_ref()
                        .is_some_and(|u| u.connection == connection) =>
                {
                    slot.writer.clone()
                }
                _ => break, // The device left
            }
        };
        let _ = device.send(opcode, data);
    }
    writer.shutdown();
    detach(state, &id, connection);
    Ok(())
}

/// Lets go of a device; a session the user left open is closed on the device.
fn detach(state: &State, id: &str, connection: u64) {
    let mut devices = state.devices.lock().unwrap();
    let Some(slot) = devices.get_mut(id) else {
        return;
    };
    if slot
        .user
        .as_ref()
        .is_none_or(|u| u.connection != connection)
    {
        return;
    }
    if slot.user.take().is_some_and(|u| u.session) {
        let device = slot.writer.clone();
        drop(devices);
        let _ = device.send(OP_TEXT, b"close");
    }
}

const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;

/// The writing side of one connection.
struct Writer(Mutex<TcpStream>);

impl Writer {
    /// Sends one unmasked (server) WebSocket message in a single frame.
    fn send(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => frame.push(n as u8),
            n if n <= 0xffff => {
                frame.push(126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        self.write_frames(&frame)
    }

    fn write_frames(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stream = self.0.lock().unwrap();
        // A write cut short leaves part of a frame on the wire; end the connection.
        stream.write_all(bytes).inspect_err(|_| {
            let _ = stream.shutdown(Shutdown::Both);
        })
    }

    /// Ends the WebSocket properly, so the other side sees it closed, not reset.
    fn close(&self) {
        let _ = self.send(OP_CLOSE, &[]);
        self.shutdown();
    }

    fn shutdown(&self) {
        let _ = self.0.lock().unwrap().shutdown(Shutdown::Both);
    }
}

/// A connection as tungstenite sees it: reads from the socket, writes through
/// the shared writer. tungstenite writes whole frames (handshake answers,
/// pongs, close frames) in one call, so they never interleave with forwarded ones.
struct Duplex {
    read: TcpStream,
    writer: Arc<Writer>,
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write_frames(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::{Frame, LOG},
        transport::{Transport, WebSocket},
    };
    use std::sync::mpsc;
    use tungstenite::client::IntoClientRequest;

    const ID: &str = "7CDFA1B2C3D4";

    fn start() -> (Listener, mpsc::Receiver<Event>) {
        let (events, received) = mpsc::channel();
        let events = Mutex::new(events);
        let listener = Listener::start("127.0.0.1:0".parse().unwrap(), move |event| {
            let _ = events.lock().unwrap().send(event);
        })
        .unwrap();
        (listener, received)
    }

    /// Connects as a Soul Injector does, naming itself with its device ID.
    fn device(listener: &Listener) -> tungstenite::WebSocket<TcpStream> {
        let address = listener.local_addr();
        let mut request = format!("ws://{address}/").into_client_request().unwrap();
        request
            .headers_mut()
            .insert(DEVICE_HEADER, ID.parse().unwrap());
        let stream = TcpStream::connect(address).unwrap();
        tungstenite::client(request, stream).unwrap().0
    }

    fn expect_text(socket: &mut tungstenite::WebSocket<TcpStream>, expected: &str) {
        loop {
            match socket.read().unwrap() {
                Message::Text(text) => return assert_eq!(text.as_str(), expected),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    fn frame(seq: u8) -> Frame {
        Frame {
            kind: LOG,
            opcode: 0x210,
            id: 0,
            payload: vec![seq; 300],
        }
    }

    fn user(listener: &Listener) -> io::Result<WebSocket> {
        WebSocket::connect(&listener.local_addr().to_string(), ID)
    }

    #[test]
    fn forwards_a_session_between_a_user_and_a_device() {
        let (listener, events) = start();
        let mut device = device(&listener);
        let Event::Connected { id, .. } = events.recv().unwrap() else {
            panic!()
        };
        assert_eq!(id, ID);
        assert_eq!(listener.devices().len(), 1);

        let device = thread::spawn(move || {
            expect_text(&mut device, "open");
            device.send(Message::text("ready")).unwrap();
            match device.read().unwrap() {
                Message::Binary(data) => assert_eq!(Frame::decode(&data).unwrap(), frame(1)),
                other => panic!("{other:?}"),
            }
            let reply = frame(2).encode().unwrap();
            device.send(Message::Binary(reply.into())).unwrap();
            expect_text(&mut device, "close");
            device.send(Message::text("closed")).unwrap();
            device
        });
        let mut ws = user(&listener).unwrap();
        assert!(listener.devices()[0].in_use);
        // One user at a time.
        assert_eq!(
            user(&listener).err().unwrap().kind(),
            io::ErrorKind::ResourceBusy
        );
        ws.send(&frame(1), Duration::from_secs(1)).unwrap();
        assert_eq!(ws.receive(Duration::from_secs(2)).unwrap(), Some(frame(2)));
        ws.close();
        let device = device.join().unwrap();

        // The device is free again; it leaving is reported.
        for _ in 0..200 {
            if !listener.devices()[0].in_use {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!listener.devices()[0].in_use);
        drop(device);
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Disconnected { id: ID.into() }
        );
        assert!(listener.devices().is_empty());
        assert_eq!(
            user(&listener).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn closes_the_session_of_a_user_who_vanishes() {
        let (listener, events) = start();
        let mut device = device(&listener);
        events.recv().unwrap();
        let address = listener.local_addr();
        let stream = TcpStream::connect(address).unwrap();
        let (mut raw, _) = tungstenite::client(format!("ws://{address}/{ID}"), stream).unwrap();
        raw.send(Message::text("open")).unwrap();
        expect_text(&mut device, "open");
        device.send(Message::text("ready")).unwrap();
        expect_text(&mut raw, "ready");
        // Gone without "close", as when the program is killed.
        raw.get_ref().shutdown(Shutdown::Both).unwrap();
        expect_text(&mut device, "close");
    }

    #[test]
    fn a_user_loses_the_device_when_it_disconnects() {
        let (listener, events) = start();
        let mut device = device(&listener);
        events.recv().unwrap();
        let device = thread::spawn(move || {
            expect_text(&mut device, "open");
            device.send(Message::text("ready")).unwrap();
            device
        });
        let mut ws = user(&listener).unwrap();
        drop(device.join().unwrap());
        let error = loop {
            match ws.receive(Duration::from_secs(2)) {
                Ok(None) => continue,
                Ok(Some(frame)) => panic!("{frame:?}"),
                Err(e) => break e,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn device_ids_are_twelve_uppercase_hex_digits() {
        assert!(is_device_id(ID));
        assert!(!is_device_id("7cdfa1b2c3d4"));
        assert!(!is_device_id("7CDFA1B2C3"));
        assert!(!is_device_id("7CDFA1B2C3DG"));
    }
}
