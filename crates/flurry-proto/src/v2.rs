//! Protocol v2 (PROTOCOL.md Appendix B2): batched region streaming.
//!
//! One `SFRAME` per capture pass per screen carries every changed region in
//! a single packet (socket sends cost ~3 ms of 3DS IPC *each*, nearly
//! size-independent — batching is the transport win). Regions are addressed
//! in SCREEN coordinates; region pixel *data* stays in framebuffer order
//! (rotating pixels on the 3DS would burn the CPU the protocol exists to
//! save).
//!
//! ## Region data layout
//!
//! The 3DS framebuffer is column-major relative to the screen: one fb row
//! = one screen column, bottom-to-top. For a region (x, y, w, h) in screen
//! space (origin top-left), the payload contains `w` columns left→right;
//! each column holds `h` pixels ordered screen-bottom→top (the fb's native
//! direction). Equivalently: decoded-as-image dimensions are `h`×`w` and
//! the client applies the same rotation as legacy strips, offset by (x, y).
//!
//! JPEG regions decode to that same h×w raster. `Raw565Half` regions are
//! quarter-res: dimensions on the wire are (w/2, h/2); the client paints
//! each pixel as a 2×2 block.

/// SFRAME packet type byte (outside the legacy 0x01–0x7F / 0x80–0x8F space).
pub const SFRAME: u8 = 0x90;

/// SFRAME fixed header size on the wire.
pub const SFRAME_HEADER_LEN: usize = 8;
/// Per-region header size.
pub const REGION_HEADER_LEN: usize = 14;
/// Payload prefix ahead of the first region.
pub const PASS_HEADER_LEN: usize = 4;

/// Largest SFRAME payload either side may send (mirrors the New-3DS socket
/// buffer bound; anything larger indicates a desynced stream).
pub const MAX_PAYLOAD: u32 = 448 * 1024;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Raw RGB565 little-endian, framebuffer order.
    Raw565 = 0,
    /// Standard JPEG of the h×w raster.
    Jpeg = 1,
    /// Raw RGB565, both axes halved; client paints 2×2 per pixel.
    Raw565Half = 2,
    /// Raw 24-bit BGR8 (3 bytes/px), framebuffer order — 24/32bpp sources.
    RawBgr8 = 3,
}

/// Region flags.
pub mod region_flags {
    /// Interlace field B (odd sample phase); field A when clear.
    pub const FIELD_B: u8 = 1 << 0;
    /// Region is an interlaced field (half horizontal resolution).
    pub const INTERLACED: u8 = 1 << 1;
    /// Region content is 2x-downscaled on both axes (client paints 2x2).
    /// Implied for [`Codec::Raw565Half`]; required for downscaled JPEG.
    pub const DOWNSCALED: u8 = 1 << 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SFrameHeader {
    pub screen: u8,
    /// Increments per capture pass; receiver reports reference it.
    pub seq: u16,
    pub payload_len: u32,
}

/// Parse the 8-byte SFRAME header: `[0x90][screen][seq u16][len u32]`.
pub fn parse_sframe_header(buf: &[u8]) -> Result<SFrameHeader> {
    if buf.len() < SFRAME_HEADER_LEN {
        return Err(Error::Truncated);
    }
    if buf[0] != SFRAME {
        return Err(Error::UnknownType(buf[0]));
    }
    let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if payload_len > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge(payload_len));
    }
    Ok(SFrameHeader {
        screen: buf[1],
        seq: u16::from_le_bytes(buf[2..4].try_into().unwrap()),
        payload_len,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region<'a> {
    /// Screen-space rect, origin top-left.
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub codec: Codec,
    pub flags: u8,
    pub data: &'a [u8],
}

/// Parsed SFRAME payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SFramePayload<'a> {
    pub pass_flags: u8,
    pub regions: Vec<Region<'a>>,
}

