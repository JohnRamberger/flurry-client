//! Flurry streaming protocol codec.
//!
//! Implements the wire format defined in PROTOCOL.md (v1 **draft** — see the
//! flurry repo). Pure encode/decode over byte slices; no I/O, so it can be
//! unit-tested and reused by any transport layer.
//!
//! Framing: every message is an 8-byte header (`[type:u8][reserved:u8*3]
//! [length:u32 LE]`) followed by `length` payload bytes. All integers are
//! little-endian.

pub mod legacy;
pub mod v2;

/// TCP port the 3DS listens on.
pub const PORT: u16 = 6464;
/// `HELLO` magic, ASCII "FLRY".
pub const MAGIC: [u8; 4] = *b"FLRY";
/// Protocol version this crate implements.
pub const PROTO_VERSION: u16 = 1;
/// Framing header size in bytes.
pub const HEADER_LEN: usize = 8;
/// Largest payload either side may send (New 3DS send-buffer size).
/// Anything larger indicates a desynced or hostile stream.
pub const MAX_PAYLOAD: u32 = 448 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than the field being decoded requires.
    Truncated,
    /// `length` in a framing header exceeds [`MAX_PAYLOAD`].
    PayloadTooLarge(u32),
    /// `HELLO` magic was not "FLRY".
    BadMagic([u8; 4]),
    /// Enum field held a value outside its defined range.
    BadValue { field: &'static str, value: u8 },
    /// Message type not defined by this protocol version. Per PROTOCOL.md
    /// these are skipped, not fatal; surfaced so the caller can log them.
    UnknownType(u8),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => write!(f, "message truncated"),
            Error::PayloadTooLarge(n) => write!(f, "payload length {n} exceeds maximum"),
            Error::BadMagic(m) => write!(f, "bad HELLO magic {m:02x?}"),
            Error::BadValue { field, value } => write!(f, "invalid value {value} for {field}"),
            Error::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Message type bytes. Client→server 0x01–0x7F, server→client 0x80–0xFE,
/// 0xFF = ERROR (either direction).
pub mod msg_type {
    pub const CONFIG: u8 = 0x02;
    pub const START: u8 = 0x03;
    pub const STOP: u8 = 0x04;
    pub const DISCONNECT: u8 = 0x05;
    pub const DEBUG: u8 = 0x7F;
    pub const HELLO: u8 = 0x80;
    pub const FRAME: u8 = 0x81;
    pub const STATS: u8 = 0x82;
    pub const ERROR: u8 = 0xFF;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    New3ds = 0,
    Old3ds = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenSelect {
    #[default]
    Top = 0,
    Bottom = 1,
    Both = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    #[default]
    Jpeg = 0,
    Tga = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// 400×240
    Top = 0,
    /// 320×240
    Bottom = 1,
}

impl Screen {
    pub fn width(self) -> u32 {
        match self {
            Screen::Top => 400,
            Screen::Bottom => 320,
        }
    }
    pub const HEIGHT: u32 = 240;
}

/// GSP source framebuffer format. Informative only — image payloads are
/// self-describing RGB regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    Rgba8 = 0,
    Rgb8 = 1,
    Rgb565 = 2,
    Rgb5a1 = 3,
    Rgba4 = 4,
}

/// Which pixels of each framebuffer row a `FRAME` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interlace {
    Progressive = 0,
    /// Framebuffer-row pixels 0, 2, 4, … 238.
    FieldA = 1,
    /// Framebuffer-row pixels 1, 3, 5, … 239.
    FieldB = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub proto_version: u16,
    pub device: Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// JPEG quality 1–100.
    pub quality: u8,
    pub screens: ScreenSelect,
    pub format: ImageFormat,
    pub interlace: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            quality: 70,
            screens: ScreenSelect::Top,
            format: ImageFormat::Jpeg,
            interlace: false,
        }
    }
}

/// Metadata prefix of a `FRAME` payload; the encoded image bytes follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub screen: Screen,
    pub format: ImageFormat,
    pub pixfmt: PixFmt,
    pub interlace: Interlace,
    pub chunk_index: u8,
    pub chunk_count: u8,
    pub frame_id: u16,
}

pub const FRAME_HEADER_LEN: usize = 8;

/// A fully decoded protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Config(Config),
    Start,
    Stop,
    Disconnect,
    Debug(String),
    Frame { header: FrameHeader, image: Vec<u8> },
    Stats(String),
    Error { code: u8, message: String },
}

