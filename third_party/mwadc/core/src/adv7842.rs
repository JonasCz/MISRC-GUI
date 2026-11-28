//! ADV7842 register maps and the recipe that turns the CP into a free-running raw three-channel
//! digitizer (docs 03, 11). Register positions are from UG-214/UG-206; values marked "measured" were
//! established on the card (docs 11, 13).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::parse::RegWrite;

/// Where the real CP map goes while the EDID RAM stands in for it at 0x44 (no NACKs on this bus,
/// so free addresses cannot be probed; 0x46 is no ADV7842 default and worked on the cards).
pub const CP_MOVED: u8 = 0x46;
/// IO-map registers holding the EDID and CP map addresses.
pub const IO_EDID_ADDR: u8 = 0xFA;
pub const IO_CP_ADDR: u8 = 0xFD;

/// ADV7842 I2C maps, at the 8-bit addresses the Magewell lib programs (ADI defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Map {
    Io,
    Cp,
    Afe,
    Sdp,
    SdpIo,
    Avlink,
    Cec,
    Infoframe,
    Repeater,
    Edid,
    Hdmi,
    Vdp,
    /// The real CP map at its capture-time address (`CP_MOVED`), while the EDID RAM stands in at 0x44.
    CpMoved,
}

impl Map {
    pub const ALL: [Map; 12] = [
        Map::Io,
        Map::Cp,
        Map::Afe,
        Map::Sdp,
        Map::SdpIo,
        Map::Avlink,
        Map::Cec,
        Map::Infoframe,
        Map::Repeater,
        Map::Edid,
        Map::Hdmi,
        Map::Vdp,
    ];

    /// 8-bit (write) address, as used by the Magewell I2C ioctl / KS property.
    pub fn addr(self) -> u8 {
        match self {
            Map::Io => 0x40,
            Map::Cp => 0x44,
            Map::Afe => 0x4C,
            Map::Sdp => 0x90,
            Map::SdpIo => 0x94,
            Map::Avlink => 0x84,
            Map::Cec => 0x80,
            Map::Infoframe => 0x7C,
            Map::Repeater => 0x64,
            Map::Edid => 0x6C,
            Map::Hdmi => 0x68,
            Map::Vdp => 0x48,
            Map::CpMoved => CP_MOVED,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Map::Io => "io",
            Map::Cp => "cp",
            Map::Afe => "afe",
            Map::Sdp => "sdp",
            Map::SdpIo => "sdpio",
            Map::Avlink => "avlink",
            Map::Cec => "cec",
            Map::Infoframe => "infoframe",
            Map::Repeater => "repeater",
            Map::Edid => "edid",
            Map::Hdmi => "hdmi",
            Map::Vdp => "vdp",
            Map::CpMoved => "cp@moved",
        }
    }

    /// IO-map register holding this map's programmed address (doc 07 §2).
    pub fn io_addr_reg(self) -> Option<u8> {
        match self {
            Map::Io | Map::CpMoved => None,
            Map::Sdp => Some(0xF1),
            Map::SdpIo => Some(0xF2),
            Map::Avlink => Some(0xF3),
            Map::Cec => Some(0xF4),
            Map::Infoframe => Some(0xF5),
            Map::Afe => Some(0xF8),
            Map::Repeater => Some(0xF9),
            Map::Edid => Some(0xFA),
            Map::Hdmi => Some(0xFB),
            Map::Cp => Some(0xFD),
            Map::Vdp => Some(0xFE),
        }
    }

    pub fn from_addr(dev8: u8) -> Option<Map> {
        Map::ALL.into_iter().find(|m| m.addr() == dev8)
    }
}

/// A map name (`cp`) or a raw 8-bit device address (`0x44`).
pub fn parse_dev(s: &str) -> Result<u8> {
    let l = s.to_ascii_lowercase();
    if let Some(m) = Map::ALL.into_iter().find(|m| m.name() == l) {
        return Ok(m.addr());
    }
    crate::parse::parse_u8(s).with_context(|| format!("unknown I2C map/address '{s}'"))
}

