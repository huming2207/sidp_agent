//! RSP data access and dynamic target-description callbacks.
use super::*;
use architecture::RegisterId;
use gdbstub::target::{
    Target, TargetError, TargetResult,
    ext::{
        base::{
            BaseOps,
            single_register_access::{SingleRegisterAccess, SingleRegisterAccessOps},
            singlethread::{SingleThreadBase, SingleThreadResumeOps},
        },
        breakpoints::BreakpointsOps,
        target_description_xml_override::{
            TargetDescriptionXmlOverride, TargetDescriptionXmlOverrideOps,
        },
    },
};

fn map<T: Transport, R>(
    adapter: &Adapter<T>,
    result: Result<R, Error>,
) -> TargetResult<R, Adapter<T>> {
    result.map_err(|e| {
        if adapter.session.is_closed() || matches!(e, Error::Io(_)) {
            TargetError::Fatal(e)
        } else {
            TargetError::NonFatal
        }
    })
}
impl<T: Transport> Target for Adapter<T> {
    type Arch = CortexM;
    type Error = Error;
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }
    fn support_target_description_xml_override(
        &mut self,
    ) -> Option<TargetDescriptionXmlOverrideOps<'_, Self>> {
        Some(self)
    }
    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}
impl<T: Transport> TargetDescriptionXmlOverride for Adapter<T> {
    fn target_description_xml(
        &self,
        annex: &[u8],
        offset: u64,
        length: usize,
        out: &mut [u8],
    ) -> TargetResult<usize, Self> {
        if annex != b"target.xml" {
            return Err(TargetError::NonFatal);
        }
        let Some(bytes) = usize::try_from(offset)
            .ok()
            .and_then(|n| self.layout.xml.as_bytes().get(n..))
        else {
            return Ok(0);
        };
        let n = bytes.len().min(length).min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
}
impl<T: Transport> SingleThreadBase for Adapter<T> {
    fn read_registers(&mut self, regs: &mut Registers) -> TargetResult<(), Self> {
        let ids = self.layout.ids();
        let result = self.session.read_registers(&ids);
        let values = map(self, result)?;
        regs.0.clear();
        for id in ids {
            let bytes = values.get(&id).copied().flatten().map(u32::to_le_bytes);
            regs.0.extend((0..4).map(|n| bytes.map(|b| b[n])));
        }
        Ok(())
    }
    fn write_registers(&mut self, regs: &Registers) -> TargetResult<(), Self> {
        let ids = self.layout.ids();
        if regs.0.len() != ids.len() * 4 {
            return Err(TargetError::NonFatal);
        }
        let mut values = Vec::new();
        for (id, bytes) in ids.into_iter().zip(regs.0.as_chunks::<4>().0) {
            let bytes: Option<Vec<u8>> = bytes.iter().copied().collect();
            let bytes = bytes.ok_or(TargetError::NonFatal)?;
            values.push((id, u32::from_le_bytes(bytes.try_into().unwrap())));
        }
        let result = self.session.write_registers(&values);
        map(self, result)
    }
    fn read_addrs(&mut self, address: u32, out: &mut [u8]) -> TargetResult<usize, Self> {
        let result = self.session.read_memory(u64::from(address), out);
        map(self, result)?;
        Ok(out.len())
    }
    fn write_addrs(&mut self, address: u32, bytes: &[u8]) -> TargetResult<(), Self> {
        let result = self.session.write_memory(u64::from(address), bytes);
        map(self, result)
    }
    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
    fn support_single_register_access(&mut self) -> Option<SingleRegisterAccessOps<'_, (), Self>> {
        Some(self)
    }
}
impl<T: Transport> SingleRegisterAccess<()> for Adapter<T> {
    fn read_register(
        &mut self,
        _: (),
        reg: RegisterId,
        out: &mut [u8],
    ) -> TargetResult<usize, Self> {
        let ids = self
            .layout
            .registers
            .iter()
            .find(|r| r.number == reg.0)
            .ok_or(TargetError::NonFatal)?
            .ids
            .clone();
        let result = self.session.read_registers(&ids);
        let values = map(self, result)?;
        let mut bytes = Vec::new();
        for id in ids {
            let Some(value) = values.get(&id).copied().flatten() else {
                return Ok(0);
            };
            bytes.extend(value.to_le_bytes());
        }
        if out.len() < bytes.len() {
            return Err(TargetError::NonFatal);
        }
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn write_register(&mut self, _: (), reg: RegisterId, bytes: &[u8]) -> TargetResult<(), Self> {
        let ids = &self
            .layout
            .registers
            .iter()
            .find(|r| r.number == reg.0)
            .ok_or(TargetError::NonFatal)?
            .ids;
        if bytes.len() != ids.len() * 4 {
            return Err(TargetError::NonFatal);
        }
        let values: Vec<_> = ids
            .iter()
            .zip(bytes.as_chunks::<4>().0)
            .map(|(&id, b)| (id, u32::from_le_bytes(*b)))
            .collect();
        let result = self.session.write_registers(&values);
        map(self, result)
    }
}
