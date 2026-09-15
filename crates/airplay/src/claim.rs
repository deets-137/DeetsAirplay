//! Who is holding which speaker, machine-wide.
//!
//! A receiver takes one sender at a time, and two of this family's apps can
//! send: DeetsAirplay (the tray app, the whole PC's output) and DeetsMusic
//! (the player, its own sound). Neither could see the other, so both would
//! offer to connect to a speaker the other was already streaming to, and the
//! loser of that race got a handshake failure with no explanation.
//!
//! A claim is written here by `session::connect` and dropped by
//! `Session::disconnect` (and by its `Drop`, so a session thrown away without
//! being disconnected still lets go). Because it lives in the crate, an app
//! adopts it by taking the new revision — no code of its own.
//!
//! The file is `%LOCALAPPDATA%\Deets\airplay-claims.tsv`: local, not roaming,
//! because a pid claimed on this PC means nothing on another one. The format
//! is one escaped tab-separated record per line under a version line, not
//! JSON, so the crate needs no parser and no new dependency; nothing outside
//! this module ever sees the text.
//!
//! A claim is believed only while a process with that pid *and* that exe name
//! is alive, so a crash, a kill, or a pid reused by something else all read as
//! "gone" without anyone having to clean up.
//!
//! Two writers can still interleave a read-modify-write and lose one record.
//! Each write only ever adds or removes its own line and copies the other live
//! ones through, so the cost of losing that race is one missed warning, healed
//! by the next write. A lock is not worth the failure modes it would add to a
//! path that runs during a connect.

use std::path::PathBuf;

use crate::crypto::random;

const HEADER: &str = "deets-airplay-claims 1";

/// What the holder is actually sending — the difference between "everything
/// this PC plays" (so every other app's sound is already on the speaker) and
/// "these apps only" (so another app's sound is not). A picker for the second
/// case is the roadmap item; the field is here from the start so the file
/// never has to change shape for it.
#[derive(Clone, Debug, PartialEq)]
pub enum Send {
    /// The default output's loopback: whatever the PC plays, including us.
    All,
    /// One or more process trees, by exe name.
    Apps(Vec<String>),
    /// The holder has not said (an older build, or a source we do not model).
    Unknown,
}

impl Send {
    /// Does this stream already carry every other app's sound?
    pub fn carries_everything(&self) -> bool {
        matches!(self, Send::All)
    }

    /// Does it carry `exe` (case-insensitive)? `Unknown` answers `false`: a
    /// caller asking this is deciding whether to show something as being on
    /// the speaker, and a guess would be a lie either way.
    pub fn carries(&self, exe: &str) -> bool {
        match self {
            Send::All => true,
            Send::Apps(names) => names.iter().any(|n| n.eq_ignore_ascii_case(exe)),
            Send::Unknown => false,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Send::All => "all",
            Send::Apps(_) => "apps",
            Send::Unknown => "unknown",
        }
    }

    fn detail(&self) -> String {
        match self {
            Send::Apps(names) => names.join(","),
            _ => String::new(),
        }
    }

    fn parse(kind: &str, detail: &str) -> Send {
        match kind {
            "all" => Send::All,
            "apps" => Send::Apps(detail.split(',').filter(|s| !s.is_empty()).map(String::from).collect()),
            _ => Send::Unknown,
        }
    }
}

/// One live sender's hold on one speaker.
#[derive(Clone, Debug)]
pub struct Claim {
    /// Random per session, so a release only ever drops its own record.
    pub id: u64,
    /// The holder's own name for itself (`Config::client_name`): "DeetsMusic".
    pub app: String,
    pub pid: u32,
    pub exe: String,
    pub speaker: String,
    pub ip: String,
    pub port: u16,
    /// Unix seconds at the moment the session came up.
    pub since: u64,
    pub send: Send,
}

