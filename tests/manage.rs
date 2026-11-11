//! Management client against scripted device responses.
use prost::Message;
use sidp_agent::codec::{Frame, RESPONSE};
use sidp_agent::manage::proto::{
    AssetBeginRequest, AssetBeginResponse, BootRecord, JobRunOnceResponse, JobSetRequest,
    JobStatus, LogEntry, LogEntryType, LogReadRequest, LogReadResponse, Outcome, RunRecord,
    RunResult, Stage, Trigger,
};
use sidp_agent::manage::{self, MAX_CHUNK, STAGED_JOB_NAME};
use sidp_agent::{session::Session, transport::Transport};
use std::{cell::RefCell, collections::VecDeque, io, rc::Rc, time::Duration};

#[derive(Default)]
struct Wire {
    sent: Vec<Frame>,
    incoming: VecDeque<Frame>,
}
struct Script(Rc<RefCell<Wire>>);
impl Transport for Script {
    fn send(&mut self, frame: &Frame, _: Duration) -> io::Result<()> {
        self.0.borrow_mut().sent.push(frame.clone());
        Ok(())
    }
    fn receive(&mut self, _: Duration) -> io::Result<Option<Frame>> {
        Ok(self.0.borrow_mut().incoming.pop_front())
    }
    fn close(&mut self) {}
}

/// Scripts OK responses to requests numbered from ID 1.
fn setup(responses: &[(u16, Vec<u8>)]) -> (Session<Script>, Rc<RefCell<Wire>>) {
    let wire = Rc::new(RefCell::new(Wire::default()));
    for (index, (opcode, body)) in responses.iter().enumerate() {
        let mut payload = vec![0; 4];
        payload.extend(body);
        wire.borrow_mut().incoming.push_back(Frame {
            kind: RESPONSE,
            opcode: *opcode,
            id: index as u32 + 1,
            payload,
        });
    }
    let session = Session::new(Script(wire.clone()), Duration::from_millis(20));
    (session, wire)
}

fn begin_response(present: bool, offset: u32) -> Vec<u8> {
    AssetBeginResponse { present, offset }.encode_to_vec()
}

fn write_offset(frame: &Frame) -> u32 {
    u32::from_le_bytes(frame.payload[..4].try_into().unwrap())
}

