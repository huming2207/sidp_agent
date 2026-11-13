//! Complete-frame transport boundary. USB uses SLIP; WebSocket carries one
//! SIDP frame per binary message.
use crate::codec::{Frame, SlipDecoder, slip_encode};
use serialport::SerialPort;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    time::{Duration, Instant},
};
use tungstenite::Message;

/// Ordered SIDP message transport for a single physical connection.
///
/// Calls must respect the supplied timeout. `receive` returns `None` only for
/// an idle timeout and must preserve partial framing across calls. Fatal I/O
/// errors must not be hidden as idle timeouts. No implementation may reconnect
/// or replay frames automatically. A fresh physical connection needs a new session.
///
/// A frame that fails validation (size, version or CRC) is dropped, as SIDP
/// requires: its request ID cannot be trusted, so a lost response surfaces as
/// that request's timeout, and lost power data as a sequence gap.
pub trait Transport {
    /// Send a complete frame within the given time budget.
    fn send(&mut self, frame: &Frame, timeout: Duration) -> io::Result<()>;
    /// Receive one validated frame, or return `None` when the budget expires.
    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Frame>>;
    /// Close this connection. Must be safe to call more than once.
    fn close(&mut self);
    /// Frames dropped because they failed validation.
    fn dropped_frames(&self) -> u64 {
        0
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send(&mut self, frame: &Frame, timeout: Duration) -> io::Result<()> {
        (**self).send(frame, timeout)
    }
    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Frame>> {
        (**self).receive(timeout)
    }
    fn close(&mut self) {
        (**self).close();
    }
    fn dropped_frames(&self) -> u64 {
        (**self).dropped_frames()
    }
}

/// USB CDC serial connection with bounded SLIP decoding and explicit DTR control.
pub struct UsbCdc {
    port: Option<Box<dyn SerialPort>>,
    decoder: SlipDecoder,
    input: VecDeque<u8>,
    dtr: bool,
    dropped: u64,
}

impl UsbCdc {
    /// Open a CDC port, clear stale driver data and toggle DTR for a new session.
    ///
    /// `settle` gives the device owner task time to accept the connection before
    /// ATTACH. An unanswered ATTACH is never replayed. `control_dtr = false` is
    /// exclusively for PTY tests: real Soul Injector firmware requires DTR.
    /// Baud rate is CDC line coding, not the target's SWD clock.
    pub fn open(path: &str, baud: u32, settle: Duration, control_dtr: bool) -> io::Result<Self> {
        let mut port = serialport::new(path, baud)
            .timeout(Duration::from_millis(20))
            .open()?;
        if control_dtr {
            port.write_data_terminal_ready(false)?;
            std::thread::sleep(Duration::from_millis(100));
        }
        port.clear(serialport::ClearBuffer::All)?;
        if control_dtr {
            port.write_data_terminal_ready(true)?;
        }
        let result = Self {
            port: Some(port),
            decoder: SlipDecoder::default(),
            input: VecDeque::new(),
            dtr: control_dtr,
            dropped: 0,
        };
        std::thread::sleep(settle);
        Ok(result)
    }
}

impl Transport for UsbCdc {
    fn send(&mut self, frame: &Frame, timeout: Duration) -> io::Result<()> {
        let bytes = slip_encode(&frame.encode()?);
        let deadline = Instant::now() + timeout;
        let port = self
            .port
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            let time = deadline.saturating_duration_since(Instant::now());
            if time.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            port.set_timeout(time)?;
            match port.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Do not use tcdrain/flush: some drivers can wait indefinitely there.
        // The SIDP response, not a local FIFO flush, acknowledges an operation.
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(byte) = self.input.pop_front() {
                if let Some(packet) = self.decoder.push(byte) {
                    match Frame::decode(&packet) {
                        Ok(frame) => return Ok(Some(frame)),
                        Err(_) => self.dropped += 1,
                    }
                }
            }
            let time = deadline.saturating_duration_since(Instant::now());
            if time.is_zero() {
                return Ok(None);
            }
            let port = self
                .port
                .as_mut()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
            port.set_timeout(time)?;
            let mut buf = [0; 512];
            match port.read(&mut buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.input.extend(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn close(&mut self) {
        if let Some(mut port) = self.port.take()
            && self.dtr
        {
            let _ = port.write_data_terminal_ready(false);
        }
        self.input.clear();
        self.decoder = SlipDecoder::default();
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped
    }
}
impl Drop for UsbCdc {
    fn drop(&mut self) {
        self.close();
    }
}

/// TCP port a Soul Injector connects to by default: plain `ws://` uses 80.
/// The port is part of the server URL stored on the device.
pub const WEBSOCKET_PORT: u16 = 80;
/// Time allowed for the WebSocket opening handshake and the closing message.
const WEBSOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the device may take to accept a connection, or to clean up the
/// target after one (it retries cleanup for up to 3 s).
const WEBSOCKET_SESSION_TIMEOUT: Duration = Duration::from_secs(10);

/// A Soul Injector connected over WiFi.
///
/// The device connects to this computer, so the agent listens. The device
/// keeps its WebSocket open between SIDP connections; text messages do what
/// DTR does on USB. The agent sends "open" and the device answers "ready", or
/// "busy" while USB is using it. The agent ends with "close" and the device
/// answers "closed" once the target is cleaned up. The device also sends
/// "closed" on its own when USB takes over.
pub struct WebSocket {
    socket: Option<tungstenite::WebSocket<TcpStream>>,
    peer: SocketAddr,
    dropped: u64,
    /// The device answered "ready" and has not sent "closed".
    open: bool,
}

impl WebSocket {
    /// Listen on `address` until a Soul Injector connects, for at most `wait`,
    /// and open a SIDP connection on it.
    pub fn accept(address: SocketAddr, wait: Duration) -> io::Result<Self> {
        let listener = bind(address)?;
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + wait;
        let (stream, peer) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no Soul Injector connected to {address}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        };
        stream.set_nonblocking(false)?;
        // Requests and responses are small; do not let them wait for ACKs.
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(WEBSOCKET_HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(WEBSOCKET_HANDSHAKE_TIMEOUT))?;
        let socket = tungstenite::accept(stream)
            .map_err(|e| io::Error::other(format!("WebSocket handshake with {peer}: {e}")))?;
        Self::open(socket, peer)
    }

    /// Uses the Soul Injector `device_id` through the agent listening at
    /// `address` (`HOST:PORT`) on this computer: `sidp-agent serve` or the
    /// desktop app.
    pub fn connect(address: &str, device_id: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        let peer = stream.peer_addr()?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(WEBSOCKET_HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(WEBSOCKET_HANDSHAKE_TIMEOUT))?;
        let socket = match tungstenite::client(format!("ws://{address}/{device_id}"), stream) {
            Ok((socket, _)) => socket,
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                let (kind, text) = match response.status().as_u16() {
                    404 => (
                        io::ErrorKind::NotFound,
                        format!(
                            "no Soul Injector {device_id} is connected to the agent at {address}"
                        ),
                    ),
                    409 => (
                        io::ErrorKind::ResourceBusy,
                        format!("Soul Injector {device_id} is in use by another program"),
                    ),
                    status => (
                        io::ErrorKind::PermissionDenied,
                        format!("the agent at {address} refused the connection ({status})"),
                    ),
                };
                return Err(io::Error::new(kind, text));
            }
            Err(e) => {
                return Err(io::Error::other(format!(
                    "WebSocket handshake with the agent at {address}: {e}"
                )));
            }
        };
        Self::open(socket, peer)
    }

    /// Starts a SIDP connection: "open", answered by "ready" or "busy".
    fn open(socket: tungstenite::WebSocket<TcpStream>, peer: SocketAddr) -> io::Result<Self> {
        let mut ws = Self {
            socket: Some(socket),
            peer,
            dropped: 0,
            open: false,
        };
        ws.socket()?
            .send(Message::text("open"))
            .map_err(websocket_error)?;
        match ws.wait_for_text(WEBSOCKET_SESSION_TIMEOUT)?.as_str() {
            "ready" => {
                ws.open = true;
                Ok(ws)
            }
            "busy" => Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "the Soul Injector is in use over USB",
            )),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected answer to open: {other:?}"),
            )),
        }
    }

    /// Waits for the device's next text message; frames before it are dropped.
    fn wait_for_text(&mut self, timeout: Duration) -> io::Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let time = deadline.saturating_duration_since(Instant::now());
            if time.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the Soul Injector did not answer",
                ));
            }
            let socket = self.socket()?;
            socket.get_mut().set_read_timeout(socket_timeout(time))?;
            match socket.read() {
                Ok(Message::Text(text)) => return Ok(text.to_string()),
                Ok(Message::Close(_)) => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(websocket_error(e)),
            }
        }
    }

    /// Address the device connected from.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    fn socket(&mut self) -> io::Result<&mut tungstenite::WebSocket<TcpStream>> {
        self.socket
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }
}

