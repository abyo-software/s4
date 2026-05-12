//! Server-side encryption (SSE-S4) — AES-256-GCM (v0.4 #21).
//!
//! Wraps the post-compression S3 object body with authenticated
//! encryption. Compress-then-encrypt is the right order: encryption
//! produces high-entropy bytes that don't compress, so encrypting last
//! preserves the codec's ratio.
//!
//! ## Wire format (S4E1)
//!
//! ```text
//! [magic: "S4E1" 4B]
//! [algo:  u8]            # 1 = AES-256-GCM (v0.4 only supports this)
//! [reserved: 3B]         # 0x00 0x00 0x00
//! [nonce: 12B]           # random per-object
//! [tag:   16B]           # AES-GCM authentication tag
//! [ciphertext: variable] # encrypted-then-authenticated body
//! ```
//!
//! Total overhead: 36 bytes per object.
//!
//! Since the body S4 wraps is already S4F2-framed, the on-the-wire
//! object stored in S3 looks like:
//!
//! ```text
//! [S4E1 header 36B][AES-GCM(S4F2 body)]
//! ```
//!
//! ## v0.4 scope cuts
//!
//! - **Server-managed key only**: a single 32-byte key loaded from a
//!   local file via `--sse-s4-key <path>`. KMS / vault integration is a
//!   follow-up issue.
//! - **No SSE-C** (customer-provided keys via `x-amz-server-side-
//!   encryption-customer-key` header) yet — same follow-up issue.
//! - **One key, no rotation** — no key-id field in the header. v0.5 will
//!   bump the wire format to S4E2 with a key-id slot.
//!
//! Operators who need any of those today should layer S4 behind an
//! IAM-aware proxy that handles encryption at its end.

use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use bytes::Bytes;
use rand::RngCore;
use thiserror::Error;

pub const SSE_MAGIC: &[u8; 4] = b"S4E1";
pub const SSE_HEADER_BYTES: usize = 4 + 1 + 3 + 12 + 16; // magic + algo + reserved + nonce + tag = 36
pub const ALGO_AES_256_GCM: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum SseError {
    #[error("SSE key file {path:?}: {source}")]
    KeyFileIo {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error(
        "SSE key file must be exactly 32 raw bytes (or 64-char hex / 44-char base64); got {got} bytes after parse"
    )]
    BadKeyLength { got: usize },
    #[error("SSE-encrypted body too short ({got} bytes; need at least {SSE_HEADER_BYTES})")]
    TooShort { got: usize },
    #[error("SSE bad magic: expected S4E1, got {got:?}")]
    BadMagic { got: [u8; 4] },
    #[error("SSE unsupported algo tag: {tag} (this build only knows AES-256-GCM = 1)")]
    UnsupportedAlgo { tag: u8 },
    #[error("SSE decryption / authentication failed (key mismatch or ciphertext tampered with)")]
    DecryptFailed,
}

/// 32-byte symmetric key. Held inside an `Arc` so cloning the key-ring
/// across handler tasks is cheap.
#[derive(Clone)]
pub struct SseKey(Arc<[u8; KEY_LEN]>);

impl SseKey {
    /// Load a 32-byte key from disk. Accepts three on-disk encodings:
    /// raw 32 bytes, 64-char ASCII hex, or 44-char ASCII base64 (with or
    /// without padding). Whitespace is trimmed.
    pub fn from_path(path: &Path) -> Result<Self, SseError> {
        let raw = std::fs::read(path).map_err(|source| SseError::KeyFileIo {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_bytes(&raw)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SseError> {
        // Try raw first.
        if bytes.len() == KEY_LEN {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(bytes);
            return Ok(Self(Arc::new(k)));
        }
        // Trim whitespace and try hex / base64.
        let s = std::str::from_utf8(bytes).unwrap_or("").trim();
        if s.len() == KEY_LEN * 2 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut k = [0u8; KEY_LEN];
            for (i, k_byte) in k.iter_mut().enumerate() {
                *k_byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                    .map_err(|_| SseError::BadKeyLength { got: bytes.len() })?;
            }
            return Ok(Self(Arc::new(k)));
        }
        if let Ok(decoded) =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_bytes())
            && decoded.len() == KEY_LEN
        {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&decoded);
            return Ok(Self(Arc::new(k)));
        }
        Err(SseError::BadKeyLength { got: bytes.len() })
    }

    fn as_aes_key(&self) -> &Key<Aes256Gcm> {
        Key::<Aes256Gcm>::from_slice(self.0.as_ref())
    }
}

impl std::fmt::Debug for SseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseKey")
            .field("len", &KEY_LEN)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Encrypt `plaintext` with the given key, producing the on-the-wire
/// S4E1-framed output: `[magic 4][algo 1][reserved 3][nonce 12][tag 16][ciphertext]`.
pub fn encrypt(key: &SseKey, plaintext: &[u8]) -> Bytes {
    let cipher = Aes256Gcm::new(key.as_aes_key());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    // Use the magic + algo bytes as additional authenticated data so
    // tampering with the header bumps the algo or magic still fails the
    // tag check.
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(SSE_MAGIC);
    aad[4] = ALGO_AES_256_GCM;
    let ct_with_tag = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .expect("aes-gcm encrypt cannot fail with a 32-byte key");
    // ct_with_tag = ciphertext || tag (16 last bytes)
    debug_assert!(ct_with_tag.len() >= TAG_LEN);
    let split = ct_with_tag.len() - TAG_LEN;
    let (ct, tag) = ct_with_tag.split_at(split);

    let mut out = Vec::with_capacity(SSE_HEADER_BYTES + ct.len());
    out.extend_from_slice(SSE_MAGIC);
    out.push(ALGO_AES_256_GCM);
    out.extend_from_slice(&[0u8; 3]); // reserved
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    Bytes::from(out)
}

