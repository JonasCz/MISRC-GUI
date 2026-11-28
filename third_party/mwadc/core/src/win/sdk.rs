//! LibMWCapture.dll (Magewell Capture SDK 3.3.1, Windows) loaded at run time.
//!
//! Structures mirror `MWCaptureExtension.h` / `MWCaptureDef.h`, which are `#pragma pack(1)`;
//! enums are 4-byte ints, BOOLEAN is one byte, BOOL four. Declarations have no calling
//! convention, i.e. cdecl (the only x64 convention anyway).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{anyhow, bail, Context, Result};

pub type HChannel = *mut c_void;
pub type Handle = *mut c_void;

pub const MW_SUCCEEDED: i32 = 0;

pub const NOTIFY_VIDEO_INPUT_SOURCE_CHANGE: u64 = 0x0004;
pub const NOTIFY_INPUT_SPECIFIC_CHANGE: u64 = 0x0010;
pub const NOTIFY_VIDEO_SIGNAL_CHANGE: u64 = 0x0020;
pub const NOTIFY_VIDEO_FRAME_BUFFERED: u64 = 0x0400;
pub const NOTIFY_AUDIO_SIGNAL_CHANGE: u64 = 0x0040;
pub const NOTIFY_AUDIO_FRAME_BUFFERED: u64 = 0x1000;
/// MWCAP_AUDIO_SAMPLES_PER_FRAME and MWCAP_AUDIO_MAX_NUM_CHANNELS; per sample: L0 L1 L2 L3 R0 R1 R2 R3.
pub const AUDIO_SAMPLES_PER_FRAME: usize = 192;
pub const AUDIO_CHANNELS: usize = 8;

pub const COLOR_RGB: i32 = 1;
pub const COLOR_YUV601: i32 = 2;
pub const QUANT_FULL: i32 = 1;
pub const SAT_UNKNOWN: i32 = 0;

pub fn input_name(src: u32) -> String {
    let t = match src >> 8 {
        0x01 => "HDMI",
        0x02 => "VGA",
        0x04 => "SDI",
        0x08 => "Component",
        0x10 => "CVBS",
        0x20 => "Y/C",
        0 => "none",
        _ => "?",
    };
    format!("{t} ({src:#x})")
}

