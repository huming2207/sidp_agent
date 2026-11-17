//! Login checks against a real Soul Injector on WiFi, through `sidp-agent
//! serve` (or the desktop app) on this computer:
//!
//!   SIDP_WS_DEVICE=127.0.0.1:8080/7CDFA1B2C3D4 cargo test --test network -- --ignored
use prost::Message;
use sidp_agent::{
    codec::{Frame, REQUEST, RESPONSE},
    login::{
        self,
        messages::{Command0, Sec2Payload, SessionData},
    },
    transport::{Transport, WebSocket},
};
use std::time::{Duration, Instant};

const LOGIN: u16 = 0x0150;
const DEVICE_INFO: u16 = 0x0100;
const STATUS_OK: i32 = 0;
const STATUS_UNAUTHORIZED: i32 = 31;

fn connect() -> WebSocket {
    let target = std::env::var("SIDP_WS_DEVICE").expect("SIDP_WS_DEVICE=HOST:PORT/DEVICE_ID");
    let (address, id) = target.split_once('/').expect("HOST:PORT/DEVICE_ID");
    WebSocket::connect(address, id).unwrap()
}

/// Sends one request and returns the response's status.
fn status(ws: &mut WebSocket, id: u32, opcode: u16, payload: Vec<u8>) -> i32 {
    let request = Frame {
        kind: REQUEST,
        opcode,
        id,
        payload,
    };
    ws.send(&request, Duration::from_secs(2)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(frame) = ws.receive(Duration::from_millis(200)).unwrap()
            && frame.kind == RESPONSE
            && frame.id == id
        {
            return i32::from_le_bytes(frame.payload[..4].try_into().unwrap());
        }
    }
    panic!("no response to request {id}");
}

/// Command 0 of the Security 2 handshake carries no proof: anyone can send it.
fn command_0() -> Vec<u8> {
    let client = login::Client::new("not the password").unwrap();
    SessionData {
        sec_ver: 2,
        sec2: Some(Sec2Payload {
            msg: 0,
            sc0: Some(Command0 {
                client_username: login::USERNAME.as_bytes().to_vec(),
                client_pubkey: client.public_key().to_vec(),
            }),
            ..Default::default()
        }),
    }
    .encode_to_vec()
}

/// ESP-IDF's handler accepts command 0 again by restarting the handshake. Two
/// accepted login messages must not count as a login: only command 1, which
/// checks the proof, does.
#[test]
#[ignore = "needs a Soul Injector on WiFi: SIDP_WS_DEVICE=HOST:PORT/DEVICE_ID"]
fn command_0_twice_does_not_log_in() {
    let mut ws = connect();
    assert_eq!(
        status(&mut ws, 1, DEVICE_INFO, Vec::new()),
        STATUS_UNAUTHORIZED
    );
    assert_eq!(status(&mut ws, 2, LOGIN, command_0()), STATUS_OK);
    assert_eq!(status(&mut ws, 3, LOGIN, command_0()), STATUS_OK);
    assert_eq!(
        status(&mut ws, 4, DEVICE_INFO, Vec::new()),
        STATUS_UNAUTHORIZED,
        "logged in without the password"
    );
}
