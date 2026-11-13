//! Bounded RAM/Flash transfers and conservative stack-cache reads.
use super::*;

/// Flash cache block: one aligned read covers GDB's small reads around the PC.
const FLASH_BLOCK: u64 = 64;
/// At most 16 KiB of flash is kept per stop.
const FLASH_BLOCKS_PER_STOP: usize = 256;

impl<T: Transport> Session<T> {
    fn region(&self, address: u64, len: usize, write: bool) -> Result<Region> {
        let info = self.info.as_ref().ok_or(Error::State("not attached"))?;
        let region = info
            .regions
            .iter()
            .find(|r| r.contains(address, len))
            .ok_or(Error::State("memory range outside target map"))?;
        if region.flags & if write { 2 } else { 1 } == 0 {
            return Err(Error::State("memory access not permitted"));
        }
        if region.kind == 3 {
            return Err(Error::State(
                "MMIO requires explicit access widths; unsupported by this GDB bridge",
            ));
        }
        if write && region.kind != 1 {
            return Err(Error::State(
                "Flash programming is not supported in a debug session",
            ));
        }
        Ok(region.clone())
    }
    /// Read halted RAM/Flash in chunks bounded by the advertised transfer limit.
    /// The current stop's cacheable, nonvolatile RAM snapshot is used, and
    /// small flash reads are cached for the stop when the flash cache is on.
    /// MMIO is rejected because GDB byte reads lack an explicit access width.
    pub fn read_memory(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        let generation = self.generation()?;
        if out.is_empty() {
            return Ok(());
        }
        let region = self.region(address, out.len(), false)?;
        let stop = self.stop.as_ref().unwrap();
        let cached = address
            .checked_sub(stop.stack_address)
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| {
                offset
                    .checked_add(out.len())
                    .and_then(|end| stop.stack.get(offset..end))
            });
        if region.kind == 1
            && region.flags & 0x18 == 8
            && let Some(bytes) = cached
        {
            out.copy_from_slice(bytes);
            return Ok(());
        }
        let limit = self.info.as_ref().unwrap().max_transfer;
        if region.kind == 2
            && self.flash_cache
            && out.len() as u64 <= FLASH_BLOCK
            && limit as u64 >= FLASH_BLOCK
        {
            return self.read_flash_cached(generation, &region, address, out);
        }
        self.read_exact(generation, address, out)
    }
    /// Copies `out` from the aligned blocks that cover it, reading each block
    /// once per stop. Blocks are clipped to the region.
    fn read_flash_cached(
        &mut self,
        generation: u32,
        region: &Region,
        address: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let end = address + out.len() as u64;
        let mut block = address & !(FLASH_BLOCK - 1);
        while block < end {
            let start = block.max(region.start);
            let bytes =
                self.flash_block(generation, start, (block + FLASH_BLOCK).min(region.end))?;
            let from = address.max(start);
            let to = end.min(start + bytes.len() as u64);
            out[(from - address) as usize..(to - address) as usize]
                .copy_from_slice(&bytes[(from - start) as usize..(to - start) as usize]);
            block += FLASH_BLOCK;
        }
        Ok(())
    }
    fn flash_block(&mut self, generation: u32, start: u64, end: u64) -> Result<Vec<u8>> {
        let stop = self.stop.as_ref().unwrap();
        if let Some((_, bytes)) = stop.flash.iter().find(|(at, _)| *at == start) {
            return Ok(bytes.clone());
        }
        let mut bytes = vec![0; (end - start) as usize];
        self.read_exact(generation, start, &mut bytes)?;
        if let Some(stop) = self.stop.as_mut()
            && stop.flash.len() < FLASH_BLOCKS_PER_STOP
        {
            stop.flash.push((start, bytes.clone()));
        }
        Ok(bytes)
    }
    /// Reads target memory as requested, split only at the transfer limit.
    fn read_exact(&mut self, generation: u32, address: u64, out: &mut [u8]) -> Result<()> {
        let limit = self.info.as_ref().unwrap().max_transfer;
        for (index, chunk) in out.chunks_mut(limit).enumerate() {
            let addr = address + (index * limit) as u64;
            let payload = memory_request(generation, addr, chunk.len());
            let response = self.request(0x10, payload)?;
            let mut r = Reader::new(&response);
            let parsed = (|| {
                if r.u64()? != addr || r.u32()? as usize != chunk.len() {
                    return Err(invalid("mismatched memory response"));
                }
                chunk.copy_from_slice(r.take(chunk.len())?);
                r.end()
            })();
            if let Err(e) = parsed {
                return self.wire_error(e);
            }
            if self.generation()? != generation {
                return self.wire_error(invalid("stop changed during memory read"));
            }
        }
        Ok(())
    }
    /// Write halted RAM in bounded chunks. A failed multi-chunk write can have
    /// partial effects; no chunk is automatically retried and all snapshots expire.
    pub fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let generation = self.generation()?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.region(address, bytes.len(), true)?;
        let limit = self.info.as_ref().unwrap().max_transfer;
        self.invalidate_values();
        for (index, chunk) in bytes.chunks(limit).enumerate() {
            let mut payload =
                memory_request(generation, address + (index * limit) as u64, chunk.len());
            payload.extend(chunk);
            self.empty_request(0x12, payload)?;
            if self.generation()? != generation {
                return self.wire_error(invalid("stop changed during memory write"));
            }
        }
        Ok(())
    }
}
fn memory_request(stop: u32, address: u64, len: usize) -> Vec<u8> {
    let mut payload = stop.to_le_bytes().to_vec();
    payload.extend(address.to_le_bytes());
    payload.extend((len as u32).to_le_bytes());
    payload.extend([1, 0, 0, 0]);
    payload
}
