//! Streaming capture: every framestore slot is read, in order, by explicit slot index (docs/11 §2),
//! into a pool of page-aligned, pinned frame buffers, and handed to a sink on its own thread.
//!
//! ```text
//!   DMA thread (raised priority)                       sink thread
//!   wait for "frame buffered" ─► take free buffer ─►    receive (buffer, meta)
//!   MWCaptureVideoFrameToVirtualAddressEx (card DMA)    sink.frame(&FrameRef)  ── one linear pass
//!   timestamps, gap/torn check ─► send ──────────────►  return buffer to the pool
//! ```
//!
//! The host CPU never copies frame data: the card writes it, the sink reads it once, front to back
//! (the cache-friendly order: hardware prefetchers stream it). A slow sink only starves the pool;
//! the card's framestore (8 slots) then fills and frames are reported as skipped (`gap_frames`),
//! never torn silently.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::{null, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

use super::ks::Event;
use super::sdk::{self, api, check, Channel};
use crate::audiolock::RfClock;
use crate::fourcc::Format;
use crate::{say, warn};

/// Frame layout as delivered to sinks.
#[derive(Clone, Debug, Serialize)]
pub struct Geometry {
    pub format: Format,
    pub cx: usize,
    pub cy: usize,
    /// Bytes per line in the buffer (>= cx * bytes per sample).
    pub stride: usize,
    pub frame_bytes: usize,
    pub sample_rate_hz: f64,
}

impl Geometry {
    pub fn new(format: Format, cx: usize, cy: usize, sample_rate_hz: f64) -> Geometry {
        let stride = format.min_stride(cx);
        Geometry { format, cx, cy, stride, frame_bytes: stride * cy, sample_rate_hz }
    }

    pub fn samples_per_frame(&self) -> usize {
        self.cx * self.cy
    }
}

/// One captured frame, valid only during `FrameSink::frame`.
pub struct FrameRef<'a> {
    pub data: &'a [u8],
    pub geometry: &'a Geometry,
    /// Frame counter of this stream, from 0.
    pub seq: u64,
    pub slot: u8,
    /// Device timestamp of the frame start (100 ns units, same clock as the audio timestamps).
    pub start_100ns: i64,
    pub buffered_100ns: i64,
    /// Frames the card produced but this stream skipped right before this one (0 = continuous).
    pub gap_frames: u32,
    /// The slot was overwritten while it was being read.
    pub torn: bool,
    pub read_us: u32,
}

impl FrameRef<'_> {
    /// The frame as 32-bit words (RG10), one per sample instant; empty for other formats.
    pub fn words(&self) -> &[u32] {
        if self.geometry.format != Format::Rgb10 {
            return &[];
        }
        // Buffers are page aligned and stride is a multiple of 4.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr() as *const u32, self.data.len() / 4) }
    }
}

pub trait FrameSink: Send {
    /// Called on the sink thread for every frame, in order. Return an error to stop the stream.
    fn frame(&mut self, f: &FrameRef) -> Result<()>;
    /// Called once on the sink thread after the last frame.
    fn finish(&mut self, _s: &Summary) -> Result<()> {
        Ok(())
    }
}

impl<F: FnMut(&FrameRef) -> Result<()> + Send> FrameSink for F {
    fn frame(&mut self, f: &FrameRef) -> Result<()> {
        self(f)
    }
}

#[derive(Clone, Debug)]
pub struct StreamOptions {
    /// Stop after this many frames (`None` = until stopped).
    pub frames: Option<u64>,
    /// Frame buffers in the pool (>= 2). More absorbs sink hiccups; each costs one frame of RAM.
    pub pool: usize,
    /// Stop when no new frame arrives for this long.
    pub stall: Duration,
    /// Shared stop flag; `Stream::stop` sets it too.
    pub stop: Arc<AtomicBool>,
    /// Publish every frame's timestamp and RF sample index here (with the nominal RF rate), for
    /// audio locked to the RF clock. `Device::stream` fills in the device's clock.
    pub rf_clock: Option<(Arc<RfClock>, f64)>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        StreamOptions { frames: None, pool: 6, stall: Duration::from_secs(3), stop: Arc::new(AtomicBool::new(false)), rf_clock: None }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub captured: u64,
    pub gaps: u64,
    pub torn: u64,
    pub stalled: bool,
    pub seconds: f64,
    /// Median device frame period (100 ns), 0 if unknown.
    pub period_100ns: i64,
    pub max_behind: u32,
}

