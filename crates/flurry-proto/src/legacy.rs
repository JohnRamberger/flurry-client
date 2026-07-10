//! Legacy HzMod/ChirunoMod protocol (PROTOCOL.md Appendix A).
//!
//! What the pre-rewrite Flurry sysmodule speaks on the wire. Client-side
//! view only: encode PC→3DS control messages, decode 3DS→PC packets.
//! Kept alongside the v1 codec so the client can talk to sysmodules built
//! before the protocol rewrite.
//!
//! Framing: `[type:u8][subtype:u8][subtypeB:u8][unused:u8][size:u32 LE]`
//! then `size` payload bytes.

use crate::{Error, Result};

pub const HEADER_LEN: usize = 8;
/// New-3DS send buffer is 448 KB; anything bigger means desync.
pub const MAX_PAYLOAD: u32 = 448 * 1024;

/// Packet type bytes.
pub mod pkt {
    /// 3DS → PC: encoded image.
    pub const IMAGE: u8 = 0x01;
    /// PC → 3DS: start streaming.
    pub const INIT: u8 = 0x02;
    /// PC → 3DS: disconnect.
    pub const DISCONNECT: u8 = 0x03;
    /// PC → 3DS: setting (subtype selects which).
    pub const SETTING: u8 = 0x04;
    /// Both directions: debug/stats/error text (subtype selects which).
    pub const META: u8 = 0xFF;
}

/// `SETTING` subtypes. 0x06+ are Flurry extensions (PROTOCOL.md "Legacy
/// extensions"); pre-extension sysmodules ignore them.
pub mod setting {
    pub const QUALITY: u8 = 0x01; // u8 payload, 1–100
    pub const CPU_CAP: u8 = 0x02; // u8 payload; dummied out on the 3DS
    pub const SCREEN: u8 = 0x03; // u8 payload: 1 top, 2 bottom, 3 both
    pub const FORMAT: u8 = 0x04; // u8 payload: 0 JPEG, 1 TGA
    pub const INTERLACE: u8 = 0x05; // u8 payload: bool
    pub const STRIP_SKIP: u8 = 0x06; // u8 payload: bool — skip unchanged strips
    pub const REFRESH_INTERVAL: u8 = 0x07; // u8 payload: force-send every N frames (0 = never)
    pub const FPS_CAP: u8 = 0x08; // u8 payload: target fps (0 = uncapped)
    pub const CHUNKS: u8 = 0x09; // u8 payload: strips per screen on Old 3DS (2, 4 or 8)
    pub const STRIP_SLEEP: u8 = 0x0A; // u8 payload: ms pause between strips (0-20)
    pub const DOWNSCALE: u8 = 0x0B; // u8 payload: bool — quarter-res mode (Old 3DS)
    pub const STATS_ENABLE: u8 = 0x0C; // u8 payload: bool — 1 Hz perf stats packets
    pub const V2_ENABLE: u8 = 0x0F; // u8 payload: bool — switch to protocol v2 framing
    pub const GRID_COLS: u8 = 0x10; // u8 payload: dirty-grid columns per screen (4/8/16)
    pub const GRID_ROWS: u8 = 0x11; // u8 payload: dirty-grid rows per screen (1/2/4/8)
    pub const CAPTURE: u8 = 0x12; // u8 payload: capture backend (0 DMA, 1 GPU)
}

/// Second feature byte (announce payload byte 2; absent on older builds).
pub mod feature2 {
    /// Dirty-grid geometry settings (GRID_COLS / GRID_ROWS).
    pub const CELL_GRID: u8 = 1 << 0;
    /// Selectable capture backend (CAPTURE).
    pub const CAPTURE: u8 = 1 << 1;
}

/// Feature bits carried by the [`meta::ANNOUNCE`] packet.
pub mod feature {
    pub const STRIP_SKIP: u8 = 1 << 0;
    pub const FPS_CAP: u8 = 1 << 1;
    pub const OLD3DS_INTERLACE: u8 = 1 << 2;
    pub const CHUNKS: u8 = 1 << 3;
    pub const STRIP_SLEEP: u8 = 1 << 4;
    pub const DOWNSCALE: u8 = 1 << 5;
    /// Stats are opt-in via [`setting::STATS_ENABLE`]. Sysmodules without
    /// this bit but with an announce stream stats unconditionally.
    pub const STATS_TOGGLE: u8 = 1 << 6;
    /// Protocol v2 (SFRAME region streaming) available via
    /// [`setting::V2_ENABLE`].
    pub const V2: u8 = 1 << 7;
}

