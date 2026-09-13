//! Execution and explicit hardware-breakpoint callbacks. Z0 is rejected rather
//! than allowing GDB to silently patch RAM through ordinary memory writes.
use super::*;
use crate::session::{CAP_HW_BP, CAP_STEP};
use gdbstub::{
    common::Signal,
    target::{
        TargetError, TargetResult,
        ext::{
            base::singlethread::{
                SingleThreadResume, SingleThreadSingleStep, SingleThreadSingleStepOps,
            },
            breakpoints::*,
        },
    },
};
impl<T: Transport> SingleThreadResume for Adapter<T> {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Error> {
        if signal.is_some() {
            return Err(Error::State("signal delivery is unsupported"));
        }
        self.session.run(false)
    }
    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        self.session
            .info()
            .filter(|i| i.capabilities & CAP_STEP != 0)?;
        Some(self)
    }
}
impl<T: Transport> SingleThreadSingleStep for Adapter<T> {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Error> {
        if signal.is_some() {
            return Err(Error::State("signal delivery is unsupported"));
        }
        self.session.run(true)
    }
}
impl<T: Transport> Breakpoints for Adapter<T> {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
    fn support_hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
        self.session
            .info()
            .filter(|i| i.capabilities & CAP_HW_BP != 0)?;
        Some(self)
    }
}
impl<T: Transport> SwBreakpoint for Adapter<T> {
    fn add_sw_breakpoint(&mut self, _: u32, _: usize) -> TargetResult<bool, Self> {
        Err(TargetError::NonFatal)
    }
    fn remove_sw_breakpoint(&mut self, _: u32, _: usize) -> TargetResult<bool, Self> {
        Err(TargetError::NonFatal)
    }
}
impl<T: Transport> HwBreakpoint for Adapter<T> {
    fn add_hw_breakpoint(&mut self, address: u32, kind: usize) -> TargetResult<bool, Self> {
        if !matches!(kind, 2..=4) {
            return Err(TargetError::NonFatal);
        }
        self.session
            .add_breakpoint(address)
            .map(|()| true)
            .map_err(|_| TargetError::NonFatal)
    }
    fn remove_hw_breakpoint(&mut self, address: u32, _: usize) -> TargetResult<bool, Self> {
        Ok(self.session.remove_breakpoint(address))
    }
}
