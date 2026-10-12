//! Wire messages for macrd. See PROTOCOL.md at the repo root; this mirrors
//! `server/Sources/MacRDCore/Protocol/Messages.swift`.

use std::fmt;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
pub const DEFAULT_PORT: u16 = 7878;

pub const CODEC_H264: u8 = 0;
pub const CODEC_HEVC: u8 = 1;

pub const CLIPBOARD_TEXT: u8 = 0;
pub const CLIPBOARD_PNG: u8 = 1;

pub const START_FLAG_VIRTUAL_DISPLAY: u8 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f32,
    pub is_main: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello { version: u16, caps: u32 },
    AuthChallenge { nonce: [u8; 32] },
    AuthResponse { mac: [u8; 32] },
    AuthResult { ok: bool },
    ServerInfo { displays: Vec<DisplayInfo> },
    StartStream { display_id: u32, width: u32, height: u32, fps: u8, bitrate_kbps: u32, codec: u8, flags: u8 },
    StreamStarted { display_id: u32, width: u32, height: u32, codec: u8 },
    RequestKeyframe,
    Resize { width: u32, height: u32 },
    Ping { t: u64 },
    Pong { t: u64 },
    VideoFrame { pts_us: u64, keyframe: bool, data: Vec<u8> },
    FrameAck { pts_us: u64 },
    Key { usage: u16, down: bool },
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, down: bool },
    Scroll { dx: f32, dy: f32, precise: bool },
    Clipboard { kind: u8, data: Vec<u8> },
    Bye { reason: String },
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Truncated,
    FrameTooLarge(usize),
    InvalidUtf8,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "truncated message"),
            Error::FrameTooLarge(n) => write!(f, "frame too large ({n} bytes)"),
            Error::InvalidUtf8 => write!(f, "invalid UTF-8 string"),
        }
    }
}

impl std::error::Error for Error {}

impl Message {
    pub fn type_byte(&self) -> u8 {
        match self {
            Message::Hello { .. } => 0x01,
            Message::AuthChallenge { .. } => 0x02,
            Message::AuthResponse { .. } => 0x03,
            Message::AuthResult { .. } => 0x04,
            Message::ServerInfo { .. } => 0x05,
            Message::StartStream { .. } => 0x10,
            Message::StreamStarted { .. } => 0x11,
            Message::RequestKeyframe => 0x12,
            Message::Resize { .. } => 0x13,
            Message::Ping { .. } => 0x14,
            Message::Pong { .. } => 0x15,
            Message::VideoFrame { .. } => 0x20,
            Message::FrameAck { .. } => 0x21,
            Message::Key { .. } => 0x30,
            Message::MouseMove { .. } => 0x31,
            Message::MouseButton { .. } => 0x32,
            Message::Scroll { .. } => 0x33,
            Message::Clipboard { .. } => 0x40,
            Message::Bye { .. } => 0x7F,
        }
    }

