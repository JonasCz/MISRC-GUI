//! C ABI of mwadc-core (`mwadc_ffi.dll`); the contract is `include/mwadc.h`.
//!
//! Every entry point catches panics and turns errors into -1/NULL plus a per-thread message
//! (`mwadc_last_error`).

#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};

use mwadc_core::adv7842::AA_FC_MHZ;
use mwadc_core::calib::{Levels, Store};
use mwadc_core::config::{CaptureConfig, KEYS};
use mwadc_core::fourcc::Format;
use mwadc_core::log::{self, Level};

pub const API_VERSION: u32 = 5;

/// `mwadc_audio_start` flag: deliver the line-in locked to the RF clock (API 4).
pub const AUDIO_LOCK_RF: u32 = 1;

// ---------------------------------------------------------------- C types (mirror mwadc.h)

#[repr(C)]
pub struct DeviceInfo {
    pub index: i32,
    pub board: u8,
    pub channel: u8,
    pub product_id: u16,
    pub serial: [c_char; 32],
    pub product: [c_char; 64],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Field {
    pub pin: u32,
    pub lsb: u32,
    pub bits: u32,
    pub step: u32,
    pub gain: u16,
    pub centre: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StreamInfo {
    pub format: u32,
    pub bytes_per_sample: u32,
    pub cx: u32,
    pub cy: u32,
    pub stride: u32,
    pub frame_bytes: u32,
    pub sample_rate_hz: f64,
    pub field: [Field; 3],
    pub clamp: i32,
    pub calibrated: i32,
    pub aa_enable: u32,
    pub aa_mhz: u32,
}

#[repr(C)]
pub struct Frame {
    pub data: *const u8,
    pub bytes: u32,
    pub gap_frames: u32,
    pub seq: u64,
    pub start_100ns: i64,
    pub torn: u32,
    pub read_us: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CalResult {
    pub adc: u32,
    pub pin: u32,
    pub clamp: u32,
    pub min_code: u32,
    pub max_code: u32,
    pub rest: f64,
    pub tau_s: f64,
    pub noise_rms: f64,
    pub centre: u16,
    pub gain: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub channels: u32,
    pub channel_valid: u32,
    pub source: u32,
}

#[repr(C)]
pub struct AudioBlock {
    pub samples: *const i32,
    pub frames: u32,
    pub missed: u32,
    pub seq: u64,
    pub timestamp_100ns: i64,
    pub sdk_frame: u32,
    pub buffered: u32,
    // API 4
    pub locked: u32,
    pub segment: u32,
    pub position: u64,
    pub rf_rate_hz: f64,
    pub step_ppm: f64,
    pub error: f64,
}

pub type LogFn = Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_char)>;
pub type FrameFn = Option<unsafe extern "C" fn(*mut c_void, *const Frame)>;
pub type AudioFn = Option<unsafe extern "C" fn(*mut c_void, *const AudioBlock)>;
pub type ProgressFn = Option<unsafe extern "C" fn(*mut c_void, f64, *const f64, *const f64)>;

/// `mwadc_config`
pub struct Config(CaptureConfig);

// ---------------------------------------------------------------- errors

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(e: &anyhow::Error) {
    let s = CString::new(format!("{e:#}").replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|l| *l.borrow_mut() = s);
}

/// Run `f`, mapping errors and panics to `fail`.
fn guard<T>(fail: T, f: impl FnOnce() -> Result<T>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            set_error(&e);
            fail
        }
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            set_error(&anyhow!("internal error (panic): {msg}"));
            fail
        }
    }
}

unsafe fn opt_str(p: *const c_char) -> Result<Option<String>> {
    if p.is_null() {
        return Ok(None);
    }
    Ok(Some(CStr::from_ptr(p).to_str().map_err(|_| anyhow!("string is not UTF-8"))?.to_string()))
}

unsafe fn req_str(p: *const c_char, what: &str) -> Result<String> {
    opt_str(p)?.ok_or_else(|| anyhow!("{what} is NULL"))
}

