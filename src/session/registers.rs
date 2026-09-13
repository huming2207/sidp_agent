//! Register reads and writes against one stop generation.
use super::*;

impl<T: Transport> Session<T> {
    /// Get registers from the stop cache, fetching the full set only if needed.
    pub fn read_registers(&mut self, ids: &[u16]) -> Result<RegisterValues> {
        let generation = self.generation()?;
        if ids
            .iter()
            .all(|id| self.stop.as_ref().unwrap().registers.contains_key(id))
        {
            return Ok(self.stop.as_ref().unwrap().registers.clone());
        }
        let mut payload = generation.to_le_bytes().to_vec();
        payload.extend([0; 4]); // core 0, count 0 = full register set
        let response = self.request(0x13, payload)?;
        let parsed = (|| {
            let mut r = Reader::new(&response);
            if r.u32()? != generation {
                return Err(invalid("stale register response"));
            }
            let count = r.u16()?;
            r.take(2)?;
            let regs = registers(&mut r, count)?;
            r.end()?;
            Ok(regs)
        })();
        let regs = match parsed {
            Ok(r) => r,
            Err(e) => return self.wire_error(e),
        };
        if self.generation()? != generation {
            return self.wire_error(invalid("stop changed during register read"));
        }
        self.stop.as_mut().unwrap().registers.extend(regs);
        Ok(self.stop.as_ref().unwrap().registers.clone())
    }
    /// Write selected SIDP registers. Cached registers/stack are invalidated even
    /// when a target error leaves the write's side effects uncertain.
    pub fn write_registers(&mut self, values: &[(u16, u32)]) -> Result<()> {
        let generation = self.generation()?;
        if values.is_empty() || values.len() > 128 {
            return Err(Error::State("invalid register count"));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut payload = generation.to_le_bytes().to_vec();
        payload.extend(0u16.to_le_bytes());
        payload.extend((values.len() as u16).to_le_bytes());
        for &(id, value) in values {
            if !seen.insert(id) {
                return Err(Error::State("duplicate register write"));
            }
            payload.extend(id.to_le_bytes());
            payload.extend([4, 0]);
            payload.extend(value.to_le_bytes());
        }
        self.invalidate_values();
        self.empty_request(0x14, payload)
    }
}