pub fn encode_v2_enable(on: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::V2_ENABLE, &[on as u8])
}

pub fn encode_grid_cols(cols: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::GRID_COLS, &[cols])
}

pub fn encode_grid_rows(rows: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::GRID_ROWS, &[rows])
}

pub fn encode_capture(method: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::CAPTURE, &[method])
}

/// Legacy screen-select values (1-based, unlike v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSet {
    Top = 1,
    Bottom = 2,
    Both = 3,
}

fn packet(ptype: u8, subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(ptype);
    out.push(subtype);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn encode_init() -> Vec<u8> {
    packet(pkt::INIT, 0, &[])
}

pub fn encode_disconnect() -> Vec<u8> {
    packet(pkt::DISCONNECT, 0, &[])
}

pub fn encode_quality(quality: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::QUALITY, &[quality.clamp(1, 100)])
}

pub fn encode_screen(screen: ScreenSet) -> Vec<u8> {
    packet(pkt::SETTING, setting::SCREEN, &[screen as u8])
}

/// `false` = JPEG, `true` = TGA.
pub fn encode_format_tga(tga: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::FORMAT, &[tga as u8])
}

pub fn encode_interlace(on: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::INTERLACE, &[on as u8])
}

pub fn encode_strip_skip(on: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::STRIP_SKIP, &[on as u8])
}

pub fn encode_refresh_interval(frames: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::REFRESH_INTERVAL, &[frames])
}

pub fn encode_fps_cap(fps: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::FPS_CAP, &[fps])
}

pub fn encode_chunks(chunks: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::CHUNKS, &[chunks])
}

pub fn encode_strip_sleep(ms: u8) -> Vec<u8> {
    packet(pkt::SETTING, setting::STRIP_SLEEP, &[ms])
}

pub fn encode_downscale(on: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::DOWNSCALE, &[on as u8])
}

pub fn encode_stats_enabled(on: bool) -> Vec<u8> {
    packet(pkt::SETTING, setting::STATS_ENABLE, &[on as u8])
}

/// Parsed legacy framing header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub ptype: u8,
    pub subtype: u8,
    pub subtype_b: u8,
    pub payload_len: u32,
}

pub fn parse_header(buf: &[u8]) -> Result<PacketInfo> {
    if buf.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if payload_len > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge(payload_len));
    }
    Ok(PacketInfo {
        ptype: buf[0],
        subtype: buf[1],
        subtype_b: buf[2],
        payload_len,
    })
}

/// Decoded metadata of an `IMAGE` packet, unpacked from the subtype flag
/// bits and `subtypeB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// 0 = top, 1 = bottom (subtype bit 4).
    pub bottom: bool,
    /// Subtype bit 3: payload is TGA instead of JPEG.
    pub tga: bool,
    /// GSP source pixel format (subtype bits 0–2).
    pub pixfmt: u8,
    /// Subtype bit 5.
    pub interlaced: bool,
    /// Subtype bit 6 (only meaningful when `interlaced`).
    pub interlace_phase: bool,
    /// Subtype bit 7 (Flurry extension): quarter-res frame — both axes
    /// halved on the 3DS; the client scales each pixel to 2x2.
    pub downscaled: bool,
    /// Old-3DS chunk index 0–7; `None` when the packet is a full frame.
    pub chunk: Option<u8>,
}

impl ImageInfo {
    pub fn from_packet(info: &PacketInfo) -> ImageInfo {
        ImageInfo {
            bottom: info.subtype & 0b0001_0000 != 0,
            tga: info.subtype & 0b0000_1000 != 0,
            pixfmt: info.subtype & 0b0000_0111,
            interlaced: info.subtype & 0b0010_0000 != 0,
            interlace_phase: info.subtype & 0b0100_0000 != 0,
            downscaled: info.subtype & 0b1000_0000 != 0,
            // Old 3DS sets subtypeB = 0b1000 + chunk_index.
            chunk: (info.subtype_b & 0b0000_1000 != 0).then_some(info.subtype_b & 0b0111),
        }
    }

