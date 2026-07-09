//! Benchmark: sweep the tunable knobs on the live connection, measure fps
//! per configuration via the client-side meter, and pick the best config
//! for a user-chosen FPS↔Quality goal.
//!
//! Driven by `App` each frame: apply config → settle → measure → next.
//! Always runs with BOTH screens streaming, but the fps goal is the TOP
//! screen rate only: the moving top screen is the fluidity that matters,
//! and cycles wasted redrawing the static bottom screen show up as lost
//! top fps automatically (bottom fps is kept as a diagnostic — it should
//! be near the forced-refresh rate; higher means strip skip is failing).
//! fps per config
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
    pub dma: f32,
    pub torn: f32,
}

/// Which screen carries the moving content — decides which fps the score
/// uses (the other screen's rate stays visible as a diagnostic).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Top screen moves, bottom static (the Flurry test pattern).
    TopMoves,
    /// Both screens move (in-game benchmarking).
    BothMove,
    /// Bottom screen moves, top static.
    BottomMoves,
}

/// What to sweep.
#[derive(Clone, Copy)]
pub struct Options {
    /// 0.0 = pure fps, 1.0 = pure quality.
    pub goal: f32,
    /// Sweep depth 0..=3: Quick / Standard / Thorough / Exhaustive.
    pub depth: u8,
    pub motion: Motion,
}

fn depth_qualities(depth: u8) -> &'static [u8] {
    match depth {
        0 => &[70],
        1 => &[45, 90],
        _ => &[45, 70, 90],
    }
}

