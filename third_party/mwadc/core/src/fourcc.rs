//! Capture pixel formats and where each ADC's samples sit in the pixel word (measured, docs/11 §5).
//!
//! RG10: one little-endian 32-bit word per sample instant, 2:10:10:10 with bits 31:30 always 1, packed
//! as R:G:B = ADC2:ADC0:ADC1 (bits 20-29 / 10-19 / 0-9; with Component input Pb / Y / Pr). ADC1/ADC2 arrive with
//! 2x scale and their lowest bit always 0. Y16: ADC0 only, 10 bits MSB-aligned in 16.

use serde::{Deserialize, Serialize};

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Y16,
    Rgb10,
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub lsb: u32,
    pub bits: u32,
}

const fn f(name: &'static str, lsb: u32, bits: u32) -> Field {
    Field { name, lsb, bits }
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Y16 => "y16",
            Format::Rgb10 => "rgb10",
        }
    }

    pub fn fourcc(self) -> u32 {
        match self {
            Format::Y16 => fourcc(b"Y16 "),
            Format::Rgb10 => fourcc(b"RG10"),
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Format::Y16 => 2,
            Format::Rgb10 => 4,
        }
    }

    /// Sample fields within the little-endian pixel word, named by ADC.
    pub fn fields(self) -> &'static [Field] {
        const Y16: &[Field] = &[f("adc0", 6, 10)];
        const RG10: &[Field] = &[f("adc0", 10, 10), f("adc1", 0, 10), f("adc2", 20, 10)];
        match self {
            Format::Y16 => Y16,
            Format::Rgb10 => RG10,
        }
    }

    pub fn min_stride(self, cx: usize) -> usize {
        (cx * self.bytes_per_pixel() + 3) & !3
    }
}

#[inline]
pub fn pixel_word(px: &[u8]) -> u32 {
    let mut w = 0u32;
    for (i, &b) in px.iter().enumerate().take(4) {
        w |= (b as u32) << (8 * i);
    }
    w
}

#[inline]
pub fn extract(word: u32, fld: &Field) -> u32 {
    (word >> fld.lsb) & ((1u64 << fld.bits) - 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_codes() {
        // MWFOURCC('R','G','1','0') from MWFOURCC.h
        assert_eq!(Format::Rgb10.fourcc(), 0x3031_4752);
        assert_eq!(Format::Y16.fourcc(), 0x2036_3159);
    }

    #[test]
    fn rg10_fields() {
        // ADC0 = 1 in bits 10-19, ADC1 = 2 in bits 0-9, ADC2 = 3 in bits 20-29 (mux sweep on the card)
        let word: u32 = 0xC000_0000 | 3 << 20 | 1 << 10 | 2;
        let w = pixel_word(&word.to_le_bytes());
        let fl = Format::Rgb10.fields();
        assert_eq!([extract(w, &fl[0]), extract(w, &fl[1]), extract(w, &fl[2])], [1, 2, 3]);
        let y = pixel_word(&(500u16 << 6).to_le_bytes());
        assert_eq!(extract(y, &Format::Y16.fields()[0]), 500);
    }
}
