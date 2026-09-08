//! Multicast DNS browse for `_airplay._tcp.local`: one query out on
//! 224.0.0.251:5353, then parse every response (PTR, SRV, TXT, A) into a
//! [`Speaker`]. No general DNS resolver here, just what AirPlay advertises.
//!
//! A HomePod answers with its name, the RTSP port (7000), and TXT keys such
//! as `deviceid`, `model`, `features`, `pk`, `pi`, `flags`, `srcvers`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const SERVICE: &str = "_airplay._tcp.local";
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Speaker {
    /// The instance name, e.g. "Living Room".
    pub name: String,
    pub host: String,
    pub ip: Option<Ipv4Addr>,
    pub port: u16,
    pub model: String,
    pub device_id: String,
    pub txt: HashMap<String, String>,
}

impl Speaker {
    /// Feature bits from the TXT `features` ("0x...,0x..." low,high) value.
    pub fn features(&self) -> u64 {
        let Some(f) = self.txt.get("features") else { return 0 };
        let mut parts = f.split(',');
        let parse = |s: Option<&str>| s.and_then(|x| u64::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok()).unwrap_or(0);
        let low = parse(parts.next());
        let high = parse(parts.next());
        (high << 32) | low
    }
    /// Bit 48: the receiver accepts HAP transient pairing (PIN 3939).
    pub fn supports_transient_pairing(&self) -> bool {
        self.features() & (1 << 48) != 0
    }
}

// ── query ───────────────────────────────────────────────────────────────

fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn ptr_query() -> Vec<u8> {
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]); // id 0, flags 0, 1 question
    encode_name(SERVICE, &mut q);
    q.extend_from_slice(&[0, 12]); // PTR
    // IN with the QU (unicast-response) bit: we listen on an ephemeral port,
    // not 5353, so responders must answer us directly. Apple's mDNSResponder
    // honours QU; this is how a sender without a Bonjour daemon browses.
    q.extend_from_slice(&[0x80, 1]);
    q
}

// ── response parsing ────────────────────────────────────────────────────

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(((self.u8()? as u16) << 8) | self.u8()? as u16)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(((self.u16()? as u32) << 16) | self.u16()? as u32)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    /// DNS name with compression pointers; returns dotted labels.
    fn name(&mut self) -> Option<String> {
        let mut labels = Vec::new();
        let mut pos = self.pos;
        let mut jumped = false;
        let mut hops = 0;
        loop {
            let len = *self.data.get(pos)? as usize;
            if len == 0 {
                pos += 1;
                break;
            }
            if len & 0xC0 == 0xC0 {
                let ptr = ((len & 0x3F) << 8) | *self.data.get(pos + 1)? as usize;
                if !jumped {
                    self.pos = pos + 2;
                }
                jumped = true;
                pos = ptr;
                hops += 1;
                if hops > 16 {
                    return None;
                }
                continue;
            }
            labels.push(String::from_utf8_lossy(self.data.get(pos + 1..pos + 1 + len)?).into_owned());
            pos += 1 + len;
        }
        if !jumped {
            self.pos = pos;
        }
        Some(labels.join("."))
    }
}

#[derive(Default)]
struct Partial {
    host: Option<String>,
    port: Option<u16>,
    txt: HashMap<String, String>,
}

/// Fold one mDNS packet into `found`; `hosts` collects A records by hostname.
fn parse_packet(pkt: &[u8], found: &mut HashMap<String, Partial>, hosts: &mut HashMap<String, Ipv4Addr>) -> Option<()> {
    let mut r = Reader { data: pkt, pos: 0 };
    let _id = r.u16()?;
    let flags = r.u16()?;
    if flags & 0x8000 == 0 {
        return None; // a query, not a response
    }
    let qd = r.u16()?;
    let an = r.u16()?;
    let ns = r.u16()?;
    let ar = r.u16()?;
    for _ in 0..qd {
        r.name()?;
        r.bytes(4)?;
    }
    for _ in 0..(an as u32 + ns as u32 + ar as u32) {
        let name = r.name()?;
        let rtype = r.u16()?;
        let _class = r.u16()?;
        let _ttl = r.u32()?;
        let rdlen = r.u16()? as usize;
        let rd_start = r.pos;
        match rtype {
            12 if name.eq_ignore_ascii_case(SERVICE) => {
                let instance = r.name()?;
                found.entry(instance).or_default();
            }
            33 if name.ends_with(SERVICE) => {
                let _prio = r.u16()?;
                let _weight = r.u16()?;
                let port = r.u16()?;
                let target = r.name()?;
                let e = found.entry(name.clone()).or_default();
                e.port = Some(port);
                e.host = Some(target);
            }
            16 if name.ends_with(SERVICE) => {
                let mut txt = HashMap::new();
                let mut p = rd_start;
                while p < rd_start + rdlen {
                    let l = *pkt.get(p)? as usize;
                    let s = String::from_utf8_lossy(pkt.get(p + 1..p + 1 + l)?);
                    if let Some((k, v)) = s.split_once('=') {
                        txt.insert(k.to_string(), v.to_string());
                    }
                    p += 1 + l;
                }
                found.entry(name.clone()).or_default().txt.extend(txt);
            }
            1 if rdlen == 4 => {
                let b = pkt.get(rd_start..rd_start + 4)?;
                hosts.insert(name.clone(), Ipv4Addr::new(b[0], b[1], b[2], b[3]));
            }
            _ => {}
        }
        r.pos = rd_start + rdlen;
    }
    Some(())
}

/// Browse for `timeout`, sending the PTR query every 500 ms so a sleepy
/// HomePod that missed the first one still shows up.
pub fn browse(timeout: Duration) -> std::io::Result<Vec<Speaker>> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(150)))?;
    sock.set_multicast_loop_v4(false).ok();
    let target = SocketAddrV4::new(MDNS_ADDR, MDNS_PORT);
    let query = ptr_query();

    let mut found: HashMap<String, Partial> = HashMap::new();
    let mut hosts: HashMap<String, Ipv4Addr> = HashMap::new();
    let start = Instant::now();
    let mut last_query: Option<Instant> = None;
    let mut buf = [0u8; 9000];
    while start.elapsed() < timeout {
        if last_query.map(|t| t.elapsed() > Duration::from_millis(500)).unwrap_or(true) {
            sock.send_to(&query, target)?;
            last_query = Some(Instant::now());
        }
        if let Ok((n, _from)) = sock.recv_from(&mut buf) {
            parse_packet(&buf[..n], &mut found, &mut hosts);
        }
    }

    let mut out: Vec<Speaker> = found
        .into_iter()
        .filter_map(|(instance, p)| {
            let port = p.port?;
            let host = p.host.unwrap_or_default();
            let name = instance.strip_suffix(&format!(".{SERVICE}")).unwrap_or(&instance).to_string();
            Some(Speaker {
                ip: hosts.get(&host).copied(),
                model: p.txt.get("model").cloned().unwrap_or_default(),
                device_id: p.txt.get("deviceid").cloned().unwrap_or_default(),
                name,
                host,
                port,
                txt: p.txt,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
