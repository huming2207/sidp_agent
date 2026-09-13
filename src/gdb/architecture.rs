//! GDB architecture primitives. Runtime layout comes from the attach report.
use gdbstub::arch::{Arch, RegId};
use std::num::NonZeroUsize;

#[derive(Debug)]
pub(super) enum CortexM {}
impl Arch for CortexM {
    type Usize = u32;
    type Registers = Registers;
    type BreakpointKind = usize;
    type RegId = RegisterId;
}
#[derive(Debug, Clone, Copy)]
pub(super) struct RegisterId(pub usize);
impl RegId for RegisterId {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        (id <= 48).then_some((Self(id), None))
    }
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Registers(pub Vec<Option<u8>>);
impl gdbstub::arch::Registers for Registers {
    type ProgramCounter = u32;
    fn pc(&self) -> u32 {
        let bytes: Option<Vec<u8>> = self.0.get(60..64).and_then(|s| s.iter().copied().collect());
        bytes
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .unwrap_or(0)
    }
    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for &byte in &self.0 {
            write_byte(byte);
        }
    }
    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        // Exact negotiated length is validated by the adapter before any write.
        self.0 = bytes.iter().copied().map(Some).collect();
        Ok(())
    }
}