pub fn state_name(s: i32) -> &'static str {
    match s {
        0 => "NONE",
        1 => "UNSUPPORTED",
        2 => "LOCKING",
        3 => "LOCKED",
        _ => "?",
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ChannelInfo {
    pub family_id: u16,
    pub product_id: u16,
    pub hardware_version: u8,
    pub firmware_id: u8,
    pub firmware_version: u32,
    pub driver_version: u32,
    pub family_name: [u8; 64],
    pub product_name: [u8; 64],
    pub firmware_name: [u8; 64],
    pub board_serial: [u8; 16],
    pub board_index: u8,
    pub channel_index: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct VideoSignalStatus {
    pub state: i32,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub cx_total: i32,
    pub cy_total: i32,
    pub interlaced: u8,
    pub frame_duration: u32,
    pub aspect_x: i32,
    pub aspect_y: i32,
    pub segmented: u8,
    pub frame_type: i32,
    pub color_format: i32,
    pub quant_range: i32,
    pub sat_range: i32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SyncInfo {
    pub sync_type: u8,
    pub hs_polarity: u8,
    pub vs_polarity: u8,
    pub interlaced: u8,
    pub frame_duration: u32,
    pub vsync_line_count: u16,
    pub frame_line_count: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct TimingSettings {
    pub aspect_x: u16,
    pub aspect_y: u16,
    pub x: u16,
    pub y: u16,
    pub cx: u16,
    pub cy: u16,
    pub cx_total: u16,
    pub clamp_pos: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CustomTiming {
    pub sync_info: SyncInfo,
    pub timing_settings: TimingSettings,
}

/// MWCAP_AUDIO_SIGNAL_STATUS (IEC60958 channel status = 24 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioSignalStatus {
    pub channel_valid: u16,
    pub lpcm: u8,
    pub bits_per_sample: u8,
    pub sample_rate: u32,
    pub channel_status_valid: u8,
    pub channel_status: [u8; 24],
}

/// MWCAP_AUDIO_CAPTURE_FRAME: 32-bit samples, MSB-aligned.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct AudioFrame {
    pub frame_count: u32,
    pub frame: u32,
    pub sync_code: u32,
    pub flags: u32,
    pub timestamp: i64,
    pub samples: [u32; AUDIO_SAMPLES_PER_FRAME * AUDIO_CHANNELS],
}

/// MWCAP_VIDEO_CUSTOM_TIMING_ARRAY (MWUSBCaptureExtension.h, pack 1): the driver's stored list.
#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CustomTimingArray {
    pub count: u8,
    pub timings: [CustomTiming; 8],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct BufferInfo {
    pub max_frames: u32,
    pub newest_buffering: u8,
    pub buffering_field_index: u8,
    pub newest_buffered: u8,
    pub buffered_field_index: u8,
    pub newest_buffered_full_frame: u8,
    pub buffered_full_frames: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct FrameInfo {
    pub state: i32,
    pub interlaced: u8,
    pub segmented: u8,
    pub top_field_first: u8,
    pub top_field_inverted: u8,
    pub cx: i32,
    pub cy: i32,
    pub aspect_x: i32,
    pub aspect_y: i32,
    pub field_start_times: [i64; 2],
    pub field_buffered_times: [i64; 2],
    pub smpte_timecodes: [u32; 2],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CaptureStatus {
    pub context: u64,
    pub physical_address: u8,
    pub frame: u64,
    pub i_frame: i32,
    pub frame_completed: u8,
    pub cy_completed: u16,
    pub cy_completed_prev: u16,
}

const _: () = {
    assert!(std::mem::size_of::<ChannelInfo>() == 224);
    assert!(std::mem::size_of::<CustomTiming>() == 27);
    assert!(std::mem::size_of::<BufferInfo>() == 13);
    assert!(std::mem::size_of::<FrameInfo>() == 64);
    assert!(std::mem::size_of::<CaptureStatus>() == 26);
    assert!(std::mem::size_of::<AudioSignalStatus>() == 33);
    assert!(std::mem::size_of::<AudioFrame>() == 24 + 192 * 8 * 4);
};

macro_rules! sdk_api {
    ($( fn $name:ident ( $($arg:ty),* ) -> $ret:ty ; )*) => {
        #[allow(non_snake_case, dead_code)]
        pub struct Api {
            pub path: PathBuf,
            _lib: libloading::Library,
            $( pub $name: unsafe extern "C" fn($($arg),*) -> $ret, )*
        }
        impl Api {
            #[allow(non_snake_case)]
            unsafe fn bind(lib: libloading::Library, path: PathBuf) -> Result<Api> {
                $( let $name = *lib
                    .get::<unsafe extern "C" fn($($arg),*) -> $ret>(concat!(stringify!($name), "\0").as_bytes())
                    .with_context(|| format!("{} does not export {}", path.display(), stringify!($name)))?; )*
                Ok(Api { path, _lib: lib, $($name),* })
            }
        }
    };
}

sdk_api! {
    fn MWGetVersion(*mut u8, *mut u8, *mut u16) -> i32;
    fn MWCaptureInitInstance() -> i32;
    fn MWCaptureExitInstance() -> ();
    fn MWRefreshDevice() -> i32;
    fn MWGetChannelCount() -> i32;
    fn MWGetChannelInfoByIndex(i32, *mut ChannelInfo) -> i32;
    fn MWGetDevicePath(i32, *mut u16) -> i32;
    fn MWOpenChannelByPath(*const u16) -> HChannel;
    fn MWCloseChannel(HChannel) -> ();
    fn MWGetChannelInfo(HChannel, *mut ChannelInfo) -> i32;
    fn MWGetVideoInputSourceArray(HChannel, *mut u32, *mut u32) -> i32;
    fn MWGetInputSourceScan(HChannel, *mut u8) -> i32;
    fn MWSetInputSourceScan(HChannel, u8) -> i32;
    fn MWGetVideoInputSource(HChannel, *mut u32) -> i32;
    fn MWSetVideoInputSource(HChannel, u32) -> i32;
    fn MWGetVideoSignalStatus(HChannel, *mut VideoSignalStatus) -> i32;
    fn MWSetVideoInputColorFormat(HChannel, i32) -> i32;
    fn MWSetVideoInputQuantizationRange(HChannel, i32) -> i32;
    fn MWSetCustomVideoTiming(HChannel, *mut CustomTiming) -> i32;
    fn MWGetCustomVideoTimingsCount(HChannel, *mut u32) -> i32;
    fn MWGetCustomVideoTimings(HChannel, *mut CustomTimingArray) -> i32;
    fn MWSetCustomVideoTimings(HChannel, *mut CustomTimingArray) -> i32;
    fn MWGetTemperature(HChannel, *mut u32) -> i32;
    fn MWGetAudioInputSource(HChannel, *mut u32) -> i32;
    fn MWGetAudioSignalStatus(HChannel, *mut AudioSignalStatus) -> i32;
    fn MWStartAudioCapture(HChannel) -> i32;
    fn MWStopAudioCapture(HChannel) -> i32;
    fn MWCaptureAudioFrame(HChannel, *mut AudioFrame) -> i32;
    fn MWRegisterNotify(HChannel, Handle, u64) -> u64;
    fn MWUnregisterNotify(HChannel, u64) -> i32;
    fn MWGetNotifyStatus(HChannel, u64, *mut u64) -> i32;
    fn MWStartVideoCapture(HChannel, Handle) -> i32;
    fn MWStopVideoCapture(HChannel) -> i32;
    fn MWPinVideoBuffer(HChannel, *mut u8, u32) -> i32;
    fn MWUnpinVideoBuffer(HChannel, *mut u8) -> i32;
    fn MWGetVideoBufferInfo(HChannel, *mut BufferInfo) -> i32;
    fn MWGetVideoFrameInfo(HChannel, u8, *mut FrameInfo) -> i32;
    fn MWGetVideoCaptureStatus(HChannel, *mut CaptureStatus) -> i32;
    fn MWCaptureVideoFrameToVirtualAddressEx(
        HChannel, i32, *mut u8, u32, u32, u8, u64, u32, i32, i32,
        u32, i32, u64, *const c_void, i32,
        i16, i16, i16, i16,
        i32, i32, *const c_void, *const c_void, i32, i32,
        i32, i32, i32
    ) -> i32;
}

static API: OnceLock<Api> = OnceLock::new();

/// Load the SDK DLL: explicit path, next to mwadc.exe, then the DLL search path. The release
/// `LibMWCapture.dll` comes with the Magewell runtime (`MWCaptureRT.exe`). The SDK installer's
/// debug `LibMWCaptured.dll` is deliberately not used.
pub fn load(explicit: Option<&Path>) -> Result<&'static Api> {
    if let Some(a) = API.get() {
        return Ok(a);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p.to_path_buf());
    } else {
        if let Ok(p) = std::env::var("MWADC_SDK_DLL") {
            candidates.push(PathBuf::from(p));
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            candidates.push(dir.join("LibMWCapture.dll"));
        }
        candidates.push(PathBuf::from("LibMWCapture.dll"));
    }
    let mut errs = Vec::new();
    for c in &candidates {
        match unsafe { libloading::Library::new(c) } {
            Ok(lib) => {
                let api = unsafe { Api::bind(lib, c.clone())? };
                return Ok(API.get_or_init(|| api));
            }
            Err(e) => errs.push(format!("  {}: {e}", c.display())),
        }
    }
    bail!(
        "The Magewell runtime (LibMWCapture.dll) needed to use the card is not installed.\n\
         \n\
         Download and install MWCaptureRT.exe from:\n\
         https://github.com/jonasCz/mwadc/releases\n\
         \n\
         (Or point mwadc at an existing copy with --dll <path> or the MWADC_SDK_DLL environment variable.)\n\
         \n\
         Locations tried:\n{}",
        errs.join("\n")
    )
}

pub fn api() -> &'static Api {
    API.get().expect("SDK not loaded")
}

pub fn check(r: i32, what: &str) -> Result<()> {
    if r == MW_SUCCEEDED {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (MW_RESULT {r})"))
    }
}

pub fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// `MWCaptureInitInstance` / `MWCaptureExitInstance` pair. Use `instance()` to share one per process.
pub struct Instance;

static SHARED: OnceLock<Mutex<Weak<Instance>>> = OnceLock::new();

/// The process-wide SDK instance (initialised on first use, released when the last user drops it).
pub fn instance(dll: Option<&Path>) -> Result<Arc<Instance>> {
    let m = SHARED.get_or_init(|| Mutex::new(Weak::new()));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = g.upgrade() {
        return Ok(i);
    }
    let i = Arc::new(Instance::init(dll)?);
    *g = Arc::downgrade(&i);
    Ok(i)
}

impl Instance {
    pub fn init(dll: Option<&Path>) -> Result<Instance> {
        let a = load(dll)?;
        if unsafe { (a.MWCaptureInitInstance)() } == 0 {
            bail!("MWCaptureInitInstance failed");
        }
        Ok(Instance)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { (api().MWCaptureExitInstance)() }
    }
}

pub fn version() -> (u8, u8, u16) {
    let (mut a, mut b, mut c) = (0, 0, 0);
    unsafe { (api().MWGetVersion)(&mut a, &mut b, &mut c) };
    (a, b, c)
}

pub fn channel_count() -> i32 {
    unsafe { (api().MWGetChannelCount)() }
}

pub fn channel_info_by_index(i: i32) -> Result<ChannelInfo> {
    let mut ci: ChannelInfo = unsafe { std::mem::zeroed() };
    check(unsafe { (api().MWGetChannelInfoByIndex)(i, &mut ci) }, "MWGetChannelInfoByIndex")?;
    Ok(ci)
}

/// Device interface path; also the file the KS properties are sent to.
pub fn device_path(i: i32) -> Result<Vec<u16>> {
    let mut buf = vec![0u16; 1024];
    check(unsafe { (api().MWGetDevicePath)(i, buf.as_mut_ptr()) }, "MWGetDevicePath")?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf.truncate(n + 1);
    Ok(buf)
}

pub fn wide_to_string(w: &[u16]) -> String {
    String::from_utf16_lossy(w.split(|&c| c == 0).next().unwrap_or(&[]))
}

pub struct Channel {
    pub h: HChannel,
    pub index: i32,
    pub path: Vec<u16>,
}

// The SDK's channel handle may be used from several threads (the lib serialises internally;
// mwadc only issues capture calls from its DMA thread and status reads elsewhere).
unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe { (api().MWCloseChannel)(self.h) }
    }
}

