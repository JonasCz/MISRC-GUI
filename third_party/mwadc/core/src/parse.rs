//! Text parsers shared by the CLI and the C API's key/value configuration.

use anyhow::{bail, Context, Result};

use crate::adv7842::{self, Map};

pub fn parse_num(s: &str) -> Result<u64> {
    let t = s.trim().replace('_', "");
    let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        t.parse()
    };
    v.with_context(|| format!("not a number: '{s}'"))
}

pub fn parse_u8(s: &str) -> Result<u8> {
    u8::try_from(parse_num(s)?).with_context(|| format!("'{s}' does not fit in 8 bits"))
}

pub fn parse_u16(s: &str) -> Result<u16> {
    u16::try_from(parse_num(s)?).with_context(|| format!("'{s}' does not fit in 16 bits"))
}

pub fn parse_u32(s: &str) -> Result<u32> {
    u32::try_from(parse_num(s)?).with_context(|| format!("'{s}' does not fit in 32 bits"))
}

/// Rotary switch value: 0-15, decimal or hex digit ("a".."f" accepted).
pub fn parse_board(s: &str) -> Result<u8> {
    let v = if s.len() == 1 { u8::from_str_radix(s, 16)? } else { parse_u8(s)? };
    if v > 15 {
        bail!("board (rotary switch) must be 0-F, got {s}");
    }
    Ok(v)
}

/// "40M", "28.636M", "40e6", "1.5k", "40000000".
pub fn parse_hz(s: &str) -> Result<f64> {
    let t = s.trim();
    let (num, mul) = match t.chars().last() {
        Some('M') | Some('m') => (&t[..t.len() - 1], 1e6),
        Some('k') | Some('K') => (&t[..t.len() - 1], 1e3),
        Some('G') | Some('g') => (&t[..t.len() - 1], 1e9),
        _ => (t, 1.0),
    };
    let v: f64 = num.parse().with_context(|| format!("not a frequency: '{s}'"))?;
    if !(v > 0.0) {
        bail!("frequency must be positive: '{s}'");
    }
    Ok(v * mul)
}

pub fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Ok(true),
        "0" | "off" | "false" | "no" => Ok(false),
        _ => bail!("expected on/off, got '{s}'"),
    }
}

/// A register write: `map:reg=val` or `map:reg=val/mask`, e.g. `cp:0xbf=0x01`, `io:0x30=0x10/0x10`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegWrite {
    pub map: Map,
    pub reg: u8,
    pub val: u8,
    /// 0xFF = whole register.
    pub mask: u8,
}

pub fn parse_set(s: &str) -> Result<RegWrite> {
    let (lhs, rhs) = s.split_once('=').with_context(|| format!("register write wants map:reg=val[/mask], got '{s}'"))?;
    let (map, reg) = lhs.split_once(':').with_context(|| format!("register write wants map:reg=val[/mask], got '{s}'"))?;
    let dev = adv7842::parse_dev(map)?;
    let map = Map::from_addr(dev).with_context(|| format!("register write needs a named map, got '{map}'"))?;
    let (val, mask) = match rhs.split_once('/') {
        Some((v, m)) => (parse_u8(v)?, parse_u8(m)?),
        None => (parse_u8(rhs)?, 0xFF),
    };
    if val & !mask != 0 {
        bail!("{s}: value has bits outside the mask");
    }
    Ok(RegWrite { map, reg: parse_u8(reg)?, val, mask })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_u8("0xBF").unwrap(), 0xBF);
        assert_eq!(parse_u32("8228").unwrap(), 8228);
        assert_eq!(parse_hz("40M").unwrap(), 40e6);
        assert_eq!(parse_hz("28.63636M").unwrap(), 28_636_360.0);
        assert_eq!(parse_hz("40e6").unwrap(), 40e6);
        assert_eq!(parse_set("cp:0xbf=1").unwrap(), RegWrite { map: Map::Cp, reg: 0xBF, val: 1, mask: 0xFF });
        assert_eq!(parse_set("io:0x30=0x10/0x10").unwrap(), RegWrite { map: Map::Io, reg: 0x30, val: 0x10, mask: 0x10 });
        assert!(parse_set("io:0x30=0x90/0x10").is_err());
        assert_eq!(parse_board("1").unwrap(), 1);
        assert_eq!(parse_board("f").unwrap(), 15);
        assert!(parse_board("16").is_err());
        assert!(parse_bool("On").unwrap());
    }
}
