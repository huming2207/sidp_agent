//! Soul Injector power service: target current and supply voltage.
//!
//! SIDP opcodes 0x0200 and up, described in sidp_client's
//! `docs/sidp-power.md`. Requests and replies are the protobuf messages in
//! `proto/power.proto` (a copy of sidp_client's; keep them identical).
//! POWER_DATA frames are packed little-endian runs of raw INA226 register
//! values: [`DataFrame::parse`] reads them and [`Converter`] turns them into
//! amps and volts, applying the [`Zero`] correction measured on this computer.
use crate::codec::invalid;
use serde::{Deserialize, Serialize};
use std::io;

/// Generated power service messages.
#[allow(missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/si.power.rs"));
}

pub use proto::{PowerInfo, Range, StartRequest, StartResponse, StopReason, StopResponse, Stopped};

/// POWER_INFO opcode.
pub const POWER_INFO: u16 = 0x0200;
/// POWER_START opcode.
pub const POWER_START: u16 = 0x0201;
/// POWER_STOP opcode.
pub const POWER_STOP: u16 = 0x0202;
/// POWER_DATA log-stream opcode.
pub const POWER_DATA: u16 = 0x0210;
/// POWER_STOPPED event opcode.
pub const POWER_STOPPED: u16 = 0x0220;

/// Number of current ranges.
pub const RANGE_COUNT: usize = 3;

const DATA_HEADER: usize = 16;
const RUN_HEADER: usize = 16;
const RAW_SAMPLE: usize = 4;
const ENCODING_RAW: u8 = 0;

/// Something the device sent about a measurement, outside any request.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// A POWER_DATA frame.
    Data(DataFrame),
    /// The device stopped measuring on its own.
    Stopped(Stopped),
}

/// Raw INA226 register values of one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawSample {
    /// Shunt voltage register.
    pub shunt: i16,
    /// Bus voltage register, measuring VPP_INPUT.
    pub bus: u16,
}

/// Back-to-back conversions in one range, evenly spaced from `t_first_us`
/// to `t_last_us`.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Range the samples were measured in.
    pub range: Range,
    /// How many samples at the start follow a step down.
    pub settling: u16,
    /// End of the first conversion, device clock in microseconds.
    pub t_first_us: u64,
    /// End of the last conversion, device clock in microseconds.
    pub t_last_us: u64,
    /// The samples, at least one.
    pub samples: Vec<RawSample>,
}

/// One POWER_DATA frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DataFrame {
    /// Frame sequence number; a jump means frames were dropped.
    pub seq: u32,
    /// Device clock in microseconds that run times are relative to.
    pub t_us: u64,
    /// Runs, in time order.
    pub runs: Vec<Run>,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

impl DataFrame {
    /// Reads a POWER_DATA payload, checking every length and field.
    ///
    /// ```
    /// use sidp_agent::power::{DataFrame, Range};
    /// let mut payload = vec![7, 0, 0, 0, 0x10, 0x27, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
    /// payload.extend([0, 0, 2, 0, 1, 0, 0, 0, 5, 0, 0, 0, 105, 0, 0, 0]);
    /// payload.extend([1, 0, 0x40, 0x0a, 2, 0, 0x40, 0x0a]);
    /// let frame = DataFrame::parse(&payload)?;
    /// assert_eq!(frame.seq, 7);
    /// assert_eq!(frame.runs[0].range, Range::Low);
    /// assert_eq!(frame.runs[0].t_last_us, 10_105);
    /// assert_eq!(frame.runs[0].samples[1].shunt, 2);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn parse(payload: &[u8]) -> io::Result<Self> {
        if payload.len() < DATA_HEADER {
            return Err(invalid("short POWER_DATA frame"));
        }
        let seq = u32_at(payload, 0);
        let t_us = u64::from_le_bytes(payload[4..12].try_into().unwrap());
        let run_count = usize::from(u16_at(payload, 12));