impl Message {
    pub fn msg_type(&self) -> u8 {
        match self {
            Message::Hello(_) => msg_type::HELLO,
            Message::Config(_) => msg_type::CONFIG,
            Message::Start => msg_type::START,
            Message::Stop => msg_type::STOP,
            Message::Disconnect => msg_type::DISCONNECT,
            Message::Debug(_) => msg_type::DEBUG,
            Message::Frame { .. } => msg_type::FRAME,
            Message::Stats(_) => msg_type::STATS,
            Message::Error { .. } => msg_type::ERROR,
        }
    }

    /// Encode as a complete framed message (header + payload).
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(self.msg_type());
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn encode_payload(&self) -> Vec<u8> {
        match self {
            Message::Hello(h) => {
                let mut p = Vec::with_capacity(8);
                p.extend_from_slice(&MAGIC);
                p.extend_from_slice(&h.proto_version.to_le_bytes());
                p.push(h.device as u8);
                p.push(0);
                p
            }
            Message::Config(c) => vec![
                c.quality,
                c.screens as u8,
                c.format as u8,
                c.interlace as u8,
            ],
            Message::Start | Message::Stop | Message::Disconnect => Vec::new(),
            Message::Debug(s) | Message::Stats(s) => s.as_bytes().to_vec(),
            Message::Frame { header, image } => {
                let mut p = Vec::with_capacity(FRAME_HEADER_LEN + image.len());
                p.push(header.screen as u8);
                p.push(header.format as u8);
                p.push(header.pixfmt as u8);
                p.push(header.interlace as u8);
                p.push(header.chunk_index);
                p.push(header.chunk_count);
                p.extend_from_slice(&header.frame_id.to_le_bytes());
                p.extend_from_slice(image);
                p
            }
            Message::Error { code, message } => {
                let mut p = Vec::with_capacity(1 + message.len());
                p.push(*code);
                p.extend_from_slice(message.as_bytes());
                p
            }
        }
    }

    /// Decode a message from its type byte and payload.
    ///
    /// Per PROTOCOL.md, payloads longer than the known fields are accepted
    /// and the trailing bytes ignored.
    pub fn decode(msg_type: u8, payload: &[u8]) -> Result<Message> {
        match msg_type {
            msg_type::HELLO => {
                let p = need(payload, 8)?;
                let magic: [u8; 4] = p[0..4].try_into().unwrap();
                if magic != MAGIC {
                    return Err(Error::BadMagic(magic));
                }
                Ok(Message::Hello(Hello {
                    proto_version: u16::from_le_bytes(p[4..6].try_into().unwrap()),
                    device: match p[6] {
                        0 => Device::New3ds,
                        1 => Device::Old3ds,
                        v => return Err(Error::BadValue { field: "device", value: v }),
                    },
                }))
            }
            msg_type::CONFIG => {
                let p = need(payload, 4)?;
                Ok(Message::Config(Config {
                    quality: p[0].clamp(1, 100),
                    screens: match p[1] {
                        0 => ScreenSelect::Top,
                        1 => ScreenSelect::Bottom,
                        2 => ScreenSelect::Both,
                        v => return Err(Error::BadValue { field: "screens", value: v }),
                    },
                    format: decode_image_format(p[2])?,
                    interlace: p[3] != 0,
                }))
            }
            msg_type::START => Ok(Message::Start),
            msg_type::STOP => Ok(Message::Stop),
            msg_type::DISCONNECT => Ok(Message::Disconnect),
            msg_type::DEBUG => Ok(Message::Debug(text(payload))),
            msg_type::STATS => Ok(Message::Stats(text(payload))),
            msg_type::FRAME => {
                let p = need(payload, FRAME_HEADER_LEN)?;
                Ok(Message::Frame {
                    header: FrameHeader {
                        screen: match p[0] {
                            0 => Screen::Top,
                            1 => Screen::Bottom,
                            v => return Err(Error::BadValue { field: "screen", value: v }),
                        },
                        format: decode_image_format(p[1])?,
                        pixfmt: match p[2] {
                            0 => PixFmt::Rgba8,
                            1 => PixFmt::Rgb8,
                            2 => PixFmt::Rgb565,
                            3 => PixFmt::Rgb5a1,
                            4 => PixFmt::Rgba4,
                            v => return Err(Error::BadValue { field: "pixfmt", value: v }),
                        },
                        interlace: match p[3] {
                            0 => Interlace::Progressive,
                            1 => Interlace::FieldA,
                            2 => Interlace::FieldB,
                            v => return Err(Error::BadValue { field: "interlace", value: v }),
                        },
                        chunk_index: p[4],
                        chunk_count: p[5].max(1),
                        frame_id: u16::from_le_bytes(p[6..8].try_into().unwrap()),
                    },
                    image: payload[FRAME_HEADER_LEN..].to_vec(),
                })
            }
            msg_type::ERROR => {
                let p = need(payload, 1)?;
                Ok(Message::Error {
                    code: p[0],
                    message: text(&p[1..]),
                })
            }
            t => Err(Error::UnknownType(t)),
        }
    }
}

