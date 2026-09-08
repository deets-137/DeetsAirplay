//! The arithmetic DeetsAirplay does NOT hand-roll lives behind these modules:
//! SHA-512 (`sha2`), ChaCha20-Poly1305 (`chacha20poly1305`) and big-integer
//! modpow (`num-bigint`). Everything protocol-shaped on top of them (HMAC,
//! HKDF, the SRP-6a flow, the HAP framing) is written here.

pub mod hkdf;
pub mod random;
pub mod srp;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;

/// HAP-style ChaCha20-Poly1305: the 12-byte IETF nonce is 4 zero bytes then
/// the caller's 8-byte nonce (a little-endian counter, or an ASCII label in
/// the pair-setup messages). Output is ciphertext || 16-byte tag.
pub fn chacha_seal(key: &[u8; 32], nonce8: &[u8; 8], plain: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(nonce8);
    cipher
        .encrypt((&nonce).into(), Payload { msg: plain, aad })
        .expect("chacha20poly1305 encrypt cannot fail on in-memory data")
}

/// Inverse of [`chacha_seal`]; `None` when the tag does not verify.
pub fn chacha_open(key: &[u8; 32], nonce8: &[u8; 8], sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 16 {
        return None;
    }
    let cipher = ChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(nonce8);
    cipher.decrypt((&nonce).into(), Payload { msg: sealed, aad }).ok()
}

/// The 8-byte little-endian counter nonce every HAP channel uses.
pub fn counter_nonce(counter: u64) -> [u8; 8] {
    counter.to_le_bytes()
}