fn copy_cstr(dst: &mut [c_char], s: &str) {
    let n = s.len().min(dst.len() - 1);
    for (d, b) in dst.iter_mut().zip(s.as_bytes()[..n].iter()) {
        *d = *b as c_char;
    }
    dst[n] = 0;
}

// ---------------------------------------------------------------- version / log

#[no_mangle]
pub extern "C" fn mwadc_api_version() -> u32 {
    API_VERSION
}

#[no_mangle]
pub extern "C" fn mwadc_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn mwadc_last_error() -> *const c_char {
    LAST_ERROR.with(|l| l.borrow().as_ptr())
}

struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

#[no_mangle]
pub unsafe extern "C" fn mwadc_set_log(f: LogFn, user: *mut c_void, min_level: c_int) {
    match f {
        None => log::set_sink(None),
        Some(f) => {
            let u = SendPtr(user);
            log::set_sink(Some(Box::new(move |level: Level, s: &str| {
                if (level as c_int) < min_level {
                    return;
                }
                if let Ok(c) = CString::new(s.replace('\0', " ")) {
                    let u = &u;
                    unsafe { f(u.0, level as c_int, c.as_ptr()) };
                }
            })));
        }
    }
}

// ---------------------------------------------------------------- config

#[no_mangle]
pub extern "C" fn mwadc_config_new() -> *mut Config {
    Box::into_raw(Box::new(Config(CaptureConfig::default())))
}

#[no_mangle]
pub unsafe extern "C" fn mwadc_config_free(cfg: *mut Config) {
    if !cfg.is_null() {
        drop(Box::from_raw(cfg));
    }
}

#[no_mangle]
pub unsafe extern "C" fn mwadc_config_set(cfg: *mut Config, key: *const c_char, value: *const c_char) -> c_int {
    guard(-1, || {
        let c = cfg.as_mut().ok_or_else(|| anyhow!("config is NULL"))?;
        c.0.set(&req_str(key, "key")?, &req_str(value, "value")?)?;
        Ok(0)
    })
}

#[no_mangle]
pub extern "C" fn mwadc_config_keys() -> *const c_char {
    static KEYS_TEXT: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    KEYS_TEXT
        .get_or_init(|| CString::new(KEYS.iter().map(|(k, d)| format!("{k}\t{d}\n")).collect::<String>()).unwrap_or_default())
        .as_ptr()
}

fn fill_info(cfg: &CaptureConfig, levels: Option<&Levels>, clamp: bool, fe: Option<mwadc_core::adv7842::FrontEnd>) -> Result<StreamInfo> {
    cfg.validate()?;
    let plan = cfg.plan()?;
    let format = cfg.format;
    let bps = format.bytes_per_pixel();
    let cx = plan.pll_div as usize;
    let cy = plan.lines as usize;
    let stride = format.min_stride(cx);
    let fe = fe.unwrap_or_else(|| cfg.front_end(levels));
    let pins = cfg.active_adc();
    let mut info = StreamInfo {
        format: if format == Format::Y16 { 1 } else { 0 },
        bytes_per_sample: bps as u32,
        cx: cx as u32,
        cy: cy as u32,
        stride: stride as u32,
        frame_bytes: (stride * cy) as u32,
        sample_rate_hz: plan.sample_rate_hz,
        clamp: clamp as i32,
        calibrated: levels.is_some() as i32,
        aa_enable: fe.aa_enable.iter().enumerate().map(|(i, &e)| (e as u32) << i).sum(),
        aa_mhz: AA_FC_MHZ[fe.aa_bw as usize & 15] as u32,
        ..Default::default()
    };
    let fields = format.fields();
    for adc in 0..3 {
        let fl = fields.iter().find(|f| f.name == ["adc0", "adc1", "adc2"][adc]);
        if let (Some(fl), true) = (fl, pins[adc] != 0) {
            info.field[adc] = Field {
                pin: pins[adc] as u32,
                lsb: fl.lsb,
                bits: fl.bits,
                step: if adc == 0 { 1 } else { 2 },
                gain: fe.gain[adc],
                centre: fe.centre[adc],
            };
        }
    }
    Ok(info)
}

