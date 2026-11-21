//! `ASR2` — the Audio Share Rust wire protocol (v2).
//!
//! Two channels between host and players, mirroring the original project's
//! proven design but with self-describing, endian-safe framing:
//!
//! - **TCP (control, and optional audio):** every message is a frame
//!   `magic("ASR2") | version u16le | type u8 | flags u8 | payload_len u32le | payload`
//! - **UDP (audio):** datagrams `session u32le | seq u32le | pcm...`
//!   sized to fit one MTU and snapped to sample-frame boundaries.
//!
//! Sequence:
//! 1. client connects TCP, server pushes [`FrameType::Hello`] (audio format)
//! 2. client sends [`FrameType::Start`] (flag bit 0 = "send audio over TCP")
//! 3. server replies [`FrameType::Started`] with a session token
//! 4. client sends its token over UDP to open the data channel
//! 5. server pings every [`HEARTBEAT_INTERVAL`], client pongs; either side
//!    may end with [`FrameType::Stop`]
//! 6. [`FrameType::Stats`] frames are echoed by the server so the client can
//!    measure round-trip time; they also count as heartbeat keep-alives.

use std::fmt;
use std::time::Duration;

/// Wire magic at the head of every TCP frame.
pub const MAGIC: [u8; 4] = *b"ASR2";
/// Current protocol version.
pub const PROTOCOL_VERSION: u16 = 2;
/// Default TCP/UDP port (kept from the original project).
pub const DEFAULT_PORT: u16 = 65530;
/// Largest audio segment we put in one UDP datagram (1492 IPv4 MTU - IP 20 - UDP 8).
pub const MTU_DATA_MAX: usize = 1464;
/// Server→client ping cadence.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
/// A peer is dropped when nothing (pongs, stats, data) arrives within this window.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(12);
/// Hard cap on a single TCP frame payload (audio frames carry ~0.4 s of PCM at most).
pub const MAX_PAYLOAD_LEN: usize = 4 * 1024 * 1024;

/// PCM sample encoding on the wire. All multi-byte values are little endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// 16-bit signed integer PCM (lowest bandwidth, what Android plays natively).
    S16Le,
    /// 32-bit IEEE float PCM (bit-exact loopback, higher bandwidth).
    F32Le,
}

impl Encoding {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Encoding::S16Le),
            1 => Some(Encoding::F32Le),
            _ => None,
        }
    }

    pub fn as_u16(self) -> u16 {
        match self {
            Encoding::S16Le => 0,
            Encoding::F32Le => 1,
        }
    }

    /// Bytes per sample.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Encoding::S16Le => 2,
            Encoding::F32Le => 4,
        }
    }

    /// Human label used by the CLI and GUI.
    pub fn label(self) -> &'static str {
        match self {
            Encoding::S16Le => "s16",
            Encoding::F32Le => "f32",
        }
    }

    pub fn parse_label(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "s16" | "s16le" | "pcm16" | "pcm_s16le" => Some(Encoding::S16Le),
            "f32" | "f32le" | "float" | "pcm_f32le" => Some(Encoding::F32Le),
            _ => None,
        }
    }
}

/// The negotiated audio format: encoding, channel count, sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub encoding: Encoding,
    pub channels: u16,
    pub sample_rate: u32,
}

impl AudioFormat {
    pub const fn new(encoding: Encoding, channels: u16, sample_rate: u32) -> Self {
        Self {
            encoding,
            channels,
            sample_rate,
        }
    }

    /// Bytes per sample frame (all channels).
    pub fn bytes_per_frame(&self) -> usize {
        self.encoding.bytes_per_sample() * self.channels as usize
    }

    /// Bytes of raw PCM per second of audio.
    pub fn bytes_per_second(&self) -> usize {
        self.bytes_per_frame() * self.sample_rate as usize
    }

    /// 8-byte wire form: `encoding u16le | channels u16le | sample_rate u32le`.
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&self.encoding.as_u16().to_le_bytes());
        b[2..4].copy_from_slice(&self.channels.to_le_bytes());
        b[4..8].copy_from_slice(&self.sample_rate.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, ProtocolError> {
        if b.len() < 8 {
            return Err(ProtocolError::Truncated);
        }
        let encoding = Encoding::from_u16(u16::from_le_bytes([b[0], b[1]]))
            .ok_or(ProtocolError::BadEncoding)?;
        let channels = u16::from_le_bytes([b[2], b[3]]);
        let sample_rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        if channels == 0 || channels > 32 || !(8_000..=384_000).contains(&sample_rate) {
            return Err(ProtocolError::BadFormat);
        }
        Ok(Self {
            encoding,
            channels,
            sample_rate,
        })
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Hz, {} ch, {}",
            self.sample_rate,
            self.channels,
            match self.encoding {
                Encoding::S16Le => "PCM 16-bit",
                Encoding::F32Le => "PCM 32-bit float",
            }
        )
    }
}

