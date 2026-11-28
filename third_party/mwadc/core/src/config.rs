//! Capture configuration, shared by the CLI and the C API. Every option can be set from text with
//! `CaptureConfig::set(key, value)`, so new options need no C ABI change.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::adv7842::{self, aa_index_for_mhz, FrontEnd, AA_FC_MHZ, AFE_GAIN_NORMAL, AFE_GAIN_WIDE, CENTRE_DEFAULT, INPUT_OFFSET_CODES, INPUT_OFFSET_DEFAULT, CENTRE_MAX, GAIN_DEFAULT, GAIN_MAX};
use crate::fourcc::Format;
use crate::parse::{parse_bool, parse_hz, parse_num, parse_set, parse_u16, RegWrite};
use crate::timing::{self, ClockPlan};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Clamp {
    /// On when ADC0 is the only active ADC (bottom clamp, ~780 steps of swing), off otherwise
    /// (clean on every channel, ~444 steps). docs/13 §5.
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalibUse {
    /// Ignore stored calibration: default levels (output = raw ADC code).
    Off,
    /// Use the stored calibration for this card and input setup when there is one.
    Auto,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// Target sample rate; the widest line <= 2048 that hits it is chosen unless `width` is set.
    pub rate_hz: f64,
    pub width: Option<u16>,
    /// Lines per frame (2..=2048). Fewer lines = smaller frames = lower latency, more wake-ups.
    pub lines: u16,
    pub format: Format,
    /// ADC0..2 input pins (`adv7842::parse_ain`), 0 = off. The default suits the HDMI and AIO cards
    /// (Component); `for_board` picks one for any card.
    pub adc: [u8; 3],
    pub clamp: Clamp,
    pub aa_enable: [bool; 3],
    /// Anti-alias response index 0..15 (`adv7842::AA_FC_MHZ`).
    pub aa_bw: u8,
    /// AFE 0x0C[4:0] analog gain (shared by all ADCs): 0x1F normal, 0x00 wide input range.
    pub afe_gain: u8,
    /// AFE 0x11[1:0] analog input offset (shared by all ADCs), see `adv7842::INPUT_OFFSET_CODES`.
    pub input_offset: u8,
    /// CP gain per ADC (2.8 fixed point); `None` = from calibration, else the default.
    pub gain: [Option<u16>; 3],
    /// Centre per ADC (12-bit ADC units, see `FrontEnd::centre`); `None` = from calibration, else ADC code 512.
    pub centre: [Option<u16>; 3],
    pub calibration: CalibUse,
    /// Also capture the card's line-in audio.
    pub audio: bool,
    /// Extra register writes after the recipe.
    pub extra: Vec<RegWrite>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            rate_hz: 40e6,
            width: None,
            lines: 2048,
            format: Format::Rgb10,
            adc: [4, 5, 6],
            clamp: Clamp::Off,
            aa_enable: [false; 3],
            aa_bw: 15,
            afe_gain: AFE_GAIN_NORMAL,
            input_offset: INPUT_OFFSET_DEFAULT,
            gain: [None; 3],
            centre: [None; 3],
            calibration: CalibUse::Auto,
            audio: false,
            extra: Vec::new(),
        }
    }
}

/// Keys accepted by `CaptureConfig::set`, with a one-line description (for help texts and UIs).
pub const KEYS: &[(&str, &str)] = &[
    ("rate", "sample rate, e.g. 40M"),
    ("width", "samples per line (<= 2048), or auto"),
    ("lines", "lines per frame, 2..2048"),
    ("format", "rgb10 (ADC0/1/2) or y16 (ADC0 only, up to 160 MSPS)"),
    ("adc0", "input of ADC0: comp-y, yc-y, cvbs, vga-g, ainN or off"),
    ("adc1", "input of ADC1: comp-y, comp-pr, cvbs, vga-r, ainN or off"),
    ("adc2", "input of ADC2: comp-y, comp-pr, comp-pb, yc-c, vga-b, ainN or off"),
    ("clamp", "auto, on or off (one switch for all ADCs)"),
    ("aa", "anti-alias filter on all ADCs: on/off"),
    ("aa0", "anti-alias filter ADC0: on/off"),
    ("aa1", "anti-alias filter ADC1: on/off"),
    ("aa2", "anti-alias filter ADC2: on/off"),
    ("aa_bw", "anti-alias corner in MHz (10..145, rounded up to a hardware step)"),
    ("range", "analog input range: normal or wide (≈1.9x, more noise; undocumented AFE 0x0C)"),
    ("afe_gain", "raw analog gain AFE 0x0C[4:0], 0..31 (31 = normal)"),
    ("input_offset", "analog input offset before the ADCs, shared: +120 (default, idle near mid-scale), +60, 0 or -60 ADC codes"),
    ("gain0", "CP gain ADC0 (0..3.996, applied to 12-bit data), or auto"),
    ("gain1", "CP gain ADC1, or auto"),
    ("gain2", "CP gain ADC2, or auto"),
    ("centre0", "ADC0 level (ADC code 0..1023.75, in 0.25 steps) placed at output code 512; the gain zooms around it; or auto"),
    ("centre1", "ADC1 centre, or auto"),
    ("centre2", "ADC2 centre, or auto"),
    ("calibration", "auto (use stored calibration) or off"),
    ("audio", "also capture line-in audio: on/off"),
    ("set", "extra register write map:reg=val[/mask] (repeatable)"),
];

