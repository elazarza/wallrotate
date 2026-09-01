//! Opt-in diagnostics. Silent and free unless WALLROTATE_DEBUG=1 is set, which
//! matters because the desktop-layer plumbing is undocumented and the failure
//! modes differ between Windows builds.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};

const UNKNOWN: u8 = 2;
static ENABLED: AtomicU8 = AtomicU8::new(UNKNOWN);

pub fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var("WALLROTATE_DEBUG")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
            ENABLED.store(u8::from(on), Ordering::Relaxed);
            on
        }
    }
}

pub fn path() -> PathBuf {
    crate::config::dir().join("debug.log")
}

pub fn line(msg: impl AsRef<str>) {
    if !enabled() {
        return;
    }
    let _ = std::fs::create_dir_all(crate::config::dir());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())
    {
        let _ = writeln!(file, "{}", msg.as_ref());
    }
}
