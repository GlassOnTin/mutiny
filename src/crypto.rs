//! Minimal pure-Rust cryptographic primitives for M3 bilateral peer pairing
//! and signed consent envelopes: SHA-256 (FIPS 180-4 / RFC 6234), HMAC-SHA256
//! (RFC 2104), constant-time slice comparison, and kernel CSPRNG token
//! generation (`libc::getrandom`).

use std::io;

use base64::Engine;

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256 state so large messages and HMAC inner/outer passes need
/// no heap allocation.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bytes: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub const fn new() -> Self {
        Self {
            state: H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            if data.len() < need {
                self.buf[self.buf_len..self.buf_len + data.len()].copy_from_slice(data);
                self.buf_len += data.len();
                return;
            }
            self.buf[self.buf_len..64].copy_from_slice(&data[..need]);
            let block = self.buf;
            compress_block(&mut self.state, &block);
            self.buf_len = 0;
            data = &data[need..];
        }

        let (blocks, remainder) = data.as_chunks::<64>();
        for block in blocks {
            compress_block(&mut self.state, block);
        }
        if !remainder.is_empty() {
            self.buf[..remainder.len()].copy_from_slice(remainder);
            self.buf_len = remainder.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_bytes.wrapping_mul(8);
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;

        if self.buf_len > 56 {
            self.buf[self.buf_len..64].fill(0);
            let block = self.buf;
            compress_block(&mut self.state, &block);
            self.buf_len = 0;
        }
        self.buf[self.buf_len..56].fill(0);
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        compress_block(&mut self.state, &block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress_block(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes(*chunk);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Computes SHA-256 (FIPS 180-4 / RFC 6234).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// Lowercase hexadecimal SHA-256 digest.
pub fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&sha256(data))
}

/// Computes HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let hashed = sha256(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_digest);
    outer.finalize()
}

/// Length-independent byte comparison over equal-length slices. `black_box`
/// keeps the optimizer from turning the XOR accumulator into an early-exit loop.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// Fills `buf` from the Linux kernel CSPRNG via `libc::getrandom`.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let rc = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - filled,
                0,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        filled += rc as usize;
    }
    Ok(())
}

