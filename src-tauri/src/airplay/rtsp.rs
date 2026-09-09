//! The RTSP control connection. Plaintext until pair-setup finishes, then
//! every byte in both directions is ChaCha20-Poly1305 framed:
//! `[2-byte LE len][ciphertext][16-byte tag]`, at most 1024 plaintext bytes
//! per frame, the length prefix as AAD, and a separate little-endian counter
//! nonce per direction.
//!
//! Requests and responses are the same text shape as HTTP; the receiver
//! parses `POST /pair-setup HTTP/1.1` and `SETUP rtsp://... RTSP/1.0` on the
//! one socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::crypto::{chacha_open, chacha_seal, counter_nonce};

pub const USER_AGENT: &str = "AirPlay/550.10";
const MAX_FRAME: usize = 1024;

pub struct Response {
    pub code: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// The identity headers the receiver expects on every request.
#[derive(Clone)]
pub struct Identity {
    pub dacp_id: String,
    pub active_remote: u32,
    pub client_name: String,
}

pub struct Channel {
    stream: TcpStream,
    cseq: u32,
    rx: Vec<u8>,
    keys: Option<Keys>,
    pub log: bool,
    /// Suppress the request/response lines for a poll that repeats forever.
    /// Receiver-initiated requests are still logged: those are never routine.
    pub quiet: bool,
}

struct Keys {
    write: [u8; 32],
    read: [u8; 32],
    write_ctr: u64,
    read_ctr: u64,
    enc_rx: Vec<u8>,
}

impl Channel {
    pub fn connect(addr: SocketAddr, timeout: Duration) -> std::io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self { stream, cseq: 0, rx: Vec::new(), keys: None, log: false, quiet: false })
    }

    pub fn local_ip(&self) -> String {
        self.stream.local_addr().map(|a| a.ip().to_string()).unwrap_or_default()
    }

    pub fn try_clone(&self) -> std::io::Result<TcpStream> {
        self.stream.try_clone()
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) {
        self.stream.set_read_timeout(d).ok();
    }

    /// Flip to the encrypted framing (after transient pair-setup M4).
    pub fn enable_encryption(&mut self, write: [u8; 32], read: [u8; 32]) {
        self.keys = Some(Keys { write, read, write_ctr: 0, read_ctr: 0, enc_rx: Vec::new() });
    }

    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match &mut self.keys {
            None => self.stream.write_all(data),
            Some(k) => {
                let mut out = Vec::with_capacity(data.len() + 32 * (data.len() / MAX_FRAME + 1));
                for chunk in data.chunks(MAX_FRAME) {
                    let len = (chunk.len() as u16).to_le_bytes();
                    let sealed = chacha_seal(&k.write, &counter_nonce(k.write_ctr), chunk, &len);
                    k.write_ctr += 1;
                    out.extend_from_slice(&len);
                    out.extend_from_slice(&sealed);
                }
                self.stream.write_all(&out)
            }
        }
    }

    /// Pull more plaintext into `rx`; returns false on EOF.
    fn fill(&mut self) -> std::io::Result<bool> {
        let mut buf = [0u8; 8192];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        match &mut self.keys {
            None => self.rx.extend_from_slice(&buf[..n]),
            Some(k) => {
                k.enc_rx.extend_from_slice(&buf[..n]);
                loop {
                    if k.enc_rx.len() < 2 {
                        break;
                    }
                    let len = u16::from_le_bytes([k.enc_rx[0], k.enc_rx[1]]) as usize;
                    let need = 2 + len + 16;
                    if k.enc_rx.len() < need {
                        break;
                    }
                    let aad = [k.enc_rx[0], k.enc_rx[1]];
                    let plain = chacha_open(&k.read, &counter_nonce(k.read_ctr), &k.enc_rx[2..need], &aad)
                        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "control channel: bad auth tag"))?;
                    k.read_ctr += 1;
                    self.rx.extend_from_slice(&plain);
                    k.enc_rx.drain(..need);
                }
            }
        }
        Ok(true)
    }

    /// Parse one complete message off `rx` if present: (status line, headers, body).
    fn take_message(&mut self) -> Option<(String, HashMap<String, String>, Vec<u8>)> {
        let head_end = self.rx.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&self.rx[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap_or_default().trim().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let content_len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let total = head_end + 4 + content_len;
        if self.rx.len() < total {
            return None;
        }
        let body = self.rx[head_end + 4..total].to_vec();
        self.rx.drain(..total);
        Some((status, headers, body))
    }

    /// Send a request and wait for its response. Receiver-initiated requests
    /// that arrive in between (rare on the control channel) are answered
    /// with a bare 200 and skipped.
    pub fn request(
        &mut self,
        method: &str,
        uri: &str,
        protocol: &str,
        identity: &Identity,
        extra_headers: &[(&str, String)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> std::io::Result<Response> {
        self.cseq += 1;
        let mut req = format!("{method} {uri} {protocol}\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n", self.cseq);
        req.push_str(&format!(
            "DACP-ID: {}\r\nActive-Remote: {}\r\nClient-Instance: {}\r\nX-Apple-Client-Name: {}\r\n",
            identity.dacp_id, identity.active_remote, identity.dacp_id, identity.client_name
        ));
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(ct) = content_type {
            req.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        if !body.is_empty() || protocol.starts_with("HTTP") {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        if self.log && !self.quiet {
            super::log(&format!("→ {method} {uri} ({} body bytes)", body.len()));
        }
        let mut bytes = req.into_bytes();
        bytes.extend_from_slice(body);
        self.write_all(&bytes)?;

        loop {
            if let Some((status, headers, body)) = self.take_message() {
                if status.starts_with("RTSP/") || status.starts_with("HTTP/") {
                    let code = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
                    if self.log && !self.quiet {
                        super::log(&format!("← {status} ({} body bytes)", body.len()));
                    }
                    return Ok(Response { code, headers, body });
                }
                // A request from the receiver: acknowledge and keep waiting.
                if self.log {
                    super::log(&format!("← (receiver request) {status}"));
                }
                let cseq = headers.get("cseq").cloned();
                self.respond_ok(cseq.as_deref())?;
                continue;
            }
            if !self.fill()? {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "receiver closed the control channel"));
            }
        }
    }

    /// The minimal 200 OK the receiver wants back for its own requests: no
    /// Content-Length, no Audio-Latency (those corrupt its realtime timeline).
    pub fn respond_ok(&mut self, cseq: Option<&str>) -> std::io::Result<()> {
        let mut resp = String::from("RTSP/1.0 200 OK\r\nServer: AirTunes/550.10\r\n");
        if let Some(c) = cseq {
            resp.push_str(&format!("CSeq: {c}\r\n"));
        }
        resp.push_str("\r\n");
        self.write_all(resp.as_bytes())
    }
}

/// A command the receiver relays to us over the event channel. AirPlay 2
/// wraps Apple's MediaRemote four-character codes in a `POST /command`
/// binary plist: `{type: "sendMediaRemoteCommand", value: "paus", params: …}`.
/// Siri's own commands carry `SenderBundleIdentifier =
/// <com.apple.AssistantServices>` in `params`; the Home app and the HomePod's
/// touch surface use the same envelope, so we do not filter on the sender.
///
/// The HomePod does NOT report its own volume here — a Siri volume change
/// produces no traffic at all, which is why `session.rs` polls for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCommand {
    Play,
    Pause,
    TogglePlayPause,
    Stop,
    Next,
    Previous,
}

impl RemoteCommand {
    /// `None` for a code we have not seen on the desk yet; the caller logs it
    /// so the mapping can grow from evidence rather than guesswork.
    pub fn from_fourcc(value: &str) -> Option<Self> {
        Some(match value {
            "play" => Self::Play,
            "paus" => Self::Pause,
            "togl" => Self::TogglePlayPause,
            "stop" => Self::Stop,
            "nitm" => Self::Next,
            "pitm" => Self::Previous,
            _ => return None,
        })
    }
}

/// Where [`serve_events`] hands decoded commands. Runs on the event thread,
/// so it must not block: `media.rs` calls WinRT, which returns promptly.
pub type CommandSink = std::sync::Arc<dyn Fn(RemoteCommand) + Send + Sync>;

/// The event channel is a REVERSE connection: the receiver pushes encrypted
/// requests at us and we must answer each with a bare 200 or it tears the
/// session down after ~30 s. Its keys are swapped relative to the control
/// channel, so this is the same framing with `read`/`write` chosen by the
/// caller. Runs until the socket closes or `stop` flips.
pub fn serve_events(
    mut stream: TcpStream,
    read_key: [u8; 32],
    write_key: [u8; 32],
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    log: bool,
    on_command: Option<CommandSink>,
) {
    use std::sync::atomic::Ordering;
    stream.set_read_timeout(Some(Duration::from_millis(250))).ok();
    let mut enc_rx = Vec::new();
    let mut plain = Vec::new();
    let (mut read_ctr, mut write_ctr) = (0u64, 0u64);
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => enc_rx.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
        loop {
            if enc_rx.len() < 2 {
                break;
            }
            let len = u16::from_le_bytes([enc_rx[0], enc_rx[1]]) as usize;
            let need = 2 + len + 16;
            if enc_rx.len() < need {
                break;
            }
            let aad = [enc_rx[0], enc_rx[1]];
            match chacha_open(&read_key, &counter_nonce(read_ctr), &enc_rx[2..need], &aad) {
                Some(p) => plain.extend_from_slice(&p),
                None => {
                    if log {
                        super::log(&format!("[events] bad auth tag; closing"));
                    }
                    return;
                }
            }
            read_ctr += 1;
            enc_rx.drain(..need);
        }
        // Answer every complete request in `plain`.
        while let Some(head_end) = plain.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&plain[..head_end]).into_owned();
            let mut content_len = 0usize;
            let mut cseq = None;
            for line in head.split("\r\n").skip(1) {
                if let Some((k, v)) = line.split_once(':') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => content_len = v.trim().parse().unwrap_or(0),
                        "cseq" => cseq = Some(v.trim().to_string()),
                        _ => {}
                    }
                }
            }
            let total = head_end + 4 + content_len;
            if plain.len() < total {
                break;
            }
            let line = head.lines().next().unwrap_or_default().to_string();
            let parsed = super::bplist::decode(&plain[head_end + 4..total]);
            plain.drain(..total);
            if log {
                match &parsed {
                    Some(v) => super::log(&format!("[events] {line} {}", super::bplist::pretty(v))),
                    None => super::log(&format!("[events] {line}")),
                }
            }
            if let Some(v) = &parsed {
                if v.get("type").and_then(super::bplist::Value::as_str) == Some("sendMediaRemoteCommand") {
                    match v.get("value").and_then(super::bplist::Value::as_str) {
                        Some(code) => match RemoteCommand::from_fourcc(code) {
                            Some(cmd) => {
                                super::log(&format!("[events] remote command: {cmd:?}"));
                                if let Some(sink) = &on_command {
                                    sink(cmd);
                                }
                            }
                            None => super::log(&format!("[events] unmapped MediaRemote code {code:?} — add it to RemoteCommand::from_fourcc")),
                        },
                        None => super::log("[events] sendMediaRemoteCommand with no value"),
                    }
                }
            }
            let mut resp = String::from("RTSP/1.0 200 OK\r\nServer: AirTunes/550.10\r\n");
            if let Some(c) = cseq {
                resp.push_str(&format!("CSeq: {c}\r\n"));
            }
            resp.push_str("\r\n");
            let len = (resp.len() as u16).to_le_bytes();
            let sealed = chacha_seal(&write_key, &counter_nonce(write_ctr), resp.as_bytes(), &len);
            write_ctr += 1;
            let mut out = len.to_vec();
            out.extend_from_slice(&sealed);
            if stream.write_all(&out).is_err() {
                return;
            }
        }
    }
}
