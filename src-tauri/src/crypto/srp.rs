//! SRP-6a client, HAP flavour: the RFC 5054 3072-bit group, g = 5, SHA-512,
//! and Apple's padding rules. `k = H(pad(N) | pad(g))` and
//! `u = H(pad(A) | pad(B))` are padded to N's length, while `M1`, `K` and
//! `M2` hash the values at their natural length. The username is always
//! "Pair-Setup" and, for a HomePod's transient pairing, the password is the
//! fixed PIN "3939".
//!
//! Only the group arithmetic (`modpow`) comes from `num-bigint`.

use num_bigint::BigUint;
use sha2::{Digest, Sha512};

use super::random;

const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";
const N_BYTES: usize = 384;
const USERNAME: &str = "Pair-Setup";

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn pad(x: &BigUint) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; N_BYTES.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    big_a: BigUint,
    password: String,
    /// Filled by [`SrpClient::process`].
    key: Option<[u8; 64]>,
    m1: Option<[u8; 64]>,
}

impl SrpClient {
    pub fn new(password: &str) -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("SRP group N");
        let g = BigUint::from(5u8);
        // 256-bit ephemeral, the same size pair_ap draws.
        let a = BigUint::from_bytes_be(&random::bytes::<32>());
        let big_a = g.modpow(&a, &n);
        Self { n, g, a, big_a, password: password.to_string(), key: None, m1: None }
    }

    /// Our public ephemeral `A`, sent in M3.
    pub fn public_a(&self) -> Vec<u8> {
        self.big_a.to_bytes_be()
    }

    /// Take the accessory's salt and `B` (from M2); computes S, K and the
    /// client proof M1. Returns `false` when B is 0 mod N.
    pub fn process(&mut self, salt: &[u8], server_b: &[u8]) -> bool {
        let big_b = BigUint::from_bytes_be(server_b);
        let zero = BigUint::from(0u8);
        if &big_b % &self.n == zero {
            return false;
        }
        let k = BigUint::from_bytes_be(&sha512(&[&pad(&self.n), &pad(&self.g)]));
        let u = BigUint::from_bytes_be(&sha512(&[&pad(&self.big_a), &pad(&big_b)]));
        let inner = sha512(&[format!("{USERNAME}:{}", self.password).as_bytes()]);
        let x = BigUint::from_bytes_be(&sha512(&[salt, &inner]));

        // S = (B - k * g^x) ^ (a + u * x) mod N, kept non-negative mod N.
        let gx = self.g.modpow(&x, &self.n);
        let kgx = (&k * &gx) % &self.n;
        let base = ((&big_b + &self.n) - &kgx) % &self.n;
        let exp = &self.a + &u * &x;
        let s = base.modpow(&exp, &self.n);

        let key = sha512(&[&s.to_bytes_be()]);
        let hn = sha512(&[&self.n.to_bytes_be()]);
        let hg = sha512(&[&self.g.to_bytes_be()]);
        let mut hxor = [0u8; 64];
        for i in 0..64 {
            hxor[i] = hn[i] ^ hg[i];
        }
        let hi = sha512(&[USERNAME.as_bytes()]);
        let m1 = sha512(&[&hxor, &hi, salt, &self.big_a.to_bytes_be(), server_b, &key]);
        self.key = Some(key);
        self.m1 = Some(m1);
        true
    }

    pub fn proof_m1(&self) -> [u8; 64] {
        self.m1.expect("process() first")
    }

    /// K = SHA-512(S): 64 bytes. The HAP channels HKDF the whole thing; the
    /// AirPlay audio key is its first 32 bytes, raw.
    pub fn session_key(&self) -> [u8; 64] {
        self.key.expect("process() first")
    }

    /// Verify the accessory's proof from M4: M2 = H(A | M1 | K).
    pub fn verify_server_proof(&self, m2: &[u8]) -> bool {
        let (Some(key), Some(m1)) = (self.key, self.m1) else { return false };
        let ours = sha512(&[&self.big_a.to_bytes_be(), &m1, &key]);
        ours.len() == m2.len() && ours.iter().zip(m2).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
    }
}
