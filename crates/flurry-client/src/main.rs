//! Flurry PC client — eframe/egui GUI.
//!
//! Speaks the legacy HzMod protocol (via `flurry_proto::legacy`) so it works
//! against current Flurry sysmodule builds; will switch to the redesigned
//! protocol when the 3DS side is rewritten.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod worker;

use flurry_proto::legacy::ScreenSet;
use worker::{Cmd, Event, Worker};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 620.0])
            .with_title("Flurry"),
        ..Default::default()
    };
    eframe::run_native(
        "Flurry",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

/// Stream settings as shown in the UI. Diffed against the last values sent
/// to the worker so edits apply live.
#[derive(Clone, Copy, PartialEq)]
struct Settings {
    quality: u8,
    screen: ScreenSet,
    interlace: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            quality: 70,
            screen: ScreenSet::Top,
            interlace: false,
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
    },
}

struct App {
    address: String,
    settings: Settings,
    conn: Conn,
    status: String,
    stats: String,
    top_tex: Option<egui::TextureHandle>,
    bottom_tex: Option<egui::TextureHandle>,
}

impl Default for App {
    fn default() -> Self {
        App {
            address: String::new(),
            settings: Settings::default(),
            conn: Conn::Idle,
            status: "Not connected".into(),
            stats: String::new(),
            top_tex: None,
            bottom_tex: None,
        }
    }
}

impl App {
    fn drain_events(&mut self, ctx: &egui::Context) {
        let Conn::Active { worker, connected, .. } = &mut self.conn else {
            return;
        };
        let mut disconnect_reason = None;
        for ev in worker.events.try_iter() {
            match ev {
                Event::Connected => {
                    *connected = true;
                    self.status = "Connected".into();
                }
                Event::Screen { bottom, image } => {
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

    /// Send only the settings that changed since last send.
    fn push_settings(&mut self) {
        let Conn::Active { worker, sent, connected } = &mut self.conn else {
            return;
        };
        if !*connected || *sent == self.settings {
            return;
        }
        if sent.quality != self.settings.quality {
            let _ = worker.cmds.send(Cmd::SetQuality(self.settings.quality));
        }
        if sent.screen != self.settings.screen {
            let _ = worker.cmds.send(Cmd::SetScreen(self.settings.screen));
        }
        if sent.interlace != self.settings.interlace {
            let _ = worker.cmds.send(Cmd::SetInterlace(self.settings.interlace));
        }
        *sent = self.settings;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events(&ui.ctx().clone());

        egui::Panel::left(egui::Id::new("controls"))
            .resizable(false)
            .default_size(230.0)
            .show(ui, |ui| {
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
                                self.settings.screen,
                                self.settings.interlace,
                            );
                            self.conn = Conn::Active {
                                worker,
                                sent: self.settings,
                                connected: false,
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
                ui.add(egui::Slider::new(&mut self.settings.quality, 1..=100).text("JPEG quality"));
                egui::ComboBox::from_label("Screen")
                    .selected_text(match self.settings.screen {
                        ScreenSet::Top => "Top",
                        ScreenSet::Bottom => "Bottom",
                        ScreenSet::Both => "Both",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.settings.screen, ScreenSet::Top, "Top");
                        ui.selectable_value(&mut self.settings.screen, ScreenSet::Bottom, "Bottom");
                        ui.selectable_value(&mut self.settings.screen, ScreenSet::Both, "Both");
                    });
                ui.checkbox(&mut self.settings.interlace, "Interlace (New 3DS)");

                if !self.stats.is_empty() {
                    ui.separator();
                    ui.heading("Stats");
                    ui.label(&self.stats);
                }
            });

        self.push_settings();

        egui::CentralPanel::default().show(ui, |ui| {
            let show_top = !matches!(self.settings.screen, ScreenSet::Bottom);
            let show_bottom = !matches!(self.settings.screen, ScreenSet::Top);
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
