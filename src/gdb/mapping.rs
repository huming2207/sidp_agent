//! GDB remote register numbers are distinct from SIDP IDs. GDB's double-width
//! D0..D15 are assembled from pairs of SIDP S0..S31, only with CAP_FPU.
use crate::session::{CAP_FPU, TargetInfo};

pub(super) struct Register {
    pub number: usize,
    pub ids: Vec<u16>,
}
pub(super) struct Layout {
    pub registers: Vec<Register>,
    pub xml: String,
}
impl Layout {
    pub fn new(info: &TargetInfo) -> Self {
        let mut result = Self {
            registers: vec![],
            xml: String::from(
                "<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\"><architecture>arm</architecture><feature name=\"org.gnu.gdb.arm.m-profile\">",
            ),
        };
        for n in 0..16 {
            let name = match n {
                13 => "sp".into(),
                14 => "lr".into(),
                15 => "pc".into(),
                _ => format!("r{n}"),
            };
            result.add(
                n,
                &name,
                &[n as u16],
                if n == 15 {
                    "code_ptr"
                } else if n == 13 {
                    "data_ptr"
                } else {
                    "uint32"
                },
            );
        }
        result.add(25, "xpsr", &[16], "uint32");
        result
            .xml
            .push_str("</feature><feature name=\"org.gnu.gdb.arm.m-system\">");
        for (number, name, id) in [
            (26, "msp", 17),
            (27, "psp", 18),
            (28, "primask", 19),
            (29, "basepri", 20),
            (30, "faultmask", 21),
            (31, "control", 22),
        ] {
            if info.profile == 1 && matches!(id, 20 | 21) {
                continue;
            }
            result.add(number, name, &[id], "uint32");
        }
        result.xml.push_str("</feature>");
        if info.capabilities & CAP_FPU != 0 {
            result
                .xml
                .push_str("<feature name=\"org.gnu.gdb.arm.vfp\">");
            for n in 0..16 {
                result.add(
                    32 + n,
                    &format!("d{n}"),
                    &[0x40 + n as u16 * 2, 0x41 + n as u16 * 2],
                    "ieee_double",
                );
            }
            result.add(48, "fpscr", &[0x60], "uint32");
            result.xml.push_str("</feature>");
        }
        result.xml.push_str("</target>");
        result
    }
    fn add(&mut self, number: usize, name: &str, ids: &[u16], kind: &str) {
        self.xml.push_str(&format!(
            "<reg name=\"{name}\" bitsize=\"{}\" regnum=\"{number}\" type=\"{kind}\"/>",
            ids.len() * 32
        ));
        self.registers.push(Register {
            number,
            ids: ids.to_vec(),
        });
    }
    pub fn ids(&self) -> Vec<u16> {
        self.registers
            .iter()
            .flat_map(|r| r.ids.iter().copied())
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn info(profile: u8, capabilities: u32) -> TargetInfo {
        TargetInfo {
            profile,
            capabilities,
            hardware_breakpoints: 0,
            max_transfer: 64,
            regions: vec![],
        }
    }
    #[test]
    fn profiles_and_optional_fpu() {
        let m0 = Layout::new(&info(1, 0));
        assert!(!m0.xml.contains("basepri"));
        assert!(!m0.xml.contains("faultmask"));
        let m4 = Layout::new(&info(3, 0));
        assert!(m4.xml.contains("basepri"));
        assert!(!m4.xml.contains("fpscr"));
        let fpu = Layout::new(&info(3, CAP_FPU));
        let d0 = fpu.registers.iter().find(|r| r.number == 32).unwrap();
        assert_eq!(d0.ids, vec![0x40, 0x41]);
        assert_eq!(fpu.ids().len(), 56);
        assert_eq!(m4.registers[16].number, 25);
        assert_eq!(m4.registers[16].ids, vec![16]);
    }
}
