//! Power recording through the device worker, against a real Soul Injector
//! with a power profiler (Rev 8) and a target attached. Run with:
//!
//! ```sh
//! SIDP_DEVICE=/dev/ttyACM0 cargo test --test hardware -- --ignored --test-threads 1
//! ```
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use sidp_desktop::{
    power::{CaptureEvent, DeviceFeed, DeviceSetting, Recording},
    worker::{Connection, DebugPlan, Event, Operation, Outcome, Worker},
};
use std::{
    sync::{Arc, atomic::AtomicBool, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

fn connection() -> Connection {
    Connection {
        port: std::env::var("SIDP_DEVICE").expect("set SIDP_DEVICE to the Soul Injector's port"),
        timeout: Duration::from_secs(5),
        settle: Duration::from_millis(250),
        ntp_server: String::new(),
        dtr: true,
        password: None,
    }
}

fn feed(rate: u32, duration: Option<Duration>) -> (DeviceFeed, mpsc::Receiver<CaptureEvent>) {
    let (sender, receiver) = mpsc::channel();
    let feed = DeviceFeed {
        rate,
        setting: DeviceSetting::for_rate(rate),
        duration,
        zero: Default::default(),
        events: sender,
        stop: Arc::new(AtomicBool::new(false)),
    };
    (feed, receiver)
}

/// Collects samples until the capture ends or `limit` passes.
fn collect(
    receiver: &mpsc::Receiver<CaptureEvent>,
    rate: u32,
    limit: Duration,
) -> (Recording, Option<Result<(), String>>) {
    let mut recording = Recording::new(rate);
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(CaptureEvent::Samples(samples)) => recording.extend(&samples),
            Ok(CaptureEvent::Ended(result)) => return (recording, Some(result)),
            Err(_) => {}
        }
    }
    (recording, None)
}

fn finished(events: &mut UnboundedReceiver<Event>) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(Event::Finished(outcome)) => return outcome,
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("the operation did not finish");
}

#[test]
#[ignore = "needs a Soul Injector at SIDP_DEVICE"]
fn records_for_the_requested_time() {
    let (feed, receiver) = feed(909, Some(Duration::from_secs(2)));
    let (sender, mut events) = unbounded();
    let _worker = Worker::start(connection(), Operation::Power(feed), sender);

    let (recording, ended) = collect(&receiver, 909, Duration::from_secs(10));
    assert_eq!(ended, Some(Ok(())));
    assert!(matches!(finished(&mut events), Outcome::Done(_)));
    // The grid follows device time: 2 s at 909 per second.
    let expected = 2.0 * 909.0;
    assert!(
        (recording.len() as f64 - expected).abs() < expected * 0.02,
        "{} samples",
        recording.len()
    );
    let last = recording.last().unwrap();
    assert!(last.supply > 1.7 && last.supply < 3.7, "{last:?}");
    eprintln!("{} samples, last {last:?}", recording.len());
}

#[test]
#[ignore = "needs a Soul Injector at SIDP_DEVICE"]
fn measures_power_on_the_debug_connection() {
    let (feed, receiver) = feed(909, None);
    let stop = feed.stop.clone();
    let plan = DebugPlan {
        program: None,
        port: 0,
        connect_under_reset: false,
        resume_on_detach: false,
        keep_serving: false,
        power: Some(feed),
    };
    let (sender, mut events) = unbounded();
    let worker = Worker::start(connection(), Operation::Debug(plan), sender);

    // Samples flow while the server waits for GDB on the same connection.
    let (recording, ended) = collect(&receiver, 909, Duration::from_secs(3));
    assert_eq!(ended, None, "the recording ended early");
    assert!(recording.len() > 1000, "{} samples", recording.len());
    eprintln!("{} samples while debugging", recording.len());

    stop.store(true, Ordering::SeqCst);
    worker.cancel();
    assert!(matches!(finished(&mut events), Outcome::Done(_)));
}
