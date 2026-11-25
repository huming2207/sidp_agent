//! Soul Injector agent sessions: JavaScript on the device that observes and
//! drives a Cortex-M target, for an LLM agent (`sidp-agent mcp`).
//!
//! SIDP opcodes 0x0300 and up, described in sidp_client's
//! `docs/sidp-agent.md`. Requests and replies are the protobuf messages in
//! `proto/agent.proto` (a copy of sidp_client's; keep them identical).
//! Captures are the binary records [`Capture::parse`] reads.
use serde_json::{Value, json};
use std::{collections::BTreeMap, io, path::Path};

/// Generated agent service messages.
#[allow(missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/si.agent.rs"));
}

pub use proto::{
    CaptureReadResponse, Event, EventReadResponse, OpenRequest, Permissions, ScriptResult,
    SessionEnd, SessionInfo, script_result::Outcome,
};

/// AGENT_OPEN opcode.
pub const AGENT_OPEN: u16 = 0x0300;
/// AGENT_CLOSE opcode.
pub const AGENT_CLOSE: u16 = 0x0301;
/// AGENT_INFO opcode.
pub const AGENT_INFO: u16 = 0x0302;
/// SCRIPT_RUN opcode.
pub const SCRIPT_RUN: u16 = 0x0310;
/// SCRIPT_EVAL opcode.
pub const SCRIPT_EVAL: u16 = 0x0311;
/// SCRIPT_STOP opcode.
pub const SCRIPT_STOP: u16 = 0x0312;
/// SCRIPT_UPLOAD opcode.
pub const SCRIPT_UPLOAD: u16 = 0x0313;
/// EVENT_READ opcode.
pub const EVENT_READ: u16 = 0x0320;
/// CAPTURE_READ opcode.
pub const CAPTURE_READ: u16 = 0x0321;
/// AGENT_EVENT log-stream opcode: new events are ready.
pub const AGENT_EVENT: u16 = 0x0380;

/// Status of agent requests other than AGENT_OPEN without a session.
pub const STATUS_NO_SESSION: i32 = 40;
/// Status when the firmware can't run scripts now.
pub const STATUS_NO_SCRIPTS: i32 = 41;
/// Status of AGENT_OPEN when the device's job isn't a Cortex-M one.
pub const STATUS_NEEDS_CORTEX_M: i32 = 42;

/// Largest bytecode container sent inline (nanopb max_size); bigger ones go by SCRIPT_UPLOAD.
pub const MAX_INLINE: usize = 7936;
/// Largest value the device returns from an eval (nanopb max_size, less the NUL).
pub const MAX_VALUE: usize = 4095;

/// What an agent request's failure status means.
pub fn describe_status(status: i32) -> &'static str {
    match status {
        2 => "the device refused the request (bad argument, or a capture that is gone)",
        3 => "this firmware has no agent sessions",
        4 => {
            "the target is busy: GDB is attached, a job is running, or another agent session is open"
        }
        20 => "SWD error: the target didn't answer (powered? wired? asleep?)",
        STATUS_NO_SESSION => "no agent session is open",
        STATUS_NO_SCRIPTS => {
            "the device can't run scripts now (no PSRAM, or they failed; see the device log)"
        }
        STATUS_NEEDS_CORTEX_M => {
            "the device needs a Cortex-M job to know the target: push one with `sidp-agent job push`"
        }
        _ => crate::manage::describe_status(status),
    }
}

/// What the session's scripts may do besides observe, halt and resume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    /// Memory and register writes, `target.call()`.
    pub write: bool,
    /// `flash.erase()` and `flash.write()`.
    pub flash: bool,
    /// `target.reset()`, `reinit()`, and 'reset' reflexes and handler results.
    pub reset: bool,
}

impl From<Allow> for Permissions {
    fn from(a: Allow) -> Self {
        Permissions {
            write: a.write,
            flash: a.flash,
            reset: a.reset,
        }
    }
}

/// Name of a watch kind in captures and events.
pub fn watch_kind(on: u8) -> &'static str {
    match on {
        0 => "fault",
        1 => "halt",
        2 => "lockup",
        3 => "reset",
        _ => "unknown",
    }
}