    /// Physical screen width in pixels (400 top / 320 bottom).
    pub fn screen_width(&self) -> usize {
        if self.bottom {
            320
        } else {
            400
        }
    }
}

/// `META` (0xFF) subtypes from the 3DS.
pub mod meta {
    pub const ERROR: u8 = 0x00;
    pub const STATS: u8 = 0x03;
    /// Flurry extension: sent once on connect by extended sysmodules.
    /// Payload: `[announce_rev: u8][feature bits: u8]` (see [`super::feature`]).
    /// Absence within ~1 s of connecting means a pre-extension sysmodule.
    pub const ANNOUNCE: u8 = 0x04;
}

/// Parsed [`meta::ANNOUNCE`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub revision: u8,
    pub features: u8,
    /// Second feature byte; 0 on sysmodules that predate it.
    pub features2: u8,
}

impl Announce {
    pub fn parse(payload: &[u8]) -> Result<Announce> {
        if payload.len() < 2 {
            return Err(Error::Truncated);
        }
        Ok(Announce {
            revision: payload[0],
            features: payload[1],
            features2: payload.get(2).copied().unwrap_or(0),
        })
    }

    pub fn has(&self, feature_bit: u8) -> bool {
        self.features & feature_bit != 0
    }

    pub fn has2(&self, feature_bit: u8) -> bool {
        self.features2 & feature_bit != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_packets_match_wire_format() {
        assert_eq!(encode_init(), vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_disconnect(), vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            encode_quality(70),
            vec![0x04, 0x01, 0, 0, 1, 0, 0, 0, 70]
        );
        assert_eq!(
            encode_screen(ScreenSet::Both),
            vec![0x04, 0x03, 0, 0, 1, 0, 0, 0, 3]
        );
        assert_eq!(
            encode_interlace(true),
            vec![0x04, 0x05, 0, 0, 1, 0, 0, 0, 1]
        );
        assert_eq!(encode_quality(0)[8], 1, "quality clamped to 1..=100");
    }

    #[test]
    fn image_flags_unpack() {
        // Bottom screen, JPEG, RGB565, interlaced, phase set.
        let info = PacketInfo {
            ptype: pkt::IMAGE,
            subtype: 0b0111_0010,
            subtype_b: 0,
            payload_len: 4,
        };
        let img = ImageInfo::from_packet(&info);
        assert!(img.bottom && img.interlaced && img.interlace_phase && !img.tga);
        assert_eq!(img.pixfmt, 2);
        assert_eq!(img.chunk, None);
        assert_eq!(img.screen_width(), 320);

        // Old-3DS chunk 5, top screen, TGA.
        let info = PacketInfo {
            ptype: pkt::IMAGE,
            subtype: 0b0000_1001,
            subtype_b: 0b0000_1101,
            payload_len: 4,
        };
        let img = ImageInfo::from_packet(&info);
        assert!(!img.bottom && img.tga);
        assert_eq!(img.chunk, Some(5));
    }

    #[test]
    fn extension_packets_and_announce() {
        assert_eq!(
            encode_strip_skip(true),
            vec![0x04, 0x06, 0, 0, 1, 0, 0, 0, 1]
        );
        assert_eq!(
            encode_refresh_interval(64),
            vec![0x04, 0x07, 0, 0, 1, 0, 0, 0, 64]
        );
        assert_eq!(encode_fps_cap(30), vec![0x04, 0x08, 0, 0, 1, 0, 0, 0, 30]);

        let a = Announce::parse(&[1, feature::STRIP_SKIP | feature::FPS_CAP]).unwrap();
        assert_eq!(a.revision, 1);
        assert!(a.has(feature::STRIP_SKIP) && a.has(feature::FPS_CAP));
        assert!(!a.has(feature::OLD3DS_INTERLACE));
        assert_eq!(Announce::parse(&[1]).unwrap_err(), Error::Truncated);
    }

    #[test]
    fn header_roundtrip() {
        let p = encode_quality(90);
        let info = parse_header(&p).unwrap();
        assert_eq!(info.ptype, pkt::SETTING);
        assert_eq!(info.subtype, setting::QUALITY);
        assert_eq!(info.payload_len, 1);
    }
}
