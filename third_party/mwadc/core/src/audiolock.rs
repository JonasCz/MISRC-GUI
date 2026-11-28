//! Line-in audio locked to the RF sample clock (docs/12 §9).
//!
//! The codec, the ADV7842 crystal and the device timestamp counter are three free-running clocks
//! (audio/RF = nominal + 65 ppm on board 0). Both streams carry device timestamps, so audio can be
//! resampled onto the RF timeline: output audio sample `n` belongs to RF sample `n × rf_nominal /
//! audio_nominal` of the current RF stream, and output sample 0 to RF sample 0.
//!
//! ```text
//!   RF stream (DMA thread) ── frame start timestamp, RF sample index ──► RfClock (shared)
//!                                                                          │ fit: RF index ↔ device time
//!   audio thread: block + timestamp ─► AudioLock ◄────────────────────────┘
//!                 fit: input index ↔ device time
//!                 error = output count at this block − RF index at its time × A/R
//!                 step  = fitted clock ratio × (1 + PI(error))   (slow loop, ~0.1 Hz)
//!                 windowed-sinc resampler reads the input at position p, p += step
//! ```
//!
//! The resampler is written out here rather than taken from a crate: alignment and the ±1 sample
//! count need the exact fractional read position of every output sample, which chunked
//! resamplers hide (and their chunking adds latency). Ratio changes only move the step, so the
//! read position stays continuous: no dropped or repeated samples, no clicks.
//!
//! Added latency: the filter's half length (48 samples, 1 ms) plus the wait for the RF frame
//! that covers the audio (output is held until its RF samples are known, so a segment can end
//! exactly where the RF stream ends): 0 to one frame (≈ 26 ms at 512 lines, 40 MSPS).
//!
//! Timestamps are taken as the time of a block's first sample and of a frame's first sample.
//! The absolute RF/audio offset this implies is not verified on hardware (needs one signal on
//! both inputs); the rate lock does not depend on it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

/// Device timestamp units per second (100 ns).
const TS_HZ: f64 = 1e7;

/// y = y0 + slope × (x − x0).
#[derive(Clone, Copy, Debug)]
pub struct Line {
    pub x0: f64,
    pub y0: f64,
    pub slope: f64,
}

impl Line {
    pub fn at(&self, x: f64) -> f64 {
        self.y0 + self.slope * (x - self.x0)
    }
    pub fn inv(&self, y: f64) -> f64 {
        self.x0 + (y - self.y0) / self.slope
    }
}

/// Least-squares line through the points, centred on their mean (no cancellation with large
/// timestamps). The slope is pulled towards `prior` with weight `lambda` (= (point noise / prior
/// slope uncertainty)², in x units squared): the first few points of a stream span milliseconds,
/// and their jitter alone would put the slope hundreds of ppm off. Over a second of points the
/// prior no longer matters. `None` without points.
fn fit(pts: &VecDeque<(f64, f64)>, prior: f64, lambda: f64) -> Option<Line> {
    let &(ax, ay) = pts.front()?;
    let n = pts.len() as f64;
    let (mut sx, mut sy) = (0.0, 0.0);
    for &(x, y) in pts {
        sx += x - ax;
        sy += y - ay;
    }
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for &(x, y) in pts {
        let (dx, dy) = (x - ax - mx, y - ay - my);
        sxx += dx * dx;
        sxy += dx * dy;
    }
    Some(Line { x0: ax + mx, y0: ay + my, slope: (sxy + lambda * prior) / (sxx + lambda) })
}

/// Prior slope uncertainty: crystals are within ±100 ppm, so 200 ppm is a loose bound.
const PRIOR_PPM: f64 = 200.0;
/// Timestamp jitter assumed for the prior weights: audio blocks fit to ≈ 1 sample rms (21 µs);
/// RF frame timestamps are taken as 5 µs.
const AUDIO_TS_NOISE: f64 = TS_HZ / 48000.0;
const RF_TS_NOISE: f64 = 50.0;

// ---------------------------------------------------------------- RF clock reference

/// RF frames kept for the fit: the last 4 s (clock wander is ±1 ppm over minutes, so a longer
/// window only adds lag), at most this many.
const RF_FIT_SECONDS: f64 = 4.0;
const RF_FIT_MAX: usize = 1024;

#[derive(Debug, Default)]
struct RfState {
    /// Streams started so far; 0 = none.
    epoch: u64,
    nominal_hz: f64,
    spf: u64,
    t0: Option<f64>,
    /// (timestamp, RF index of the frame's first sample)
    pts: VecDeque<(f64, f64)>,
    map: Option<Line>,
    /// RF index after the last published frame.
    known_end: u64,
    ended: bool,
    /// Where the previous stream ended: (epoch, RF samples).
    prev_end: Option<(u64, u64)>,
}