/// Generates a 256-bit pairing token encoded as `"mty_"` plus base64url without
/// padding (47 characters total).
pub fn generate_pairing_token() -> io::Result<String> {
    let mut raw = [0u8; 32];
    fill_random(&mut raw)?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    Ok(format!("mty_{encoded}"))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    let trimmed = hex.trim();
    if !trimmed.len().is_multiple_of(2) {
        return Err(String::from("hex string must have even length"));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let raw = trimmed.as_bytes();
    let mut i = 0usize;
    while i < raw.len() {
        let hi = (raw[i] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit at byte {i}"))?;
        let lo = (raw[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit at byte {}", i + 1))?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Ok(out)
}

pub fn hex_decode_32(hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex_decode(hex)?;
    bytes.try_into().map_err(|v: Vec<u8>| {
        format!(
            "expected 32-byte digest (64 hex chars), got {} bytes",
            v.len()
        )
    })
}

pub const MAX_REPLAY_CACHE_ENTRIES: usize = 512;

/// Expected binding parameters for a cross-limb Haven `EgressGrantSigner`
/// HMAC-SHA256 consent decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedEgressGrant {
    pub request_id: String,
    pub sandbox_id: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedEgressDecision {
    Once,
    Session,
    Deny(String),
}

/// Bounded replay cache tracking consumed `(request_id, grant_id, nonce)`
/// tuples so a captured Haven signature cannot be replayed.
#[derive(Debug, Default)]
pub struct GrantReplayCache {
    order: std::collections::VecDeque<(String, String, String)>,
    seen_grants: std::collections::HashSet<(String, String)>,
    seen_requests: std::collections::HashSet<String>,
}

impl GrantReplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_replayed(&self, request_id: &str, grant_id: &str, nonce: &str) -> bool {
        self.seen_requests.contains(request_id)
            || self
                .seen_grants
                .contains(&(grant_id.to_owned(), nonce.to_owned()))
    }

    pub fn record(&mut self, request_id: String, grant_id: String, nonce: String) {
        while self.order.len() >= MAX_REPLAY_CACHE_ENTRIES {
            if let Some((old_req, old_grant, old_nonce)) = self.order.pop_front() {
                self.seen_requests.remove(&old_req);
                self.seen_grants.remove(&(old_grant, old_nonce));
            }
        }
        self.seen_requests.insert(request_id.clone());
        self.seen_grants.insert((grant_id.clone(), nonce.clone()));
        self.order.push_back((request_id, grant_id, nonce));
    }
}

/// Reproduces `sh.haven.core.security.EgressGrantSigner.canonicalMessage`
/// byte-for-byte: LF-delimited UTF-8, no trailing newline, empty string when
/// `expiry_ms` is `None`.
#[allow(clippy::too_many_arguments)]
pub fn canonical_egress_message(
    request_id: &str,
    sandbox_id: &str,
    host: &str,
    port: u16,
    decision: &str,
    expiry_ms: Option<u64>,
    grant_id: &str,
    nonce: &str,
) -> String {
    let expiry_str = expiry_ms.map(|v| v.to_string()).unwrap_or_default();
    format!(
        "{request_id}\n{sandbox_id}\n{host}\n{port}\n{decision}\n{expiry_str}\n{grant_id}\n{nonce}"
    )
}

/// Signs a canonical egress decision with standard padded Base64, matching
/// Haven's `EgressGrantSigner.signDecision`.
#[allow(clippy::too_many_arguments)]
pub fn sign_egress_decision(
    secret: &[u8],
    request_id: &str,
    sandbox_id: &str,
    host: &str,
    port: u16,
    decision: &str,
    expiry_ms: Option<u64>,
    grant_id: &str,
    nonce: &str,
) -> String {
    let msg = canonical_egress_message(
        request_id, sandbox_id, host, port, decision, expiry_ms, grant_id, nonce,
    );
    let mac = hmac_sha256(secret, msg.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac)
}

/// Validates and decodes a Base64-encoded 256-bit (or larger) HMAC signing key.
pub fn decode_signing_secret_b64(secret_b64: &str) -> Result<Vec<u8>, String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(secret_b64.trim())
        .map_err(|e| format!("signing secret is not valid padded Base64: {e}"))?;
    if raw.len() < 32 {
        return Err(format!(
            "signing secret must be at least 32 bytes, got {}",
            raw.len()
        ));
    }
    Ok(raw)
}

/// Fail-closed verifier for Haven `EgressGrantSigner` responses. Any missing
/// field, target mismatch, expired timestamp, replay, or bad HMAC-SHA256
/// signature resolves to `VerifiedEgressDecision::Deny`.
pub fn verify_egress_decision(
    secret_b64: &str,
    expected: &ExpectedEgressGrant,
    response: &serde_json::Value,
    now_epoch_ms: u64,
    replay_cache: &mut GrantReplayCache,
) -> VerifiedEgressDecision {
    let secret = match decode_signing_secret_b64(secret_b64) {
        Ok(s) => s,
        Err(err) => return VerifiedEgressDecision::Deny(err),
    };

    let Some(decision) = response.get("decision").and_then(serde_json::Value::as_str) else {
        return VerifiedEgressDecision::Deny(String::from("missing `decision` field"));
    };
    if decision == "deny" {
        return VerifiedEgressDecision::Deny(String::from("denied on Haven"));
    }
    if decision != "once" && decision != "session" {
        return VerifiedEgressDecision::Deny(format!("unknown decision {decision:?}"));
    }

    let req_id = response
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let sandbox_id = response
        .get("sandbox_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let host = response
        .get("host")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let port = response
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    if req_id != expected.request_id
        || sandbox_id != expected.sandbox_id
        || host != expected.host
        || port != u64::from(expected.port)
    {
        return VerifiedEgressDecision::Deny(String::from(
            "signed response fields do not match requested target",
        ));
    }

    let grant_id = response
        .get("grant_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let nonce = response
        .get("nonce")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if grant_id.is_empty()
        || nonce.is_empty()
        || grant_id.contains(['\n', '\r'])
        || nonce.contains(['\n', '\r'])
    {
        return VerifiedEgressDecision::Deny(String::from("missing or malformed grant_id/nonce"));
    }
    if replay_cache.is_replayed(req_id, grant_id, nonce) {
        return VerifiedEgressDecision::Deny(String::from(
            "replayed consent grant (request_id or grant_id/nonce already consumed)",
        ));
    }

    let Some(expiry_ms) = response
        .get("expiry_ms")
        .and_then(serde_json::Value::as_u64)
        .filter(|&ms| ms > 0)
    else {
        return VerifiedEgressDecision::Deny(String::from(
            "allow decision requires a positive expiry_ms",
        ));
    };
    if now_epoch_ms > expiry_ms {
        return VerifiedEgressDecision::Deny(String::from("signed consent grant has expired"));
    }

    let Some(sig_b64) = response
        .get("signature")
        .and_then(serde_json::Value::as_str)
    else {
        return VerifiedEgressDecision::Deny(String::from("missing HMAC signature"));
    };
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else {
        return VerifiedEgressDecision::Deny(String::from("malformed Base64 HMAC signature"));
    };

    let canonical = canonical_egress_message(
        req_id,
        sandbox_id,
        host,
        expected.port,
        decision,
        Some(expiry_ms),
        grant_id,
        nonce,
    );
    let expected_mac = hmac_sha256(&secret, canonical.as_bytes());
    if !constant_time_eq(&expected_mac, &sig_bytes) {
        return VerifiedEgressDecision::Deny(String::from("HMAC-SHA256 signature mismatch"));
    }

    replay_cache.record(req_id.to_owned(), grant_id.to_owned(), nonce.to_owned());
    if decision == "session" {
        VerifiedEgressDecision::Session
    } else {
        VerifiedEgressDecision::Once
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_fips_180_4_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );

        // 1,000,000 repetitions of 'a', fed in awkward 1000-byte chunks to
        // exercise cross-block buffering.
        let mut streaming = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            streaming.update(&chunk);
        }
        assert_eq!(
            hex_encode(&streaming.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_vectors() {
        // Test Case 1
        let key1 = [0x0bu8; 20];
        assert_eq!(
            hex_encode(&hmac_sha256(&key1, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );

        // Test Case 2 ("Jefe")
        assert_eq!(
            hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );

        // Test Case 3
        let key3 = [0xaau8; 20];
        let data3 = [0xddu8; 50];
        assert_eq!(
            hex_encode(&hmac_sha256(&key3, &data3)),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );

        // Test Case 4
        let key4 = hex_decode("0102030405060708090a0b0c0d0e0f10111213141516171819").unwrap();
        let data4 = [0xcdu8; 50];
        assert_eq!(
            hex_encode(&hmac_sha256(&key4, &data4)),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );

        // Test Case 6: 131-byte key (> 64-byte block size, exercises key hashing)
        let key6 = [0xaau8; 131];
        assert_eq!(
            hex_encode(&hmac_sha256(
                &key6,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );

        // Test Case 7: 131-byte key + multi-block message
        assert_eq!(
            hex_encode(&hmac_sha256(
                &key6,
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm."
            )),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn constant_time_comparison_and_token_generation() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"same-digest", b"same-digest"));
        assert!(!constant_time_eq(b"same-digest", b"same-digesu"));
        assert!(!constant_time_eq(b"short", b"longer"));

        let t1 = generate_pairing_token().expect("getrandom token 1");
        let t2 = generate_pairing_token().expect("getrandom token 2");
        assert!(t1.starts_with("mty_"), "got {t1}");
        assert_eq!(t1.len(), 4 + 43, "32 bytes base64url-no-pad is 43 chars");
        assert_ne!(t1, t2);
        assert!(
            t1[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must be URL-safe base64 without padding: {t1}"
        );
    }

    #[test]
    fn egress_grant_canonical_message_and_fail_closed_verifier() {
        let msg_allow = canonical_egress_message(
            "mty-consent-01",
            "mutiny:end-session:irreversible:agent",
            "mutiny.local",
            1,
            "once",
            Some(1_900_000_000_000),
            "grant-1",
            "nonce-1",
        );
        assert_eq!(
            msg_allow,
            "mty-consent-01\nmutiny:end-session:irreversible:agent\nmutiny.local\n1\nonce\n1900000000000\ngrant-1\nnonce-1"
        );
        assert!(!msg_allow.ends_with('\n'));

        let msg_deny = canonical_egress_message(
            "mty-consent-02",
            "mutiny:split:act:agent",
            "mutiny.local",
            1,
            "deny",
            None,
            "grant-2",
            "nonce-2",
        );
        assert_eq!(
            msg_deny,
            "mty-consent-02\nmutiny:split:act:agent\nmutiny.local\n1\ndeny\n\ngrant-2\nnonce-2"
        );

        let key = [0x42u8; 32];
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let sig = sign_egress_decision(
            &key,
            "mty-consent-01",
            "mutiny:end-session:irreversible:agent",
            "mutiny.local",
            1,
            "once",
            Some(1_900_000_000_000),
            "grant-1",
            "nonce-1",
        );
        let payload = serde_json::json!({
            "decision": "once",
            "request_id": "mty-consent-01",
            "sandbox_id": "mutiny:end-session:irreversible:agent",
            "host": "mutiny.local",
            "port": 1,
            "expiry_ms": 1_900_000_000_000u64,
            "grant_id": "grant-1",
            "nonce": "nonce-1",
            "signature": sig,
        });
        let expected = ExpectedEgressGrant {
            request_id: String::from("mty-consent-01"),
            sandbox_id: String::from("mutiny:end-session:irreversible:agent"),
            host: String::from("mutiny.local"),
            port: 1,
        };
        let mut cache = GrantReplayCache::new();
        assert_eq!(
            verify_egress_decision(&key_b64, &expected, &payload, 1_800_000_000_000, &mut cache),
            VerifiedEgressDecision::Once
        );
        // Immediate replay of identical signed payload must fail closed.
        assert!(matches!(
            verify_egress_decision(&key_b64, &expected, &payload, 1_800_000_000_000, &mut cache),
            VerifiedEgressDecision::Deny(_)
        ));
    }
}
