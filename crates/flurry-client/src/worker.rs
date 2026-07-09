//! Network worker: talks the legacy HzMod protocol to the 3DS on a
//! background thread pair and hands decoded frames to the UI.
//!
//! - writer thread: owns the write half, drains [`Cmd`]s into packets.
//! - reader thread: owns the read half, decodes IMAGE packets into
//!   persistent per-screen RGBA buffers and emits [`Event`]s.
//!
//! Disconnect: writer sends the legacy disconnect packet and shuts the
//! socket down, which unblocks the reader.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use flurry_proto::legacy::{self, pkt, ImageInfo, ScreenSet};
use flurry_proto::{v2, PORT};

/// UI → worker.
pub enum Cmd {
    SetQuality(u8),
    SetScreen(ScreenSet),
    SetInterlace(bool),
    SetStripSkip(bool),
    SetRefreshInterval(u8),
    SetFpsCap(u8),
    SetChunks(u8),
    SetStripSleep(u8),
    SetDownscale(bool),
    SetStatsEnabled(bool),
    SetV2Enabled(bool),
    Disconnect,
}

/// Worker → UI.
pub enum Event {
    Connected,
    /// Extended sysmodule announced its feature set.
    Capabilities(legacy::Announce),
    /// A screen buffer changed. `bottom` selects which texture to update;
    /// `bytes` is the wire size of the packet (for the bandwidth meter);
    /// `chunk` is the strip index for chunked (Old 3DS) frames, used by the
    /// client to infer strips-per-frame for the fps meter.
    Screen {
        bottom: bool,
        image: egui::ColorImage,
        bytes: usize,
        chunk: Option<u8>,
        /// Screen-space rects updated by this packet (x, y, w, h) — feeds
        /// the client's update-overlay debug view.
        rects: Vec<[u16; 4]>,
    },
    Stats(String),
    /// Non-fatal notice (3DS-side error text, unsupported format, ...).
    Info(String),
    /// Worker exited; reason is human-readable.
    Disconnected(String),
}

pub struct Worker {
    pub cmds: Sender<Cmd>,
    pub events: Receiver<Event>,
}

/// Spawn the worker. Returns immediately; connection outcome arrives as
/// [`Event::Connected`] or [`Event::Disconnected`].
pub fn spawn(addr: String, ctx: egui::Context, quality: u8, screen: ScreenSet, interlace: bool) -> Worker {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (event_tx, event_rx) = channel::<Event>();

    std::thread::spawn(move || {
        let emit = |ev: Event| {
            let _ = event_tx.send(ev);
            ctx.request_repaint();
        };

        let target = if addr.contains(':') {
            addr.clone()
        } else {
            format!("{addr}:{PORT}")
        };
        let stream = match connect(&target) {
            Ok(s) => s,
            Err(e) => {
                emit(Event::Disconnected(format!("connect to {target} failed: {e}")));
                return;
            }
        };

        // Initial settings, then INIT to start the stream.
        let mut wr = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                emit(Event::Disconnected(format!("socket clone failed: {e}")));
                return;
            }
        };
        let hello: Vec<u8> = [
            legacy::encode_quality(quality),
            legacy::encode_screen(screen),
            legacy::encode_format_tga(false),
            legacy::encode_interlace(interlace),
            legacy::encode_init(),
        ]
        .concat();
        if let Err(e) = wr.write_all(&hello) {
            emit(Event::Disconnected(format!("handshake write failed: {e}")));
            return;
        }
        emit(Event::Connected);

        // Writer thread: commands → packets.
        let writer_stream = wr.try_clone();
        std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                let (bytes, quit) = match cmd {
                    Cmd::SetQuality(q) => (legacy::encode_quality(q), false),
                    Cmd::SetScreen(s) => (legacy::encode_screen(s), false),
                    Cmd::SetInterlace(i) => (legacy::encode_interlace(i), false),
                    Cmd::SetStripSkip(s) => (legacy::encode_strip_skip(s), false),
                    Cmd::SetRefreshInterval(n) => (legacy::encode_refresh_interval(n), false),
                    Cmd::SetFpsCap(f) => (legacy::encode_fps_cap(f), false),
                    Cmd::SetChunks(c) => (legacy::encode_chunks(c), false),
                    Cmd::SetStripSleep(ms) => (legacy::encode_strip_sleep(ms), false),
                    Cmd::SetDownscale(d) => (legacy::encode_downscale(d), false),
                    Cmd::SetStatsEnabled(e) => (legacy::encode_stats_enabled(e), false),
                    Cmd::SetV2Enabled(e) => (legacy::encode_v2_enable(e), false),
                    Cmd::Disconnect => (legacy::encode_disconnect(), true),
                };
                let _ = wr.write_all(&bytes);
                if quit {
                    let _ = wr.shutdown(Shutdown::Both);
                    return;
                }
            }
            // UI dropped the sender (window closed): tell the 3DS goodbye.
            let _ = wr.write_all(&legacy::encode_disconnect());
            let _ = wr.shutdown(Shutdown::Both);
        });
        drop(writer_stream);

        // Reader loop on this thread.
        let reason = read_loop(stream, &emit);
        emit(Event::Disconnected(reason));
    });

    Worker {
        cmds: cmd_tx,
        events: event_rx,
    }
}

