//! Request matching, asynchronous events and connection acceptance.
use super::*;

impl<T: Transport> Session<T> {
    pub(super) fn wire_error<R>(&mut self, error: io::Error) -> Result<R> {
        self.close();
        Err(Error::Io(error))
    }
    fn receive(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.transport.receive(timeout) {
            Ok(frame) => Ok(frame),
            Err(e) => self.wire_error(e),
        }
    }
    fn event(&mut self, frame: Frame) -> Result<()> {
        match (frame.kind, frame.opcode, frame.id) {
            (codec::EVENT, 0x80, 0) => {
                let stop = match Stop::parse(&frame.payload) {
                    Ok(s) => s,
                    Err(e) => return self.wire_error(e),
                };
                if self.info.is_none() || stop.id <= self.last_stop_id {
                    return self.wire_error(invalid("unexpected or stale STOPPED"));
                }
                self.last_stop_id = stop.id;
                self.stop = Some(stop);
                Ok(())
            }
            (codec::EVENT, 0x81, 0) => {
                self.close();
                Err(Error::State("target lost; reopen and attach"))
            }
            // Logs are discarded here; no unbounded log queue can block control.
            (codec::LOG, 0x31, 0) => Ok(()),
            _ => self.wire_error(invalid("unexpected SIDP event")),
        }
    }
    pub(super) fn request(&mut self, opcode: u16, payload: Vec<u8>) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::State("connection closed; reopen and attach"));
        }
        let id = self.next_id;
        if id == u32::MAX {
            self.close();
            return Err(Error::State("request IDs exhausted; reconnect"));
        }
        self.next_id += 1;
        let deadline = Instant::now() + self.timeout;
        let frame = Frame {
            kind: codec::REQUEST,
            opcode,
            id,
            payload,
        };
        if let Err(e) = self.transport.send(&frame, self.timeout) {
            return self.wire_error(e);
        }
        self.requests += 1;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.wire_error(io::ErrorKind::TimedOut.into());
            }
            let Some(frame) = self.receive(remaining)? else {
                return self.wire_error(io::ErrorKind::TimedOut.into());
            };
            if frame.kind != codec::RESPONSE {
                self.event(frame)?;
                continue;
            }
            if frame.id != id || frame.opcode != opcode || frame.payload.len() < 4 {
                return self.wire_error(invalid("mismatched SIDP response"));
            }
            let status = i32::from_le_bytes(frame.payload[..4].try_into().unwrap());
            if status != 0 {
                if matches!(status, 12 | 13) {
                    self.close();
                }
                return Err(Error::Status(status));
            }
            return Ok(frame.payload[4..].to_vec());
        }
    }
    pub(super) fn empty_request(&mut self, opcode: u16, payload: Vec<u8>) -> Result<()> {
        if !self.request(opcode, payload)?.is_empty() {
            return self.wire_error(invalid("unexpected response payload"));
        }
        Ok(())
    }
    /// Attach without reset, request a halt, and wait for its first STOPPED.
    pub fn attach(&mut self) -> Result<()> {
        if self.info.is_some() {
            return Err(Error::State("already attached"));
        }
        let payload = self.request(1, vec![64, 0, 0, 2, 0, 1, 0, 0])?;
        let info = match TargetInfo::parse(&payload) {
            Ok(info) => info,
            Err(e) => return self.wire_error(e),
        };
        self.info = Some(info);
        self.wait_stopped()
    }
    /// Wait a bounded time for a stop after attach or a synchronous halt.
    pub(super) fn wait_stopped(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        while self.stop.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.wire_error(io::ErrorKind::TimedOut.into());
            }
            self.poll(remaining)?;
        }
        Ok(())
    }
    /// Process at most one unsolicited event. Idle timeout is normal while running.
    pub fn poll(&mut self, timeout: Duration) -> Result<()> {
        if self.closed {
            return Err(Error::State("connection closed"));
        }
        if let Some(frame) = self.receive(timeout)? {
            self.event(frame)?;
        }
        Ok(())
    }
}
