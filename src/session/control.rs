//! Desired hardware breakpoints, run, halt and detach.
use super::*;

impl<T: Transport> Session<T> {
    /// Add a hardware breakpoint locally; the full set is installed by RUN.
    pub fn add_breakpoint(&mut self, address: u32) -> Result<()> {
        let info = self.info.as_ref().ok_or(Error::State("not attached"))?;
        if self.breakpoints.contains_key(&address) {
            return Ok(());
        }
        if info.capabilities & CAP_HW_BP == 0
            || self.breakpoints.len() >= usize::from(info.hardware_breakpoints)
        {
            return Err(Error::Status(22));
        }
        if address & 1 != 0
            || !info
                .regions
                .iter()
                .any(|r| r.contains(u64::from(address), 2) && r.flags & 4 != 0)
        {
            return Err(Error::State("breakpoint address is not executable/aligned"));
        }
        let id = self.next_breakpoint;
        self.next_breakpoint = id
            .checked_add(1)
            .ok_or(Error::State("breakpoint IDs exhausted"))?;
        self.breakpoints.insert(address, id);
        Ok(())
    }
    /// Remove a desired hardware breakpoint; returns whether it was present.
    pub fn remove_breakpoint(&mut self, address: u32) -> bool {
        self.breakpoints.remove(&address).is_some()
    }
    /// Continue or single-step. Success means RUN was accepted, not that it stopped.
    pub fn run(&mut self, step: bool) -> Result<()> {
        let generation = self.generation()?;
        if step && self.info.as_ref().unwrap().capabilities & CAP_STEP == 0 {
            return Err(Error::Status(3));
        }
        let mut payload = generation.to_le_bytes().to_vec();
        payload.extend([0, 0, if step { 2 } else { 1 }, 0]);
        payload.extend([0; 12]); // vector catch and run-to address
        payload.extend((self.breakpoints.len() as u16).to_le_bytes());
        payload.extend([0; 2]);
        for (&address, &id) in &self.breakpoints {
            payload.extend(id.to_le_bytes());
            payload.extend(u64::from(address).to_le_bytes());
            payload.extend([1, 0, 1, 0]); // hardware, instruction size 0, enabled, permanent
        }
        self.empty_request(0x20, payload)?;
        self.stop = None;
        Ok(())
    }
    /// Halt the target and await its confirmed stop, or keep the existing stop.
    pub fn halt(&mut self) -> Result<()> {
        match self.empty_request(0x21, vec![]) {
            Ok(()) => self.wait_stopped(),
            Err(Error::Status(11)) if self.stop.is_some() => Ok(()),
            Err(e) => Err(e),
        }
    }
    /// Detach with keep-halted (default) or explicit resume, then close DTR.
    pub fn detach(&mut self, resume: bool) -> Result<()> {
        let result = self.empty_request(2, vec![if resume { 2 } else { 1 }, 0, 0, 0]);
        self.close();
        result
    }
}
