//! Wire format spoken between the host agent (Linux, real headset) and the VM
//! agent (Windows, virtual audio devices).
//!
//! Everything rides on a single UDP socket pair. The host is the client: it
//! sends `Hello`, the VM answers `HelloAck` and from then on remembers the
//! address it replied to, so both media directions flow over the same 5-tuple
//! and only one inbound firewall rule is needed on the Windows side.
//!
//! ```text
//! every packet   MAGIC(4) VER(1) TYPE(1)
//!
//! Hello          TOKEN_LEN(1) TOKEN(n) SESSION(4) FORMAT(mic) FORMAT(spk)
//! HelloAck       STATUS(1) SESSION(4) FORMAT(mic) FORMAT(spk) MSG_LEN(1) MSG(n)
//! Audio          SESSION(4) DIR(1) SEQ(4) FRAMES(2) PAYLOAD(...)
//! Bye            SESSION(4)
//! Ping / Pong    SESSION(4) NONCE(8)
//!
//! FORMAT         RATE(4) CHANNELS(1) CODEC(1) FRAME_MS(1)
//! ```
//!
//! All multi-byte integers are big endian. `SEQ` counts audio packets per
//! direction and wraps; the receiver uses it to detect loss and reordering.
//!
//! The trust model is the same as the tunnel it ships next to: a shared token
//! over a private host <-> VM link. There is no encryption — do not expose the
//! listener to an untrusted network.

use std::fmt;

pub mod jitter;

pub use jitter::{JitterBuffer, JitterStats};

pub const MAGIC: [u8; 4] = *b"ABRG";
pub const VERSION: u8 = 1;

/// Enough for a 20 ms stereo frame at 48 kHz plus headers.
pub const MAX_PACKET: usize = 4096;

const HEADER_LEN: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PacketType {
    Hello = 1,
    HelloAck = 2,
    Audio = 3,
    Bye = 4,
    Ping = 5,
    Pong = 6,
}

impl PacketType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(PacketType::Hello),
            2 => Some(PacketType::HelloAck),
            3 => Some(PacketType::Audio),
            4 => Some(PacketType::Bye),
            5 => Some(PacketType::Ping),
            6 => Some(PacketType::Pong),
            _ => None,
        }
    }
}

/// Which way an audio packet travels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Direction {
    /// Host microphone -> VM virtual microphone.
    Mic = 1,
    /// VM playback -> host speakers.
    Speaker = 2,
}

impl Direction {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Direction::Mic),
            2 => Some(Direction::Speaker),
            _ => None,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Mic => f.write_str("mic"),
            Direction::Speaker => f.write_str("speaker"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Codec {
    /// Interleaved signed 16-bit little endian, no compression.
    PcmS16Le = 0,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Codec::PcmS16Le),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    AuthFailed = 1,
    /// The requested format is not something this side can produce or consume.
    UnsupportedFormat = 2,
    BadRequest = 3,
    /// Another host already owns the session.
    Busy = 4,
    ServerError = 5,
}

impl Status {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Status::Ok),
            1 => Some(Status::AuthFailed),
            2 => Some(Status::UnsupportedFormat),
            3 => Some(Status::BadRequest),
            4 => Some(Status::Busy),
            5 => Some(Status::ServerError),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Status::Ok => "ok",
            Status::AuthFailed => "authentication failed",
            Status::UnsupportedFormat => "unsupported audio format",
            Status::BadRequest => "malformed request",
            Status::Busy => "another host owns the session",
            Status::ServerError => "server error",
        };
        f.write_str(s)
    }
}

/// PCM layout of one audio stream, agreed during the handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Format {
    pub rate: u32,
    pub channels: u8,
    pub codec: Codec,
    /// Nominal packet duration. Both sides size their buffers from this.
    pub frame_ms: u8,
}

impl Format {
    pub const ENCODED_LEN: usize = 7;

    pub fn new(rate: u32, channels: u8, frame_ms: u8) -> Self {
        Format {
            rate,
            channels,
            codec: Codec::PcmS16Le,
            frame_ms,
        }
    }

    /// Sample frames carried by one packet.
    pub fn frames_per_packet(&self) -> usize {
        (self.rate as usize * self.frame_ms as usize) / 1000
    }

