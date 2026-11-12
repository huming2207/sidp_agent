//! Power measurement commands of the command-line tool.
use crate::commands::Connection;
use serde::{Deserialize, Serialize};
use sidp_agent::power::{self, Converter, Range, StartRequest, StopReason, Zero};
use std::{
    error::Error,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

type CommandResult = Result<(), Box<dyn Error>>;

/// How often the recording loop checks for data.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Data frames queue behind the stop event, so keep reading this long after it.
const DRAIN_TIME: Duration = Duration::from_millis(300);
/// How long past the requested duration to wait for the device to stop.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Zero measurement: settling time, then averaging time, per range.
const ZERO_SETTLE: Duration = Duration::from_secs(1);
const ZERO_MEASURE: Duration = Duration::from_secs(2);

/// A zero correction and the device it was measured on.
#[derive(Serialize, Deserialize)]
struct ZeroFile {
    serial: String,
    zero: Zero,
}

/// `power info`.
pub fn info(connection: &Connection) -> CommandResult {
    let info = connection.open()?.power_info()?;
    for range in &info.ranges {
        let name = Range::try_from(range.range).map_or("?", power::range_name);
        println!(
            "{name:<4}  shunt {} ohm, {} ohm from VPP_INPUT to VPP_OUTPUT",
            range.shunt_ohm, range.chain_ohm
        );
    }
    println!("Conversion times (us): {:?}", info.conversion_us);
    println!("Averages:              {:?}", info.averages);
    println!(
        "Shortest sample period: {} us (conversion time x averages)",
        info.min_sample_period_us
    );
    if !info.vout_measured {
        println!("VPP_OUTPUT is computed from VPP_INPUT and the range's resistance.");
    }
    Ok(())
}

/// `power record`: records for `seconds` and writes every sample to `output`.
pub fn record(
    connection: &Connection,
    mut request: StartRequest,
    seconds: f64,
    output: &Path,
    zero_path: Option<&Path>,
) -> CommandResult {
    if !(seconds > 0.0 && seconds <= f64::from(u32::MAX) / 1000.0) {
        return Err("--seconds must be positive".into());
    }
    let mut session = connection.open()?;
    let zero = match zero_path {
        Some(path) => {
            let file: ZeroFile = serde_json::from_reader(File::open(path)?)?;
            let serial = session.device_info()?.serial;
            if file.serial != serial {
                eprintln!(
                    "warning: the zero was measured on {}, not this device ({serial})",
                    file.serial
                );
            }
            file.zero
        }
        None => Zero::default(),
    };

    let info = session.power_info()?;
    power::check_start(&info, &request)?;
    let (sender, receiver) = mpsc::channel();
    session.set_power_sink(Some(Box::new(move |message| {
        let _ = sender.send(message);
    })));
    request.duration_ms = (seconds * 1000.0).ceil() as u32;
    let start = session.power_start(&request)?;
    let mut converter = Converter::new(&info, zero, &start)?;
    eprintln!(
        "Recording for {seconds} s, one sample every {} us",
        start.sample_period_us
    );

    let mut out = BufWriter::new(File::create(output)?);
    writeln!(out, "time_s,current_a,vin_v,vout_v,range,settling,gap")?;
    let mut summary = Summary::default();
    let mut samples = Vec::new();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds) + STOP_GRACE;
    let mut stopped: Option<(power::Stopped, Instant)> = None;

    loop {
        session.poll(POLL_INTERVAL)?;
        for message in receiver.try_iter() {
            match message {
                power::Message::Data(frame) => {
                    samples.clear();
                    converter.convert(&frame, &mut samples);
                    for s in &samples {
                        writeln!(
                            out,
                            "{:.6},{:.5e},{:.4},{:.4},{},{},{}",
                            s.t_s,
                            s.current_a,
                            s.vin_v,
                            s.vout_v,
                            power::range_name(s.range),
                            u8::from(s.settling),
                            u8::from(s.gap_before)
                        )?;
                        summary.add(s);
                    }
                }
                power::Message::Stopped(event) => stopped = Some((event, Instant::now())),
            }
        }
        match &stopped {
            Some((_, at)) if at.elapsed() >= DRAIN_TIME => break,
            Some(_) => {}
            None if Instant::now() > deadline => {
                session.power_stop()?;
                return Err("the device did not stop measuring".into());
            }
            None => {}
        }
    }
    out.flush()?;

    let (event, _) = stopped.unwrap();
    summary.print(event.dropped_frames, session.dropped_frames());
    let reason = event.reason();
    if reason != StopReason::StopDuration {
        return Err(format!(
            "measurement stopped early: {}",
            power::describe_stop(reason)
        )
        .into());
    }
    Ok(())
}

/// `power zero`: measures the zero correction and writes it to `output`.
pub fn zero(connection: &Connection, output: &Path) -> CommandResult {
    eprintln!(
        "Measuring the zero; VPP_OUTPUT (PROG pin 9) must be open and VPP_INPUT (pin 11) powered."
    );
    let mut session = connection.open()?;
    let serial = session.device_info()?.serial;
    let info = session.power_info()?;
    let zero = session.power_measure_zero(&info, ZERO_SETTLE, ZERO_MEASURE)?;
    for range in [Range::Low, Range::Mid, Range::High] {
        let per_volt = zero.amps_per_volt[range as usize];
        println!(
            "{:<4}  {:.3} uA/V ({:.2} uA at 3.3 V)",
            power::range_name(range),
            per_volt * 1e6,
            per_volt * 3.3e6
        );
    }
    let file = ZeroFile { serial, zero };
    serde_json::to_writer_pretty(File::create(output)?, &file)?;
    println!("Written to {}", output.display());
    Ok(())
}

/// Totals printed after a recording.
#[derive(Default)]
struct Summary {
    count: u64,
    gaps: u64,
    sum_a: f64,
    min_a: f64,
    max_a: f64,
    sum_vin: f64,
    min_vin: f64,
    last_t_s: f64,
}

impl Summary {
    fn add(&mut self, s: &power::Sample) {
        if self.count == 0 {
            self.min_a = s.current_a;
            self.max_a = s.current_a;
            self.min_vin = s.vin_v;
        }
        self.count += 1;
        self.gaps += u64::from(s.gap_before);
        self.sum_a += s.current_a;
        self.min_a = self.min_a.min(s.current_a);
        self.max_a = self.max_a.max(s.current_a);
        self.sum_vin += s.vin_v;
        self.min_vin = self.min_vin.min(s.vin_v);
        self.last_t_s = s.t_s;
    }

    fn print(&self, dropped_frames: u32, damaged_frames: u64) {
        if self.count == 0 {
            println!("No samples received");
            return;
        }
        let count = self.count as f64;
        println!(
            "{} samples over {:.3} s ({:.0} per second)",
            self.count,
            self.last_t_s,
            count / self.last_t_s.max(f64::MIN_POSITIVE)
        );
        println!(
            "Current: average {:.3} uA, min {:.3} uA, max {:.3} uA",
            self.sum_a / count * 1e6,
            self.min_a * 1e6,
            self.max_a * 1e6
        );
        println!(
            "VPP_INPUT: average {:.4} V, min {:.4} V",
            self.sum_vin / count,
            self.min_vin
        );
        println!(
            "Gaps: {}, frames dropped by the device: {dropped_frames}, damaged on the wire: {damaged_frames}",
            self.gaps
        );
    }
}