/// Frame types carried over TCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Hello,
    Start,
    Started,
    Audio,
    Ping,
    Pong,
    Stats,
    Stop,
}

impl FrameType {
    pub fn as_u8(self) -> u8 {
        match self {
            FrameType::Hello => 1,
            FrameType::Start => 2,
            FrameType::Started => 3,
            FrameType::Audio => 4,
            FrameType::Ping => 5,
            FrameType::Pong => 6,
            FrameType::Stats => 7,
            FrameType::Stop => 8,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => FrameType::Hello,
            2 => FrameType::Start,
            3 => FrameType::Started,
            4 => FrameType::Audio,
            5 => FrameType::Ping,
            6 => FrameType::Pong,
            7 => FrameType::Stats,
            8 => FrameType::Stop,
            _ => return None,
        })
    }
}

/// One TCP frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ftype: FrameType,
    /// Bit 0 (client `Start`): deliver audio over TCP instead of UDP.
    /// Bit 1 (`Started`/`Hello`): sender supports mDNS discovery (informational).
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub const HEADER_LEN: usize = 12;

    pub fn new(ftype: FrameType, flags: u8, payload: Vec<u8>) -> Self {
        Self {
            ftype,
            flags,
            payload,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.push(self.ftype.as_u8());
        out.push(self.flags);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode one frame from `buf` (which must start at a frame boundary).
    /// Returns the frame and the number of bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError> {
        if buf.len() < Self::HEADER_LEN {
            return Err(ProtocolError::Truncated);
        }
        if buf[..4] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::BadVersion(version));
        }
        let ftype = FrameType::from_u8(buf[6]).ok_or(ProtocolError::BadFrameType(buf[6]))?;
        let flags = buf[7];
        let len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge(len));
        }
        let total = Self::HEADER_LEN + len;
        if buf.len() < total {
            return Err(ProtocolError::Truncated);
        }
        Ok((
            Self {
                ftype,
                flags,
                payload: buf[Self::HEADER_LEN..total].to_vec(),
            },
            total,
        ))
    }
}

/// Server → client handshake payload: format + chunking + friendly name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub format: AudioFormat,
    /// Server-side capture chunk length in milliseconds (scheduling hint).
    pub block_ms: u16,
    /// Human-readable server name (UTF-8, ≤ 64 bytes).
    pub name: String,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(12 + self.name.len());
        p.extend_from_slice(&self.format.to_bytes());
        p.extend_from_slice(&self.block_ms.to_le_bytes());
        let mut name = self.name.as_bytes();
        if name.len() > 64 {
            name = &name[..64];
        }
        p.extend_from_slice(&(name.len() as u16).to_le_bytes());
        p.extend_from_slice(name);
        p
    }

    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() < 12 {
            return Err(ProtocolError::Truncated);
        }
        let format = AudioFormat::from_bytes(&payload[..8])?;
        let block_ms = u16::from_le_bytes([payload[8], payload[9]]);
        let name_len = u16::from_le_bytes([payload[10], payload[11]]) as usize;
        if payload.len() < 12 + name_len {
            return Err(ProtocolError::Truncated);
        }
        let name = String::from_utf8_lossy(&payload[12..12 + name_len]).into_owned();
        Ok(Self {
            format,
            block_ms,
            name,
        })
    }
}

/// Build a `Start` frame. `tcp_data`: request audio over the TCP channel
/// (for networks where UDP is filtered).
pub fn start_frame(tcp_data: bool) -> Frame {
    Frame::new(FrameType::Start, if tcp_data { 1 } else { 0 }, Vec::new())
}

/// Build a `Started` frame carrying the session token.
pub fn started_frame(session: u32) -> Frame {
    Frame::new(FrameType::Started, 0, session.to_le_bytes().to_vec())
}

/// Build a `Ping` frame carrying a server-clock timestamp.
pub fn ping_frame(timestamp_ms: u64) -> Frame {
    Frame::new(FrameType::Ping, 0, timestamp_ms.to_le_bytes().to_vec())
}

/// Client replies to a ping by echoing the timestamp in a `Pong`.
pub fn pong_frame(timestamp_ms: u64) -> Frame {
    Frame::new(FrameType::Pong, 0, timestamp_ms.to_le_bytes().to_vec())
}