/// Decrypt an S4E1-framed body. Returns the plaintext (= S4F2-framed
/// codec body). Fails if the magic, algo tag, or AES-GCM auth tag don't
/// validate — meaning either the wrong key was supplied or the
/// ciphertext was tampered with.
pub fn decrypt(key: &SseKey, body: &[u8]) -> Result<Bytes, SseError> {
    if body.len() < SSE_HEADER_BYTES {
        return Err(SseError::TooShort { got: body.len() });
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&body[..4]);
    if &magic != SSE_MAGIC {
        return Err(SseError::BadMagic { got: magic });
    }
    let algo = body[4];
    if algo != ALGO_AES_256_GCM {
        return Err(SseError::UnsupportedAlgo { tag: algo });
    }
    // body[5..8] reserved
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&body[8..8 + NONCE_LEN]);
    let mut tag_bytes = [0u8; TAG_LEN];
    tag_bytes.copy_from_slice(&body[8 + NONCE_LEN..SSE_HEADER_BYTES]);
    let ct = &body[SSE_HEADER_BYTES..];

    let cipher = Aes256Gcm::new(key.as_aes_key());
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(SSE_MAGIC);
    aad[4] = ALGO_AES_256_GCM;
    let mut ct_with_tag = Vec::with_capacity(ct.len() + TAG_LEN);
    ct_with_tag.extend_from_slice(ct);
    ct_with_tag.extend_from_slice(&tag_bytes);
    let plain = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &ct_with_tag,
                aad: &aad,
            },
        )
        .map_err(|_| SseError::DecryptFailed)?;
    Ok(Bytes::from(plain))
}

/// Detect whether `body` is S4E1-encrypted by sniffing the magic bytes.
/// Used by the GET path to decide whether to run decryption before
/// frame parsing.
pub fn looks_encrypted(body: &[u8]) -> bool {
    body.len() >= SSE_HEADER_BYTES && &body[..4] == SSE_MAGIC
}

pub type SharedSseKey = Arc<SseKey>;

#[cfg(test)]
mod tests {
    use super::*;

    fn key32() -> SseKey {
        SseKey::from_bytes(&[7u8; 32]).unwrap()
    }

    #[test]
    fn roundtrip_basic() {
        let k = key32();
        let pt = b"the quick brown fox jumps over the lazy dog";
        let ct = encrypt(&k, pt);
        assert!(looks_encrypted(&ct));
        assert_eq!(&ct[..4], SSE_MAGIC);
        assert_eq!(ct[4], ALGO_AES_256_GCM);
        assert_eq!(ct.len(), SSE_HEADER_BYTES + pt.len());
        let pt2 = decrypt(&k, &ct).unwrap();
        assert_eq!(pt2.as_ref(), pt);
    }

    #[test]
    fn wrong_key_fails() {
        let k1 = SseKey::from_bytes(&[1u8; 32]).unwrap();
        let k2 = SseKey::from_bytes(&[2u8; 32]).unwrap();
        let ct = encrypt(&k1, b"secret");
        let err = decrypt(&k2, &ct).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let k = key32();
        let mut ct = encrypt(&k, b"secret message").to_vec();
        // Flip a bit deep in the ciphertext (past the header)
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let err = decrypt(&k, &ct).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed));
    }

    #[test]
    fn tampered_algo_byte_fails() {
        let k = key32();
        let mut ct = encrypt(&k, b"secret").to_vec();
        ct[4] = 99; // unsupported algo
        let err = decrypt(&k, &ct).unwrap_err();
        assert!(matches!(err, SseError::UnsupportedAlgo { tag: 99 }));
    }

    #[test]
    fn rejects_short_body() {
        let k = key32();
        let err = decrypt(&k, b"short").unwrap_err();
        assert!(matches!(err, SseError::TooShort { got: 5 }));
    }

    #[test]
    fn looks_encrypted_passthrough_returns_false() {
        // S4F2 frame magic, NOT S4E1
        assert!(!looks_encrypted(b"S4F2\x01\x00\x00\x00........"));
        assert!(!looks_encrypted(b""));
    }

    #[test]
    fn key_from_hex_string() {
        let k =
            SseKey::from_bytes(b"0102030405060708090a0b0c0d0e0f10111213141516171819202122232425")
                .unwrap_err();
        // Wrong hex length (62 hex chars, not 64)
        assert!(matches!(k, SseError::BadKeyLength { .. }));
        let good = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let _ = SseKey::from_bytes(good).expect("64-char hex should parse");
    }

    #[test]
    fn encrypt_uses_random_nonce() {
        // Two encrypts of the same plaintext with the same key produce
        // different ciphertexts because the nonce is freshly random.
        let k = key32();
        let pt = b"deterministic input";
        let a = encrypt(&k, pt);
        let b = encrypt(&k, pt);
        assert_ne!(a, b, "nonce must be random per-call");
    }
}