const SEC_REGS: u8 = 1;
const SEC_FAULT: u8 = 2;
const SEC_MEM: u8 = 3;
const SEC_STOP: u8 = 4;
const REG_SPECIAL: u8 = 20;

/// One memory range read into a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemRange {
    /// The stack (from the frame's SP), or a `capture.mem` range.
    pub stack: bool,
    /// First address read.
    pub addr: u32,
    /// Bytes asked for; `data` is shorter when a read failed part way.
    pub requested: u32,
    /// What was read.
    pub data: Vec<u8>,
}

/// Fault status registers; a field is `None` when its read failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaultRegs {
    /// SHCSR.
    pub shcsr: Option<u32>,
    /// CFSR (MMFSR, BFSR, UFSR).
    pub cfsr: Option<u32>,
    /// HFSR.
    pub hfsr: Option<u32>,
    /// DFSR.
    pub dfsr: Option<u32>,
    /// MMFAR.
    pub mmfar: Option<u32>,
    /// BFAR.
    pub bfar: Option<u32>,
    /// AFSR.
    pub afsr: Option<u32>,
}

/// A watch's snapshot of the target, as CAPTURE_READ returns it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// Capture ID, as in its "watch" event.
    pub id: u32,
    /// Watch kind (0 fault, 1 halt, 2 lockup, 3 reset).
    pub on: u8,
    /// The watch's ID.
    pub watch: u16,
    /// Core registers by DCRSR REGSEL (r0-r15 0-15, xPSR 16, MSP 17, PSP 18,
    /// CONTROL/FAULTMASK/BASEPRI/PRIMASK 20); `None` when the read failed.
    pub regs: BTreeMap<u8, Option<u32>>,
    /// Fault registers, when the watch captured them on an ARMv7-M or later core.
    pub fault: Option<FaultRegs>,
    /// Stack and memory ranges.
    pub mem: Vec<MemRange>,
    /// DFSR when the core stopped.
    pub dfsr: u32,
    /// The core was halted when the capture was taken (registers are only read then).
    pub halted: bool,
    /// Device uptime of the capture.
    pub uptime_us: u64,
    /// Why the watch fired, e.g. "HardFault" or "breakpoint".
    pub cause: String,
}

fn u32_at(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| crate::codec::invalid("capture: truncated"))
}

impl Capture {
    /// Reads a capture record (sidp_client's docs/sidp-agent.md, "Captures").
    pub fn parse(b: &[u8]) -> io::Result<Self> {
        let bad = crate::codec::invalid;
        if b.len() < 16 || &b[..4] != b"SICP" || b[4] != 1 {
            return Err(bad("capture: not a version 1 capture"));
        }
        let total = u32_at(b, 12)? as usize;
        if total != b.len() {
            return Err(bad("capture: length mismatch"));
        }
        let mut c = Capture {
            on: b[5],
            watch: u16::from_le_bytes([b[6], b[7]]),
            id: u32_at(b, 8)?,
            ..Default::default()
        };
        let mut at = 16;
        while at < b.len() {
            let ty = *b.get(at).ok_or_else(|| bad("capture: truncated"))?;
            let kind = *b.get(at + 1).ok_or_else(|| bad("capture: truncated"))?;
            let len = u32_at(b, at + 4)? as usize;
            let start = at + 8;
            let body = b
                .get(
                    start
                        ..start
                            .checked_add(len)
                            .ok_or_else(|| bad("capture: bad length"))?,
                )
                .ok_or_else(|| bad("capture: section past the end"))?;
            match ty {
                SEC_REGS => {
                    for e in body.chunks_exact(8) {
                        c.regs
                            .insert(e[0], (e[1] != 0).then(|| u32_at(e, 4).unwrap()));
                    }
                }
                SEC_FAULT if body.len() >= 32 => {
                    let valid = u32_at(body, 0)?;
                    let get = |i: usize| -> Option<u32> {
                        (valid & (1 << i) != 0).then(|| u32_at(body, 4 + 4 * i).unwrap())
                    };
                    c.fault = Some(FaultRegs {
                        shcsr: get(0),
                        cfsr: get(1),
                        hfsr: get(2),
                        dfsr: get(3),
                        mmfar: get(4),
                        bfar: get(5),
                        afsr: get(6),
                    });
                }
                SEC_MEM if body.len() >= 8 => c.mem.push(MemRange {
                    stack: kind == 0,
                    addr: u32_at(body, 0)?,
                    requested: u32_at(body, 4)?,
                    data: body[8..].to_vec(),
                }),
                SEC_STOP if body.len() >= 32 => {
                    c.dfsr = u32_at(body, 0)?;
                    c.halted = u32_at(body, 4)? != 0;
                    c.uptime_us = u64::from_le_bytes(body[8..16].try_into().unwrap());
                    let cause = &body[16..32];
                    let end = cause.iter().position(|&x| x == 0).unwrap_or(cause.len());
                    c.cause = String::from_utf8_lossy(&cause[..end]).into_owned();
                }
                _ => {} // newer sections are skipped
            }
            at = start + len.div_ceil(4) * 4;
        }
        Ok(c)
    }