fn connect(target: &str) -> std::io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let addr = target
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("address did not resolve"))?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(4))
}

/// Persistent screen buffers the decoded pieces are pasted into: chunks
/// (Old 3DS), interlaced fields, and full frames all update the same image.
struct ScreenBuf {
    image: egui::ColorImage,
}

impl ScreenBuf {
    fn new(width: usize) -> ScreenBuf {
        ScreenBuf {
            image: egui::ColorImage::new([width, 240], vec![egui::Color32::from_gray(20); width * 240]),
        }
    }

    /// Paste one decoded RGB image.
    ///
    /// Legacy geometry (PROTOCOL.md §5.1 / Appendix A): the JPEG is in
    /// framebuffer orientation — decoded row `r` is one screen *column*,
    /// bottom-to-top. `x = row_offset + r`, `y = 239 - c`. Interlaced
    /// frames carry every other pixel of each fb row (image width 120);
    /// v0 line-doubles them instead of weaving fields. Quarter-res frames
    /// (`downscaled`) halve both axes; each pixel covers a 2x2 block.
    fn paste(
        &mut self,
        rgb: &[u8],
        iw: usize,
        ih: usize,
        row_offset: usize,
        interlaced: bool,
        downscaled: bool,
    ) {
        let w = self.image.size[0];
        let (xstep, ystep) = if downscaled {
            (2, 2)
        } else if interlaced {
            (1, 2)
        } else {
            (1, 1)
        };
        for r in 0..ih {
            let x = row_offset + r * xstep;
            if x >= w {
                break;
            }
            for c in 0..iw {
                let i = (r * iw + c) * 3;
                // 3DS framebuffers are BGR; the sysmodule JPEG-encodes the
                // raw bytes as if RGB, so swap back here.
                let px = egui::Color32::from_rgb(rgb[i + 2], rgb[i + 1], rgb[i]);
                let y = 239usize.saturating_sub(c * ystep);
                for dx in 0..xstep {
                    if x + dx >= w {
                        break;
                    }
                    self.image.pixels[y * w + x + dx] = px;
                    if ystep == 2 && y > 0 {
                        self.image.pixels[(y - 1) * w + x + dx] = px; // fill gap
                    }
                }
            }
        }
    }
}