    /// Full frame: length prefix + type + payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(16));
        w.u32(0); // length placeholder
        w.u8(self.type_byte());
        match self {
            Message::Hello { version, caps } => {
                w.u16(*version);
                w.u32(*caps);
            }
            Message::AuthChallenge { nonce } => w.bytes(nonce),
            Message::AuthResponse { mac } => w.bytes(mac),
            Message::AuthResult { ok } => w.u8(*ok as u8),
            Message::ServerInfo { displays } => {
                w.u8(displays.len().min(255) as u8);
                for d in displays.iter().take(255) {
                    w.u32(d.id);
                    w.u32(d.width_px);
                    w.u32(d.height_px);
                    w.f32(d.scale);
                    w.u8(d.is_main as u8);
                    w.str(&d.name);
                }
            }
            Message::StartStream { display_id, width, height, fps, bitrate_kbps, codec, flags } => {
                w.u32(*display_id);
                w.u32(*width);
                w.u32(*height);
                w.u8(*fps);
                w.u32(*bitrate_kbps);
                w.u8(*codec);
                w.u8(*flags);
            }
            Message::StreamStarted { display_id, width, height, codec } => {
                w.u32(*display_id);
                w.u32(*width);
                w.u32(*height);
                w.u8(*codec);
            }
            Message::RequestKeyframe => {}
            Message::Resize { width, height } => {
                w.u32(*width);
                w.u32(*height);
            }
            Message::Ping { t } | Message::Pong { t } => w.u64(*t),
            Message::VideoFrame { pts_us, keyframe, data } => {
                w.u64(*pts_us);
                w.u8(*keyframe as u8);
                w.bytes(data);
            }
            Message::FrameAck { pts_us } => w.u64(*pts_us),
            Message::Key { usage, down } => {
                w.u16(*usage);
                w.u8(*down as u8);
            }
            Message::MouseMove { x, y } => {
                w.u16(*x);
                w.u16(*y);
            }
            Message::MouseButton { button, down } => {
                w.u8(*button);
                w.u8(*down as u8);
            }
            Message::Scroll { dx, dy, precise } => {
                w.f32(*dx);
                w.f32(*dy);
                w.u8(*precise as u8);
            }
            Message::Clipboard { kind, data } => {
                w.u8(*kind);
                w.bytes(data);
            }
            Message::Bye { reason } => w.str(reason),
        }
        let len = (w.0.len() - 4) as u32;
        w.0[..4].copy_from_slice(&len.to_le_bytes());
        w.0
    }

    /// Decodes a message from its type byte and payload. `Ok(None)` for unknown types.
    pub fn decode(ty: u8, payload: &[u8]) -> Result<Option<Message>, Error> {
        let mut r = Reader(payload);
        let m = match ty {
            0x01 => Message::Hello { version: r.u16()?, caps: r.u32()? },
            0x02 => Message::AuthChallenge { nonce: r.array()? },
            0x03 => Message::AuthResponse { mac: r.array()? },
            0x04 => Message::AuthResult { ok: r.u8()? != 0 },
            0x05 => {
                let n = r.u8()?;
                let mut displays = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    displays.push(DisplayInfo {
                        id: r.u32()?,
                        width_px: r.u32()?,
                        height_px: r.u32()?,
                        scale: r.f32()?,
                        is_main: r.u8()? != 0,
                        name: r.str()?,
                    });
                }
                Message::ServerInfo { displays }
            }
            0x10 => Message::StartStream {
                display_id: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                fps: r.u8()?,
                bitrate_kbps: r.u32()?,
                codec: r.u8()?,
                flags: r.u8()?,
            },
            0x11 => Message::StreamStarted { display_id: r.u32()?, width: r.u32()?, height: r.u32()?, codec: r.u8()? },
            0x12 => Message::RequestKeyframe,
            0x13 => Message::Resize { width: r.u32()?, height: r.u32()? },
            0x14 => Message::Ping { t: r.u64()? },
            0x15 => Message::Pong { t: r.u64()? },
            0x20 => Message::VideoFrame { pts_us: r.u64()?, keyframe: r.u8()? & 1 != 0, data: r.rest().to_vec() },
            0x21 => Message::FrameAck { pts_us: r.u64()? },
            0x30 => Message::Key { usage: r.u16()?, down: r.u8()? != 0 },
            0x31 => Message::MouseMove { x: r.u16()?, y: r.u16()? },
            0x32 => Message::MouseButton { button: r.u8()?, down: r.u8()? != 0 },
            0x33 => Message::Scroll { dx: r.f32()?, dy: r.f32()?, precise: r.u8()? != 0 },
            0x40 => Message::Clipboard { kind: r.u8()?, data: r.rest().to_vec() },
            0x7F => Message::Bye { reason: r.str()? },
            _ => return Ok(None),
        };
        Ok(Some(m))
    }
}

/// Parses the 4-byte length prefix, validating it.
pub fn frame_len(header: [u8; 4]) -> Result<usize, Error> {
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge(len));
    }
    Ok(len)
}

