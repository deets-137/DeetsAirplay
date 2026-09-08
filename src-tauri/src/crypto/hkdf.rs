//! HMAC-SHA512 and HKDF-SHA512 (RFC 2104 / RFC 5869), the ~40 lines HAP key
//! derivation needs. Every AirPlay 2 channel key is
//! `HKDF(secret, salt = "<Channel>-Salt", info = "<Channel>-<Dir>-Encryption-Key", 32)`.

use sha2::{Digest, Sha512};

const BLOCK: usize = 128; // SHA-512 block size

pub fn hmac_sha512(key: &[u8], msg: &[u8]) -> [u8; 64] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..64].copy_from_slice(&Sha512::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha512::new().chain_update(&ipad).chain_update(msg).finalize();
    let outer = Sha512::new().chain_update(&opad).chain_update(inner).finalize();
    outer.into()
}

pub fn hkdf_sha512(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hmac_sha512(salt, ikm);
    let mut out = Vec::with_capacity(len + 64);
    let mut prev: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < len {
        let mut msg = prev.clone();
        msg.extend_from_slice(info);
        msg.push(counter);
        prev = hmac_sha512(&prk, &msg).to_vec();
        out.extend_from_slice(&prev);
        counter += 1;
    }
    out.truncate(len);
    out
}

/// One 32-byte HAP channel key.
pub fn channel_key(secret: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let v = hkdf_sha512(salt.as_bytes(), secret, info.as_bytes(), 32);
    let mut k = [0u8; 32];
    k.copy_from_slice(&v);
    k
}
