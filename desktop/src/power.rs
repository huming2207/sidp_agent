//! Power measurement: recordings, their statistics and export, and the
//! sources that produce samples.
//!
//! The Soul Injector measures the target's supply with three INA226
//! sensors on a shunt chain between `VPP_INPUT` and `VPP_OUTPUT`. The
//! hardware moves up to a higher current range on its own; the firmware
//! moves back down. Each sample therefore carries the range it was
//! measured in.
//!
//! Samples from the Soul Injector come through the device worker, which owns
//! the USB connection: it fills a [`DeviceFeed`] that the view reads like any
//! other capture. [`Source::Simulated`] produces a realistic waveform so the
//! interface can be tried without a device.
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The shunt in use when a sample was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Range {
    /// Up to about 216 µA, 162 Ω shunt.
    #[default]
    Low,
    /// About 216 µA to 8.8 mA, 4.02 Ω shunt.
    Mid,
    /// About 8.8 mA to 1.64 A, 50 mΩ shunt.
    High,
}

impl Range {
    /// Short display name.
    pub fn label(self) -> &'static str {
        match self {
            Range::Low => "LOW",
            Range::Mid => "MID",
            Range::High => "HIGH",
        }
    }

    /// The range the hardware switches to for `current` at 3.3 V.
    pub fn for_current(current: f32) -> Self {
        if current > 8.8e-3 {
            Range::High
        } else if current > 216e-6 {
            Range::Mid
        } else {
            Range::Low
        }
    }
}

/// Sample rates offered, in samples per second. Each is one of the Soul
/// Injector's settings; see [`DeviceSetting::for_rate`].
pub const SAMPLE_RATES: [u32; 3] = [227, 909, 1_700];

/// How the Soul Injector samples: an INA226 conversion time, averaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceSetting {
    /// Conversion time in microseconds.
    pub conversion_us: u32,
    /// Conversions averaged into one sample.
    pub averages: u32,
}

impl DeviceSetting {
    /// The setting behind a rate from [`SAMPLE_RATES`]. 1.7 kS/s is the
    /// fastest the device reads, and can miss the odd conversion.
    pub fn for_rate(rate: u32) -> Self {
        let (conversion_us, averages) = match rate {
            ..=227 => (1100, 4),
            228..=909 => (1100, 1),
            _ => (588, 1),
        };
        Self {
            conversion_us,
            averages,
        }
    }
}

/// Recording lengths offered; `None` records until stopped.
pub const DURATIONS: [Option<Duration>; 4] = [
    None,
    Some(Duration::from_secs(10)),
    Some(Duration::from_secs(60)),
    Some(Duration::from_secs(600)),
];

/// Samples per summary block, used to draw long recordings quickly.
const BLOCK: usize = 256;

/// Minimum, maximum and sum over a run of samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    /// Lowest current, in amperes.
    pub min: f32,
    /// Highest current, in amperes.
    pub max: f32,
    /// Highest range used.
    pub range: Range,
}

impl Envelope {
    fn of(current: f32, range: Range) -> Self {
        Self {
            min: current,
            max: current,
            range,
        }
    }

    fn merge(&mut self, other: &Envelope) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.range = self.range.max(other.range);
    }
}

/// Statistics over part of a recording.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// Samples included.
    pub samples: usize,
    /// Length of time covered, in seconds.
    pub duration: f64,
    /// Mean current, in amperes.
    pub average: f64,
    /// Lowest current, in amperes.
    pub min: f32,
    /// Highest current, in amperes.
    pub max: f32,
    /// Charge drawn, in coulombs.
    pub charge: f64,
    /// Energy drawn from the supply, in joules.
    pub energy: f64,
}

/// One measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Target current, in amperes.
    pub current: f32,
    /// Supply voltage (`VPP_INPUT`), in volts.
    pub supply: f32,
    /// Range the current was measured in.
    pub range: Range,
}

/// Samples taken at a fixed rate.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    rate: u32,
    current: Vec<f32>,
    supply: Vec<f32>,
    range: Vec<Range>,
    blocks: Vec<Envelope>,
}