fn need(payload: &[u8], n: usize) -> Result<&[u8]> {
    if payload.len() < n {
        Err(Error::Truncated)
    } else {
        Ok(payload)
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_image_format(v: u8) -> Result<ImageFormat> {
    match v {
        0 => Ok(ImageFormat::Jpeg),
        1 => Ok(ImageFormat::Tga),
        v => Err(Error::BadValue { field: "format", value: v }),
    }
}

/// A parsed framing header: message type + payload length still to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub msg_type: u8,
    pub payload_len: u32,
}

/// Parse the 8-byte framing header. Validates `length` against
/// [`MAX_PAYLOAD`]; the caller then reads exactly `payload_len` bytes and
/// hands them to [`Message::decode`].
pub fn parse_header(buf: &[u8]) -> Result<FrameInfo> {
    let b = need(buf, HEADER_LEN)?;
    let payload_len = u32::from_le_bytes(b[4..8].try_into().unwrap());
    if payload_len > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge(payload_len));
    }
    Ok(FrameInfo {
        msg_type: b[0],
        payload_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let bytes = msg.encode();
        let info = parse_header(&bytes).unwrap();
        assert_eq!(info.msg_type, msg.msg_type());
        assert_eq!(info.payload_len as usize, bytes.len() - HEADER_LEN);
        let decoded = Message::decode(info.msg_type, &bytes[HEADER_LEN..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn roundtrip_all_messages() {
        roundtrip(Message::Hello(Hello {
            proto_version: PROTO_VERSION,
            device: Device::Old3ds,
        }));
        roundtrip(Message::Config(Config::default()));
        roundtrip(Message::Config(Config {
            quality: 100,
            screens: ScreenSelect::Both,
            format: ImageFormat::Tga,
            interlace: true,
        }));
        roundtrip(Message::Start);
        roundtrip(Message::Stop);
        roundtrip(Message::Disconnect);
        roundtrip(Message::Debug("hello".into()));
        roundtrip(Message::Stats("fps=24\nencode_ms=11".into()));
        roundtrip(Message::Frame {
            header: FrameHeader {
                screen: Screen::Bottom,
                format: ImageFormat::Jpeg,
                pixfmt: PixFmt::Rgb565,
                interlace: Interlace::FieldB,
                chunk_index: 3,
                chunk_count: 8,
                frame_id: 0xBEEF,
            },
            image: vec![0xFF, 0xD8, 0xFF, 0xE0],
        });
        roundtrip(Message::Error {
            code: 0,
            message: "boom".into(),
        });
    }

    #[test]
    fn header_layout_is_exact() {
        let bytes = Message::Start.encode();
        assert_eq!(bytes, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);

        let bytes = Message::Debug("ab".into()).encode();
        assert_eq!(bytes[0], 0x7F);
        assert_eq!(&bytes[4..8], &2u32.to_le_bytes());
        assert_eq!(&bytes[8..], b"ab");
    }

    #[test]
    fn hello_magic_checked() {
        let mut bytes = Message::Hello(Hello {
            proto_version: 1,
            device: Device::New3ds,
        })
        .encode();
        bytes[8] = b'X';
        let err = Message::decode(msg_type::HELLO, &bytes[HEADER_LEN..]).unwrap_err();
        assert!(matches!(err, Error::BadMagic(_)));
    }

    #[test]
    fn longer_payloads_accepted() {
        // Extensibility rule: unknown trailing bytes are ignored.
        let mut bytes = Message::Config(Config::default()).encode();
        bytes.extend_from_slice(&[9, 9, 9]); // future fields
        let decoded = Message::decode(msg_type::CONFIG, &bytes[HEADER_LEN..]).unwrap();
        assert_eq!(decoded, Message::Config(Config::default()));
    }

    #[test]
    fn truncated_payload_rejected() {
        assert_eq!(
            Message::decode(msg_type::CONFIG, &[70, 0]).unwrap_err(),
            Error::Truncated
        );
        assert_eq!(parse_header(&[0x03, 0, 0]).unwrap_err(), Error::Truncated);
    }

    #[test]
    fn oversized_payload_rejected() {
        let mut buf = vec![0x81, 0, 0, 0];
        buf.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert_eq!(
            parse_header(&buf).unwrap_err(),
            Error::PayloadTooLarge(MAX_PAYLOAD + 1)
        );
    }

    #[test]
    fn unknown_type_surfaced() {
        assert_eq!(
            Message::decode(0x60, &[]).unwrap_err(),
            Error::UnknownType(0x60)
        );
    }
}