    /// The register `id` (DCRSR REGSEL), if it was read.
    pub fn reg(&self, id: u8) -> Option<u32> {
        self.regs.get(&id).copied().flatten()
    }

    /// The 8-word exception frame the core stacked on entry to the handler it
    /// stopped in, when it stopped in one: r0-r3, r12, lr, pc, xpsr of the
    /// code that faulted.
    pub fn exception_frame(&self) -> Option<[u32; 8]> {
        let ipsr = self.reg(16)? & 0x1ff;
        let lr = self.reg(14)?;
        if ipsr == 0 || lr & 0xFFFF_FF00 != 0xFFFF_FF00 {
            return None;
        }
        let stack = self.mem.iter().find(|m| m.stack)?;
        if stack.data.len() < 32 {
            return None;
        }
        let mut frame = [0u32; 8];
        for (i, w) in frame.iter_mut().enumerate() {
            *w = u32_at(&stack.data, i * 4).ok()?;
        }
        Some(frame)
    }

    /// The capture as JSON for an agent; `symbols` names code addresses.
    pub fn to_json(&self, symbols: Option<&Symbols>) -> Value {
        const NAMES: [&str; 19] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc", "xpsr", "msp", "psp",
        ];
        let mut regs = serde_json::Map::new();
        for (&id, value) in &self.regs {
            let Some(v) = value else { continue };
            if let Some(name) = NAMES.get(id as usize) {
                regs.insert((*name).into(), hex(*v).into());
            } else if id == REG_SPECIAL {
                regs.insert("primask".into(), (v & 0xff).into());
                regs.insert("basepri".into(), ((v >> 8) & 0xff).into());
                regs.insert("faultmask".into(), ((v >> 16) & 0xff).into());
                regs.insert("control".into(), (v >> 24).into());
            }
        }
        let mut out = json!({
            "capture_id": self.id,
            "watch": self.watch,
            "on": watch_kind(self.on),
            "cause": self.cause,
            "halted": self.halted,
            "uptime_us": self.uptime_us,
            "dfsr": hex(self.dfsr),
            "regs": regs,
        });
        if let Some(syms) = symbols {
            for (name, id) in [("pc_symbol", 15), ("lr_symbol", 14)] {
                if let Some(s) = self.reg(id).and_then(|a| syms.describe(a)) {
                    out[name] = s.into();
                }
            }
        }
        if let Some(ipsr) = self.reg(16).map(|x| x & 0x1ff).filter(|&i| i != 0) {
            out["exception"] = exception_name(ipsr).into();
        }
        if let Some(f) = self.exception_frame() {
            let mut frame = json!({
                "r0": hex(f[0]), "r1": hex(f[1]), "r2": hex(f[2]), "r3": hex(f[3]),
                "r12": hex(f[4]), "lr": hex(f[5]), "pc": hex(f[6]), "xpsr": hex(f[7]),
            });
            if let Some(syms) = symbols {
                if let Some(s) = syms.describe(f[6]) {
                    frame["pc_symbol"] = s.into();
                }
                if let Some(s) = syms.describe(f[5]) {
                    frame["lr_symbol"] = s.into();
                }
            }
            out["stacked"] = frame;
        }
        if let Some(fault) = &self.fault {
            out["fault"] = fault_json(fault);
        }
        let mem: Vec<Value> = self
            .mem
            .iter()
            .map(|m| {
                let mut v = json!({
                    "addr": hex(m.addr),
                    "requested": m.requested,
                    "hex": to_hex(&m.data),
                });
                if m.stack {
                    v["stack"] = true.into();
                }
                if m.data.len() < m.requested as usize {
                    v["note"] = format!("only {} bytes could be read", m.data.len()).into();
                }
                v
            })
            .collect();
        out["mem"] = mem.into();
        out
    }
}