/// Incrementally splits a byte stream into messages.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Next complete message, skipping unknown types; `Ok(None)` when more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, Error> {
        loop {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let len = frame_len(self.buf[..4].try_into().unwrap())?;
            if self.buf.len() < 4 + len {
                return Ok(None);
            }
            let msg = Message::decode(self.buf[4], &self.buf[5..4 + len]);
            self.buf.drain(..4 + len);
            if let Some(m) = msg? {
                return Ok(Some(m));
            }
        }
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        let mut end = s.len().min(u16::MAX as usize);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.bytes(&s.as_bytes()[..end]);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        if self.0.len() < n {
            return Err(Error::Truncated);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    fn str(&mut self) -> Result<String, Error> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Error::InvalidUtf8)
    }
    fn rest(&mut self) -> &[u8] {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_messages() -> Vec<Message> {
        vec![
            Message::Hello { version: 1, caps: 7 },
            Message::AuthChallenge { nonce: [0xAB; 32] },
            Message::AuthResponse { mac: [0xCD; 32] },
            Message::AuthResult { ok: true },
            Message::ServerInfo {
                displays: vec![
                    DisplayInfo { id: 1, width_px: 3024, height_px: 1964, scale: 2.0, is_main: true, name: "Built-in Display".into() },
                    DisplayInfo { id: 5, width_px: 2560, height_px: 1440, scale: 1.0, is_main: false, name: "DELL ✓".into() },
                ],
            },
            Message::StartStream { display_id: 1, width: 1920, height: 1080, fps: 60, bitrate_kbps: 40_000, codec: 1, flags: 1 },
            Message::StreamStarted { display_id: 1, width: 1920, height: 1080, codec: 0 },
            Message::RequestKeyframe,
            Message::Resize { width: 800, height: 600 },
            Message::Ping { t: 123_456_789 },
            Message::Pong { t: u64::MAX },
            Message::VideoFrame { pts_us: 42, keyframe: true, data: vec![0, 0, 0, 1, 0x67] },
            Message::FrameAck { pts_us: 42 },
            Message::Key { usage: 0xE3, down: true },
            Message::MouseMove { x: 0, y: 65535 },
            Message::MouseButton { button: 2, down: false },
            Message::Scroll { dx: -1.5, dy: 3.25, precise: true },
            Message::Clipboard { kind: CLIPBOARD_TEXT, data: b"hello".to_vec() },
            Message::Bye { reason: "bye".into() },
        ]
    }

    #[test]
    fn round_trip_all() {
        for m in all_messages() {
            let mut f = Framer::default();
            f.push(&m.encode());
            assert_eq!(f.next_message().unwrap(), Some(m));
        }
    }

    #[test]
    fn wire_layout_matches_spec() {
        assert_eq!(Message::MouseMove { x: 0x0102, y: 0x0304 }.encode(), [5, 0, 0, 0, 0x31, 0x02, 0x01, 0x04, 0x03]);
        assert_eq!(Message::RequestKeyframe.encode(), [1, 0, 0, 0, 0x12]);
    }

    #[test]
    fn framer_handles_byte_splits_and_unknown_types() {
        let mut bytes = vec![2, 0, 0, 0, 0x99, 0xFF];
        bytes.extend(Message::Ping { t: 9 }.encode());
        bytes.extend(Message::Key { usage: 4, down: true }.encode());
        let mut f = Framer::default();
        let mut out = vec![];
        for b in bytes {
            f.push(&[b]);
            while let Some(m) = f.next_message().unwrap() {
                out.push(m);
            }
        }
        assert_eq!(out, vec![Message::Ping { t: 9 }, Message::Key { usage: 4, down: true }]);
    }

    #[test]
    fn rejects_huge_frames() {
        let mut f = Framer::default();
        f.push(&[0xFF, 0xFF, 0xFF, 0x7F, 0x20]);
        assert!(f.next_message().is_err());
    }

    #[test]
    fn truncated_payload_is_an_error() {
        assert_eq!(Message::decode(0x31, &[1, 2, 3]), Err(Error::Truncated));
    }
}
