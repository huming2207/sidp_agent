//! Single-request SIDP session, stop snapshots and desired hardware breakpoints.
//!
//! A session owns exactly one physical connection. Timeout, corrupt framing,
//! mismatched responses and target loss invalidate all cached state and close
//! that connection; ambiguous operations are never retried. No speculative
//! prefetch is performed in this milestone.
use crate::{
    codec::{self, Frame, Reader, invalid},
    transport::Transport,
};
use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};

/// An operation failed locally, on the wire, or on the target.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Fatal transport or malformed wire data. The session is closed.
    #[error("SIDP I/O: {0}")]
    Io(#[from] io::Error),
    /// Target returned a nonzero SIDP status.
    #[error("SIDP target status {0}")]
    Status(i32),
    /// Operation is unavailable in the current local state.
    #[error("{0}")]
    State(&'static str),
}
/// Session operation result.
pub type Result<T> = std::result::Result<T, Error>;
/// Stop snapshot capability.
pub const CAP_SNAPSHOT: u32 = 1;
/// Hardware breakpoint capability.
pub const CAP_HW_BP: u32 = 1 << 3;
/// Single step capability.
pub const CAP_STEP: u32 = 1 << 5;
/// Optional Cortex-M4 FPU capability.
pub const CAP_FPU: u32 = 1 << 9;

mod control;
mod exchange;
mod memory;
mod registers;
mod types;
use types::registers;
pub use types::{Region, RegisterValues, Stop, TargetInfo};

/// Single-core SIDP session with one outstanding request and no reconnect replay.
pub struct Session<T: Transport> {
    transport: T,
    timeout: Duration,
    next_id: u32,
    closed: bool,
    info: Option<TargetInfo>,
    stop: Option<Stop>,
    last_stop_id: u32,
    breakpoints: BTreeMap<u32, u32>, // address -> stable SIDP breakpoint id
    next_breakpoint: u32,
    requests: u64,
}
impl<T: Transport> Session<T> {
    /// Create an unattached session on a fresh physical connection.
    pub fn new(transport: T, timeout: Duration) -> Self {
        Self {
            transport,
            timeout,
            next_id: 1,
            closed: false,
            info: None,
            stop: None,
            last_stop_id: 0,
            breakpoints: BTreeMap::new(),
            next_breakpoint: 1,
            requests: 0,
        }
    }
    /// Accepted target description, if attached.
    pub fn info(&self) -> Option<&TargetInfo> {
        self.info.as_ref()
    }
    /// Current halted observation; absent while running or disconnected.
    pub fn stop(&self) -> Option<&Stop> {
        self.stop.as_ref()
    }
    /// Number of wire requests sent, for auditing snapshot/cache effectiveness.
    pub fn request_count(&self) -> u64 {
        self.requests
    }
    /// Whether the connection is permanently closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    /// Close and discard all state. Firmware owns halt/cleanup after link loss.
    pub fn close(&mut self) {
        self.closed = true;
        self.info = None;
        self.stop = None;
        self.breakpoints.clear();
        self.transport.close();
    }
    fn generation(&self) -> Result<u32> {
        self.stop
            .as_ref()
            .map(|s| s.id)
            .ok_or(Error::State("target is not halted"))
    }
    fn invalidate_values(&mut self) {
        if let Some(stop) = &mut self.stop {
            stop.registers.clear();
            stop.stack.clear();
        }
    }
}
impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        self.close();
    }
}
