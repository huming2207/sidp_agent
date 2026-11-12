//! Power service requests (opcodes 0x0200 and up).
//!
//! Measurement data arrives outside any request; see [`Session::set_power_sink`].
use super::*;
use crate::power::{
    Converter, DataFrame, POWER_INFO, POWER_START, POWER_STOP, PowerInfo, Range, StartRequest,
    StartResponse, StopResponse, Zero,
};
use prost::Message as _;
use std::sync::mpsc;

/// Settings used to measure the zero: 1.1 ms conversions averaged 4 times.
const ZERO_CONVERSION_US: u32 = 1100;
const ZERO_AVERAGES: u32 = 4;
/// Largest believable open-output current per range; more means something is connected.
const ZERO_LIMIT_A: [f64; power::RANGE_COUNT] = [30e-6, 60e-6, 300e-6];
const ZERO_MIN_SAMPLES: usize = 20;

impl<T: Transport> Session<T> {
    /// Ranges, conversion constants and accepted settings of the device's profiler.
    pub fn power_info(&mut self) -> Result<PowerInfo> {
        let payload = self.request(POWER_INFO, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Starts measuring; data then arrives at the power sink.
    pub fn power_start(&mut self, request: &StartRequest) -> Result<StartResponse> {
        let payload = self.request(POWER_START, request.encode_to_vec())?;
        self.decode_response(&payload)
    }

    /// Stops measuring and returns the totals. Data frames sent before the
    /// stop may still arrive afterwards.
    pub fn power_stop(&mut self) -> Result<StopResponse> {
        let payload = self.request(POWER_STOP, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Measures the zero correction. Nothing may be connected to VPP_OUTPUT.
    ///
    /// Each range is held in turn with `lowest_range`; after `settle`, its
    /// samples for `measure` are averaged and divided by VPP_INPUT. The power
    /// sink is replaced while this runs and restored afterwards.
    pub fn power_measure_zero(
        &mut self,
        info: &PowerInfo,
        settle: Duration,
        measure: Duration,
    ) -> Result<Zero> {
        let previous = self.power_sink.take();
        let result = self.measure_zero_ranges(info, settle, measure);
        self.power_sink = previous;
        result
    }

    fn measure_zero_ranges(
        &mut self,
        info: &PowerInfo,
        settle: Duration,
        measure: Duration,
    ) -> Result<Zero> {
        let (sender, receiver) = mpsc::channel();
        self.power_sink = Some(Box::new(move |message| {
            if let power::Message::Data(frame) = message {
                let _ = sender.send(frame);
            }
        }));

        let mut zero = Zero::default();
        for range in [Range::Low, Range::Mid, Range::High] {
            let request = StartRequest {
                conversion_us: ZERO_CONVERSION_US,
                averages: ZERO_AVERAGES,
                lowest_range: range as i32,
                duration_ms: 0,
            };
            let start = self.power_start(&request)?;
            let deadline = Instant::now() + settle + measure;
            while Instant::now() < deadline {
                self.poll(Duration::from_millis(50))?;
            }
            self.power_stop()?;

            let frames: Vec<DataFrame> = receiver.try_iter().collect();
            zero.amps_per_volt[range as usize] =
                average_zero(info, &start, &frames, range, settle)?;
        }
        Ok(zero)
    }
}

/// Open-output current per volt of VPP_INPUT in `range`, from its samples after `settle`.
fn average_zero(
    info: &PowerInfo,
    start: &StartResponse,
    frames: &[DataFrame],
    range: Range,
    settle: Duration,
) -> Result<f64> {
    let mut converter = Converter::new(info, Zero::default(), start).map_err(Error::Io)?;
    let mut samples = Vec::new();
    for frame in frames {
        converter.convert(frame, &mut samples);
    }
    let held: Vec<_> = samples
        .iter()
        .filter(|s| s.range == range && !s.settling && s.t_s >= settle.as_secs_f64())
        .collect();
    if held.len() < ZERO_MIN_SAMPLES {
        return Err(Error::State(
            "the profiler did not hold the range; is something connected to VPP_OUTPUT (PROG pin 9)?",
        ));
    }

    let count = held.len() as f64;
    let current = held.iter().map(|s| s.current_a).sum::<f64>() / count;
    let vin = held.iter().map(|s| s.vin_v).sum::<f64>() / count;
    if current.abs() > ZERO_LIMIT_A[range as usize] {
        return Err(Error::State(
            "too much current to be a zero; disconnect everything from VPP_OUTPUT (PROG pin 9)",
        ));
    }
    if vin <= 0.0 {
        return Err(Error::State("no supply on VPP_INPUT (PROG pin 11)"));
    }
    Ok(current / vin)
}
