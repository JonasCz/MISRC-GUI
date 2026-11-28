//! Register access abstraction and the recipe applier.

use anyhow::{bail, Result};

use crate::adv7842::{Map, Op, Step};
use crate::log::{line, Level};

pub trait RegBus {
    fn i2c_read(&mut self, dev8: u8, reg: u8, buf: &mut [u8]) -> Result<()>;
    fn i2c_write(&mut self, dev8: u8, reg: u8, data: &[u8]) -> Result<()>;
    fn bar0_read(&mut self, off: u32) -> Result<u32>;

    fn rd(&mut self, map: Map, reg: u8) -> Result<u8> {
        self.rd_raw(map.addr(), reg)
    }

    /// Read one register at an arbitrary 8-bit address (maps that have been moved).
    fn rd_raw(&mut self, dev8: u8, reg: u8) -> Result<u8> {
        let mut b = [0u8];
        self.i2c_read(dev8, reg, &mut b)?;
        Ok(b[0])
    }
}

/// BAR0 offsets the driver itself reads (doc 06). The driver does no bounds check on the hidden
/// property, and other offsets may be read-to-clear interrupt/DMA registers.
pub const BAR0_SAFE_READ: &[(u32, u32, &str)] = &[
    (0x0008, 0x000C, "product id"),
    (0x0018, 0x001C, "PCIe link status"),
    (0x0028, 0x002C, "on-board memory size"),
    (0x003C, 0x0040, "reference clock frequency"),
    (0x0200, 0x0210, "video sync meter"),
    (0x2020, 0x2200, "video capture ch0 + read pipelines"),
];

pub fn bar0_check(off: u32) -> Result<()> {
    if off % 4 != 0 {
        bail!("BAR0 offset {off:#x} is not 4-byte aligned");
    }
    if BAR0_SAFE_READ.iter().any(|&(lo, hi, _)| off >= lo && off < hi) {
        Ok(())
    } else {
        bail!(
            "BAR0 read at {off:#x} is outside the known-safe ranges {:x?}",
            BAR0_SAFE_READ.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>()
        )
    }
}

/// Apply a recipe, logging each register's old and new value at `level`.
pub fn apply(bus: &mut dyn RegBus, steps: &[Step], level: Level) -> Result<()> {
    let say = |s: String| line(level, &s);
    for s in steps {
        match &s.op {
            Op::Write { map, reg, val } => {
                let before = bus.rd(*map, *reg)?;
                bus.i2c_write(map.addr(), *reg, &[*val])?;
                say(format!("  {:<4} {:#04x}: {:#04x} -> {:#04x}  {}", map.name(), reg, before, val, s.why));
            }
            Op::Rmw { map, reg, mask, val } => {
                let cur = bus.rd(*map, *reg)?;
                let new = (cur & !mask) | (val & mask);
                bus.i2c_write(map.addr(), *reg, &[new])?;
                say(format!("  {:<4} {:#04x}: {:#04x} -> {:#04x}  {}", map.name(), reg, cur, new, s.why));
            }
            Op::Seq { map, reg, vals } => {
                let mut before = vec![0u8; vals.len()];
                bus.i2c_read(map.addr(), *reg, &mut before)?;
                bus.i2c_write(map.addr(), *reg, vals)?;
                say(format!("  {:<4} {:#04x}..: {:02x?} -> {:02x?}  {}", map.name(), reg, before, vals, s.why));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adv7842::{FrontEnd, Recipe};
    use std::collections::HashMap;

    #[derive(Default)]
    struct MockBus {
        i2c: HashMap<(u8, u8), u8>,
    }

    impl RegBus for MockBus {
        fn i2c_read(&mut self, dev8: u8, reg: u8, buf: &mut [u8]) -> Result<()> {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.i2c.get(&(dev8, reg.wrapping_add(i as u8))).unwrap_or(&0);
            }
            Ok(())
        }
        fn i2c_write(&mut self, dev8: u8, reg: u8, data: &[u8]) -> Result<()> {
            for (i, &b) in data.iter().enumerate() {
                self.i2c.insert((dev8, reg.wrapping_add(i as u8)), b);
            }
            Ok(())
        }
        fn bar0_read(&mut self, _off: u32) -> Result<u32> {
            Ok(0)
        }
    }

    #[test]
    fn apply_keeps_unmasked_bits() {
        let mut bus = MockBus::default();
        bus.i2c.insert((0x44, 0xBF), 0xF4);
        bus.i2c.insert((0x40, 0x05), 0x28);
        bus.i2c.insert((0x40, 0x30), 0x88);
        let r = Recipe { adc: [4, 5, 6], pll_div: 1936, fr_ll: 1386, lines: 2048, fe: FrontEnd::default(), extra: vec![] };
        apply(&mut bus, &r.steps(), Level::Debug).unwrap();
        assert_eq!(bus.i2c[&(0x44, 0xBF)], 0xF1, "RMW keeps the upper bits");
        assert_eq!(bus.i2c[&(0x40, 0x05)], 0x20);
        assert_eq!(bus.i2c[&(0x40, 0x30)], 0x08, "clamp off clears only bit 7");
    }

    #[test]
    fn bar0_guard() {
        assert!(bar0_check(0x2024).is_ok());
        assert!(bar0_check(0x0204).is_ok());
        assert!(bar0_check(0x0280).is_err());
        assert!(bar0_check(0x2026).is_err());
        assert!(bar0_check(0x2004).is_err());
    }
}
