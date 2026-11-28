//! `export`: turn a capture session into one continuous sample file per ADC (runs on any OS).
//!
//! Frames are gapless when captured with `mwadc capture` (window = whole CP frame), so the frames are
//! simply concatenated. Skipped frames (`gap_frames` in frames.csv) are reported, not filled.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::fourcc::{extract, pixel_word, Format};
use crate::say;

/// `capture.json` next to `frames.raw` (frames back to back, `frame_bytes` each).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureMeta {
    pub format: Format,
    pub cx: usize,
    pub cy: usize,
    pub stride: usize,
    pub frame_bytes: usize,
    pub frames: usize,
    pub sample_rate_hz: Option<f64>,
    pub mode: String,
    pub notes: Vec<String>,
}

pub fn run(dir: &Path, kind: &str, max_frames: Option<usize>) -> Result<()> {
    if kind != "s16" && kind != "u16" {
        bail!("--kind takes s16 or u16");
    }
    let meta: CaptureMeta = serde_json::from_slice(
        &fs::read(dir.join("capture.json")).with_context(|| format!("no capture.json in {}", dir.display()))?,
    )?;
    let raw_len = fs::metadata(dir.join("frames.raw")).context("frames.raw")?.len() as usize;
    let nframes = (raw_len / meta.frame_bytes).min(max_frames.unwrap_or(usize::MAX));
    if nframes == 0 {
        bail!("frames.raw holds no complete frame");
    }
    let gaps: i64 = read_gaps(dir).iter().take(nframes).sum();
    let fields = meta.format.fields();
    let bpp = meta.format.bytes_per_pixel();
    let mut outs: Vec<BufWriter<File>> = fields
        .iter()
        .map(|f| File::create(dir.join(format!("{}.{kind}", f.name))).map(|f| BufWriter::with_capacity(4 << 20, f)))
        .collect::<std::io::Result<_>>()?;
    let mut stats = vec![(u32::MAX, 0u32, 0u64); fields.len()];

    let mut src = File::open(dir.join("frames.raw"))?;
    let mut frame = vec![0u8; meta.frame_bytes];
    for _ in 0..nframes {
        src.read_exact(&mut frame)?;
        for y in 0..meta.cy {
            let line = &frame[y * meta.stride..y * meta.stride + meta.cx * bpp];
            for px in line.chunks_exact(bpp) {
                let w = pixel_word(px);
                for ((fl, out), st) in fields.iter().zip(outs.iter_mut()).zip(stats.iter_mut()) {
                    let x = extract(w, fl);
                    st.0 = st.0.min(x);
                    st.1 = st.1.max(x);
                    st.2 += x as u64;
                    let v = if kind == "s16" { (((x as i32) << (16 - fl.bits)) - 32_768) as i16 as u16 } else { x as u16 };
                    out.write_all(&v.to_le_bytes())?;
                }
            }
        }
    }
    let n = nframes * meta.cx * meta.cy;
    for mut out in outs {
        out.flush()?;
    }
    say!(
        "{}: {} frames {}x{} -> {} samples per channel ({:.3} s){}",
        dir.display(),
        nframes,
        meta.cx,
        meta.cy,
        n,
        meta.sample_rate_hz.map(|r| n as f64 / r).unwrap_or(0.0),
        meta.sample_rate_hz.map(|r| format!(", vhs-decode: -f {:.6}", r / 1e6)).unwrap_or_default()
    );
    for (fl, (lo, hi, sum)) in fields.iter().zip(stats) {
        say!("  {}.{kind}: codes {lo}..{hi}, mean {:.1}", fl.name, sum as f64 / n as f64);
    }
    if gaps > 0 {
        say!("  !! {gaps} frame(s) were skipped during capture: the files have discontinuities there");
    }
    Ok(())
}

fn read_gaps(dir: &Path) -> Vec<i64> {
    let Ok(s) = fs::read_to_string(dir.join("frames.csv")) else { return Vec::new() };
    let mut lines = s.lines();
    let Some(hdr) = lines.next() else { return Vec::new() };
    let Some(col) = hdr.split(',').position(|c| c == "gap_frames") else { return Vec::new() };
    lines.map(|l| l.split(',').nth(col).and_then(|v| v.parse().ok()).unwrap_or(0)).collect()
}