        let mut runs = Vec::with_capacity(run_count);
        let mut at = DATA_HEADER;
        for _ in 0..run_count {
            let header = payload
                .get(at..at + RUN_HEADER)
                .ok_or_else(|| invalid("short POWER_DATA run"))?;
            let range = Range::try_from(i32::from(header[0]))
                .map_err(|_| invalid("unknown POWER_DATA range"))?;
            if header[1] != ENCODING_RAW {
                return Err(invalid("unknown POWER_DATA encoding"));
            }
            let count = usize::from(u16_at(header, 2));
            let settling = u16_at(header, 4);
            let t_first = u32_at(header, 8);
            let t_last = u32_at(header, 12);
            if count == 0 || usize::from(settling) > count || t_last < t_first {
                return Err(invalid("inconsistent POWER_DATA run"));
            }
            at += RUN_HEADER;

            let bytes = payload
                .get(at..at + count * RAW_SAMPLE)
                .ok_or_else(|| invalid("short POWER_DATA samples"))?;
            let samples = bytes
                .as_chunks::<RAW_SAMPLE>()
                .0
                .iter()
                .map(|raw| RawSample {
                    shunt: i16::from_le_bytes([raw[0], raw[1]]),
                    bus: u16_at(raw, 2),
                })
                .collect();
            at += count * RAW_SAMPLE;

            runs.push(Run {
                range,
                settling,
                t_first_us: t_us + u64::from(t_first),
                t_last_us: t_us + u64::from(t_last),
                samples,
            });
        }
        if at != payload.len() {
            return Err(invalid("trailing bytes in POWER_DATA frame"));
        }
        Ok(Self { seq, t_us, runs })
    }
}

/// Checks `request` against what the device accepts, explaining any problem.
///
/// ```
/// use sidp_agent::power::{PowerInfo, StartRequest, check_start};
/// let info = PowerInfo {
///     conversion_us: vec![140, 1100],
///     averages: vec![1, 4],
///     min_sample_period_us: 588,
///     ..Default::default()
/// };
/// let request = |conversion_us, averages| StartRequest { conversion_us, averages, ..Default::default() };
/// assert!(check_start(&info, &request(1100, 1)).is_ok());
/// assert!(check_start(&info, &request(140, 4)).is_err()); // 560 us is too short
/// assert!(check_start(&info, &request(588, 1)).is_err()); // not a conversion time
/// ```
pub fn check_start(info: &PowerInfo, request: &StartRequest) -> Result<(), String> {
    if !info.conversion_us.contains(&request.conversion_us) {
        return Err(format!(
            "conversion time {} us is not one of {:?}",
            request.conversion_us, info.conversion_us
        ));
    }
    let averages = request.averages.max(1);
    if !info.averages.contains(&averages) {
        return Err(format!(
            "{averages} averages is not one of {:?}",
            info.averages
        ));
    }
    let period = u64::from(request.conversion_us) * u64::from(averages);
    if period < u64::from(info.min_sample_period_us) {
        return Err(format!(
            "a sample every {period} us is faster than the device can read; the shortest is {} us",
            info.min_sample_period_us
        ));
    }
    Ok(())
}

/// Why the device stopped measuring, in words.
pub fn describe_stop(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Unknown => "stopped for an unknown reason",
        StopReason::StopDuration => "the requested duration elapsed",
        StopReason::StopNoSupply => {
            "no supply on VPP_INPUT (PROG pin 11), or it is outside 1.7 to 3.7 V"
        }
        StopReason::StopSensorFailed => "a current sensor stopped answering",
        StopReason::StopRangeFault => "the current range switches did not respond",
    }
}

/// Short display name of a range.
pub fn range_name(range: Range) -> &'static str {
    match range {
        Range::Low => "LOW",
        Range::Mid => "MID",
        Range::High => "HIGH",
    }
}

/// Zero correction: each range's current with nothing on VPP_OUTPUT, per
/// volt of VPP_INPUT. Mostly the INA226 input bias current, which scales with
/// the supply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Zero {
    /// Indexed by [`Range`] value: LOW, MID, HIGH.
    pub amps_per_volt: [f64; RANGE_COUNT],
}

impl Zero {
    /// Current to subtract in `range` at supply `vin_v`.
    pub fn offset(&self, range: Range, vin_v: f64) -> f64 {
        self.amps_per_volt[range as usize] * vin_v
    }
}