/// Number of configs a run at `depth` will test (for the UI time estimate).
pub fn plan_len(depth: u8, has_downscale: bool, has_chunks: bool) -> usize {
    let modes = if depth == 0 {
        2
    } else if has_downscale {
        3
    } else {
        2
    };
    let n = modes * depth_qualities(depth).len();
    if depth >= 3 && has_chunks {
        n * 2
    } else {
        n
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Settle(Instant),
    Measure(Instant),
}

pub struct Bench {
    opts: Options,
    plan: Vec<(Settings, f32)>, // config + fallback quality weight
    results: Vec<(f32, f32, StatsSnap, f32, f32)>, // top fps, bottom fps, stats, sharpness, blockiness
    idx: usize,
    phase: Phase,
    samples: Vec<f32>,
    bot_samples: Vec<f32>,
    qual_samples: Vec<(f32, f32)>,
    stats_samples: Vec<StatsSnap>,
    last_sample: Instant,
    restore: Settings,
    pub summary: Option<String>,
    /// Full result table, filled when the run completes.
    pub table: Vec<BenchResult>,
}

#[derive(Clone)]
pub struct BenchResult {
    pub label: String,
    /// The full settings of this config (screen restored to the user's
    /// selection) — applied live when the row is selected.
    pub settings: Settings,
    /// Top-screen (moving content) fps — the score's fps input.
    pub fps: f32,
    /// Bottom-screen fps — diagnostic; should be near the forced-refresh
    /// rate on static content, higher means strip skip isn't working.
    pub bot: f32,
    pub stats: StatsSnap,
    /// Measured no-reference sharpness (relative across the run).
    pub sharp: f32,
    /// Measured blockiness (1.0 = no 8-px grid artifacts).
    pub block: f32,
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

        let qualities: &[u8] = depth_qualities(opts.depth);
        let chunk_opts: &[u8] = if opts.depth >= 3 && has(feature::CHUNKS) {
            &[4, 8]
        } else {
            &[0] // sentinel: keep base
        };
        // Quick depth: progressive plus the strongest available decimation.
        let modes: &[(bool, bool)] = if opts.depth == 0 {
            if has(feature::DOWNSCALE) {
                &[(false, false), (false, true)]
            } else {
                &[(false, false), (true, false)]
            }
        } else {
            &[(false, false), (true, false), (false, true)]
        };

        let mut plan = Vec::new();
        for &chunks in chunk_opts {
            for &(interlace, downscale) in modes {
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
            bot_samples: Vec::new(),
            qual_samples: Vec::new(),
            stats_samples: Vec::new(),
            last_sample: Instant::now(),
            restore: current,
            summary: None,
            table: Vec::new(),
        }
    }

    /// Label for plan entry `i` (also used for screenshot filenames).
    pub fn label(&self, i: usize) -> String {
        let (cfg, _) = &self.plan[i.min(self.plan.len() - 1)];
        format!("{}_q{}_c{}", mode_name(cfg), cfg.quality, cfg.chunks)
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

    /// Advance the state machine. `fps_top`/`fps_bot` = current per-screen
    /// fps readings, `qual` = (sharpness, blockiness) of the latest decoded
    /// top frame, `stats` = latest parsed 3DS stats. Returns the winning
    /// settings once finished.
    pub fn tick(
        &mut self,
        fps_top: f32,
        fps_bot: f32,
        qual: (f32, f32),
        stats: StatsSnap,
    ) -> Option<Settings> {
        match self.phase {
            Phase::Settle(until) => {
                if Instant::now() >= until {
                    self.samples.clear();
                    self.bot_samples.clear();
                    self.qual_samples.clear();
                    self.stats_samples.clear();
                    self.last_sample = Instant::now();
                    self.phase = Phase::Measure(Instant::now() + MEASURE);
                }
                None
            }
            Phase::Measure(until) => {
                if self.last_sample.elapsed() >= SAMPLE_EVERY {
                    self.samples.push(fps_top);
                    self.bot_samples.push(fps_bot);
                    self.qual_samples.push(qual);
                    self.stats_samples.push(stats);
                    self.last_sample = Instant::now();
                }
                if Instant::now() < until {
                    return None;
                }
                let fps_avg = trimmed_mean(std::mem::take(&mut self.samples));
                let bot_avg = trimmed_mean(std::mem::take(&mut self.bot_samples));
                let qs = std::mem::take(&mut self.qual_samples);
                let n = qs.len().max(1) as f32;
                let sharp = qs.iter().map(|(s, _)| s).sum::<f32>() / n;
                let block = qs.iter().map(|(_, b)| b).sum::<f32>() / n;
                // Stats arrive at 1 Hz; sampling repeats values between
                // packets, which weights the mean toward what was current —
                // good enough for a window average.
                let ss = std::mem::take(&mut self.stats_samples);
                let sn = ss.len().max(1) as f32;
                let stats_avg = StatsSnap {
                    enc: ss.iter().map(|s| s.enc).sum::<f32>() / sn,
                    send: ss.iter().map(|s| s.send).sum::<f32>() / sn,
                    sent: ss.iter().map(|s| s.sent).sum::<f32>() / sn,
                    skip: ss.iter().map(|s| s.skip).sum::<f32>() / sn,
                    dma: ss.iter().map(|s| s.dma).sum::<f32>() / sn,
                    torn: ss.iter().map(|s| s.torn).sum::<f32>() / sn,
                };
                self.results.push((fps_avg, bot_avg, stats_avg, sharp, block));
                self.idx += 1;
                if self.idx < self.plan.len() {
                    self.phase = Phase::Settle(Instant::now() + SETTLE);
                    return None;
                }

                // Done: score everything. Quality is the MEASURED relative
                // sharpness (penalized by blockiness) when we got frames;
                // the static per-mode weight is only a fallback.
                let g = self.opts.goal;
                let max_sharp = self
                    .results
                    .iter()
                    .map(|(_, _, _, s, _)| *s)
                    .fold(0.0f32, f32::max);
                let quality_of = |i: usize| -> f32 {
                    let (_, _, _, sharp, block) = self.results[i];
                    if max_sharp > 0.01 {
                        // Measured sharpness counts JPEG ringing as detail
                        // (q45 ranked above q90), so blend in a JPEG-quality
                        // prior and penalize measured blockiness harder.
                        let q = self.plan[i].0.quality as f32 / 100.0;
                        (0.6 * sharp / max_sharp + 0.4 * q
                            - 0.3 * (block - 1.0).clamp(0.0, 2.0))
                        .max(0.0)
                    } else {
                        self.plan[i].1
                    }
                };
                // The scored fps depends on where the motion is.
                let scored = |i: usize| -> f32 {
                    let (fps, bot, _, _, _) = self.results[i];
                    match self.opts.motion {
                        Motion::TopMoves => fps,
                        Motion::BothMove => fps + bot,
                        Motion::BottomMoves => bot,
                    }
                };
                let mut best = 0usize;
                let mut best_score = f32::MIN;
                for i in 0..self.results.len() {
                    let score =
                        (1.0 - g) * (scored(i) / FPS_TARGET).min(1.0) + g * quality_of(i);
                    if score > best_score {
                        best_score = score;
                        best = i;
                    }
                }
                self.table = (0..self.plan.len())
                    .map(|i| {
                        let (cfg, _) = &self.plan[i];
                        let (fps, bot, stats, sharp, block) = self.results[i];
                        // The forced both-screens view was for measurement
                        // only; applied settings keep the user's screen.
                        let mut settings = *cfg;
                        settings.screen = self.restore.screen;
                        BenchResult {
                            label: format!(
                                "{} q={} c={}",
                                mode_name(cfg),
                                cfg.quality,
                                cfg.chunks
                            ),
                            settings,
                            fps,
                            bot,
                            stats,
                            sharp,
                            block,
                            score: (1.0 - g) * (scored(i) / FPS_TARGET).min(1.0)
                                + g * quality_of(i),
                            winner: i == best,
                        }
                    })
                    .collect();
                self.table.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let (win, _) = self.plan[best];
                self.summary = Some(format!(
                    "Top result: {} q={} — {:.1} fps (score {:.2}). Select a row to try it live.",
                    mode_name(&win),
                    win.quality,
                    scored(best),
                    best_score,
                ));
                // No auto-apply: hand back the user's previous settings.
                Some(self.restore)
            }
        }
    }

    /// Settings to restore if the user cancels mid-run.
    pub fn cancel(&self) -> Settings {
        self.restore
    }
}
