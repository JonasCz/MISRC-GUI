//! Analog line-in audio (the card's audio jack): SDK audio capture on its own channel handle and
//! thread, independent of the raw video `Stream` (both may run at once). Blocks of 192 stereo
//! samples go to an `AudioSink` with the device timestamp, which is on the same 100 ns clock as
//! the video frame timestamps, so audio and RF can be aligned.
//!
//! The SDK delivers 8 channels of 32-bit samples per instant (L0 L1 L2 L3 R0 R1 R2 R3); the
//! line-in is the L0/R0 pair. Samples are left-justified: a 24-bit codec leaves the low 8 bits 0.
//!
//! `LockedSink` puts `audiolock` in front of a sink: the sink then gets the audio resampled onto
//! the RF clock of the card's stream (docs/12 §9).
//!
//! `Recorder` is the CLI's file sink: `audio.wav` (stereo, 32-bit PCM; RF-locked or the card's
//! clock), optionally `audio_native.wav` (card clock), and `audio.csv` (one row per block with the
//! device timestamp and the lock's state).

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Result};

use super::ks::Event;
use super::sdk::{self, api, check, AudioFrame, AudioSignalStatus, Channel, Instance, AUDIO_CHANNELS, AUDIO_SAMPLES_PER_FRAME};
use crate::audiolock::{AudioLock, Chunk, LockParams, RfClock};
use crate::{say, util, warn};

/// Stereo samples per block (one SDK audio frame).
pub const BLOCK_FRAMES: usize = AUDIO_SAMPLES_PER_FRAME;

/// What the card reports for its audio input when the stream starts.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioFormat {
    pub sample_rate: u32,
    /// Significant (high) bits of each 32-bit sample.
    pub bits_per_sample: u32,
    /// Bit n = channel pair n carries data (the line-in is pair 0).
    pub channel_valid: u16,
    /// MWCAP audio input source (0x400 = line in).
    pub source: u32,
    pub lpcm: bool,
}

/// One block, valid only during `AudioSink::block`.
pub struct AudioBlock<'a> {
    /// Interleaved L, R; `BLOCK_FRAMES` pairs, left-justified 32-bit.
    pub samples: &'a [i32],
    /// Block counter of this stream, from 0.
    pub seq: u64,
    /// The SDK's audio frame number.
    pub sdk_frame: u32,
    /// SDK frames lost right before this one (0 = continuous).
    pub missed: u32,
    /// Device timestamp of the block (100 ns).
    pub timestamp_100ns: i64,
    /// Frames the SDK still had buffered.
    pub buffered: u32,
    /// Host time the block was read (`util::host_us`).
    pub host_us: u64,
    /// Set when the samples are RF-locked output (`LockedSink`); then `samples` holds any number
    /// of pairs, `timestamp_100ns` is the first sample's time from the RF fit, and `missed` /
    /// `sdk_frame` / `buffered` / `host_us` describe the input block that produced them.
    pub lock: Option<LockInfo>,
}

/// State of the RF lock for a block of locked output.
#[derive(Clone, Copy, Debug)]
pub struct LockInfo {
    /// Output index of the first sample: it belongs to RF sample `position × rf_hz / sample rate`
    /// of the stream (`epoch`); 0 = RF sample 0.
    pub position: u64,
    pub epoch: u64,
    /// First / last block of this RF stream.
    pub first: bool,
    pub last: bool,
    pub rf_hz: f64,
    /// Input samples per output sample: 1 + the card's audio/RF clock offset.
    pub step: f64,
    /// Output count minus the RF target, in samples.
    pub error: f64,
}

pub trait AudioSink: Send {
    /// Called once on the audio thread when the capture runs, before the first block.
    fn started(&mut self, _f: &AudioFormat) {}
    /// Called on the audio thread for every block, in order. Return an error to stop the stream.
    fn block(&mut self, b: &AudioBlock) -> Result<()>;
    /// Called once on the audio thread after the last block.
    fn finish(&mut self, _s: &AudioSummary) -> Result<()> {
        Ok(())
    }
}