/// One converted sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Seconds since the measurement started, by the device clock.
    pub t_s: f64,
    /// Target current, zero corrected.
    pub current_a: f64,
    /// VPP_INPUT, as measured.
    pub vin_v: f64,
    /// VPP_OUTPUT, computed from VPP_INPUT and the range's series resistance.
    pub vout_v: f64,
    /// Range the sample was measured in.
    pub range: Range,
    /// Follows a step down: real supply current, including output capacitor
    /// recharge, so not the target's current at that moment.
    pub settling: bool,
    /// The time since the previous sample was not measured.
    pub gap_before: bool,
}

/// Turns POWER_DATA frames into [`Sample`]s.
pub struct Converter {
    shunt_lsb_v: f64,
    bus_lsb_v: f64,
    shunt_ohm: [f64; RANGE_COUNT],
    chain_ohm: [f64; RANGE_COUNT],
    zero: Zero,
    start_us: u64,
    gap_us: f64,
    next_seq: Option<u32>,
    last_us: Option<f64>,
}

impl Converter {
    /// A converter for the measurement `start` began, on a device described by `info`.
    pub fn new(info: &PowerInfo, zero: Zero, start: &StartResponse) -> io::Result<Self> {
        let mut shunt_ohm = [0.0; RANGE_COUNT];
        let mut chain_ohm = [0.0; RANGE_COUNT];
        let mut seen = [false; RANGE_COUNT];
        for range in &info.ranges {
            let index = usize::try_from(range.range)
                .ok()
                .filter(|index| *index < RANGE_COUNT)
                .ok_or_else(|| invalid("unknown range in POWER_INFO"))?;
            shunt_ohm[index] = f64::from(range.shunt_ohm);
            chain_ohm[index] = f64::from(range.chain_ohm);
            seen[index] = true;
        }
        if seen.contains(&false)
            || shunt_ohm.iter().any(|ohm| *ohm <= 0.0)
            || info.shunt_lsb_v <= 0.0
            || info.bus_lsb_v <= 0.0
        {
            return Err(invalid("incomplete POWER_INFO"));
        }
        Ok(Self {
            shunt_lsb_v: f64::from(info.shunt_lsb_v),
            bus_lsb_v: f64::from(info.bus_lsb_v),
            shunt_ohm,
            chain_ohm,
            zero,
            start_us: start.start_us,
            // Runs restart on a missed conversion, so anything longer is a gap.
            gap_us: f64::from(start.sample_period_us) * 1.5,
            next_seq: None,
            last_us: None,
        })
    }