impl ScreenBuf {
    /// Paste a protocol-v2 region (see `flurry_proto::v2` for the layout:
    /// screen-space rect, framebuffer-ordered pixel data — `w` columns
    /// left→right, each column bottom→top).
    fn paste_v2_region(&mut self, r: &v2::Region<'_>) -> Result<(), String> {
        let (rx, ry, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        let w = self.image.size[0];
        if rx + rw > w || ry + rh > 240 || rw == 0 || rh == 0 {
            return Err(format!("region out of bounds: {rx},{ry} {rw}x{rh}"));
        }
        let interlaced = r.flags & v2::region_flags::INTERLACED != 0;
        let field_b = (r.flags & v2::region_flags::FIELD_B != 0) as usize;
        // Vertical stride between successive samples in a column, and the
        // block each sample paints (fills interlace/downscale gaps).
        let scale = if matches!(r.codec, v2::Codec::Raw565Half)
            || r.flags & v2::region_flags::DOWNSCALED != 0
        {
            2
        } else {
            1
        };
        let ystep = scale * if interlaced { 2 } else { 1 };

        let mut put = |col: usize, sample: usize, px: egui::Color32| {
            let x0 = rx + col * scale;
            // Column data runs screen-bottom → top.
            let y_hi = (ry + rh).saturating_sub(1 + sample * ystep + field_b * scale);
            for dx in 0..scale {
                let x = x0 + dx;
                if x >= w {
                    break;
                }
                for dy in 0..ystep {
                    let y = match y_hi.checked_sub(dy) {
                        Some(y) if y >= ry => y,
                        _ => continue,
                    };
                    self.image.pixels[y * w + x] = px;
                }
            }
        };

        match r.codec {
            v2::Codec::Raw565 | v2::Codec::Raw565Half => {
                let cols = rw / scale;
                let colpx = (rh / scale) / if interlaced { 2 } else { 1 };
                if r.data.len() < cols * colpx * 2 {
                    return Err("raw region data short".into());
                }
                for i in 0..cols {
                    for j in 0..colpx {
                        let o = (i * colpx + j) * 2;
                        let v = u16::from_le_bytes([r.data[o], r.data[o + 1]]);
                        // Channel order mirrors the legacy BGR finding; if
                        // raw regions come out swapped, flip r/b here.
                        let b = ((v >> 11) & 0x1F) as u8;
                        let g = ((v >> 5) & 0x3F) as u8;
                        let rr = (v & 0x1F) as u8;
                        put(i, j, egui::Color32::from_rgb(rr << 3, g << 2, b << 3));
                    }
                }
            }
            v2::Codec::Jpeg => {
                let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(r.data));
                let rgb = dec.decode().map_err(|e| format!("jpeg: {e}"))?;
                let Some((iw, ih)) = dec.dimensions() else {
                    return Err("jpeg: no dimensions".into());
                };
                if rgb.len() < iw * ih * 3 {
                    return Err("jpeg: short decode".into());
                }
                // Image rows = columns of the region; image cols = pixels
                // along each column (possibly one interlace field).
                for row in 0..ih.min(rw) {
                    for c in 0..iw {
                        let o = (row * iw + c) * 3;
                        // BGR framebuffer, encoded as if RGB: swap back.
                        let px = egui::Color32::from_rgb(rgb[o + 2], rgb[o + 1], rgb[o]);
                        put(row, c, px);
                    }
                }
            }
        }
        Ok(())
    }
}

