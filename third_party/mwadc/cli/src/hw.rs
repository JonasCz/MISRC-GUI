//! Hardware commands (Windows). `capture` arms the card, streams frames into the session
//! directory and always puts the card back afterwards; `reset` does the clean-up after an
//! interrupted run.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use mwadc_core::adv7842::{self, Map, CP_MOVED, IO_CP_ADDR};
use mwadc_core::bus::RegBus;
use mwadc_core::export::CaptureMeta;
use mwadc_core::util::{self, hexdump};
use mwadc_core::win::audio::{AudioClock, Recorder};
use mwadc_core::win::device::describe_signal;
use mwadc_core::win::ks::KsDevice;
use mwadc_core::win::sdk::{self, api};
use mwadc_core::win::{self, CalibrateOptions, Device, FrameRef, FrameSink, Geometry, Selector, StreamOptions, Summary};
use mwadc_core::{say, warn};

use crate::cli::*;
use crate::session::Session;
use crate::{stop_flag, stopped};

pub fn dispatch(o: &DeviceOpts, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::List => list(o),
        Cmd::Probe { no_dumps } => probe(o, no_dumps),
        Cmd::Status { watch } => status(o, watch),
        Cmd::I2c { op } => i2c(o, op),
        Cmd::Bar0 { off, count } => bar0(o, off, count),
        Cmd::Capture(a) => capture(o, a),
        Cmd::Calibrate(a) => calibrate(o, a),
        Cmd::Reset => reset(o),
        Cmd::Export(_) | Cmd::Options => unreachable!("handled in main"),
    }
}

fn selector(o: &DeviceOpts) -> Selector {
    Selector { board: o.board, channel: o.channel, device: o.device }
}

fn open(o: &DeviceOpts) -> Result<Device> {
    Device::open(&selector(o), o.dll.as_deref())
}

// ---------------------------------------------------------------- list / probe / status

fn list(o: &DeviceOpts) -> Result<()> {
    let devs = win::list(o.dll.as_deref())?;
    let (a, b, c) = sdk::version();
    say!("LibMWCapture {a}.{b}.{c} ({})", api().path.display());
    list_channels(&devs);
    Ok(())
}

fn list_channels(devs: &[win::DeviceInfo]) {
    say!("{} channel(s)", devs.len());
    for d in devs {
        say!(
            "  [{}] board {:X} (rotary switch) channel {}: {} (product {:#x}{}), serial {}, firmware {:#x}, driver {:#x}",
            d.index,
            d.board,
            d.channel,
            d.product,
            d.product_id,
            match d.model {
                Some(m) => format!(", revision {}", m.revision(d.firmware)),
                None => ", not supported".into(),
            },
            d.serial,
            d.firmware,
            d.driver
        );
    }
}

fn sdk_state(dev: &Device) -> Result<()> {
    let ch = dev.channel();
    let ci = dev.info();
    say!("product {} ({:#x}), serial {}", ci.product, ci.product_id, ci.serial);
    match ci.model {
        Some(m) => say!("  {} revision {}; jacks: {}", m.name(), m.revision(ci.firmware), m.describe_jacks()),
        None => say!("  note: not supported by mwadc (needs a Pro Capture HDMI, DVI or AIO)"),
    }
    let srcs: Vec<String> = ch.input_sources()?.into_iter().map(sdk::input_name).collect();
    say!("inputs offered: {}", srcs.join(", "));
    say!("current input {}, input scan {}", sdk::input_name(ch.input_source()?), ch.input_scan()?);
    let sig = ch.signal()?;
    if sig.state > 0 {
        say!("signal: {}", describe_signal(&sig));
    } else {
        say!("signal: none");
    }
    say!("framestore: {} slots", { ch.buffer_info()?.max_frames });
    say!("custom timings stored: {}", ch.custom_timing_count()?);
    if let Ok(t) = ch.temperature() {
        say!("core temperature: {:.1} C", t as f64 / 10.0);
    }
    Ok(())
}