impl Recording {
    /// An empty recording at `rate` samples per second.
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            ..Self::default()
        }
    }

    /// Samples per second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.current.len()
    }

    /// Whether there are no samples.
    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }

    /// Length of the recording, in seconds.
    pub fn duration(&self) -> f64 {
        self.len() as f64 / f64::from(self.rate)
    }

    /// The most recent sample.
    pub fn last(&self) -> Option<Sample> {
        self.sample(self.len().checked_sub(1)?)
    }

    /// Sample `index`.
    pub fn sample(&self, index: usize) -> Option<Sample> {
        Some(Sample {
            current: *self.current.get(index)?,
            supply: self.supply[index],
            range: self.range[index],
        })
    }

    /// The sample nearest `time` seconds.
    pub fn sample_at(&self, time: f64) -> Option<Sample> {
        if self.is_empty() || time < 0.0 {
            return None;
        }
        let index = (time * f64::from(self.rate)) as usize;
        self.sample(index.min(self.len() - 1))
    }

    /// Appends samples.
    pub fn extend(&mut self, samples: &[Sample]) {
        for sample in samples {
            if self.len().is_multiple_of(BLOCK) {
                self.blocks.push(Envelope::of(sample.current, sample.range));
            } else if let Some(block) = self.blocks.last_mut() {
                block.merge(&Envelope::of(sample.current, sample.range));
            }
            self.current.push(sample.current);
            self.supply.push(sample.supply);
            self.range.push(sample.range);
        }
    }

    /// Index range of the samples between `from` and `to` seconds.
    pub fn indices(&self, from: f64, to: f64) -> std::ops::Range<usize> {
        let rate = f64::from(self.rate);
        let clamp = |t: f64| ((t.max(0.0) * rate).round() as usize).min(self.len());
        let (start, end) = (clamp(from.min(to)), clamp(from.max(to)));
        start..end
    }

    /// Statistics over the samples between `from` and `to` seconds.
    pub fn stats(&self, from: f64, to: f64) -> Option<Stats> {
        let range = self.indices(from, to);
        if range.is_empty() {
            return None;
        }
        let dt = 1.0 / f64::from(self.rate);
        let mut sum = 0.0f64;
        let mut energy = 0.0f64;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for index in range.clone() {
            let current = self.current[index];
            sum += f64::from(current);
            energy += f64::from(current) * f64::from(self.supply[index]) * dt;
            min = min.min(current);
            max = max.max(current);
        }
        let samples = range.len();
        Some(Stats {
            samples,
            duration: samples as f64 * dt,
            average: sum / samples as f64,
            min,
            max,
            charge: sum * dt,
            energy,
        })
    }

    /// The current envelope between `from` and `to` seconds, split into
    /// `columns` equal parts for drawing. Columns without samples are `None`.
    pub fn envelope(&self, from: f64, to: f64, columns: usize) -> Vec<Option<Envelope>> {
        let mut out = vec![None; columns];
        if columns == 0 || to <= from || self.is_empty() {
            return out;
        }
        let rate = f64::from(self.rate);
        let per_column = (to - from) * rate / columns as f64;
        for (column, slot) in out.iter_mut().enumerate() {
            let start = from * rate + column as f64 * per_column;
            let end = start + per_column;
            let first = start.floor().max(0.0) as usize;
            let last = (end.ceil().max(0.0) as usize).min(self.len());
            if first >= last {
                continue;
            }
            *slot = Some(self.envelope_of(first, last));
        }
        out
    }

    /// Envelope of samples `first..last`, using block summaries for the
    /// whole blocks inside.
    fn envelope_of(&self, first: usize, last: usize) -> Envelope {
        let sample = |i: usize| Envelope::of(self.current[i], self.range[i]);
        let mut result = sample(first);
        let mut i = first;
        while i < last {
            if i.is_multiple_of(BLOCK) && i + BLOCK <= last {
                result.merge(&self.blocks[i / BLOCK]);
                i += BLOCK;
            } else {
                result.merge(&sample(i));
                i += 1;
            }
        }
        result
    }

    /// Writes the samples between `from` and `to` seconds as CSV.
    pub fn write_csv(&self, from: f64, to: f64, out: &mut impl Write) -> io::Result<()> {
        writeln!(out, "time_s,current_a,supply_v,range")?;
        let rate = f64::from(self.rate);
        for index in self.indices(from, to) {
            writeln!(
                out,
                "{:.6},{:e},{:.4},{}",
                index as f64 / rate,
                self.current[index],
                self.supply[index],
                self.range[index].label()
            )?;
        }
        Ok(())
    }
}

/// Where samples come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// The connected Soul Injector.
    #[default]
    SoulInjector,
    /// A generated waveform for trying the interface.
    Simulated,
}