/// `0x` and 8 hex digits.
pub fn hex(v: u32) -> String {
    format!("0x{v:08x}")
}

/// Lowercase hex digits, two per byte.
pub fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Name of a Cortex-M exception number.
pub fn exception_name(ipsr: u32) -> String {
    match ipsr {
        1 => "Reset".into(),
        2 => "NMI".into(),
        3 => "HardFault".into(),
        4 => "MemManage".into(),
        5 => "BusFault".into(),
        6 => "UsageFault".into(),
        7 => "SecureFault".into(),
        11 => "SVCall".into(),
        12 => "DebugMonitor".into(),
        14 => "PendSV".into(),
        15 => "SysTick".into(),
        n if n >= 16 => format!("IRQ{}", n - 16),
        n => format!("exception {n}"),
    }
}

const CFSR_BITS: &[(u32, &str, &str)] = &[
    (
        0,
        "IACCVIOL",
        "MPU: instruction fetch from a no-execute region",
    ),
    (
        1,
        "DACCVIOL",
        "MPU: data access violation (MMFAR has the address)",
    ),
    (3, "MUNSTKERR", "MPU fault unstacking on exception return"),
    (4, "MSTKERR", "MPU fault stacking on exception entry"),
    (5, "MLSPERR", "MPU fault saving FP state lazily"),
    (7, "MMARVALID", "MMFAR holds the faulting address"),
    (8, "IBUSERR", "bus error on instruction fetch"),
    (
        9,
        "PRECISERR",
        "precise data bus error (BFAR has the address)",
    ),
    (
        10,
        "IMPRECISERR",
        "imprecise data bus error: the stacked PC is after the access",
    ),
    (11, "UNSTKERR", "bus error unstacking on exception return"),
    (
        12,
        "STKERR",
        "bus error stacking on exception entry (stack overflow?)",
    ),
    (13, "LSPERR", "bus error saving FP state lazily"),
    (15, "BFARVALID", "BFAR holds the faulting address"),
    (16, "UNDEFINSTR", "undefined instruction"),
    (
        17,
        "INVSTATE",
        "invalid state: a branch to an even (ARM-mode) address?",
    ),
    (18, "INVPC", "invalid EXC_RETURN on exception return"),
    (19, "NOCP", "coprocessor (FPU) used while disabled"),
    (20, "STKOF", "stack limit overflow (ARMv8-M)"),
    (24, "UNALIGNED", "unaligned access with UNALIGN_TRP set"),
    (25, "DIVBYZERO", "divide by zero with DIV_0_TRP set"),
];

const HFSR_BITS: &[(u32, &str, &str)] = &[
    (1, "VECTTBL", "bus fault reading the vector table"),
    (
        30,
        "FORCED",
        "a configurable fault escalated to HardFault; see CFSR",
    ),
    (
        31,
        "DEBUGEVT",
        "debug event (BKPT with no debugger attached?)",
    ),
];

fn decode_bits(v: u32, table: &[(u32, &str, &str)]) -> Vec<Value> {
    table
        .iter()
        .filter(|(bit, _, _)| v & (1 << bit) != 0)
        .map(|(_, name, text)| json!({ "bit": name, "meaning": text }))
        .collect()
}