/// Parse an SFRAME payload:
/// `[pass_flags][region_count][reserved u16]` then `region_count` regions of
/// `[x u16][y u16][w u16][h u16][codec][flags][len u32][data…]`.
pub fn parse_sframe_payload(payload: &[u8]) -> Result<SFramePayload<'_>> {
    if payload.len() < PASS_HEADER_LEN {
        return Err(Error::Truncated);
    }
    let pass_flags = payload[0];
    let count = payload[1] as usize;
    let mut regions = Vec::with_capacity(count);
    let mut off = PASS_HEADER_LEN;
    for _ in 0..count {
        if payload.len() < off + REGION_HEADER_LEN {
            return Err(Error::Truncated);
        }
        let r = &payload[off..];
        let len = u32::from_le_bytes(r[10..14].try_into().unwrap()) as usize;
        let codec = match r[8] {
            0 => Codec::Raw565,
            1 => Codec::Jpeg,
            2 => Codec::Raw565Half,
            3 => Codec::RawBgr8,
            v => return Err(Error::BadValue { field: "codec", value: v }),
        };
        off += REGION_HEADER_LEN;
        if payload.len() < off + len {
            return Err(Error::Truncated);
        }
        regions.push(Region {
            x: u16::from_le_bytes(r[0..2].try_into().unwrap()),
            y: u16::from_le_bytes(r[2..4].try_into().unwrap()),
            w: u16::from_le_bytes(r[4..6].try_into().unwrap()),
            h: u16::from_le_bytes(r[6..8].try_into().unwrap()),
            codec,
            flags: r[9],
            data: &payload[off..off + len],
        });
        off += len;
    }
    Ok(SFramePayload { pass_flags, regions })
}

/// Encode a full SFRAME (header + payload). Reference implementation for
/// tests and for the sysmodule port.
pub fn encode_sframe(screen: u8, seq: u16, pass_flags: u8, regions: &[Region<'_>]) -> Vec<u8> {
    let payload_len: usize = PASS_HEADER_LEN
        + regions
            .iter()
            .map(|r| REGION_HEADER_LEN + r.data.len())
            .sum::<usize>();
    let mut out = Vec::with_capacity(SFRAME_HEADER_LEN + payload_len);
    out.push(SFRAME);
    out.push(screen);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(payload_len as u32).to_le_bytes());
    out.push(pass_flags);
    out.push(regions.len() as u8);
    out.extend_from_slice(&[0, 0]);
    for r in regions {
        out.extend_from_slice(&r.x.to_le_bytes());
        out.extend_from_slice(&r.y.to_le_bytes());
        out.extend_from_slice(&r.w.to_le_bytes());
        out.extend_from_slice(&r.h.to_le_bytes());
        out.push(r.codec as u8);
        out.push(r.flags);
        out.extend_from_slice(&(r.data.len() as u32).to_le_bytes());
        out.extend_from_slice(r.data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sframe_roundtrip() {
        let a = Region {
            x: 100,
            y: 0,
            w: 100,
            h: 240,
            codec: Codec::Jpeg,
            flags: 0,
            data: &[0xFF, 0xD8, 0xFF],
        };
        let b = Region {
            x: 0,
            y: 60,
            w: 8,
            h: 60,
            codec: Codec::Raw565,
            flags: region_flags::FIELD_B | region_flags::INTERLACED,
            data: &[1, 2, 3, 4],
        };
        let bytes = encode_sframe(1, 0xBEEF, 0x00, &[a, b]);
        let hdr = parse_sframe_header(&bytes).unwrap();
        assert_eq!(hdr.screen, 1);
        assert_eq!(hdr.seq, 0xBEEF);
        assert_eq!(hdr.payload_len as usize, bytes.len() - SFRAME_HEADER_LEN);
        let p = parse_sframe_payload(&bytes[SFRAME_HEADER_LEN..]).unwrap();
        assert_eq!(p.regions, vec![a, b]);
    }

    #[test]
    fn empty_pass_is_legal() {
        let bytes = encode_sframe(0, 7, 0, &[]);
        let p = parse_sframe_payload(&bytes[SFRAME_HEADER_LEN..]).unwrap();
        assert!(p.regions.is_empty());
    }

    #[test]
    fn truncation_and_bad_codec_rejected() {
        let r = Region {
            x: 0, y: 0, w: 4, h: 4,
            codec: Codec::Raw565,
            flags: 0,
            data: &[0; 32],
        };
        let mut bytes = encode_sframe(0, 1, 0, &[r]);
        assert!(parse_sframe_payload(&bytes[SFRAME_HEADER_LEN..bytes.len() - 1]).is_err());
        bytes[SFRAME_HEADER_LEN + PASS_HEADER_LEN + 8] = 9; // codec
        assert!(matches!(
            parse_sframe_payload(&bytes[SFRAME_HEADER_LEN..]),
            Err(Error::BadValue { field: "codec", .. })
        ));
    }
}