#[no_mangle]
pub unsafe extern "C" fn mwadc_config_describe(cfg: *const Config, info: *mut StreamInfo) -> c_int {
    guard(-1, || {
        let c = cfg.as_ref().ok_or_else(|| anyhow!("config is NULL"))?;
        let i = fill_info(&c.0, None, c.0.clamp_on(), None)?;
        if let Some(o) = info.as_mut() {
            *o = i;
        }
        Ok(0)
    })
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CalInfo {
    pub active: u32,
    pub matched: u32,
    pub updated: [c_char; 24],
    pub gain: [u16; 3],
    pub centre: [u16; 3],
}

#[no_mangle]
pub unsafe extern "C" fn mwadc_calibration_lookup(serial: *const c_char, cfg: *const Config, out: *mut CalInfo) -> c_int {
    guard(-1, || {
        let serial = req_str(serial, "serial")?;
        let c = &cfg.as_ref().ok_or_else(|| anyhow!("config is NULL"))?.0;
        let l = Store::load_default()?.lookup(serial.trim(), c.active_adc(), c.clamp_on(), c.afe_gain, c.input_offset);
        if let Some(o) = out.as_mut() {
            *o = CalInfo { active: l.active, matched: l.matched, ..Default::default() };
            copy_cstr(&mut o.updated, &l.updated);
            if let Some(lv) = l.levels {
                o.gain = lv.gain;
                o.centre = lv.centre;
            }
        }
        Ok(l.complete() as c_int)
    })
}

#[no_mangle]
pub unsafe extern "C" fn mwadc_config_max_rate(cfg: *const Config) -> f64 {
    guard(-1.0, || Ok(cfg.as_ref().ok_or_else(|| anyhow!("config is NULL"))?.0.max_rate_hz()))
}

#[no_mangle]
pub extern "C" fn mwadc_calibration_path() -> *const c_char {
    static P: Mutex<Option<CString>> = Mutex::new(None);
    let mut g = P.lock().unwrap_or_else(|e| e.into_inner());
    *g = CString::new(Store::default_path().to_string_lossy().into_owned()).ok();
    g.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null())
}

// ---------------------------------------------------------------- device (Windows)

#[cfg(windows)]
mod dev {
    use super::*;
    use mwadc_core::win::{self, calibrate::Progress, AudioSink, AudioStream, CalibrateOptions, Device, FrameRef, LockedSink, Selector, Stream, StreamOptions};

    pub struct Handle {
        inner: Mutex<Inner>,
    }