impl<F: FnMut(&AudioBlock) -> Result<()> + Send> AudioSink for F {
    fn block(&mut self, b: &AudioBlock) -> Result<()> {
        self(b)
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioSummary {
    /// SDK frames (blocks) delivered.
    pub frames: u64,
    pub sample_rate: u32,
    pub missed: u64,
    /// Peak |sample| per channel, left-justified 32-bit scale.
    pub peak: [u32; 2],
}

pub struct AudioStream {
    stop: Arc<AtomicBool>,
    th: Option<JoinHandle<Result<AudioSummary>>>,
    format: AudioFormat,
}

impl AudioStream {
    /// Start audio capture on SDK channel `index`; returns once the SDK capture runs. The SDK
    /// instance must already be initialised (an open `Device` holds it).
    pub fn start(index: i32, sink: Box<dyn AudioSink>) -> Result<AudioStream> {
        let inst = sdk::instance(None)?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let (tx, rx) = mpsc::channel();
        let th = thread::Builder::new().name("mwadc-audio".into()).spawn(move || run(inst, index, sink, st, tx))?;
        match rx.recv() {
            Ok(Ok(format)) => Ok(AudioStream { stop, th: Some(th), format }),
            Ok(Err(e)) => {
                let _ = th.join();
                Err(e)
            }
            Err(_) => Err(anyhow!("audio thread ended before starting")),
        }
    }

    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    pub fn is_running(&self) -> bool {
        self.th.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stop(mut self) -> Result<AudioSummary> {
        self.join()
    }

    fn join(&mut self) -> Result<AudioSummary> {
        self.stop.store(true, Ordering::Relaxed);
        match self.th.take() {
            Some(t) => t.join().map_err(|_| anyhow!("audio thread panicked"))?,
            None => Ok(AudioSummary::default()),
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn source_name(src: u32) -> &'static str {
    match src >> 8 {
        1 => "HDMI",
        2 => "SDI",
        4 => "line in",
        8 => "mic in",
        _ => "?",
    }
}

fn setup(index: i32) -> Result<(Channel, Event, u64, AudioFormat)> {
    let ch = Channel::open(index)?;
    let mut src = 0u32;
    check(unsafe { (api().MWGetAudioInputSource)(ch.h, &mut src) }, "MWGetAudioInputSource")?;
    let mut st = AudioSignalStatus::default();
    check(unsafe { (api().MWGetAudioSignalStatus)(ch.h, &mut st) }, "MWGetAudioSignalStatus")?;
    let ev = Event::new(false)?;
    let n = unsafe { (api().MWRegisterNotify)(ch.h, ev.0, sdk::NOTIFY_AUDIO_FRAME_BUFFERED | sdk::NOTIFY_AUDIO_SIGNAL_CHANGE) };
    if n == 0 {
        return Err(anyhow!("MWRegisterNotify (audio) failed"));
    }
    if let Err(e) = check(unsafe { (api().MWStartAudioCapture)(ch.h) }, "MWStartAudioCapture") {
        unsafe { (api().MWUnregisterNotify)(ch.h, n) };
        return Err(e);
    }
    let format = AudioFormat {
        // The line-in codec runs at 48 kHz / 24 bit; the status reads 0 before the first frame on some inputs.
        sample_rate: if st.sample_rate > 0 { st.sample_rate } else { 48000 },
        bits_per_sample: if st.bits_per_sample > 0 { st.bits_per_sample as u32 } else { 24 },
        channel_valid: st.channel_valid,
        source: src,
        lpcm: st.lpcm != 0,
    };
    Ok((ch, ev, n, format))
}

fn run(_inst: Arc<Instance>, index: i32, mut sink: Box<dyn AudioSink>, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<AudioFormat>>) -> Result<AudioSummary> {
    // Blocks come every 4 ms and the SDK holds only a few: boosted like the frame threads.
    let _boost = super::stream::raise_priority("audio", "Capture", windows_sys::Win32::System::Threading::AVRT_PRIORITY_HIGH);
    let (ch, ev, notify, format) = match setup(index) {
        Ok(x) => x,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e:#}")));
            return Err(e);
        }
    };
    say!(
        "audio: input source {:#x} ({}), channel-valid mask {:#x}, {} bit, {} Hz, LPCM {}",
        format.source,
        source_name(format.source),
        format.channel_valid,
        format.bits_per_sample,
        format.sample_rate,
        format.lpcm as u8
    );
    let _ = ready.send(Ok(format));
    sink.started(&format);

    let mut f: Box<AudioFrame> = Box::new(unsafe { std::mem::zeroed() });
    let mut lr = vec![0i32; BLOCK_FRAMES * 2];
    let mut sum = AudioSummary { sample_rate: format.sample_rate, ..Default::default() };
    // Losses are found from the timestamps: the SDK frame number is a ring index (it wraps at 32).
    let period = (BLOCK_FRAMES as f64 * 1e7 / format.sample_rate as f64) as i64;
    let mut last_ts: Option<i64> = None;
    let mut res = Ok(());
    'outer: while !stop.load(Ordering::Relaxed) {
        if !ev.wait(100) {
            continue;
        }
        let mut bits = 0u64;
        unsafe { (api().MWGetNotifyStatus)(ch.h, notify, &mut bits) };
        if bits & sdk::NOTIFY_AUDIO_SIGNAL_CHANGE != 0 {
            let mut s2 = AudioSignalStatus::default();
            unsafe { (api().MWGetAudioSignalStatus)(ch.h, &mut s2) };
            say!("audio: signal change: mask {:#x}, {} Hz", { s2.channel_valid }, { s2.sample_rate });
        }
        // Drain everything buffered.
        while unsafe { (api().MWCaptureAudioFrame)(ch.h, &mut *f) } == sdk::MW_SUCCEEDED {
            let host_us = util::host_us();
            let idx = f.frame;
            let ts = f.timestamp;
            let mut missed = 0u32;
            if let Some(l) = last_ts {
                let dt = ts - l;
                if dt > period * 3 / 2 {
                    missed = ((dt as f64 / period as f64).round() as i64 - 1).clamp(0, u32::MAX as i64) as u32;
                }
            }
            last_ts = Some(ts);
            sum.missed += missed as u64;
            let samples = f.samples;
            for i in 0..BLOCK_FRAMES {
                for k in 0..2 {
                    let v = samples[i * AUDIO_CHANNELS + k * 4] as i32;
                    sum.peak[k] = sum.peak[k].max(v.unsigned_abs());
                    lr[i * 2 + k] = v;
                }
            }
            let b = AudioBlock { samples: &lr, seq: sum.frames, sdk_frame: idx, missed, timestamp_100ns: f.timestamp, buffered: f.frame_count, host_us, lock: None };
            sum.frames += 1;
            if let Err(e) = sink.block(&b) {
                warn!("!! audio sink: {e:#}; stopping");
                res = Err(e);
                break 'outer;
            }
        }
    }
    unsafe {
        (api().MWStopAudioCapture)(ch.h);
        (api().MWUnregisterNotify)(ch.h, notify);
    }
    let fin = sink.finish(&sum);
    res.and(fin).map(|_| sum)
}

// ---------------------------------------------------------------- RF lock adapter

/// Wraps a sink so it receives the audio locked to the RF clock (`audiolock::AudioLock`) instead
/// of the card's own: output sample 0 = RF sample 0 of each stream on `clock`, then exactly
/// `rate / rf_hz` samples per RF sample. Without an RF stream nothing is delivered (the last 2 s
/// are held in case one starts).
pub struct LockedSink {
    lock: AudioLock,
    inner: Box<dyn AudioSink>,
    seq: u64,
    missed: u32,
}

impl LockedSink {
    pub fn new(clock: Arc<RfClock>, inner: Box<dyn AudioSink>) -> LockedSink {
        LockedSink { lock: AudioLock::new(clock, LockParams::default()), inner, seq: 0, missed: 0 }
    }
}

fn lock_info(c: &Chunk) -> LockInfo {
    LockInfo { position: c.position, epoch: c.epoch, first: c.first, last: c.last, rf_hz: c.rf_hz, step: c.step, error: c.error }
}

impl AudioSink for LockedSink {
    fn started(&mut self, f: &AudioFormat) {
        self.lock.set_audio_hz(f.sample_rate as f64);
        self.inner.started(f);
    }

    fn block(&mut self, b: &AudioBlock) -> Result<()> {
        let LockedSink { lock, inner, seq, missed } = self;
        *missed += b.missed;
        lock.push(b.samples, b.timestamp_100ns, b.missed, &mut |c: &Chunk| {
            let o = AudioBlock { samples: c.samples, seq: *seq, missed: *missed, timestamp_100ns: c.start_100ns, lock: Some(lock_info(c)), ..*b };
            *seq += 1;
            *missed = 0;
            inner.block(&o)
        })
    }

    fn finish(&mut self, s: &AudioSummary) -> Result<()> {
        self.inner.finish(s)
    }
}

// ---------------------------------------------------------------- file recorder (CLI)

/// Clock of `audio.wav`.
pub enum AudioClock {
    /// The card's own (free-running, +65 ppm against the RF on board 0).
    Card,
    /// Resampled onto the RF stream's clock: needs that stream (`Device::rf_clock`).
    Rf(Arc<RfClock>),
}

/// How the RF lock went, for the end-of-capture summary.
#[derive(Clone, Debug, Default)]
pub struct LockReport {
    /// Locked samples written to audio.wav.
    pub samples: u64,
    /// The RF stream ended and audio.wav covers all of it.
    pub complete: bool,
    pub rf_hz: f64,
    /// Count error (output − RF target, samples) at the last block, largest overall, and largest
    /// after the first 2 s (the loop's run-in).
    pub error_last: f64,
    pub error_max: f64,
    pub error_max_settled: f64,
    /// Step (input per output sample) as ppm off 1: mean over the run, min, max.
    pub step_ppm_mean: f64,
    pub step_ppm_min: f64,
    pub step_ppm_max: f64,
}

pub struct Recorder {
    stream: AudioStream,
    report: Option<Arc<Mutex<LockReport>>>,
}

impl Recorder {
    /// Record SDK channel `index` into `dir`: `audio.wav` on `clock`, plus `audio_native.wav` on
    /// the card's clock when `native` (and the clock is the RF), and `audio.csv`.
    pub fn start(index: i32, dir: &Path, clock: AudioClock, native: bool) -> Result<Recorder> {
        let mut csv = BufWriter::new(File::create(dir.join("audio.csv"))?);
        writeln!(csv, "index,sdk_frame,timestamp_100ns,buffered,host_us,missed,locked_position,locked_frames,step_ppm,error_samples")?;
        let (lock, report, native) = match clock {
            AudioClock::Card => (None, None, false),
            AudioClock::Rf(c) => {
                let r = Arc::new(Mutex::new(LockReport { step_ppm_min: f64::MAX, step_ppm_max: f64::MIN, ..Default::default() }));
                (Some(AudioLock::new(c, LockParams::default())), Some(r), native)
            }
        };
        let sink = FileSink {
            main: Some(Wav::create(&dir.join("audio.wav"))?),
            native: if native { Some(Wav::create(&dir.join("audio_native.wav"))?) } else { None },
            csv,
            lock,
            report: report.clone(),
            step_sum: 0.0,
            steps: 0,
        };
        Ok(Recorder { stream: AudioStream::start(index, Box::new(sink))?, report })
    }

    pub fn format(&self) -> &AudioFormat {
        self.stream.format()
    }

    /// RF lock state so far (`None` on the card's clock).
    pub fn lock_report(&self) -> Option<LockReport> {
        self.report.as_ref().map(|r| r.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// After the RF stream ended: wait (up to `timeout`) until the audio covering its end has
    /// arrived and been written, so audio.wav spans exactly the RF capture.
    pub fn wait_rf_end(&self, timeout: std::time::Duration) -> bool {
        let t0 = std::time::Instant::now();
        loop {
            match self.lock_report() {
                None => return true,
                Some(r) if r.complete => return true,
                _ if t0.elapsed() >= timeout || !self.stream.is_running() => return false,
                _ => thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    pub fn stop(self) -> Result<(AudioSummary, Option<LockReport>)> {
        let report = self.report.clone();
        let sum = self.stream.stop()?;
        Ok((sum, report.map(|r| r.lock().unwrap_or_else(|e| e.into_inner()).clone())))
    }
}

struct Wav {
    w: BufWriter<File>,
    bytes: u64,
}

impl Wav {
    fn create(path: &Path) -> Result<Wav> {
        let mut w = BufWriter::new(File::create(path)?);
        // Placeholder header; the real rate and length are written at the end.
        wav_header(&mut w, 48000, 0)?;
        Ok(Wav { w, bytes: 0 })
    }

    fn write(&mut self, samples: &[i32]) -> Result<()> {
        for v in samples {
            self.w.write_all(&v.to_le_bytes())?;
        }
        self.bytes += (samples.len() * 4) as u64;
        Ok(())
    }

    fn finish(self, rate: u32) -> Result<()> {
        let mut file = self.w.into_inner().map_err(|e| anyhow!("{e}"))?;
        file.seek(SeekFrom::Start(0))?;
        wav_header(&mut file, rate, self.bytes.min(u32::MAX as u64 - 36) as u32)?;
        Ok(())
    }
}

struct FileSink {
    /// audio.wav: locked when `lock` is set, else the card's samples.
    main: Option<Wav>,
    native: Option<Wav>,
    csv: BufWriter<File>,
    lock: Option<AudioLock>,
    report: Option<Arc<Mutex<LockReport>>>,
    step_sum: f64,
    steps: u64,
}

impl AudioSink for FileSink {
    fn started(&mut self, f: &AudioFormat) {
        if let Some(l) = self.lock.as_mut() {
            l.set_audio_hz(f.sample_rate as f64);
        }
    }

    fn block(&mut self, b: &AudioBlock) -> Result<()> {
        let main = self.main.as_mut().ok_or_else(|| anyhow!("audio.wav already closed"))?;
        if let Some(n) = self.native.as_mut() {
            n.write(b.samples)?;
        }
        let Some(lock) = self.lock.as_mut() else {
            writeln!(self.csv, "{},{},{},{},{},{},,,,", b.seq, b.sdk_frame, b.timestamp_100ns, b.buffered, b.host_us, b.missed)?;
            return main.write(b.samples);
        };
        let rep = self.report.as_ref().expect("report with lock");
        let (mut frames, mut last): (u64, Option<(u64, f64, f64)>) = (0, None);
        let (step_sum, steps) = (&mut self.step_sum, &mut self.steps);
        lock.push(b.samples, b.timestamp_100ns, b.missed, &mut |c: &Chunk| {
            main.write(c.samples)?;
            let n = (c.samples.len() / 2) as u64;
            frames += n;
            last = Some((c.position + n, c.step, c.error));
            let mut r = rep.lock().unwrap_or_else(|e| e.into_inner());
            let ppm = (c.step - 1.0) * 1e6;
            r.samples += n;
            r.rf_hz = c.rf_hz;
            r.complete |= c.last;
            r.error_last = c.error;
            r.error_max = r.error_max.max(c.error.abs());
            if c.position as f64 > 2.0 * 48000.0 {
                r.error_max_settled = r.error_max_settled.max(c.error.abs());
            }
            *step_sum += ppm;
            *steps += 1;
            r.step_ppm_mean = *step_sum / *steps as f64;
            r.step_ppm_min = r.step_ppm_min.min(ppm);
            r.step_ppm_max = r.step_ppm_max.max(ppm);
            Ok(())
        })?;
        match last {
            Some((pos, step, err)) => writeln!(
                self.csv,
                "{},{},{},{},{},{},{pos},{frames},{:.4},{err:.4}",
                b.seq,
                b.sdk_frame,
                b.timestamp_100ns,
                b.buffered,
                b.host_us,
                b.missed,
                (step - 1.0) * 1e6
            )?,
            None => writeln!(self.csv, "{},{},{},{},{},{},,0,,", b.seq, b.sdk_frame, b.timestamp_100ns, b.buffered, b.host_us, b.missed)?,
        }
        Ok(())
    }

    fn finish(&mut self, s: &AudioSummary) -> Result<()> {
        self.csv.flush()?;
        for w in [self.main.take(), self.native.take()].into_iter().flatten() {
            w.finish(s.sample_rate)?;
        }
        Ok(())
    }
}

fn wav_header(w: &mut impl Write, rate: u32, data_bytes: u32) -> std::io::Result<()> {
    let (ch, bits) = (2u16, 32u16);
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&ch.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * ch as u32 * bits as u32 / 8).to_le_bytes())?;
    w.write_all(&(ch * bits / 8).to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_layout() {
        let mut v = Vec::new();
        wav_header(&mut v, 48000, 1536).unwrap();
        assert_eq!(v.len(), 44);
        assert_eq!(&v[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(v[4..8].try_into().unwrap()), 36 + 1536);
        assert_eq!(u16::from_le_bytes(v[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(v[24..28].try_into().unwrap()), 48000);
        assert_eq!(u32::from_le_bytes(v[28..32].try_into().unwrap()), 48000 * 8);
        assert_eq!(u32::from_le_bytes(v[40..44].try_into().unwrap()), 1536);
    }
}
