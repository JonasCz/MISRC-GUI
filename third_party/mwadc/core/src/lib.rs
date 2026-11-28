//! mwadc-core: raw-ADC capture with the Magewell Pro Capture HDMI, DVI and AIO (Artix-7 + ADV7842) on
//! their stock driver. Shared by the `mwadc` CLI and the C API (`mwadc_ffi.dll`, used by MISRC-GUI).
//!
//! - `adv7842`, `alias`, `timing`, `bus`: the register recipe and the capture sequence (docs/03, 11).
//! - `board`: the supported cards and their jacks (docs/15).
//! - `audiolock`: line-in audio resampled onto the RF sample clock.
//! - `config`: capture options, settable from text (`CaptureConfig::set`).
//! - `calib`: settling fit, level choice and the per-card calibration store.
//! - `win`: the Windows driver path: `Device` (open/arm/restore), `Stream` (threaded capture).
//!   Magewell's Linux driver has the same core and private ioctls (docs/07); a `linux` module could
//!   implement the same `Device`/`Stream` API, but none exists yet.

pub mod adv7842;
pub mod alias;
pub mod audiolock;
pub mod board;
pub mod bus;
pub mod calib;
pub mod config;
pub mod export;
pub mod fourcc;
pub mod log;
pub mod parse;
pub mod timing;
pub mod util;
#[cfg(windows)]
pub mod win;