    /// Appends the samples of `frame` to `out`.
    pub fn convert(&mut self, frame: &DataFrame, out: &mut Vec<Sample>) {
        let dropped = self.next_seq.is_some_and(|seq| seq != frame.seq);
        self.next_seq = Some(frame.seq.wrapping_add(1));

        for (run_index, run) in frame.runs.iter().enumerate() {
            let index = run.range as usize;
            let count = run.samples.len();
            let spacing = if count > 1 {
                (run.t_last_us - run.t_first_us) as f64 / (count - 1) as f64
            } else {
                0.0
            };
            for (i, raw) in run.samples.iter().enumerate() {
                let t_us = run.t_first_us as f64 + spacing * i as f64;
                let gap_before = i == 0
                    && ((dropped && run_index == 0)
                        || self.last_us.is_some_and(|last| t_us - last > self.gap_us));
                self.last_us = Some(t_us);

                let vin_v = f64::from(raw.bus) * self.bus_lsb_v;
                let measured = f64::from(raw.shunt) * self.shunt_lsb_v / self.shunt_ohm[index];
                let current_a = measured - self.zero.offset(run.range, vin_v);
                out.push(Sample {
                    t_s: (t_us - self.start_us as f64) / 1e6,
                    current_a,
                    vin_v,
                    vout_v: vin_v - current_a * self.chain_ohm[index],
                    range: run.range,
                    settling: i < usize::from(run.settling),
                    gap_before,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn info() -> PowerInfo {
        let range = |range: Range, shunt_ohm, chain_ohm| proto::RangeInfo {
            range: range as i32,
            shunt_ohm,
            chain_ohm,
        };
        PowerInfo {
            ranges: vec![
                range(Range::Low, 162.0, 166.07),
                range(Range::Mid, 4.02, 4.07),
                range(Range::High, 0.05, 0.05),
            ],
            shunt_lsb_v: 2.5e-6,
            bus_lsb_v: 1.25e-3,
            conversion_us: vec![140, 204, 332, 588, 1100, 2116, 4156, 8244],
            averages: vec![1, 4, 16, 64, 128, 256, 512, 1024],
            min_sample_period_us: 588,
            vout_measured: false,
        }
    }

    fn run(range: Range, settling: u16, t_first_us: u64, t_last_us: u64, shunts: &[i16]) -> Run {
        Run {
            range,
            settling,
            t_first_us,
            t_last_us,
            samples: shunts
                .iter()
                .map(|&shunt| RawSample { shunt, bus: 2640 })
                .collect(),
        }
    }

    fn start() -> StartResponse {
        StartResponse {
            sample_period_us: 1100,
            start_us: 1_000_000,
        }
    }

    #[test]
    fn converts_raw_codes() {
        let mut converter = Converter::new(&info(), Zero::default(), &start()).unwrap();
        let frame = DataFrame {
            seq: 0,
            t_us: 1_001_100,
            runs: vec![run(Range::Low, 1, 1_001_100, 1_003_300, &[648, 648, 648])],
        };
        let mut out = Vec::new();
        converter.convert(&frame, &mut out);

        assert_eq!(out.len(), 3);
        // 648 x 2.5 uV / 162 ohm = 10 uA; 2640 x 1.25 mV = 3.3 V
        // The constants are f32 on the wire, so compare to a few parts per million.
        assert!((out[0].current_a - 10e-6).abs() < 1e-11);
        assert!((out[0].vin_v - 3.3).abs() < 1e-6);
        assert!((out[0].vout_v - (3.3 - 10e-6 * 166.07)).abs() < 1e-6);
        assert!((out[2].t_s - 0.0033).abs() < 1e-9);
        assert!(out[0].settling && !out[1].settling);
        assert!(!out[0].gap_before);
    }

    #[test]
    fn subtracts_the_zero_scaled_with_vin() {
        let zero = Zero {
            amps_per_volt: [2e-6, 0.0, 0.0],
        };
        let mut converter = Converter::new(&info(), zero, &start()).unwrap();
        let frame = DataFrame {
            seq: 0,
            t_us: 0,
            runs: vec![run(Range::Low, 0, 1_001_100, 1_001_100, &[648])],
        };
        let mut out = Vec::new();
        converter.convert(&frame, &mut out);
        assert!((out[0].current_a - (10e-6 - 2e-6 * 3.3)).abs() < 1e-11);
    }

    #[test]
    fn marks_gaps_and_dropped_frames() {
        let mut converter = Converter::new(&info(), Zero::default(), &start()).unwrap();
        let mut out = Vec::new();
        let frame = |seq, runs| DataFrame { seq, t_us: 0, runs };
        converter.convert(
            &frame(
                0,
                vec![
                    run(Range::High, 0, 1_001_100, 1_002_200, &[1, 1]),
                    // A missed conversion: more than 1.5 periods later.
                    run(Range::High, 0, 1_005_000, 1_005_000, &[1]),
                ],
            ),
            &mut out,
        );
        // Next in sequence and on time: no gap.
        converter.convert(
            &frame(1, vec![run(Range::High, 0, 1_006_100, 1_006_100, &[1])]),
            &mut out,
        );
        // Frame 2 was dropped.
        converter.convert(
            &frame(3, vec![run(Range::High, 0, 1_007_200, 1_007_200, &[1])]),
            &mut out,
        );
        let gaps: Vec<bool> = out.iter().map(|s| s.gap_before).collect();
        assert_eq!(gaps, [false, false, true, false, true]);
    }

    #[test]
    fn rejects_malformed_frames() {
        let mut payload = vec![0u8; 16];
        payload[12] = 1; // one run, but no run header
        assert!(DataFrame::parse(&payload).is_err());

        payload.extend([3, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(DataFrame::parse(&payload).is_err(), "range 3 is unknown");

        payload[16] = 0;
        payload[17] = 1;
        assert!(DataFrame::parse(&payload).is_err(), "encoding 1 is unknown");

        payload[17] = 0;
        assert!(DataFrame::parse(&payload).is_ok());
        payload.push(0);
        assert!(DataFrame::parse(&payload).is_err(), "trailing byte");
    }
}