/// What the RF stream publishes for the audio side: per frame, its start timestamp and RF sample
/// index (skipped frames included, so the index stays on the RF timeline). One per device; a
/// mutex is enough (≈ 40 frames/s write, 250 audio blocks/s read, both a few µs).
#[derive(Debug, Default)]
pub struct RfClock {
    st: Mutex<RfState>,
}

/// A consistent view of the current RF stream.
#[derive(Clone, Debug)]
pub struct RfView {
    pub epoch: u64,
    pub nominal_hz: f64,
    /// Timestamp of RF sample 0.
    pub t0: f64,
    /// RF sample index as a function of the device timestamp.
    pub map: Line,
    /// RF samples covered by the frames published so far.
    pub known_end: u64,
    /// The stream ended; `known_end` is its length.
    pub ended: bool,
    pub prev_end: Option<(u64, u64)>,
}

impl RfClock {
    pub fn new() -> Arc<RfClock> {
        Arc::new(RfClock::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RfState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new RF stream starts: its first frame will be RF sample 0.
    pub fn begin(&self, nominal_hz: f64, samples_per_frame: u64) {
        let mut s = self.lock();
        let prev_end = (s.epoch > 0).then_some((s.epoch, s.known_end));
        *s = RfState { epoch: s.epoch + 1, nominal_hz, spf: samples_per_frame, prev_end, ..Default::default() };
    }

    /// A frame starting at device time `start_100ns` with RF sample `index` (skipped frames
    /// counted) was captured.
    pub fn frame(&self, start_100ns: i64, index: u64) {
        let mut s = self.lock();
        let t = start_100ns as f64;
        let t0 = t - index as f64 * TS_HZ / s.nominal_hz.max(1.0);
        s.t0.get_or_insert(t0);
        s.pts.push_back((t, index as f64));
        while s.pts.len() > RF_FIT_MAX || s.pts.len() > 2 && t - s.pts[0].0 > RF_FIT_SECONDS * TS_HZ {
            s.pts.pop_front();
        }
        let nom = s.nominal_hz / TS_HZ;
        s.map = fit(&s.pts, nom, (RF_TS_NOISE / (PRIOR_PPM * 1e-6)).powi(2));
        s.known_end = index + s.spf;
    }

    /// The stream ended after the last published frame.
    pub fn end(&self) {
        self.lock().ended = true;
    }

    /// `None` until the current stream has published a frame.
    pub fn view(&self) -> Option<RfView> {
        let s = self.lock();
        let t0 = s.t0?;
        let map = s.map?;
        Some(RfView { epoch: s.epoch, nominal_hz: s.nominal_hz, t0, map, known_end: s.known_end, ended: s.ended, prev_end: s.prev_end })
    }
}

// ---------------------------------------------------------------- resampling kernel

/// Taps either side of the read position.
const HALF: usize = 48;
const TAPS: usize = 2 * HALF;
/// Fractional positions in the table; coefficients in between are interpolated linearly.
const PHASES: usize = 1024;
/// Cut-off as a fraction of the input rate (21.6 kHz at 48 kHz) and Kaiser β (≈ 115 dB stop band,
/// pass band flat to ≈ 20 kHz).
const CUTOFF: f64 = 0.45;
const KAISER_BETA: f64 = 12.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..60 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Windowed-sinc table: row `ph` holds the taps for read position frac = ph / PHASES, tap `j`
/// weighting input `floor(p) − (HALF − 1) + j`. Each row sums to 1 (flat DC).
fn kernel() -> &'static [[f32; TAPS]] {
    static K: OnceLock<Vec<[f32; TAPS]>> = OnceLock::new();
    K.get_or_init(|| {
        let i0b = bessel_i0(KAISER_BETA);
        (0..=PHASES)
            .map(|ph| {
                let frac = ph as f64 / PHASES as f64;
                let mut row = [0f64; TAPS];
                for (j, r) in row.iter_mut().enumerate() {
                    let x = j as f64 - (HALF as f64 - 1.0) - frac;
                    let t = 2.0 * CUTOFF * x;
                    let sinc = if t.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t) };
                    let w = x / HALF as f64;
                    let win = if w.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - w * w).sqrt()) / i0b };
                    *r = 2.0 * CUTOFF * sinc * win;
                }
                let s: f64 = row.iter().sum();
                row.map(|v| (v / s) as f32)
            })
            .collect()
    })
}

// ---------------------------------------------------------------- the lock