fn i2c_sanity(ks: &mut KsDevice) -> Result<()> {
    let mut b = [0u8; 14];
    ks.i2c_read(Map::Io.addr(), 0xF1, &mut b).context("reading IO 0xF1.. (hidden I2C property)")?;
    let mut ok = 0;
    let mut rows = Vec::new();
    for m in Map::ALL {
        if let Some(r) = m.io_addr_reg() {
            let got = b[(r - 0xF1) as usize];
            ok += (got == m.addr()) as u32;
            rows.push(format!("{}={:#04x}{}", m.name(), got, if got == m.addr() { "" } else { "(!)" }));
        }
    }
    say!("ADV7842 map addresses: {} [{ok}/11 at the defaults]", rows.join(" "));
    if b[(IO_CP_ADDR - 0xF1) as usize] == CP_MOVED {
        say!("  the CP map is moved: an interrupted capture left the card in capture mode; run `mwadc reset`");
    }
    Ok(())
}

fn print_watch_regs(ks: &mut KsDevice) {
    say!("ADV7842 key registers:");
    for &(m, r, what) in adv7842::WATCH {
        match ks.rd(m, r) {
            Ok(v) => say!("  {:<4} {r:#04x} = {v:#04x}  {what}", m.name()),
            Err(e) => say!("  {:<4} {r:#04x} = ?  ({e:#})", m.name()),
        }
    }
}

fn read_map(ks: &mut KsDevice, dev8: u8) -> Result<Vec<u8>> {
    let mut v = vec![0u8; 256];
    for chunk in 0..4 {
        ks.i2c_read(dev8, (chunk * 64) as u8, &mut v[chunk * 64..chunk * 64 + 64])?;
    }
    Ok(v)
}

fn dump_maps(ks: &mut KsDevice, s: &Session, maps: &[Map]) {
    for &m in maps {
        match read_map(ks, m.addr()) {
            Ok(v) => {
                let _ = s.write_bytes(&format!("adv7842_{}.bin", m.name()), &v);
                let _ = s.write_bytes(&format!("adv7842_{}.txt", m.name()), hexdump(0, &v).as_bytes());
            }
            Err(e) => say!("  dump of {} failed: {e:#}", m.name()),
        }
    }
    say!("ADV7842 map dumps written to {}", s.dir.display());
}

fn bar0_survey(ks: &mut KsDevice) {
    say!("BAR0:");
    let named: &[(u32, &str)] = &[
        (0x08, "product id (0x102 AIO, 0x103 DVI, 0x104 HDMI)"),
        (0x18, "PCIe link status"),
        (0x28, "on-board memory size"),
        (0x3C, "reference clock frequency"),
        (0x2020, "capture control (0 idle, 0x3 writing)"),
    ];
    for &(off, what) in named {
        match ks.bar0_read(off) {
            Ok(v) => say!("  {off:#06x} = {v:#010x}  {what}"),
            Err(e) => say!("  {off:#06x} = ?  ({e:#})"),
        }
    }
}

fn probe(o: &DeviceOpts, no_dumps: bool) -> Result<()> {
    let s = Session::create(&o.out_dir, "probe")?;
    let mut dev = open(o)?;
    let (a, b, v) = sdk::version();
    say!("LibMWCapture {a}.{b}.{v} ({})", api().path.display());
    list_channels(&win::list(o.dll.as_deref())?);
    say!("");
    sdk_state(&dev)?;
    say!("");
    i2c_sanity(&mut dev.ks)?;
    print_watch_regs(&mut dev.ks);
    if !no_dumps {
        dump_maps(&mut dev.ks, &s, &[Map::Io, Map::Cp, Map::Afe, Map::Hdmi, Map::Sdp]);
    }
    say!("");
    bar0_survey(&mut dev.ks);
    Ok(())
}

