//! Flurry PC client — eframe/egui GUI.
//!
//! Speaks the legacy HzMod protocol (via `flurry_proto::legacy`) so it works
//! against current Flurry sysmodule builds; will switch to the redesigned
//! protocol when the 3DS side is rewritten. Extended sysmodules announce a
//! feature set on connect which unlocks the advanced tuning knobs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bench;
mod profiles;
mod quality;
mod worker;

use std::collections::VecDeque;
use std::time::Instant;

use flurry_proto::legacy::{feature, Announce, ScreenSet};
use profiles::{Device, Profile, DEVICE_TYPES};
use serde::{Deserialize, Serialize};
use worker::{Cmd, Event, Worker};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_title("Flurry"),
        ..Default::default()
    };
    eframe::run_native(
        "Flurry",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

/// All tunable stream settings. Serialized into profiles, diffed against the
/// last values sent to the worker so edits apply live.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Master quality↔FPS balance, 0.0 = max FPS … 1.0 = max quality.
    /// Drives the knobs below unless `custom`.
    pub master: f32,
    /// User touched an advanced knob; master slider stops driving them.
    pub custom: bool,
    pub quality: u8,
    /// Legacy screen-select value: 1 top, 2 bottom, 3 both.
    pub screen: u8,
    pub interlace: bool,
    pub strip_skip: bool,
    /// Force-send every N frames per strip (0 = never). Extension knob.
    pub refresh_interval: u8,
    /// Target fps, 0 = uncapped. Extension knob.
    pub fps_cap: u8,
    /// Strips per screen on Old 3DS (8 default, 4 or 2). Extension knob.
    pub chunks: u8,
    /// Pause between strips in ms (Old 3DS pacing floor). Extension knob.
    pub strip_sleep: u8,
    /// Quarter-res mode (Old 3DS): ~4x encode speedup, 2x2 upscale. Extension knob.
    pub downscale: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let mut s = Settings {
            master: 0.5,
            custom: false,
            quality: 70,
            screen: 1,
            interlace: false,
            strip_skip: true,
            refresh_interval: 64,
            fps_cap: 0,
            // Measured on Old 3DS XL: 4 chunks + no inter-strip sleep gave
            // +65% sent fps over the legacy 8/5ms (see flurry PERF notes).
            chunks: 4,
            strip_sleep: 0,
            downscale: false,
        };
        s.apply_master();
        s
    }
}

impl Settings {
    /// Preset curve: derive the individual knobs from the master slider.
    fn apply_master(&mut self) {
        let m = self.master.clamp(0.0, 1.0);
        // Quality 40 (fps end) … 95 (quality end).
        self.quality = (40.0 + m * 55.0).round() as u8;
        // Interlace on the fps-priority half.
        self.interlace = m < 0.5;
        // Uncapped fps toward the fps end; give the encoder breathing room
        // (and thus better quality per frame) toward the quality end.
        self.fps_cap = if m < 0.75 { 0 } else { 24 };
        // Far fps end: quarter-res for ~4x encode speedup.
        self.downscale = m < 0.25;
        // Strip skip always pays; refresh faster when quality-focused.
        self.strip_skip = true;
        self.refresh_interval = if m < 0.5 { 64 } else { 32 };
    }

    fn screen_set(&self) -> ScreenSet {
        match self.screen {
            2 => ScreenSet::Bottom,
            3 => ScreenSet::Both,
            _ => ScreenSet::Top,
        }
    }
}

enum Conn {
    Idle,
    Active {
        worker: Worker,
        /// What the worker last applied; used to send only changed settings.
        sent: Settings,
        connected: bool,
        /// Feature announce from an extended sysmodule, if received.
        caps: Option<Announce>,
    },
}

/// Sliding one-second window for FPS / bandwidth meters.
#[derive(Default)]
struct Meter {
    samples: VecDeque<(Instant, usize)>,
    /// Strip arrivals per screen (0 top, 1 bottom): time + chunk index
    /// (0 for unchunked full frames).
    strips: [VecDeque<(Instant, u8)>; 2],
}

