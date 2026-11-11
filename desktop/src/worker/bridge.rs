//! Runs `sidp_agent::gdb::serve` for one GDB connection behind a loopback
//! relay.
//!
//! `serve` blocks until GDB goes away and offers no way to stop it. The relay
//! gives the app that control without changing the library: GDB talks to the
//! app's port, the app forwards every byte to `serve` on a private port, and
//! closing the relay ends `serve` exactly as if GDB had disconnected. The
//! relay adds no buffering beyond the sockets' own, so GDB's Ctrl-C and
//! packet timing pass through unchanged.
use super::{DeviceSession, Reporter};
use crate::describe;
use gdbstub::stub::GdbStubError;
use sidp_agent::{gdb::Served, session};
use std::{
    error::Error,
    io::{self, Read, Write},
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

/// The error type `serve` returns for its GDB state machine.
type StubError = GdbStubError<session::Error, io::Error>;

/// How one GDB connection ended.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// GDB detached or killed; `resumed` tells whether the target runs.
    Detached {
        /// The target was resumed rather than left halted.
        resumed: bool,
    },
    /// GDB closed the connection without detaching.
    GdbClosed,
    /// The user stopped the server.
    Stopped,
    /// The device or target failed.
    Failed(String),
}

/// Serves the attached `session` to the already-connected `client`.
pub fn serve(
    client: TcpStream,
    session: DeviceSession,
    resume_on_detach: bool,
    r: &Reporter,
) -> End {
    match relay(client, session, resume_on_detach, r) {
        Ok(end) => end,
        Err(error) => End::Failed(format!("GDB relay failed: {error}")),
    }
}

fn relay(
    client: TcpStream,
    session: DeviceSession,
    resume_on_detach: bool,
    r: &Reporter,
) -> io::Result<End> {
    let private = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = private.local_addr()?;
    let (done_tx, done) = mpsc::channel();
    let server = std::thread::Builder::new()
        .name("gdb-server".into())
        .spawn(move || {
            let result = sidp_agent::gdb::serve(private, session, resume_on_detach);
            let _ = done_tx.send(classify(result));
        })?;

    let upstream = TcpStream::connect(address)?;
    for stream in [&client, &upstream] {
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    }
    let to_server = pump("gdb-to-server", client.try_clone()?, upstream.try_clone()?)?;
    let to_gdb = pump("server-to-gdb", upstream.try_clone()?, client.try_clone()?)?;

    let mut stopping = false;
    let end = loop {
        if !stopping && r.cancelled() {
            // `serve` sees GDB disconnect, and dropping its session
            // deasserts DTR; the Soul Injector then keeps the target halted.
            stopping = true;
            let _ = client.shutdown(Shutdown::Both);
            let _ = upstream.shutdown(Shutdown::Both);
        }
        match done.recv_timeout(Duration::from_millis(50)) {
            Ok(end) => break end,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break End::Failed("the debug server stopped unexpectedly".into());
            }
        }
    };

    // `serve` has dropped its socket, so this pump ends once the last reply
    // (such as the OK to a detach) has reached GDB.
    let _ = to_gdb.join();
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = to_server.join();
    let _ = server.join();
    Ok(if stopping { End::Stopped } else { end })
}

/// Copies `from` to `to` until either side closes, then passes the close on.
fn pump(name: &str, mut from: TcpStream, mut to: TcpStream) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                match from.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if to.write_all(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = to.shutdown(Shutdown::Write);
        })
}

/// Sorts `serve`'s result into GDB leaving, GDB detaching and real failures.
fn classify(result: Result<Served, Box<dyn Error>>) -> End {
    let error = match result {
        Ok(served) => {
            return End::Detached {
                resumed: served.resumed,
            };
        }
        Err(error) => error,
    };
    // Socket errors from serve's own reads mean GDB went away.
    if let Some(io) = error.downcast_ref::<io::Error>() {
        return if gdb_gone(io) {
            End::GdbClosed
        } else {
            End::Failed(format!("GDB connection error: {io}"))
        };
    }
    if let Some(error) = error.downcast_ref::<session::Error>() {
        return End::Failed(describe::error(error));
    }
    match error.downcast::<StubError>() {
        Ok(stub) if stub.is_connection_error() => End::GdbClosed,
        Ok(stub) => match stub.into_target_error() {
            Some(error) => End::Failed(describe::error(&error)),
            None => End::Failed("GDB sent something the debug server does not support".into()),
        },
        Err(error) => End::Failed(error.to_string()),
    }
}

fn gdb_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_serve_results() {
        let served = |resumed| Served {
            resumed,
            requests: 3,
        };
        assert_eq!(classify(Ok(served(true))), End::Detached { resumed: true });
        assert_eq!(
            classify(Ok(served(false))),
            End::Detached { resumed: false }
        );
        let eof: Box<dyn Error> = Box::new(io::Error::from(io::ErrorKind::UnexpectedEof));
        assert_eq!(classify(Err(eof)), End::GdbClosed);
        let lost: Box<dyn Error> = Box::new(session::Error::Status(12));
        match classify(Err(lost)) {
            End::Failed(message) => assert!(message.contains("stopped answering"), "{message}"),
            other => panic!("{other:?}"),
        }
        let other: Box<dyn Error> = "listener must be loopback".into();
        assert_eq!(
            classify(Err(other)),
            End::Failed("listener must be loopback".into())
        );
    }
}