fn parse_gain(v: &str) -> Result<Option<u16>> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let g: f64 = v.trim().parse().with_context(|| format!("gain wants a number like 1.0, got '{v}'"))?;
    let raw = (g * 256.0).round();
    if !(0.0..=GAIN_MAX as f64).contains(&raw) {
        bail!("gain must be 0..{:.3}, got {v}", GAIN_MAX as f64 / 256.0);
    }
    Ok(Some(raw as u16))
}

fn parse_centre(v: &str) -> Result<Option<u16>> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let c: f64 = v.trim().parse().with_context(|| format!("centre wants an ADC code like 512 or 400.25, got '{v}'"))?;
    let raw = (c * 4.0).round();
    if !(0.0..=CENTRE_MAX as f64).contains(&raw) {
        bail!("centre must be 0..{:.2}, got {v}", CENTRE_MAX as f64 / 4.0);
    }
    Ok(Some(raw as u16))
}

impl CaptureConfig {
    /// Defaults with the board's default inputs (VGA R/G/B on the DVI card, Component elsewhere).
    pub fn for_board(board: crate::board::Board) -> CaptureConfig {
        CaptureConfig { adc: board.default_adc(), ..CaptureConfig::default() }
    }

    pub fn set(&mut self, key: &str, v: &str) -> Result<()> {
        let k = key.trim().to_ascii_lowercase();
        let idx = |k: &str| k.chars().last().and_then(|c| c.to_digit(10)).map(|d| d as usize).filter(|&d| d < 3);
        match k.as_str() {
            "rate" => self.rate_hz = parse_hz(v)?,
            "width" => self.width = if v.eq_ignore_ascii_case("auto") { None } else { Some(parse_u16(v)?) },
            "lines" => self.lines = parse_u16(v)?,
            "format" => {
                self.format = match v.to_ascii_lowercase().as_str() {
                    "rgb10" | "rg10" => Format::Rgb10,
                    "y16" => Format::Y16,
                    _ => bail!("format is rgb10 or y16, got '{v}'"),
                }
            }
            "adc0" | "adc1" | "adc2" => self.adc[idx(&k).unwrap()] = adv7842::parse_ain(v)?,
            "clamp" => {
                self.clamp = match v.to_ascii_lowercase().as_str() {
                    "auto" => Clamp::Auto,
                    _ => {
                        if parse_bool(v)? {
                            Clamp::On
                        } else {
                            Clamp::Off
                        }
                    }
                }
            }
            "aa" => self.aa_enable = [parse_bool(v)?; 3],
            "aa0" | "aa1" | "aa2" => self.aa_enable[idx(&k).unwrap()] = parse_bool(v)?,
            "aa_bw" => {
                let mhz = v.trim().trim_end_matches(['M', 'm']).trim();
                self.aa_bw = aa_index_for_mhz(mhz.parse().with_context(|| format!("aa_bw wants MHz, got '{v}'"))?)
            }
            "range" => {
                self.afe_gain = match v.to_ascii_lowercase().as_str() {
                    "normal" => AFE_GAIN_NORMAL,
                    "wide" => AFE_GAIN_WIDE,
                    _ => bail!("range is normal or wide, got '{v}'"),
                }
            }
            "afe_gain" | "analog_gain" => {
                let g = parse_num(v)?;
                if g > 31 {
                    bail!("afe_gain is 0..31");
                }
                self.afe_gain = g as u8
            }
            "input_offset" => {
                let c: i32 = v.trim().trim_start_matches('+').parse().with_context(|| format!("input_offset wants +120, +60, 0 or -60, got '{v}'"))?;
                self.input_offset = adv7842::input_offset_field(c).with_context(|| format!("input_offset is +120, +60, 0 or -60, got '{v}'"))?
            }
            "gain0" | "gain1" | "gain2" => self.gain[idx(&k).unwrap()] = parse_gain(v)?,
            "centre0" | "centre1" | "centre2" | "center0" | "center1" | "center2" => self.centre[idx(&k).unwrap()] = parse_centre(v)?,
            "calibration" => {
                self.calibration = match v.to_ascii_lowercase().as_str() {
                    "auto" | "on" => CalibUse::Auto,
                    "off" => CalibUse::Off,
                    _ => bail!("calibration is auto or off, got '{v}'"),
                }
            }
            "audio" => self.audio = parse_bool(v)?,
            "set" => self.extra.push(parse_set(v)?),
            "aa_bw_index" => {
                let i = parse_num(v)?;
                if i > 15 {
                    bail!("aa_bw_index is 0..15");
                }
                self.aa_bw = i as u8
            }
            _ => bail!("unknown option '{key}' (known: {})", KEYS.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")),
        }
        Ok(())
    }

    /// ADC input pins as captured: Y16 carries ADC0 only.
    pub fn active_adc(&self) -> [u8; 3] {
        match self.format {
            Format::Rgb10 => self.adc,
            Format::Y16 => [self.adc[0], 0, 0],
        }
    }

    pub fn validate(&self) -> Result<()> {
        adv7842::check_reach(self.adc)?;
        if self.active_adc().iter().all(|&a| a == 0) {
            bail!("no ADC has an input");
        }
        if !(2..=2048).contains(&self.lines) {
            bail!("lines must be 2..2048");
        }
        let max = self.max_rate_hz();
        if self.rate_hz > max * 1.0005 {
            bail!(
                "{:.3} MSPS is above the {:.0} MSPS this format sustains (docs/11 §4){}",
                self.rate_hz / 1e6,
                max / 1e6,
                if self.format == Format::Rgb10 { "; for one channel on ADC0 use format=y16 (up to 160 MSPS)" } else { "" }
            );
        }
        Ok(())
    }

    /// Highest gapless rate measured for the format (docs/11 §4): RG10 carries all three ADCs in
    /// 4 bytes per sample, so one, two or three channels alike reach 100 MSPS (120 skips frames: PCIe
    /// Gen2 x1); Y16 (ADC0 only, 2 bytes) reaches 160 MSPS.
    pub fn max_rate_hz(&self) -> f64 {
        match self.format {
            Format::Rgb10 => 100e6,
            Format::Y16 => 160e6,
        }
    }

    pub fn clamp_on(&self) -> bool {
        match self.clamp {
            Clamp::On => true,
            Clamp::Off => false,
            Clamp::Auto => {
                let a = self.active_adc();
                a[0] != 0 && a[1] == 0 && a[2] == 0
            }
        }
    }

    pub fn plan(&self) -> Result<ClockPlan> {
        match self.width {
            Some(w) => timing::plan_freerun(w, self.lines, self.rate_hz),
            None => timing::plan_auto(self.lines, self.rate_hz),
        }
    }

    /// Front-end settings: explicit values, else the calibration's, else defaults.
    pub fn front_end(&self, cal: Option<&crate::calib::Levels>) -> FrontEnd {
        let mut fe = FrontEnd { clamp: self.clamp_on(), aa_enable: self.aa_enable, aa_bw: self.aa_bw, afe_gain: self.afe_gain, input_offset: self.input_offset, ..FrontEnd::default() };
        for i in 0..3 {
            fe.gain[i] = self.gain[i].or(cal.map(|c| c.gain[i])).unwrap_or(GAIN_DEFAULT[i]);
            fe.centre[i] = self.centre[i].or(cal.map(|c| c.centre[i])).unwrap_or(CENTRE_DEFAULT[i]);
        }
        fe
    }

    pub fn describe(&self) -> String {
        let pins: Vec<String> = self
            .active_adc()
            .iter()
            .map(|&p| match (p, crate::board::jack_of(p)) {
                (0, _) => "off".into(),
                (p, Some(j)) => format!("Ain{p} ({})", j.name),
                (p, None) => format!("Ain{p}"),
            })
            .collect();
        format!(
            "{} ADC0/1/2 = {}, clamp {}{}, AA {} ({} MHz), range {}, input offset {:+}",
            self.format.name(),
            pins.join("/"),
            if self.clamp_on() { "on" } else { "off" },
            if self.clamp == Clamp::Auto { " (auto)" } else { "" },
            self.aa_enable.iter().map(|&e| if e { '1' } else { '0' }).collect::<String>(),
            AA_FC_MHZ[self.aa_bw as usize & 15],
            match self.afe_gain {
                AFE_GAIN_NORMAL => "normal".to_string(),
                AFE_GAIN_WIDE => "wide".to_string(),
                g => format!("afe {g:#04x}"),
            },
            INPUT_OFFSET_CODES[self.input_offset as usize & 3]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        let mut c = CaptureConfig::default();
        c.set("rate", "28.63636M").unwrap();
        c.set("adc1", "off").unwrap();
        c.set("adc2", "off").unwrap();
        c.set("clamp", "auto").unwrap();
        assert!(c.clamp_on(), "auto clamp with ADC0 alone");
        c.set("adc2", "comp-pb").unwrap();
        assert!(!c.clamp_on(), "auto clamp off with two ADCs");
        c.set("aa_bw", "40").unwrap();
        assert_eq!(c.aa_bw, 7);
        c.set("input_offset", "+60").unwrap();
        assert_eq!(c.input_offset, 1);
        assert!(c.set("input_offset", "50").is_err());
        c.set("gain0", "2").unwrap();
        assert_eq!(c.gain[0], Some(0x200));
        c.set("centre1", "400.25").unwrap();
        assert_eq!(c.centre[1], Some(1601));
        assert!(c.set("centre1", "1024").is_err());
        c.set("gain0", "auto").unwrap();
        assert_eq!(c.gain[0], None);
        assert!(c.set("adc0", "comp-pb").and_then(|_| c.validate()).is_err());
        assert!(c.set("nope", "1").is_err());
        for (k, _) in KEYS {
            assert!(!matches!(CaptureConfig::default().set(k, "x"), Err(e) if e.to_string().starts_with("unknown option")), "{k}");
        }
    }
}
