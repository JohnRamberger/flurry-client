//! Benchmark: sweep the tunable knobs on the live connection, measure fps
//! per configuration via the client-side meter, and pick the best config
//! for a user-chosen FPS↔Quality goal.
//!
//! Driven by `App` each frame: apply config → settle → measure → next.
//! Always runs with BOTH screens streaming (the mostly-static bottom screen
//! exercises strip skip; the fps goal is the combined rate). fps per config
//! is a trimmed mean of ~200 ms samples across the measure window, so a
//! single hiccup or lucky burst doesn't decide the winner. The 3DS-side
//! stats (enc/send ms/s, skip rate) captured at the end of each window go
//! into the results table.
//!
//! Needs moving screen content to measure anything (the Flurry app's test
//! pattern is ideal); strip-skip makes static content read ~0 fps.

use std::time::{Duration, Instant};

use flurry_proto::legacy::{feature, Announce};

use crate::Settings;

const SETTLE: Duration = Duration::from_millis(1200);
const MEASURE: Duration = Duration::from_millis(2500);
const SAMPLE_EVERY: Duration = Duration::from_millis(200);
/// fps at which the fps half of the score saturates.
const FPS_TARGET: f32 = 24.0;

/// Parsed 3DS stats snapshot (from the 1 Hz stats packet).
#[derive(Clone, Copy, Default)]
pub struct StatsSnap {
    pub enc: f32,
    pub send: f32,
    pub sent: f32,
    pub skip: f32,
}

/// What to sweep.
#[derive(Clone, Copy)]
pub struct Options {
    /// 0.0 = pure fps, 1.0 = pure quality.
    pub goal: f32,
    /// Also A/B chunk counts (8 vs 4) on capable sysmodules.
    pub sweep_chunks: bool,
    /// Three quality points instead of two.
    pub fine_quality: bool,
}

#[derive(Clone, Copy)]
enum Phase {
    Settle(Instant),
    Measure(Instant),
}

pub struct Bench {
    opts: Options,
    plan: Vec<(Settings, f32)>, // config + quality weight
    results: Vec<(f32, StatsSnap)>,
    idx: usize,
    phase: Phase,
    samples: Vec<f32>,
    last_sample: Instant,
    restore: Settings,
    pub summary: Option<String>,
    /// Full result table, filled when the run completes.
    pub table: Vec<BenchResult>,
}

#[derive(Clone)]
pub struct BenchResult {
    pub label: String,
    pub fps: f32,
    pub stats: StatsSnap,
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

fn trimmed_mean(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Drop the bottom and top ~20% (at least one sample each when possible).
    let cut = (v.len() / 5).max(usize::from(v.len() >= 3));
    let mid = &v[cut..v.len() - cut];
    mid.iter().sum::<f32>() / mid.len() as f32
}

impl Bench {
    /// Build the sweep from the current settings and announced features.
    pub fn start(opts: Options, current: Settings, caps: Option<Announce>) -> Bench {
        let has = |bit| caps.is_some_and(|a| a.has(bit));

        let mut base = current;
        base.custom = true;
        // Both screens: the static bottom screen exercises strip skip and
        // the goal fps is the combined rate.
        base.screen = 3;
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

        let qualities: &[u8] = if opts.fine_quality {
            &[45, 70, 90]
        } else {
            &[45, 90]
        };
        let chunk_opts: &[u8] = if opts.sweep_chunks && has(feature::CHUNKS) {
            &[4, 8]
        } else {
            &[0] // sentinel: keep base
        };

        let mut plan = Vec::new();
        for &chunks in chunk_opts {
            for &(interlace, downscale) in &[(false, false), (true, false), (false, true)] {
                if downscale && !has(feature::DOWNSCALE) {
                    continue;
                }
                for &q in qualities {
                    let mut cfg = base;
                    if chunks != 0 {
                        cfg.chunks = chunks;
                    }
                    cfg.interlace = interlace;
                    cfg.downscale = downscale;
                    cfg.quality = q;
                    let weight = mode_weight(&cfg) * (0.5 + 0.5 * q as f32 / 100.0);
                    plan.push((cfg, weight));
                }
            }
        }

        Bench {
            opts,
            plan,
            results: Vec::new(),
            idx: 0,
            phase: Phase::Settle(Instant::now() + SETTLE),
            samples: Vec::new(),
            last_sample: Instant::now(),
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

    /// Rough total duration for `n` plan entries (for UI estimates).
    pub fn estimate(n: usize) -> Duration {
        (SETTLE + MEASURE) * n as u32
    }

    /// Config the app should have applied right now.
    pub fn current_config(&self) -> Settings {
        self.plan[self.idx.min(self.plan.len() - 1)].0
    }

    pub fn describe_current(&self) -> String {
        let (cfg, _) = &self.plan[self.idx.min(self.plan.len() - 1)];
        format!("{} q={} chunks={}", mode_name(cfg), cfg.quality, cfg.chunks)
    }

    /// Advance the state machine. `fps` = current combined fps reading,
    /// `stats` = latest parsed 3DS stats. Returns the winning settings once
    /// finished.
    pub fn tick(&mut self, fps: f32, stats: StatsSnap) -> Option<Settings> {
        match self.phase {
            Phase::Settle(until) => {
                if Instant::now() >= until {
                    self.samples.clear();
                    self.last_sample = Instant::now();
                    self.phase = Phase::Measure(Instant::now() + MEASURE);
                }
                None
            }
            Phase::Measure(until) => {
                if self.last_sample.elapsed() >= SAMPLE_EVERY {
                    self.samples.push(fps);
                    self.last_sample = Instant::now();
                }
                if Instant::now() < until {
                    return None;
                }
                let fps_avg = trimmed_mean(std::mem::take(&mut self.samples));
                self.results.push((fps_avg, stats));
                self.idx += 1;
                if self.idx < self.plan.len() {
                    self.phase = Phase::Settle(Instant::now() + SETTLE);
                    return None;
                }

                // Done: score everything.
                let g = self.opts.goal;
                let mut best = 0usize;
                let mut best_score = f32::MIN;
                for (i, ((_, weight), (fps, _))) in
                    self.plan.iter().zip(&self.results).enumerate()
                {
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
                    .map(|(i, ((cfg, weight), (fps, stats)))| BenchResult {
                        label: format!(
                            "{} q={} c={}",
                            mode_name(cfg),
                            cfg.quality,
                            cfg.chunks
                        ),
                        fps: *fps,
                        stats: *stats,
                        score: (1.0 - g) * (fps / FPS_TARGET).min(1.0) + g * weight,
                        winner: i == best,
                    })
                    .collect();
                self.table.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let (mut win, _) = self.plan[best];
                // The forced both-screens view was for measurement only.
                win.screen = self.restore.screen;
                self.summary = Some(format!(
                    "Winner: {} q={} — {:.1} fps (score {:.2})",
                    mode_name(&win),
                    win.quality,
                    self.results[best].0,
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