/// Fault registers as JSON, with the set CFSR and HFSR bits explained.
pub fn fault_json(f: &FaultRegs) -> Value {
    let mut out = serde_json::Map::new();
    for (name, v) in [
        ("shcsr", f.shcsr),
        ("cfsr", f.cfsr),
        ("hfsr", f.hfsr),
        ("dfsr", f.dfsr),
        ("afsr", f.afsr),
    ] {
        if let Some(v) = v {
            out.insert(name.into(), hex(v).into());
        }
    }
    if let Some(cfsr) = f.cfsr {
        out.insert("cfsr_bits".into(), decode_bits(cfsr, CFSR_BITS).into());
        if cfsr & (1 << 7) != 0
            && let Some(a) = f.mmfar
        {
            out.insert("mmfar".into(), hex(a).into());
        }
        if cfsr & (1 << 15) != 0
            && let Some(a) = f.bfar
        {
            out.insert("bfar".into(), hex(a).into());
        }
    }
    if let Some(hfsr) = f.hfsr {
        out.insert("hfsr_bits".into(), decode_bits(hfsr, HFSR_BITS).into());
    }
    out.into()
}

/// Function symbols of the target's firmware, for naming code addresses.
#[derive(Debug, Default)]
pub struct Symbols {
    funcs: Vec<(u32, u32, String)>, // start, size, name; sorted by start
}