/// Audio timestamps kept for the input fit.
const AUDIO_FIT_SECONDS: f64 = 4.0;
const AUDIO_FIT_MAX: usize = 2048;
/// Refit every this many blocks once the fit has settled (every block before).
const REFIT_EVERY: u64 = 16;
/// Loop: natural frequency and damping. 0.1 Hz averages timestamp jitter over seconds while
/// following the ±1 ppm wander; the fitted clock ratio (feed-forward) does the heavy lifting.
const LOOP_HZ: f64 = 0.1;
const LOOP_ZETA: f64 = 1.0;
/// Audio blocks seen before a segment starts (64 ms, once per RF stream).
const START_BLOCKS: usize = 16;
/// Most the loop may move the step away from the fitted ratio.
const MAX_CORRECTION: f64 = 300e-6;

#[derive(Clone, Copy, Debug)]
pub struct LockParams {
    /// Nominal line-in rate (48000).
    pub audio_hz: f64,
    /// An RF stream that started at most this long before the first audio still gets its output
    /// from RF sample 0, padded with silence; later audio starts at its own RF position.
    pub max_pad_s: f64,
    /// Audio held while no RF stream runs (it may start shortly).
    pub hold_s: f64,
}

impl Default for LockParams {
    fn default() -> Self {
        LockParams { audio_hz: 48000.0, max_pad_s: 2.0, hold_s: 2.0 }
    }
}

/// Locked output, valid during the callback.
#[derive(Debug)]
pub struct Chunk<'a> {
    /// Interleaved L, R, left-justified 32-bit like the input.
    pub samples: &'a [i32],
    /// Output index of the first sample: RF sample `position × rf_hz / audio_hz` of the stream.
    pub position: u64,
    /// RF stream (epoch) this belongs to; a new one restarts `position`.
    pub epoch: u64,
    /// First chunk of this RF stream.
    pub first: bool,
    /// Last chunk: the RF stream ended and its audio is complete.
    pub last: bool,
    /// Nominal RF rate the output is locked to.
    pub rf_hz: f64,
    /// Device time of the first sample (from the RF fit).
    pub start_100ns: i64,
    /// Input samples per output sample (1 + 65 ppm on board 0).
    pub step: f64,
    /// Output count minus the RF target at the latest block, in output samples.
    pub error: f64,
}

struct Segment {
    epoch: u64,
    rf_hz: f64,
    /// Next output index and its input read position.
    n: u64,
    p: f64,
    step: f64,
    integ: f64,
    error: f64,
    end: Option<u64>,
    first: bool,
}

pub struct AudioLock {
    clock: Arc<RfClock>,
    par: LockParams,
    /// Stereo input from input index `buf_start`; missed blocks are filled with silence.
    buf: VecDeque<[f32; 2]>,
    buf_start: i64,
    /// (input index of a block's first sample, its timestamp)
    pts: VecDeque<(f64, f64)>,
    afit: Option<Line>,
    blocks: u64,
    block_frames: usize,
    seg: Option<Segment>,
    done_epoch: u64,
    out: Vec<i32>,
    /// Test hooks: skip the filter (long count-only runs) / record every read position.
    filter: bool,
    trace: Option<Vec<f64>>,
}

impl AudioLock {
    pub fn new(clock: Arc<RfClock>, par: LockParams) -> AudioLock {
        AudioLock {
            clock,
            par,
            buf: VecDeque::new(),
            buf_start: 0,
            pts: VecDeque::new(),
            afit: None,
            blocks: 0,
            block_frames: 0,
            seg: None,
            done_epoch: 0,
            out: Vec::new(),
            filter: true,
            trace: None,
        }
    }

    pub fn set_audio_hz(&mut self, hz: f64) {
        if hz > 0.0 {
            self.par.audio_hz = hz;
        }
    }

    fn in_end(&self) -> i64 {
        self.buf_start + self.buf.len() as i64
    }

    /// Output index at input position `pos` in the running segment.
    pub fn out_index_at(&self, pos: f64) -> Option<f64> {
        self.seg.as_ref().map(|s| s.n as f64 + (pos - s.p) / s.step)
    }

    /// A segment is running (an RF stream is being followed).
    pub fn locked(&self) -> bool {
        self.seg.is_some()
    }

