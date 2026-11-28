//! Command-line interface. Hardware commands run on Windows only; `export` runs anywhere.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use mwadc_core::board::Board;
use mwadc_core::config::CaptureConfig;
use mwadc_core::parse::{parse_board, parse_hz, parse_u32, parse_u8};

#[derive(Parser)]
#[command(name = "mwadc", version, about = "Raw ADC capture with the Magewell Pro Capture HDMI, DVI and AIO (see docs/12)")]
pub struct Cli {
    #[command(flatten)]
    pub dev: DeviceOpts,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Args, Clone, Debug)]
pub struct DeviceOpts {
    /// Card to use, by its rotary-switch number (0-F, the "board" in `mwadc list`).
    /// Required when more than one card is installed, unless --device is given.
    #[arg(long, short = 'b', global = true, value_parser = |s: &str| parse_board(s).map_err(|e| e.to_string()))]
    pub board: Option<u8>,
    /// Channel on that card (the HDMI, DVI and AIO cards have one: 0)
    #[arg(long, default_value_t = 0, global = true)]
    pub channel: u8,
    /// Alternative to --board: SDK enumeration index, as printed by `mwadc list` in [brackets]
    #[arg(long, short = 'd', global = true, conflicts_with = "board")]
    pub device: Option<i32>,
    /// Path to LibMWCapture.dll (default: $MWADC_SDK_DLL, next to mwadc.exe, then the DLL search path)
    #[arg(long, global = true)]
    pub dll: Option<PathBuf>,
    /// Directory for session output (one sub-directory per run)
    #[arg(long, global = true, default_value = "sessions")]
    pub out_dir: PathBuf,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List Magewell channels found by the SDK
    List,
    /// Read-only survey: SDK state, ADV7842 registers and map dumps, BAR0
    Probe {
        /// Skip the full 256-byte ADV7842 map dumps
        #[arg(long)]
        no_dumps: bool,
    },
    /// Print signal status and lock-related registers (repeat every N seconds with --watch)
    Status {
        #[arg(long, default_value_t = 0.0)]
        watch: f64,
    },
    /// ADV7842 register access (hidden KS property 0x40000005)
    I2c {
        #[command(subcommand)]
        op: I2cOp,
    },
    /// Read FPGA BAR0 registers (hidden KS property 0x40000006; known-safe offsets only)
    Bar0 {
        #[arg(value_parser = |s: &str| parse_u32(s).map_err(|e| e.to_string()))]
        off: u32,
        #[arg(default_value_t = 1)]
        count: u32,
    },
    /// Capture raw ADC samples (no external sync, gapless), then put the card back to normal
    Capture(CaptureArgs),
    /// Measure where each input settles with nothing playing and store centres/gains for it
    Calibrate(CalibrateArgs),
    /// Put the card back to normal after an interrupted capture
    Reset,
    /// Write each channel of a capture as a continuous sample file for vhs-decode (any OS)
    Export(ExportArgs),
    /// List the capture options accepted by -o key=value
    Options,
}

