//! Calibration: measure where each ADC's input settles with nothing playing, then choose each
//! channel's centre (the resting level, placed mid-output) and gain so that the usable swing fills
//! the output window. Results are stored per card (serial) and input setup, and applied
//! automatically by `Device::arm` (`calibration = auto`).
//!
//! With the clamp off the input relaxes exponentially (τ ≈ 25–30 s) from the clamped level to its
//! resting point (docs/13 §3); the resting point is found by fitting `y = R + A·exp(-t/τ)` to the
//! per-frame means, so a measurement need not wait for the full settling.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adv7842::{ADC_FLOOR, ADC_TOP, CENTRE_DEFAULT, CENTRE_MAX, GAIN_DEFAULT, GAIN_MAX, OUT_MID};
use crate::fourcc::{Field, Format};

// ---------------------------------------------------------------- per-frame statistics

/// Statistics of one frame, per ADC field (index = ADC number).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FrameStats {
    /// Device time of the frame start, seconds from the first frame.
    pub t: f64,
    pub mean: [f64; 3],
    /// AC rms (standard deviation) in codes.
    pub rms: [f64; 3],
    pub min: [u32; 3],
    pub max: [u32; 3],
}

/// Compute `FrameStats` over every `line_step`-th line of an RG10/Y16 frame, all fields in one pass.
pub fn frame_stats(data: &[u8], format: Format, cx: usize, cy: usize, stride: usize, line_step: usize) -> FrameStats {
    let fields = format.fields();
    let mut sum = [0u64; 3];
    let mut sq = [0u64; 3];
    let mut lo = [u32::MAX; 3];
    let mut hi = [0u32; 3];
    let mut n = 0u64;
    let step = line_step.max(1);
    let mut acc = |w: u32, fl: &[Field]| {
        for (i, f) in fl.iter().enumerate() {
            let x = (w >> f.lsb) & ((1 << f.bits) - 1);
            sum[i] += x as u64;
            sq[i] += (x * x) as u64;
            lo[i] = lo[i].min(x);
            hi[i] = hi[i].max(x);
        }
    };
    for y in (0..cy).step_by(step) {
        let line = &data[y * stride..];
        match format {
            Format::Rgb10 => {
                for px in line[..cx * 4].chunks_exact(4) {
                    acc(u32::from_le_bytes([px[0], px[1], px[2], px[3]]), fields);
                }
            }
            Format::Y16 => {
                for px in line[..cx * 2].chunks_exact(2) {
                    acc(u16::from_le_bytes([px[0], px[1]]) as u32, fields);
                }
            }
        }
        n += cx as u64;
    }
    let mut s = FrameStats::default();
    for i in 0..fields.len() {
        let m = sum[i] as f64 / n as f64;
        s.mean[i] = m;
        s.rms[i] = (sq[i] as f64 / n as f64 - m * m).max(0.0).sqrt();
        s.min[i] = lo[i];
        s.max[i] = hi[i];
    }
    s
}

// ---------------------------------------------------------------- settling fit

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Settling {
    /// Resting level (fit), output codes.
    pub rest: f64,
    /// Time constant (s); 0 when the level was flat.
    pub tau: f64,
    /// Fitted level at t = 0 minus the resting level.
    pub amplitude: f64,
    /// RMS residual of the fit, codes.
    pub residual: f64,
    /// Level of the last second of data (for comparison with `rest`).
    pub last: f64,
}

/// Settling time constant measured with the clamp off (docs/13 §3: 25–30 s; calibration runs 18–33 s).
pub const TAU_TYPICAL: f64 = 28.0;
/// Range a fitted τ may take; outside it the curve is not the input's RC settling.
pub const TAU_RANGE: (f64, f64) = (10.0, 60.0);

