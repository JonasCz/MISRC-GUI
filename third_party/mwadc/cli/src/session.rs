//! Session directories (one per run): `log.txt` gets every log line, plus whatever the command writes.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;

use mwadc_core::log::{self, Level};
use mwadc_core::say;
use mwadc_core::util::timestamp;

static LOG: Mutex<Option<File>> = Mutex::new(None);

/// Route core log lines to stdout (stderr for warnings) and the current session's `log.txt`.
pub fn install_log() {
    log::set_sink(Some(Box::new(|level, s| {
        if level >= Level::Warn {
            eprintln!("{s}");
        } else if level >= Level::Info {
            println!("{s}");
        }
        if let Ok(mut g) = LOG.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{s}");
            }
        }
    })));
}

#[derive(Clone)]
pub struct Session {
    pub dir: Arc<PathBuf>,
}

impl Session {
    pub fn create(base: &Path, tag: &str) -> Result<Session> {
        let dir = base.join(format!("{}_{tag}", timestamp()));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let f = File::create(dir.join("log.txt"))?;
        *LOG.lock().unwrap() = Some(f);
        let s = Session { dir: Arc::new(dir) };
        say!("session: {}", s.dir.display());
        say!("command: {}", std::env::args().collect::<Vec<_>>().join(" "));
        Ok(s)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn write_json<T: Serialize>(&self, name: &str, v: &T) -> Result<()> {
        let p = self.path(name);
        fs::write(&p, serde_json::to_string_pretty(v)?).with_context(|| format!("writing {}", p.display()))
    }

    pub fn write_bytes(&self, name: &str, b: &[u8]) -> Result<()> {
        let p = self.path(name);
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        fs::write(&p, b).with_context(|| format!("writing {}", p.display()))
    }
}