fn status_line(dev: &mut Device) -> String {
    let st = dev.signal().map(|s| sdk::state_name(s.state)).unwrap_or("?");
    let ks = &mut dev.ks;
    let r = |ks: &mut KsDevice, d: u8, reg: u8| ks.rd_raw(d, reg).map(|v| format!("{v:02x}")).unwrap_or_else(|_| "??".into());
    let cp = ks.rd_raw(Map::Io.addr(), IO_CP_ADDR).unwrap_or(Map::Cp.addr());
    let ff = r(ks, cp, 0xFF);
    let io30 = r(ks, Map::Io.addr(), 0x30);
    let ctrl = ks.bar0_read(0x2020).map(|v| format!("{v:#x}")).unwrap_or_else(|_| "?".into());
    format!("state {st} | CP at {cp:#04x}, CP FF {ff} (free-run = bit 4) | IO 30 {io30} (clamp = bit 7) | capture ctrl {ctrl}")
}

fn status(o: &DeviceOpts, watch: f64) -> Result<()> {
    let mut dev = open(o)?;
    sdk_state(&dev)?;
    loop {
        say!("{}", status_line(&mut dev));
        if watch <= 0.0 || stopped() {
            return Ok(());
        }
        sleep(Duration::from_secs_f64(watch));
    }
}

// ---------------------------------------------------------------- raw register access

fn i2c(o: &DeviceOpts, op: I2cOp) -> Result<()> {
    let mut dev = open(o)?;
    let ks = &mut dev.ks;
    match op {
        I2cOp::Read { map, reg, count } => {
            let d = adv7842::parse_dev(&map)?;
            let mut b = vec![0u8; count];
            ks.i2c_read(d, reg, &mut b)?;
            say!("{} ({d:#04x})", adv7842::dev_name(d));
            say!("{}", hexdump(reg as u32, &b).trim_end());
        }
        I2cOp::Write { map, reg, vals } => {
            let d = adv7842::parse_dev(&map)?;
            let mut before = vec![0u8; vals.len()];
            ks.i2c_read(d, reg, &mut before)?;
            ks.i2c_write(d, reg, &vals)?;
            say!("{} {reg:#04x}: {before:02x?} -> {vals:02x?}", adv7842::dev_name(d));
        }
        I2cOp::Dump { maps } => {
            let s = Session::create(&o.out_dir, "i2c-dump")?;
            let maps: Vec<Map> = if maps.is_empty() {
                vec![Map::Io, Map::Cp, Map::Afe, Map::Hdmi]
            } else {
                maps.iter()
                    .map(|m| adv7842::parse_dev(m).and_then(|d| Map::from_addr(d).context("dump takes map names")))
                    .collect::<Result<_>>()?
            };
            dump_maps(ks, &s, &maps);
        }
    }
    Ok(())
}

fn bar0(o: &DeviceOpts, off: u32, count: u32) -> Result<()> {
    let mut dev = open(o)?;
    for i in 0..count {
        let a = off + 4 * i;
        say!("{a:#06x} = {:#010x}", dev.ks.bar0_read(a)?);
    }
    Ok(())
}

// ---------------------------------------------------------------- capture / calibrate / reset

fn reset(o: &DeviceOpts) -> Result<()> {
    let mut dev = open(o)?;
    let input = dev.channel().input_source()?;
    dev.restore_to(input, None)
}

/// Writes `frames.raw` (frames back to back, straight from the pool buffers: no copy),
/// `frames.csv` (per-frame timing) and, at the end, `capture.json`.
struct SessionSink {
    raw: Option<File>,
    csv: BufWriter<File>,
    prev_start: Option<i64>,
    frames: usize,
    last_tick: Instant,
    total: Option<u64>,
}

