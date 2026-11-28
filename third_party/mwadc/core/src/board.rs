//! The Pro Capture cards with an ADV7842 analog front end, and what each one wires to its jacks.
//!
//! From Magewell's driver 1.3.0.4437 (docs/15): product 0x102 (AIO), 0x103 (DVI) and 0x104 (HDMI)
//! run the same ADV7842 code. Its input mux puts VGA on Ain1/2/3, Component on Ain4/5/6, S-video on
//! Ain10/12 and composite on Ain11 (ADI's reference wiring, UG-214 Table 7); the boards differ in
//! which of those inputs the driver offers. "Version B" of the HDMI and DVI cards (firmware 2.x)
//! only changes the line-in codec, which mwadc reaches through the SDK.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// SDK input sources (`INPUT_SOURCE(type, 0)`, `MWCAP_VIDEO_INPUT_TYPE`).
pub const INPUT_VGA: u32 = 0x02 << 8;
pub const INPUT_COMPONENT: u32 = 0x08 << 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Board {
    /// Pro Capture HDMI: Component, S-video and composite on the HD15 breakout.
    Hdmi,
    /// Pro Capture DVI: VGA (analog RGB) on the DVI-I connector; no Component, S-video or composite.
    Dvi,
    /// Pro Capture AIO: VGA, Component, S-video and composite.
    Aio,
}

/// A breakout jack: its `--adcN` name, a label and the ADV7842 pin it is wired to.
#[derive(Clone, Copy, Debug)]
pub struct Jack {
    pub name: &'static str,
    pub label: &'static str,
    pub pin: u8,
}

/// Every jack of any supported card, in pin order.
pub const JACKS: &[Jack] = &[
    Jack { name: "vga-g", label: "VGA G", pin: 1 },
    Jack { name: "vga-r", label: "VGA R", pin: 2 },
    Jack { name: "vga-b", label: "VGA B", pin: 3 },
    Jack { name: "comp-y", label: "Component Y", pin: 4 },
    Jack { name: "comp-pr", label: "Component Pr", pin: 5 },
    Jack { name: "comp-pb", label: "Component Pb", pin: 6 },
    Jack { name: "yc-y", label: "S-video Y", pin: 10 },
    Jack { name: "cvbs", label: "Composite", pin: 11 },
    Jack { name: "yc-c", label: "S-video C", pin: 12 },
];

impl Board {
    pub const ALL: [Board; 3] = [Board::Hdmi, Board::Dvi, Board::Aio];

    /// The board for an SDK product id (`MWCAP_CHANNEL_INFO.wProductID`), if mwadc supports it.
    pub fn from_product(id: u16) -> Option<Board> {
        match id {
            0x104 => Some(Board::Hdmi),
            0x103 => Some(Board::Dvi),
            0x102 => Some(Board::Aio),
            _ => None,
        }
    }

    pub fn product_id(self) -> u16 {
        match self {
            Board::Hdmi => 0x104,
            Board::Dvi => 0x103,
            Board::Aio => 0x102,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Board::Hdmi => "Pro Capture HDMI",
            Board::Dvi => "Pro Capture DVI",
            Board::Aio => "Pro Capture AIO",
        }
    }

    /// ADV7842 pins wired to a jack on this board.
    pub fn pins(self) -> &'static [u8] {
        match self {
            Board::Hdmi => &[4, 5, 6, 10, 11, 12],
            Board::Dvi => &[1, 2, 3],
            Board::Aio => &[1, 2, 3, 4, 5, 6, 10, 11, 12],
        }
    }

    pub fn jacks(self) -> impl Iterator<Item = &'static Jack> {
        JACKS.iter().filter(move |j| self.pins().contains(&j.pin))
    }

    /// Default ADC0/1/2 pins: one jack on every ADC.
    pub fn default_adc(self) -> [u8; 3] {
        match self {
            Board::Dvi => [1, 2, 3],
            _ => [4, 5, 6],
        }
    }

    /// The SDK input `arm` selects so that the driver sets the ADV7842 up for its CP (graphics)
    /// path; the recipe then overrides the input mux. The DVI card offers no Component.
    pub fn carrier_input(self) -> u32 {
        match self {
            Board::Dvi => INPUT_VGA,
            _ => INPUT_COMPONENT,
        }
    }

    /// Hardware revision as the driver tells it from the firmware version: "B" (new line-in codec)
    /// from firmware 2.0 on the HDMI and DVI cards, 4.0 on the AIO; "A" (WM8731) before.
    pub fn revision(self, firmware: u32) -> char {
        let b_from = if self == Board::Aio { 0x40000 } else { 0x20000 };
        if firmware >= b_from {
            'B'
        } else {
            'A'
        }
    }

    /// Pins of `adc` (0 = off) that are not wired to a jack on this board.
    pub fn unwired(self, adc: [u8; 3]) -> Vec<u8> {
        adc.iter().copied().filter(|&p| p != 0 && !self.pins().contains(&p)).collect()
    }

    /// One line listing the board's jacks, e.g. "vga-g (Ain1), vga-r (Ain2), vga-b (Ain3)".
    pub fn describe_jacks(self) -> String {
        self.jacks().map(|j| format!("{} (Ain{})", j.name, j.pin)).collect::<Vec<_>>().join(", ")
    }
}

/// Jack name of a pin, if it is a jack on some card.
pub fn jack_of(pin: u8) -> Option<&'static Jack> {
    JACKS.iter().find(|j| j.pin == pin)
}

/// Board for a product id, or an error naming the supported ones.
pub fn require(product_id: u16, product: &str) -> Result<Board> {
    match Board::from_product(product_id) {
        Some(b) => Ok(b),
        None => bail!(
            "{product} (product {product_id:#x}) is not supported: mwadc needs the ADV7842 analog front end of the {}",
            Board::ALL.iter().map(|b| format!("{} ({:#x})", b.name(), b.product_id())).collect::<Vec<_>>().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adv7842::{check_reach, parse_ain};

    #[test]
    fn boards() {
        assert_eq!(Board::from_product(0x103), Some(Board::Dvi));
        assert_eq!(Board::from_product(0x108), None);
        for b in Board::ALL {
            assert_eq!(Board::from_product(b.product_id()), Some(b));
            check_reach(b.default_adc()).unwrap();
            assert!(b.unwired(b.default_adc()).is_empty(), "{b:?}");
            for j in b.jacks() {
                assert_eq!(parse_ain(j.name).unwrap(), j.pin);
            }
        }
        assert_eq!(Board::Dvi.unwired([4, 2, 0]), vec![4]);
        assert!(Board::Aio.unwired([1, 5, 12]).is_empty());
        assert!(require(0x108, "Pro Capture Dual HDMI").is_err());
    }
}
