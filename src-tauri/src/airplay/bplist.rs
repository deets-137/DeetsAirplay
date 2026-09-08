//! Apple binary property list (`bplist00`), the subset AirPlay 2 uses:
//! bool, integer, real, string, data, array, dict. The encoder writes 4-byte
//! object refs and 4-byte offsets (simple, and receivers accept it); the
//! decoder handles any ref/offset width.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<Value>),
    Dict(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Real(r) => Some(*r as i64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
}

/// Convenience for building dict literals at call sites.
pub fn dict(items: Vec<(&str, Value)>) -> Value {
    Value::Dict(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

// ── encode ──────────────────────────────────────────────────────────────

struct Encoder {
    objects: Vec<Vec<u8>>,
}

impl Encoder {
    fn marker_len(out: &mut Vec<u8>, marker: u8, len: usize) {
        if len < 15 {
            out.push(marker | len as u8);
        } else {
            out.push(marker | 0x0F);
            Self::int(out, len as i64);
        }
    }

    fn int(out: &mut Vec<u8>, v: i64) {
        if (0..=0xFF).contains(&v) {
            out.push(0x10);
            out.push(v as u8);
        } else if (0..=0xFFFF).contains(&v) {
            out.push(0x11);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        } else if (0..=0xFFFF_FFFF).contains(&v) {
            out.push(0x12);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        } else {
            out.push(0x13);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    /// Append `v` to the object table; returns its index.
    fn add(&mut self, v: &Value) -> u32 {
        let idx = self.objects.len() as u32;
        self.objects.push(Vec::new()); // reserve the slot so children come after
        let mut obj = Vec::new();
        match v {
            Value::Bool(b) => obj.push(if *b { 0x09 } else { 0x08 }),
            Value::Int(i) => Self::int(&mut obj, *i),
            Value::Real(r) => {
                obj.push(0x23);
                obj.extend_from_slice(&r.to_bits().to_be_bytes());
            }
            Value::Str(s) => {
                if s.is_ascii() {
                    Self::marker_len(&mut obj, 0x50, s.len());
                    obj.extend_from_slice(s.as_bytes());
                } else {
                    let utf16: Vec<u16> = s.encode_utf16().collect();
                    Self::marker_len(&mut obj, 0x60, utf16.len());
                    for u in utf16 {
                        obj.extend_from_slice(&u.to_be_bytes());
                    }
                }
            }
            Value::Data(d) => {
                Self::marker_len(&mut obj, 0x40, d.len());
                obj.extend_from_slice(d);
            }
            Value::Array(items) => {
                let refs: Vec<u32> = items.iter().map(|i| self.add(i)).collect();
                Self::marker_len(&mut obj, 0xA0, refs.len());
                for r in refs {
                    obj.extend_from_slice(&r.to_be_bytes());
                }
            }
            Value::Dict(items) => {
                let keys: Vec<u32> = items.iter().map(|(k, _)| self.add(&Value::Str(k.clone()))).collect();
                let vals: Vec<u32> = items.iter().map(|(_, v)| self.add(v)).collect();
                Self::marker_len(&mut obj, 0xD0, items.len());
                for r in keys.into_iter().chain(vals) {
                    obj.extend_from_slice(&r.to_be_bytes());
                }
            }
        }
        self.objects[idx as usize] = obj;
        idx
    }
}

pub fn encode(root: &Value) -> Vec<u8> {
    let mut enc = Encoder { objects: Vec::new() };
    enc.add(root);
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(enc.objects.len());
    for obj in &enc.objects {
        offsets.push(out.len() as u32);
        out.extend_from_slice(obj);
    }
    let table_offset = out.len() as u64;
    for off in &offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    let mut trailer = [0u8; 32];
    trailer[6] = 4; // offset int size
    trailer[7] = 4; // object ref size
    trailer[8..16].copy_from_slice(&(enc.objects.len() as u64).to_be_bytes());
    trailer[16..24].copy_from_slice(&0u64.to_be_bytes()); // top object = 0
    trailer[24..32].copy_from_slice(&table_offset.to_be_bytes());
    out.extend_from_slice(&trailer);
    out
}

// ── decode ──────────────────────────────────────────────────────────────

struct Decoder<'a> {
    data: &'a [u8],
    offsets: Vec<usize>,
    ref_size: usize,
}

impl<'a> Decoder<'a> {
    fn read_uint(&self, at: usize, size: usize) -> Option<u64> {
        let s = self.data.get(at..at + size)?;
        Some(s.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64))
    }

    /// Marker low nibble, or the following int object when it is 0xF.
    fn marker_len(&self, at: usize) -> Option<(usize, usize)> {
        let m = *self.data.get(at)?;
        let low = (m & 0x0F) as usize;
        if low != 0x0F {
            return Some((low, at + 1));
        }
        let im = *self.data.get(at + 1)?;
        let size = 1usize << (im & 0x0F);
        let len = self.read_uint(at + 2, size)? as usize;
        Some((len, at + 2 + size))
    }

    fn object(&self, idx: usize, depth: usize) -> Option<Value> {
        if depth > 64 {
            return None;
        }
        let at = *self.offsets.get(idx)?;
        let m = *self.data.get(at)?;
        match m >> 4 {
            0x0 => match m {
                0x08 => Some(Value::Bool(false)),
                0x09 => Some(Value::Bool(true)),
                _ => None,
            },
            0x1 => {
                let size = 1usize << (m & 0x0F);
                Some(Value::Int(self.read_uint(at + 1, size)? as i64))
            }
            0x2 => match m & 0x0F {
                2 => Some(Value::Real(f32::from_bits(self.read_uint(at + 1, 4)? as u32) as f64)),
                3 => Some(Value::Real(f64::from_bits(self.read_uint(at + 1, 8)?))),
                _ => None,
            },
            0x4 => {
                let (len, start) = self.marker_len(at)?;
                Some(Value::Data(self.data.get(start..start + len)?.to_vec()))
            }
            0x5 => {
                let (len, start) = self.marker_len(at)?;
                Some(Value::Str(String::from_utf8_lossy(self.data.get(start..start + len)?).into_owned()))
            }
            0x6 => {
                let (len, start) = self.marker_len(at)?;
                let bytes = self.data.get(start..start + len * 2)?;
                let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                Some(Value::Str(String::from_utf16_lossy(&units)))
            }
            0xA => {
                let (len, start) = self.marker_len(at)?;
                let mut items = Vec::with_capacity(len);
                for i in 0..len {
                    let r = self.read_uint(start + i * self.ref_size, self.ref_size)? as usize;
                    items.push(self.object(r, depth + 1)?);
                }
                Some(Value::Array(items))
            }
            0xD => {
                let (len, start) = self.marker_len(at)?;
                let mut items = Vec::with_capacity(len);
                for i in 0..len {
                    let kr = self.read_uint(start + i * self.ref_size, self.ref_size)? as usize;
                    let vr = self.read_uint(start + (len + i) * self.ref_size, self.ref_size)? as usize;
                    let key = match self.object(kr, depth + 1)? {
                        Value::Str(s) => s,
                        _ => return None,
                    };
                    items.push((key, self.object(vr, depth + 1)?));
                }
                Some(Value::Dict(items))
            }
            _ => None, // dates, sets, UIDs: not used by AirPlay
        }
    }
}

pub fn decode(data: &[u8]) -> Option<Value> {
    if data.len() < 40 || &data[..8] != b"bplist00" {
        return None;
    }
    let trailer = &data[data.len() - 32..];
    let offset_size = trailer[6] as usize;
    let ref_size = trailer[7] as usize;
    let count = u64::from_be_bytes(trailer[8..16].try_into().ok()?) as usize;
    let top = u64::from_be_bytes(trailer[16..24].try_into().ok()?) as usize;
    let table = u64::from_be_bytes(trailer[24..32].try_into().ok()?) as usize;
    if offset_size == 0 || ref_size == 0 || count > 1_000_000 {
        return None;
    }
    let mut d = Decoder { data, offsets: Vec::with_capacity(count), ref_size };
    for i in 0..count {
        let off = d.read_uint(table + i * offset_size, offset_size)? as usize;
        d.offsets.push(off);
    }
    d.object(top, 0)
}

/// Debug rendering for the probe log: one line, keys sorted.
pub fn pretty(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Data(d) => format!("<{} bytes>", d.len()),
        Value::Array(a) => format!("[{}]", a.iter().map(pretty).collect::<Vec<_>>().join(", ")),
        Value::Dict(items) => {
            let sorted: BTreeMap<&String, &Value> = items.iter().map(|(k, v)| (k, v)).collect();
            format!(
                "{{{}}}",
                sorted.iter().map(|(k, v)| format!("{k}: {}", pretty(v))).collect::<Vec<_>>().join(", ")
            )
        }
    }
}
