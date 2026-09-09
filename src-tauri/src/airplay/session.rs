//! One streaming session to one speaker: the RTSP handshake, then four
//! threads (pacer, timing, control, events) plus a keep-alive on the control
//! channel. `connect` blocks through the handshake so a failure is a plain
//! `Err`; after that everything runs until [`Session::disconnect`].
//!
//! Timeline: the RTP timestamp of each packet is `latency + frames_sent`, the
//! sync packet says "play `frames_sent` at NTP(start + frames_sent)", so the
//! receiver holds exactly `latency` frames of buffer. That number is the
//! latency knob; everything else is fixed by the protocol.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::alac::{self, CHANNELS, FRAMES_PER_PACKET, SAMPLE_RATE};
use super::bplist::{self, Value};
use super::pairing::{self, SessionKeys};
use super::rtp;
use super::rtsp::{self, Channel, Identity};
use crate::crypto::{chacha_seal, counter_nonce, random};

/// Fills a 352-frame interleaved stereo buffer; must pad with silence itself.
pub type Source = Box<dyn FnMut(&mut [i16]) + Send>;

#[derive(Clone)]
pub struct Config {
    /// Receiver buffer in frames (44.1 kHz). 11025 = 250 ms is the floor the
    /// protocol advertises; below that a HomePod starts dropping.
    pub latency_frames: u32,
    pub volume_pct: f64,
    pub client_name: String,
    pub log: bool,
    /// Where transport commands relayed by the receiver (Siri, the Home app,
    /// the HomePod's touch surface) are delivered. `None` ignores them.
    pub on_command: Option<rtsp::CommandSink>,
}

pub const MIN_LATENCY_FRAMES: u32 = 11_025;
pub const MAX_LATENCY_FRAMES: u32 = 88_200;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stats {
    pub packets_sent: u64,
    pub retransmit_requests: u64,
    pub timing_requests: u64,
    /// Packets that went out padded with silence because the source ran dry.
    pub starved_packets: u64,
    /// Keep-alive round trips on the control channel, ms.
    pub rtt_last_ms: f64,
    pub rtt_p95_ms: f64,
    pub seconds: u64,
    pub latency_ms: u32,
}

#[derive(Default)]
struct Counters {
    packets: AtomicU64,
    retransmits: AtomicU64,
    timing: AtomicU64,
    starved: AtomicU64,
}

struct Backlog {
    packets: Vec<Vec<u8>>,
    seqs: Vec<i32>,
}

const BACKLOG: usize = 1024;