/// Fit `y = R + A·exp(-t/τ)` (least squares, R and A closed form). τ is searched within `TAU_RANGE`
/// when the data spans at least 1.5 τ_typical; shorter runs cannot separate τ from R, so τ is fixed
/// at `TAU_TYPICAL` there.
pub fn fit_settling(t: &[f64], y: &[f64]) -> Settling {
    let n = t.len().min(y.len());
    let span = if n > 0 { t[n - 1] - t[0] } else { 0.0 };
    let (lo, hi) = if span >= 1.5 * TAU_TYPICAL { TAU_RANGE } else { (TAU_TYPICAL, TAU_TYPICAL) };
    fit_settling_in(t, y, lo, hi)
}

/// `fit_settling` with an explicit τ search range.
pub fn fit_settling_in(t: &[f64], y: &[f64], tau_lo: f64, tau_hi: f64) -> Settling {
    let n = t.len().min(y.len());
    let t_end = t.get(n.wrapping_sub(1)).copied().unwrap_or(0.0);
    let tail: Vec<f64> = (0..n).filter(|&i| t[i] >= t_end - 1.0).map(|i| y[i]).collect();
    let last = if tail.is_empty() { f64::NAN } else { tail.iter().sum::<f64>() / tail.len() as f64 };
    let flat_mean = y[..n].iter().sum::<f64>() / n.max(1) as f64;
    let flat_res = (y[..n].iter().map(|v| (v - flat_mean).powi(2)).sum::<f64>() / n.max(1) as f64).sqrt();
    let mut best = Settling { rest: flat_mean, tau: 0.0, amplitude: 0.0, residual: flat_res, last };
    if n < 4 {
        return best;
    }
    let mut tau = tau_lo;
    while tau <= tau_hi * 1.0001 {
        // y = R + A·e, e = exp(-t/τ): linear least squares in (R, A).
        let (mut se, mut see, mut sy, mut sey) = (0.0, 0.0, 0.0, 0.0);
        for i in 0..n {
            let e = (-t[i] / tau).exp();
            se += e;
            see += e * e;
            sy += y[i];
            sey += e * y[i];
        }
        let nn = n as f64;
        let det = nn * see - se * se;
        if det.abs() > 1e-12 {
            let a = (nn * sey - se * sy) / det;
            let r = (sy - a * se) / nn;
            let res = ((0..n).map(|i| (y[i] - r - a * (-t[i] / tau).exp()).powi(2)).sum::<f64>() / nn).sqrt();
            if res < best.residual * 0.999 {
                best = Settling { rest: r, tau, amplitude: a, residual: res, last };
            }
        }
        if tau_hi <= tau_lo {
            break;
        }
        tau *= 1.03;
    }
    best
}

// ---------------------------------------------------------------- levels

/// Output levels applied by `arm`: CP gain (2.8) and centre (12-bit ADC units) per ADC.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Levels {
    pub gain: [u16; 3],
    pub centre: [u16; 3],
}

impl Default for Levels {
    fn default() -> Self {
        Levels { gain: GAIN_DEFAULT, centre: CENTRE_DEFAULT }
    }
}

impl Levels {
    pub fn describe(&self) -> String {
        format!(
            "gain {:.3}/{:.3}/{:.3}, centre {:.2}/{:.2}/{:.2}",
            self.gain[0] as f64 / 256.0,
            self.gain[1] as f64 / 256.0,
            self.gain[2] as f64 / 256.0,
            self.centre[0] as f64 / 4.0,
            self.centre[1] as f64 / 4.0,
            self.centre[2] as f64 / 4.0
        )
    }
}

/// Output window, codes.
pub const CODE_MAX: f64 = 1023.0;
/// Fraction of the window the calibrated swing may use (headroom for noise and level differences).
pub const FILL: f64 = 0.95;
/// Output code the trough sits at with the bottom clamp (ADC0, clamp on).
pub const CLAMP_TROUGH_CODE: f64 = 24.0;

