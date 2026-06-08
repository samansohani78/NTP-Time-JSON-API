//! RFC 5905 NTP/SNTP packet codec.
//!
//! NTPv4 packet layout (48 bytes, big-endian):
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |LI | VN  |Mode |    Stratum     |     Poll      |  Precision   |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         Root Delay                            |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         Root Dispersion                       |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                          Reference ID                         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! +                     Reference Timestamp (64)                  +
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! +                      Origin Timestamp (64)                    +
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! +                      Receive Timestamp (64)                   +
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! +                      Transmit Timestamp (64)                  +
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! All timestamps are 64-bit unsigned fixed-point: the upper 32 bits are
//! seconds since 1900-01-01 00:00:00 UTC, the lower 32 bits are the
//! fractional part.

// Constants and helpers in this module are part of the public NTP
// protocol API; downstream users may reference any of the standard
// values even if the in-tree consumer does not.
#![allow(dead_code)]

use std::time::SystemTime;

/// Seconds from 1900-01-01T00:00:00Z to 1970-01-01T00:00:00Z (== 70 years).
pub const NTP_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;

/// Standard NTP packet size.
pub const NTP_PACKET_SIZE: usize = 48;

/// NTP version we speak (NTPv4, RFC 5905).
pub const NTP_VERSION: u8 = 4;

/// NTPv3 (still widely seen in the wild).
pub const NTP_VERSION_3: u8 = 3;

// --- Mode (3 bits) ---
pub const MODE_RESERVED: u8 = 0;
pub const MODE_SYMMETRIC_ACTIVE: u8 = 1;
pub const MODE_SYMMETRIC_PASSIVE: u8 = 2;
pub const MODE_CLIENT: u8 = 3;
pub const MODE_SERVER: u8 = 4;
pub const MODE_BROADCAST: u8 = 5;
pub const MODE_CONTROL: u8 = 6;

// --- Leap indicator (2 bits) ---
pub const LI_NO_WARNING: u8 = 0;
pub const LI_LAST_MINUTE_61: u8 = 1;
pub const LI_LAST_MINUTE_59: u8 = 2;
pub const LI_ALARM_UNSYNCHRONIZED: u8 = 3;

// --- Stratum ---
pub const STRATUM_UNSPECIFIED: u8 = 0;
pub const STRATUM_PRIMARY: u8 = 1;
pub const STRATUM_SECONDARY_MAX: u8 = 15; // RFC 5905 §3
pub const STRATUM_UNSYNCHRONIZED: u8 = 16; // "kiss-of-death" / unsynced

/// Errors that can occur when parsing an NTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Packet is shorter than the 48-byte NTP minimum.
    TooShort { received: usize, minimum: usize },
    /// First byte had an unsupported mode (we only respond to Mode 3 client).
    UnsupportedMode(u8),
    /// Version field was not 3 or 4.
    UnsupportedVersion(u8),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::TooShort { received, minimum } => write!(
                f,
                "NTP packet too short: got {received} bytes, need >= {minimum}"
            ),
            ProtocolError::UnsupportedMode(m) => write!(f, "unsupported NTP mode: {m}"),
            ProtocolError::UnsupportedVersion(v) => write!(f, "unsupported NTP version: {v}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A decoded NTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpPacket {
    pub li: u8,
    pub vn: u8,
    pub mode: u8,
    pub stratum: u8,
    pub poll: i8,
    pub precision: i8,
    pub root_delay: u32,
    pub root_dispersion: u32,
    pub reference_id: u32,
    pub ref_timestamp: u64,
    pub origin_timestamp: u64,
    pub receive_timestamp: u64,
    pub transmit_timestamp: u64,
}

impl NtpPacket {
    /// Build a zeroed packet with LI/VN/Mode set, other fields zeroed.
    pub const fn new(li: u8, vn: u8, mode: u8) -> Self {
        Self {
            li,
            vn,
            mode,
            stratum: 0,
            poll: 0,
            precision: 0,
            root_delay: 0,
            root_dispersion: 0,
            reference_id: 0,
            ref_timestamp: 0,
            origin_timestamp: 0,
            receive_timestamp: 0,
            transmit_timestamp: 0,
        }
    }

    /// Reference ID as 4 ASCII bytes (used for Stratum 2 servers, e.g.
    /// "LOCL", "NTP ", or a source server's IPv4 octets).
    pub fn reference_id_ascii(&self) -> [char; 4] {
        let b = self.reference_id.to_be_bytes();
        [b[0] as char, b[1] as char, b[2] as char, b[3] as char]
    }
}