    /// Feed one input block (interleaved L, R) with its device timestamp; `missed` blocks were
    /// lost right before it. Locked output goes to `out` (zero or more chunks).
    pub fn push(&mut self, samples: &[i32], ts_100ns: i64, missed: u32, out: &mut dyn FnMut(&Chunk) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let frames = samples.len() / 2;
        if self.block_frames == 0 {
            self.block_frames = frames;
        }
        // Lost blocks become silence so input indices stay on the codec's clock.
        let gap = missed as usize * self.block_frames;
        self.buf.extend(std::iter::repeat_n([0f32; 2], gap));
        let ib = self.in_end();
        // Left-justified 24-bit: the top 24 bits are the sample (exact in f32).
        self.buf.extend(samples.chunks_exact(2).map(|c| [(c[0] >> 8) as f32, (c[1] >> 8) as f32]));
        self.add_point(ib as f64, ts_100ns as f64);

        let view = self.clock.view();
        // The followed stream was replaced by a new one: finish it where it ended.
        if let (Some(s), Some(v)) = (self.seg.as_mut(), view.as_ref()) {
            if v.epoch != s.epoch {
                let end = v.prev_end.filter(|e| e.0 == s.epoch).map(|e| e.1);
                s.end = Some(end.map(|e| out_count(e, s.rf_hz, self.par.audio_hz)).unwrap_or(s.n).max(s.n));
            } else if v.ended {
                s.end = Some(out_count(v.known_end, s.rf_hz, self.par.audio_hz));
            }
        }
        if self.seg.is_none() {
            if let Some(v) = view.as_ref().filter(|v| v.epoch > self.done_epoch) {
                self.start_segment(v);
            }
        }
        if let Some(v) = view.as_ref() {
            if self.seg.as_ref().is_some_and(|s| s.epoch == v.epoch) {
                self.steer(ib as f64, v);
            }
        }
        if self.seg.is_some() {
            let known = view.as_ref().filter(|v| Some(v.epoch) == self.seg.as_ref().map(|s| s.epoch)).map(|v| v.known_end);
            self.produce(known, out)?;
        }
        self.trim();
        Ok(())
    }

    fn add_point(&mut self, i: f64, t: f64) {
        self.pts.push_back((i, t));
        while self.pts.len() > AUDIO_FIT_MAX || self.pts.len() > 2 && t - self.pts[0].1 > AUDIO_FIT_SECONDS * TS_HZ {
            self.pts.pop_front();
        }
        self.blocks += 1;
        if self.afit.is_none() || self.blocks.is_multiple_of(REFIT_EVERY) || self.pts.len() < 64 {
            // Time as a function of the input index; one block so far: the nominal rate.
            let nom = TS_HZ / self.par.audio_hz;
            self.afit = fit(&self.pts, nom, (AUDIO_TS_NOISE / (PRIOR_PPM * 1e-6 * nom)).powi(2));
        }
    }

    /// Align output sample 0 with RF sample 0 (or, for audio that starts long after the RF stream,
    /// the first output sample with the RF sample it falls on).
    fn start_segment(&mut self, v: &RfView) {
        // A few blocks first: one block's timestamp is ±1 sample, sixteen fit to ±0.25.
        if self.pts.len() < START_BLOCKS {
            return;
        }
        let Some(af) = self.afit else { return };
        let a_over_r = self.par.audio_hz / v.nominal_hz;
        let t_first = af.at(self.buf_start as f64);
        // A stream that ended before this audio began has nothing to lock to: wait for the next.
        if v.ended && t_first >= v.map.inv(v.known_end as f64) {
            self.done_epoch = v.epoch;
            return;
        }
        let (n0, p0) = if t_first - v.t0 <= self.par.max_pad_s * TS_HZ {
            // Input before p0 is dropped; positions before the first input read as silence.
            (0u64, af.inv(v.t0))
        } else {
            let n0 = (v.map.at(t_first) * a_over_r).ceil().max(0.0) as u64;
            (n0, af.inv(v.map.inv(n0 as f64 / a_over_r)))
        };
        let step = self.ratio(v, &af);
        self.seg = Some(Segment { epoch: v.epoch, rf_hz: v.nominal_hz, n: n0, p: p0, step, integ: 0.0, error: 0.0, end: None, first: true });
    }

    /// Fitted input samples per output sample.
    fn ratio(&self, v: &RfView, af: &Line) -> f64 {
        let audio_per_ts = 1.0 / af.slope;
        let out_per_ts = v.map.slope * self.par.audio_hz / v.nominal_hz;
        audio_per_ts / out_per_ts
    }

    /// One loop update at the block starting at input index `ib`.
    fn steer(&mut self, ib: f64, v: &RfView) {
        let Some(af) = self.afit else { return };
        let ff = self.ratio(v, &af);
        let a_over_r = self.par.audio_hz / v.nominal_hz;
        let nb = self.block_frames.max(1) as f64;
        let w = 2.0 * std::f64::consts::PI * LOOP_HZ * nb / self.par.audio_hz;
        let (kp, ki) = (2.0 * LOOP_ZETA * w / nb, w * w / nb);
        let s = self.seg.as_mut().expect("segment");
        let target = v.map.at(af.at(ib)) * a_over_r;
        let e = s.n as f64 + (ib - s.p) / s.step - target;
        s.error = e;
        s.integ = (s.integ + ki * e).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        let u = (kp * e + s.integ).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        // Too many outputs (e > 0) → read the input faster.
        s.step = ff * (1.0 + u);
    }

