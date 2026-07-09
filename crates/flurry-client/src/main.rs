//! Flurry PC client — eframe/egui GUI.
//!
//! Speaks the legacy HzMod protocol (via `flurry_proto::legacy`) so it works
//! against current Flurry sysmodule builds; will switch to the redesigned
//! protocol when the 3DS side is rewritten. Extended sysmodules announce a
//! feature set on connect which unlocks the advanced tuning knobs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod profiles;
mod worker;

use std::collections::VecDeque;
use std::time::Instant;

use flurry_proto::legacy::{feature, Announce, ScreenSet};
use serde::{Deserialize, Serialize};
use worker::{Cmd, Event, Worker};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
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
}

impl Meter {
    fn push(&mut self, bytes: usize) {
        self.samples.push_back((Instant::now(), bytes));
    }

    /// (updates per second, megabits per second) over the last second.
    fn rates(&mut self) -> (usize, f32) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(1);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
        let bytes: usize = self.samples.iter().map(|(_, b)| b).sum();
        (self.samples.len(), bytes as f32 * 8.0 / 1_000_000.0)
    }
}

struct App {
    address: String,
    settings: Settings,
    conn: Conn,
    status: String,
    stats: String,
    top_tex: Option<egui::TextureHandle>,
    bottom_tex: Option<egui::TextureHandle>,
    store: profiles::Store,
    /// Profile name field (doubles as "save as" input).
    profile_name: String,
    meter: Meter,
}

impl App {
    fn new() -> App {
        let store = profiles::Store::load();
        let mut app = App {
            address: String::new(),
            settings: Settings::default(),
            conn: Conn::Idle,
            status: "Not connected".into(),
            stats: String::new(),
            top_tex: None,
            bottom_tex: None,
            profile_name: String::new(),
            store,
            meter: Meter::default(),
        };
        if let Some(name) = app.store.last.clone() {
            app.load_profile(&name);
        }
        app
    }

    fn load_profile(&mut self, name: &str) {
        if let Some(p) = self.store.get(name) {
            self.address = p.address.clone();
            self.settings = p.settings;
            self.profile_name = p.name.clone();
            self.store.last = Some(p.name.clone());
            self.store.save();
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
                }
                Event::Capabilities(a) => {
                    *caps = Some(a);
                    self.status = format!("Connected (extended sysmodule rev {})", a.revision);
                }
                Event::Screen { bottom, image, bytes } => {
                    self.meter.push(bytes);
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
                Event::Stats(s) => self.stats = s,
                Event::Info(msg) => self.status = msg,
                Event::Disconnected(reason) => disconnect_reason = Some(reason),
            }
        }
        if let Some(reason) = disconnect_reason {
            self.conn = Conn::Idle;
            self.status = reason;
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
        }
        *sent = self.settings;
    }

    fn controls_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Profile");
        let selected = self.store.last.clone().unwrap_or_default();
        let mut load: Option<String> = None;
        egui::ComboBox::from_id_salt("profile")
            .width(160.0)
            .selected_text(if selected.is_empty() { "—" } else { &selected })
            .show_ui(ui, |ui| {
                for p in &self.store.profiles {
                    if ui.selectable_label(p.name == selected, &p.name).clicked() {
                        load = Some(p.name.clone());
                    }
                }
            });
        if let Some(name) = load {
            self.load_profile(&name);
        }
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut self.profile_name);
        });
        ui.horizontal(|ui| {
            let name = self.profile_name.trim().to_string();
            if ui.add_enabled(!name.is_empty(), egui::Button::new("Save")).clicked() {
                self.store.upsert(profiles::Profile {
                    name,
                    address: self.address.trim().to_string(),
                    settings: self.settings,
                });
            } else if ui
                .add_enabled(self.store.get(&name).is_some(), egui::Button::new("Delete"))
                .clicked()
            {
                self.store.delete(&name);
            }
        });

        ui.separator();
        ui.heading("Connection");
        ui.horizontal(|ui| {
            ui.label("3DS IP:");
            ui.text_edit_singleline(&mut self.address);
        });
        match &self.conn {
            Conn::Idle => {
                if ui.button("Connect").clicked() && !self.address.trim().is_empty() {
                    let worker = worker::spawn(
                        self.address.trim().to_string(),
                        ui.ctx().clone(),
                        self.settings.quality,
                        self.settings.screen_set(),
                        self.settings.interlace,
                    );
                    self.conn = Conn::Active {
                        worker,
                        sent: self.settings,
                        connected: false,
                        caps: None,
                    };
                    self.status = "Connecting…".into();
                }
            }
            Conn::Active { worker, .. } => {
                if ui.button("Disconnect").clicked() {
                    let _ = worker.cmds.send(Cmd::Disconnect);
                }
            }
        }
        ui.label(&self.status);

        ui.separator();
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

        let caps = match &self.conn {
            Conn::Active { caps, .. } => *caps,
            Conn::Idle => None,
        };
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
            let skip_ok = caps.is_some_and(|a| a.has(feature::STRIP_SKIP));
            ui.add_enabled_ui(skip_ok || matches!(self.conn, Conn::Idle), |ui| {
                ui.checkbox(&mut s.strip_skip, format!("Skip unchanged strips{}", ext(skip_ok)));
                ui.add(
                    egui::Slider::new(&mut s.refresh_interval, 0..=255)
                        .text(format!("Refresh interval{}", ext(skip_ok))),
                );
            });
            let cap_ok = caps.is_some_and(|a| a.has(feature::FPS_CAP));
            ui.add_enabled_ui(cap_ok || matches!(self.conn, Conn::Idle), |ui| {
                ui.add(
                    egui::Slider::new(&mut s.fps_cap, 0..=60)
                        .text(format!("FPS cap (0 = off){}", ext(cap_ok))),
                );
            });
            if *s != before {
                s.custom = true;
            }
        });

        let (ups, mbps) = self.meter.rates();
        ui.separator();
        ui.label(format!("{ups} updates/s   {mbps:.2} Mbit/s"));
        if !self.stats.is_empty() {
            egui::CollapsingHeader::new("3DS stats").show(ui, |ui| {
                ui.label(&self.stats);
            });
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events(&ui.ctx().clone());

        egui::Panel::left(egui::Id::new("controls"))
            .resizable(false)
            .default_size(250.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls_panel(ui));
            });

        self.push_settings();

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