impl Claim {
    /// Is this our own process's claim? A reader wants the others.
    pub fn is_ours(&self) -> bool {
        self.pid == std::process::id()
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Deets").join("airplay-claims.tsv"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn own_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace(['\n', '\r'], " ")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

fn line_of(c: &Claim) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c.id,
        esc(&c.app),
        c.pid,
        esc(&c.exe),
        esc(&c.speaker),
        esc(&c.ip),
        c.port,
        c.since,
        c.send.kind(),
        esc(&c.send.detail()),
    )
}

/// A record, or `None` if it is malformed. Extra columns are ignored, so a
/// newer build can add one without this one choking on it.
fn claim_of(line: &str) -> Option<Claim> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 10 {
        return None;
    }
    Some(Claim {
        id: f[0].parse().ok()?,
        app: unesc(f[1]),
        pid: f[2].parse().ok()?,
        exe: unesc(f[3]),
        speaker: unesc(f[4]),
        ip: unesc(f[5]),
        port: f[6].parse().ok()?,
        since: f[7].parse().unwrap_or(0),
        send: Send::parse(f[8], &unesc(f[9])),
    })
}

/// Every claim on disk whose process is still alive and still the same exe.
pub fn live() -> Vec<Claim> {
    let Some(p) = path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&p) else { return Vec::new() };
    let parsed: Vec<Claim> = text.lines().skip(1).filter_map(claim_of).collect();
    if parsed.is_empty() {
        return parsed;
    }
    // One process snapshot for the whole list, however long it is.
    let running = crate::mixer::running_exes();
    parsed.into_iter().filter(|c| c.is_ours() || running.get(&c.pid).is_some_and(|e| *e == c.exe)).collect()
}

/// Live claims held by some other process — what a UI wants to warn about.
pub fn others() -> Vec<Claim> {
    live().into_iter().filter(|c| !c.is_ours()).collect()
}

/// The other process holding this speaker, if one is.
pub fn on_speaker(name: &str) -> Option<Claim> {
    others().into_iter().find(|c| c.speaker.eq_ignore_ascii_case(name))
}

/// Rewrite the file: every live record except `drop`, plus `add`. Never fails
/// loudly — a claim is an aid to the UI, not a part of the session.
fn rewrite(drop: Option<u64>, add: Option<Claim>) {
    let Some(p) = path() else { return };
    let mut kept: Vec<Claim> = live().into_iter().filter(|c| Some(c.id) != drop).collect();
    if let Some(c) = add {
        kept.retain(|k| k.id != c.id);
        kept.push(c);
    }
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut text = String::from(HEADER);
    for c in &kept {
        text.push('\n');
        text.push_str(&line_of(c));
    }
    text.push('\n');
    // Written beside the file and renamed over it, so a reader never sees a
    // half-written list (a rename replaces on Windows).
    let tmp = p.with_extension(format!("tsv.{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        std::fs::remove_file(&tmp).ok();
    }
}

/// Record that this process now holds `speaker`. The send set starts as
/// `Unknown`; the caller narrows it with [`describe`] once it knows.
pub fn hold(app: &str, speaker: &str, ip: &str, port: u16) -> u64 {
    let id = random::u64();
    rewrite(
        None,
        Some(Claim {
            id,
            app: app.to_string(),
            pid: std::process::id(),
            exe: own_exe(),
            speaker: speaker.to_string(),
            ip: ip.to_string(),
            port,
            since: now_unix(),
            send: Send::Unknown,
        }),
    );
    id
}

/// Say what the held stream carries. Safe to call at any point in a session.
pub fn describe(id: u64, send: Send) {
    let Some(mut c) = live().into_iter().find(|c| c.id == id && c.is_ours()) else { return };
    if c.send == send {
        return;
    }
    c.send = send;
    rewrite(Some(id), Some(c));
}

/// Let a speaker go. Idempotent: releasing twice, or an id that was never
/// held, does nothing.
pub fn release(id: u64) {
    rewrite(Some(id), None);
}