    fn produce(&mut self, known_end: Option<u64>, out: &mut dyn FnMut(&Chunk) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let audio_hz = self.par.audio_hz;
        let in_end = self.in_end();
        let k = kernel();
        let s = self.seg.as_mut().expect("segment");
        // Output waits for the RF frames that cover it, so the stream can end exactly with the RF.
        let mut limit = known_end.map(|e| out_count(e, s.rf_hz, audio_hz)).unwrap_or(s.n);
        if let Some(e) = s.end {
            limit = e;
        }
        let n_first = s.n;
        self.out.clear();
        while s.n < limit {
            let ip = s.p.floor();
            if ip as i64 + HALF as i64 >= in_end {
                break;
            }
            let (l, r) = if self.filter {
                let f = (s.p - ip) * PHASES as f64;
                let ph = (f as usize).min(PHASES - 1);
                let w = f - ph as f64;
                let (r0, r1) = (&k[ph], &k[ph + 1]);
                let base = ip as i64 - (HALF as i64 - 1);
                let (mut l, mut r) = (0f64, 0f64);
                for j in 0..TAPS {
                    let i = base + j as i64;
                    if i < self.buf_start {
                        continue;
                    }
                    let x = self.buf[(i - self.buf_start) as usize];
                    let c = r0[j] as f64 + (r1[j] as f64 - r0[j] as f64) * w;
                    l += c * x[0] as f64;
                    r += c * x[1] as f64;
                }
                (l, r)
            } else {
                (0.0, 0.0)
            };
            if let Some(t) = self.trace.as_mut() {
                t.push(s.p);
            }
            self.out.push(to_i32(l));
            self.out.push(to_i32(r));
            s.n += 1;
            s.p += s.step;
        }
        let last = s.end.is_some_and(|e| s.n >= e);
        if !self.out.is_empty() || last {
            let v = self.clock.view();
            let t = v.filter(|v| v.epoch == s.epoch).map(|v| v.map.inv(n_first as f64 * s.rf_hz / audio_hz) as i64).unwrap_or(0);
            let c = Chunk {
                samples: &self.out,
                position: n_first,
                epoch: s.epoch,
                first: s.first,
                last,
                rf_hz: s.rf_hz,
                start_100ns: t,
                step: s.step,
                error: s.error,
            };
            s.first = false;
            out(&c)?;
        }
        if last {
            self.done_epoch = s.epoch;
            self.seg = None;
        }
        Ok(())
    }

    /// Drop input that is no longer needed: behind the read position, or older than the hold time
    /// while waiting for an RF stream.
    fn trim(&mut self) {
        let keep_from = match &self.seg {
            Some(s) => s.p.floor() as i64 - HALF as i64,
            None => self.in_end() - (self.par.hold_s * self.par.audio_hz) as i64,
        };
        let n = (keep_from - self.buf_start).clamp(0, self.buf.len() as i64) as usize;
        self.buf.drain(..n);
        self.buf_start += n as i64;
    }
}

/// Output samples that belong to `rf` RF samples.
pub fn out_count(rf: u64, rf_hz: f64, audio_hz: f64) -> u64 {
    (rf as f64 * audio_hz / rf_hz).round() as u64
}