impl Meter {
    fn push(&mut self, bytes: usize, bottom: bool, chunk: Option<u8>) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        self.strips[bottom as usize].push_back((now, chunk.unwrap_or(0)));
    }

    fn reset(&mut self) {
        *self = Meter::default();
    }

    fn trim(&mut self) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(1);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
        for s in &mut self.strips {
            while s.front().is_some_and(|(t, _)| *t < cutoff) {
                s.pop_front();
            }
        }
    }

    /// (updates per second, megabits per second) over the last second.
    fn rates(&mut self) -> (usize, f32) {
        self.trim();
        let bytes: usize = self.samples.iter().map(|(_, b)| b).sum();
        (self.samples.len(), bytes as f32 * 8.0 / 1_000_000.0)
    }

    /// Effective full-frame fps for a screen: strips/s divided by the
    /// strips-per-frame inferred from the same window (max chunk index + 1;
    /// 1 for unchunked streams). Tracks live chunk-count changes.
    fn fps(&mut self, bottom: bool) -> f32 {
        self.trim();
        let s = &self.strips[bottom as usize];
        let per_frame = s.iter().map(|(_, c)| *c).max().unwrap_or(0) as f32 + 1.0;
        s.len() as f32 / per_frame
    }
}

struct App {
    store: profiles::Store,
    /// Working copy of the selected device.
    device: Device,
    settings: Settings,
    conn: Conn,
    status: String,
    stats: String,
    log: VecDeque<String>,
    top_tex: Option<egui::TextureHandle>,
    bottom_tex: Option<egui::TextureHandle>,
    meter: Meter,
    /// Device editor modal draft (None = closed).
    device_editor: Option<Device>,
    /// Inline "save profile as" name field.
    new_profile_name: String,
    show_bench: bool,
    bench_goal: f32,
    bench_sweep_chunks: bool,
    bench_fine_quality: bool,
    bench: Option<bench::Bench>,
    bench_summary: Option<String>,
    bench_table: Vec<bench::BenchResult>,
    /// Show 3DS stats/log; keeps the sysmodule stats packets enabled.
    debug_stats: bool,
    /// Latest parsed 3DS stats (for the benchmark).
    stats_snap: bench::StatsSnap,
    /// CPU copy of the latest decoded top-screen frame (kept only while a
    /// benchmark runs; feeds the quality metrics and screenshots).
    last_top: Option<egui::ColorImage>,
    /// Screenshot folder of the current/last benchmark run.
    bench_dir: Option<std::path::PathBuf>,
}

/// Parse the sysmodule's key=value stats text.
fn parse_stats(text: &str) -> bench::StatsSnap {
    let mut s = bench::StatsSnap::default();
    for tok in text.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else { continue };
        let Ok(f) = v.parse::<f32>() else { continue };
        match k {
            "enc" => s.enc = f,
            "send" => s.send = f,
            "sent" => s.sent = f,
            "skip" => s.skip = f,
            _ => {}
        }
    }
    s
}

impl App {
    fn new() -> App {
        let store = profiles::Store::load();
        let device = store
            .last_device
            .as_deref()
            .and_then(|n| store.get_device(n))
            .cloned()
            .unwrap_or_default();
        let settings = device
            .profile
            .as_deref()
            .and_then(|n| store.get_profile(n))
            .map(|p| p.settings)
            .unwrap_or_default();
        App {
            store,
            device,
            settings,
            conn: Conn::Idle,
            status: "Not connected".into(),
            stats: String::new(),
            log: VecDeque::new(),
            top_tex: None,
            bottom_tex: None,
            meter: Meter::default(),
            device_editor: None,
            new_profile_name: String::new(),
            show_bench: false,
            bench_goal: 0.5,
            bench_sweep_chunks: false,
            bench_fine_quality: false,
            bench: None,
            bench_summary: None,
            bench_table: Vec::new(),
            debug_stats: false,
            stats_snap: bench::StatsSnap::default(),
            last_top: None,
            bench_dir: None,
        }
    }

    fn send_stats_enabled(&self, on: bool) {
        if let Conn::Active { worker, caps, .. } = &self.conn {
            if caps.is_some_and(|a| a.has(feature::STATS_TOGGLE)) {
                let _ = worker.cmds.send(Cmd::SetStatsEnabled(on));
            }
        }
    }

    fn select_device(&mut self, name: &str) {
        if let Some(d) = self.store.get_device(name).cloned() {
            if let Some(p) = d.profile.as_deref().and_then(|n| self.store.get_profile(n)) {
                self.settings = p.settings;
            }
            self.device = d;
            self.store.last_device = Some(name.to_string());
            self.store.save();
        }
    }