pub fn dev_name(dev8: u8) -> String {
    match Map::from_addr(dev8) {
        Some(m) => m.name().to_string(),
        None => format!("{dev8:#04x}"),
    }
}

/// Pins each ADC's input switch can reach (UG-214 Table 6; the switch code is the Ain number).
/// Confirmed on the card for Ain4 (all three) and Ain6 (ADC2 only).
pub const REACH: [&[u8]; 3] = [&[1, 4, 7, 9, 10, 11], &[2, 4, 5, 8, 11], &[3, 4, 5, 6, 9, 12]];

/// Analog input pin for an ADC, by breakout jack (`board::JACKS`) or `ainN`; 0 = off. Jacks follow ADI's
/// reference wiring, which the driver uses: VGA G/R/B = Ain1/2/3, Component Y/Pr/Pb = Ain4/5/6,
/// S-video Y/C = Ain10/12, CVBS = Ain11.
pub fn parse_ain(s: &str) -> Result<u8> {
    let l = s.to_ascii_lowercase();
    if let Some(j) = crate::board::JACKS.iter().find(|j| j.name == l) {
        return Ok(j.pin);
    }
    Ok(match l.as_str() {
        "off" | "none" => 0,
        "svideo-y" => 10,
        "svideo-c" => 12,
        "y" => 4,
        "pr" => 5,
        "pb" => 6,
        _ => {
            let num = l.strip_prefix("ain").with_context(|| format!("unknown ADC source '{s}'"))?;
            let n: u8 = num.parse().with_context(|| format!("bad Ain number in '{s}'"))?;
            if !(1..=12).contains(&n) {
                bail!("Ain number must be 1..12, got {n}");
            }
            n
        }
    })
}

