//! Log output. The core never prints by itself: every line goes to the installed sink (the CLI
//! prints and appends to the session log; the C API forwards to the host's callback). Without a
//! sink, lines go to stderr.

use std::sync::RwLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

pub type Sink = Box<dyn Fn(Level, &str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Install (or with `None`, remove) the process-wide log sink.
pub fn set_sink(s: Option<Sink>) {
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = s;
}

pub fn line(level: Level, s: &str) {
    let g = SINK.read().unwrap_or_else(|e| e.into_inner());
    match g.as_ref() {
        Some(f) => f(level, s),
        None => eprintln!("{s}"),
    }
}

/// Info-level log line, `format!` syntax.
#[macro_export]
macro_rules! say {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Info, &format!($($t)*)) };
}

/// Warning-level log line.
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Warn, &format!($($t)*)) };
}
