//! One Magewell channel as a raw digitizer: open, arm (the capture sequence of docs/11 §3, leaving
//! the card in capture mode), live front-end changes, streaming, and restore.
//!
//! The card stays armed between streams, so a GUI can preview, record and change settings
//! without the 1-2 minute clamp-off settling that every re-arm costs (docs/13 §3). It is restored
//! by `restore()`, by dropping the `Device`, or (after a crash) by the next `open()`+`arm()`.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::ks::KsDevice;
use super::sdk::{self, Channel, CustomTiming, Instance, VideoSignalStatus};
use super::stream::{FrameSink, Geometry, Stream, StreamOptions};
use crate::audiolock::RfClock;
use crate::adv7842::{FrontEnd, Map, Recipe, CP_MOVED, IO_CP_ADDR, IO_EDID_ADDR};
use crate::alias::FakeSync;
use crate::board::{self, Board};
use crate::bus::{self, RegBus};
use crate::calib::{self, Levels};
use crate::config::{CalibUse, CaptureConfig};
use crate::fourcc::Format;
use crate::log::Level;
use crate::timing::{self, ClockPlan};
use crate::{say, warn};

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    /// SDK enumeration index.
    pub index: i32,
    /// Rotary-switch number.
    pub board: u8,
    pub channel: u8,
    pub serial: String,
    pub product: String,
    pub product_id: u16,
    /// The card model, when mwadc supports it (HDMI, DVI, AIO).
    pub model: Option<Board>,
    pub firmware: u32,
    pub driver: u32,
}

/// Which channel to open: by rotary switch (`board` + `channel`) or SDK index (`device`); with
/// neither, the only channel present.
#[derive(Clone, Debug, Default)]
pub struct Selector {
    pub board: Option<u8>,
    pub channel: u8,
    pub device: Option<i32>,
}

pub fn list(dll: Option<&std::path::Path>) -> Result<Vec<DeviceInfo>> {
    let _inst = sdk::instance(dll)?;
    let n = sdk::channel_count();
    (0..n).map(|i| sdk::channel_info_by_index(i).map(|ci| info_of(i, &ci))).collect()
}

fn info_of(index: i32, ci: &sdk::ChannelInfo) -> DeviceInfo {
    DeviceInfo {
        index,
        board: ci.board_index,
        channel: ci.channel_index,
        serial: sdk::cstr(&ci.board_serial).trim().to_string(),
        product: sdk::cstr(&ci.product_name),
        product_id: ci.product_id,
        model: Board::from_product(ci.product_id),
        firmware: ci.firmware_version,
        driver: ci.driver_version,
    }
}

fn select(sel: &Selector) -> Result<i32> {
    let n = sdk::channel_count();
    let infos: Vec<(i32, u8, u8)> =
        (0..n).filter_map(|i| sdk::channel_info_by_index(i).ok().map(|ci| (i, ci.board_index, ci.channel_index))).collect();
    let listing = || infos.iter().map(|(i, b, c)| format!("[{i}] board {b:X} channel {c}")).collect::<Vec<_>>().join(", ");
    if let Some(b) = sel.board {
        return infos
            .iter()
            .find(|&&(_, bb, cc)| bb == b && cc == sel.channel)
            .map(|&(i, ..)| i)
            .with_context(|| format!("no channel with board {b:X} channel {}; found: {}", sel.channel, listing()));
    }
    if let Some(d) = sel.device {
        return Ok(d);
    }
    match infos.len() {
        0 => bail!("no Magewell channels found"),
        1 => Ok(infos[0].0),
        _ => bail!("{} channels found ({}); choose one with --board <rotary switch> (or --device <index>)", infos.len(), listing()),
    }
}

/// What the card is set up for while armed.
#[derive(Clone, Debug, Serialize)]
pub struct Armed {
    pub cfg: CaptureConfig,
    pub plan: ClockPlan,
    pub recipe: Recipe,
    pub geometry: Geometry,
    /// The calibration levels that were applied (if any).
    pub calibration: Option<Levels>,
}

impl Armed {
    pub fn front_end(&self) -> FrontEnd {
        self.recipe.fe
    }
}

pub struct Device {
    // Field order is drop order: restore (in Drop) first, then the KS handle, the channel, the SDK.
    armed: Option<Armed>,
    /// Input source and scan state to put back on restore.
    saved: Option<(u32, bool)>,
    pub ks: KsDevice,
    ch: Arc<Channel>,
    info: DeviceInfo,
    /// Frame timestamps of this card's RF streams, for line-in audio locked to the RF clock.
    clock: Arc<RfClock>,
    _inst: Arc<Instance>,
}

