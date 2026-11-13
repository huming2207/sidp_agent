//! Execution, hardware-breakpoint and monitor callbacks. Z0 is rejected rather
//! than allowing GDB to silently patch RAM through ordinary memory writes.
use super::*;
use crate::session::{CAP_HW_BP, CAP_STEP, ResetKind};
use gdbstub::{
    common::Signal,
    target::{
        TargetError, TargetResult,
        ext::{
            base::singlethread::{
                SingleThreadResume, SingleThreadSingleStep, SingleThreadSingleStepOps,
            },
            breakpoints::*,
            monitor_cmd::{ConsoleOutput, MonitorCmd, outputln},
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
impl<T: Transport> MonitorCmd for Adapter<T> {
    fn handle_monitor_cmd(&mut self, cmd: &[u8], mut out: ConsoleOutput<'_>) -> Result<(), Error> {
        let kind = match cmd {
            b"reset" | b"reset system" => ResetKind::System,
            b"reset nrst" => ResetKind::Nrst,
            b"flash-cache" | b"flash-cache on" | b"flash-cache off" => {
                if cmd != b"flash-cache" {
                    self.session.set_flash_cache(cmd == b"flash-cache on");
                }
                let state = if self.session.flash_cache() {
                    "on"
                } else {
                    "off"
                };
                outputln!(
                    out,
                    "Flash cache {state}: flash reads are cached until the target runs again."
                );
                return Ok(());
            }
            _ => {
                outputln!(out, "usage: monitor reset [system|nrst]");
                outputln!(out, "       monitor flash-cache [on|off]");
                return Ok(());
            }
        };
        match self.session.reset_halt(kind) {
            // GDB keeps its cached registers across a monitor command, and RSP
            // has no all-stop packet that would make it read them again.
            Ok(()) => outputln!(
                out,
                "Target reset and halted. Run `maint flush register-cache` to see the new registers."
            ),
            Err(e) if self.session.is_closed() || matches!(e, Error::Io(_)) => return Err(e),
            Err(e) => outputln!(out, "Reset failed: {e}"),
        }
        Ok(())
    }
}
