//! Management client against scripted device responses.
use prost::Message;
use sidp_agent::codec::{Frame, RESPONSE};
use sidp_agent::manage::proto::{
    AssetBeginRequest, AssetBeginResponse, JobRunOnceResponse, JobSetRequest, JobStatus, Outcome,
    RunResult, Trigger,
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
fn asset_names_follow_device_rules() {
    assert!(manage::is_valid_asset_name("firmware-v1.2_a.bin"));
    for name in ["", ".hidden", "a/b", "x.part", "x.sha256", &"a".repeat(32)] {
        assert!(!manage::is_valid_asset_name(name), "{name}");
    }
}
