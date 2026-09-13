//! Complete-frame transport boundary. USB uses SLIP; a future WebSocket adapter
//! should deliver one SIDP frame per binary message through the same trait.
use crate::codec::{Frame, SlipDecoder, slip_encode};
use serialport::SerialPort;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    time::{Duration, Instant},
};

/// Ordered SIDP message transport for a single physical connection.
///
/// Calls must respect the supplied timeout. `receive` returns `None` only for
/// an idle timeout and must preserve partial framing across calls. Fatal I/O
/// errors must not be hidden as idle timeouts. No implementation may reconnect
/// or replay frames automatically. A fresh physical connection needs a new session.
pub trait Transport {
    /// Send a complete frame within the given time budget.
    fn send(&mut self, frame: &Frame, timeout: Duration) -> io::Result<()>;
    /// Receive one validated frame, or return `None` when the budget expires.
    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Frame>>;
    /// Close this connection. Must be safe to call more than once.
    fn close(&mut self);
}

/// USB CDC serial connection with bounded SLIP decoding and explicit DTR control.
pub struct UsbCdc {
    port: Option<Box<dyn SerialPort>>,
    decoder: SlipDecoder,
    input: VecDeque<u8>,
    dtr: bool,
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
                    return Frame::decode(&packet).map(Some);
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
}
impl Drop for UsbCdc {
    fn drop(&mut self) {
        self.close();
    }
}
