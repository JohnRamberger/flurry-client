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
use flurry_proto::PORT;

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
    /// v0 line-doubles them instead of weaving fields.
    fn paste(&mut self, rgb: &[u8], iw: usize, ih: usize, row_offset: usize, interlaced: bool) {
        let w = self.image.size[0];
        let step = if interlaced { 2 } else { 1 };
        for r in 0..ih {
            let x = row_offset + r;
            if x >= w {
                break;
            }
            for c in 0..iw {
                let i = (r * iw + c) * 3;
                // 3DS framebuffers are BGR; the sysmodule JPEG-encodes the
                // raw bytes as if RGB, so swap back here.
                let px = egui::Color32::from_rgb(rgb[i + 2], rgb[i + 1], rgb[i]);
                let y = 239usize.saturating_sub(c * step);
                self.image.pixels[y * w + x] = px;
                if interlaced && y > 0 {
                    self.image.pixels[(y - 1) * w + x] = px; // line-double
                }
            }
        }
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
                let row_offset = img.chunk.map(|i| i as usize * ih).unwrap_or(0);
                let buf = if img.bottom { &mut bottom } else { &mut top };
                buf.paste(&rgb, iw, ih, row_offset, img.interlaced);
                emit(Event::Screen {
                    bottom: img.bottom,
                    image: buf.image.clone(),
                    bytes: legacy::HEADER_LEN + payload.len(),
                    chunk: img.chunk,
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