impl FrameSink for SessionSink {
    fn frame(&mut self, f: &FrameRef) -> Result<()> {
        if let Some(raw) = self.raw.as_mut() {
            raw.write_all(f.data).context("writing frames.raw")?;
        }
        let dt = self.prev_start.map(|p| f.start_100ns - p).unwrap_or(0);
        self.prev_start = Some(f.start_100ns);
        writeln!(
            self.csv,
            "{},{},{},{},{dt},{},{},{},{}",
            f.seq,
            f.slot,
            f.start_100ns,
            f.buffered_100ns,
            f.gap_frames,
            f.torn as u8,
            f.read_us,
            util::host_us()
        )?;
        self.frames += 1;
        if self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.last_tick = Instant::now();
            match self.total {
                Some(t) => say!("  {} / {t} frames", self.frames),
                None => say!("  {} frames", self.frames),
            }
        }
        Ok(())
    }

    fn finish(&mut self, _s: &Summary) -> Result<()> {
        self.csv.flush()?;
        if let Some(raw) = self.raw.as_mut() {
            raw.flush()?;
        }
        Ok(())
    }
}

fn capture(o: &DeviceOpts, a: CaptureArgs) -> Result<()> {
    a.setup.config(None)?; // option errors before a session is created
    let s = Session::create(&o.out_dir, "capture")?;
    let mut dev = open(o)?;
    let mut cfg = a.setup.config(dev.info().model)?;
    cfg.audio = a.audio;
    let armed = dev.arm(&cfg)?.clone();
    s.write_json("recipe.json", &armed)?;
    if a.settle > 0.0 {
        say!("settling {:.0} s ...", a.settle);
        let t0 = Instant::now();
        while t0.elapsed().as_secs_f64() < a.settle && !stopped() {
            sleep(Duration::from_millis(100));
        }
    }
    let geo: Geometry = armed.geometry.clone();
    let frames = a.frames.unwrap_or_else(|| (a.seconds * armed.plan.frame_rate_hz).ceil().max(1.0) as u64);
    say!(
        "capture: {} {}x{} stride {} ({} KiB/frame), {} frames",
        geo.format.name(),
        geo.cx,
        geo.cy,
        geo.stride,
        geo.frame_bytes / 1024,
        frames
    );
    let mut csv = BufWriter::new(File::create(s.path("frames.csv"))?);
    writeln!(csv, "index,slot,start_100ns,buffered_100ns,dt_100ns,gap_frames,torn,read_us,host_us")?;
    let sink = SessionSink {
        raw: if a.no_raw { None } else { Some(File::create(s.path("frames.raw"))?) },
        csv,
        prev_start: None,
        frames: 0,
        last_tick: Instant::now(),
        total: Some(frames),
    };
    let locked = a.audio_clock == AudioClockArg::Rf;
    let rec = if cfg.audio {
        let clock = if locked { AudioClock::Rf(dev.rf_clock()) } else { AudioClock::Card };
        Some(Recorder::start(dev.channel().index, &s.dir, clock, a.audio_native)?)
    } else {
        None
    };
    let opts = StreamOptions { frames: Some(frames), pool: a.pool, stop: stop_flag(), ..Default::default() };
    let r = dev.stream(Box::new(sink), opts).and_then(|st| st.wait());
    if let Some(rec) = rec {
        // The audio for the last RF frame arrives a few ms after it: let it in before stopping.
        if locked && !rec.wait_rf_end(Duration::from_secs(2)) {
            warn!("  audio: the line-in did not reach the end of the RF capture; audio.wav ends early");
        }
        match rec.stop() {
            Ok((x, lock)) => {
                say!(
                    "  audio: {} SDK frames at {} Hz ({:.2} s), {} missed, peak L/R {:#x}/{:#x}",
                    x.frames,
                    x.sample_rate,
                    x.frames as f64 * 192.0 / x.sample_rate as f64,
                    x.missed,
                    x.peak[0],
                    x.peak[1]
                );
                if let Some(l) = lock {
                    audio_lock_summary(&l, r.as_ref().ok(), &geo, x.sample_rate);
                }
            }
            Err(e) => warn!("  audio failed: {e:#}"),
        }
    }
    let rr = dev.restore();
    let sum = r?;
    rr?;

    say!(
        "  captured {} frames in {:.2}s ({:.1} fps, {:.1} MB/s); skipped frames {}, torn {}, max slots behind {}; device frame period {}",
        sum.captured,
        sum.seconds,
        sum.captured as f64 / sum.seconds,
        sum.captured as f64 * geo.frame_bytes as f64 / sum.seconds / 1e6,
        sum.gaps,
        sum.torn,
        sum.max_behind,
        if sum.period_100ns > 0 { format!("{:.3} ms", sum.period_100ns as f64 / 1e4) } else { "unknown (timestamps static)".into() }
    );
    let mut notes = vec![format!("clock {}", armed.plan.describe()), format!("setup {}", cfg.describe()), format!("levels {}", {
        let fe = armed.front_end();
        format!("gain {:?} centre {:?}", fe.gain, fe.centre)
    })];
    if sum.period_100ns > 0 {
        let implied = geo.samples_per_frame() as f64 / (sum.period_100ns as f64 * 1e-7);
        notes.push(format!(
            "device frame period {:.4} ms; cx*cy/period = {:.4} MSPS (equals the sample rate only if the window covers the whole CP frame)",
            sum.period_100ns as f64 / 1e4,
            implied / 1e6
        ));
    }
    s.write_json(
        "capture.json",
        &CaptureMeta {
            format: geo.format,
            cx: geo.cx,
            cy: geo.cy,
            stride: geo.stride,
            frame_bytes: geo.frame_bytes,
            frames: sum.captured as usize,
            sample_rate_hz: Some(geo.sample_rate_hz),
            mode: "capture".into(),
            notes,
        },
    )?;
    say!("session: {}  (mwadc export {} -> per-channel .s16 for vhs-decode)", s.dir.display(), s.dir.display());
    Ok(())
}