/// A socket timeout; zero means no timeout to the OS, so wait at least 1 ms.
fn socket_timeout(time: Duration) -> Option<Duration> {
    Some(time.max(Duration::from_millis(1)))
}

fn websocket_error(error: tungstenite::Error) -> io::Error {
    match error {
        tungstenite::Error::Io(e) => e,
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            io::ErrorKind::ConnectionAborted.into()
        }
        e => io::Error::other(e),
    }
}

impl Transport for WebSocket {
    fn send(&mut self, frame: &Frame, timeout: Duration) -> io::Result<()> {
        let bytes = frame.encode()?;
        let socket = self.socket()?;
        socket
            .get_mut()
            .set_write_timeout(socket_timeout(timeout))?;
        // A send cut short by the timeout leaves part of the frame on the wire,
        // so the error ends the connection.
        socket
            .send(Message::Binary(bytes.into()))
            .map_err(websocket_error)
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            let time = deadline.saturating_duration_since(Instant::now());
            let socket = self.socket()?;
            // A zero timeout only takes what has arrived: the GDB server polls
            // the device that way between GDB's packets.
            socket.get_mut().set_nonblocking(time.is_zero())?;
            socket.get_mut().set_read_timeout(socket_timeout(time))?;
            // tungstenite keeps a partly received message across timeouts and
            // answers pings by itself.
            let read = socket.read();
            if time.is_zero() {
                socket.get_mut().set_nonblocking(false)?;
            }
            match read {
                Ok(Message::Binary(data)) => match Frame::decode(&data) {
                    Ok(frame) => return Ok(Some(frame)),
                    Err(_) => self.dropped += 1,
                },
                Ok(Message::Close(_)) => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(Message::Text(text)) if text.as_str() == "closed" => {
                    self.open = false;
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "the Soul Injector ended the connection; USB may have taken over",
                    ));
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(websocket_error(e)),
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn close(&mut self) {
        // "close" makes the device clean up the target; "closed" says it has.
        // Closing the WebSocket afterwards also works when it never answers.
        if std::mem::take(&mut self.open)
            && let Ok(socket) = self.socket()
        {
            let _ = socket
                .get_mut()
                .set_write_timeout(Some(WEBSOCKET_HANDSHAKE_TIMEOUT));
            if socket.send(Message::text("close")).is_ok() {
                let _ = self.wait_for_text(WEBSOCKET_SESSION_TIMEOUT);
            }
        }
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close(None);
            let _ = socket.flush();
        }
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped
    }
}
impl Drop for WebSocket {
    fn drop(&mut self) {
        self.close();
    }
}