    struct Inner {
        stream: Option<Stream>,
        audio: Option<AudioStream>,
        dev: Device,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if let Some(s) = self.stream.take() {
                let _ = s.stop();
            }
            if let Some(a) = self.audio.take() {
                let _ = a.stop();
            }
            // Device::drop restores the card.
        }
    }

    fn handle<'a>(d: *mut Handle) -> Result<&'a Handle> {
        unsafe { d.as_ref() }.ok_or_else(|| anyhow!("device is NULL"))
    }

    fn lock(h: &Handle) -> std::sync::MutexGuard<'_, Inner> {
        h.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_list(sdk_dll: *const c_char, out: *mut DeviceInfo, max: c_int) -> c_int {
        guard(-1, || {
            let dll = opt_str(sdk_dll)?.map(PathBuf::from);
            let devs = win::list(dll.as_deref())?;
            if !out.is_null() {
                for (i, d) in devs.iter().take(max.max(0) as usize).enumerate() {
                    let o = &mut *out.add(i);
                    o.index = d.index;
                    o.board = d.board;
                    o.channel = d.channel;
                    o.product_id = d.product_id;
                    copy_cstr(&mut o.serial, &d.serial);
                    copy_cstr(&mut o.product, &d.product);
                }
            }
            Ok(devs.len() as c_int)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_open(sdk_dll: *const c_char, board: c_int, sdk_index: c_int) -> *mut Handle {
        guard(std::ptr::null_mut(), || {
            let dll = opt_str(sdk_dll)?.map(PathBuf::from);
            let sel = Selector {
                board: (board >= 0).then_some(board as u8),
                channel: 0,
                device: (board < 0 && sdk_index >= 0).then_some(sdk_index),
            };
            let dev = Device::open(&sel, dll.as_deref())?;
            Ok(Box::into_raw(Box::new(Handle { inner: Mutex::new(Inner { stream: None, audio: None, dev }) })))
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_close(d: *mut Handle) {
        if !d.is_null() {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(d))));
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_info(d: *mut Handle, out: *mut DeviceInfo) -> c_int {
        guard(-1, || {
            let g = lock(handle(d)?);
            let i = g.dev.info();
            let o = out.as_mut().ok_or_else(|| anyhow!("out is NULL"))?;
            o.index = i.index;
            o.board = i.board;
            o.channel = i.channel;
            o.product_id = i.product_id;
            copy_cstr(&mut o.serial, &i.serial);
            copy_cstr(&mut o.product, &i.product);
            Ok(0)
        })
    }

    fn armed_info(g: &Inner) -> Result<StreamInfo> {
        let a = g.dev.armed().ok_or_else(|| anyhow!("not armed"))?;
        fill_info(&a.cfg, a.calibration.as_ref(), a.recipe.fe.clamp, Some(a.recipe.fe))
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_arm(d: *mut Handle, cfg: *const Config, info: *mut StreamInfo) -> c_int {
        guard(-1, || {
            let h = handle(d)?;
            let c = &cfg.as_ref().ok_or_else(|| anyhow!("config is NULL"))?.0;
            let mut g = lock(h);
            if g.stream.as_ref().is_some_and(|s| s.is_running()) {
                // Only a front-end change is possible while streaming.
                let a = g.dev.armed().ok_or_else(|| anyhow!("not armed"))?;
                let plan = c.plan()?;
                if a.recipe.adc != c.active_adc() || a.plan.pll_div != plan.pll_div || a.plan.fr_ll != plan.fr_ll || a.plan.lines != plan.lines || a.cfg.format != c.format || a.recipe.extra != c.extra {
                    bail!("this change needs the card re-locked: stop the stream first");
                }
            }
            g.dev.arm(c)?;
            if let Some(o) = info.as_mut() {
                *o = armed_info(&g)?;
            }
            Ok(0)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_is_armed(d: *mut Handle) -> c_int {
        guard(-1, || Ok(lock(handle(d)?).dev.armed().is_some() as c_int))
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_start(d: *mut Handle, pool: u32, f: FrameFn, user: *mut c_void) -> c_int {
        guard(-1, || {
            let h = handle(d)?;
            let f = f.ok_or_else(|| anyhow!("frame callback is NULL"))?;
            let mut g = lock(h);
            if let Some(s) = g.stream.take() {
                if s.is_running() {
                    g.stream = Some(s);
                    bail!("already streaming");
                }
                let _ = s.wait();
            }
            let u = SendPtr(user);
            let sink = move |fr: &FrameRef| -> Result<()> {
                let c = Frame {
                    data: fr.data.as_ptr(),
                    bytes: fr.data.len() as u32,
                    gap_frames: fr.gap_frames,
                    seq: fr.seq,
                    start_100ns: fr.start_100ns,
                    torn: fr.torn as u32,
                    read_us: fr.read_us,
                };
                let u = &u;
                unsafe { f(u.0, &c) };
                Ok(())
            };
            let opts = StreamOptions { pool: if pool == 0 { 6 } else { pool as usize }, ..Default::default() };
            let s = g.dev.stream(Box::new(sink), opts)?;
            g.stream = Some(s);
            Ok(0)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_is_streaming(d: *mut Handle) -> c_int {
        guard(-1, || Ok(lock(handle(d)?).stream.as_ref().is_some_and(|s| s.is_running()) as c_int))
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_stop(d: *mut Handle) -> c_int {
        guard(-1, || {
            let s = lock(handle(d)?).stream.take();
            if let Some(s) = s {
                s.stop()?;
            }
            Ok(0)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_audio_start(d: *mut Handle, flags: u32, f: AudioFn, user: *mut c_void, info: *mut AudioInfo) -> c_int {
        guard(-1, || {
            let h = handle(d)?;
            let f = f.ok_or_else(|| anyhow!("audio callback is NULL"))?;
            let mut g = lock(h);
            if let Some(a) = g.audio.take() {
                if a.is_running() {
                    g.audio = Some(a);
                    bail!("audio already running");
                }
                let _ = a.stop();
            }
            let u = SendPtr(user);
            let sink = move |b: &win::AudioBlock| -> Result<()> {
                let l = b.lock;
                let c = AudioBlock {
                    samples: b.samples.as_ptr(),
                    frames: (b.samples.len() / 2) as u32,
                    missed: b.missed,
                    seq: b.seq,
                    timestamp_100ns: b.timestamp_100ns,
                    sdk_frame: b.sdk_frame,
                    buffered: b.buffered,
                    locked: l.is_some() as u32,
                    segment: l.map(|l| l.first as u32 | (l.last as u32) << 1).unwrap_or(0),
                    position: l.map(|l| l.position).unwrap_or(0),
                    rf_rate_hz: l.map(|l| l.rf_hz).unwrap_or(0.0),
                    step_ppm: l.map(|l| (l.step - 1.0) * 1e6).unwrap_or(0.0),
                    error: l.map(|l| l.error).unwrap_or(0.0),
                };
                let u = &u;
                unsafe { f(u.0, &c) };
                Ok(())
            };
            // Locked: the device's streams publish their frame timestamps to its RF clock.
            let sink: Box<dyn AudioSink> = if flags & AUDIO_LOCK_RF != 0 { Box::new(LockedSink::new(g.dev.rf_clock(), Box::new(sink))) } else { Box::new(sink) };
            let a = AudioStream::start(g.dev.channel().index, sink)?;
            if let Some(o) = info.as_mut() {
                let fm = a.format();
                *o = AudioInfo { sample_rate: fm.sample_rate, bits_per_sample: fm.bits_per_sample, channels: 2, channel_valid: fm.channel_valid as u32, source: fm.source };
            }
            g.audio = Some(a);
            Ok(0)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_audio_is_running(d: *mut Handle) -> c_int {
        guard(-1, || Ok(lock(handle(d)?).audio.as_ref().is_some_and(|a| a.is_running()) as c_int))
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_audio_stop(d: *mut Handle) -> c_int {
        guard(-1, || {
            let a = lock(handle(d)?).audio.take();
            if let Some(a) = a {
                a.stop()?;
            }
            Ok(0)
        })
    }

    #[no_mangle]
    pub unsafe extern "C" fn mwadc_restore(d: *mut Handle) -> c_int {
        guard(-1, || {
            let mut g = lock(handle(d)?);
            if let Some(s) = g.stream.take() {
                let _ = s.stop();
            }
            g.dev.restore()?;
            Ok(0)
        })
    }

    #[no_mangle]
    #[allow(clippy::too_many_arguments)]
    pub unsafe extern "C" fn mwadc_calibrate(
        d: *mut Handle,
        cfg: *const Config,
        seconds: f64,
        save: c_int,
        progress: ProgressFn,
        user: *mut c_void,
        out: *mut CalResult,
        max: c_int,
    ) -> c_int {
        guard(-1, || {
            let h = handle(d)?;
            let c = &cfg.as_ref().ok_or_else(|| anyhow!("config is NULL"))?.0;
            let mut g = lock(h);
            if g.stream.as_ref().is_some_and(|s| s.is_running()) {
                bail!("stop the stream before calibrating");
            }
            g.stream = None;
            let opts = CalibrateOptions { seconds: if seconds > 0.0 { seconds } else { 40.0 }, save: save != 0, ..Default::default() };
            let prog: Option<Progress> = progress.map(|p| {
                let u = SendPtr(user);
                Box::new(move |frac: f64, st: &mwadc_core::calib::FrameStats| {
                    let u = &u;
                    unsafe { p(u.0, frac, st.mean.as_ptr(), st.rms.as_ptr()) }
                }) as Progress
            });
            let res = win::calibrate(&mut g.dev, c, &opts, prog)?;
            if !out.is_null() {
                for (i, r) in res.iter().take(max.max(0) as usize).enumerate() {
                    *out.add(i) = CalResult {
                        adc: r.adc as u32,
                        pin: r.pin as u32,
                        clamp: r.clamp as u32,
                        min_code: r.min,
                        max_code: r.max,
                        rest: r.settling.rest,
                        tau_s: r.settling.tau,
                        noise_rms: r.noise_rms,
                        centre: r.centre,
                        gain: r.gain,
                    };
                }
            }
            Ok(res.len() as c_int)
        })
    }
}

#[cfg(not(windows))]
mod dev {
    //! Hardware access exists only on Windows so far; these report that.
    use super::*;

    pub struct Handle;

    fn unsupported<T>(fail: T) -> T {
        set_error(&anyhow!("mwadc hardware access is only implemented on Windows"));
        fail
    }

    #[no_mangle]
    pub extern "C" fn mwadc_list(_: *const c_char, _: *mut DeviceInfo, _: c_int) -> c_int {
        unsupported(-1)
    }
    #[no_mangle]
    pub extern "C" fn mwadc_open(_: *const c_char, _: c_int, _: c_int) -> *mut Handle {
        unsupported(std::ptr::null_mut())
    }
    #[no_mangle]
    pub extern "C" fn mwadc_close(_: *mut Handle) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_describe() {
        unsafe {
            let c = mwadc_config_new();
            assert_eq!(mwadc_config_set(c, c"adc1".as_ptr(), c"off".as_ptr()), 0);
            assert_eq!(mwadc_config_set(c, c"lines".as_ptr(), c"512".as_ptr()), 0);
            assert_eq!(mwadc_config_set(c, c"bogus".as_ptr(), c"1".as_ptr()), -1);
            let e = CStr::from_ptr(mwadc_last_error()).to_string_lossy().into_owned();
            assert!(e.contains("unknown option"), "{e}");
            let mut i = StreamInfo::default();
            assert_eq!(mwadc_config_describe(c, &mut i), 0);
            assert_eq!(i.cy, 512);
            assert_eq!(i.field[0].lsb, 10);
            assert_eq!(i.field[1].pin, 0);
            assert_eq!(i.field[2].lsb, 20);
            assert_eq!(i.field[2].step, 2);
            assert!((i.sample_rate_hz - 40e6).abs() < 40e6 * 0.005, "{}", i.sample_rate_hz);
            mwadc_config_free(c);
        }
    }

    #[test]
    fn layout_matches_header() {
        // Sizes the C header implies (x86_64): keep in sync with mwadc.h.
        assert_eq!(std::mem::size_of::<DeviceInfo>(), 4 + 4 + 32 + 64);
        assert_eq!(std::mem::size_of::<Field>(), 20);
        assert_eq!(std::mem::size_of::<StreamInfo>(), 6 * 4 + 8 + 3 * 20 + 4 * 4 + 4 /* tail padding */);
        assert_eq!(std::mem::size_of::<Frame>(), 8 + 4 + 4 + 8 + 8 + 4 + 4);
        assert_eq!(std::mem::size_of::<CalResult>(), 5 * 4 + 4 + 3 * 8 + 2 + 2 + 4);
        assert_eq!(std::mem::size_of::<AudioInfo>(), 5 * 4);
        assert_eq!(std::mem::size_of::<AudioBlock>(), 8 + 4 + 4 + 8 + 8 + 4 + 4 + 4 + 4 + 8 + 8 + 8 + 8);
    }
}