/// Check that each ADC can reach its pin; names the ADCs that can when it cannot.
pub fn check_reach(adc: [u8; 3]) -> Result<()> {
    for (i, &pin) in adc.iter().enumerate() {
        if pin != 0 && !REACH[i].contains(&pin) {
            let able: Vec<String> = (0..3).filter(|&k| REACH[k].contains(&pin)).map(|k| format!("ADC{k}")).collect();
            bail!("ADC{i} cannot reach Ain{pin}; it can be routed to {}", if able.is_empty() { "no captured ADC".into() } else { able.join(", ") });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    Write { map: Map, reg: u8, val: u8 },
    /// Read-modify-write of the bits in `mask`.
    Rmw { map: Map, reg: u8, mask: u8, val: u8 },
    /// Consecutive registers in one I2C transfer. Needed where the chip latches a block on its last
    /// byte (CP offsets 0x77..0x7A: single-byte writes are ignored) or the order matters (PLL, gain).
    Seq { map: Map, reg: u8, vals: Vec<u8> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub op: Op,
    pub why: String,
}

fn step(op: Op, why: impl Into<String>) -> Step {
    Step { op, why: why.into() }
}

/// Measured data path (docs/14 §1, §3a), CP pre-gain bypassed, digital fine clamp manual (CP 0x6C[4] = 1):
/// with `x = adc10 − C/4` (C = the 12-bit clamp value CLMP_A/B/C, our "centre") and G = gain/256,
///   A:   out = clamp(OFF_A + G·x, 0, 1023)
///   B/C: out = clamp(2·(OFF_BC + G·x) − 1024, 0, 1022)   (even codes only)
/// The post-gain offsets are fixed (`OUT_OFFSET`) so that ADC level C/4 lands on output code
/// `OUT_MID` on every channel: out = OUT_MID + k·G·(adc10 − C/4), k = 1 (A) or 2 (B/C).
/// The gain acts on 12-bit data before the 12→10-bit cut, so G > 1 is real resolution.
/// ADC rails (adc10), measured with a clipping RF signal, the same on A/B/C.
pub const ADC_FLOOR: f64 = 2.0;
pub const ADC_TOP: f64 = 1023.0;
/// Output code the centre lands on.
pub const OUT_MID: f64 = 512.0;
/// CP offsets A/B/C (0x77..0x7A), fixed: A out = 512 + G·x, B/C out = 2·(768 + G·x) − 1024 = 512 + 2G·x.
pub const OUT_OFFSET: [u16; 3] = [512, 768, 768];

/// Default levels: the output is the raw ADC code on every channel (gain A 1.0, B/C 0.5 so B/C
/// have A's scale; centre = ADC code 512 lands on output code 512).
pub const GAIN_DEFAULT: [u16; 3] = [0x100, 0x080, 0x080];
/// Centre (CLMP, 12-bit ADC units: 4 per adc10 step), 0..4095.
pub const CENTRE_DEFAULT: [u16; 3] = [2048; 3];
pub const CENTRE_MAX: u16 = 4095;

/// Output code (unclipped) of ADC level `adc10` on ADC `adc` under these levels.
pub fn code_of_adc(adc: usize, adc10: f64, gain: u16, centre: u16) -> f64 {
    let k = if adc == 0 { 1.0 } else { 2.0 };
    OUT_MID + k * gain as f64 / 256.0 * (adc10 - centre as f64 / 4.0)
}

/// ADC level (adc10) behind an output code under these levels.
pub fn adc_of_code(adc: usize, code: f64, gain: u16, centre: u16) -> f64 {
    let k = if adc == 0 { 1.0 } else { 2.0 };
    centre as f64 / 4.0 + (code - OUT_MID) / (k * gain.max(1) as f64 / 256.0)
}

/// AFE 0x0C[4:0]: undocumented analog gain (≈ binary weighted, sensitivity ≈ 0.531 + 0.0151·code).
/// 0x1F = ADI default ("normal"); 0x00 ≈ 1.9× wider input range ("wide"), at ≈ 1.7× more input-referred noise.
pub const AFE_GAIN_NORMAL: u8 = 0x1F;
pub const AFE_GAIN_WIDE: u8 = 0x00;

/// AFE 0x11[1:0]: undocumented analog input offset, added before the analog gain and the same on every ADC
/// (docs/14). Field value -> nominal shift in ADC codes at AFE 0x0C = 0x1F (measured +118/+59/0/−60).
pub const INPUT_OFFSET_CODES: [i32; 4] = [120, 60, 0, -60];
/// +120: idle inputs (resting ≈ 390–415 without it) sit near mid-scale.
pub const INPUT_OFFSET_DEFAULT: u8 = 0;

/// AFE 0x11[1:0] field value for a nominal shift (+120, +60, 0, −60).
pub fn input_offset_field(codes: i32) -> Option<u8> {
    INPUT_OFFSET_CODES.iter().position(|&c| c == codes).map(|i| i as u8)
}

pub fn afe_gain_sensitivity(code: u8) -> f64 {
    0.531 + 0.0151 * (code & 0x1F) as f64
}

/// Analog front end and CP level settings: everything that may also change while the card is
/// capturing (CP writes then go to the moved CP map, see `FrontEnd::steps`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrontEnd {
    /// IO 0x30 bit 7 (VOLTAGE_CLAMP_EN; ADV7604 name, confirmed on the ADV7842 by measurement). One bit for all ADCs.
    pub clamp: bool,
    /// AA_FILTER_EN0..2 (AFE 0x05[2:0]).
    pub aa_enable: [bool; 3],
    /// Anti-alias response 0..15 = AA_FILT_HIGH_BW[1:0] * 4 + AA_FILT_PROG_BW[1:0] (UG-214 Table 10),
    /// shared by all ADCs. See `AA_FC_MHZ`.
    pub aa_bw: u8,
    /// CP manual gain A/B/C (ADC0/1/2), 2.8 fixed point: 0x100 = 1.0, max 0x3FF.
    pub gain: [u16; 3],
    /// Centre A/B/C (ADC0/1/2): CP manual clamp value, 12-bit ADC units, subtracted before the gain;
    /// that ADC level lands on output code `OUT_MID`.
    pub centre: [u16; 3],
    /// AFE 0x0C[4:0] analog gain (`AFE_GAIN_NORMAL` / `AFE_GAIN_WIDE`), shared by all ADCs.
    pub afe_gain: u8,
    /// AFE 0x11[1:0] analog input offset (`INPUT_OFFSET_CODES`), shared by all ADCs.
    pub input_offset: u8,
}

/// -3 dB corner of each anti-alias response index (UG-214 Table 10), MHz.
pub const AA_FC_MHZ: [u16; 16] = [10, 12, 14, 16, 27, 32, 36, 41, 59, 69, 80, 91, 95, 109, 126, 145];

/// Response index with the lowest corner at or above `mhz` (the widest when none is).
pub fn aa_index_for_mhz(mhz: f64) -> u8 {
    AA_FC_MHZ.iter().position(|&f| f as f64 >= mhz).unwrap_or(15) as u8
}

pub const GAIN_UNITY: u16 = 0x100;
pub const GAIN_MAX: u16 = 0x3FF;

impl Default for FrontEnd {
    fn default() -> Self {
        FrontEnd { clamp: false, aa_enable: [false; 3], aa_bw: 15, gain: GAIN_DEFAULT, centre: CENTRE_DEFAULT, afe_gain: AFE_GAIN_NORMAL, input_offset: INPUT_OFFSET_DEFAULT }
    }
}

impl FrontEnd {
    /// Register steps for these settings, for the CP map at `cp` (the default `Map::Cp` before the
    /// alias; `Map::CpMoved` while capturing).
    pub fn steps(&self, cp: Map) -> Vec<Step> {
        let mut v = Vec::new();
        let en = (self.aa_enable[0] as u8) | (self.aa_enable[1] as u8) << 1 | (self.aa_enable[2] as u8) << 2;
        let bw = self.aa_bw & 0x0F;
        let (high, prog) = (bw >> 2, bw & 3);
        v.push(step(Op::Rmw { map: Map::Afe, reg: 0x05, mask: 0x07, val: en }, format!("anti-alias filters ADC0-2: {:03b} (bit 0 = ADC0)", en)));
        v.push(step(Op::Rmw { map: Map::Afe, reg: 0x06, mask: 0x20, val: (high >> 1) << 5 }, "AA_FILT_HIGH_BW[1]"));
        v.push(step(
            Op::Rmw { map: Map::Afe, reg: 0x07, mask: 0xE0, val: ((high & 1) << 7) | (prog << 5) },
            format!("AA_FILT_HIGH_BW[0], AA_FILT_PROG_BW = {prog}: response {}_{} ({} MHz)", high, prog, AA_FC_MHZ[bw as usize]),
        ));
        v.push(step(
            Op::Rmw { map: Map::Afe, reg: 0x0C, mask: 0x1F, val: self.afe_gain & 0x1F },
            format!("analog gain (undocumented AFE 0x0C[4:0]) {:#04x}: sensitivity {:.3}", self.afe_gain & 0x1F, afe_gain_sensitivity(self.afe_gain)),
        ));
        let io = self.input_offset & 3;
        v.push(step(
            Op::Rmw { map: Map::Afe, reg: 0x11, mask: 0x03, val: io },
            format!("analog input offset (undocumented AFE 0x11[1:0]) {io}: {:+} codes", INPUT_OFFSET_CODES[io as usize]),
        ));
        // Clamp, gain and offset values latch on their second byte; one block write keeps the three in step.
        let [ca, cb, cc] = self.centre.map(|c| c.min(CENTRE_MAX));
        v.push(step(
            Op::Seq { map: cp, reg: 0x6C, vals: clamp_bytes(ca, cb, cc) },
            format!("digital fine clamp manual (0x6C[4] = 1), centre A/B/C {:.2}/{:.2}/{:.2}", ca as f64 / 4.0, cb as f64 / 4.0, cc as f64 / 4.0),
        ));
        let [ga, gb, gc] = self.gain.map(|g| g.min(GAIN_MAX));
        v.push(step(Op::Seq { map: cp, reg: 0x73, vals: gain_bytes(ga, gb, gc) }, format!("manual gain A/B/C {:.3}/{:.3}/{:.3}", ga as f64 / 256.0, gb as f64 / 256.0, gc as f64 / 256.0)));
        let [oa, ob, oc] = OUT_OFFSET;
        v.push(step(Op::Seq { map: cp, reg: 0x77, vals: offset_bytes(oa, ob, oc) }, format!("CP offsets A/B/C {oa}/{ob}/{oc} (centre -> code 512)")));
        v.push(step(
            Op::Rmw { map: Map::Io, reg: 0x30, mask: 0x80, val: if self.clamp { 0x80 } else { 0 } },
            format!("analog voltage clamp {}", if self.clamp { "on" } else { "off" }),
        ));
        v
    }
}

/// The raw-capture recipe (applied while the CP map is still at its default address).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recipe {
    /// ADC0..2 input pins (see `parse_ain`).
    pub adc: [u8; 3],
    /// Free-run clock: LLC = pll_div x 28.63636 MHz / fr_ll, frame = lines lines.
    pub pll_div: u16,
    pub fr_ll: u16,
    pub lines: u16,
    pub fe: FrontEnd,
    /// User register writes, applied last.
    pub extra: Vec<RegWrite>,
}

impl Recipe {
    pub fn steps(&self) -> Vec<Step> {
        let mut v = Vec::new();
        let [a0, a1, a2] = self.adc;
        let mut pdn_mask = 0u8;
        for (i, a) in self.adc.iter().enumerate() {
            if *a != 0 {
                pdn_mask |= (1 << i) | (1 << (i + 4));
            }
        }
        v.push(step(Op::Rmw { map: Map::Afe, reg: 0x00, mask: pdn_mask, val: 0 }, "power up the used ADCs and their clocks"));
        v.push(step(Op::Write { map: Map::Afe, reg: 0x01, val: 0x06 }, "AFE 0x01 = 0x06, as the lib's component setup"));
        v.push(step(Op::Rmw { map: Map::Afe, reg: 0x02, mask: 0x80, val: 0x80 }, "ADC_SWITCH_MAN = 1 (manual ADC input mux)"));
        v.push(step(Op::Write { map: Map::Afe, reg: 0x03, val: (a0 << 4) | a1 }, format!("ADC0 <- Ain{a0}, ADC1 <- Ain{a1}")));
        v.push(step(Op::Rmw { map: Map::Afe, reg: 0x04, mask: 0xF0, val: a2 << 4 }, format!("ADC2 <- Ain{a2}")));

        v.push(step(Op::Write { map: Map::Io, reg: 0x00, val: 0x07 }, "VID_STD = 0x07 (auto graphics)"));
        v.push(step(Op::Write { map: Map::Io, reg: 0x01, val: 0x82 }, "ADC_HDMI_SIMULT_MODE = 1, PRIM_MODE = 2 (graphics on the ADCs)"));
        // ADI "Register Settings Recommendations", digitizer block (as mainline adv7842.c).
        for (map, reg, val, why) in [
            // Bits 6:5 = 1 is needed for 12-bit ADC data; bit 7 adds noise (docs/14 §3).
            (Map::Afe, 0x12, 0x63, "ADI recommended (digitizer): 12-bit ADC data, low noise"),
            // The graphics pre-gain (0x3E[2] with 0x40 = 0x5C, ×0.72) is purely digital and only shrinks the
            // ADC range to 734 codes; bypass it (ADI's own sheet also lists 44 3E 00). docs/14 §3.
            (Map::Cp, 0x3E, 0x00, "CP core pre-gain bypassed (gain = manual gain only)"),
            (Map::Cp, 0xC3, 0x39, "ADI recommended (graphics): CP coast control"),
        ] {
            v.push(step(Op::Write { map, reg, val }, why));
        }

        let (pll, fr_ll, lines) = (self.pll_div, self.fr_ll, self.lines);
        v.push(step(
            Op::Seq { map: Map::Io, reg: 0x16, vals: vec![0x80 | ((pll >> 8) & 0x1F) as u8, (pll & 0xFF) as u8] },
            format!("PLL_DIV_MAN_EN = 1, PLL_DIV_RATIO = {pll}"),
        ));
        v.push(step(
            Op::Rmw { map: Map::Cp, reg: 0x8F, mask: 0x07, val: ((fr_ll >> 8) & 0x07) as u8 },
            format!("CH1_FR_LL[10:8] (line = {fr_ll} crystal clocks)"),
        ));
        v.push(step(Op::Write { map: Map::Cp, reg: 0x90, val: (fr_ll & 0xFF) as u8 }, "CH1_FR_LL[7:0]"));
        v.push(step(
            Op::Seq { map: Map::Cp, reg: 0xAB, vals: vec![(lines >> 4) as u8, ((lines & 0x0F) << 4) as u8] },
            format!("CP_LCOUNT_MAX = {lines} lines per frame"),
        ));
        // The lib sets INTERLACED when it detected an interlaced signal on Component (e.g. composite on Y)
        // before we arm; an interlaced free run never completes the FPGA's progressive frame (docs/11 §3).
        v.push(step(Op::Rmw { map: Map::Cp, reg: 0x91, mask: 0x40, val: 0x00 }, "INTERLACED = 0 (progressive free run)"));
        v.push(step(Op::Rmw { map: Map::Cp, reg: 0xBF, mask: 0x07, val: 0x01 }, "CP_FORCE_FREERUN = 1, CP_DEF_COL_AUTO = 0"));

        v.push(step(Op::Write { map: Map::Io, reg: 0x02, val: 0x12 }, "RGB full range in, RGB out (CSC pass-through)"));
        v.push(step(Op::Rmw { map: Map::Io, reg: 0x05, mask: 0x08, val: 0x00 }, "DATA_BLANK_EN = 0 (real samples during blanking)"));
        v.push(step(Op::Rmw { map: Map::Cp, reg: 0xBD, mask: 0x10, val: 0x10 }, "DPP_BYPASS_EN = 1"));
        v.extend(self.fe.steps(Map::Cp));
        v.extend(self.extra.iter().map(|w| step(w.op(), "user register write")));
        v
    }
}

impl RegWrite {
    pub fn op(&self) -> Op {
        if self.mask == 0xFF {
            Op::Write { map: self.map, reg: self.reg, val: self.val }
        } else {
            Op::Rmw { map: self.map, reg: self.reg, mask: self.mask, val: self.val }
        }
    }
}

/// CP 0x73..0x76, same packing as mainline `adv7842_set_gain` (0x100 = 1.0).
pub fn gain_bytes(a: u16, b: u16, c: u16) -> Vec<u8> {
    vec![
        0x80 | 0x40 | ((a & 0x3F0) >> 4) as u8,
        (((a & 0x00F) << 4) | ((b & 0x3C0) >> 6)) as u8,
        (((b & 0x03F) << 2) | ((c & 0x300) >> 8)) as u8,
        (c & 0x0FF) as u8,
    ]
}

/// CP 0x6C..0x70: CLMP_A_MAN = CLMP_BC_MAN = 1, not frozen, bit 4 = 1 (reserved, default 1: without it
/// the manual values are ignored and the CP subtracts ≈ 1600, docs/14 §3a), CLMP_A/B/C 12 bits each.
pub fn clamp_bytes(a: u16, b: u16, c: u16) -> Vec<u8> {
    vec![0xD0 | ((a >> 8) & 0x0F) as u8, (a & 0xFF) as u8, ((b >> 4) & 0xFF) as u8, (((b & 0x0F) << 4) | ((c >> 8) & 0x0F)) as u8, (c & 0xFF) as u8]
}

/// CP 0x77..0x7A, same packing as mainline `adv76xx_set_offset`; CP_PREC (0x77[7:6]) = 3 as the lib leaves it.
pub fn offset_bytes(a: u16, b: u16, c: u16) -> Vec<u8> {
    vec![
        0xC0 | ((a & 0x3F0) >> 4) as u8,
        (((a & 0x00F) << 4) | ((b & 0x3C0) >> 6)) as u8,
        (((b & 0x03F) << 2) | ((c & 0x300) >> 8)) as u8,
        (c & 0x0FF) as u8,
    ]
}

/// Registers worth printing in probe/status output, with a short meaning.
pub const WATCH: &[(Map, u8, &str)] = &[
    (Map::Io, 0x00, "VID_STD"),
    (Map::Io, 0x01, "SIMULT[7] / PRIM_MODE[3:0]"),
    (Map::Io, 0x02, "INP_COLOR_SPACE[7:4] / RGB_OUT[1]"),
    (Map::Io, 0x03, "OP_FORMAT_SEL (0x42 = 36-bit 4:4:4 SDR)"),
    (Map::Io, 0x05, "DATA_BLANK_EN[3]"),
    (Map::Io, 0x07, "SYNC_CH1 HS/VS select (0x7F = HDMI)"),
    (Map::Io, 0x16, "PLL_DIV_MAN_EN[7] / RATIO[12:8]"),
    (Map::Io, 0x17, "PLL_DIV_RATIO[7:0]"),
    (Map::Io, 0x30, "VOLTAGE_CLAMP_EN[7] (measured) / CURRENT_CLAMP_EN[3] / PIXBUS_MSB_TO_LSB_REORDER[4]"),
    (Map::Io, 0x42, "CP lock status (raw)"),
    (Map::Afe, 0x00, "PDN_ADC_CLKx[7:4] / PDN_ADCx[3:0]"),
    (Map::Afe, 0x01, ""),
    (Map::Afe, 0x02, "ADC_SWITCH_MAN[7] / AIN_SEL[2:0]"),
    (Map::Afe, 0x03, "ADC0_SW[7:4] / ADC1_SW[3:0]"),
    (Map::Afe, 0x04, "ADC2_SW[7:4] / ADC3_SW[3:0]"),
    (Map::Afe, 0x05, "AA_FILTER_EN[3:0]"),
    (Map::Afe, 0x0C, "undocumented: ADC range / CP core gain (ADI: 0x1F digitizer, 0x0D HDMI)"),
    (Map::Afe, 0x11, "undocumented: analog input offset[1:0] (10 = 0, 00 = +120 codes)"),
    (Map::Afe, 0x12, "undocumented: ADC noise shaping (ADI: 0x63 digitizer, 0xFB HDMI)"),
    (Map::Cp, 0x6C, "CLMP_A_MAN[7] BC_MAN[6] FREEZE[5] manual values used[4] A[11:8]"),
    (Map::Cp, 0x73, "GAIN_MAN[7] AGC_MAN[6] A_GAIN[9:4]"),
    (Map::Cp, 0x3E, "CP core pre-gain (ADI: 0x04 graphics, 0x00 HDMI)"),
    (Map::Cp, 0x40, "CP core pre-gain (ADI: 0x5C graphics)"),
    (Map::Cp, 0x77, "CP_PREC[7:6] / offset A[9:4]"),
    (Map::Cp, 0x8F, "CH1_FR_LL[10:8]"),
    (Map::Cp, 0x90, "CH1_FR_LL[7:0]"),
    (Map::Cp, 0xAB, "CP_LCOUNT_MAX[11:4]"),
    (Map::Cp, 0xAC, "CP_LCOUNT_MAX[3:0]"),
    (Map::Cp, 0xB1, "CH1_STDI_DVALID[7]"),
    (Map::Cp, 0xB8, "CH1_FCL[12:8]"),
    (Map::Cp, 0xB9, "CH1_FCL[7:0]"),
    (Map::Cp, 0xBD, "DPP_BYPASS_EN[4]"),
    (Map::Cp, 0xC3, "CP coast control (ADI: 0x39 graphics, 0x33 component/HDMI)"),
    (Map::Cp, 0xC5, "CLAMP_AVG_FCTR[7:6] (digital clamp IIR)"),
    (Map::Cp, 0xC8, "CP_DFC_POS_START[7:0] digital fine clamp window start (lib writes per timing)"),
    (Map::Cp, 0xBF, "DEF_COL_MAN[2] DEF_COL_AUTO[1] FORCE_FREERUN[0]"),
    (Map::Cp, 0xFF, "CP_FREE_RUN[4] (status)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_unity_matches_mainline() {
        assert_eq!(gain_bytes(0x100, 0x100, 0x100), vec![0xD0, 0x04, 0x01, 0x00]);
    }

    #[test]
    fn default_levels_are_raw_adc() {
        for adc in 0..3 {
            for x in [ADC_FLOOR, 300.0, ADC_TOP] {
                let c = code_of_adc(adc, x, GAIN_DEFAULT[adc], CENTRE_DEFAULT[adc]);
                assert!((c - x).abs() < 1e-9, "ADC{adc} {x} -> {c}");
                assert!((adc_of_code(adc, c, GAIN_DEFAULT[adc], CENTRE_DEFAULT[adc]) - x).abs() < 1e-9);
            }
        }
        assert_eq!(clamp_bytes(0x123, 0x456, 0x789), vec![0xD1, 0x23, 0x45, 0x67, 0x89]);
    }

    #[test]
    fn reach() {
        assert!(check_reach([4, 5, 6]).is_ok(), "component Y/Pr/Pb");
        assert!(check_reach([10, 11, 12]).is_ok(), "S-video Y, CVBS, S-video C");
        assert!(check_reach([6, 0, 0]).is_err(), "Pb only on ADC2");
        assert!(check_reach([0, 0, 0]).is_ok());
    }

    #[test]
    fn ain_names() {
        assert_eq!(parse_ain("cvbs").unwrap(), 11);
        assert_eq!(parse_ain("ain3").unwrap(), 3);
        assert!(parse_ain("ain13").is_err());
    }

    #[test]
    fn recipe_encodes_registers() {
        let r = Recipe { adc: [4, 5, 6], pll_div: 1936, fr_ll: 1386, lines: 2048, fe: FrontEnd::default(), extra: vec![] };
        let steps = r.steps();
        let find = |m: Map, reg: u8| {
            steps.iter().find_map(|s| match &s.op {
                Op::Write { map, reg: r, val } if *map == m && *r == reg => Some(vec![*val]),
                Op::Seq { map, reg: r, vals } if *map == m && *r == reg => Some(vals.clone()),
                _ => None,
            })
        };
        assert_eq!(find(Map::Afe, 0x03), Some(vec![0x45]));
        assert_eq!(find(Map::Io, 0x16), Some(vec![0x87, 0x90]));
        assert_eq!(find(Map::Cp, 0x90), Some(vec![0x6A]));
        assert_eq!(find(Map::Cp, 0xAB), Some(vec![0x80, 0x00]));
        // default levels: output = raw ADC code (docs/14 §3a)
        assert_eq!(find(Map::Cp, 0x6C), Some(vec![0xD8, 0x00, 0x80, 0x08, 0x00]), "manual clamp, centre 512 on A/B/C");
        assert_eq!(find(Map::Cp, 0x77), Some(vec![0xE0, 0x0C, 0x03, 0x00]), "offsets 512/768/768");
        assert_eq!(find(Map::Cp, 0x73), Some(vec![0xD0, 0x02, 0x00, 0x80]));
        assert_eq!(find(Map::Cp, 0x3E), Some(vec![0x00]), "pre-gain bypassed");
    }
}