macro_rules! get {
    ($self:ident, $f:ident, $t:ty) => {{
        let mut v: $t = Default::default();
        check(unsafe { (api().$f)($self.h, &mut v) }, stringify!($f))?;
        Ok::<$t, anyhow::Error>(v)
    }};
}


impl Channel {
    pub fn open(index: i32) -> Result<Channel> {
        let n = channel_count();
        if index < 0 || index >= n {
            bail!("channel {index} does not exist ({n} channel(s) found)");
        }
        let path = device_path(index)?;
        let h = unsafe { (api().MWOpenChannelByPath)(path.as_ptr()) };
        if h.is_null() {
            bail!("MWOpenChannelByPath failed for {}", wide_to_string(&path));
        }
        Ok(Channel { h, index, path })
    }

    pub fn info(&self) -> Result<ChannelInfo> {
        let mut ci: ChannelInfo = unsafe { std::mem::zeroed() };
        check(unsafe { (api().MWGetChannelInfo)(self.h, &mut ci) }, "MWGetChannelInfo")?;
        Ok(ci)
    }
    pub fn signal(&self) -> Result<VideoSignalStatus> {
        get!(self, MWGetVideoSignalStatus, VideoSignalStatus)
    }
    pub fn buffer_info(&self) -> Result<BufferInfo> {
        get!(self, MWGetVideoBufferInfo, BufferInfo)
    }
    pub fn capture_status(&self) -> Result<CaptureStatus> {
        get!(self, MWGetVideoCaptureStatus, CaptureStatus)
    }
    pub fn input_source(&self) -> Result<u32> {
        get!(self, MWGetVideoInputSource, u32)
    }
    pub fn input_scan(&self) -> Result<bool> {
        let v: u8 = get!(self, MWGetInputSourceScan, u8)?;
        Ok(v != 0)
    }
    pub fn custom_timing_count(&self) -> Result<u32> {
        get!(self, MWGetCustomVideoTimingsCount, u32)
    }
    pub fn temperature(&self) -> Result<u32> {
        get!(self, MWGetTemperature, u32)
    }
    pub fn frame_info(&self, slot: u8) -> Result<FrameInfo> {
        let mut v = FrameInfo::default();
        check(unsafe { (api().MWGetVideoFrameInfo)(self.h, slot, &mut v) }, "MWGetVideoFrameInfo")?;
        Ok(v)
    }
    pub fn input_sources(&self) -> Result<Vec<u32>> {
        let mut buf = [0u32; 32];
        let mut n = buf.len() as u32;
        check(unsafe { (api().MWGetVideoInputSourceArray)(self.h, buf.as_mut_ptr(), &mut n) }, "MWGetVideoInputSourceArray")?;
        Ok(buf[..(n as usize).min(buf.len())].to_vec())
    }

