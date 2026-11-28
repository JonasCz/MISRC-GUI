//! mwadc: raw ADC capture with the Magewell Pro Capture HDMI, DVI and AIO on their stock driver and firmware.
//! See docs/12-mwadc.md. The capture machinery lives in `mwadc-core`; this is the command line.

mod cli;
#[cfg(windows)]
mod hw;
mod session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};
use mwadc_core::say;

/// Set by Ctrl-C; streams and waits check it and stop cleanly (buffers unpinned, card restored).
pub fn stop_flag() -> Arc<AtomicBool> {
    static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    STOP.get_or_init(|| Arc::new(AtomicBool::new(false))).clone()
}

pub fn stopped() -> bool {
    stop_flag().load(Ordering::Relaxed)
}

fn main() {
    session::install_log();
    let _ = ctrlc::set_handler(|| {
        if stop_flag().swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
        eprintln!("stopping (Ctrl-C again to abort immediately)");
    });
    if let Err(e) = run(Cli::parse()) {
        say!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Export(a) => mwadc_core::export::run(&a.dir, &a.kind, a.max_frames),
        Cmd::Options => {
            for (k, d) in mwadc_core::config::KEYS {
                println!("  {k:<12} {d}");
            }
            Ok(())
        }
        #[cfg(windows)]
        cmd => hw::dispatch(&cli.dev, cmd),
        #[cfg(not(windows))]
        _ => anyhow::bail!("this command talks to the card and only runs on Windows; `export` works everywhere"),
    }
}