pub struct Stream {
    stop: Arc<AtomicBool>,
    dma: Option<JoinHandle<Result<Summary>>>,
    sink: Option<JoinHandle<Result<()>>>,
}

impl Stream {
    pub fn start(ch: Arc<Channel>, geo: Geometry, sink: Box<dyn FrameSink>, opts: StreamOptions) -> Result<Stream> {
        let pool = opts.pool.max(2);
        let mut bufs = Vec::with_capacity(pool);
        for _ in 0..pool {
            bufs.push(FrameBuf::new(geo.frame_bytes)?);
        }
        let (free_tx, free_rx) = mpsc::sync_channel::<FrameBuf>(pool);
        let (work_tx, work_rx) = mpsc::sync_channel::<(FrameBuf, Meta)>(pool);
        for b in bufs {
            free_tx.send(b).expect("pool channel");
        }
        let stop = opts.stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let geo2 = geo.clone();
        let stop2 = stop.clone();
        let sink_th = thread::Builder::new()
            .name("mwadc-sink".into())
            .spawn(move || sink_loop(sink, geo2, work_rx, free_tx, stop2))?;
        let dma_th = thread::Builder::new()
            .name("mwadc-dma".into())
            .spawn(move || dma_loop(&ch, &geo, &opts, pool, free_rx, work_tx, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Stream { stop, dma: Some(dma_th), sink: Some(sink_th) }),
            Ok(Err(e)) => {
                let _ = dma_th.join();
                let _ = sink_th.join();
                Err(e)
            }
            Err(_) => {
                let r = dma_th.join().map_err(|_| anyhow!("DMA thread panicked"))?;
                let _ = sink_th.join();
                Err(r.err().unwrap_or_else(|| anyhow!("DMA thread ended before starting")))
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.dma.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Ask the stream to stop and wait for both threads.
    pub fn stop(mut self) -> Result<Summary> {
        self.stop.store(true, Ordering::Relaxed);
        self.join()
    }

    /// Wait until the stream ends on its own (frame count reached, stall, error, or stop flag).
    pub fn wait(mut self) -> Result<Summary> {
        self.join()
    }

    fn join(&mut self) -> Result<Summary> {
        let d = self.dma.take().map(|t| t.join().map_err(|_| anyhow!("DMA thread panicked"))).transpose()?;
        let s = self.sink.take().map(|t| t.join().map_err(|_| anyhow!("sink thread panicked"))).transpose()?;
        let sum = d.unwrap_or_else(|| Ok(Summary::default()));
        // A sink error is the more specific cause when both failed.
        if let Some(Err(e)) = s {
            return Err(e);
        }
        sum
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.dma.is_some() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.join();
        }
    }
}

// ---------------------------------------------------------------- buffers

/// Page-aligned frame buffer (the driver pins it once; DMA prefers page-aligned targets).
struct FrameBuf {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for FrameBuf {}

const PAGE: usize = 4096;

impl FrameBuf {
    fn new(len: usize) -> Result<FrameBuf> {
        let layout = Layout::from_size_align(len.max(1).next_multiple_of(PAGE), PAGE)?;
        // Zeroed: touches every page once now instead of page-faulting during the first DMA.
        let p = unsafe { alloc_zeroed(layout) };
        Ok(FrameBuf { ptr: NonNull::new(p).ok_or_else(|| anyhow!("out of memory for a {len}-byte frame buffer"))?, len })
    }
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len.max(1).next_multiple_of(PAGE), PAGE).unwrap();
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

struct Meta {
    seq: u64,
    slot: u8,
    start: i64,
    buffered: i64,
    gap: u32,
    torn: bool,
    read_us: u32,
}

// ---------------------------------------------------------------- sink thread

fn sink_loop(
    mut sink: Box<dyn FrameSink>,
    geo: Geometry,
    work_rx: Receiver<(FrameBuf, Meta)>,
    free_tx: SyncSender<FrameBuf>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let _boost = raise_priority("sink", "Capture", windows_sys::Win32::System::Threading::AVRT_PRIORITY_HIGH);
    let mut sum = Summary::default();
    let mut res = Ok(());
    for (buf, m) in work_rx {
        if res.is_ok() {
            let f = FrameRef {
                data: buf.as_slice(),
                geometry: &geo,
                seq: m.seq,
                slot: m.slot,
                start_100ns: m.start,
                buffered_100ns: m.buffered,
                gap_frames: m.gap,
                torn: m.torn,
                read_us: m.read_us,
            };
            if let Err(e) = sink.frame(&f) {
                warn!("!! sink: {e:#}; stopping");
                stop.store(true, Ordering::Relaxed);
                res = Err(e);
            }
            sum.captured += 1;
            sum.gaps += m.gap as u64;
            sum.torn += m.torn as u64;
        }
        // Back to the pool; the DMA thread may already be gone (then the buffer is dropped here).
        let _ = free_tx.try_send(buf);
    }
    let fin = sink.finish(&sum);
    res.and(fin)
}

// ---------------------------------------------------------------- DMA thread

/// Scheduling boost for a stream thread, undone when dropped (on the same thread).
///
/// MMCSS registers the thread for a multimedia task: the scheduler runs it in the real-time
/// priority range (above every normal thread, also those of other processes) and keeps doing so
/// under load, which `SetThreadPriority` alone does not (HIGHEST is still a dynamic priority that
/// CPU-heavy foreground work and priority decay can starve for 100+ ms). All stream threads (DMA,
/// sink, audio) use the "Capture" task (HKLM\...\Multimedia\SystemProfile\Tasks, category Medium):
/// above normal work, without starving the host application's own threads. Without MMCSS
/// (service off, task missing): plain THREAD_PRIORITY_HIGHEST.
pub(crate) struct Boost(windows_sys::Win32::Foundation::HANDLE);

impl Drop for Boost {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0) };
        }
    }
}

