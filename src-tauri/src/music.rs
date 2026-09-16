//! DeetsMusic, seen from here.
//!
//! DeetsMusic runs a loopback HTTP bridge (its `bridge.rs`, ports 47825–47828,
//! a token in its own `settings.json`). When it is the thing playing, it knows
//! far more than the Windows media session does — the artwork, the exact
//! position, whether a station is live — and it can be driven precisely
//! instead of through a media-key tap. This module is the whole of that
//! conversation.
//!
//! The HTTP is hand-rolled, like everything else protocol-shaped in this
//! project: four known routes on 127.0.0.1 with small JSON bodies, so a client
//! is a request line, three headers, and a body split. `Connection: close`
//! makes the reply a read-to-EOF.
//!
//! Everything here is best-effort. DeetsMusic is usually NOT installed
//! alongside this app, so every call must fail into "not running" quietly and
//! cheaply: a refused connection on loopback returns at once, and a failed
//! probe is not retried for a few seconds.
//!
//! What needs what, on the DeetsMusic side:
//!   - `GET /health`      nothing. Tells us the version and whether Agent control is on.
//!   - `GET /now-playing` the token only.
//!   - `GET /airplay`     the token only. Which speaker it is streaming to (newer builds;
//!                        an older one 404s, which is how we know not to offer a hand-over).
//!   - `POST /command`    the token AND Agent control (Settings › Connections).
//!   - `POST /airplay`    the same. Hands the speaker over.
//! So the now-playing card works out of the box; driving DeetsMusic from here
//! needs the user to have turned Agent control on, and the panel says so
//! rather than failing silently.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

const PORTS: [u16; 4] = [47825, 47826, 47827, 47828];
/// Both installs: DeetsMusic's own CLI keeps them apart, but from here we
/// simply want whichever one is running.
const IDS: [&str; 2] = ["com.deetsmusic.app", "com.deetsmusic.dev"];

/// Loopback, so these are generous. A refused port fails long before them.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// A command goes through DeetsMusic's window, which can be busy.
const COMMAND_READ_TIMEOUT: Duration = Duration::from_millis(4000);
/// After a failed probe, do not hammer the four ports every second.
const RETRY_AFTER: Duration = Duration::from_secs(5);

// ── the wire ──────────────────────────────────────────────────────────

/// One request, one reply: `(status, body)`. `Connection: close` means the
/// body ends at EOF, so the only header we must understand is the chunked
/// framing tiny_http uses when it does not know the length up front.
fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>, read_timeout: Duration) -> Result<(u16, String), String> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut sock = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(read_timeout)).ok();
    sock.set_write_timeout(Some(Duration::from_millis(500))).ok();

    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n");
    if let Some(t) = token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    if let Some(b) = body {
        sock.write_all(b.as_bytes()).map_err(|e| e.to_string())?;
    }
    sock.flush().ok();

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no header break")?;
    let status: u16 = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).ok_or("no status line")?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    Ok((status, if chunked { dechunk(body) } else { body.to_string() }))
}

/// `<hex length>\r\n<bytes>\r\n`, repeated, ending at a zero length.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size, tail)) = rest.split_once("\r\n") else { break };
        let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else { break };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].strip_prefix("\r\n").unwrap_or("");
    }
    out
}

fn json_at(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>, timeout: Duration) -> Result<Value, String> {
    let (status, text) = request(port, method, path, token, body, timeout)?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    // The bridge's own words are the useful ones ("Agent control is off…").
    let said = value.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    Err(if said.is_empty() { format!("DeetsMusic answered {status}") } else { said })
}

// ── finding it ────────────────────────────────────────────────────────

struct Cache {
    found: Option<(u16, String)>,
    quiet_until: Option<Instant>,
    /// `/health` changes about never (a version, a settings toggle), and every
    /// request is a line in DeetsMusic's own log; asking once a poll would
    /// bury its history under our chatter.
    health: Option<(Instant, Value)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache { found: None, quiet_until: None, health: None }))
}

const HEALTH_FOR: Duration = Duration::from_secs(30);

