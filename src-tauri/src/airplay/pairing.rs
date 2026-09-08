//! HAP transient pair-setup, the HomePod path: SRP-6a with PIN 3939 over
//! `POST /pair-setup` with `X-Apple-HKP: 4`, four messages, no pair-verify.
//! The result is K (64 bytes), from which the control channel, event channel
//! and audio keys all derive.

use super::rtsp::{Channel, Identity};
use super::tlv8;
use crate::crypto::hkdf::channel_key;
use crate::crypto::srp::SrpClient;

pub struct SessionKeys {
    /// K = SHA-512(S), the whole thing; HKDF input for the channels.
    pub shared_secret: [u8; 64],
    pub control_write: [u8; 32],
    pub control_read: [u8; 32],
    /// Event channel: swapped, because the receiver is the one writing.
    pub event_read: [u8; 32],
    pub event_write: [u8; 32],
    /// The audio `shk`: first 32 bytes of K, raw. No HKDF.
    pub audio: [u8; 32],
}

const TRANSIENT_PIN: &str = "3939";

fn post(ch: &mut Channel, id: &Identity, body: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let resp = ch
        .request(
            "POST",
            "/pair-setup",
            "HTTP/1.1",
            id,
            &[("X-Apple-HKP", "4".to_string()), ("Connection", "keep-alive".to_string())],
            Some("application/octet-stream"),
            body,
        )
        .map_err(|e| format!("pair-setup: {e}"))?;
    match resp.code {
        200..=299 => {}
        470 => return Err("the speaker refuses PIN-less pairing (470). In the Home app set Allow Speaker & TV Access to Everyone or Anyone on the Same Network.".into()),
        403 => return Err("the speaker refused pairing (403). Check the Home app access setting.".into()),
        c => return Err(format!("pair-setup returned {c}")),
    }
    let items = tlv8::decode(&resp.body);
    if let Some(err) = tlv8::get(&items, tlv8::ERROR) {
        return Err(format!("pair-setup error {}", err.first().copied().unwrap_or(0)));
    }
    Ok(items)
}

/// Run M1..M4 on an (still plaintext) control channel. On success the
/// channel is switched to encrypted framing and the keys are returned.
pub fn transient_pair_setup(ch: &mut Channel, id: &Identity) -> Result<SessionKeys, String> {
    // M1: State 1, Method 0 (PairSetup), Flags 0x10 (transient).
    let m1 = tlv8::encode(&[(tlv8::METHOD, &[0x00]), (tlv8::STATE, &[0x01]), (tlv8::FLAGS, &[0x10])]);
    let m2 = post(ch, id, &m1)?;
    let salt = tlv8::get(&m2, tlv8::SALT).ok_or("M2: no salt")?.to_vec();
    let server_b = tlv8::get(&m2, tlv8::PUBLIC_KEY).ok_or("M2: no public key")?.to_vec();

    let mut srp = SrpClient::new(TRANSIENT_PIN);
    if !srp.process(&salt, &server_b) {
        return Err("M2: the speaker sent a bad SRP public key".into());
    }
    // M3: State 3, our A, our proof.
    let a = srp.public_a();
    let proof = srp.proof_m1();
    let m3 = tlv8::encode(&[(tlv8::STATE, &[0x03]), (tlv8::PUBLIC_KEY, &a), (tlv8::PROOF, &proof)]);
    let m4 = post(ch, id, &m3)?;
    let server_proof = tlv8::get(&m4, tlv8::PROOF).ok_or("M4: no proof (PIN rejected?)")?;
    if !srp.verify_server_proof(server_proof) {
        return Err("M4: the speaker's proof did not verify".into());
    }

    let k = srp.session_key();
    let mut audio = [0u8; 32];
    audio.copy_from_slice(&k[..32]);
    let keys = SessionKeys {
        shared_secret: k,
        control_write: channel_key(&k, "Control-Salt", "Control-Write-Encryption-Key"),
        control_read: channel_key(&k, "Control-Salt", "Control-Read-Encryption-Key"),
        event_read: channel_key(&k, "Events-Salt", "Events-Write-Encryption-Key"),
        event_write: channel_key(&k, "Events-Salt", "Events-Read-Encryption-Key"),
        audio,
    };
    ch.enable_encryption(keys.control_write, keys.control_read);
    Ok(keys)
}