impl Backlog {
    fn new() -> Self {
        Self { packets: vec![Vec::new(); BACKLOG], seqs: vec![-1; BACKLOG] }
    }
    fn put(&mut self, seq: u16, pkt: Vec<u8>) {
        let slot = seq as usize & (BACKLOG - 1);
        self.packets[slot] = pkt;
        self.seqs[slot] = seq as i32;
    }
    fn get(&self, seq: u16) -> Option<&[u8]> {
        let slot = seq as usize & (BACKLOG - 1);
        (self.seqs[slot] == seq as i32).then(|| self.packets[slot].as_slice())
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    control: Arc<Mutex<Channel>>,
    identity: Identity,
    uri: String,
    counters: Arc<Counters>,
    rtts: Arc<Mutex<Vec<f64>>>,
    /// The receiver's own volume as of the last poll, 0–100. `None` until the
    /// first successful GET_PARAMETER (or forever, if it does not support it).
    receiver_volume: Arc<Mutex<Option<f64>>>,
    /// When we last pushed a volume, so the poll does not fight the slider.
    volume_set_at: Arc<Mutex<Instant>>,
    started: Instant,
    pub config: Config,
    pub speaker_name: String,
}

fn volume_db(pct: f64) -> f64 {
    let pct = pct.clamp(0.0, 100.0);
    if pct < 0.01 {
        -144.0
    } else {
        (pct * 3.0 - 300.0) / 10.0
    }
}

/// Inverse of [`volume_db`]. The receiver clamps to its own range, so a value
/// outside 0–100 here means it reported something we do not model; clamp it
/// rather than letting the slider jump off the end.
fn volume_pct(db: f64) -> f64 {
    if db <= -144.0 {
        0.0
    } else {
        ((db * 10.0 + 300.0) / 3.0).clamp(0.0, 100.0)
    }
}

/// Read the receiver's current volume. AirPlay volume is the receiver's own
/// gain, not a second one stacked on ours, so this is what the user actually
/// hears — including after a Siri "set the volume to 30 percent", which the
/// HomePod applies locally and never announces.
fn get_volume_on(ch: &mut Channel, id: &Identity, uri: &str) -> Option<f64> {
    // This runs every 2 s for the life of the session, and the log is the
    // first thing read when something goes wrong; keep it out of there.
    ch.quiet = true;
    let reply = ch.request("GET_PARAMETER", uri, "RTSP/1.0", id, &[], Some("text/parameters"), b"volume\r\n");
    ch.quiet = false;
    let r = reply.ok()?;
    if !r.ok() {
        return None;
    }
    let text = String::from_utf8_lossy(&r.body);
    let db: f64 = text.split_once("volume:")?.1.trim().parse().ok()?;
    Some(volume_pct(db))
}

fn set_volume_on(ch: &mut Channel, id: &Identity, uri: &str, pct: f64) -> Result<(), String> {
    let body = format!("volume: {:.6}", volume_db(pct));
    let r = ch
        .request("SET_PARAMETER", uri, "RTSP/1.0", id, &[], Some("text/parameters"), body.as_bytes())
        .map_err(|e| format!("SET_PARAMETER: {e}"))?;
    if !r.ok() {
        return Err(format!("SET_PARAMETER volume returned {}", r.code));
    }
    Ok(())
}

fn bind_udp() -> std::io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
}

/// Ask the OS for a 1 ms timer tick and Pro Audio scheduling on this thread.
pub fn realtime_thread(name: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::timeBeginPeriod;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe {
        timeBeginPeriod(1);
        let mut idx = 0u32;
        let wide: Vec<u16> = "Pro Audio".encode_utf16().chain(std::iter::once(0)).collect();
        if AvSetMmThreadCharacteristicsW(PCWSTR(wide.as_ptr()), &mut idx).is_err() {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL).ok();
        }
    }
    let _ = name;
}

