//! Power recording on the worker's own USB connection, alone or alongside
//! a debug session.
use super::{DeviceSession, Reporter, Step, Stop};
use crate::power::{CaptureEvent, DeviceFeed, Resampler};
use sidp_agent::{
    power::{self, Converter, Message, StartRequest, StopReason},
    session,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Status a device without a profiler answers POWER_INFO with.
const STATUS_UNSUPPORTED: i32 = 3;
/// How long zeroing waits for each range to settle, then averages it.
const ZERO_SETTLE: Duration = Duration::from_secs(1);
const ZERO_MEASURE: Duration = Duration::from_secs(2);

/// Where a feed's samples are on its recording's time line. Shared by every
/// connection that records into the feed, so a debug session that reconnects
/// carries on where the last connection stopped.
pub(super) struct Timeline {
    origin: Instant,
    resampler: Resampler,
}

impl Timeline {
    pub(super) fn new(feed: &DeviceFeed) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            origin: Instant::now(),
            resampler: Resampler::new(feed.rate),
        }))
    }
}

/// Turns one connection's power messages into recording samples.
struct Writer {
    feed: DeviceFeed,
    converter: Converter,
    /// Seconds from the feed's start to this connection's measurement start.
    offset_s: f64,
    timeline: Arc<Mutex<Timeline>>,
    device: Vec<power::Sample>,
    ended: bool,
}

impl Writer {
    fn handle(&mut self, message: Message) {
        if self.ended {
            return;
        }
        if self.feed.stopped() {
            self.end(Ok(()));
            return;
        }
        match message {
            Message::Data(frame) => {
                self.device.clear();
                self.converter.convert(&frame, &mut self.device);
                for sample in &mut self.device {
                    sample.t_s += self.offset_s;
                }
                let mut samples = Vec::with_capacity(self.device.len());
                self.timeline
                    .lock()
                    .unwrap()
                    .resampler
                    .push(&self.device, &mut samples);
                if !samples.is_empty()
                    && self
                        .feed
                        .events
                        .send(CaptureEvent::Samples(samples))
                        .is_err()
                {
                    self.ended = true;
                }
            }
            Message::Stopped(stopped) => {
                let reason = stopped.reason();
                self.end(if reason == StopReason::StopDuration {
                    Ok(())
                } else {
                    Err(format!(
                        "Power measurement stopped: {}.",
                        power::describe_stop(reason)
                    ))
                });
            }
        }
    }

    fn end(&mut self, result: Result<(), String>) {
        self.ended = true;
        let _ = self.feed.events.send(CaptureEvent::Ended(result));
    }
}

/// Starts measuring on `session`, with the data going into `feed`.
/// Returns whether the measurement has ended, for callers that wait on it.
pub(super) fn start(
    session: &mut DeviceSession,
    feed: &DeviceFeed,
    timeline: &Arc<Mutex<Timeline>>,
    r: &Reporter,
) -> Step<Arc<Mutex<bool>>> {
    r.step("Starting the power measurement");
    let info = session.power_info().map_err(unsupported)?;
    let request = StartRequest {
        conversion_us: feed.setting.conversion_us,
        averages: feed.setting.averages,
        lowest_range: power::Range::Low as i32,
        duration_ms: feed
            .duration
            .map_or(0, |d| d.as_millis().clamp(1, u128::from(u32::MAX)) as u32),
    };
    power::check_start(&info, &request).map_err(Stop::Failed)?;
    let started = session.power_start(&request)?;
    let converter = Converter::new(&info, feed.zero, &started).map_err(|e| {
        Stop::Failed(format!(
            "The Soul Injector described its profiler wrongly: {e}"
        ))
    })?;

    let offset_s = timeline.lock().unwrap().origin.elapsed().as_secs_f64();
    let ended = Arc::new(Mutex::new(false));
    let mut writer = Writer {
        feed: feed.clone(),
        converter,
        offset_s,
        timeline: timeline.clone(),
        device: Vec::new(),
        ended: false,
    };
    let flag = ended.clone();
    session.set_power_sink(Some(Box::new(move |message| {
        writer.handle(message);
        *flag.lock().unwrap() = writer.ended;
    })));
    r.info(format!(
        "Measuring power, one sample every {} µs",
        started.sample_period_us
    ));
    Ok(ended)
}

/// Records into `feed` until the user stops, the duration elapses or the
/// device stops on its own.
pub(super) fn record(session: &mut DeviceSession, feed: &DeviceFeed, r: &Reporter) -> Step<String> {
    let timeline = Timeline::new(feed);
    let ended = start(session, feed, &timeline, r)?;
    r.step("Measuring power");
    while !r.cancelled() && !feed.stopped() && !*ended.lock().unwrap() {
        session.poll(Duration::from_millis(50))?;
    }
    if !*ended.lock().unwrap() {
        session.power_stop()?;
    }
    Ok("Power recording finished".into())
}

/// Measures the zero correction; nothing may be connected to VPP_OUTPUT.
pub(super) fn measure_zero(session: &mut DeviceSession, r: &Reporter) -> Step<power::Zero> {
    r.step("Measuring the zero offset of each range");
    let info = session.power_info().map_err(unsupported)?;
    Ok(session.power_measure_zero(&info, ZERO_SETTLE, ZERO_MEASURE)?)
}

fn unsupported(error: session::Error) -> Stop {
    match error {
        session::Error::Status(STATUS_UNSUPPORTED) => {
            Stop::Failed("This Soul Injector cannot measure power.".into())
        }
        other => other.into(),
    }
}