/// Parse an NTP packet from raw bytes.
///
/// Accepts any buffer of length >= 48; only the first 48 bytes are
/// interpreted. Returns `UnsupportedMode` for non-client packets and
/// `UnsupportedVersion` for versions other than 3/4.
pub fn parse_packet(bytes: &[u8]) -> Result<NtpPacket, ProtocolError> {
    if bytes.len() < NTP_PACKET_SIZE {
        return Err(ProtocolError::TooShort {
            received: bytes.len(),
            minimum: NTP_PACKET_SIZE,
        });
    }
    let b0 = bytes[0];
    let li = b0 >> 6;
    let vn = (b0 >> 3) & 0x07;
    let mode = b0 & 0x07;

    if mode != MODE_CLIENT {
        return Err(ProtocolError::UnsupportedMode(mode));
    }
    if vn != NTP_VERSION && vn != NTP_VERSION_3 {
        return Err(ProtocolError::UnsupportedVersion(vn));
    }

    Ok(NtpPacket {
        li,
        vn,
        mode,
        stratum: bytes[1],
        poll: bytes[2] as i8,
        precision: bytes[3] as i8,
        root_delay: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        root_dispersion: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        reference_id: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        ref_timestamp: read_u64(&bytes[16..24]),
        origin_timestamp: read_u64(&bytes[24..32]),
        receive_timestamp: read_u64(&bytes[32..40]),
        transmit_timestamp: read_u64(&bytes[40..48]),
    })
}

/// Serialize a packet into a 48-byte buffer (big-endian wire format).
pub fn serialize_packet(packet: &NtpPacket) -> [u8; NTP_PACKET_SIZE] {
    let mut buf = [0u8; NTP_PACKET_SIZE];
    buf[0] = ((packet.li & 0x03) << 6) | ((packet.vn & 0x07) << 3) | (packet.mode & 0x07);
    buf[1] = packet.stratum;
    buf[2] = packet.poll as u8;
    buf[3] = packet.precision as u8;
    buf[4..8].copy_from_slice(&packet.root_delay.to_be_bytes());
    buf[8..12].copy_from_slice(&packet.root_dispersion.to_be_bytes());
    buf[12..16].copy_from_slice(&packet.reference_id.to_be_bytes());
    write_u64(&mut buf[16..24], packet.ref_timestamp);
    write_u64(&mut buf[24..32], packet.origin_timestamp);
    write_u64(&mut buf[32..40], packet.receive_timestamp);
    write_u64(&mut buf[40..48], packet.transmit_timestamp);
    buf
}

/// Convert a Unix epoch in milliseconds to an NTP 64-bit timestamp.
///
/// The fractional part is computed with a 1000 ms ↔ 2^32 fixed-point
/// conversion. The integer seconds part is shifted by
/// [`NTP_EPOCH_OFFSET_SECS`].
pub fn unix_ms_to_ntp(epoch_ms: i64) -> u64 {
    if epoch_ms < 0 {
        return 0;
    }
    let secs = (epoch_ms / 1000) as u64;
    let ms_part = (epoch_ms % 1000) as u64;
    // millis / 1000 * 2^32  ==  millis * 2^32 / 1000
    let frac = ((ms_part << 32) / 1000) & 0xFFFF_FFFF;
    ((secs + NTP_EPOCH_OFFSET_SECS) << 32) | frac
}

/// Convert an NTP 64-bit timestamp to a Unix epoch in milliseconds.
pub fn ntp_to_unix_ms(ntp_ts: u64) -> i64 {
    let secs_ntp = ntp_ts >> 32;
    let frac = ntp_ts & 0xFFFF_FFFF;
    let secs_unix = secs_ntp as i64 - NTP_EPOCH_OFFSET_SECS as i64;
    // 2^32 / 1000 ≈ 4294967.296 — multiplication overflows u64 only above
    // ~4.3e9 years of fractional part, which we will not see.
    let ms = (frac * 1000) >> 32;
    secs_unix.saturating_mul(1000).saturating_add(ms as i64)
}

/// Current Unix epoch in milliseconds using the system clock.
///
/// Used only for the rare "no NTP sync yet" path of the server, where we
/// fall back to the system clock and advertise Stratum 16 / LI=3.
pub fn system_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[inline]
fn read_u64(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(arr)
}

