//! Local GDB RSP frontend backed by a SIDP session.
//!
//! GDB owns ELF/DWARF, expressions and unwinding. This module maps Cortex-M
//! registers, hardware breakpoints and execution control to SIDP. No Flash
//! programming, software patches, signal delivery, RTOS threads or MMIO access
//! are advertised. One invocation serves one local debugger connection.
mod architecture;
mod control;
mod mapping;
mod server;
mod target;

use crate::{
    session::{Error, Session},
    transport::Transport,
};
use architecture::{CortexM, Registers};
use mapping::Layout;
pub use server::{Served, serve};

struct Adapter<T: Transport> {
    session: Session<T>,
    layout: Layout,
}
impl<T: Transport> Adapter<T> {
    fn new(session: Session<T>) -> Result<Self, Error> {
        let info = session
            .info()
            .ok_or(Error::State("GDB requires an attached target"))?;
        let layout = Layout::new(info);
        Ok(Self { session, layout })
    }
}