/// How well audio.wav followed the RF clock.
fn audio_lock_summary(l: &mwadc_core::win::LockReport, sum: Option<&Summary>, geo: &Geometry, rate: u32) {
    if l.samples == 0 {
        warn!("  audio.wav is empty: RF-locked audio needs the RF stream, which delivered no frames (--audio-clock card records without it)");
        return;
    }
    let mut line = format!(
        "  audio locked to the RF clock ({:.6} MSPS nominal): {} samples, count error at the end {:+.2} samples, max |error| {:.2} ({:.2} after 2 s), step {:+.2} ppm (min {:+.2}, max {:+.2})",
        l.rf_hz / 1e6,
        l.samples,
        l.error_last,
        l.error_max,
        l.error_max_settled,
        l.step_ppm_mean,
        l.step_ppm_min,
        l.step_ppm_max
    );
    if let Some(s) = sum {
        let rf = (s.captured + s.gaps) * geo.samples_per_frame() as u64;
        let want = mwadc_core::audiolock::out_count(rf, l.rf_hz, rate as f64);
        line += &format!("; RF {} samples x {}/{} = {} ({:+} written{})", rf, rate, l.rf_hz, want, l.samples as i64 - want as i64, if l.complete { "" } else { ", incomplete" });
    }
    say!("{line}");
}

fn calibrate(o: &DeviceOpts, a: CalibrateArgs) -> Result<()> {
    a.setup.config(None)?;
    let s = Session::create(&o.out_dir, "calibrate")?;
    let mut dev = open(o)?;
    let cfg = a.setup.config(dev.info().model)?;
    let opts = CalibrateOptions { seconds: a.seconds, save: !a.no_save, store_path: a.store.clone(), ..Default::default() };
    let mut last = Instant::now();
    let progress: win::calibrate::Progress = Box::new(move |frac, st| {
        if last.elapsed() >= Duration::from_secs(2) {
            last = Instant::now();
            say!("  {:3.0} %  t {:5.1} s  mean {:.1}/{:.1}/{:.1}", frac * 100.0, st.t, st.mean[0], st.mean[1], st.mean[2]);
        }
    });
    let r = win::calibrate(&mut dev, &cfg, &opts, Some(progress));
    let rr = dev.restore();
    let chans = r?;
    rr?;
    s.write_json("calibration.json", &chans)?;
    Ok(())
}