fn read_loop(mut stream: TcpStream, emit: &dyn Fn(Event)) -> String {
    let mut top = ScreenBuf::new(400);
    let mut bottom = ScreenBuf::new(320);
    let mut warned_tga = false;
    let mut header = [0u8; legacy::HEADER_LEN];

    loop {
        if let Err(e) = stream.read_exact(&mut header) {
            return format!("connection closed: {e}");
        }

        // Protocol v2 SFRAME shares the 8-byte header size; the type byte
        // 0x90 sits outside the legacy packet-type space.
        if header[0] == v2::SFRAME {
            let hdr = match v2::parse_sframe_header(&header) {
                Ok(h) => h,
                Err(e) => return format!("v2 protocol error: {e}"),
            };
            let mut payload = vec![0u8; hdr.payload_len as usize];
            if let Err(e) = stream.read_exact(&mut payload) {
                return format!("connection closed mid-sframe: {e}");
            }
            let sf = match v2::parse_sframe_payload(&payload) {
                Ok(s) => s,
                Err(e) => return format!("v2 payload error: {e}"),
            };
            let bottom_screen = hdr.screen == 1;
            if sf.regions.is_empty() {
                continue; // heartbeat pass
            }
            let buf = if bottom_screen { &mut bottom } else { &mut top };
            for region in &sf.regions {
                if let Err(e) = buf.paste_v2_region(region) {
                    emit(Event::Info(format!("v2 region error: {e}")));
                }
            }
            // Stage-2 sysmodules send one strip per SFRAME; expose its
            // strip index (x / width) so the fps meter can infer strips-
            // per-frame. True multi-region passes count as whole frames.
            let chunk = match sf.regions.as_slice() {
                [only] if only.w > 0 && only.w < 400 => Some((only.x / only.w) as u8),
                _ => None,
            };
            let rects = sf.regions.iter().map(|r| [r.x, r.y, r.w, r.h]).collect();
            emit(Event::Screen {
                bottom: bottom_screen,
                image: buf.image.clone(),
                bytes: v2::SFRAME_HEADER_LEN + payload.len(),
                chunk,
                rects,
            });
            continue;
        }

        let info = match legacy::parse_header(&header) {
            Ok(i) => i,
            Err(e) => return format!("protocol error: {e}"),
        };
        let mut payload = vec![0u8; info.payload_len as usize];
        if let Err(e) = stream.read_exact(&mut payload) {
            return format!("connection closed mid-packet: {e}");
        }

        match info.ptype {
            pkt::IMAGE => {
                let img = ImageInfo::from_packet(&info);
                if img.tga {
                    if !warned_tga {
                        warned_tga = true;
                        emit(Event::Info("TGA frames not supported yet — switch format to JPEG".into()));
                    }
                    continue;
                }
                let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&payload[..]));
                let rgb = match dec.decode() {
                    Ok(px) => px,
                    Err(e) => {
                        emit(Event::Info(format!("JPEG decode failed: {e}")));
                        continue;
                    }
                };
                let Some((iw, ih)) = dec.dimensions() else {
                    continue;
                };
                if rgb.len() < iw * ih * 3 {
                    emit(Event::Info(format!(
                        "unexpected decode output: {}x{} but {} bytes",
                        iw,
                        ih,
                        rgb.len()
                    )));
                    continue;
                }
                let scale = if img.downscaled { 2 } else { 1 };
                let row_offset = img.chunk.map(|i| i as usize * ih * scale).unwrap_or(0);
                let buf = if img.bottom { &mut bottom } else { &mut top };
                buf.paste(&rgb, iw, ih, row_offset, img.interlaced, img.downscaled);
                let rects = vec![[row_offset as u16, 0, (ih * scale) as u16, 240]];
                emit(Event::Screen {
                    bottom: img.bottom,
                    image: buf.image.clone(),
                    bytes: legacy::HEADER_LEN + payload.len(),
                    chunk: img.chunk,
                    rects,
                });
            }
            pkt::META => match info.subtype {
                legacy::meta::STATS => {
                    emit(Event::Stats(String::from_utf8_lossy(&payload).into_owned()))
                }
                legacy::meta::ANNOUNCE => {
                    if let Ok(a) = legacy::Announce::parse(&payload) {
                        emit(Event::Capabilities(a));
                    }
                }
                _ => emit(Event::Info(format!(
                    "3DS: {}",
                    String::from_utf8_lossy(&payload)
                ))),
            },
            // Unknown packet types: payload already consumed, skip.
            _ => {}
        }
    }
}