/// What a capture reports.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// New samples, in order.
    Samples(Vec<Sample>),
    /// The capture ended: `Ok` when stopped or finished, `Err` with a reason.
    Ended(Result<(), String>),
}

/// What the device worker needs to record into a view: the setting, how
/// long, the zero correction, and where samples go.
#[derive(Clone)]
pub struct DeviceFeed {
    /// Samples per second of the recording the samples go into.
    pub rate: u32,
    /// Sampling setting.
    pub setting: DeviceSetting,
    /// Recording length; `None` records until stopped.
    pub duration: Option<Duration>,
    /// Zero correction for this device.
    pub zero: sidp_agent::power::Zero,
    /// Where samples and the end go.
    pub events: Sender<CaptureEvent>,
    /// Set when the user stops the recording.
    pub stop: Arc<AtomicBool>,
}

impl DeviceFeed {
    /// Whether the user stopped the recording.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Places device samples, which carry their own times, on a recording's
/// fixed-rate grid. Each sample is the average over its conversion, so it
/// fills the grid points since the previous one, including any gap before it.
pub struct Resampler {
    rate: f64,
    next: u64,
}

impl Resampler {
    /// A resampler for a recording at `rate` samples per second.
    pub fn new(rate: u32) -> Self {
        Self {
            rate: f64::from(rate.max(1)),
            next: 0,
        }
    }

    /// Appends grid samples up to the time of each device sample.
    pub fn push(&mut self, samples: &[sidp_agent::power::Sample], out: &mut Vec<Sample>) {
        for sample in samples {
            let point = Sample {
                current: sample.current_a as f32,
                supply: sample.vin_v as f32,
                range: match sample.range {
                    sidp_agent::power::Range::Low => Range::Low,
                    sidp_agent::power::Range::Mid => Range::Mid,
                    sidp_agent::power::Range::High => Range::High,
                },
            };
            let end = (sample.t_s * self.rate).round().max(0.0) as u64;
            while self.next <= end {
                out.push(point);
                self.next += 1;
            }
        }
    }
}

/// A running capture.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing from `source` at `rate` samples per second for
    /// `duration` (until stopped when `None`).
    pub fn start(
        source: Source,
        rate: u32,
        duration: Option<Duration>,
        events: Sender<CaptureEvent>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("power".into())
            .spawn(move || {
                let result = match source {
                    // The view hands device recordings to the worker instead.
                    Source::SoulInjector => {
                        Err("The Soul Injector records through its own connection.".to_owned())
                    }
                    Source::Simulated => {
                        simulate(rate.max(1), duration, &flag, &events);
                        Ok(())
                    }
                };
                let _ = events.send(CaptureEvent::Ended(result));
            })
            .expect("spawn power thread");
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// A capture that another thread produces, such as the device worker;
    /// `stop` is how it learns to stop.
    pub fn external(stop: Arc<AtomicBool>) -> Self {
        Self { stop, thread: None }
    }

    /// Asks the capture to stop; an `Ended` event follows.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Produces simulated samples in real time, 20 batches a second.
fn simulate(
    rate: u32,
    duration: Option<Duration>,
    stop: &AtomicBool,
    events: &Sender<CaptureEvent>,
) {
    let started = Instant::now();
    let limit = duration.map(|d| (d.as_secs_f64() * f64::from(rate)) as u64);
    let mut produced = 0u64;
    let mut noise = Noise(0x2545_f491_4f6c_dd1d);
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        let mut due = (started.elapsed().as_secs_f64() * f64::from(rate)) as u64;
        if let Some(limit) = limit {
            due = due.min(limit);
        }
        let batch: Vec<Sample> = (produced..due)
            .map(|index| simulated_sample(index as f64 / f64::from(rate), &mut noise))
            .collect();
        produced = due;
        if !batch.is_empty() && events.send(CaptureEvent::Samples(batch)).is_err() {
            return;
        }
        if limit.is_some_and(|limit| produced >= limit) {
            return;
        }
    }
}

/// A small, fast random source for measurement noise.
struct Noise(u64);

impl Noise {
    /// Uniform in -1..1.
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// A low-power MCU at 3.3 V: asleep at a few µA, awake for 5 ms every
/// second, and a 20 ms radio burst every five seconds.
fn simulated_sample(time: f64, noise: &mut Noise) -> Sample {
    let in_second = time.fract();
    let in_burst = time % 5.0;
    let base: f32 = if (1.0..1.02).contains(&in_burst) {
        // Ramp up, transmit, ramp down.
        let t = ((in_burst - 1.0) / 0.02) as f32;
        let shape = (t * std::f32::consts::PI).sin().max(0.0).powf(0.3);
        14e-3 * shape + 1.2e-3
    } else if in_second < 0.005 {
        3.1e-3
    } else if in_second < 0.0065 {
        // Clocks slowing down before sleep.
        0.4e-3
    } else {
        4.2e-6
    };
    let current = (base * (1.0 + 0.04 * noise.next()) + 0.3e-6 * noise.next()).max(0.0);
    let supply = 3.30 - 0.6 * current + 0.002 * noise.next();
    Sample {
        current,
        supply,
        range: Range::for_current(current),
    }
}

/// `seconds` of the simulated waveform at `rate`, generated at once.
pub fn simulated_recording(rate: u32, seconds: f64) -> Recording {
    let mut noise = Noise(0x2545_f491_4f6c_dd1d);
    let count = (seconds * f64::from(rate)) as u64;
    let samples: Vec<Sample> = (0..count)
        .map(|index| simulated_sample(index as f64 / f64::from(rate), &mut noise))
        .collect();
    let mut recording = Recording::new(rate);
    recording.extend(&samples);
    recording
}

/// Current in A, with a unit that keeps three significant digits.
pub fn format_current(amperes: f64) -> String {
    format_si(amperes, "A")
}

/// Charge in C.
pub fn format_charge(coulombs: f64) -> String {
    format_si(coulombs, "C")
}

/// Energy in J.
pub fn format_energy(joules: f64) -> String {
    format_si(joules, "J")
}

/// A duration in seconds, at a resolution suited to its length.
pub fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds >= 60.0 {
        let whole = seconds as u64;
        format!("{}:{:02}", whole / 60, whole % 60)
    } else if seconds >= 1.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{:.1} ms", seconds * 1e3)
    }
}