/// Choose gain and centre for one channel from its resting level `rest` (adc10).
///
/// Clamp off (and ADC1/2 always): the resting level is the centre (mid-window) and the gain makes the
/// smaller of the two distances to the ADC rails fill `FILL` of each half window (RF swings
/// symmetrically around the rest). Clamp on, ADC0: the trough is held at the rest and the wave hangs
/// upward, so the rest goes near code 0 and the distance to the top rail fills the window.
/// The gain acts on 12-bit data, so a gain above 1 adds real resolution (docs/14 §2).
pub fn choose_levels(adc: usize, clamp: bool, rest: f64) -> (u16, u16) {
    // Output codes per ADC step (k·G, k = 2 on B/C).
    let (slope, rest_code) = if clamp && adc == 0 {
        ((CODE_MAX - CLAMP_TROUGH_CODE) * FILL / (ADC_TOP - rest).max(1.0), CLAMP_TROUGH_CODE)
    } else {
        ((OUT_MID - 1.0) * FILL / (rest - ADC_FLOOR).min(ADC_TOP - rest).max(1.0), OUT_MID)
    };
    let k = if adc == 0 { 1.0 } else { 2.0 };
    let gain = ((slope / k * 256.0).round() as u16).clamp(1, GAIN_MAX);
    let slope = k * gain as f64 / 256.0;
    // rest_code = OUT_MID + slope * (rest - centre)
    let centre = rest - (rest_code - OUT_MID) / slope;
    (gain, (centre * 4.0).round().clamp(0.0, CENTRE_MAX as f64) as u16)
}

/// One ADC's measurement, taken at `measured_at` levels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCal {
    pub adc: u8,
    pub pin: u8,
    pub clamp: bool,
    /// AFE 0x0C analog gain the measurement was taken at (part of the key: it moves the rest).
    #[serde(default = "afe_normal")]
    pub afe_gain: u8,
    /// AFE 0x11 input offset the measurement was taken at (part of the key: it moves the rest).
    #[serde(default = "offset_none")]
    pub input_offset: u8,
    pub measured_at: Levels,
    pub settling: Settling,
    /// Noise with no input, codes rms (median over the last frames).
    pub noise_rms: f64,
    /// Resting ADC level (adc10).
    pub adc_rest: f64,
    /// Lowest/highest code seen.
    pub min: u32,
    pub max: u32,
    /// Chosen centre (12-bit ADC units) and gain for this channel.
    pub centre: u16,
    pub gain: u16,
    pub note: String,
}

fn offset_none() -> u8 {
    2
}

fn afe_normal() -> u8 {
    crate::adv7842::AFE_GAIN_NORMAL
}

// ---------------------------------------------------------------- storage

/// Store format; version 2 = levels as gain + centre.
pub const STORE_VERSION: u32 = 2;

/// All calibrations on this machine, keyed by card serial.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub version: u32,
    pub cards: Vec<CardCal>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CardCal {
    pub serial: String,
    /// UTC `YYYYmmdd-HHMMSS` of the last update.
    pub updated: String,
    pub channels: Vec<ChannelCal>,
}