#[derive(Subcommand)]
pub enum I2cOp {
    /// Read registers: i2c read cp 0xbf [count]
    Read {
        map: String,
        #[arg(value_parser = |s: &str| parse_u8(s).map_err(|e| e.to_string()))]
        reg: u8,
        #[arg(default_value_t = 1)]
        count: usize,
    },
    /// Write registers: i2c write cp 0xbf 0x01 [more bytes...]
    Write {
        map: String,
        #[arg(value_parser = |s: &str| parse_u8(s).map_err(|e| e.to_string()))]
        reg: u8,
        #[arg(value_parser = |s: &str| parse_u8(s).map_err(|e| e.to_string()), required = true)]
        vals: Vec<u8>,
    },
    /// Dump whole maps (default io cp afe hdmi) to the session directory
    Dump { maps: Vec<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ClampArg {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AudioClockArg {
    /// Resampled onto the RF sample clock: 48000 samples per 40 M RF samples (at 40 MSPS),
    /// sample 0 at RF sample 0
    Rf,
    /// The card's own audio clock (free-running: drifts ~65 ppm against the RF)
    Card,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CapFormat {
    /// Three channels, 10 bits each (ADC0/1/2)
    Rgb10,
    /// ADC0 only, 16-bit words (10 significant bits); for rates above 100 MSPS
    Y16,
}

/// Input setup shared by `capture` and `calibrate`.
#[derive(Args, Clone, Debug)]
pub struct SetupArgs {
    /// Sample rate, e.g. 40M. The line width is chosen automatically: the widest line
    /// (<= 2048) that gives this rate exactly, or as close as the crystal allows
    #[arg(long, value_parser = |s: &str| parse_hz(s).map_err(|e| e.to_string()), default_value = "40M")]
    pub rate: f64,
    #[arg(long, value_enum, default_value_t = CapFormat::Rgb10)]
    pub format: CapFormat,
    /// ADV7842 analog clamp, one switch for all ADCs. off: clean on every channel, input rests ~22 % up the
    /// ADC range; on: only ADC0 (Component Y) is clean, trough held near the bottom; auto: on when ADC0 is
    /// the only active ADC (docs/13)
    #[arg(long, value_enum, default_value_t = ClampArg::Off)]
    pub clamp: ClampArg,
    /// Input of ADC0 (RG10 bits 10-19; the Y16 channel): comp-y, yc-y, cvbs, vga-g, ainN (1 4 7 9 10 11)
    /// or off [default: comp-y; vga-g on the DVI card]
    #[arg(long)]
    pub adc0: Option<String>,
    /// Input of ADC1 (RG10 bits 0-9): comp-y, comp-pr, cvbs, vga-r, ainN (2 4 5 8 11) or off
    /// [default: comp-pr; vga-r on the DVI card]
    #[arg(long)]
    pub adc1: Option<String>,
    /// Input of ADC2 (RG10 bits 20-29): comp-y, comp-pr, comp-pb, yc-c, vga-b, ainN (3 4 5 6 9 12) or off
    /// [default: comp-pb; vga-b on the DVI card]
    #[arg(long)]
    pub adc2: Option<String>,
    /// Samples per line instead of the automatic choice (<= 2048)
    #[arg(long)]
    pub width: Option<u16>,
    /// Lines per frame (2..=2048; 2048 = largest frame, fewest wake-ups)
    #[arg(long, default_value_t = 2048)]
    pub lines: u16,
    /// Any capture option as key=value (repeatable; `mwadc options` lists them), e.g. -o aa=on -o aa_bw=41
    /// -o gain0=1.5 -o calibration=off
    #[arg(short = 'o', long = "opt")]
    pub opts: Vec<String>,
    /// Extra ADV7842 writes after the recipe: map:reg=val or map:reg=val/mask (repeatable),
    /// e.g. --set cp:0x73=0xd0 (CP map writes go to the real CP)
    #[arg(long = "set")]
    pub extra: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct CaptureArgs {
    #[command(flatten)]
    pub setup: SetupArgs,
    /// Seconds to capture
    #[arg(long, default_value_t = 1.0, conflicts_with = "frames")]
    pub seconds: f64,
    /// Frames to capture instead of --seconds
    #[arg(long)]
    pub frames: Option<u64>,
    /// Wait this many seconds after arming before recording (clamp-off inputs settle with τ ≈ 25-30 s)
    #[arg(long, default_value_t = 0.0)]
    pub settle: f64,
    /// Also record the card's analog audio input (audio.wav + audio.csv)
    #[arg(long)]
    pub audio: bool,
    /// Clock of audio.wav: locked to the RF samples, or the card's own
    #[arg(long, value_enum, default_value = "rf", requires = "audio")]
    pub audio_clock: AudioClockArg,
    /// With --audio-clock rf, also write the unlocked audio as audio_native.wav
    #[arg(long, requires = "audio")]
    pub audio_native: bool,
    /// Stream without writing frames.raw (frames.csv and audio are still written): long timing runs
    #[arg(long)]
    pub no_raw: bool,
    /// Frame buffers between the card and the file writer
    #[arg(long, default_value_t = 6)]
    pub pool: usize,
}

#[derive(Args, Clone, Debug)]
pub struct CalibrateArgs {
    #[command(flatten)]
    pub setup: SetupArgs,
    /// Measurement length; with the clamp off >= 30 s lets the fit see the settling curve
    #[arg(long, default_value_t = 40.0)]
    pub seconds: f64,
    /// Measure and print only; do not store the result
    #[arg(long)]
    pub no_save: bool,
    /// Calibration file (default: $MWADC_CALIBRATION or %APPDATA%\mwadc\calibration.json)
    #[arg(long)]
    pub store: Option<PathBuf>,
}

#[derive(Args, Clone, Debug)]
pub struct ExportArgs {
    /// Session directory containing capture.json and frames.raw
    pub dir: PathBuf,
    /// Sample format: s16 (signed 16-bit, what vhs-decode reads) or u16
    #[arg(long, default_value = "s16")]
    pub kind: String,
    /// Export at most this many frames (default: all)
    #[arg(long)]
    pub max_frames: Option<usize>,
}

impl SetupArgs {
    /// The capture options; inputs not given default to the card's (`model`; Component when unknown).
    pub fn config(&self, model: Option<Board>) -> Result<CaptureConfig> {
        let mut c = model.map(CaptureConfig::for_board).unwrap_or_default();
        c.set("rate", &self.rate.to_string())?;
        c.set("format", if self.format == CapFormat::Y16 { "y16" } else { "rgb10" })?;
        c.set(
            "clamp",
            match self.clamp {
                ClampArg::Auto => "auto",
                ClampArg::On => "on",
                ClampArg::Off => "off",
            },
        )?;
        for (k, v) in [("adc0", &self.adc0), ("adc1", &self.adc1), ("adc2", &self.adc2)] {
            if let Some(v) = v {
                c.set(k, v)?;
            }
        }
        c.width = self.width;
        c.lines = self.lines;
        for kv in &self.opts {
            let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("-o wants key=value, got '{kv}'"))?;
            c.set(k, v)?;
        }
        for s in &self.extra {
            c.set("set", s)?;
        }
        c.validate()?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn setup_to_config() {
        let cli = Cli::try_parse_from(["mwadc", "capture", "--adc1", "off", "-o", "aa=on", "--set", "cp:0xbf=1"]).unwrap();
        let Cmd::Capture(a) = cli.cmd else { panic!() };
        let c = a.setup.config(None).unwrap();
        assert_eq!(c.adc, [4, 0, 6]);
        assert_eq!(a.setup.config(Some(Board::Dvi)).unwrap().adc, [1, 0, 3]);
        assert_eq!(c.aa_enable, [true; 3]);
        assert_eq!(c.extra.len(), 1);
    }
}