    pub fn set_input_scan(&self, on: bool) -> Result<()> {
        check(unsafe { (api().MWSetInputSourceScan)(self.h, on as u8) }, "MWSetInputSourceScan")
    }
    pub fn set_input_source(&self, src: u32) -> Result<()> {
        check(unsafe { (api().MWSetVideoInputSource)(self.h, src) }, "MWSetVideoInputSource")
    }
    pub fn set_input_color(&self, color: i32, quant: i32) -> Result<()> {
        check(unsafe { (api().MWSetVideoInputColorFormat)(self.h, color) }, "MWSetVideoInputColorFormat")?;
        check(unsafe { (api().MWSetVideoInputQuantizationRange)(self.h, quant) }, "MWSetVideoInputQuantizationRange")
    }
    pub fn set_custom_timing(&self, t: &CustomTiming) -> Result<()> {
        let mut t = *t;
        check(unsafe { (api().MWSetCustomVideoTiming)(self.h, &mut t) }, "MWSetCustomVideoTiming")
    }
    /// The driver's stored custom timings (persistent; matched before the built-in table?).
    pub fn custom_timings(&self) -> Result<Vec<CustomTiming>> {
        let mut a = CustomTimingArray::default();
        check(unsafe { (api().MWGetCustomVideoTimings)(self.h, &mut a) }, "MWGetCustomVideoTimings")?;
        Ok(a.timings[..(a.count as usize).min(8)].to_vec())
    }
    pub fn set_custom_timings(&self, t: &[CustomTiming]) -> Result<()> {
        if t.len() > 8 {
            bail!("at most 8 custom timings");
        }
        let mut a = CustomTimingArray { count: t.len() as u8, ..Default::default() };
        a.timings[..t.len()].copy_from_slice(t);
        check(unsafe { (api().MWSetCustomVideoTimings)(self.h, &mut a) }, "MWSetCustomVideoTimings")
    }
}