    /// Interleaved `i16` values carried by one packet.
    pub fn samples_per_packet(&self) -> usize {
        self.frames_per_packet() * self.channels as usize
    }

    pub fn payload_len(&self) -> usize {
        self.samples_per_packet() * 2
    }

    /// True when a packet of this format still fits a 1500-byte path without
    /// IP fragmentation. Callers warn rather than refuse: a virtio link between
    /// host and guest reassembles fragments without breaking a sweat.
    pub fn fits_ethernet_mtu(&self) -> bool {
        self.payload_len() + HEADER_LEN + 13 + 28 <= 1500
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.rate,
            8000 | 16000 | 22050 | 24000 | 32000 | 44100 | 48000 | 96000
        ) {
            return Err(format!("unsupported sample rate {}", self.rate));
        }
        if self.channels < 1 || self.channels > 2 {
            return Err(format!("channels must be 1 or 2, got {}", self.channels));
        }
        if !matches!(self.frame_ms, 5 | 10 | 20) {
            return Err(format!(
                "frame_ms must be 5, 10 or 20, got {}",
                self.frame_ms
            ));
        }
        if self.frames_per_packet() == 0 {
            return Err("frame_ms is too short for this sample rate".to_string());
        }
        Ok(())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.rate.to_be_bytes());
        out.push(self.channels);
        out.push(self.codec as u8);
        out.push(self.frame_ms);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let rate = r.u32()?;
        let channels = r.u8()?;
        let codec = Codec::from_u8(r.u8()?).ok_or(DecodeError::UnknownCodec)?;
        let frame_ms = r.u8()?;
        Ok(Format {
            rate,
            channels,
            codec,
            frame_ms,
        })
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let layout = if self.channels == 1 { "mono" } else { "stereo" };
        write!(f, "{} Hz {} s16le {} ms", self.rate, layout, self.frame_ms)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    Hello {
        token: Vec<u8>,
        session: u32,
        mic: Format,
        speaker: Format,
    },
    HelloAck {
        status: Status,
        session: u32,
        mic: Format,
        speaker: Format,
        message: String,
    },
    Bye {
        session: u32,
    },
    Ping {
        session: u32,
        nonce: u64,
    },
    Pong {
        session: u32,
        nonce: u64,
    },
}

/// An audio packet, decoded in place so the PCM payload is never copied.
#[derive(Clone, Copy, Debug)]
pub struct AudioHeader {
    pub session: u32,
    pub direction: Direction,
    pub seq: u32,
    pub frames: u16,
}

