//! Benchmark: sweep the tunable knobs on the live connection, measure fps
//! per configuration via the client-side meter, and pick the best config
//! for a user-chosen FPS↔Quality goal.
//!
//! Driven by `App` each frame: apply config → settle → measure → next.
//! Needs moving screen content to measure anything (the Flurry app's test
//! pattern is ideal); strip-skip makes static content read ~0 fps.

use std::time::{Duration, Instant};

use flurry_proto::legacy::{feature, Announce};

use crate::Settings;

const SETTLE: Duration = Duration::from_millis(1200);
const MEASURE: Duration = Duration::from_millis(2500);
/// fps at which the fps half of the score saturates.
const FPS_TARGET: f32 = 24.0;

#[derive(Clone, Copy)]
enum Phase {
    Settle(Instant),
    Measure(Instant),
}

pub struct Bench {
    /// 0.0 = pure fps, 1.0 = pure quality.
    pub goal: f32,
    plan: Vec<(Settings, f32)>, // config + quality weight
    results: Vec<f32>,          // measured fps per plan entry
    idx: usize,
    phase: Phase,
    restore: Settings,
    pub summary: Option<String>,
    /// Full result table, filled when the run completes.
    pub table: Vec<BenchResult>,
}

#[derive(Clone)]
pub struct BenchResult {
    pub label: String,
    pub fps: f32,
    pub score: f32,
    pub winner: bool,
}

/// Perceptual weight of a decimation mode.
fn mode_weight(s: &Settings) -> f32 {
    if s.downscale {
        0.45
    } else if s.interlace {
        0.7
    } else {
        1.0
    }
}

fn mode_name(s: &Settings) -> &'static str {
    if s.downscale {
        "quarter-res"
    } else if s.interlace {
        "interlace"
    } else {
        "progressive"
    }
}

impl Bench {
    /// Build the sweep from the current settings and announced features.
    pub fn start(goal: f32, current: Settings, caps: Option<Announce>) -> Bench {
        let has = |bit| caps.is_some_and(|a| a.has(bit));

        let mut base = current;
        base.custom = true;
        if has(feature::STRIP_SKIP) {
            base.strip_skip = true;
            base.refresh_interval = 32;
        }
        if has(feature::STRIP_SLEEP) {
            base.strip_sleep = 0;
        }
        if has(feature::CHUNKS) {
            base.chunks = 4;
        }
        base.fps_cap = 0;

        let mut plan = Vec::new();
        for &(interlace, downscale) in &[(false, false), (true, false), (false, true)] {
            if downscale && !has(feature::DOWNSCALE) {
                continue;
            }
            for &q in &[45u8, 70, 90] {
                let mut cfg = base;
                cfg.interlace = interlace;
                cfg.downscale = downscale;
                cfg.quality = q;
                let weight = mode_weight(&cfg) * (0.5 + 0.5 * q as f32 / 100.0);
                plan.push((cfg, weight));
            }
        }

        Bench {
            goal,
            plan,
            results: Vec::new(),
            idx: 0,
            phase: Phase::Settle(Instant::now() + SETTLE),
            restore: current,
            summary: None,
            table: Vec::new(),
        }
    }

    pub fn total(&self) -> usize {
        self.plan.len()
    }

    pub fn step(&self) -> usize {
        self.idx
    }

    /// Config the app should have applied right now.
    pub fn current_config(&self) -> Settings {
        self.plan[self.idx.min(self.plan.len() - 1)].0
    }

    pub fn describe_current(&self) -> String {
        let (cfg, _) = &self.plan[self.idx.min(self.plan.len() - 1)];
        format!("{} q={}", mode_name(cfg), cfg.quality)
    }

    /// Advance the state machine. `fps` = current combined fps reading.
    /// Returns the winning settings once finished.
    pub fn tick(&mut self, fps: f32) -> Option<Settings> {
        match self.phase {
            Phase::Settle(until) => {
                if Instant::now() >= until {
                    self.phase = Phase::Measure(Instant::now() + MEASURE);
                }
                None
            }
            Phase::Measure(until) => {
                if Instant::now() < until {
                    return None;
                }
                self.results.push(fps);
                self.idx += 1;
                if self.idx < self.plan.len() {
                    self.phase = Phase::Settle(Instant::now() + SETTLE);
                    return None;
                }

                // Done: score everything.
                let g = self.goal;
                let mut best = 0usize;
                let mut best_score = f32::MIN;
                for (i, ((_, weight), fps)) in self.plan.iter().zip(&self.results).enumerate() {
                    let score = (1.0 - g) * (fps / FPS_TARGET).min(1.0) + g * weight;
                    if score > best_score {
                        best_score = score;
                        best = i;
                    }
                }
                self.table = self
                    .plan
                    .iter()
                    .zip(&self.results)
                    .enumerate()
                    .map(|(i, ((cfg, weight), fps))| BenchResult {
                        label: format!("{} q={}", mode_name(cfg), cfg.quality),
                        fps: *fps,
                        score: (1.0 - g) * (fps / FPS_TARGET).min(1.0) + g * weight,
                        winner: i == best,
                    })
                    .collect();
                self.table
                    .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                let (win, _) = self.plan[best];
                self.summary = Some(format!(
                    "Winner: {} q={} — {:.1} fps (score {:.2})",
                    mode_name(&win),
                    win.quality,
                    self.results[best],
                    best_score,
                ));
                Some(win)
            }
        }
    }

    /// Settings to restore if the user cancels mid-run.
    pub fn cancel(&self) -> Settings {
        self.restore
    }
}