impl Device {
    pub fn open(sel: &Selector, dll: Option<&std::path::Path>) -> Result<Device> {
        let inst = sdk::instance(dll)?;
        let ch = Channel::open(select(sel)?)?;
        let ks = KsDevice::open(&ch.path)?;
        let info = info_of(ch.index, &ch.info()?);
        say!("card: [{}] board {:X} channel {}, {} (product {:#x}), serial {}", info.index, info.board, info.channel, info.product, info.product_id, info.serial);
        Ok(Device { armed: None, saved: None, ks, ch: Arc::new(ch), info, clock: RfClock::new(), _inst: inst })
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn channel(&self) -> &Channel {
        &self.ch
    }

    pub fn armed(&self) -> Option<&Armed> {
        self.armed.as_ref()
    }

    /// The RF clock reference every stream of this card publishes to (`audiolock`).
    pub fn rf_clock(&self) -> Arc<RfClock> {
        self.clock.clone()
    }

    pub fn signal(&self) -> Result<VideoSignalStatus> {
        self.ch.signal()
    }

    /// True when an earlier (interrupted) run left the ADV7842 maps moved.
    pub fn maps_moved(&mut self) -> Result<bool> {
        Ok(self.ks.rd(Map::Io, IO_CP_ADDR)? != Map::Cp.addr() || self.ks.rd(Map::Io, IO_EDID_ADDR)? != Map::Edid.addr())
    }

    /// Set the card up for raw capture with `cfg` and leave it in capture mode. If it is already
    /// armed with the same clock, geometry and inputs, only the front end is updated (no re-lock).
    pub fn arm(&mut self, cfg: &CaptureConfig) -> Result<&Armed> {
        cfg.validate()?;
        let model = board::require(self.info.product_id, &self.info.product)?;
        let unwired = model.unwired(cfg.active_adc());
        if !unwired.is_empty() {
            let pins: Vec<String> = unwired.iter().map(|p| format!("Ain{p}")).collect();
            warn!("!! {} is not wired to a jack on the {} (its jacks: {}); capturing it anyway", pins.join(", "), model.name(), model.describe_jacks());
        }
        let plan = cfg.plan()?;
        let cal = match cfg.calibration {
            CalibUse::Off => None,
            CalibUse::Auto => calib::Store::load_default()
                .ok()
                .and_then(|s| s.levels_for(&self.info.serial, cfg.active_adc(), cfg.clamp_on(), cfg.afe_gain, cfg.input_offset)),
        };
        let fe = cfg.front_end(cal.as_ref());
        let recipe = Recipe { adc: cfg.active_adc(), pll_div: plan.pll_div, fr_ll: plan.fr_ll, lines: plan.lines, fe, extra: cfg.extra.clone() };
        if let Some(a) = &self.armed {
            let same = a.recipe.adc == recipe.adc
                && a.recipe.pll_div == recipe.pll_div
                && a.recipe.fr_ll == recipe.fr_ll
                && a.recipe.lines == recipe.lines
                && a.recipe.extra == recipe.extra
                && a.cfg.format == cfg.format;
            if same {
                self.set_front_end(fe)?;
                let a = self.armed.as_mut().unwrap();
                a.cfg = cfg.clone();
                a.calibration = cal;
                return Ok(self.armed.as_ref().unwrap());
            }
            self.restore()?;
            sleep(Duration::from_millis(1500));
        } else if self.maps_moved()? {
            say!("the card was left in capture mode by an interrupted run; resetting first");
            let input = self.ch.input_source()?;
            self.restore_to(input, None)?;
            sleep(Duration::from_millis(1500));
        }
        if let Some(c) = &cal {
            say!("calibration: {}", c.describe());
        }
        say!("clock: {}", plan.describe());
        say!("setup: {}", cfg.describe());
        self.saved = Some((self.ch.input_source()?, self.ch.input_scan()?));
        let geometry = Geometry::new(cfg.format, plan.pll_div as usize, plan.lines as usize, plan.sample_rate_hz);
        // From here on the card may be modified: mark it armed so a failure still restores it.
        self.armed = Some(Armed { cfg: cfg.clone(), plan: plan.clone(), recipe: recipe.clone(), geometry, calibration: cal });
        if let Err(e) = self.arm_sequence(model, cfg.format, &plan, &recipe) {
            warn!("!! arm failed: {e:#}");
            let _ = self.restore();
            return Err(e);
        }
        Ok(self.armed.as_ref().unwrap())
    }

    fn arm_sequence(&mut self, model: Board, fmt: Format, plan: &ClockPlan, recipe: &Recipe) -> Result<()> {
        let io = Map::Io.addr();
        let (w, h) = (plan.pll_div, plan.lines);
        let fake = FakeSync::new(plan.lines, plan.fake_hz())?;

        // 1. Component (VGA on the DVI card, which has no Component), selected the normal way: the
        //    lib sets the ADV7842 up for its CP path. Input colour = output colour, full range, so the
        //    FPGA colour matrix is identity.
        self.ch.set_input_scan(false)?;
        let color = if fmt == Format::Rgb10 { sdk::COLOR_RGB } else { sdk::COLOR_YUV601 };
        self.ch.set_input_color(color, sdk::QUANT_FULL)?;
        let carrier = model.carrier_input();
        self.ch.set_input_source(carrier).with_context(|| format!("selecting {} on the {}", sdk::input_name(carrier), model.name()))?;
        sleep(Duration::from_millis(1500));

        // 2. The raw recipe on the real CP, while it is still at 0x44.
        say!("applying the raw-ADC recipe:");
        bus::apply(&mut self.ks, &recipe.steps(), Level::Info)?;

        // 3. Move the CP away, put the EDID RAM on 0x44 and write the fake STDI readbacks there.
        self.ks.i2c_write(io, IO_CP_ADDR, &[CP_MOVED])?;
        self.ks.rd_raw(CP_MOVED, 0xFF).context("the CP map does not answer at its new address")?;
        self.ks.i2c_write(io, IO_EDID_ADDR, &[Map::Cp.addr()])?;
        let write_fake = |ks: &mut KsDevice| -> Result<()> {
            for (reg, val) in fake.regs()? {
                ks.i2c_write(Map::Cp.addr(), reg, &[val])?;
            }
            Ok(())
        };
        write_fake(&mut self.ks)?;
        let sig = wait_state(&self.ch, |st| st >= 1, 15.0, "the driver did not see the fake signal")?;
        say!("driver sees: {}", describe_signal(&sig));

        // 4. Custom timing covering the whole CP frame; the SDK only accepts it while a signal is
        //    present, so blank the fake signal once and let the driver re-detect it with the new timing.
        let d = fake.decoded()?;
        let ct = CustomTiming {
            sync_info: sdk::SyncInfo {
                sync_type: d.sync_type,
                hs_polarity: d.hs_pol as u8,
                vs_polarity: d.vs_pol as u8,
                interlaced: d.interlaced as u8,
                frame_duration: d.frame_duration,
                vsync_line_count: d.vs_lines as u16,
                frame_line_count: d.frame_lines,
            },
            timing_settings: sdk::TimingSettings { aspect_x: 4, aspect_y: 3, x: 0, y: 0, cx: w, cy: h, cx_total: w, clamp_pos: 0 },
        };
        let mut list: Vec<CustomTiming> = self.ch.custom_timings()?.into_iter().filter(|t| !is_ours(t)).collect();
        list.push(ct);
        self.ch.set_custom_timings(&list)?;
        let _ = self.ch.set_custom_timing(&ct);
        self.ks.i2c_write(Map::Cp.addr(), 0xB1, &[0x00])?;
        wait_state(&self.ch, |st| st == 0, 5.0, "the driver did not drop the fake signal")?;
        write_fake(&mut self.ks)?;
        let sig = wait_state(&self.ch, |st| st >= 2, 15.0, "no lock with the custom timing")?;
        say!("locked: {}", describe_signal(&sig));
        if (sig.x, sig.y, sig.cx, sig.cy) != (0, 0, w as i32, h as i32) {
            bail!("the driver's window is {}x{} at ({},{}), not {w}x{h} at (0,0): capture would not be gapless", { sig.cx }, { sig.cy }, { sig.x }, { sig.y });
        }
        let mut pll = [0u8; 2];
        self.ks.i2c_read(io, 0x16, &mut pll)?;
        let pll_div = (((pll[0] & 0x1F) as u16) << 8) | pll[1] as u16;
        if pll_div != plan.pll_div {
            warn!("!! PLL_DIV_RATIO reads {pll_div}, planned {}", plan.pll_div);
        }
        Ok(())
    }

    /// Change clamp, anti-alias filters, CP gains and centres while armed (also while streaming).
    pub fn set_front_end(&mut self, fe: FrontEnd) -> Result<()> {
        let Some(a) = self.armed.as_mut() else { bail!("the card is not armed") };
        if a.recipe.fe == fe {
            return Ok(());
        }
        bus::apply(&mut self.ks, &fe.steps(Map::CpMoved), Level::Debug)?;
        a.recipe.fe = fe;
        Ok(())
    }

    /// Start streaming frames to `sink` (the card must be armed). The returned `Stream` runs on its
    /// own threads; front-end changes stay possible meanwhile.
    /// The stream publishes its frame timestamps to `rf_clock()`, locked to the requested
    /// (nominal) rate: the crystal grid's −0.13 ppm at 40 MSPS is below the crystal's own error.
    pub fn stream(&self, sink: Box<dyn FrameSink>, mut opts: StreamOptions) -> Result<Stream> {
        let Some(a) = self.armed.as_ref() else { bail!("the card is not armed") };
        opts.rf_clock.get_or_insert_with(|| (self.clock.clone(), a.plan.target_hz));
        Stream::start(self.ch.clone(), a.geometry.clone(), sink, opts)
    }

    /// Put the card back to normal (no-op when not armed).
    pub fn restore(&mut self) -> Result<()> {
        if self.armed.take().is_none() {
            return Ok(());
        }
        let (input, scan) = match self.saved.take() {
            Some(s) => s,
            None => (self.ch.input_source()?, true),
        };
        self.restore_to(input, Some(scan))
    }

    /// Put the card back to normal: maps at their defaults (EDID first, so two maps never share an
    /// address), our custom timings removed, analog clamp on, input re-selected so the driver
    /// re-initialises the ADV7842.
    pub fn restore_to(&mut self, input: u32, scan: Option<bool>) -> Result<()> {
        self.armed = None;
        let io = Map::Io.addr();
        self.ks.i2c_write(io, IO_EDID_ADDR, &[Map::Edid.addr()])?;
        self.ks.i2c_write(io, IO_CP_ADDR, &[Map::Cp.addr()])?;
        let timings = self.ch.custom_timings()?;
        let keep: Vec<CustomTiming> = timings.iter().copied().filter(|t| !is_ours(t)).collect();
        if keep.len() != timings.len() {
            self.ch.set_custom_timings(&keep)?;
        }
        let io30 = self.ks.rd(Map::Io, 0x30)?;
        self.ks.i2c_write(io, 0x30, &[io30 | 0x80])?;
        // The input offset (AFE 0x11[1:0]) survives the driver's re-initialisation: back to the chip default.
        let afe11 = self.ks.rd(Map::Afe, 0x11)?;
        self.ks.i2c_write(Map::Afe.addr(), 0x11, &[(afe11 & !0x03) | 0x02])?;
        self.ch.set_input_source(input)?;
        if let Some(s) = scan {
            self.ch.set_input_scan(s)?;
        }
        say!("card restored: maps at their defaults, clamp on, input {} re-selected", sdk::input_name(input));
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.armed.is_some() {
            if let Err(e) = self.restore() {
                warn!("!! restoring the card failed: {e:#} (run `mwadc reset`)");
            }
        }
    }
}

fn wait_state(ch: &Channel, ok: impl Fn(i32) -> bool, secs: f64, what: &str) -> Result<VideoSignalStatus> {
    let t0 = Instant::now();
    loop {
        let s = ch.signal()?;
        if ok(s.state) {
            return Ok(s);
        }
        if t0.elapsed().as_secs_f64() > secs {
            bail!("{what} within {secs:.0}s: {}", describe_signal(&s));
        }
        sleep(Duration::from_millis(50));
    }
}

/// A custom timing written by `arm`: window at (0,0) covering the whole line.
fn is_ours(t: &CustomTiming) -> bool {
    let s = t.timing_settings;
    s.x == 0 && s.y == 0 && s.cx == s.cx_total
}

pub fn describe_signal(s: &VideoSignalStatus) -> String {
    let pclk = timing::pixel_clock_hz(s.cx_total, s.cy_total, s.frame_duration, s.interlaced != 0);
    format!(
        "{} active {}x{} at ({},{}) total {}x{}, frame {:.3} Hz, pixel clock {:.3} MHz",
        sdk::state_name(s.state),
        { s.cx },
        { s.cy },
        { s.x },
        { s.y },
        { s.cx_total },
        { s.cy_total },
        if s.frame_duration > 0 { 1e7 / s.frame_duration as f64 } else { 0.0 },
        pclk / 1e6
    )
}