pub(crate) fn raise_priority(role: &str, task: &str, prio: windows_sys::Win32::System::Threading::AVRT_PRIORITY) -> Boost {
    use windows_sys::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    let name: Vec<u16> = task.encode_utf16().chain(Some(0)).collect();
    let mut index = 0u32;
    let h = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index) };
    if h.is_null() {
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        warn!("  {role} thread: MMCSS task \"{task}\" unavailable, using THREAD_PRIORITY_HIGHEST");
    } else {
        unsafe { AvSetMmThreadPriority(h, prio) };
        say!("  {role} thread: MMCSS \"{task}\" priority {prio}");
    }
    Boost(h)
}

fn dma_loop(
    ch: &Channel,
    geo: &Geometry,
    opts: &StreamOptions,
    pool: usize,
    free_rx: Receiver<FrameBuf>,
    work_tx: SyncSender<(FrameBuf, Meta)>,
    ready: mpsc::Sender<Result<()>>,
) -> Result<Summary> {
    let _boost = raise_priority("DMA", "Capture", windows_sys::Win32::System::Threading::AVRT_PRIORITY_HIGH);
    let a = api();
    let fail = |e: anyhow::Error, ready: &mpsc::Sender<Result<()>>| -> anyhow::Error {
        let _ = ready.send(Err(anyhow!("{e:#}")));
        e
    };

    // Pin every pool buffer once: the driver (MWProCapture.sys 1.3.0.4323) locks the pages with an
    // MDL (MmProbeAndLockPages, not charged to this process's working set), maps them, builds their
    // scatter-gather list and keeps it until unpin; it has no count or size limit of its own, only
    // the system's locked-memory / nonpaged-pool limits. A capture reuses that list only when it
    // targets the pinned base address exactly (as here). A buffer the driver will not pin is
    // VirtualLock'ed instead, after growing the working set to hold it (VirtualLock is charged to
    // it): resident, mapped per frame by the driver (slower, still no page faults).
    let mut held: Vec<FrameBuf> = (0..pool).filter_map(|_| free_rx.recv().ok()).collect();
    let mut pinned: Vec<usize> = Vec::new();
    let mut unpinned: Vec<(usize, usize)> = Vec::new();
    for b in held.iter_mut() {
        if unsafe { (a.MWPinVideoBuffer)(ch.h, b.as_mut_ptr(), geo.frame_bytes as u32) } == sdk::MW_SUCCEEDED {
            pinned.push(b.as_mut_ptr() as usize);
        } else {
            unpinned.push((b.as_mut_ptr() as usize, b.len));
        }
    }
    let mut locked: Vec<(usize, usize)> = Vec::new();
    let _ws = if unpinned.is_empty() {
        None
    } else {
        let ws = WorkingSet::reserve(unpinned.len() * geo.frame_bytes.next_multiple_of(PAGE));
        if !ws.ok {
            warn!("  could not grow the working set by {} MB for unpinned buffers", ws.bytes >> 20);
        }
        for &(p, len) in &unpinned {
            if unsafe { windows_sys::Win32::System::Memory::VirtualLock(p as *const _, len) } != 0 {
                locked.push((p, len));
            }
        }
        warn!("  {} frame buffer(s) not pinned by the driver; {} VirtualLock'ed instead", unpinned.len(), locked.len());
        Some(ws)
    };
    let pool_mb = (pool * geo.frame_bytes) as f64 / (1024.0 * 1024.0);
    let pool_ms = pool as f64 * geo.samples_per_frame() as f64 / geo.sample_rate_hz * 1e3;
    if pinned.len() < pool {
        warn!("  frame pool: {pool} x {} bytes ({pool_mb:.0} MB, {pool_ms:.0} ms), only {} pinned", geo.frame_bytes, pinned.len());
    } else {
        say!("  frame pool: {pool} x {} bytes ({pool_mb:.0} MB, {pool_ms:.0} ms), all pinned", geo.frame_bytes);
    }
    let mut free: Vec<FrameBuf> = held;

    let cap_ev = Event::new(false).map_err(|e| fail(e, &ready))?;
    if let Err(e) = check(unsafe { (a.MWStartVideoCapture)(ch.h, cap_ev.0) }, "MWStartVideoCapture") {
        unpin(ch, &pinned);
        unlock(&locked);
        return Err(fail(e, &ready));
    }
    let notify_ev = Event::new(false).map_err(|e| fail(e, &ready))?;
    let mask = sdk::NOTIFY_VIDEO_FRAME_BUFFERED | sdk::NOTIFY_VIDEO_SIGNAL_CHANGE | sdk::NOTIFY_INPUT_SPECIFIC_CHANGE | sdk::NOTIFY_VIDEO_INPUT_SOURCE_CHANGE;
    let notify = unsafe { (a.MWRegisterNotify)(ch.h, notify_ev.0, mask) };
    if notify == 0 {
        warn!("  MWRegisterNotify failed: polling the buffer info only");
    }
    if let Some((c, hz)) = &opts.rf_clock {
        c.begin(*hz, geo.samples_per_frame() as u64);
    }
    let _ = ready.send(Ok(()));

    let r = dma_run(ch, geo, opts, &cap_ev, &notify_ev, notify, &mut free, &free_rx, &work_tx);
    if let Some((c, _)) = &opts.rf_clock {
        c.end();
    }
    drop(work_tx);
    unsafe {
        if notify != 0 {
            (a.MWUnregisterNotify)(ch.h, notify);
        }
        (a.MWStopVideoCapture)(ch.h);
    }
    // Collect the pool back before unpinning (buffers in flight return from the sink thread).
    let deadline = Instant::now() + Duration::from_secs(10);
    while free.len() < pool {
        match free_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(b) => free.push(b),
            Err(_) => break,
        }
    }
    unpin(ch, &pinned);
    unlock(&locked);
    if free.len() < pool {
        warn!("!! {} frame buffer(s) not returned by the sink", pool - free.len());
        // Leak them: they may still be pinned or in use.
        std::mem::forget(free);
    }
    r
}