impl Store {
    /// `$MWADC_CALIBRATION`, else `%APPDATA%\mwadc\calibration.json` (`$HOME/.config/mwadc/...` elsewhere).
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("MWADC_CALIBRATION") {
            return PathBuf::from(p);
        }
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("mwadc").join("calibration.json")
    }

    pub fn load(p: &Path) -> Result<Store> {
        if !p.exists() {
            return Ok(Store { version: STORE_VERSION, cards: Vec::new() });
        }
        let s = fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        // Stores from before the centre (CP offsets, version 1) do not apply any more: start afresh.
        let v: serde_json::Value = serde_json::from_slice(&s).with_context(|| format!("parsing {}", p.display()))?;
        if v.get("version").and_then(|v| v.as_u64()) != Some(STORE_VERSION as u64) {
            return Ok(Store { version: STORE_VERSION, cards: Vec::new() });
        }
        serde_json::from_value(v).with_context(|| format!("parsing {}", p.display()))
    }

    pub fn load_default() -> Result<Store> {
        Store::load(&Store::default_path())
    }

    pub fn save(&self, p: &Path) -> Result<()> {
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, p).with_context(|| format!("writing {}", p.display()))
    }

    /// Replace the entries for these (adc, pin, clamp) combinations of card `serial`.
    pub fn update(&mut self, serial: &str, chans: Vec<ChannelCal>) {
        self.version = STORE_VERSION;
        let idx = match self.cards.iter().position(|c| c.serial == serial) {
            Some(i) => i,
            None => {
                self.cards.push(CardCal { serial: serial.into(), ..Default::default() });
                self.cards.len() - 1
            }
        };
        let card = &mut self.cards[idx];
        card.channels.retain(|c| !chans.iter().any(|n| n.adc == c.adc && n.pin == c.pin && n.clamp == c.clamp && n.afe_gain == c.afe_gain && n.input_offset == c.input_offset));
        card.channels.extend(chans);
        card.channels.sort_by_key(|c| (c.adc, c.pin, c.clamp, c.afe_gain, c.input_offset));
        card.updated = crate::util::timestamp();
    }

    /// Levels for an input setup: per ADC, the stored entry for its pin and clamp state, else defaults.
    /// `None` when nothing is stored for any active ADC.
    pub fn levels_for(&self, serial: &str, pins: [u8; 3], clamp: bool, afe_gain: u8, input_offset: u8) -> Option<Levels> {
        let card = self.cards.iter().find(|c| c.serial == serial)?;
        let mut l = Levels::default();
        let mut any = false;
        for (i, &pin) in pins.iter().enumerate() {
            if pin == 0 {
                continue;
            }
            if let Some(c) = card.channels.iter().find(|c| c.adc as usize == i && c.pin == pin && c.clamp == clamp && c.afe_gain == afe_gain && c.input_offset == input_offset) {
                l.centre[i] = c.centre;
                l.gain[i] = c.gain;
                any = true;
            }
        }
        any.then_some(l)
    }

    /// How much of an input setup is calibrated: active ADCs, how many of them have a stored entry,
    /// and when the card's calibration was last updated.
    pub fn lookup(&self, serial: &str, pins: [u8; 3], clamp: bool, afe_gain: u8, input_offset: u8) -> CalLookup {
        let active = pins.iter().filter(|&&p| p != 0).count() as u32;
        let Some(card) = self.cards.iter().find(|c| c.serial == serial) else {
            return CalLookup { active, ..Default::default() };
        };
        let matched = pins
            .iter()
            .enumerate()
            .filter(|&(i, &pin)| {
                pin != 0 && card.channels.iter().any(|c| c.adc as usize == i && c.pin == pin && c.clamp == clamp && c.afe_gain == afe_gain && c.input_offset == input_offset)
            })
            .count() as u32;
        CalLookup { active, matched, updated: card.updated.clone(), levels: self.levels_for(serial, pins, clamp, afe_gain, input_offset) }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CalLookup {
    pub active: u32,
    pub matched: u32,
    /// UTC `YYYYmmdd-HHMMSS` of the card's last calibration update ("" if none).
    pub updated: String,
    pub levels: Option<Levels>,
}

impl CalLookup {
    /// Every active ADC has a stored calibration for this setup.
    pub fn complete(&self) -> bool {
        self.active > 0 && self.matched == self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_recovers_exponential() {
        let t: Vec<f64> = (0..600).map(|i| i as f64 * 0.1).collect();
        let y: Vec<f64> = t.iter().map(|&t| 445.0 + 120.0 * (-t / 27.0).exp()).collect();
        let s = fit_settling(&t, &y);
        assert!((s.rest - 445.0).abs() < 1.0, "{s:?}");
        assert!((s.tau - 27.0).abs() < 2.0, "{s:?}");
    }

    #[test]
    fn fit_short_run_uses_typical_tau() {
        // 20 s of a τ = 30 s settle: τ is fixed at 28 s, the rest lands near the truth instead of
        // running off with a long τ.
        let t: Vec<f64> = (0..200).map(|i| i as f64 * 0.1).collect();
        let y: Vec<f64> = t.iter().map(|&t| 300.0 + 80.0 * (-t / 30.0).exp()).collect();
        let s = fit_settling(&t, &y);
        assert_eq!(s.tau, TAU_TYPICAL);
        assert!((s.rest - 300.0).abs() < 6.0, "{s:?}");
    }

    #[test]
    fn fit_flat() {
        let t: Vec<f64> = (0..50).map(|i| i as f64 * 0.1).collect();
        let y = vec![300.0; 50];
        let s = fit_settling(&t, &y);
        assert!((s.rest - 300.0).abs() < 1e-6);
    }

    #[test]
    fn levels_centre() {
        use crate::adv7842::code_of_adc;
        // Y resting at adc10 300: rest centred, smaller rail distance (to the floor) fills the window
        let (g, c) = choose_levels(0, false, 300.0);
        let gq = g as f64 / 256.0;
        assert!((gq - 511.0 * FILL / 298.0).abs() < 0.01, "{gq}");
        assert!((code_of_adc(0, 300.0, g, c) - 512.0).abs() <= 1.0);
        let floor = code_of_adc(0, ADC_FLOOR, g, c);
        assert!(floor > 0.0 && floor < 30.0, "{floor}");
        // B/C: same window with half the gain
        let (gb, cb) = choose_levels(2, false, 300.0);
        assert!((gb as f64 - g as f64 / 2.0).abs() <= 1.0);
        assert!((code_of_adc(2, 300.0, gb, cb) - 512.0).abs() <= 2.0);
        // clamp on, ADC0: rest near the bottom, top rail at the top
        let (gc, cc) = choose_levels(0, true, 250.0);
        assert!((code_of_adc(0, 250.0, gc, cc) - CLAMP_TROUGH_CODE).abs() <= 1.0);
        assert!(code_of_adc(0, ADC_TOP, gc, cc) <= 1023.0);
    }

    #[test]
    fn store_roundtrip() {
        let mut s = Store::default();
        let ch = |adc, off| ChannelCal {
            adc,
            pin: 4,
            clamp: false,
            afe_gain: 0x1F,
            input_offset: 0,
            measured_at: Levels::default(),
            adc_rest: 0.0,
            settling: fit_settling(&[0.0], &[1.0]),
            noise_rms: 1.0,
            min: 0,
            max: 0,
            centre: off,
            gain: 0x100,
            note: String::new(),
        };
        s.update("A1", vec![ch(0, 0x210)]);
        s.update("A1", vec![ch(0, 0x220)]);
        assert_eq!(s.cards[0].channels.len(), 1);
        let l = s.levels_for("A1", [4, 0, 0], false, 0x1F, 0).unwrap();
        assert_eq!(l.centre[0], 0x220);
        assert!(s.levels_for("A1", [4, 0, 0], true, 0x1F, 0).is_none());
        assert!(s.levels_for("B2", [4, 0, 0], false, 0x1F, 0).is_none());
    }

    #[test]
    fn stats_rg10() {
        let mut d = Vec::new();
        for i in 0..8u32 {
            let w = 0xC000_0000 | (100 + i) << 20 | 500 << 10 | 2 * i;
            d.extend_from_slice(&w.to_le_bytes());
        }
        let s = frame_stats(&d, Format::Rgb10, 4, 2, 16, 1);
        assert_eq!(s.mean[0], 500.0);
        assert_eq!(s.min[2], 100);
        assert_eq!(s.max[1], 14);
    }
}
