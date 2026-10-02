//! Minimal SNTP client (RFC 4330) for setting the device clock.
//!
//! The device clock comes from an NTP server rather than this computer's
//! clock, which may be wrong. All times are UTC milliseconds since the Unix
//! epoch; local time zones are only applied when times are displayed.
use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    io::{self, ErrorKind},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

/// Server used when none is given.
pub const DEFAULT_SERVER: &str = "pool.ntp.org";

const NTP_PORT: u16 = 123;
const PACKET_LEN: usize = 48;
/// LI 0 (no warning), version 4, mode 3 (client).
const CLIENT_HEADER: u8 = 0x23;
const MODE_SERVER: u8 = 4;
const LEAP_UNSYNCHRONISED: u8 = 3;
/// Seconds from 1900-01-01 (NTP era 0) to 1970-01-01.
const UNIX_EPOCH_IN_NTP: u64 = 2_208_988_800;

/// UTC time from an NTP server, kept running with this computer's monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct NtpTime {
    utc_ms: u64,
    at: Instant,
}

impl NtpTime {
    /// Current UTC time in milliseconds since the Unix epoch.
    pub fn now_ms(&self) -> u64 {
        self.utc_ms + self.at.elapsed().as_millis() as u64
    }
}

/// Asks `server` for the time, trying each of its addresses in turn.
pub fn query(server: &str, timeout: Duration) -> io::Result<NtpTime> {
    let mut last_error = io::Error::new(ErrorKind::NotFound, "no address");
    for addr in (server, NTP_PORT).to_socket_addrs()? {
        match query_addr(addr, timeout) {
            Ok(time) => return Ok(time),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn query_addr(addr: SocketAddr, timeout: Duration) -> io::Result<NtpTime> {
    let local: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(local)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.connect(addr)?;

    // The server echoes the transmit timestamp; a random one identifies the reply.
    let nonce = RandomState::new().build_hasher().finish();
    let mut request = [0u8; PACKET_LEN];
    request[0] = CLIENT_HEADER;
    request[40..48].copy_from_slice(&nonce.to_be_bytes());

    let sent = Instant::now();
    socket.send(&request)?;
    let mut response = [0u8; PACKET_LEN];
    let len = socket.recv(&mut response)?;
    let at = Instant::now();
    let utc_ms = parse_response(&response[..len], nonce, at - sent)?;
    Ok(NtpTime { utc_ms, at })
}

/// UTC milliseconds when the reply arrived, from a server reply and the round trip.
fn parse_response(packet: &[u8], nonce: u64, round_trip: Duration) -> io::Result<u64> {
    if packet.len() < PACKET_LEN {
        return Err(invalid("short reply"));
    }
    if packet[0] & 0x07 != MODE_SERVER {
        return Err(invalid("not a server reply"));
    }
    if packet[0] >> 6 == LEAP_UNSYNCHRONISED || !(1..=15).contains(&packet[1]) {
        return Err(invalid("server is not synchronised"));
    }
    if timestamp(packet, 24) != nonce {
        return Err(invalid("reply does not match the request"));
    }

    // Half of the network delay passed after the server sent its reply.
    let received = ntp_to_unix_ms(timestamp(packet, 32));
    let transmitted = ntp_to_unix_ms(timestamp(packet, 40));
    let held_ms = transmitted.saturating_sub(received);
    let delay_ms = (round_trip.as_millis() as u64).saturating_sub(held_ms);
    Ok(transmitted + delay_ms / 2)
}

fn timestamp(packet: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(packet[offset..offset + 8].try_into().unwrap())
}

/// Converts an NTP timestamp to Unix milliseconds; valid from 1968 to 2104.
fn ntp_to_unix_ms(timestamp: u64) -> u64 {
    let seconds = timestamp >> 32;
    let fraction = timestamp & 0xffff_ffff;
    // Era 1 starts in 2036; small values there are after the rollover.
    let era = if seconds < 0x8000_0000 { 1 << 32 } else { 0 };
    (seconds + era - UNIX_EPOCH_IN_NTP) * 1000 + ((fraction * 1000) >> 32)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: u64 = 0x0123_4567_89ab_cdef;

    /// 2026-10-02 00:00:00 UTC as NTP seconds.
    const SECONDS: u64 = 1_790_899_200 + UNIX_EPOCH_IN_NTP;

    fn reply(received: u64, transmitted: u64) -> [u8; PACKET_LEN] {
        let mut packet = [0u8; PACKET_LEN];
        packet[0] = (4 << 3) | MODE_SERVER;
        packet[1] = 2;
        packet[24..32].copy_from_slice(&NONCE.to_be_bytes());
        packet[32..40].copy_from_slice(&received.to_be_bytes());
        packet[40..48].copy_from_slice(&transmitted.to_be_bytes());
        packet
    }

    #[test]
    fn converts_ntp_timestamps() {
        assert_eq!(ntp_to_unix_ms(UNIX_EPOCH_IN_NTP << 32), 0);
        assert_eq!(
            ntp_to_unix_ms((SECONDS << 32) | 0x8000_0000),
            1_790_899_200_500
        );
        // 2036-02-07 06:28:16 UTC, the first second of era 1.
        assert_eq!(ntp_to_unix_ms(0), 2_085_978_496_000);
    }

    #[test]
    fn adds_half_the_network_delay() {
        // 100 ms round trip, of which the server held the request for 20 ms.
        let packet = reply(SECONDS << 32, (SECONDS << 32) + (20 << 32) / 1000 + 1);
        let utc_ms = parse_response(&packet, NONCE, Duration::from_millis(100)).unwrap();
        assert_eq!(utc_ms, 1_790_899_200_000 + 20 + 40);
    }

    #[test]
    fn rejects_bad_replies() {
        let good = reply(SECONDS << 32, SECONDS << 32);
        let round_trip = Duration::from_millis(10);
        assert!(parse_response(&good, NONCE, round_trip).is_ok());
        assert!(parse_response(&good[..40], NONCE, round_trip).is_err());
        assert!(parse_response(&good, NONCE + 1, round_trip).is_err());

        let mut kiss_of_death = good;
        kiss_of_death[1] = 0;
        assert!(parse_response(&kiss_of_death, NONCE, round_trip).is_err());

        let mut unsynchronised = good;
        unsynchronised[0] |= LEAP_UNSYNCHRONISED << 6;
        assert!(parse_response(&unsynchronised, NONCE, round_trip).is_err());

        let mut client = good;
        client[0] = CLIENT_HEADER;
        assert!(parse_response(&client, NONCE, round_trip).is_err());
    }
}
