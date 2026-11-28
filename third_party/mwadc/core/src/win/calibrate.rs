//! The calibration run on the card: arm with neutral levels, stream for a while with nothing
//! playing, fit each channel's settling, choose levels, optionally store them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};

use super::device::Device;
use super::stream::{FrameRef, StreamOptions};
use crate::adv7842::adc_of_code;
use crate::calib::{self, choose_levels, fit_settling, ChannelCal, FrameStats, Levels, Store};
use crate::config::{CalibUse, CaptureConfig};
use crate::say;

pub struct CalibrateOptions {
    /// Measurement length. With the clamp off, ≥ 30 s lets the fit see the settling curve;
    /// a card that has been armed (settled) for minutes needs only a few seconds.
    pub seconds: f64,
    /// Analyse every n-th line (the means need far fewer samples than the card delivers).
    pub line_step: usize,
    /// Store the result (`Store::default_path()` when `None` and `save` is set).
    pub save: bool,
    pub store_path: Option<std::path::PathBuf>,
}

impl Default for CalibrateOptions {
    fn default() -> Self {
        CalibrateOptions { seconds: 40.0, line_step: 8, save: true, store_path: None }
    }
}

pub type Progress = Box<dyn FnMut(f64, &FrameStats) + Send>;

/// Measure every active ADC of `cfg` and return the per-channel results. Leaves the card armed
/// with the new levels applied.
pub fn calibrate(dev: &mut Device, cfg: &CaptureConfig, opts: &CalibrateOptions, mut progress: Option<Progress>) -> Result<Vec<ChannelCal>> {
    let mut c = cfg.clone();
    c.calibration = CalibUse::Off;
    let neutral = Levels::default();
    c.gain = neutral.gain.map(Some);
    c.centre = neutral.centre.map(Some);
    c.audio = false;
    let armed = dev.arm(&c)?;
    let geo = armed.geometry.clone();
    let clamp = armed.recipe.fe.clamp;
    let afe_gain = armed.recipe.fe.afe_gain;
    let input_offset = armed.recipe.fe.input_offset;
    let pins = armed.recipe.adc;
    say!("calibrating {:.0} s with no input: {}", opts.seconds, c.describe());

    let frames = ((opts.seconds * geo.sample_rate_hz / geo.samples_per_frame() as f64).ceil() as u64).max(4);
    let stats: Arc<Mutex<Vec<FrameStats>>> = Arc::new(Mutex::new(Vec::with_capacity(frames as usize)));
    let st = stats.clone();
    let step = opts.line_step;
    let mut t0: Option<i64> = None;
    let sink = move |f: &FrameRef| -> Result<()> {
        let g = f.geometry;
        let mut s = calib::frame_stats(f.data, g.format, g.cx, g.cy, g.stride, step);
        let t = *t0.get_or_insert(f.start_100ns);
        s.t = (f.start_100ns - t) as f64 * 1e-7;
        if let Some(p) = progress.as_mut() {
            p((f.seq + 1) as f64 / frames as f64, &s);
        }
        st.lock().unwrap().push(s);
        Ok(())
    };
    let opts_s = StreamOptions { frames: Some(frames), stall: Duration::from_secs(3), ..Default::default() };
    let sum = dev.stream(Box::new(sink), opts_s)?.wait()?;
    let stats = std::mem::take(&mut *stats.lock().unwrap());
    if stats.len() < 4 {
        bail!("calibration got only {} frames", stats.len());
    }
    if sum.gaps > 0 {
        say!("  note: {} skipped frame(s) during calibration (timing still from device timestamps)", sum.gaps);
    }

    let t: Vec<f64> = stats.iter().map(|s| s.t).collect();
    let mut out = Vec::new();
    for (adc, &pin) in pins.iter().enumerate() {
        if pin == 0 {
            continue;
        }
        let y: Vec<f64> = stats.iter().map(|s| s.mean[adc]).collect();
        let fit = fit_settling(&t, &y);
        let tail = &stats[stats.len() * 3 / 4..];
        let mut noise: Vec<f64> = tail.iter().map(|s| s.rms[adc]).collect();
        noise.sort_by(|a, b| a.total_cmp(b));
        let noise_rms = noise[noise.len() / 2];
        let min = stats.iter().map(|s| s.min[adc]).min().unwrap_or(0);
        let max = stats.iter().map(|s| s.max[adc]).max().unwrap_or(0);
        let adc_rest = adc_of_code(adc, fit.rest, neutral.gain[adc], neutral.centre[adc]);
        let (gain, centre) = choose_levels(adc, clamp, adc_rest);
        let mut note = String::new();
        if fit.tau > 0.0 && fit.tau > t.last().copied().unwrap_or(0.0) {
            note = format!("settling τ {:.0} s is longer than the measurement: resting level extrapolated", fit.tau);
        }
        say!(
            "  ADC{adc} (Ain{pin}): rest {:.1} = ADC level {:.0} (τ {:.1} s, fit residual {:.2}, last {:.1}), noise {:.2} rms, codes {min}..{max} -> centre {:.2}, gain {:.3}{}",
            fit.rest,
            adc_rest,
            fit.tau,
            fit.residual,
            fit.last,
            noise_rms,
            centre as f64 / 4.0,
            gain as f64 / 256.0,
            if note.is_empty() { String::new() } else { format!(" ({note})") }
        );
        out.push(ChannelCal { adc: adc as u8, pin, clamp, afe_gain, input_offset, measured_at: neutral, settling: fit, noise_rms, adc_rest, min, max, centre, gain, note });
    }

    if opts.save {
        let p = opts.store_path.clone().unwrap_or_else(Store::default_path);
        let mut store = Store::load(&p)?;
        store.update(&dev.info().serial, out.clone());
        store.save(&p)?;
        say!("calibration stored in {}", p.display());
    }

    // Apply the new levels right away.
    let mut fe = dev.armed().map(|a| a.recipe.fe).unwrap_or_default();
    for ch in &out {
        fe.centre[ch.adc as usize] = ch.centre;
        fe.gain[ch.adc as usize] = ch.gain;
    }
    dev.set_front_end(fe)?;
    Ok(out)
}