pub fn connect(ip: Ipv4Addr, port: u16, speaker_name: &str, config: Config, mut source: Source) -> Result<Session, String> {
    let log = config.log;
    let latency = config.latency_frames.clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES);
    let identity = Identity {
        dacp_id: format!("{:X}", random::u64()),
        active_remote: random::u32(),
        client_name: config.client_name.clone(),
    };
    let session_id = random::u32();
    let session_uuid = random::uuid_v4().to_uppercase();

    // UDP first: the SETUP bodies advertise our timing/control ports.
    let timing_sock = bind_udp().map_err(|e| format!("bind timing socket: {e}"))?;
    let control_sock = bind_udp().map_err(|e| format!("bind control socket: {e}"))?;
    let audio_sock = bind_udp().map_err(|e| format!("bind audio socket: {e}"))?;
    let timing_port = timing_sock.local_addr().map_err(|e| e.to_string())?.port();
    let control_port = control_sock.local_addr().map_err(|e| e.to_string())?.port();

    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Counters::default());
    let mut threads = Vec::new();

    // Timing responder FIRST. The receiver sends NTP requests to our timing
    // port and waits for the answers before it replies to the session SETUP;
    // a socket nobody reads stalls the handshake right there.
    {
        let (st, c) = (stop.clone(), counters.clone());
        timing_sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
        threads.push(
            std::thread::Builder::new()
                .name("ap-timing".into())
                .spawn(move || {
                    realtime_thread("timing");
                    let mut buf = [0u8; 64];
                    while !st.load(Ordering::Relaxed) {
                        if let Ok((n, from)) = timing_sock.recv_from(&mut buf) {
                            if let Some(reply) = rtp::timing_reply(&buf[..n], rtp::ntp_now()) {
                                timing_sock.send_to(&reply, from).ok();
                                c.timing.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
    }

    let addr = SocketAddr::V4(SocketAddrV4::new(ip, port));
    let mut ch = Channel::connect(addr, Duration::from_secs(5)).map_err(|e| format!("connect {addr}: {e}"))?;
    ch.log = log;
    let local_ip = ch.local_ip();
    let uri = format!("rtsp://{local_ip}/{session_id}");
    if log {
        super::log(&format!("[session] connected to {addr}, local {local_ip}, uri {uri}"));
    }

    // 1. Pair. Control channel is encrypted from here on.
    let keys: SessionKeys = pairing::transient_pair_setup(&mut ch, &identity)?;
    if log {
        super::log(&format!("[session] transient pairing ok; control channel encrypted"));
    }

    // 2. GET /info.
    let info = ch.request("GET", "/info", "RTSP/1.0", &identity, &[], None, &[]).map_err(|e| format!("GET /info: {e}"))?;
    if !info.ok() {
        return Err(format!("GET /info returned {}", info.code));
    }
    if log {
        if let Some(v) = bplist::decode(&info.body) {
            super::log(&format!("[session] /info: {}", bplist::pretty(&v)));
        }
    }

    // 3. Session SETUP: the minimal body OwnTone and airplay2-rs use with
    // HomePods — who we are and NTP timing on our port. (pyatv sends a dozen
    // more identity keys; the receiver does not need them.)
    let mac = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        (session_id >> 24) as u8 | 0x02, (session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8, 0xDE, 0xE7
    );
    let setup1 = bplist::dict(vec![
        ("deviceID", Value::Str(mac)),
        ("sessionUUID", Value::Str(session_uuid)),
        ("timingPort", Value::Int(timing_port as i64)),
        ("timingProtocol", Value::Str("NTP".into())),
    ]);
    let r = ch
        .request(
            "SETUP",
            &uri,
            "RTSP/1.0",
            &identity,
            &[("X-Apple-StreamID", "1".to_string())],
            Some("application/x-apple-binary-plist"),
            &bplist::encode(&setup1),
        )
        .map_err(|e| format!("SETUP (session): {e} — the receiver waits for our NTP timing replies before answering; {} timing request(s) reached us. If that is 0, allow the probe through Windows Firewall (inbound UDP).", counters.timing.load(Ordering::Relaxed)))?;
    if !r.ok() {
        return Err(format!("SETUP (session) returned {}", r.code));
    }
    let reply = bplist::decode(&r.body).ok_or("SETUP (session): reply is not a bplist")?;
    if log {
        super::log(&format!("[session] session SETUP: {}", bplist::pretty(&reply)));
    }
    let event_port = reply.get("eventPort").and_then(Value::as_int).unwrap_or(0) as u16;
    if log {
        super::log(&format!("[session] timing requests answered so far: {} (0 here usually means Windows Firewall is dropping the receiver's UDP)", counters.timing.load(Ordering::Relaxed)));
    }

    // 4. Event channel: reverse connection the receiver talks on.
    if event_port != 0 {
        let ev = std::net::TcpStream::connect_timeout(&SocketAddr::V4(SocketAddrV4::new(ip, event_port)), Duration::from_secs(5))
            .map_err(|e| format!("event channel {ip}:{event_port}: {e}"))?;
        let (rk, wk, st) = (keys.event_read, keys.event_write, stop.clone());
        let sink = config.on_command.clone();
        threads.push(
            std::thread::Builder::new()
                .name("ap-events".into())
                .spawn(move || rtsp::serve_events(ev, rk, wk, st, log, sink))
                .map_err(|e| e.to_string())?,
        );
        if log {
            super::log(&format!("[session] event channel open on {event_port}"));
        }
    }

    // 5. RECORD (bare). Some receivers answer 500 here; that is non-fatal.
    let r = ch.request("RECORD", &uri, "RTSP/1.0", &identity, &[], None, &[]).map_err(|e| format!("RECORD: {e}"))?;
    if log {
        super::log(&format!("[session] RECORD → {}", r.code));
    }

    // 6. Stream SETUP: realtime ALAC, our control port, the audio key.
    let stream = bplist::dict(vec![
        ("audioFormat", Value::Int(0x40000)),
        ("audioMode", Value::Str("default".into())),
        ("controlPort", Value::Int(control_port as i64)),
        ("ct", Value::Int(2)),
        ("isMedia", Value::Bool(true)),
        ("latencyMax", Value::Int(MAX_LATENCY_FRAMES as i64)),
        ("latencyMin", Value::Int(MIN_LATENCY_FRAMES as i64)),
        ("shk", Value::Data(keys.audio.to_vec())),
        ("spf", Value::Int(FRAMES_PER_PACKET as i64)),
        ("sr", Value::Int(SAMPLE_RATE as i64)),
        ("type", Value::Int(0x60)),
        ("supportsDynamicStreamID", Value::Bool(false)),
        ("streamConnectionID", Value::Int(session_id as i64)),
    ]);
    let setup2 = bplist::dict(vec![("streams", Value::Array(vec![stream]))]);
    let r = ch
        .request(
            "SETUP",
            &uri,
            "RTSP/1.0",
            &identity,
            &[("X-Apple-StreamID", "1".to_string())],
            Some("application/x-apple-binary-plist"),
            &bplist::encode(&setup2),
        )
        .map_err(|e| format!("SETUP (stream): {e}"))?;
    if !r.ok() {
        return Err(format!("SETUP (stream) returned {}", r.code));
    }
    let reply = bplist::decode(&r.body).ok_or("SETUP (stream): reply is not a bplist")?;
    if log {
        super::log(&format!("[session] stream SETUP: {}", bplist::pretty(&reply)));
    }
    let s0 = reply.get("streams").and_then(Value::as_array).and_then(|a| a.first()).ok_or("SETUP (stream): no streams in reply")?;
    let data_port = s0.get("dataPort").and_then(Value::as_int).unwrap_or(0) as u16;
    let mut rx_control_port = s0.get("controlPort").and_then(Value::as_int).unwrap_or(0) as u16;
    if data_port == 0 {
        return Err("SETUP (stream): no dataPort".into());
    }
    if rx_control_port == 0 {
        rx_control_port = data_port;
    }

    // 7. Volume, then stream.
    set_volume_on(&mut ch, &identity, &uri, config.volume_pct)?;

    let backlog = Arc::new(Mutex::new(Backlog::new()));
    let audio_key = keys.audio;
    let data_addr = SocketAddrV4::new(ip, data_port);
    let ctrl_addr = SocketAddrV4::new(ip, rx_control_port);

    // Control receiver: retransmit requests come back on our control port.
    let control_tx = control_sock.try_clone().map_err(|e| e.to_string())?;
    {
        let (st, c, bl) = (stop.clone(), counters.clone(), backlog.clone());
        control_sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
        threads.push(
            std::thread::Builder::new()
                .name("ap-control".into())
                .spawn(move || {
                    let mut buf = [0u8; 64];
                    while !st.load(Ordering::Relaxed) {
                        let Ok((n, from)) = control_sock.recv_from(&mut buf) else { continue };
                        let Some((first, count)) = rtp::parse_retransmit_request(&buf[..n]) else { continue };
                        c.retransmits.fetch_add(1, Ordering::Relaxed);
                        let bl = bl.lock().unwrap();
                        for i in 0..count {
                            let seq = first.wrapping_add(i);
                            if let Some(pkt) = bl.get(seq) {
                                control_sock.send_to(&rtp::retransmit_reply(seq, pkt), from).ok();
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
    }

    // Pacer: the audio clock. Wall-clock token bucket at 44.1 kHz.
    {
        let (st, c, bl) = (stop.clone(), counters.clone(), backlog.clone());
        threads.push(
            std::thread::Builder::new()
                .name("ap-pacer".into())
                .spawn(move || {
                    realtime_thread("pacer");
                    let mut seq: u16 = random::u16();
                    let mut nonce: u64 = 0;
                    let start_ts = rtp::ntp_to_ts(rtp::ntp_now());
                    let clock_start = Instant::now();
                    let mut frames_sent: u64 = 0;
                    let mut first = true;
                    let mut last_sync = Instant::now() - Duration::from_secs(2);
                    let mut pcm = vec![0i16; FRAMES_PER_PACKET * CHANNELS];
                    while !st.load(Ordering::Relaxed) {
                        let now_rtp = (latency as u64 + frames_sent) as u32;
                        if last_sync.elapsed() >= Duration::from_secs(1) {
                            let ntp = rtp::ts_to_ntp(start_ts + frames_sent);
                            control_tx.send_to(&rtp::sync_packet(first, now_rtp, latency, ntp), ctrl_addr).ok();
                            last_sync = Instant::now();
                        }
                        let target = clock_start.elapsed().as_nanos() as u64 * SAMPLE_RATE as u64 / 1_000_000_000;
                        let mut burst = 0;
                        while frames_sent + FRAMES_PER_PACKET as u64 <= target && burst < 16 {
                            pcm.iter_mut().for_each(|s| *s = 0);
                            source(&mut pcm);
                            if pcm.iter().all(|s| *s == 0) {
                                c.starved.fetch_add(1, Ordering::Relaxed);
                            }
                            let rtptime = (latency as u64 + frames_sent) as u32;
                            let header = rtp::audio_header(first, seq, rtptime, session_id);
                            let payload = alac::pack_frame(&pcm);
                            let n8 = counter_nonce(nonce);
                            nonce += 1;
                            let mut pkt = header.to_vec();
                            pkt.extend_from_slice(&chacha_seal(&audio_key, &n8, &payload, &header[4..12]));
                            pkt.extend_from_slice(&n8);
                            audio_sock.send_to(&pkt, data_addr).ok();
                            bl.lock().unwrap().put(seq, pkt);
                            c.packets.fetch_add(1, Ordering::Relaxed);
                            seq = seq.wrapping_add(1);
                            frames_sent += FRAMES_PER_PACKET as u64;
                            first = false;
                            burst += 1;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                })
                .map_err(|e| e.to_string())?,
        );
    }

    // Keep-alive: POST /feedback every 2 s, timed, on the shared channel.
    ch.set_read_timeout(Some(Duration::from_secs(5)));
    let control = Arc::new(Mutex::new(ch));
    let rtts = Arc::new(Mutex::new(Vec::<f64>::new()));
    let receiver_volume = Arc::new(Mutex::new(None::<f64>));
    let volume_set_at = Arc::new(Mutex::new(Instant::now()));
    {
        let (st, ctl, id, r) = (stop.clone(), control.clone(), identity.clone(), rtts.clone());
        let (rv, vsa, poll_uri) = (receiver_volume.clone(), volume_set_at.clone(), uri.clone());
        threads.push(
            std::thread::Builder::new()
                .name("ap-keepalive".into())
                .spawn(move || {
                    let mut last = Instant::now();
                    // Stop asking once the receiver has shown it will not answer.
                    let mut volume_readable = true;
                    while !st.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        if last.elapsed() < Duration::from_secs(2) {
                            continue;
                        }
                        last = Instant::now();
                        let t0 = Instant::now();
                        let ok = ctl.lock().unwrap().request("POST", "/feedback", "RTSP/1.0", &id, &[], None, &[]).is_ok();
                        if ok {
                            let mut v = r.lock().unwrap();
                            v.push(t0.elapsed().as_secs_f64() * 1000.0);
                            if v.len() > 60 {
                                v.remove(0);
                            }
                        } else if log {
                            super::log(&format!("[session] keep-alive failed"));
                        }
                        // A volume we pushed ourselves is still settling; reading
                        // it back now would fight the slider the user is dragging.
                        if !ok || !volume_readable || vsa.lock().unwrap().elapsed() < Duration::from_secs(2) {
                            continue;
                        }
                        match get_volume_on(&mut ctl.lock().unwrap(), &id, &poll_uri) {
                            Some(pct) => {
                                let mut cur = rv.lock().unwrap();
                                if cur.map(|c: f64| (c - pct).abs() >= 0.5).unwrap_or(true) && log {
                                    super::log(&format!("[session] receiver volume {pct:.0}%"));
                                }
                                *cur = Some(pct);
                            }
                            None => {
                                volume_readable = false;
                                if log {
                                    super::log("[session] receiver does not answer GET_PARAMETER volume; the slider will not follow it");
                                }
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
    }

    if log {
        super::log(&format!("[session] streaming: data {data_addr}, control {ctrl_addr}, latency {latency} frames ({} ms)", latency * 1000 / SAMPLE_RATE));
    }
    Ok(Session {
        stop,
        threads,
        control,
        identity,
        uri,
        counters,
        rtts,
        receiver_volume,
        volume_set_at,
        started: Instant::now(),
        config: Config { latency_frames: latency, ..config },
        speaker_name: speaker_name.to_string(),
    })
}

impl Session {
    pub fn set_volume(&mut self, pct: f64) -> Result<(), String> {
        self.config.volume_pct = pct;
        *self.volume_set_at.lock().unwrap() = Instant::now();
        *self.receiver_volume.lock().unwrap() = Some(pct);
        let mut ch = self.control.lock().unwrap();
        set_volume_on(&mut ch, &self.identity, &self.uri, pct)
    }

    /// The receiver's own volume, 0–100, as of the last poll. Changes made on
    /// the HomePod itself (Siri, the touch surface) show up here within ~2 s.
    pub fn receiver_volume_pct(&self) -> Option<f64> {
        *self.receiver_volume.lock().unwrap()
    }

    pub fn stats(&self) -> Stats {
        let rtts = self.rtts.lock().unwrap();
        let mut sorted = rtts.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p95 = if sorted.is_empty() { 0.0 } else { sorted[((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1)] };
        Stats {
            packets_sent: self.counters.packets.load(Ordering::Relaxed),
            retransmit_requests: self.counters.retransmits.load(Ordering::Relaxed),
            timing_requests: self.counters.timing.load(Ordering::Relaxed),
            starved_packets: self.counters.starved.load(Ordering::Relaxed),
            rtt_last_ms: rtts.last().copied().unwrap_or(0.0),
            rtt_p95_ms: p95,
            seconds: self.started.elapsed().as_secs(),
            latency_ms: self.config.latency_frames * 1000 / SAMPLE_RATE,
        }
    }

    /// Has the receiver dropped us? True once a keep-alive has failed.
    pub fn alive(&self) -> bool {
        self.threads.iter().all(|t| !t.is_finished())
    }

    pub fn disconnect(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        {
            let mut ch = self.control.lock().unwrap();
            ch.set_read_timeout(Some(Duration::from_secs(2)));
            ch.request("TEARDOWN", &self.uri, "RTSP/1.0", &self.identity, &[], None, &[]).ok();
        }
        for t in self.threads.drain(..) {
            t.join().ok();
        }
    }
}