    fn select_profile(&mut self, name: &str) {
        if let Some(p) = self.store.get_profile(name) {
            self.settings = p.settings;
            self.device.profile = Some(name.to_string());
            self.store.upsert_device(self.device.clone());
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let Conn::Active { worker, connected, caps, .. } = &mut self.conn else {
            return;
        };
        let mut disconnect_reason = None;
        for ev in worker.events.try_iter() {
            match ev {
                Event::Connected => {
                    *connected = true;
                    self.status = "Connected".into();
                    // Chunk-count inference must not carry across sessions.
                    self.meter.reset();
                }
                Event::Capabilities(a) => {
                    *caps = Some(a);
                    self.status = format!(
                        "Connected (extended rev {}, features {:#08b})",
                        a.revision, a.features
                    );
                    // Stats are opt-in on toggle-capable sysmodules; enable
                    // them if the user wants debug info or a bench is live.
                    if a.has(feature::STATS_TOGGLE)
                        && (self.debug_stats || self.bench.is_some())
                    {
                        let _ = worker.cmds.send(Cmd::SetStatsEnabled(true));
                    }
                }
                Event::Screen { bottom, image, bytes, chunk } => {
                    self.meter.push(bytes, bottom, chunk);
                    if !bottom && self.bench.is_some() {
                        self.last_top = Some(image.clone());
                    }
                    let (slot, name) = if bottom {
                        (&mut self.bottom_tex, "bottom")
                    } else {
                        (&mut self.top_tex, "top")
                    };
                    match slot {
                        Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                        None => *slot = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR)),
                    }
                }
                Event::Stats(s) => {
                    self.stats_snap = parse_stats(&s);
                    self.stats = s;
                }
                Event::Info(msg) => {
                    self.log.push_back(msg.clone());
                    while self.log.len() > 8 {
                        self.log.pop_front();
                    }
                    self.status = msg;
                }
                Event::Disconnected(reason) => disconnect_reason = Some(reason),
            }
        }
        if let Some(reason) = disconnect_reason {
            self.conn = Conn::Idle;
            self.status = reason;
            self.bench = None; // benchmark cannot continue without a stream
        }
    }

    /// Send only the settings that changed since last send. Extension knobs
    /// are only sent once the sysmodule has announced support.
    fn push_settings(&mut self) {
        let Conn::Active { worker, sent, connected, caps } = &mut self.conn else {
            return;
        };
        if !*connected || *sent == self.settings {
            return;
        }
        let s = &self.settings;
        if sent.quality != s.quality {
            let _ = worker.cmds.send(Cmd::SetQuality(s.quality));
        }
        if sent.screen != s.screen {
            let _ = worker.cmds.send(Cmd::SetScreen(s.screen_set()));
        }
        if sent.interlace != s.interlace {
            let _ = worker.cmds.send(Cmd::SetInterlace(s.interlace));
        }
        if let Some(a) = caps {
            if a.has(feature::STRIP_SKIP) {
                if sent.strip_skip != s.strip_skip {
                    let _ = worker.cmds.send(Cmd::SetStripSkip(s.strip_skip));
                }
                if sent.refresh_interval != s.refresh_interval {
                    let _ = worker.cmds.send(Cmd::SetRefreshInterval(s.refresh_interval));
                }
            }
            if a.has(feature::FPS_CAP) && sent.fps_cap != s.fps_cap {
                let _ = worker.cmds.send(Cmd::SetFpsCap(s.fps_cap));
            }
            if a.has(feature::CHUNKS) && sent.chunks != s.chunks {
                let _ = worker.cmds.send(Cmd::SetChunks(s.chunks));
            }
            if a.has(feature::STRIP_SLEEP) && sent.strip_sleep != s.strip_sleep {
                let _ = worker.cmds.send(Cmd::SetStripSleep(s.strip_sleep));
            }
            if a.has(feature::DOWNSCALE) && sent.downscale != s.downscale {
                let _ = worker.cmds.send(Cmd::SetDownscale(s.downscale));
            }
        }
        *sent = self.settings;
    }

    fn caps(&self) -> Option<Announce> {
        match &self.conn {
            Conn::Active { caps, .. } => *caps,
            Conn::Idle => None,
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        if self.device.address.trim().is_empty() {
            self.status = "Set the device IP first (Device → Edit)".into();
            return;
        }
        let worker = worker::spawn(
            format!("{}:{}", self.device.address.trim(), self.device.port),
            ctx.clone(),
            self.settings.quality,
            self.settings.screen_set(),
            self.settings.interlace,
        );
        // `sent` must reflect the DEVICE's state, not ours: the worker's
        // hello covers quality/screen/interlace, but every extension knob
        // starts at the sysmodule default. Diffing against our own defaults
        // silently skipped pushing them (bench measured chunks=8/sleep=5ms/
        // skip-off while labeling otherwise).
        let device_state = Settings {
            strip_skip: false,
            refresh_interval: 64,
            fps_cap: 0,
            chunks: 8,
            strip_sleep: 5,
            downscale: false,
            ..self.settings
        };
        self.conn = Conn::Active {
            worker,
            sent: device_state,
            connected: false,
            caps: None,
        };
        self.status = "Connecting…".into();
    }

    // ------------------------------------------------------------------ UI

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Device menu
            let dev_label = format!("🎮 {}", self.device.name);
            ui.menu_button(dev_label, |ui| {
                let names: Vec<String> =
                    self.store.devices.iter().map(|d| d.name.clone()).collect();
                for name in names {
                    if ui
                        .selectable_label(self.device.name == name, &name)
                        .clicked()
                    {
                        self.select_device(&name);
                        ui.close();
                    }
                }
                if !self.store.devices.is_empty() {
                    ui.separator();
                }
                if ui.button("New device…").clicked() {
                    self.device_editor = Some(Device::default());
                    ui.close();
                }
                if ui.button("Edit current…").clicked() {
                    self.device_editor = Some(self.device.clone());
                    ui.close();
                }
                if ui.button("Delete current").clicked() {
                    let name = self.device.name.clone();
                    self.store.delete_device(&name);
                    self.device = self.store.devices.first().cloned().unwrap_or_default();
                    ui.close();
                }
            });

            match &self.conn {
                Conn::Idle => {
                    if ui.button("Connect").clicked() {
                        self.connect(&ui.ctx().clone());
                    }
                }
                Conn::Active { worker, .. } => {
                    if ui.button("Disconnect").clicked() {
                        let _ = worker.cmds.send(Cmd::Disconnect);
                    }
                }
            }

            ui.separator();

            // Profile menu
            let prof_label = format!(
                "📼 {}",
                self.device.profile.as_deref().unwrap_or("(no profile)")
            );
            ui.menu_button(prof_label, |ui| {
                let names: Vec<String> =
                    self.store.profiles.iter().map(|p| p.name.clone()).collect();
                for name in names {
                    if ui
                        .selectable_label(self.device.profile.as_deref() == Some(&name), &name)
                        .clicked()
                    {
                        self.select_profile(&name);
                        ui.close();
                    }
                }
                if !self.store.profiles.is_empty() {
                    ui.separator();
                }
                if let Some(current) = self.device.profile.clone() {
                    if ui.button(format!("Save to '{current}'")).clicked() {
                        self.store.upsert_profile(Profile {
                            name: current.clone(),
                            settings: self.settings,
                        });
                        ui.close();
                    }
                    if ui.button(format!("Delete '{current}'")).clicked() {
                        self.store.delete_profile(&current);
                        self.device.profile = None;
                        ui.close();
                    }
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_profile_name)
                            .hint_text("new profile name")
                            .desired_width(140.0),
                    );
                    let name = self.new_profile_name.trim().to_string();
                    if ui.add_enabled(!name.is_empty(), egui::Button::new("Save as")).clicked() {
                        self.store.upsert_profile(Profile {
                            name: name.clone(),
                            settings: self.settings,
                        });
                        self.device.profile = Some(name);
                        self.store.upsert_device(self.device.clone());
                        self.new_profile_name.clear();
                        ui.close();
                    }
                });
            });

            ui.separator();

            let bench_on = self.bench.is_some();
            if ui
                .add_enabled(
                    matches!(self.conn, Conn::Active { connected: true, .. }) || bench_on,
                    egui::Button::new(if bench_on { "⚡ Benchmarking…" } else { "⚡ Benchmark" }),
                )
                .clicked()
            {
                self.show_bench = true;
            }

        });
    }

    fn device_editor_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.device_editor.take() else { return };
        let mut open = true;
        let mut done = false;
        egui::Window::new("Device")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut draft.name);
                });
                ui.horizontal(|ui| {
                    ui.label("IP:");
                    ui.text_edit_singleline(&mut draft.address);
                });
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    ui.add(egui::DragValue::new(&mut draft.port).range(1..=65535));
                });
                egui::ComboBox::from_label("Type")
                    .selected_text(draft.device_type.label())
                    .show_ui(ui, |ui| {
                        for t in DEVICE_TYPES {
                            ui.selectable_value(&mut draft.device_type, t, t.label());
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !draft.name.trim().is_empty(),
                            egui::Button::new("Save"),
                        )
                        .clicked()
                    {
                        draft.name = draft.name.trim().to_string();
                        self.store.upsert_device(draft.clone());
                        self.device = draft.clone();
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            });
        if open && !done {
            self.device_editor = Some(draft);
        }
    }

    fn bench_window(&mut self, ctx: &egui::Context) {
        if !self.show_bench {
            return;
        }
        let mut open = true;
        egui::Window::new("Benchmark")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Sweeps decimation modes and JPEG quality on the live\nconnection and picks the best fit for your goal.");
                ui.small("Tip: open the Flurry app on the 3DS — its moving test\npattern gives worst-case (honest) fps numbers.");
                ui.separator();
                ui.add(
                    egui::Slider::new(&mut self.bench_goal, 0.0..=1.0)
                        .show_value(false)
                        .text("FPS ↔ Quality"),
                );
                match &self.bench {
                    Some(b) => {
                        ui.label(format!(
                            "Running {}/{}: {}",
                            b.step() + 1,
                            b.total(),
                            b.describe_current()
                        ));
                        let (fps_top, fps_bot) = (self.meter.fps(false), self.meter.fps(true));
                        ui.label(format!("live: {:.1} fps", fps_top + fps_bot));
                        if ui.button("Cancel").clicked() {
                            let restore = self.bench.as_ref().unwrap().cancel();
                            self.settings = restore;
                            self.bench = None;
                            self.send_stats_enabled(self.debug_stats);
                        }
                    }
                    None => {
                        ui.checkbox(&mut self.bench_sweep_chunks, "Sweep chunk counts (8 vs 4)");
                        ui.checkbox(&mut self.bench_fine_quality, "Fine quality sweep (3 points)");
                        {
                            let modes = if self
                                .caps()
                                .is_some_and(|a| a.has(feature::DOWNSCALE))
                            {
                                3
                            } else {
                                2
                            };
                            let n = modes
                                * if self.bench_fine_quality { 3 } else { 2 }
                                * if self.bench_sweep_chunks { 2 } else { 1 };
                            ui.small(format!(
                                "{} configs ≈ {:.0} s (streams BOTH screens; combined fps goal)",
                                n,
                                bench::Bench::estimate(n).as_secs_f32()
                            ));
                        }
                        if let Some(s) = &self.bench_summary {
                            ui.separator();
                            ui.label(s.clone());
                        }
                        if !self.bench_table.is_empty() {
                            egui::Grid::new("bench_results")
                                .striped(true)
                                .min_col_width(56.0)
                                .show(ui, |ui| {
                                    ui.strong("Config");
                                    ui.strong("top fps");
                                    ui.strong("bot fps");
                                    ui.strong("sharp");
                                    ui.strong("block");
                                    ui.strong("enc ms/s");
                                    ui.strong("send ms/s");
                                    ui.strong("skip/s");
                                    ui.strong("score");
                                    ui.end_row();
                                    for r in &self.bench_table {
                                        let label = if r.winner {
                                            format!("★ {}", r.label)
                                        } else {
                                            r.label.clone()
                                        };
                                        ui.label(label);
                                        ui.label(format!("{:.1}", r.fps));
                                        ui.label(format!("{:.1}", r.bot));
                                        ui.label(format!("{:.2}", r.sharp));
                                        ui.label(format!("{:.2}", r.block));
                                        ui.label(format!("{:.0}", r.stats.enc));
                                        ui.label(format!("{:.0}", r.stats.send));
                                        ui.label(format!("{:.0}", r.stats.skip));
                                        ui.label(format!("{:.2}", r.score));
                                        ui.end_row();
                                    }
                                });
                            if let Some(dir) = &self.bench_dir {
                                ui.small(format!("Screenshots: {}", dir.display()));
                            }
                        }
                        let connected =
                            matches!(self.conn, Conn::Active { connected: true, .. });
                        if ui
                            .add_enabled(connected, egui::Button::new("Run benchmark"))
                            .clicked()
                        {
                            self.bench_summary = None;
                            self.send_stats_enabled(true);
                            self.bench_dir = dirs::config_dir().map(|d| {
                                d.join("flurry-client").join("bench").join(format!(
                                    "{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs())
                                        .unwrap_or(0)
                                ))
                            });
                            self.bench = Some(bench::Bench::start(
                                bench::Options {
                                    goal: self.bench_goal,
                                    sweep_chunks: self.bench_sweep_chunks,
                                    fine_quality: self.bench_fine_quality,
                                },
                                self.settings,
                                self.caps(),
                            ));
                        }
                        if !connected {
                            ui.small("Connect first.");
                        }
                    }
                }
            });
        self.show_bench = open;
    }

    fn bench_tick(&mut self, ctx: &egui::Context) {
        let fps_top = self.meter.fps(false);
        let fps_bot = self.meter.fps(true);
        let snap = self.stats_snap;
        let qual = self
            .last_top
            .as_ref()
            .map(quality::measure)
            .unwrap_or((0.0, 1.0));
        let Some(b) = &mut self.bench else { return };
        // Keep the stream on the config under test.
        self.settings = b.current_config();
        let prev_step = b.step();
        let finished = b.tick(fps_top, fps_bot, qual, snap);
        // Config finished (advanced or run done): screenshot it.
        if b.step() != prev_step || finished.is_some() {
            if let (Some(img), Some(dir)) = (&self.last_top, &self.bench_dir) {
                let path = dir.join(format!("{}.png", b.label(prev_step)));
                if let Err(e) = quality::save_png(img, &path) {
                    self.log.push_back(format!("screenshot failed: {e}"));
                }
            }
        }
        if let Some(winner) = finished {
            self.settings = winner;
            self.bench_summary = b.summary.clone();
            self.bench_table = b.table.clone();
            self.bench = None;
            self.last_top = None;
            self.status = self.bench_summary.clone().unwrap_or_default();
            self.send_stats_enabled(self.debug_stats);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn controls_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Stream");
        let master = ui.add(
            egui::Slider::new(&mut self.settings.master, 0.0..=1.0)
                .show_value(false)
                .text("FPS ↔ Quality"),
        );
        if master.changed() {
            self.settings.custom = false;
            self.settings.apply_master();
        }
        let mut screen = self.settings.screen;
        egui::ComboBox::from_label("Screen")
            .selected_text(match screen {
                2 => "Bottom",
                3 => "Both",
                _ => "Top",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut screen, 1, "Top");
                ui.selectable_value(&mut screen, 2, "Bottom");
                ui.selectable_value(&mut screen, 3, "Both");
            });
        self.settings.screen = screen;

        let caps = self.caps();
        egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
            let s = &mut self.settings;
            let before = *s;
            ui.add(egui::Slider::new(&mut s.quality, 1..=100).text("JPEG quality"));
            ui.checkbox(&mut s.interlace, "Interlace");

            let ext = |on: bool| {
                if on {
                    ""
                } else {
                    " (needs extended sysmodule)"
                }
            };
            let idle = matches!(self.conn, Conn::Idle);
            let skip_ok = caps.is_some_and(|a| a.has(feature::STRIP_SKIP));
            ui.add_enabled_ui(skip_ok || idle, |ui| {
                ui.checkbox(&mut s.strip_skip, format!("Skip unchanged strips{}", ext(skip_ok)));
                ui.add(
                    egui::Slider::new(&mut s.refresh_interval, 0..=255)
                        .text(format!("Refresh interval{}", ext(skip_ok))),
                );
            });
            let cap_ok = caps.is_some_and(|a| a.has(feature::FPS_CAP));
            ui.add_enabled_ui(cap_ok || idle, |ui| {
                ui.add(
                    egui::Slider::new(&mut s.fps_cap, 0..=60)
                        .text(format!("FPS cap (0 = off){}", ext(cap_ok))),
                );
            });
            let chunks_ok = caps.is_some_and(|a| a.has(feature::CHUNKS));
            ui.add_enabled_ui(chunks_ok || idle, |ui| {
                egui::ComboBox::from_label(format!("Chunks (Old 3DS){}", ext(chunks_ok)))
                    .selected_text(format!("{}", s.chunks))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.chunks, 8u8, "8");
                        ui.selectable_value(&mut s.chunks, 4u8, "4");
                        ui.selectable_value(&mut s.chunks, 2u8, "2");
                    });
            });
            let sleep_ok = caps.is_some_and(|a| a.has(feature::STRIP_SLEEP));
            ui.add_enabled_ui(sleep_ok || idle, |ui| {
                ui.add(
                    egui::Slider::new(&mut s.strip_sleep, 0..=20)
                        .text(format!("Strip sleep ms{}", ext(sleep_ok))),
                );
            });
            let ds_ok = caps.is_some_and(|a| a.has(feature::DOWNSCALE));
            ui.add_enabled_ui(ds_ok || idle, |ui| {
                ui.checkbox(
                    &mut s.downscale,
                    format!("Quarter-res (~4x faster){}", ext(ds_ok)),
                );
            });
            if *s != before {
                s.custom = true;
            }
        });

        let (ups, mbps) = self.meter.rates();
        let (fps_top, fps_bot) = (self.meter.fps(false), self.meter.fps(true));
        ui.separator();
        ui.strong(format!("Top {fps_top:.1} fps   Bottom {fps_bot:.1} fps"));
        ui.label(format!("{ups} strips/s   {mbps:.2} Mbit/s"));
        if ui
            .checkbox(&mut self.debug_stats, "Debug (3DS perf stats)")
            .changed()
        {
            self.send_stats_enabled(self.debug_stats);
            if !self.debug_stats {
                self.stats.clear();
            }
        }
        // Older sysmodules stream stats unconditionally; only show them
        // when wanted (the packets are still parsed for benchmarks).
        if self.debug_stats && !self.stats.is_empty() {
            egui::CollapsingHeader::new("3DS stats")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(&self.stats);
                });
        }
        if !self.log.is_empty() {
            egui::CollapsingHeader::new("3DS log").show(ui, |ui| {
                for line in &self.log {
                    ui.small(line);
                }
            });
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.bench_tick(&ctx);

        egui::Panel::top(egui::Id::new("toolbar")).show(ui, |ui| {
            self.toolbar(ui);
        });

        egui::Panel::bottom(egui::Id::new("statusbar")).show(ui, |ui| {
            ui.small(&self.status);
        });

        egui::Panel::left(egui::Id::new("controls"))
            .resizable(false)
            .default_size(260.0)
            .show(ui, |ui| {
                let busy = self.bench.is_some();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| self.controls_panel(ui));
                });
            });

        self.push_settings();
        self.device_editor_window(&ctx);
        self.bench_window(&ctx);

        egui::CentralPanel::default().show(ui, |ui| {
            let show_top = self.settings.screen != 2;
            let show_bottom = self.settings.screen != 1;
            let mut drew = false;

            ui.vertical_centered(|ui| {
                let avail = ui.available_size();
                // Stacked layout: top 400x240 over bottom 320x240.
                let total_h = 240.0 * (show_top as u8 + show_bottom as u8) as f32;
                let scale = (avail.x / 400.0).min(avail.y / total_h.max(240.0)).max(0.1);
                if show_top {
                    if let Some(tex) = &self.top_tex {
                        ui.image((tex.id(), egui::vec2(400.0 * scale, 240.0 * scale)));
                        drew = true;
                    }
                }
                if show_bottom {
                    if let Some(tex) = &self.bottom_tex {
                        ui.image((tex.id(), egui::vec2(320.0 * scale, 240.0 * scale)));
                        drew = true;
                    }
                }
                if !drew {
                    ui.centered_and_justified(|ui| {
                        ui.label(match self.conn {
                            Conn::Idle => "Not connected",
                            Conn::Active { connected: false, .. } => "Connecting…",
                            Conn::Active { .. } => "Connected — waiting for frames…",
                        });
                    });
                }
            });
        });
    }
}