/// Process working-set growth for one stream's pool, given back when dropped. Added to (and
/// later subtracted from) whatever the process has, so streams of several cards stack.
struct WorkingSet {
    bytes: usize,
    ok: bool,
}

impl WorkingSet {
    /// Headroom kept above the pool for the rest of the process.
    const SLACK: usize = 64 << 20;

    fn adjust(add: usize, sub: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            GetProcessWorkingSetSizeEx, SetProcessWorkingSetSizeEx, QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
        unsafe {
            if GetProcessWorkingSetSizeEx(GetCurrentProcess(), &mut min, &mut max, &mut flags) == 0 {
                return false;
            }
            let new_min = (min + add).saturating_sub(sub);
            let new_max = max.max(new_min + Self::SLACK);
            // Soft limits (the defaults): the minimum is a guarantee, not a cap.
            SetProcessWorkingSetSizeEx(GetCurrentProcess(), new_min, new_max, QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE) != 0
        }
    }

    fn reserve(bytes: usize) -> WorkingSet {
        WorkingSet { bytes, ok: Self::adjust(bytes, 0) }
    }
}

impl Drop for WorkingSet {
    fn drop(&mut self) {
        if self.ok {
            Self::adjust(0, self.bytes);
        }
    }
}

/// VirtualUnlock of the buffers that were VirtualLock'ed; before they are freed.
fn unlock(locked: &[(usize, usize)]) {
    for &(p, len) in locked {
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock(p as *const _, len) };
    }
}