/// Listens on `address`, explaining the fix when the port needs privileges.
pub fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(address).map_err(|e| {
        if e.kind() != io::ErrorKind::PermissionDenied {
            return e;
        }
        io::Error::new(
            e.kind(),
            format!(
                "cannot listen on {address}: {e}. On Linux, ports below 1024 need \
                 `sudo sysctl net.ipv4.ip_unprivileged_port_start={}` (or \
                 `sudo setcap cap_net_bind_service=+ep` on this program), or use another \
                 port in the device's server URL",
                address.port()
            ),
        )
    })
}

/// The server URL a Soul Injector is given by default: this computer's mDNS
/// name, `ws://HOSTNAME.local/`, which follows the computer when its address
/// changes. Port 80, the default for `ws://`.
pub fn default_server_url() -> String {
    let name = gethostname::gethostname();
    let name = name.to_string_lossy();
    let name = name.split('.').next().unwrap_or_default();
    format!("ws://{name}.local/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{LOG, slip_encode};

    fn idle() -> UsbCdc {
        UsbCdc {
            port: None,
            decoder: SlipDecoder::default(),
            input: VecDeque::new(),
            dtr: false,
            dropped: 0,
        }
    }

    #[test]
    fn damaged_frames_are_dropped_and_counted() {
        let frame = |seq: u8| Frame {
            kind: LOG,
            opcode: 0x210,
            id: 0,
            payload: vec![seq; 20],
        };
        let mut damaged = frame(1).encode().unwrap();
        damaged[14] ^= 0xff;
        let mut usb = idle();
        usb.input.extend(slip_encode(&frame(0).encode().unwrap()));
        usb.input.extend(slip_encode(&damaged));
        // The front of a frame lost: the rest arrives without a valid header.
        usb.input
            .extend(slip_encode(&frame(2).encode().unwrap()[9..]));
        usb.input.extend(slip_encode(&frame(3).encode().unwrap()));

        assert_eq!(usb.receive(Duration::ZERO).unwrap(), Some(frame(0)));
        assert_eq!(usb.receive(Duration::ZERO).unwrap(), Some(frame(3)));
        assert_eq!(usb.receive(Duration::ZERO).unwrap(), None);
        assert_eq!(usb.dropped_frames(), 2);
    }

    /// Connects to the agent as a Soul Injector does and answers "open" with `answer`.
    fn fake_device(address: SocketAddr, answer: &'static str) -> tungstenite::WebSocket<TcpStream> {
        let stream = loop {
            if let Ok(stream) = TcpStream::connect(address) {
                break stream;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let (mut socket, _) = tungstenite::client(format!("ws://{address}/"), stream).unwrap();
        match socket.read().unwrap() {
            Message::Text(text) => assert_eq!(text.as_str(), "open"),
            other => panic!("{other:?}"),
        }
        socket.send(Message::text(answer)).unwrap();
        socket
    }

    fn free_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    #[test]
    fn websocket_carries_frames_and_closes() {
        let address = free_address();
        let frame = |seq: u8| Frame {
            kind: LOG,
            opcode: 0x210,
            id: 0,
            payload: vec![seq; 20],
        };
        let device = std::thread::spawn(move || {
            let mut socket = fake_device(address, "ready");
            let mut damaged = frame(1).encode().unwrap();
            damaged[14] ^= 0xff;
            for bytes in [
                frame(0).encode().unwrap(),
                damaged,
                frame(2).encode().unwrap(),
            ] {
                socket.send(Message::Binary(bytes.into())).unwrap();
            }
            match socket.read().unwrap() {
                Message::Binary(data) => assert_eq!(Frame::decode(&data).unwrap(), frame(9)),
                other => panic!("{other:?}"),
            }
            // The agent's "close" is what ends the device's session.
            match socket.read().unwrap() {
                Message::Text(text) => assert_eq!(text.as_str(), "close"),
                other => panic!("{other:?}"),
            }
            socket.send(Message::text("closed")).unwrap();
            loop {
                match socket.read() {
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let mut ws = WebSocket::accept(address, Duration::from_secs(5)).unwrap();
        assert_eq!(ws.receive(Duration::from_secs(2)).unwrap(), Some(frame(0)));
        assert_eq!(ws.receive(Duration::from_secs(2)).unwrap(), Some(frame(2)));
        assert_eq!(ws.dropped_frames(), 1);
        assert_eq!(ws.receive(Duration::from_millis(50)).unwrap(), None);
        ws.send(&frame(9), Duration::from_secs(1)).unwrap();
        ws.close();
        device.join().unwrap();
    }

    #[test]
    fn websocket_reports_a_device_busy_over_usb() {
        let address = free_address();
        let device = std::thread::spawn(move || fake_device(address, "busy"));
        let error = WebSocket::accept(address, Duration::from_secs(5))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
        drop(device.join().unwrap());
    }

    #[test]
    fn websocket_ends_when_the_device_closes_the_connection() {
        let address = free_address();
        let device = std::thread::spawn(move || {
            let mut socket = fake_device(address, "ready");
            socket.send(Message::text("closed")).unwrap();
            socket
        });
        let mut ws = WebSocket::accept(address, Duration::from_secs(5)).unwrap();
        let error = ws.receive(Duration::from_secs(2)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        drop(device.join().unwrap());
    }

    #[test]
    fn websocket_gives_up_when_no_device_connects() {
        let error = WebSocket::accept("127.0.0.1:0".parse().unwrap(), Duration::from_millis(50))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
