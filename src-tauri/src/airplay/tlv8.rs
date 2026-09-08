//! HomeKit TLV8: a flat list of (type, len <= 255, value) records, values
//! longer than 255 bytes split into consecutive same-type records.

pub const METHOD: u8 = 0x00;
pub const SALT: u8 = 0x02;
pub const PUBLIC_KEY: u8 = 0x03;
pub const PROOF: u8 = 0x04;
pub const STATE: u8 = 0x06;
pub const ERROR: u8 = 0x07;
pub const FLAGS: u8 = 0x13;

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[*tag, 0]);
            continue;
        }
        for chunk in value.chunks(255) {
            out.push(*tag);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Decode into (type, joined value) pairs; fragments of one type are merged.
pub fn decode(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut i = 0;
    while i + 2 <= data.len() {
        let tag = data[i];
        let len = data[i + 1] as usize;
        i += 2;
        let end = (i + len).min(data.len());
        let chunk = &data[i..end];
        i = end;
        match out.last_mut() {
            Some((t, v)) if *t == tag && v.len() % 255 == 0 && !v.is_empty() => v.extend_from_slice(chunk),
            _ => out.push((tag, chunk.to_vec())),
        }
    }
    out
}

pub fn get<'a>(items: &'a [(u8, Vec<u8>)], tag: u8) -> Option<&'a [u8]> {
    items.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_slice())
}