/// Either a control packet or the header of an audio packet plus its payload.
#[derive(Debug)]
pub enum Incoming<'a> {
    Control(Packet),
    Audio {
        header: AudioHeader,
        payload: &'a [u8],
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    UnknownType(u8),
    UnknownDirection(u8),
    UnknownCodec,
    UnknownStatus(u8),
    /// `frames` disagrees with the number of payload bytes actually present.
    PayloadMismatch,
    Utf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooShort => f.write_str("packet truncated"),
            DecodeError::BadMagic => f.write_str("bad magic"),
            DecodeError::BadVersion(v) => write!(f, "unsupported protocol version {v}"),
            DecodeError::UnknownType(t) => write!(f, "unknown packet type {t}"),
            DecodeError::UnknownDirection(d) => write!(f, "unknown direction {d}"),
            DecodeError::UnknownCodec => f.write_str("unknown codec"),
            DecodeError::UnknownStatus(s) => write!(f, "unknown status {s}"),
            DecodeError::PayloadMismatch => {
                f.write_str("audio payload length disagrees with header")
            }
            DecodeError::Utf8 => f.write_str("message is not valid UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::TooShort)?;
        let slice = self.buf.get(self.pos..end).ok_or(DecodeError::TooShort)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn rest(self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

fn header(out: &mut Vec<u8>, ty: PacketType) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(ty as u8);
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        match self {
            Packet::Hello {
                token,
                session,
                mic,
                speaker,
            } => {
                header(&mut out, PacketType::Hello);
                let len = token.len().min(u8::MAX as usize);
                out.push(len as u8);
                out.extend_from_slice(&token[..len]);
                out.extend_from_slice(&session.to_be_bytes());
                mic.encode(&mut out);
                speaker.encode(&mut out);
            }
            Packet::HelloAck {
                status,
                session,
                mic,
                speaker,
                message,
            } => {
                header(&mut out, PacketType::HelloAck);
                out.push(*status as u8);
                out.extend_from_slice(&session.to_be_bytes());
                mic.encode(&mut out);
                speaker.encode(&mut out);
                let msg = message.as_bytes();
                let len = msg.len().min(u8::MAX as usize);
                out.push(len as u8);
                out.extend_from_slice(&msg[..len]);
            }
            Packet::Bye { session } => {
                header(&mut out, PacketType::Bye);
                out.extend_from_slice(&session.to_be_bytes());
            }
            Packet::Ping { session, nonce } | Packet::Pong { session, nonce } => {
                let ty = if matches!(self, Packet::Ping { .. }) {
                    PacketType::Ping
                } else {
                    PacketType::Pong
                };
                header(&mut out, ty);
                out.extend_from_slice(&session.to_be_bytes());
                out.extend_from_slice(&nonce.to_be_bytes());
            }
        }
        out
    }
}

/// Serialise an audio packet into `out`, which is cleared first.
///
/// `samples` is interleaved native-endian `i16`; it goes on the wire as little
/// endian regardless of host byte order.
pub fn encode_audio(
    out: &mut Vec<u8>,
    session: u32,
    direction: Direction,
    seq: u32,
    channels: u8,
    samples: &[i16],
) {
    out.clear();
    header(out, PacketType::Audio);
    out.extend_from_slice(&session.to_be_bytes());
    out.push(direction as u8);
    out.extend_from_slice(&seq.to_be_bytes());
    let frames = samples.len() / channels.max(1) as usize;
    out.extend_from_slice(&(frames as u16).to_be_bytes());
    out.reserve(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Decode `payload` (little endian `i16`) into `out`, which is cleared first.
pub fn decode_samples(payload: &[u8], out: &mut Vec<i16>) {
    out.clear();
    out.reserve(payload.len() / 2);
    for pair in payload.chunks_exact(2) {
        out.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
}

pub fn decode(buf: &[u8]) -> Result<Incoming<'_>, DecodeError> {
    let mut r = Reader::new(buf);
    if r.take(4)? != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = r.u8()?;
    if version != VERSION {
        return Err(DecodeError::BadVersion(version));
    }
    let raw_type = r.u8()?;
    let ty = PacketType::from_u8(raw_type).ok_or(DecodeError::UnknownType(raw_type))?;

    match ty {
        PacketType::Hello => {
            let len = r.u8()? as usize;
            let token = r.take(len)?.to_vec();
            let session = r.u32()?;
            let mic = Format::decode(&mut r)?;
            let speaker = Format::decode(&mut r)?;
            Ok(Incoming::Control(Packet::Hello {
                token,
                session,
                mic,
                speaker,
            }))
        }
        PacketType::HelloAck => {
            let raw = r.u8()?;
            let status = Status::from_u8(raw).ok_or(DecodeError::UnknownStatus(raw))?;
            let session = r.u32()?;
            let mic = Format::decode(&mut r)?;
            let speaker = Format::decode(&mut r)?;
            let len = r.u8()? as usize;
            let message =
                String::from_utf8(r.take(len)?.to_vec()).map_err(|_| DecodeError::Utf8)?;
            Ok(Incoming::Control(Packet::HelloAck {
                status,
                session,
                mic,
                speaker,
                message,
            }))
        }
        PacketType::Audio => {
            let session = r.u32()?;
            let raw = r.u8()?;
            let direction = Direction::from_u8(raw).ok_or(DecodeError::UnknownDirection(raw))?;
            let seq = r.u32()?;
            let frames = r.u16()?;
            let payload = r.rest();
            if payload.len() % 2 != 0 || payload.is_empty() {
                return Err(DecodeError::PayloadMismatch);
            }
            Ok(Incoming::Audio {
                header: AudioHeader {
                    session,
                    direction,
                    seq,
                    frames,
                },
                payload,
            })
        }
        PacketType::Bye => Ok(Incoming::Control(Packet::Bye { session: r.u32()? })),
        PacketType::Ping => Ok(Incoming::Control(Packet::Ping {
            session: r.u32()?,
            nonce: r.u64()?,
        })),
        PacketType::Pong => Ok(Incoming::Control(Packet::Pong {
            session: r.u32()?,
            nonce: r.u64()?,
        })),
    }
}

/// Constant-time-ish token comparison. Tokens are short and the link is
/// private, but there is no reason to leak length-prefix timing either.
pub fn token_matches(expected: &str, got: &[u8]) -> bool {
    let expected = expected.as_bytes();
    if expected.len() != got.len() {
        return false;
    }
    expected
        .iter()
        .zip(got)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_mono() -> Format {
        Format::new(48_000, 1, 10)
    }

    fn fmt_stereo() -> Format {
        Format::new(48_000, 2, 10)
    }

    #[test]
    fn hello_round_trip() {
        let p = Packet::Hello {
            token: b"s3cret".to_vec(),
            session: 0xdead_beef,
            mic: fmt_mono(),
            speaker: fmt_stereo(),
        };
        let bytes = p.encode();
        match decode(&bytes).unwrap() {
            Incoming::Control(got) => assert_eq!(got, p),
            _ => panic!("expected a control packet"),
        }
    }

    #[test]
    fn hello_ack_round_trip() {
        let p = Packet::HelloAck {
            status: Status::Ok,
            session: 7,
            mic: fmt_mono(),
            speaker: fmt_stereo(),
            message: "CABLE Input".to_string(),
        };
        let bytes = p.encode();
        match decode(&bytes).unwrap() {
            Incoming::Control(got) => assert_eq!(got, p),
            _ => panic!("expected a control packet"),
        }
    }

    #[test]
    fn ping_pong_round_trip() {
        for p in [
            Packet::Ping {
                session: 1,
                nonce: 42,
            },
            Packet::Pong {
                session: 1,
                nonce: 42,
            },
        ] {
            let bytes = p.encode();
            match decode(&bytes).unwrap() {
                Incoming::Control(got) => assert_eq!(got, p),
                _ => panic!("expected a control packet"),
            }
        }
    }

    #[test]
    fn audio_round_trip() {
        let samples: Vec<i16> = (0..960).map(|i| (i as i16).wrapping_mul(37)).collect();
        let mut buf = Vec::new();
        encode_audio(&mut buf, 9, Direction::Speaker, 12345, 2, &samples);

        match decode(&buf).unwrap() {
            Incoming::Audio { header, payload } => {
                assert_eq!(header.session, 9);
                assert_eq!(header.direction, Direction::Speaker);
                assert_eq!(header.seq, 12345);
                assert_eq!(header.frames as usize, samples.len() / 2);
                let mut out = Vec::new();
                decode_samples(payload, &mut out);
                assert_eq!(out, samples);
            }
            _ => panic!("expected an audio packet"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(decode(b"nope").unwrap_err(), DecodeError::BadMagic);
        assert_eq!(decode(&[]).unwrap_err(), DecodeError::TooShort);

        let mut bad = Packet::Bye { session: 1 }.encode();
        bad[4] = 99;
        assert_eq!(decode(&bad).unwrap_err(), DecodeError::BadVersion(99));
    }

    #[test]
    fn truncated_audio_is_rejected() {
        let mut buf = Vec::new();
        encode_audio(&mut buf, 1, Direction::Mic, 0, 1, &[1, 2, 3, 4]);
        buf.truncate(HEADER_LEN + 4); // header plus part of the session id
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn format_sizes() {
        let f = fmt_stereo();
        assert_eq!(f.frames_per_packet(), 480);
        assert_eq!(f.samples_per_packet(), 960);
        assert_eq!(f.payload_len(), 1920);
        assert!(!f.fits_ethernet_mtu());

        let f = Format::new(48_000, 2, 5);
        assert_eq!(f.payload_len(), 960);
        assert!(f.fits_ethernet_mtu());
        assert!(fmt_mono().fits_ethernet_mtu());
    }

    #[test]
    fn format_validation() {
        assert!(fmt_mono().validate().is_ok());
        assert!(Format::new(44_100, 3, 10).validate().is_err());
        assert!(Format::new(12_345, 1, 10).validate().is_err());
        assert!(Format::new(48_000, 1, 7).validate().is_err());
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", b"abc"));
        assert!(!token_matches("abc", b"abd"));
        assert!(!token_matches("abc", b"ab"));
        assert!(token_matches("", b""));
    }
}