fn job_status(sha256: Option<[u8; 32]>, last_run: u32) -> Vec<u8> {
    JobStatus {
        has_job: sha256.is_some(),
        sha256: sha256.map(|h| h.to_vec()).unwrap_or_default(),
        last_result: Some(RunResult {
            run_id: last_run,
            outcome: Outcome::Pass as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

#[test]
fn upload_resumes_at_device_offset_in_frame_sized_chunks() {
    let data = vec![0xa5; MAX_CHUNK * 2 + 10];
    let (mut session, wire) = setup(&[
        (manage::ASSET_BEGIN, begin_response(false, MAX_CHUNK as u32)),
        (manage::ASSET_WRITE, Vec::new()),
        (manage::ASSET_WRITE, Vec::new()),
        (manage::ASSET_COMMIT, Vec::new()),
    ]);
    assert!(manage::push_asset(&mut session, "firmware.bin", &data).unwrap());

    let wire = wire.borrow();
    let begin = AssetBeginRequest::decode(wire.sent[0].payload.as_slice()).unwrap();
    assert_eq!(begin.name, "firmware.bin");
    assert_eq!(begin.size as usize, data.len());
    assert_eq!(begin.sha256, manage::sha256(&data));
    assert_eq!(write_offset(&wire.sent[1]) as usize, MAX_CHUNK);
    assert_eq!(wire.sent[1].payload.len(), 4 + MAX_CHUNK);
    assert_eq!(write_offset(&wire.sent[2]) as usize, MAX_CHUNK * 2);
    assert_eq!(wire.sent[2].payload.len(), 4 + 10);
    assert_eq!(wire.sent[3].opcode, manage::ASSET_COMMIT);
    assert!(wire.sent.iter().all(|f| f.encode().unwrap().len() <= 8192));
}

#[test]
fn present_asset_is_not_sent_again() {
    let (mut session, wire) = setup(&[(manage::ASSET_BEGIN, begin_response(true, 3))]);
    assert!(!manage::push_asset(&mut session, "firmware.bin", b"abc").unwrap());
    assert_eq!(wire.borrow().sent.len(), 1);
}

#[test]
fn active_job_is_only_re_armed() {
    let job = b"compiled job".to_vec();
    let images = vec![("firmware.bin".to_owned(), b"fw".to_vec())];
    let (mut session, wire) = setup(&[
        (manage::ASSET_BEGIN, begin_response(true, 2)),
        (manage::JOB_GET, job_status(Some(manage::sha256(&job)), 0)),
        (manage::JOB_SET, Vec::new()),
    ]);
    manage::push_job(&mut session, &job, &images, Trigger::AutoOnDetect).unwrap();

    let wire = wire.borrow();
    let set = JobSetRequest::decode(wire.sent[2].payload.as_slice()).unwrap();
    assert_eq!(set.sha256, manage::sha256(&job));
    assert_eq!(set.trigger, Trigger::AutoOnDetect as i32);
}

#[test]
fn new_job_is_staged_before_activation() {
    let job = b"compiled job".to_vec();
    let (mut session, wire) = setup(&[
        (manage::JOB_GET, job_status(None, 0)),
        (manage::ASSET_BEGIN, begin_response(false, 0)),
        (manage::ASSET_WRITE, Vec::new()),
        (manage::ASSET_COMMIT, Vec::new()),
        (manage::JOB_SET, Vec::new()),
    ]);
    manage::push_job(&mut session, &job, &[], Trigger::Manual).unwrap();

    let wire = wire.borrow();
    let begin = AssetBeginRequest::decode(wire.sent[1].payload.as_slice()).unwrap();
    assert_eq!(begin.name, STAGED_JOB_NAME);
    assert_eq!(wire.sent[4].opcode, manage::JOB_SET);
}

#[test]
fn run_result_is_matched_by_run_id() {
    let (mut session, _) = setup(&[
        (
            manage::JOB_RUN_ONCE,
            JobRunOnceResponse { run_id: 7 }.encode_to_vec(),
        ),
        (manage::JOB_GET, job_status(None, 6)),
        (manage::JOB_GET, job_status(None, 7)),
    ]);
    let run_id = session.job_run_once().unwrap();
    let result = manage::wait_for_run(&mut session, run_id, Duration::ZERO).unwrap();
    assert_eq!(result.run_id, 7);
}

#[test]
fn run_is_not_finished_while_the_device_is_still_in_it() {
    let recording = JobStatus {
        current_run_id: 7,
        last_result: Some(RunResult {
            run_id: 7,
            outcome: Outcome::Pass as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let (mut session, wire) = setup(&[
        (manage::JOB_GET, recording.encode_to_vec()),
        (manage::JOB_GET, job_status(None, 7)),
    ]);
    manage::wait_for_run(&mut session, 7, Duration::ZERO).unwrap();
    assert_eq!(wire.borrow().sent.len(), 2);
}

#[test]
fn asset_names_follow_device_rules() {
    assert!(manage::is_valid_asset_name("firmware-v1.2_a.bin"));
    let too_long = "a".repeat(32);
    let invalid = [
        "",
        ".hidden",
        "a/b",
        "x.part",
        "JOB.PB",
        "job.pb.",
        "Firmware.bin",
        "firmwa~1.bin",
        &too_long,
    ];
    for name in invalid {
        assert!(!manage::is_valid_asset_name(name), "{name}");
    }
}

fn log_entry(id: u64, entry_type: LogEntryType, record: Vec<u8>) -> LogEntry {
    LogEntry {
        id,
        uptime_us: 1500,
        utc_ms: 0,
        r#type: entry_type as i32,
        record,
    }
}

fn log_response(entries: Vec<LogEntry>) -> Vec<u8> {
    LogReadResponse { entries }.encode_to_vec()
}

#[test]
fn log_is_read_from_the_last_id_until_empty() {
    let boot = |id| log_entry(id, LogEntryType::LogEntryBoot, Vec::new());
    let (mut session, wire) = setup(&[
        (manage::LOG_READ, log_response(vec![boot(5), boot(6)])),
        (manage::LOG_READ, log_response(vec![boot(9)])),
        (manage::LOG_READ, log_response(Vec::new())),
    ]);
    let entries = manage::read_log(&mut session, 4).unwrap();

    let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
    assert_eq!(ids, [5, 6, 9]);
    let after: Vec<u64> = wire
        .borrow()
        .sent
        .iter()
        .map(|f| {
            LogReadRequest::decode(f.payload.as_slice())
                .unwrap()
                .after_id
        })
        .collect();
    assert_eq!(after, [4, 6, 9]);
}

#[test]
fn run_entry_becomes_one_json_object() {
    let run = RunRecord {
        result: Some(RunResult {
            run_id: 3,
            outcome: Outcome::Fail as i32,
            failed_stage: Stage::Verify as i32,
            duration_ms: 1200,
        }),
        trigger: Trigger::AutoOnDetect as i32,
        job_name: "widget".to_owned(),
        job_sha256: vec![0xab; 32],
        error_code: 0x105,
    };
    let mut entry = log_entry(
        (2 << 40) | 7,
        LogEntryType::LogEntryRun,
        run.encode_to_vec(),
    );
    entry.utc_ms = 1_800_000_000_000;

    let json = manage::log_entry_json("sn1", &entry);
    assert_eq!(json["device"], "sn1");
    assert_eq!(json["boot"], 2);
    assert_eq!(json["utc_ms"], 1_800_000_000_000u64);
    assert_eq!(json["type"], "run");
    assert_eq!(json["outcome"], "OUTCOME_FAIL");
    assert_eq!(json["failed_stage"], "STAGE_VERIFY");
    assert_eq!(json["trigger"], "TRIGGER_AUTO_ON_DETECT");
    assert_eq!(json["job_sha256"], "ab".repeat(32));
    assert_eq!(json["error_code"], 0x105);
}

#[test]
fn boot_and_unknown_entries_keep_their_ids() {
    let boot = BootRecord { reset_reason: 4 }.encode_to_vec();
    let json = manage::log_entry_json("sn1", &log_entry(1, LogEntryType::LogEntryBoot, boot));
    assert_eq!(json["type"], "boot");
    assert_eq!(json["reset_reason"], 4);
    assert!(json["utc_ms"].is_null());

    let json = manage::log_entry_json("sn1", &log_entry(2, LogEntryType::LogEntryUnknown, vec![]));
    assert_eq!(json["type"], "unknown");
    assert_eq!(json["id"], 2);
}
