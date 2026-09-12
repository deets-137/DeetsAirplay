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
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG_PATH: OnceLock<Mutex<PathBuf>> = OnceLock::new();

/// The file rotates at this size, keeping one previous generation (`<stem>.1.<ext>`),
/// so it is bounded at about 1 MB for good. A connected session writes a
/// `/feedback` request + reply every two seconds, which reached 1.1 MB in a day
/// (found from DeetsMusic, 2026-09-11); a truncate would lose the run-up to a fault,
/// so it rotates instead.
const ROTATE_AT: u64 = 512 * 1024;

/// Mirror session logs to a file (the tray app has no console). Call once.
pub fn log_to_file(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    LOG_PATH.set(Mutex::new(path.to_path_buf())).ok();
}

/// One log line to stderr and, when configured, the log file. Never panics and
/// never fails loudly: a log line must not take the session down.
pub fn log(line: &str) {
    eprintln!("{line}");
    let Some(p) = LOG_PATH.get() else { return };
    let Ok(path) = p.lock() else { return };
    if std::fs::metadata(&*path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("log");
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("log");
        std::fs::rename(&*path, path.with_file_name(format!("{stem}.1.{ext}"))).ok();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&*path) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        writeln!(f, "{t} {line}").ok();
    }
}