fn format_si(value: f64, unit: &str) -> String {
    let magnitude = value.abs();
    let (scale, prefix) = if magnitude == 0.0 || magnitude >= 1.0 {
        (1.0, "")
    } else if magnitude >= 1e-3 {
        (1e3, "m")
    } else if magnitude >= 1e-6 {
        (1e6, "µ")
    } else {
        (1e9, "n")
    };
    let scaled = value * scale;
    let digits = if scaled.abs() >= 100.0 {
        0
    } else if scaled.abs() >= 10.0 {
        1
    } else {
        2
    };
    format!("{scaled:.digits$} {prefix}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(current: f32, supply: f32, count: usize) -> Vec<Sample> {
        vec![
            Sample {
                current,
                supply,
                range: Range::for_current(current),
            };
            count
        ]
    }

    #[test]
    fn statistics_integrate_charge_and_energy() {
        let mut recording = Recording::new(1000);
        recording.extend(&constant(2e-3, 3.0, 1000));
        recording.extend(&constant(4e-3, 3.0, 1000));
        assert_eq!(recording.duration(), 2.0);

        let all = recording.stats(0.0, 2.0).unwrap();
        assert_eq!(all.samples, 2000);
        assert!((all.average - 3e-3).abs() < 1e-9);
        assert!((all.charge - 6e-3).abs() < 1e-9, "{}", all.charge);
        assert!((all.energy - 18e-3).abs() < 1e-8, "{}", all.energy);
        assert_eq!((all.min, all.max), (2e-3, 4e-3));

        let second_half = recording.stats(1.5, 1.0).unwrap();
        assert_eq!(second_half.samples, 500);
        assert!((second_half.average - 4e-3).abs() < 1e-9);
        assert!(recording.stats(5.0, 6.0).is_none());
    }

    #[test]
    fn envelopes_match_a_direct_scan() {
        let mut recording = Recording::new(1000);
        let mut noise = Noise(7);
        let samples: Vec<Sample> = (0..10_000)
            .map(|i| simulated_sample(i as f64 / 1000.0, &mut noise))
            .collect();
        recording.extend(&samples);
        for columns in [1, 7, 300, 20_000] {
            let envelope = recording.envelope(0.25, 9.75, columns);
            assert_eq!(envelope.len(), columns);
            for (column, value) in envelope.iter().enumerate() {
                // The same arithmetic as the implementation, so that edges
                // on whole samples round the same way.
                let per_column = 9500.0 / columns as f64;
                let start = 250.0 + column as f64 * per_column;
                let end = start + per_column;
                let (first, last) = (start.floor() as usize, (end.ceil() as usize).min(10_000));
                if first >= last {
                    assert!(value.is_none());
                    continue;
                }
                let slice = &samples[first..last];
                let min = slice
                    .iter()
                    .map(|s| s.current)
                    .fold(f32::INFINITY, f32::min);
                let max = slice
                    .iter()
                    .map(|s| s.current)
                    .fold(f32::NEG_INFINITY, f32::max);
                let range = slice.iter().map(|s| s.range).max().unwrap();
                assert_eq!(
                    *value,
                    Some(Envelope { min, max, range }),
                    "column {column}"
                );
            }
        }
    }

    #[test]
    fn simulated_waveform_uses_every_range() {
        let recording = simulated_recording(10_000, 5.0);
        assert_eq!(recording.len(), 50_000);
        let ranges: std::collections::BTreeSet<Range> = (0..recording.len())
            .map(|i| recording.sample(i).unwrap().range)
            .collect();
        assert_eq!(ranges.len(), 3);
    }

    #[test]
    fn csv_lists_the_chosen_samples() {
        let mut recording = Recording::new(2);
        recording.extend(&constant(1.5e-6, 3.3, 4));
        let mut out = Vec::new();
        recording.write_csv(0.5, 1.5, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "time_s,current_a,supply_v,range\n0.500000,1.5e-6,3.3000,LOW\n1.000000,1.5e-6,3.3000,LOW\n"
        );
    }

    #[test]
    fn values_read_naturally() {
        assert_eq!(format_current(4.2e-6), "4.20 µA");
        assert_eq!(format_current(0.0123), "12.3 mA");
        assert_eq!(format_current(0.0), "0.00 A");
        assert_eq!(format_charge(3.5e-3), "3.50 mC");
        assert_eq!(format_energy(250e-9), "250 nJ");
        assert_eq!(format_time(0.0125), "12.5 ms");
        assert_eq!(format_time(83.0), "1:23");
    }

    fn device_sample(t_s: f64, current_a: f64) -> sidp_agent::power::Sample {
        sidp_agent::power::Sample {
            t_s,
            current_a,
            vin_v: 3.3,
            vout_v: 3.3,
            range: sidp_agent::power::Range::Mid,
            settling: false,
            gap_before: false,
        }
    }

    #[test]
    fn resampling_keeps_device_time() {
        // A device slightly slower than nominal, and a missed sample: the
        // grid still advances with time, so charge stays right.
        let mut resampler = Resampler::new(1000);
        let mut out = Vec::new();
        resampler.push(
            &[
                device_sample(0.0011, 1e-3),
                device_sample(0.0022, 2e-3),
                device_sample(0.0055, 3e-3),
            ],
            &mut out,
        );
        // Grid points 0-1 ms take the first sample, 2 ms the second, and
        // 3-6 ms (5.5 ms rounds up) the one after the missed conversion.
        let currents: Vec<f32> = out.iter().map(|s| s.current).collect();
        assert_eq!(currents, [1e-3, 1e-3, 2e-3, 3e-3, 3e-3, 3e-3, 3e-3]);
        assert_eq!(out[0].range, Range::Mid);
        assert_eq!(out[0].supply, 3.3);
    }

    #[test]
    fn device_settings_match_the_offered_rates() {
        for rate in SAMPLE_RATES {
            let setting = DeviceSetting::for_rate(rate);
            let actual = 1e6 / f64::from(setting.conversion_us * setting.averages);
            assert!((actual - f64::from(rate)).abs() < 1.0, "{rate}: {actual}");
        }
    }

    #[test]
    fn a_simulated_capture_runs_for_its_duration() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _capture = Capture::start(
            Source::Simulated,
            1000,
            Some(Duration::from_millis(200)),
            sender,
        );
        let mut recording = Recording::new(1000);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match receiver.try_recv() {
                Ok(CaptureEvent::Samples(samples)) => recording.extend(&samples),
                Ok(CaptureEvent::Ended(result)) => {
                    assert_eq!(result, Ok(()));
                    break;
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("{error:?}"),
            }
        }
        assert_eq!(recording.len(), 200);
    }
}
