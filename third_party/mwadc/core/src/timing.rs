//! Sample-clock arithmetic for the ADV7842 CP in forced free-run (doc 03 §3).
//!
//! The line rate is 28.63636 MHz / CH1_FR_LL and F_LLC = PLL_DIV_RATIO x line rate. For gapless
//! capture the CP line must be exactly as wide as the capture window, so PLL_DIV_RATIO = width
//! and the rate is width x 28.63636 MHz / CH1_FR_LL.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const XTAL_HZ: f64 = 28_636_360.0;

pub const FR_LL_MAX: u16 = 2047;
/// Widest gapless line: the lib's active-width limit (cx = cxTotal <= 2048).
pub const WIDTH_MAX: u16 = 2048;
pub const LLC_MIN_HZ: f64 = 12.825e6;
pub const LLC_MAX_HZ: f64 = 170e6;
/// The lib refuses custom timings above this pixel rate.
pub const LIB_PIXEL_LIMIT_HZ: f64 = 166e6;
/// CP PLL HS range assumed safe (graphics modes up to ~UXGA@85); 19.5-73 kHz used on hardware.
pub const HS_MIN_HZ: f64 = 10e3;
pub const HS_MAX_HZ: f64 = 110e3;
/// The fake STDI frame duration (CH1_FCL, 13 bits of 256 crystal clocks) covers frames up to ~73 ms.
pub const FAKE_HZ_MIN: f64 = 13.7;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClockPlan {
    pub pll_div: u16,
    pub fr_ll: u16,
    pub lines: u16,
    pub sample_rate_hz: f64,
    pub line_rate_hz: f64,
    pub frame_rate_hz: f64,
    pub target_hz: f64,
    pub error_ppm: f64,
}

impl ClockPlan {
    fn new(pll_div: u16, fr_ll: u16, lines: u16, target_hz: f64) -> Self {
        let line = XTAL_HZ / fr_ll as f64;
        let rate = pll_div as f64 * line;
        ClockPlan {
            pll_div,
            fr_ll,
            lines,
            sample_rate_hz: rate,
            line_rate_hz: line,
            frame_rate_hz: line / lines.max(1) as f64,
            target_hz,
            error_ppm: (rate - target_hz) / target_hz * 1e6,
        }
    }

    /// Frame rate to report in the fake STDI readback: the real one if CH1_FCL can hold it,
    /// else just above the minimum (the FPGA follows the real syncs; the lib only matches timings).
    pub fn fake_hz(&self) -> f64 {
        if self.frame_rate_hz >= FAKE_HZ_MIN {
            self.frame_rate_hz
        } else {
            14.0
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{}x{} samples, CH1_FR_LL={} -> {:.6} MSPS (target {:.6}, {:+.2} ppm), line {:.3} kHz, frame {:.3} Hz",
            self.pll_div,
            self.lines,
            self.fr_ll,
            self.sample_rate_hz / 1e6,
            self.target_hz / 1e6,
            self.error_ppm,
            self.line_rate_hz / 1e3,
            self.frame_rate_hz
        )
    }
}

fn check(p: &ClockPlan) -> Result<()> {
    if !(LLC_MIN_HZ..=LLC_MAX_HZ).contains(&p.sample_rate_hz) {
        bail!("{:.3} MSPS is outside the ADV7842's 12.825-170 MHz", p.sample_rate_hz / 1e6);
    }
    if p.sample_rate_hz > LIB_PIXEL_LIMIT_HZ {
        bail!("{:.3} MSPS is above the driver's 166 MHz custom-timing limit", p.sample_rate_hz / 1e6);
    }
    if !(HS_MIN_HZ..=HS_MAX_HZ).contains(&p.line_rate_hz) {
        bail!("line rate {:.2} kHz is outside the CP's 10-110 kHz range", p.line_rate_hz / 1e3);
    }
    Ok(())
}