#[inline]
fn write_u64(buf: &mut [u8], val: u64) {
    buf[..8].copy_from_slice(&val.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_request() -> [u8; NTP_PACKET_SIZE] {
        // LI=0 VN=4 Mode=3 (client), Stratum=0, Poll=4, Precision=-20,
        // all timestamps = 0 (typical fresh client request).
        [
            0x23, 0x00, 0x04, 0xEC, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    #[test]
    fn parse_valid_client_request() {
        let p = parse_packet(&example_request()).expect("should parse");
        assert_eq!(p.li, 0);
        assert_eq!(p.vn, NTP_VERSION);
        assert_eq!(p.mode, MODE_CLIENT);
        assert_eq!(p.stratum, 0);
        assert_eq!(p.poll, 4);
        assert_eq!(p.precision, -20);
    }

    #[test]
    fn reject_too_short() {
        let buf = [0u8; 10];
        let err = parse_packet(&buf).unwrap_err();
        assert!(matches!(err, ProtocolError::TooShort { .. }));
    }

    #[test]
    fn reject_non_client_mode() {
        // Mode 4 (server) is not something we respond to as a server.
        let mut buf = example_request();
        buf[0] = 0x24; // LI=0 VN=4 Mode=4
        let err = parse_packet(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnsupportedMode(MODE_SERVER));
    }

    #[test]
    fn reject_bad_version() {
        let mut buf = example_request();
        buf[0] = 0x0B; // LI=0 VN=1 Mode=3
        let err = parse_packet(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnsupportedVersion(1));
    }

    #[test]
    fn roundtrip_serialize_parse() {
        let p = NtpPacket {
            li: LI_NO_WARNING,
            vn: NTP_VERSION,
            mode: MODE_SERVER,
            stratum: STRATUM_PRIMARY,
            poll: 4,
            precision: -20,
            root_delay: 0x0000_1234,
            root_dispersion: 0x0000_5678,
            reference_id: u32::from_be_bytes(*b"LOCL"),
            ref_timestamp: 0xDEAD_BEEF_CAFE_BABE,
            origin_timestamp: 0x1111_2222_3333_4444,
            receive_timestamp: 0x5555_6666_7777_8888,
            transmit_timestamp: 0x9999_AAAA_BBBB_CCCC,
        };
        let bytes = serialize_packet(&p);
        assert_eq!(bytes[0], 0x24); // LI=0 VN=4 Mode=4
        assert_eq!(bytes[1], 1); // Stratum 1
        assert_eq!(bytes[2], 4);
        assert_eq!(bytes[3] as i8, -20);
        assert_eq!(&bytes[4..8], &[0, 0, 0x12, 0x34]);
        assert_eq!(&bytes[8..12], &[0, 0, 0x56, 0x78]);
        assert_eq!(&bytes[12..16], b"LOCL");
    }

    #[test]
    fn ntp_epoch_offset_is_70_years() {
        // 70 ordinary years + 17 leap days (1900 is not a leap year, 1904..1968).
        // 70 * 365 + 17 == 25567 days.
        assert_eq!(NTP_EPOCH_OFFSET_SECS, 25567 * 86_400);
    }

    #[test]
    fn unix_ms_to_ntp_zero_is_70_years() {
        let ts = unix_ms_to_ntp(0);
        assert_eq!(ts >> 32, NTP_EPOCH_OFFSET_SECS);
        assert_eq!(ts & 0xFFFF_FFFF, 0);
    }

    #[test]
    fn unix_ntp_roundtrip_millisecond_precision() {
        // 2024-01-01T00:00:00.000Z
        let ms: i64 = 1_704_067_200_000;
        let ntp = unix_ms_to_ntp(ms);
        let back = ntp_to_unix_ms(ntp);
        // Fractional precision loss is sub-millisecond; we should be
        // within 1 ms.
        assert!(
            (back - ms).abs() < 2,
            "roundtrip drift {} ms ({} -> {} -> {})",
            (back - ms).abs(),
            ms,
            ntp,
            back
        );
    }

    #[test]
    fn unix_ntp_roundtrip_half_second() {
        let ms: i64 = 1_704_067_200_500;
        let ntp = unix_ms_to_ntp(ms);
        let back = ntp_to_unix_ms(ntp);
        assert!((back - ms).abs() < 2, "drift {} ms", (back - ms).abs());
    }

    #[test]
    fn ntp_to_unix_ms_pre_1970() {
        // NTP epoch itself (1900-01-01T00:00:00Z) → unix_ms == 0.
        let ts = NTP_EPOCH_OFFSET_SECS << 32;
        assert_eq!(ntp_to_unix_ms(ts), 0);
    }

    #[test]
    fn negative_unix_ms_returns_zero() {
        // System clock before unix epoch shouldn't crash the server.
        assert_eq!(unix_ms_to_ntp(-1), 0);
    }

    #[test]
    fn reference_id_ascii_decodes() {
        let p = NtpPacket {
            reference_id: u32::from_be_bytes(*b"NTP "),
            ..NtpPacket::new(0, 4, 4)
        };
        let s: String = p.reference_id_ascii().iter().collect();
        assert_eq!(s, "NTP ");
    }

    #[test]
    fn parse_buffer_longer_than_48_is_fine() {
        let mut buf = Vec::from(example_request());
        buf.extend_from_slice(&[0xFF; 32]);
        let p = parse_packet(&buf).expect("should still parse");
        assert_eq!(p.mode, MODE_CLIENT);
    }
}