/// Every bridge token a DeetsMusic install on this machine has written.
fn tokens() -> Vec<String> {
    let Some(appdata) = std::env::var_os("APPDATA") else { return Vec::new() };
    IDS.iter()
        .filter_map(|id| std::fs::read_to_string(std::path::Path::new(&appdata).join(id).join("settings.json")).ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .filter_map(|v| v.get("bridgeToken").or_else(|| v.get("bridge_token")).and_then(Value::as_str).map(String::from))
        .collect()
}

/// The live bridge: a port that answers `/health`, paired with the token that
/// `/health` reports as ours. Cached until a call fails.
fn bridge() -> Option<(u16, String)> {
    {
        let c = cache().lock().unwrap();
        if let Some(found) = c.found.clone() {
            return Some(found);
        }
        if c.quiet_until.is_some_and(|t| Instant::now() < t) {
            return None;
        }
    }
    let tokens = tokens();
    for port in PORTS {
        if request(port, "GET", "/health", None, None, READ_TIMEOUT).is_err() {
            continue;
        }
        for t in &tokens {
            let paired = json_at(port, "GET", "/health", Some(t), None, READ_TIMEOUT)
                .ok()
                .and_then(|v| v.get("paired").and_then(Value::as_bool))
                .unwrap_or(false);
            if paired {
                let found = (port, t.clone());
                let mut c = cache().lock().unwrap();
                c.found = Some(found.clone());
                c.quiet_until = None;
                return Some(found);
            }
        }
    }
    cache().lock().unwrap().quiet_until = Some(Instant::now() + RETRY_AFTER);
    None
}

/// A call failed: re-probe next time (DeetsMusic may have restarted on
/// another port), but not for a few seconds if it is simply gone.
fn forget() {
    let mut c = cache().lock().unwrap();
    c.found = None;
    c.health = None;
    c.quiet_until = Some(Instant::now() + RETRY_AFTER);
}

fn health(port: u16, token: &str) -> Option<Value> {
    if let Some((at, v)) = cache().lock().unwrap().health.clone() {
        if at.elapsed() < HEALTH_FOR {
            return Some(v);
        }
    }
    let v = json_at(port, "GET", "/health", Some(token), None, READ_TIMEOUT).ok()?;
    cache().lock().unwrap().health = Some((Instant::now(), v.clone()));
    Some(v)
}

/// A write, against the cached bridge. An HTTP answer of any code means the
/// bridge is alive and only the request was refused; a transport failure means
/// DeetsMusic moved or closed, so the address is dropped and re-probed.
fn call(method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<Value, String> {
    let Some((port, token)) = bridge() else { return Err("DeetsMusic isn't running".into()) };
    let (status, text) = request(port, method, path, Some(&token), body, timeout).map_err(|e| {
        forget();
        format!("DeetsMusic isn't answering ({e})")
    })?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let said = value.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    Err(if said.is_empty() { format!("DeetsMusic answered {status}") } else { said })
}

// ── what the panel gets ───────────────────────────────────────────────

/// DeetsMusic's now-playing, the parts this panel shows.
#[derive(Clone, Debug, Default, Serialize)]
pub struct NowPlaying {
    /// It has a current item, playing or paused.
    pub active: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// A station's name while one plays; its position means nothing then.
    pub station: String,
    pub live: bool,
    pub artwork: Option<String>,
    pub position: f64,
    pub duration: f64,
    /// 0–100, DeetsMusic's own slider (which, while it streams to a speaker,
    /// IS the speaker's volume — it hands its slider over).
    pub volume: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Music {
    pub version: String,
    /// Agent control is on, so `/command` and the hand-over are allowed.
    pub agent: bool,
    /// This build has `/airplay`, so it can be asked to let a speaker go.
    pub can_hand_over: bool,
    /// The speaker DeetsMusic says it is streaming to, if any. The claim file
    /// is the first source for this; `/airplay` is the fallback for a build
    /// whose crate is too old to write a claim.
    pub speaker: Option<String>,
    pub now_playing: Option<NowPlaying>,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Apple's artwork URLs come as a template with `{w}`/`{h}` placeholders; the
/// card is small, so ask for a small one.
fn artwork(np: &Value) -> Option<String> {
    let direct = s(np, "artworkUrl");
    if !direct.is_empty() {
        return Some(direct);
    }
    let template = s(np, "artworkTemplate");
    if template.is_empty() {
        return None;
    }
    Some(template.replace("{w}", "256").replace("{h}", "256"))
}

/// Everything about DeetsMusic in one poll, or `None` if it is not running.
/// Never returns an error: not running is the normal case.
pub fn snapshot() -> Option<Music> {
    let (port, token) = bridge()?;
    let Some(health) = health(port, &token) else {
        forget();
        return None;
    };
    let version = s(&health, "version");

    let now_playing = json_at(port, "GET", "/now-playing", Some(&token), None, READ_TIMEOUT).ok().map(|np| NowPlaying {
        active: b(&np, "active"),
        playing: b(&np, "playing"),
        title: s(&np, "title"),
        artist: s(&np, "artist"),
        album: s(&np, "album"),
        station: s(&np, "station"),
        live: b(&np, "live"),
        artwork: artwork(&np),
        position: f(&np, "currentTime"),
        duration: f(&np, "duration"),
        volume: f(&np, "volume") * 100.0,
    });

    // Asked for rather than inferred from the version: a build either has the
    // route or 404s, and that answer never goes stale the way a version gate
    // would if the route moved release.
    let airplay = json_at(port, "GET", "/airplay", Some(&token), None, READ_TIMEOUT);
    let can_hand_over = airplay.is_ok();
    let speaker = airplay.ok().and_then(|v| v.get("speaker").and_then(Value::as_str).map(String::from)).filter(|s| !s.is_empty());

    Some(Music { version, agent: b(&health, "agent"), can_hand_over, speaker, now_playing })
}

/// Drive DeetsMusic: `play-pause`, `next`, `previous`, or `volume` (0–1).
pub fn command(kind: &str, value: Option<f64>) -> Result<(), String> {
    let body = match value {
        Some(v) => format!("{{\"kind\":\"{kind}\",\"value\":{v}}}"),
        None => format!("{{\"kind\":\"{kind}\"}}"),
    };
    call("POST", "/command", Some(&body), COMMAND_READ_TIMEOUT).map(|_| ())
}

/// Ask DeetsMusic to let its speaker go, so we can take it.
pub fn hand_over() -> Result<(), String> {
    call("POST", "/airplay", Some("{\"action\":\"disconnect\"}"), COMMAND_READ_TIMEOUT).map(|_| ())
}