fn to_i32(v: f64) -> i32 {
    const MAX: f64 = ((1 << 23) - 1) as f64;
    (v.round().clamp(-MAX - 1.0, MAX) as i32) << 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64* with Box-Muller: reproducible noise without a dependency.
    struct Rng(u64);
    impl Rng {
        fn u(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let (a, b) = (self.u().max(1e-300), self.u());
            (-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()
        }
    }

    const BF: usize = 192;
    const RF_NOM: f64 = 40e6;
    const SPF: u64 = 2024 * 512;

    struct Scenario {
        seconds: f64,
        /// Clock errors against device time (board 0: audio +19 ppm, RF −46 ppm).
        audio_ppm: f64,
        rf_ppm: f64,
        /// Slow sinusoidal wander of each clock (ppm amplitude, period s).
        wander_ppm: f64,
        wander_s: f64,
        audio_start_s: f64,
        rf_start_s: f64,
        /// RF stops after this many seconds of streaming (None = runs past the end).
        rf_stop_s: Option<f64>,
        audio_jitter_s: f64,
        rf_jitter_s: f64,
        /// Audio block numbers to drop; RF frame numbers to skip.
        drop_blocks: Vec<u64>,
        skip_frames: Vec<u64>,
        tone_hz: f64,
        filter: bool,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Scenario {
                seconds: 30.0,
                audio_ppm: 19.0,
                rf_ppm: -46.2,
                wander_ppm: 2.0,
                wander_s: 120.0,
                audio_start_s: 0.0,
                rf_start_s: 0.3,
                rf_stop_s: None,
                audio_jitter_s: 1.0 / 48000.0,
                rf_jitter_s: 2e-6,
                drop_blocks: vec![],
                skip_frames: vec![],
                tone_hz: 1000.0,
                filter: false,
            }
        }
    }

    #[derive(Default)]
    struct Outcome {
        /// (true time, |count error|) per audio block while locked.
        max_err: f64,
        max_err_after_2s: f64,
        final_err: f64,
        /// Output sample 0's input position minus the true one.
        start_err: f64,
        total_out: u64,
        expected_total: Option<u64>,
        positions_contiguous: bool,
        steps: Vec<f64>,
        out: Vec<i32>,
        trace: Vec<f64>,
        /// True time of each input index (block start, rate) for the sine reference.
        blocks: Vec<(i64, f64, f64)>,
    }

    fn wander(t: f64, s: &Scenario, phase: f64) -> f64 {
        s.wander_ppm * 1e-6 * (2.0 * std::f64::consts::PI * t / s.wander_s + phase).sin()
    }

    /// Discrete-event run: audio blocks arrive 2 ms after they end, RF frames 5 ms after they end
    /// (DMA), both with timestamps of their first sample plus jitter.
    fn run(s: &Scenario) -> Outcome {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let clock = RfClock::new();
        let mut lock = AudioLock::new(clock.clone(), LockParams::default());
        lock.filter = s.filter;
        if s.filter {
            lock.trace = Some(Vec::new());
        }
        let a_hz = 48000.0 * (1.0 + s.audio_ppm * 1e-6);
        let r_hz = RF_NOM * (1.0 + s.rf_ppm * 1e-6);

        // RF frames: (true start, index); index counts skipped frames.
        let mut frames: Vec<(f64, u64)> = Vec::new();
        let mut t = s.rf_start_s;
        let rf_end_t = s.rf_stop_s.map(|d| s.rf_start_s + d).unwrap_or(f64::INFINITY);
        let mut k = 0u64;
        while t < s.seconds + 1.0 && t < rf_end_t {
            frames.push((t, k * SPF));
            t += SPF as f64 / (r_hz * (1.0 + wander(t, s, 0.0)));
            k += 1;
        }
        let rf_true = |t: f64| -> f64 {
            let i = frames.partition_point(|f| f.0 <= t).max(1) - 1;
            let (t0, k0) = frames[i];
            k0 as f64 + (t - t0) * r_hz * (1.0 + wander(t0, s, 0.0))
        };
        // Audio blocks: (true start, rate).
        let mut blocks: Vec<(i64, f64, f64)> = Vec::new();
        let mut t = s.audio_start_s;
        let mut idx = 0i64;
        while t < s.seconds {
            let rate = a_hz * (1.0 + wander(t, s, 1.3));
            blocks.push((idx, t, rate));
            t += BF as f64 / rate;
            idx += BF as i64;
        }

        // Events in delivery order.
        enum Ev {
            Frame(usize),
            Block(usize),
            End,
        }
        let mut evs: Vec<(f64, Ev)> = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if !s.skip_frames.contains(&(i as u64)) {
                evs.push((f.0 + SPF as f64 / r_hz + 0.005, Ev::Frame(i)));
            }
        }
        if s.rf_stop_s.is_some() {
            let last = frames.last().unwrap();
            evs.push((last.0 + SPF as f64 / r_hz + 0.006, Ev::End));
        }
        for (i, b) in blocks.iter().enumerate() {
            evs.push((b.1 + BF as f64 / b.2 + 0.002, Ev::Block(i)));
        }
        evs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        let mut o = Outcome { positions_contiguous: true, ..Default::default() };
        let mut next_pos: Option<u64> = None;
        let mut missed = 0u32;
        let mut started = false;
        let mut seg_start_t = 0.0;
        let a_over_r = 48000.0 / RF_NOM;
        let tone = |t: f64| (0.5 * 8388607.0 * (2.0 * std::f64::consts::PI * s.tone_hz * t).sin()).round() as i32;
        let mut samples = vec![0i32; BF * 2];
        for (_, ev) in evs {
            match ev {
                Ev::Frame(i) => {
                    if i == 0 {
                        clock.begin(RF_NOM, SPF);
                    }
                    let ts = ((frames[i].0 + rng.gauss() * s.rf_jitter_s) * TS_HZ).round() as i64;
                    clock.frame(ts, frames[i].1);
                }
                Ev::End => clock.end(),
                Ev::Block(i) => {
                    if s.drop_blocks.contains(&(i as u64)) {
                        missed += 1;
                        continue;
                    }
                    let (_, t0, rate) = blocks[i];
                    if s.filter {
                        for j in 0..BF {
                            let v = tone(t0 + j as f64 / rate) << 8;
                            samples[2 * j] = v;
                            samples[2 * j + 1] = -v;
                        }
                    }
                    let ts = ((t0 + rng.gauss() * s.audio_jitter_s) * TS_HZ).round() as i64;
                    let mut cb = |c: &Chunk| -> anyhow::Result<()> {
                        if let Some(p) = next_pos {
                            o.positions_contiguous &= c.position == p;
                        }
                        next_pos = Some(c.position + (c.samples.len() / 2) as u64);
                        o.total_out += (c.samples.len() / 2) as u64;
                        o.steps.push(c.step);
                        if s.filter {
                            o.out.extend_from_slice(c.samples);
                        }
                        Ok(())
                    };
                    lock.push(&samples, ts, missed, &mut cb).unwrap();
                    missed = 0;
                    if !started && lock.locked() {
                        started = true;
                        seg_start_t = t0;
                        // Input position of RF sample 0, from the true clocks.
                        let p_true = (s.rf_start_s - s.audio_start_s) * a_hz;
                        let p0 = lock.seg.as_ref().map(|g| g.p - g.n as f64 * g.step).unwrap();
                        o.start_err = p0 - p_true;
                    }
                    // Count error at this block's first sample against the true clocks.
                    if let Some(n_at) = lock.out_index_at(blocks[i].0 as f64) {
                        if t0 >= s.rf_start_s && t0 < rf_end_t - 0.1 {
                            let e = n_at - rf_true(t0) * a_over_r;
                            o.max_err = o.max_err.max(e.abs());
                            if t0 > seg_start_t + 2.0 {
                                o.max_err_after_2s = o.max_err_after_2s.max(e.abs());
                            }
                            o.final_err = e;
                        }
                    }
                }
            }
        }
        if s.rf_stop_s.is_some() {
            let f = frames.len() as u64 * SPF;
            o.expected_total = Some(out_count(f, RF_NOM, 48000.0));
        }
        o.trace = lock.trace.take().unwrap_or_default();
        o.blocks = blocks;
        o
    }

    #[test]
    fn kernel_response() {
        // Magnitude of the half-sample row (the worst case) at f (fraction of the input rate).
        let k = kernel();
        let mag = |f: f64| {
            let (mut re, mut im) = (0f64, 0f64);
            for (j, &c) in k[PHASES / 2].iter().enumerate() {
                let w = 2.0 * std::f64::consts::PI * f * j as f64;
                re += c as f64 * w.cos();
                im += c as f64 * w.sin();
            }
            20.0 * (re * re + im * im).sqrt().log10()
        };
        let (p1, p20, p23) = (mag(1.0 / 48.0), mag(20.0 / 48.0), mag(23.0 / 48.0));
        eprintln!("kernel at frac 0.5: 1 kHz {p1:+.6} dB, 20 kHz {p20:+.3} dB, 23 kHz {p23:+.1} dB");
        assert!(p1.abs() < 1e-5 && p20.abs() < 0.1 && p23 < -6.0);
    }

    #[test]
    fn kernel_rows_are_normalised_and_symmetric() {
        let k = kernel();
        assert_eq!(k.len(), PHASES + 1);
        for row in [&k[0], &k[PHASES / 3], &k[PHASES]] {
            let s: f64 = row.iter().map(|&v| v as f64).sum();
            assert!((s - 1.0).abs() < 1e-6, "{s}");
        }
        // frac 0 is an identity tap (sinc zero crossings are off-grid at 0.45, so only nearly).
        assert!(k[0][HALF - 1] > 0.85);
        // Row PHASES equals row 0 shifted by one tap.
        for j in 0..TAPS - 1 {
            assert!((k[PHASES][j + 1] - k[0][j]).abs() < 1e-6);
        }
    }

    #[test]
    fn tracks_rf_clock_for_ten_minutes() {
        let s = Scenario { seconds: 600.0, ..Default::default() };
        let o = run(&s);
        eprintln!("600 s: max |err| {:.3}, after 2 s {:.3}, final {:+.3} samples, start {:+.3}", o.max_err, o.max_err_after_2s, o.final_err, o.start_err);
        assert!(o.max_err < 1.0, "max error {}", o.max_err);
        assert!(o.final_err.abs() < 0.5, "final error {}", o.final_err);
        assert!(o.start_err.abs() < 1.0, "start error {}", o.start_err);
        assert!(o.positions_contiguous);
        // Steering found the ratio: audio/RF = +65.2 ppm → step ≈ 1 + 65.2e-6 (± wander).
        let m = o.steps[o.steps.len() / 2..].iter().sum::<f64>() / (o.steps.len() / 2) as f64;
        let ppm = (m - 1.0) * 1e6;
        assert!((ppm - 65.2).abs() < 3.0, "step {ppm} ppm");
    }

    #[test]
    fn missed_audio_block_and_rf_gaps_keep_sync() {
        let s = Scenario { seconds: 60.0, drop_blocks: vec![3000, 3001, 9000], skip_frames: vec![200, 201, 202, 900], ..Default::default() };
        let o = run(&s);
        eprintln!("gaps: max |err| {:.3}, final {:+.3}", o.max_err, o.final_err);
        assert!(o.max_err < 1.0, "max error {}", o.max_err);
        assert!(o.positions_contiguous);
    }

    #[test]
    fn audio_starting_after_rf_is_padded_from_rf_sample_0() {
        let s = Scenario { seconds: 20.0, audio_start_s: 0.35, rf_start_s: 0.3, ..Default::default() };
        let o = run(&s);
        eprintln!("audio late: start error {:+.3} input samples, max |err| {:.3}", o.start_err, o.max_err);
        assert!(o.start_err.abs() < 1.0, "start error {}", o.start_err);
        assert!(o.max_err < 1.0);
    }

    #[test]
    fn audio_after_an_ended_stream_waits_for_the_next() {
        let clock = RfClock::new();
        let mut lock = AudioLock::new(clock.clone(), LockParams::default());
        // A 1 s stream at t = 0..1 s, then audio from t = 1.5 s: nothing to lock to.
        clock.begin(RF_NOM, SPF);
        for k in 0..39u64 {
            clock.frame((k as f64 * SPF as f64 / RF_NOM * TS_HZ) as i64, k * SPF);
        }
        clock.end();
        let mut got: Vec<(u64, u64, bool)> = Vec::new();
        let block = vec![0i32; BF * 2];
        let mut t = 1.5;
        let feed = |lock: &mut AudioLock, t: &mut f64, n: usize, got: &mut Vec<(u64, u64, bool)>| {
            for _ in 0..n {
                lock.push(&block, (*t * TS_HZ) as i64, 0, &mut |c: &Chunk| {
                    got.push((c.epoch, c.position, c.first));
                    Ok(())
                })
                .unwrap();
                *t += BF as f64 / 48000.0;
            }
        };
        feed(&mut lock, &mut t, 100, &mut got);
        assert!(got.is_empty(), "{got:?}");
        // The next stream starts now: its output starts at position 0, flagged first.
        clock.begin(RF_NOM, SPF);
        let t0 = t;
        for k in 0..20u64 {
            clock.frame(((t0 + k as f64 * SPF as f64 / RF_NOM) * TS_HZ) as i64, k * SPF);
        }
        feed(&mut lock, &mut t, 100, &mut got);
        assert_eq!(got.first().copied(), Some((2, 0, true)));
    }

    #[test]
    fn segment_ends_with_the_rf_stream() {
        let s = Scenario { seconds: 25.0, rf_stop_s: Some(20.0), ..Default::default() };
        let o = run(&s);
        let want = o.expected_total.unwrap();
        eprintln!("ended: {} output samples, RF × 48000/40e6 = {want}", o.total_out);
        assert_eq!(o.total_out, want);
    }

    #[test]
    fn sine_stays_clean() {
        // Through the full loop (jitter, wander, an RF gap), against the ideal tone at each
        // output's read position: interpolation error only, the tracking error is tested above.
        let s = Scenario { seconds: 12.0, filter: true, skip_frames: vec![100], ..Default::default() };
        let o = run(&s);
        // True time of input position p (per-block rate).
        let t_of = |p: f64| -> f64 {
            let i = o.blocks.partition_point(|b| (b.0 as f64) <= p).max(1) - 1;
            let (i0, t0, rate) = o.blocks[i];
            t0 + (p - i0 as f64) / rate
        };
        let amp = 0.5 * 8388607.0;
        let (mut se, mut ss, mut max_e) = (0f64, 0f64, 0f64);
        // Skip the start (silence padding and the filter run-in) and the end.
        for (n, &p) in o.trace.iter().enumerate().skip(48000).take(o.trace.len().saturating_sub(96000)) {
            let want = amp * (2.0 * std::f64::consts::PI * s.tone_hz * t_of(p)).sin();
            let got = (o.out[2 * n] >> 8) as f64;
            let gr = (o.out[2 * n + 1] >> 8) as f64;
            let e = got - want;
            se += e * e + (gr + want).powi(2);
            ss += 2.0 * want * want;
            max_e = max_e.max(e.abs());
        }
        let snr = 10.0 * (ss / se).log10();
        let max_db = 20.0 * (max_e / amp).log10();
        eprintln!("1 kHz at -6 dBFS: SNR {snr:.1} dB, max sample error {max_db:.1} dB re amplitude ({max_e:.2} LSB)");
        assert!(snr > 100.0, "SNR {snr}");
        assert!(max_db < -95.0, "max error {max_db}");
    }
}
