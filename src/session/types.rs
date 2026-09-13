//! Validated attach metadata and stopped-event payloads.
use super::*;

/// One validated target memory region.
#[derive(Clone, Debug)]
pub struct Region {
    /// Start address, inclusive.
    pub start: u64,
    /// End address, exclusive.
    pub end: u64,
    /// SIDP RAM=1, Flash=2, MMIO=3.
    pub kind: u8,
    /// SIDP memory flags (read, write, execute, cacheable, volatile).
    pub flags: u8,
}
impl Region {
    pub(super) fn contains(&self, address: u64, len: usize) -> bool {
        address >= self.start
            && address
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.end)
    }
}

/// Capabilities and memory map accepted during attach.
#[derive(Clone, Debug)]
pub struct TargetInfo {
    /// ARMv6-M=1, ARMv7-M=2, ARMv7E-M=3.
    pub profile: u8,
    /// SIDP capability bits actually advertised by the firmware.
    pub capabilities: u32,
    /// Maximum simultaneously requested hardware breakpoints.
    pub hardware_breakpoints: u16,
    /// Maximum bytes in a single memory request.
    pub max_transfer: usize,
    /// Non-overlapping, bounded target regions.
    pub regions: Vec<Region>,
}
impl TargetInfo {
    pub(super) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(bytes);
        let arch = r.u8()?;
        let profile = r.u8()?;
        let width = r.u8()?;
        r.take(1)?;
        let capabilities = r.u32()?;
        r.take(12)?; // target id, CPU id, vector catch mask
        let hardware_breakpoints = r.u16()?;
        r.take(2)?; // watchpoint count (not exposed in this milestone)
        let count = r.u16()?;
        let max_transfer = usize::from(r.u16()?);
        if arch != 1
            || !(1..=3).contains(&profile)
            || width != 32
            || (capabilities & CAP_FPU != 0 && profile != 3)
            || max_transfer == 0
            || max_transfer > codec::MAX_FRAME - 32
        {
            return Err(invalid("unsupported profile or invalid transfer limit"));
        }
        let mut regions = Vec::new();
        for _ in 0..count {
            let start = r.u64()?;
            let len = r.u64()?;
            let kind = r.u8()?;
            let flags = r.u8()?;
            r.take(2)?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| invalid("memory region overflow"))?;
            if len == 0 || end > 1u64 << 32 || !(1..=3).contains(&kind) {
                return Err(invalid("invalid memory region"));
            }
            regions.push(Region {
                start,
                end,
                kind,
                flags,
            });
        }
        r.end()?;
        regions.sort_by_key(|region| region.start);
        if regions.windows(2).any(|w| w[0].end > w[1].start) {
            return Err(invalid("overlapping memory regions"));
        }
        Ok(Self {
            profile,
            capabilities,
            hardware_breakpoints,
            max_transfer,
            regions,
        })
    }
}

/// One register value; `None` denotes an explicitly unavailable register.
pub type RegisterValues = BTreeMap<u16, Option<u32>>;
pub(super) fn registers(r: &mut Reader<'_>, count: u16) -> io::Result<RegisterValues> {
    let mut values = BTreeMap::new();
    for _ in 0..count {
        let id = r.u16()?;
        let size = r.u8()?;
        let flags = r.u8()?;
        if size != 4 || flags & !1 != 0 {
            return Err(invalid("invalid Cortex-M register value"));
        }
        let raw = r.u32()?;
        if values
            .insert(id, if flags & 1 == 0 { Some(raw) } else { None })
            .is_some()
        {
            return Err(invalid("duplicate register ID"));
        }
    }
    Ok(values)
}

/// Target stop observation, including optional register and stack snapshots.
#[derive(Clone, Debug)]
pub struct Stop {
    /// Monotonically increasing generation within this connection.
    pub id: u32,
    /// SIDP stop reason.
    pub reason: u8,
    /// Register snapshot keyed by SIDP register ID, not GDB register number.
    pub registers: RegisterValues,
    /// Stack snapshot address.
    pub stack_address: u64,
    /// Stack snapshot bytes, usable only for cacheable, nonvolatile RAM.
    pub stack: Vec<u8>,
}
impl Stop {
    pub(super) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(bytes);
        let id = r.u32()?;
        let core = r.u16()?;
        let reason = r.u8()?;
        r.take(1)?;
        r.take(16)?; // detail, breakpoint id, watchpoint address
        let count = r.u16()?;
        let len = usize::from(r.u16()?);
        let stack_address = r.u64()?;
        let registers = registers(&mut r, count)?;
        let stack = r.take(len)?.to_vec();
        r.end()?;
        if id == 0
            || core != 0
            || reason > 9
            || stack_address
                .checked_add(len as u64)
                .is_none_or(|end| end > 1u64 << 32)
        {
            return Err(invalid("invalid stop observation"));
        }
        Ok(Self {
            id,
            reason,
            registers,
            stack_address,
            stack,
        })
    }
}