/// Free-run plan for a given line width (= capture width = PLL_DIV_RATIO).
pub fn plan_freerun(width: u16, lines: u16, target_hz: f64) -> Result<ClockPlan> {
    if width < 64 || width > WIDTH_MAX {
        bail!("width {width} must be 64..={WIDTH_MAX}");
    }
    if !(2..=2048).contains(&lines) {
        bail!("lines {lines} must be 2..=2048 (the fake STDI line count has 11 bits)");
    }
    let fr_ll = (width as f64 * XTAL_HZ / target_hz).round();
    if fr_ll < 1.0 || fr_ll > FR_LL_MAX as f64 {
        bail!("{:.3} MSPS with {width}-sample lines needs CH1_FR_LL={fr_ll}, outside 1..={FR_LL_MAX}", target_hz / 1e6);
    }
    let p = ClockPlan::new(width, fr_ll as u16, lines, target_hz);
    check(&p)?;
    Ok(p)
}

/// The widest line (largest frame, fewest wake-ups) that hits the target rate exactly, or as
/// closely as the crystal grid allows: among widths within 0.5 ppm of the best error, the widest.
pub fn plan_auto(lines: u16, target_hz: f64) -> Result<ClockPlan> {
    let mut best: Option<ClockPlan> = None;
    for w in (64..=WIDTH_MAX).rev() {
        let Ok(p) = plan_freerun(w, lines, target_hz) else { continue };
        match &best {
            Some(b) if p.error_ppm.abs() >= b.error_ppm.abs() - 0.5 => {}
            _ => best = Some(p),
        }
    }
    best.ok_or_else(|| anyhow::anyhow!("no free-run setting reaches {:.3} MSPS", target_hz / 1e6))
}

/// Pixel clock from SDK signal status (frame duration in 100 ns units).
pub fn pixel_clock_hz(cx_total: i32, cy_total: i32, frame_duration_100ns: u32, interlaced: bool) -> f64 {
    if frame_duration_100ns == 0 {
        return 0.0;
    }
    let lines_per_frame = cy_total as f64 * if interlaced { 0.5 } else { 1.0 };
    cx_total as f64 * lines_per_frame / (frame_duration_100ns as f64 * 1e-7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_40m_is_exact_and_wide() {
        let p = plan_auto(2048, 40e6).unwrap();
        assert_eq!((p.pll_div, p.fr_ll), (2024, 1449)); // 2024 = 23 x 88: exact on the 88/63 grid
        assert!(p.error_ppm.abs() < 0.5, "{}", p.error_ppm);
        assert!(p.fake_hz() >= FAKE_HZ_MIN);
    }

    #[test]
    fn auto_other_rates() {
        for r in [28.63636e6, 35.795454e6, 50e6, 64e6, 80e6, 100e6, 160e6] {
            let p = plan_auto(2048, r).unwrap();
            assert!(p.error_ppm.abs() < 200.0, "{} -> {}", r, p.describe());
            assert!(p.pll_div >= 1024, "{}", p.describe());
        }
    }

    #[test]
    fn hardware_plans() {
        // used on the card: 1408x2048 at exactly 40 MSPS, 2048x2048 at 100 MSPS
        let p = plan_freerun(1408, 2048, 40e6).unwrap();
        assert_eq!(p.fr_ll, 1008);
        let p = plan_freerun(2048, 2048, 100e6).unwrap();
        assert_eq!(p.fr_ll, 586);
    }

    #[test]
    fn out_of_range() {
        assert!(plan_freerun(2048, 2048, 5e6).is_err());
        assert!(plan_freerun(2048, 2048, 200e6).is_err());
        assert!(plan_auto(2048, 300e6).is_err());
    }

    #[test]
    fn pixel_clock() {
        let f = pixel_clock_hz(2200, 1125, 166_667, false);
        assert!((f - 148.5e6).abs() < 0.1e6);
    }
}