fn unpin(ch: &Channel, pinned: &[usize]) {
    for &p in pinned {
        unsafe { (api().MWUnpinVideoBuffer)(ch.h, p as *mut u8) };
    }
}

#[allow(clippy::too_many_arguments)]
fn dma_run(
    ch: &Channel,
    geo: &Geometry,
    opts: &StreamOptions,
    cap_ev: &Event,
    notify_ev: &Event,
    notify: u64,
    free: &mut Vec<FrameBuf>,
    free_rx: &Receiver<FrameBuf>,
    work_tx: &SyncSender<(FrameBuf, Meta)>,
) -> Result<Summary> {
    let a = api();
    let limit = opts.frames.unwrap_or(u64::MAX);
    let bi0 = ch.buffer_info()?;
    let cmax = bi0.max_frames.max(1) as u32;
    let t_start = Instant::now();
    let mut last_slot = bi0.newest_buffered;
    let mut last_new = Instant::now();
    let mut prev_start: Option<i64> = None;
    let mut dts: Vec<i64> = Vec::new();
    let mut sum = Summary::default();
    let mut last_state = ch.signal().map(|s| s.state).unwrap_or(-1);
    let mut last_tick = Instant::now();
    // RF sample index of the next frame, skipped frames included (the audio lock's timeline).
    let mut rf_index = 0u64;
    let spf = geo.samples_per_frame() as u64;

    'outer: while sum.captured < limit && !opts.stop.load(Ordering::Relaxed) {
        if notify_ev.wait(5) && notify != 0 {
            let mut st = 0u64;
            unsafe { (a.MWGetNotifyStatus)(ch.h, notify, &mut st) };
            if st & !sdk::NOTIFY_VIDEO_FRAME_BUFFERED != 0 {
                let s = ch.signal()?;
                say!("  [{:7.3}s] notify {st:#x}: signal {} {}x{}", t_start.elapsed().as_secs_f64(), sdk::state_name(s.state), { s.cx }, { s.cy });
            }
        }
        if last_tick.elapsed() >= Duration::from_millis(250) {
            last_tick = Instant::now();
            let st = ch.signal().map(|s| s.state).unwrap_or(-1);
            if st != last_state {
                say!("  [{:7.3}s] signal state {} -> {}", t_start.elapsed().as_secs_f64(), sdk::state_name(last_state), sdk::state_name(st));
                last_state = st;
            }
        }
        let newest = ch.buffer_info()?.newest_buffered;
        if newest == last_slot {
            if last_new.elapsed() > opts.stall {
                warn!("  no new frame for {:.1}s: stopping (FPGA input writer not running?)", opts.stall.as_secs_f64());
                sum.stalled = true;
                break;
            }
            continue;
        }
        let n_new = (newest as u32 + cmax - last_slot as u32) % cmax;
        sum.max_behind = sum.max_behind.max(n_new);
        for k in 1..=n_new {
            if sum.captured >= limit || opts.stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            let slot = ((last_slot as u32 + k) % cmax) as u8;
            let fi0 = ch.frame_info(slot)?;
            let mut buf = match free.pop() {
                Some(b) => b,
                None => {
                    // Pool empty: the sink is behind. Say how long it held the stream up, so a skip
                    // that follows can be told apart from a DMA-side stall.
                    let t_wait = Instant::now();
                    let b = loop {
                        match free_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(b) => break b,
                            Err(RecvTimeoutError::Timeout) if !opts.stop.load(Ordering::Relaxed) => continue,
                            Err(_) => break 'outer,
                        }
                    };
                    let waited = t_wait.elapsed();
                    if waited >= Duration::from_millis(20) {
                        warn!("  [{:7.3}s] frame pool empty: waited {:.0} ms for the sink", t_start.elapsed().as_secs_f64(), waited.as_secs_f64() * 1e3);
                    }
                    b
                }
            };
            let t0 = Instant::now();
            let r = unsafe {
                (a.MWCaptureVideoFrameToVirtualAddressEx)(
                    ch.h,
                    slot as i32,
                    buf.as_mut_ptr(),
                    geo.frame_bytes as u32,
                    geo.stride as u32,
                    0,
                    0,
                    geo.format.fourcc(),
                    geo.cx as i32,
                    geo.cy as i32,
                    0,
                    0,
                    0,
                    null(),
                    0,
                    100,
                    0,
                    100,
                    0,
                    0, // MWCAP_VIDEO_DEINTERLACE_WEAVE
                    0, // MWCAP_VIDEO_ASPECT_RATIO_IGNORE
                    null(),
                    null(),
                    0,
                    0,
                    if geo.format == Format::Rgb10 { sdk::COLOR_RGB } else { sdk::COLOR_YUV601 },
                    sdk::QUANT_FULL,
                    sdk::SAT_UNKNOWN,
                )
            };
            check(r, "MWCaptureVideoFrameToVirtualAddressEx")?;
            loop {
                if !cap_ev.wait(2000) {
                    bail!("capture of slot {slot} did not complete within 2 s");
                }
                if ch.capture_status()?.frame_completed != 0 {
                    break;
                }
            }
            let read_us = t0.elapsed().as_micros() as u32;
            let fi1 = ch.frame_info(slot)?;
            let start = fi1.field_start_times[0];
            let torn = fi0.field_start_times[0] != start;
            let dt = prev_start.map(|p| start - p).unwrap_or(0);
            if dt > 0 && dts.len() < 32 {
                dts.push(dt);
            }
            let period = median(&dts);
            let gap = if period > 0 && dt > 0 { ((dt as f64 / period as f64).round() as i64 - 1).max(0) as u32 } else { 0 };
            sum.gaps += gap as u64;
            sum.torn += torn as u64;
            if gap > 0 || torn {
                warn!(
                    "  [{:7.3}s] frame {}: {} skipped before it{}, {} slot(s) were waiting (framestore {cmax})",
                    t_start.elapsed().as_secs_f64(),
                    sum.captured,
                    gap,
                    if torn { ", torn" } else { "" },
                    n_new
                );
            }
            rf_index += gap as u64 * spf;
            // Published here rather than in the sink: the audio side learns of the frame one
            // DMA earlier, which is what bounds its added latency.
            if let Some((c, _)) = &opts.rf_clock {
                c.frame(start, rf_index);
            }
            rf_index += spf;
            let m = Meta { seq: sum.captured, slot, start, buffered: { fi1.field_buffered_times }[0], gap, torn, read_us };
            if work_tx.send((buf, m)).is_err() {
                break 'outer; // sink thread gone
            }
            prev_start = Some(start);
            sum.captured += 1;
        }
        last_slot = newest;
        last_new = Instant::now();
    }
    sum.seconds = t_start.elapsed().as_secs_f64();
    sum.period_100ns = median(&dts);
    Ok(sum)
}

fn median(v: &[i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}
