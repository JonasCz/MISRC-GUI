//! Map aliasing (docs/11 §3): fake the STDI readbacks the Magewell lib uses to decide
//! "signal present" for Component, by moving the ADV7842 CP map to another I2C address and
//! putting the EDID map (plain RAM) at the CP's old address.
//!
//! The lib reads exactly these CP-map registers in `_GetCPSignalStatus` (decompiled, driver
//! 1.3.4458): 0xB1, 0xB5, 0xB8, 0xB9, 0xB3, 0xB4. This module encodes values for them and
//! decodes them the way the lib does, so a mismatch shows up in tests instead of on hardware.

use anyhow::{bail, Context, Result};

/// `CH1_FCL` unit: 256 crystal clocks. The lib turns it into 100 ns units with
/// `(fcl * 2_560_000_000 + 0xDA79B0) / 0x1B4F360` (0x1B4F360 = 28_636_000).
const FCL_NUM: u64 = 2_560_000_000;
const FCL_DEN: u64 = 28_636_000;
const FCL_ROUND: u64 = 0xDA_79B0;

/// What the lib concludes from the six fake registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub interlaced: bool,
    pub sync_type: u8,
    pub hs_pol: bool,
    pub vs_pol: bool,
    /// Frame duration in 100 ns units (what `MWCAP_VIDEO_SYNC_INFO.dwFrameDuration` carries).
    pub frame_duration: u32,
    pub vs_lines: u8,
    pub frame_lines: u16,
}

/// Mirror of the lib's reads. `None` = the lib sees "no signal" (B1 or B5 bit 7 clear).
pub fn decode(b1: u8, b3: u8, b4: u8, b5: u8, b8: u8, b9: u8) -> Option<Decoded> {
    if b1 & 0x80 == 0 || b5 & 0x80 == 0 {
        return None;
    }
    let interlaced = b1 & 0x40 != 0;
    let fcl = ((b8 as u64 & 0x1F) << 8) | b9 as u64;
    let mut dur = (fcl * FCL_NUM + FCL_ROUND) / FCL_DEN;
    let mut lines = (((b3 & 7) as u16) << 8 | b4 as u16) + 1;
    if interlaced {
        dur *= 2;
        lines = lines * 2 + 1;
    }
    let sync_type = match b5 & 3 {
        1 => 1,
        2 => 2,
        _ => 4,
    };
    Some(Decoded {
        interlaced,
        sync_type,
        hs_pol: b5 & 0x18 == 0x18,
        vs_pol: b5 & 0x60 == 0x60,
        frame_duration: dur as u32,
        vs_lines: b3 >> 3,
        frame_lines: lines,
    })
}

/// A progressive HS/VS signal to present to the lib.
#[derive(Clone, Copy, Debug)]
pub struct FakeSync {
    pub lines: u16,
    pub frame_hz: f64,
    pub vs_lines: u8,
}

impl FakeSync {
    pub fn new(lines: u16, frame_hz: f64) -> Result<FakeSync> {
        let f = FakeSync { lines, frame_hz, vs_lines: 3 };
        f.regs().map(|_| f)
    }

    /// `CH1_FCL` for the frame rate: frame duration in units of 256 crystal clocks.
    fn fcl(&self) -> Result<u16> {
        let dur = 1e7 / self.frame_hz; // 100 ns units
        let fcl = (dur * FCL_DEN as f64 / FCL_NUM as f64).round();
        if !(1.0..=8191.0).contains(&fcl) {
            bail!("frame rate {:.3} Hz cannot be expressed in CH1_FCL (13 bits: 13.7 Hz .. ~110 kHz)", self.frame_hz);
        }
        Ok(fcl as u16)
    }

    /// (register, value) pairs for the CP-map addresses the lib reads.
    pub fn regs(&self) -> Result<[(u8, u8); 6]> {
        if self.lines < 2 || self.lines > 2048 {
            bail!("the lib reads an 11-bit frame line count: lines must be 2..=2048 (got {})", self.lines);
        }
        if self.vs_lines > 31 {
            bail!("vs_lines must be <= 31");
        }
        let fcl = self.fcl()?;
        let lcf = self.lines - 1;
        Ok([
            (0xB1, 0x80),                                          // CH1_STDI_DVALID, progressive
            (0xB3, (self.vs_lines << 3) | ((lcf >> 8) as u8 & 7)), // LCVS | LCF[10:8]
            (0xB4, lcf as u8),                                     // LCF[7:0]
            (0xB5, 0xF9),                                          // valid, HS+VS polarity, sync type 1 (HS/VS)
            (0xB8, (fcl >> 8) as u8),
            (0xB9, fcl as u8),
        ])
    }

    pub fn decoded(&self) -> Result<Decoded> {
        let r = self.regs()?;
        let g = |reg: u8| r.iter().find(|x| x.0 == reg).unwrap().1;
        decode(g(0xB1), g(0xB3), g(0xB4), g(0xB5), g(0xB8), g(0xB9)).context("fake registers decode to 'no signal'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_example_525_lines_59_94_hz() {
        let f = FakeSync::new(525, 59.94).unwrap();
        let r = f.regs().unwrap();
        assert_eq!(r, [(0xB1, 0x80), (0xB3, 0x1A), (0xB4, 0x0C), (0xB5, 0xF9), (0xB8, 0x07), (0xB9, 0x4A)]);
        let d = f.decoded().unwrap();
        assert_eq!(d.frame_lines, 525);
        assert_eq!(d.vs_lines, 3);
        assert_eq!(d.sync_type, 1);
        assert!(d.hs_pol && d.vs_pol && !d.interlaced);
        // 1866 * 2.56e9 / 28.636e6 = 166_833.. -> within one FCL step (89 units) of 59.94 Hz
        assert!((d.frame_duration as i64 - 166_833).abs() < 90, "{}", d.frame_duration);
    }

    #[test]
    fn roundtrip_various() {
        for (lines, hz) in [(480u16, 115.7f64), (1024, 60.0), (2048, 30.0), (2, 1000.0)] {
            let f = FakeSync::new(lines, hz).unwrap();
            let d = f.decoded().unwrap();
            assert_eq!(d.frame_lines, lines);
            let want = 1e7 / hz;
            assert!((d.frame_duration as f64 - want).abs() <= 90.0, "{lines} lines {hz} Hz -> {}", d.frame_duration);
        }
    }

    #[test]
    fn out_of_range() {
        assert!(FakeSync::new(2049, 60.0).is_err());
        assert!(FakeSync::new(480, 5.0).is_err(), "FCL would not fit in 13 bits");
        assert!(FakeSync::new(480, 1e6).is_err());
    }

    #[test]
    fn no_signal_cases() {
        assert!(decode(0x00, 0x1A, 0x0C, 0xF9, 0x07, 0x4A).is_none(), "B1 bit 7 clear");
        assert!(decode(0x80, 0x1A, 0x0C, 0x79, 0x07, 0x4A).is_none(), "B5 bit 7 clear");
    }

    #[test]
    fn interlaced_matches_lib_doubling() {
        let d = decode(0xC0, 0x1A, 0x0C, 0xF9, 0x07, 0x4A).unwrap();
        assert!(d.interlaced);
        assert_eq!(d.frame_lines, 525 * 2 + 1);
    }
}
