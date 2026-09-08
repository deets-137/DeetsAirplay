//! The AirPlay 2 sender, hand-rolled. Read `docs/protocol.md` first.
//!
//! - `mdns`     find the speaker
//! - `rtsp`     the control connection (plain, then ChaCha20-framed)
//! - `pairing`  HAP transient pair-setup (SRP, PIN 3939) → session keys
//! - `tlv8` / `bplist`  the two encodings the handshake speaks
//! - `alac` / `rtp`     the audio and timing packets
//! - `session`  the state machine that ties it together and streams

pub mod alac;
pub mod bplist;
pub mod mdns;
pub mod pairing;
pub mod rtp;
pub mod rtsp;
pub mod session;
pub mod tlv8;

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Mirror session logs to a file (the tray app has no console). Call once.
pub fn log_to_file(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        LOG_FILE.set(Mutex::new(f)).ok();
    }
}

/// One log line to stderr and, when configured, the log file.
pub fn log(line: &str) {
    eprintln!("{line}");
    if let Some(f) = LOG_FILE.get() {
        if let Ok(mut f) = f.lock() {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            writeln!(f, "{t} {line}").ok();
        }
    }
}
