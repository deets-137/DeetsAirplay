//! The RTP family on the wire: audio packets, sync (0x54), timing reply
//! (0x53), retransmit reply (0x56) and the retransmit request we parse
//! (0x55). Everything is big-endian. NTP <-> RTP-timestamp math is the
//! classic RAOP formula.

use super::alac::SAMPLE_RATE;

/// Wall clock in 64-bit NTP: seconds since 1900 high, 2^32 fraction low.
pub fn ntp_now() -> u64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let sec = d.as_secs() + 0x83AA_7E80;
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (sec << 32) | frac
}

pub fn ntp_to_ts(ntp: u64) -> u64 {
    ((ntp >> 16) * SAMPLE_RATE as u64) >> 16
}

pub fn ts_to_ntp(ts: u64) -> u64 {
    ((ts << 16) / SAMPLE_RATE as u64) << 16
}

/// 12-byte audio header. `marker` only on the first packet of a stream.
pub fn audio_header(marker: bool, seq: u16, rtptime: u32, ssrc: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = if marker { 0xE0 } else { 0x60 };
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&rtptime.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// Sync packet (control port, once a second and once at start with the
/// marker). `now` is the latency-shifted RTP time the audio headers carry;
/// `now - latency` is where the receiver should be PLAYING at `ntp`.
pub fn sync_packet(first: bool, now_rtp: u32, latency: u32, ntp: u64) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = if first { 0x90 } else { 0x80 };
    p[1] = 0xD4;
    p[2..4].copy_from_slice(&7u16.to_be_bytes());
    p[4..8].copy_from_slice(&now_rtp.wrapping_sub(latency).to_be_bytes());
    p[8..12].copy_from_slice(&((ntp >> 32) as u32).to_be_bytes());
    p[12..16].copy_from_slice(&(ntp as u32).to_be_bytes());
    p[16..20].copy_from_slice(&now_rtp.to_be_bytes());
    p
}

/// Reply to a 32-byte timing request: echo its send time as our reference,
/// stamp receive and send with the same `now`.
pub fn timing_reply(request: &[u8], now: u64) -> Option<[u8; 32]> {
    if request.len() < 32 {
        return None;
    }
    let mut p = [0u8; 32];
    p[0] = request[0];
    p[1] = 0xD3;
    p[2..4].copy_from_slice(&7u16.to_be_bytes());
    p[8..16].copy_from_slice(&request[24..32]);
    p[16..20].copy_from_slice(&((now >> 32) as u32).to_be_bytes());
    p[20..24].copy_from_slice(&(now as u32).to_be_bytes());
    p[24..28].copy_from_slice(&((now >> 32) as u32).to_be_bytes());
    p[28..32].copy_from_slice(&(now as u32).to_be_bytes());
    Some(p)
}

/// A retransmit request names (first lost seq, count).
pub fn parse_retransmit_request(dg: &[u8]) -> Option<(u16, u16)> {
    if dg.len() < 8 || dg[1] & 0x7F != 0x55 {
        return None;
    }
    Some((u16::from_be_bytes([dg[4], dg[5]]), u16::from_be_bytes([dg[6], dg[7]])))
}

/// Retransmit reply: 0x80 0xD6, the original seq, then the whole packet.
pub fn retransmit_reply(seq: u16, original: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + original.len());
    p.extend_from_slice(&[0x80, 0xD6]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(original);
    p
}