/// Client → server RTT probe; the server echoes the payload untouched.
pub fn stats_frame(timestamp_ms: u64) -> Frame {
    Frame::new(FrameType::Stats, 0, timestamp_ms.to_le_bytes().to_vec())
}

/// Build an audio frame payload (`seq u32le | pcm...`) for the TCP channel.
pub fn tcp_audio_payload(seq: u32, pcm: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + pcm.len());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(pcm);
    p
}

/// Split a TCP audio payload into `(seq, pcm)`.
pub fn parse_tcp_audio(payload: &[u8]) -> Result<(u32, &[u8]), ProtocolError> {
    if payload.len() < 4 {
        return Err(ProtocolError::Truncated);
    }
    let seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok((seq, &payload[4..]))
}

/// Build a UDP audio datagram: `session u32le | seq u32le | pcm...`.
pub fn udp_audio_datagram(session: u32, seq: u32, pcm: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(8 + pcm.len());
    d.extend_from_slice(&session.to_le_bytes());
    d.extend_from_slice(&seq.to_le_bytes());
    d.extend_from_slice(pcm);
    d
}

/// Parse an inbound UDP datagram from a client (session registration/keep-alive):
/// `session u32le`.
pub fn parse_udp_session(datagram: &[u8]) -> Result<u32, ProtocolError> {
    if datagram.len() < 4 {
        return Err(ProtocolError::Truncated);
    }
    Ok(u32::from_le_bytes([
        datagram[0],
        datagram[1],
        datagram[2],
        datagram[3],
    ]))
}

/// Parse an inbound UDP audio datagram from the server:
/// `session u32le | seq u32le | pcm...`.
pub fn parse_udp_audio(datagram: &[u8]) -> Result<(u32, u32, &[u8]), ProtocolError> {
    if datagram.len() < 8 {
        return Err(ProtocolError::Truncated);
    }
    let session = u32::from_le_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]);
    let seq = u32::from_le_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]);
    Ok((session, seq, &datagram[8..]))
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("buffer too short")]
    Truncated,
    #[error("bad magic bytes (not an ASR2 stream)")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    BadVersion(u16),
    #[error("unknown frame type {0}")]
    BadFrameType(u8),
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("unknown audio encoding")]
    BadEncoding,
    #[error("implausible audio format")]
    BadFormat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_roundtrip() {
        let f = AudioFormat::new(Encoding::S16Le, 2, 44100);
        assert_eq!(f.bytes_per_frame(), 4);
        assert_eq!(f.bytes_per_second(), 44100 * 4);
        let rt = AudioFormat::from_bytes(&f.to_bytes()).unwrap();
        assert_eq!(f, rt);
        assert!(AudioFormat::from_bytes(&[0u8; 3]).is_err());
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(FrameType::Ping, 0x5a, vec![1, 2, 3, 4, 5]);
        let bytes = f.encode();
        assert_eq!(bytes.len(), Frame::HEADER_LEN + 5);
        let (rt, used) = Frame::decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(rt, f);
    }

    #[test]
    fn frame_rejects_bad_magic_and_version() {
        let mut bytes = Frame::new(FrameType::Stop, 0, vec![]).encode();
        bytes[0] = b'X';
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadMagic)
        ));
        let mut bytes = Frame::new(FrameType::Stop, 0, vec![]).encode();
        bytes[4] = 9;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadVersion(9))
        ));
    }

    #[test]
    fn hello_roundtrip() {
        let h = Hello {
            format: AudioFormat::new(Encoding::F32Le, 2, 48000),
            block_ms: 20,
            name: "laptop".into(),
        };
        let (rt, _) = {
            let b = h.encode();
            let n = b.len();
            (Hello::decode(&b).unwrap(), n)
        };
        assert_eq!(h, rt);
    }

    #[test]
    fn udp_datagram_roundtrip() {
        let d = udp_audio_datagram(7, 42, &[0u8; 10]);
        let (s, q, pcm) = parse_udp_audio(&d).unwrap();
        assert_eq!((s, q, pcm.len()), (7, 42, 10));
        let s = parse_udp_session(&7u32.to_le_bytes()).unwrap();
        assert_eq!(s, 7);
    }

    #[test]
    fn tcp_audio_roundtrip() {
        let p = tcp_audio_payload(9, &[1, 2, 3]);
        let (seq, pcm) = parse_tcp_audio(&p).unwrap();
        assert_eq!(seq, 9);
        assert_eq!(pcm, &[1, 2, 3]);
    }
}