impl Symbols {
    /// Reads the function symbols of an ELF file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let data = std::fs::read(path)?;
        Self::parse(&data).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
    }

    /// Reads the function symbols of ELF data.
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        use object::{Object, ObjectSymbol, SymbolKind};
        let file = object::File::parse(data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut funcs: Vec<(u32, u32, String)> = file
            .symbols()
            .filter(|s| s.kind() == SymbolKind::Text && s.is_definition())
            .filter_map(|s| {
                let name = s.name().ok()?;
                let start = u32::try_from(s.address()).ok()? & !1; // Thumb bit
                (!name.is_empty() && !name.starts_with('$'))
                    .then(|| (start, u32::try_from(s.size()).unwrap_or(0), name.to_owned()))
            })
            .collect();
        funcs.sort();
        funcs.dedup_by_key(|f| f.0);
        if funcs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no function symbols (stripped?)",
            ));
        }
        Ok(Self { funcs })
    }

    /// `name+0x12` for an address inside a function, `None` outside them all.
    pub fn describe(&self, addr: u32) -> Option<String> {
        let addr = addr & !1;
        let i = self.funcs.partition_point(|f| f.0 <= addr).checked_sub(1)?;
        let (start, size, name) = &self.funcs[i];
        let offset = addr - start;
        if *size != 0 && offset >= *size {
            return None;
        }
        Some(if offset == 0 {
            name.clone()
        } else {
            format!("{name}+0x{offset:x}")
        })
    }

    /// The address of a function, for scripts that set breakpoints by name.
    pub fn address_of(&self, name: &str) -> Option<u32> {
        self.funcs.iter().find(|f| f.2 == name).map(|f| f.0)
    }

    /// Number of functions known.
    pub fn len(&self) -> usize {
        self.funcs.len()
    }

    /// Whether no functions are known.
    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_bytes() -> Vec<u8> {
        let mut b = b"SICP".to_vec();
        b.extend([1, 0, 7, 0]);
        b.extend(9u32.to_le_bytes());
        b.extend(0u32.to_le_bytes()); // length, below
        let mut regs = Vec::new();
        for (id, v) in [
            (14u8, 0xFFFF_FFF9u32),
            (15, 0x0800_0124),
            (16, 0x0100_0003),
            (13, 0x2000_0fe0),
        ] {
            regs.extend([id, 1, 0, 0]);
            regs.extend(v.to_le_bytes());
        }
        regs.extend([20, 1, 0, 0]);
        regs.extend(0x0200_0001u32.to_le_bytes());
        regs.extend([0, 0, 0, 0, 0, 0, 0, 0]); // r0 unread
        b.extend([SEC_REGS, 0, 0, 0]);
        b.extend((regs.len() as u32).to_le_bytes());
        b.extend(regs);
        b.extend([SEC_FAULT, 0, 0, 0]);
        b.extend(32u32.to_le_bytes());
        b.extend(0b0000110u32.to_le_bytes()); // cfsr, hfsr
        for v in [0u32, 0x0000_8200, 0x4000_0000, 0, 0, 0x2000_8000, 0] {
            b.extend(v.to_le_bytes());
        }
        b.extend([SEC_MEM, 0, 0, 0]);
        let frame: Vec<u32> = vec![1, 2, 3, 4, 12, 0x0800_0101, 0x0800_0200, 0x0100_0000];
        b.extend((8 + 32 + 2u32).to_le_bytes());
        b.extend(0x2000_0fe0u32.to_le_bytes());
        b.extend(64u32.to_le_bytes());
        for w in frame {
            b.extend(w.to_le_bytes());
        }
        b.extend([0xaa, 0xbb, 0, 0]); // 2 data bytes and padding
        b.extend([SEC_STOP, 0, 0, 0]);
        b.extend(32u32.to_le_bytes());
        b.extend(8u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(1234u64.to_le_bytes());
        let mut cause = [0u8; 16];
        cause[..9].copy_from_slice(b"HardFault");
        b.extend(cause);
        b.extend([99, 0, 0, 0, 0, 0, 0, 0]); // unknown empty section
        let len = b.len() as u32;
        b[12..16].copy_from_slice(&len.to_le_bytes());
        b
    }

    #[test]
    fn capture_decodes() {
        let c = Capture::parse(&capture_bytes()).unwrap();
        assert_eq!((c.id, c.on, c.watch), (9, 0, 7));
        assert_eq!(c.reg(15), Some(0x0800_0124));
        assert_eq!(c.reg(0), None);
        assert_eq!(c.cause, "HardFault");
        assert!(c.halted && c.dfsr == 8 && c.uptime_us == 1234);
        assert_eq!(c.mem[0].data.len(), 34);
        assert_eq!(c.exception_frame().unwrap()[6], 0x0800_0200);
        let f = c.fault.as_ref().unwrap();
        assert_eq!(
            (f.cfsr, f.hfsr, f.bfar),
            (Some(0x8200), Some(0x4000_0000), None)
        );

        let j = c.to_json(None);
        assert_eq!(j["exception"], "HardFault");
        assert_eq!(j["stacked"]["pc"], "0x08000200");
        assert_eq!(j["regs"]["control"], 2);
        assert_eq!(j["regs"]["primask"], 1);
        let bits: Vec<&str> = j["fault"]["cfsr_bits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["bit"].as_str().unwrap())
            .collect();
        assert_eq!(bits, ["PRECISERR", "BFARVALID"]);
        assert_eq!(j["fault"]["hfsr_bits"][0]["bit"], "FORCED");
        assert!(j["fault"].get("bfar").is_none()); // its read failed
    }

    #[test]
    fn capture_rejects_damage() {
        let mut b = capture_bytes();
        b.truncate(b.len() - 1);
        assert!(Capture::parse(&b).is_err());
        let mut b = capture_bytes();
        b[0] = b'X';
        assert!(Capture::parse(&b).is_err());
        let mut b = capture_bytes();
        let n = b.len();
        b[20..24].copy_from_slice(&(n as u32).to_le_bytes()); // section longer than the capture
        assert!(Capture::parse(&b).is_err());
    }

    #[test]
    fn symbols_name_addresses() {
        let s = Symbols {
            funcs: vec![(0x100, 0x20, "main".into()), (0x200, 0, "handler".into())],
        };
        assert_eq!(s.describe(0x101).as_deref(), Some("main"));
        assert_eq!(s.describe(0x112).as_deref(), Some("main+0x12"));
        assert_eq!(s.describe(0x120), None);
        assert_eq!(s.describe(0x2f0).as_deref(), Some("handler+0xf0"));
        assert_eq!(s.describe(0x50), None);
        assert_eq!(s.address_of("handler"), Some(0x200));
    }
}
