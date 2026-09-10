//! CSPRNG from the OS (BCrypt's system-preferred RNG). Used for the SRP
//! ephemeral, session ids, the RTP sequence start and SSRC.

use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

pub fn fill(buf: &mut [u8]) {
    // NTSTATUS 0 is success; anything else means the OS RNG is unavailable,
    // which is not a state worth continuing from for key material.
    let status = unsafe { BCryptGenRandom(None, buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(status.is_ok(), "BCryptGenRandom failed: {status:?}");
}

pub fn bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    fill(&mut b);
    b
}

pub fn u16() -> u16 {
    u16::from_le_bytes(bytes::<2>())
}

pub fn u32() -> u32 {
    u32::from_le_bytes(bytes::<4>())
}

pub fn u64() -> u64 {
    u64::from_le_bytes(bytes::<8>())
}

/// Lowercase RFC 4122 v4 UUID text.
pub fn uuid_v4() -> String {
    let mut b = bytes::<16>();
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h = |s: &[u8]| s.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(&b[0..4]), h(&b[4..6]), h(&b[6..8]), h(&b[8..10]), h(&b[10..16]))
}
