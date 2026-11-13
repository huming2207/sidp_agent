//! A bounded-I/O event loop services USB even while GDB is idle. Only request
//! callbacks block, for at most the session timeout; no background workers needed.
use super::*;
use gdbstub::{
    common::Signal,
    conn::Connection,
    stub::{GdbStub, SingleThreadStopReason, state_machine::GdbStubStateMachine as Machine},
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

/// How a GDB session that ended with a detach or kill left the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Served {
    /// The target was resumed. Only a detach with `resume_on_detach` does
    /// that; a kill leaves it halted.
    pub resumed: bool,
    /// SIDP requests sent, for measurement.
    pub requests: u64,
}

/// Serve one GDB client on an already-bound loopback listener.
///
/// The session must already be attached and halted. A clean GDB detach sends
/// SIDP DETACH (keep-halted unless `resume_on_detach`); EOF/errors close the
/// physical link so firmware performs its disconnect cleanup. No second
/// client or reconnect is accepted.
pub fn serve<T: Transport>(
    listener: TcpListener,
    session: Session<T>,
    resume_on_detach: bool,
) -> Result<Served, Box<dyn std::error::Error>> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("GDB listener must be loopback".into());
    }
    listener.set_nonblocking(true)?;
    let mut target = Adapter::new(session)?;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                target.session.poll(Duration::from_millis(20))?
            }
            Err(e) => return Err(e.into()),
        }
    };
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut reader = stream.try_clone()?;
    let buffered = Buffered {
        stream,
        out: Vec::new(),
    };
    let mut machine = GdbStub::new(buffered).run_state_machine(&mut target)?;
    let mut input = VecDeque::new();
    let mut polled = Instant::now();
    loop {
        if matches!(machine, Machine::Idle(_)) {
            // Halted: GDB's packets come first, so wait on its socket (in
            // next_byte) and only look at the device. While the target runs,
            // waiting on the device is what notices the stop promptly.
            target.session.poll(Duration::ZERO)?;
        } else if input.is_empty() || polled.elapsed() >= Duration::from_millis(10) {
            target.session.poll(Duration::from_millis(1))?;
            polled = Instant::now();
        }
        machine = match machine {
            Machine::Idle(state) => match next_byte(&mut reader, &mut input)? {
                Some(byte) => state.incoming_data(&mut target, byte)?,
                None => state.into(),
            },
            Machine::Running(state) => {
                if let Some(stop) = target.session.stop() {
                    let signal = match stop.reason {
                        4 => Signal::SIGINT,
                        6 | 8 => Signal::SIGSEGV,
                        _ => Signal::SIGTRAP,
                    };
                    state.report_stop(&mut target, SingleThreadStopReason::Signal(signal))?
                } else {
                    match next_byte(&mut reader, &mut input)? {
                        Some(byte) => state.incoming_data(&mut target, byte)?,
                        None => state.into(),
                    }
                }
            }
            Machine::CtrlCInterrupt(state) => {
                target.session.halt()?;
                state.interrupt_handled(
                    &mut target,
                    Some(SingleThreadStopReason::Signal(Signal::SIGINT)),
                )?
            }
            Machine::Disconnected(state) => {
                let clean_detach = matches!(
                    state.get_reason(),
                    gdbstub::stub::DisconnectReason::Disconnect
                );
                let resumed = resume_on_detach && clean_detach;
                target.session.detach(resumed)?;
                return Ok(Served {
                    resumed,
                    requests: target.session.request_count(),
                });
            }
        };
    }
}
/// GDB's side of the connection, sent a packet at a time.
///
/// gdbstub writes a reply byte by byte, and with TCP_NODELAY each byte would be
/// its own segment. Bytes wait for the flush gdbstub does after each reply. An
/// acknowledgement is sent at once: after a step or continue it is followed by
/// no flush until the target stops, and GDB waits for it.
struct Buffered {
    stream: TcpStream,
    out: Vec<u8>,
}
impl Connection for Buffered {
    type Error = io::Error;
    fn write(&mut self, byte: u8) -> io::Result<()> {
        if byte == b'+' && self.out.is_empty() {
            return Write::write_all(&mut self.stream, &[byte]);
        }
        self.out.push(byte);
        Ok(())
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.out.extend_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::write_all(&mut self.stream, &self.out)?;
        self.out.clear();
        Write::flush(&mut self.stream)
    }
}
fn next_byte(stream: &mut TcpStream, input: &mut VecDeque<u8>) -> io::Result<Option<u8>> {
    if let Some(byte) = input.pop_front() {
        return Ok(Some(byte));
    }
    let mut buf = [0; 1024];
    match stream.read(&mut buf) {
        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(n) => {
            input.extend(&buf[..n]);
            Ok(input.pop_front())
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
