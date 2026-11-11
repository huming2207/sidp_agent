//! Session regressions using scripted complete-frame transport boundaries.
use sidp_agent::{
    codec::{EVENT, Frame, RESPONSE},
    session::{CAP_HW_BP, CAP_STEP, ResetKind, Session},
    transport::Transport,
};
use std::{cell::RefCell, collections::VecDeque, io, rc::Rc, time::Duration};

#[derive(Default)]
struct Wire {
    sent: Vec<Frame>,
    incoming: VecDeque<Frame>,
    closed: bool,
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
    fn close(&mut self) {
        self.0.borrow_mut().closed = true;
    }
}
fn response(opcode: u16, id: u32, body: &[u8]) -> Frame {
    let mut payload = vec![0; 4];
    payload.extend(body);
    Frame {
        kind: RESPONSE,
        opcode,
        id,
        payload,
    }
}
fn stopped(id: u32, flags: bool) -> Frame {
    let mut p = id.to_le_bytes().to_vec();
    p.extend([0, 0, 4, 0]);
    p.extend([0; 16]);
    p.extend(2u16.to_le_bytes());
    p.extend(4u16.to_le_bytes());
    p.extend(0x20000100u64.to_le_bytes());
    p.extend([0, 0, 4, if flags { 1 } else { 0 }]);
    p.extend(7u32.to_le_bytes());
    p.extend([13, 0, 4, 0]);
    p.extend(0x20000100u32.to_le_bytes());
    p.extend([1, 2, 3, 4]);
    Frame {
        kind: EVENT,
        opcode: 0x80,
        id: 0,
        payload: p,
    }
}
fn attach_frames(flags: u8) -> [Frame; 2] {
    let mut info = vec![1, 3, 32, 0];
    info.extend((CAP_HW_BP | CAP_STEP | 1).to_le_bytes());
    info.extend([0; 12]);
    info.extend(2u16.to_le_bytes());
    info.extend(0u16.to_le_bytes());
    info.extend(1u16.to_le_bytes());
    info.extend(64u16.to_le_bytes());
    info.extend(0x20000000u64.to_le_bytes());
    info.extend(0x1000u64.to_le_bytes());
    info.extend([1, flags, 0, 0]);
    [response(1, 1, &info), stopped(1, false)]
}
fn unattached(flags: u8) -> (Session<Script>, Rc<RefCell<Wire>>) {
    let wire = Rc::new(RefCell::new(Wire::default()));
    wire.borrow_mut().incoming.extend(attach_frames(flags));
    let session = Session::new(Script(wire.clone()), Duration::from_millis(20));
    (session, wire)
}
fn setup(flags: u8) -> (Session<Script>, Rc<RefCell<Wire>>) {
    let (mut session, wire) = unattached(flags);
    session.attach().unwrap();
    (session, wire)
}
#[test]
fn attach_requests_connect_under_reset_only_when_asked() {
    let (_session, wire) = setup(15);
    assert_eq!(wire.borrow().sent[0].payload[4], 0);
    let (mut session, wire) = unattached(15);
    session.attach_under_reset().unwrap();
    assert_eq!(wire.borrow().sent[0].payload[4], 1);
}
#[test]
fn snapshot_hits_and_run_invalidation() {
    let (mut session, wire) = setup(15);
    assert_eq!(session.read_registers(&[0, 13]).unwrap()[&0], Some(7));
    let mut bytes = [0; 4];
    session.read_memory(0x20000100, &mut bytes).unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
    assert_eq!(session.request_count(), 1);
    session.add_breakpoint(0x20000000).unwrap();
    assert_eq!(wire.borrow().sent.len(), 1);
    wire.borrow_mut().incoming.push_back(response(0x20, 2, &[]));
    session.run(false).unwrap();
    assert!(session.stop().is_none());
    assert!(session.read_memory(0x20000100, &mut bytes).is_err());
    let req = wire.borrow().sent[1].clone();
    assert_eq!(req.payload.len(), 40);
    assert_eq!(&req.payload[20..24], &[1, 0, 0, 0]);
    wire.borrow_mut().incoming.push_back(stopped(2, true));
    session.poll(Duration::from_millis(1)).unwrap();
    assert_eq!(session.read_registers(&[0]).unwrap()[&0], None);
    assert_eq!(session.request_count(), 2);
}
#[test]
fn timeout_and_wrong_response_close_without_replay() {
    for wrong_id in [false, true] {
        let (mut session, wire) = setup(7);
        if wrong_id {
            wire.borrow_mut()
                .incoming
                .push_back(response(0x10, 99, &[]));
        }
        assert!(session.read_memory(0x20000000, &mut [0; 4]).is_err());
        assert!(session.is_closed());
        assert!(session.stop().is_none());
        assert!(wire.borrow().closed);
        assert!(session.attach().is_err());
        assert_eq!(wire.borrow().sent.len(), 2);
    }
}
#[test]
fn volatile_snapshot_is_never_used() {
    let (mut session, wire) = setup(31);
    let mut body = 0x20000100u64.to_le_bytes().to_vec();
    body.extend(4u32.to_le_bytes());
    body.extend([9; 4]);
    wire.borrow_mut()
        .incoming
        .push_back(response(0x10, 2, &body));
    let mut out = [0; 4];
    session.read_memory(0x20000100, &mut out).unwrap();
    assert_eq!(out, [9; 4]);
    assert_eq!(session.request_count(), 2);
}
#[test]
fn failed_write_invalidates_snapshot() {
    let (mut session, wire) = setup(15);
    let mut error = response(0x12, 2, &[]);
    error.payload = 21i32.to_le_bytes().to_vec();
    wire.borrow_mut().incoming.push_back(error);
    assert!(session.write_memory(0x20000100, &[8]).is_err());
    assert!(session.stop().unwrap().stack.is_empty());
    assert!(session.stop().unwrap().registers.is_empty());
}
#[test]
fn chunks_obey_advertised_limit() {
    let (mut session, wire) = setup(7);
    for (id, addr, len) in [(2, 0x20000000u64, 64u32), (3, 0x20000040, 6)] {
        let mut body = addr.to_le_bytes().to_vec();
        body.extend(len.to_le_bytes());
        body.extend(vec![0x5a; len as usize]);
        wire.borrow_mut()
            .incoming
            .push_back(response(0x10, id, &body));
    }
    let mut out = [0; 70];
    session.read_memory(0x20000000, &mut out).unwrap();
    assert_eq!(out, [0x5a; 70]);
    assert_eq!(session.request_count(), 3);
}
#[test]
fn stale_and_malformed_events_are_fatal() {
    for frame in [
        stopped(1, false),
        Frame {
            kind: EVENT,
            opcode: 0x80,
            id: 0,
            payload: vec![0; 4],
        },
    ] {
        let (mut session, wire) = setup(15);
        wire.borrow_mut().incoming.push_back(frame);
        assert!(session.poll(Duration::from_millis(1)).is_err());
        assert!(session.is_closed());
    }
}
#[test]
fn reset_halt_waits_for_the_new_stop() {
    let (mut session, wire) = setup(15);
    wire.borrow_mut()
        .incoming
        .extend([response(0x22, 2, &[]), stopped(2, false)]);
    session.reset_halt(ResetKind::Nrst).unwrap();
    assert_eq!(session.stop().unwrap().id, 2);
    assert_eq!(wire.borrow().sent[1].opcode, 0x22);
    assert_eq!(&wire.borrow().sent[1].payload, &[2, 0, 0, 0]);
}
#[test]
fn failed_reset_keeps_the_session_open() {
    let (mut session, wire) = setup(15);
    let mut unsupported = response(0x22, 2, &[]);
    unsupported.payload = 3i32.to_le_bytes().to_vec();
    wire.borrow_mut().incoming.push_back(unsupported);
    assert!(session.reset_halt(ResetKind::System).is_err());
    assert!(!session.is_closed());
    assert_eq!(session.stop().unwrap().id, 1);
}
#[test]
fn detach_closes_the_physical_connection() {
    let (mut session, wire) = setup(15);
    wire.borrow_mut().incoming.push_back(response(2, 2, &[]));
    session.detach(false).unwrap();
    assert!(session.is_closed());
    assert_eq!(&wire.borrow().sent[1].payload, &[1, 0, 0, 0]);
}

#[test]
fn target_running_response_discards_old_stop_cache() {
    let (mut session, wire) = setup(15);
    let mut running = response(0x10, 2, &[]);
    running.payload = 10i32.to_le_bytes().to_vec();
    wire.borrow_mut().incoming.push_back(running);
    assert!(session.read_memory(0x20000000, &mut [0; 4]).is_err());
    assert!(session.stop().is_none());
    assert!(session.read_registers(&[0]).is_err());
    assert!(!session.is_closed());
    // A later unsolicited stop can establish a new valid generation.
    wire.borrow_mut().incoming.push_back(stopped(2, false));
    session.poll(Duration::from_millis(1)).unwrap();
    assert_eq!(session.stop().unwrap().id, 2);
}
