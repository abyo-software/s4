//! Server-side encryption (SSE-S4) — AES-256-GCM (v0.4 #21, v0.5 #29).
//!
//! Wraps the post-compression S3 object body with authenticated
//! encryption. Compress-then-encrypt is the right order: encryption
//! produces high-entropy bytes that don't compress, so encrypting last
//! preserves the codec's ratio.
//!
//! ## Wire formats
//!
//! ### S4E1 (v0.4) — single key, no rotation
//!
//! ```text
//! [magic: "S4E1" 4B]
//! [algo:  u8]            # 1 = AES-256-GCM
//! [reserved: 3B]         # 0x00 0x00 0x00
//! [nonce: 12B]           # random per-object
//! [tag:   16B]           # AES-GCM authentication tag
//! [ciphertext: variable] # encrypted-then-authenticated body
//! ```
//!
//! Total overhead: 36 bytes per object.
//!
//! ### S4E2 (v0.5 #29) — keyring-aware, supports rotation
//!
//! ```text
//! [magic:  "S4E2" 4B]
//! [algo:   u8]            # 1 = AES-256-GCM
//! [key_id: u16 BE]        # which keyring slot encrypted this body
//! [reserved: 1B]          # 0x00
//! [nonce:  12B]           # random per-object
//! [tag:    16B]           # AES-GCM authentication tag
//! [ciphertext: variable]
//! ```
//!
//! Same 36-byte overhead — we reused the 3-byte reserved area in S4E1
//! to fit a 2-byte key-id + 1-byte reserved without bumping the header
//! size. The key-id is included in the AAD so a flipped key-id byte
//! fails the auth tag (i.e. an attacker can't trick the gateway into
//! decrypting under a different keyring slot).
//!
//! ## v0.5 rotation flow
//!
//! Operators wire one [`SseKeyring`] holding the **active** key plus
//! any number of **retired** keys. PUT always encrypts under the
//! active key (S4E2 with that key's id). GET sniffs the magic:
//!
//! - `S4E1`: legacy single-key path. The keyring's active key is tried
//!   first, then every retired key — this lets a v0.4 deployment
//!   migrate to a keyring with the original key as active and decrypt
//!   pre-rotation objects unchanged.
//! - `S4E2`: read the key_id, look it up in the keyring, decrypt with
//!   that exact key. Missing key_id surfaces as `KeyNotInKeyring`.
//!
//! ## Open follow-ups
//!
//! - **Server-managed key only**: keys come from local files via
//!   `--sse-s4-key` / `--sse-s4-key-rotated`. KMS / vault integration
//!   is a separate issue.
//! - **No SSE-C** (customer-provided keys via
//!   `x-amz-server-side-encryption-customer-key` header) yet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use bytes::Bytes;
use rand::RngCore;
use thiserror::Error;

pub const SSE_MAGIC_V1: &[u8; 4] = b"S4E1";
pub const SSE_MAGIC_V2: &[u8; 4] = b"S4E2";
/// Back-compat alias — v0.4 callers that imported `SSE_MAGIC` mean S4E1.
pub const SSE_MAGIC: &[u8; 4] = SSE_MAGIC_V1;

/// Header layout matches between S4E1 and S4E2 (both 36 bytes total)
/// because S4E2 reuses the 3-byte reserved slot to fit `key_id (2B) +
/// reserved (1B)`. Keeping them the same length means the rest of the
/// pipeline (sidecar offsets, multipart math) doesn't care which
/// frame variant is in flight.
pub const SSE_HEADER_BYTES: usize = 4 + 1 + 3 + 12 + 16; // = 36
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
    #[error("SSE bad magic: expected S4E1 or S4E2, got {got:?}")]
    BadMagic { got: [u8; 4] },
    #[error("SSE unsupported algo tag: {tag} (this build only knows AES-256-GCM = 1)")]
    UnsupportedAlgo { tag: u8 },
    #[error(
        "SSE key_id {id} (S4E2 frame) not present in keyring; rotation history likely incomplete"
    )]
    KeyNotInKeyring { id: u16 },
    #[error("SSE decryption / authentication failed (key mismatch or ciphertext tampered with)")]
    DecryptFailed,
}

/// 32-byte symmetric key. `bytes` is `pub` so call sites can construct
/// keys directly from already-validated bytes (e.g. KMS-decrypted DEKs)
/// without going through the on-disk parser. Hold inside an `Arc` when
/// sharing across handler tasks — `SseKeyring` does this internally.
pub struct SseKey {
    pub bytes: [u8; 32],
}

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
            return Ok(Self { bytes: k });
        }
        // Trim whitespace and try hex / base64.
        let s = std::str::from_utf8(bytes).unwrap_or("").trim();
        if s.len() == KEY_LEN * 2 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut k = [0u8; KEY_LEN];
            for (i, k_byte) in k.iter_mut().enumerate() {
                *k_byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                    .map_err(|_| SseError::BadKeyLength { got: bytes.len() })?;
            }
            return Ok(Self { bytes: k });
        }
        if let Ok(decoded) =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_bytes())
            && decoded.len() == KEY_LEN
        {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&decoded);
            return Ok(Self { bytes: k });
        }
        Err(SseError::BadKeyLength { got: bytes.len() })
    }

    fn as_aes_key(&self) -> &Key<Aes256Gcm> {
        Key::<Aes256Gcm>::from_slice(&self.bytes)
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

/// v0.5 #29: a set of `SseKey`s indexed by `u16` key-id, plus a
/// designated **active** id used for new encryptions. Rotation is just
/// "add the new key, flip `active` to its id, leave the old keys for
/// decryption-only". Cheap to clone (`Arc<SseKey>` per slot).
#[derive(Clone)]
pub struct SseKeyring {
    active: u16,
    keys: HashMap<u16, Arc<SseKey>>,
}

impl SseKeyring {
    /// Create a keyring seeded with one key, immediately marked
    /// active. Add older keys later via [`SseKeyring::add`] so the
    /// gateway can still decrypt pre-rotation objects.
    pub fn new(active: u16, key: Arc<SseKey>) -> Self {
        let mut keys = HashMap::new();
        keys.insert(active, key);
        Self { active, keys }
    }

    /// Insert another key under id `id`. Does NOT change `active`. If
    /// `id == active`, the slot is overwritten (useful for tests; in
    /// production prefer minting a fresh id).
    pub fn add(&mut self, id: u16, key: Arc<SseKey>) {
        self.keys.insert(id, key);
    }

    /// Active (id, key) — used by [`encrypt_v2`] to pick the slot for
    /// new objects.
    pub fn active(&self) -> (u16, &SseKey) {
        let id = self.active;
        let key = self
            .keys
            .get(&id)
            .expect("active key id must be present in keyring (constructor invariant)");
        (id, key.as_ref())
    }

    /// Look up a key by id. Returns `None` for unknown ids — caller
    /// should surface this as [`SseError::KeyNotInKeyring`].
    pub fn get(&self, id: u16) -> Option<&SseKey> {
        self.keys.get(&id).map(Arc::as_ref)
    }
}

impl std::fmt::Debug for SseKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseKeyring")
            .field("active", &self.active)
            .field("key_count", &self.keys.len())
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub type SharedSseKeyring = Arc<SseKeyring>;

/// Encrypt `plaintext` with the given key, producing the on-the-wire
/// S4E1-framed output: `[magic 4][algo 1][reserved 3][nonce 12][tag 16][ciphertext]`.
///
/// Kept for back-compat: v0.4 callers that hand-built an `SseKey` (no
/// keyring) still get the v1 frame. New code should use
/// [`encrypt_v2`] which writes S4E2 and supports rotation on read.
pub fn encrypt(key: &SseKey, plaintext: &[u8]) -> Bytes {
    let cipher = Aes256Gcm::new(key.as_aes_key());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    // AAD = magic + algo. Tampering with either bumps the tag check.
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(SSE_MAGIC_V1);
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
    debug_assert!(ct_with_tag.len() >= TAG_LEN);
    let split = ct_with_tag.len() - TAG_LEN;
    let (ct, tag) = ct_with_tag.split_at(split);

    let mut out = Vec::with_capacity(SSE_HEADER_BYTES + ct.len());
    out.extend_from_slice(SSE_MAGIC_V1);
    out.push(ALGO_AES_256_GCM);
    out.extend_from_slice(&[0u8; 3]); // reserved
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    Bytes::from(out)
}

/// v0.5 #29: encrypt under the keyring's currently-active key, writing
/// an S4E2-framed body (`[magic 4][algo 1][key_id 2 BE][reserved 1]
/// [nonce 12][tag 16][ciphertext]`). The key-id is included in the
/// AAD so flipping it fails the auth tag.
pub fn encrypt_v2(plaintext: &[u8], keyring: &SseKeyring) -> Bytes {
    let (key_id, key) = keyring.active();
    let cipher = Aes256Gcm::new(key.as_aes_key());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = aad_v2(key_id);
    let ct_with_tag = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .expect("aes-gcm encrypt cannot fail with a 32-byte key");
    debug_assert!(ct_with_tag.len() >= TAG_LEN);
    let split = ct_with_tag.len() - TAG_LEN;
    let (ct, tag) = ct_with_tag.split_at(split);

    let mut out = Vec::with_capacity(SSE_HEADER_BYTES + ct.len());
    out.extend_from_slice(SSE_MAGIC_V2);
    out.push(ALGO_AES_256_GCM);
    out.extend_from_slice(&key_id.to_be_bytes()); // 2B BE key_id
    out.push(0u8); // 1B reserved
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    Bytes::from(out)
}

fn aad_v1() -> [u8; 8] {
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(SSE_MAGIC_V1);
    aad[4] = ALGO_AES_256_GCM;
    aad
}

fn aad_v2(key_id: u16) -> [u8; 8] {
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(SSE_MAGIC_V2);
    aad[4] = ALGO_AES_256_GCM;
    aad[5..7].copy_from_slice(&key_id.to_be_bytes());
    aad[7] = 0u8;
    aad
}

/// v0.5 #29: dispatch on the body's magic and decrypt under whichever
/// key the keyring offers. S4E1 bodies (v0.4 vintage) are tried
/// against every key in the ring (active first); S4E2 bodies look up
/// `key_id` and use exactly that slot. Surfaces `KeyNotInKeyring`
/// (S4E2 with unknown id) and `DecryptFailed` (auth-tag fail) as
/// distinct errors so operators can tell rotation gaps apart from
/// tampering.
pub fn decrypt(body: &[u8], keyring: &SseKeyring) -> Result<Bytes, SseError> {
    if body.len() < SSE_HEADER_BYTES {
        return Err(SseError::TooShort { got: body.len() });
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&body[..4]);
    match &magic {
        m if m == SSE_MAGIC_V1 => decrypt_v1_with_keyring(body, keyring),
        m if m == SSE_MAGIC_V2 => decrypt_v2_with_keyring(body, keyring),
        _ => Err(SseError::BadMagic { got: magic }),
    }
}

fn decrypt_v1_with_keyring(body: &[u8], keyring: &SseKeyring) -> Result<Bytes, SseError> {
    let algo = body[4];
    if algo != ALGO_AES_256_GCM {
        return Err(SseError::UnsupportedAlgo { tag: algo });
    }
    // body[5..8] reserved (must be ignored — v0.4 wrote zeros, but we
    // didn't auth them so we can't insist on it).
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&body[8..8 + NONCE_LEN]);
    let mut tag_bytes = [0u8; TAG_LEN];
    tag_bytes.copy_from_slice(&body[8 + NONCE_LEN..SSE_HEADER_BYTES]);
    let ct = &body[SSE_HEADER_BYTES..];

    let aad = aad_v1();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut ct_with_tag = Vec::with_capacity(ct.len() + TAG_LEN);
    ct_with_tag.extend_from_slice(ct);
    ct_with_tag.extend_from_slice(&tag_bytes);

    // Active key first, then any others. v0.4 deployments that flip to
    // v0.5 with their original key as active hit this path on the
    // first try for every legacy object.
    let (active_id, _active_key) = keyring.active();
    let mut ids: Vec<u16> = keyring.keys.keys().copied().collect();
    ids.sort_by_key(|id| if *id == active_id { 0 } else { 1 });
    for id in ids {
        let key = keyring.get(id).expect("id came from keyring iteration");
        let cipher = Aes256Gcm::new(key.as_aes_key());
        if let Ok(plain) = cipher.decrypt(
            nonce,
            Payload {
                msg: &ct_with_tag,
                aad: &aad,
            },
        ) {
            return Ok(Bytes::from(plain));
        }
    }
    Err(SseError::DecryptFailed)
}

fn decrypt_v2_with_keyring(body: &[u8], keyring: &SseKeyring) -> Result<Bytes, SseError> {
    let algo = body[4];
    if algo != ALGO_AES_256_GCM {
        return Err(SseError::UnsupportedAlgo { tag: algo });
    }
    let key_id = u16::from_be_bytes([body[5], body[6]]);
    // body[7] reserved (1B), authenticated as 0 via AAD.
    let key = keyring
        .get(key_id)
        .ok_or(SseError::KeyNotInKeyring { id: key_id })?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&body[8..8 + NONCE_LEN]);
    let mut tag_bytes = [0u8; TAG_LEN];
    tag_bytes.copy_from_slice(&body[8 + NONCE_LEN..SSE_HEADER_BYTES]);
    let ct = &body[SSE_HEADER_BYTES..];

    let aad = aad_v2(key_id);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut ct_with_tag = Vec::with_capacity(ct.len() + TAG_LEN);
    ct_with_tag.extend_from_slice(ct);
    ct_with_tag.extend_from_slice(&tag_bytes);
    let cipher = Aes256Gcm::new(key.as_aes_key());
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

/// Detect whether `body` is SSE-S4 encrypted (either S4E1 or S4E2) by
/// sniffing the first 4 magic bytes. Used by the GET path to decide
/// whether to run decryption before frame parsing.
pub fn looks_encrypted(body: &[u8]) -> bool {
    if body.len() < SSE_HEADER_BYTES {
        return false;
    }
    let m = &body[..4];
    m == SSE_MAGIC_V1 || m == SSE_MAGIC_V2
}

pub type SharedSseKey = Arc<SseKey>;

#[cfg(test)]
mod tests {
    use super::*;

    fn key32(seed: u8) -> Arc<SseKey> {
        Arc::new(SseKey::from_bytes(&[seed; 32]).unwrap())
    }

    fn keyring_single(seed: u8) -> SseKeyring {
        SseKeyring::new(1, key32(seed))
    }

    #[test]
    fn roundtrip_basic_v1() {
        // back-compat single-key API — still works.
        let k = SseKey::from_bytes(&[7u8; 32]).unwrap();
        let pt = b"the quick brown fox jumps over the lazy dog";
        let ct = encrypt(&k, pt);
        assert!(looks_encrypted(&ct));
        assert_eq!(&ct[..4], SSE_MAGIC_V1);
        assert_eq!(ct[4], ALGO_AES_256_GCM);
        assert_eq!(ct.len(), SSE_HEADER_BYTES + pt.len());
        // decrypt via single-key keyring
        let kr = SseKeyring::new(1, Arc::new(k));
        let pt2 = decrypt(&ct, &kr).unwrap();
        assert_eq!(pt2.as_ref(), pt);
    }

    #[test]
    fn s4e2_roundtrip_active_key() {
        let kr = keyring_single(7);
        let pt = b"S4E2 active-key roundtrip";
        let ct = encrypt_v2(pt, &kr);
        assert_eq!(&ct[..4], SSE_MAGIC_V2);
        assert_eq!(ct[4], ALGO_AES_256_GCM);
        assert_eq!(u16::from_be_bytes([ct[5], ct[6]]), 1, "key_id BE");
        assert_eq!(ct[7], 0, "reserved byte");
        assert_eq!(ct.len(), SSE_HEADER_BYTES + pt.len());
        assert!(looks_encrypted(&ct));
        let pt2 = decrypt(&ct, &kr).unwrap();
        assert_eq!(pt2.as_ref(), pt);
    }

    #[test]
    fn decrypt_s4e1_via_active_only_keyring() {
        // v0.4 wrote S4E1 with key K; v0.5 keyring has K as the only
        // (active) key. Decrypt must succeed.
        let k_arc = key32(11);
        let legacy_ct = encrypt(&k_arc, b"v0.4 vintage object");
        assert_eq!(&legacy_ct[..4], SSE_MAGIC_V1);
        let kr = SseKeyring::new(1, Arc::clone(&k_arc));
        let plain = decrypt(&legacy_ct, &kr).unwrap();
        assert_eq!(plain.as_ref(), b"v0.4 vintage object");
    }

    #[test]
    fn decrypt_s4e2_under_old_key_after_rotation() {
        // Rotation flow: object was encrypted under key id=1 when 1
        // was active. Operator rotates to active=2 and keeps 1 in the
        // keyring. The S4E2 body must still decrypt.
        let k1 = key32(1);
        let k2 = key32(2);
        let mut kr_old = SseKeyring::new(1, Arc::clone(&k1));
        let ct = encrypt_v2(b"old-rotation object", &kr_old);
        assert_eq!(u16::from_be_bytes([ct[5], ct[6]]), 1);

        // After rotation: active=2, but key 1 still in ring.
        kr_old.add(2, Arc::clone(&k2));
        let mut kr_new = SseKeyring::new(2, Arc::clone(&k2));
        kr_new.add(1, Arc::clone(&k1));

        let plain = decrypt(&ct, &kr_new).unwrap();
        assert_eq!(plain.as_ref(), b"old-rotation object");

        // And new PUTs go to id 2 (active).
        let new_ct = encrypt_v2(b"new-rotation object", &kr_new);
        assert_eq!(u16::from_be_bytes([new_ct[5], new_ct[6]]), 2);
        let plain_new = decrypt(&new_ct, &kr_new).unwrap();
        assert_eq!(plain_new.as_ref(), b"new-rotation object");
    }

    #[test]
    fn s4e2_unknown_key_id_errors() {
        let kr = keyring_single(3); // only id=1 present
        let kr_other = SseKeyring::new(99, key32(3));
        let ct = encrypt_v2(b"x", &kr_other); // body claims key_id=99
        let err = decrypt(&ct, &kr).unwrap_err();
        assert!(
            matches!(err, SseError::KeyNotInKeyring { id: 99 }),
            "got {err:?}"
        );
    }

    #[test]
    fn s4e2_tampered_key_id_fails_auth() {
        let kr = SseKeyring::new(1, key32(4));
        let mut kr_with_2 = kr.clone();
        kr_with_2.add(2, key32(5)); // a real but wrong key under id=2
        let mut ct = encrypt_v2(b"do not flip my key id", &kr).to_vec();
        // Flip key_id from 1 → 2 in the header. The keyring HAS a key
        // for 2, so the lookup succeeds — but AAD authenticates the
        // original key_id, so AES-GCM tag verification must fail.
        assert_eq!(u16::from_be_bytes([ct[5], ct[6]]), 1);
        ct[5] = 0;
        ct[6] = 2;
        let err = decrypt(&ct, &kr_with_2).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed), "got {err:?}");
    }

    #[test]
    fn s4e2_tampered_ciphertext_fails() {
        let kr = SseKeyring::new(7, key32(9));
        let mut ct = encrypt_v2(b"secret message v2", &kr).to_vec();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let err = decrypt(&ct, &kr).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed));
    }

    #[test]
    fn s4e2_tampered_algo_byte_fails() {
        let kr = SseKeyring::new(1, key32(2));
        let mut ct = encrypt_v2(b"hi", &kr).to_vec();
        ct[4] = 99;
        let err = decrypt(&ct, &kr).unwrap_err();
        assert!(matches!(err, SseError::UnsupportedAlgo { tag: 99 }));
    }

    #[test]
    fn wrong_key_fails_v1_via_keyring() {
        // S4E1 written under key K1; keyring has only K2 → DecryptFailed.
        let k1 = SseKey::from_bytes(&[1u8; 32]).unwrap();
        let ct = encrypt(&k1, b"secret");
        let kr_wrong = SseKeyring::new(1, Arc::new(SseKey::from_bytes(&[2u8; 32]).unwrap()));
        let err = decrypt(&ct, &kr_wrong).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed));
    }

    #[test]
    fn rejects_short_body() {
        let kr = SseKeyring::new(1, key32(1));
        let err = decrypt(b"short", &kr).unwrap_err();
        assert!(matches!(err, SseError::TooShort { got: 5 }));
    }

    #[test]
    fn looks_encrypted_passthrough_returns_false() {
        // S4F2 frame magic, NOT S4E1 / S4E2 — must not be confused.
        let f2 = b"S4F2\x01\x00\x00\x00........................................";
        assert!(!looks_encrypted(f2));
        assert!(!looks_encrypted(b""));
    }

    #[test]
    fn looks_encrypted_detects_both_v1_and_v2() {
        let kr = SseKeyring::new(1, key32(8));
        let v1 = encrypt(&SseKey::from_bytes(&[8u8; 32]).unwrap(), b"x");
        let v2 = encrypt_v2(b"x", &kr);
        assert!(looks_encrypted(&v1));
        assert!(looks_encrypted(&v2));
    }

    #[test]
    fn key_from_hex_string() {
        let bad =
            SseKey::from_bytes(b"0102030405060708090a0b0c0d0e0f10111213141516171819202122232425")
                .unwrap_err();
        assert!(matches!(bad, SseError::BadKeyLength { .. }));
        let good = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let _ = SseKey::from_bytes(good).expect("64-char hex should parse");
    }

    #[test]
    fn encrypt_v2_uses_random_nonce() {
        let kr = SseKeyring::new(1, key32(3));
        let pt = b"deterministic input";
        let a = encrypt_v2(pt, &kr);
        let b = encrypt_v2(pt, &kr);
        assert_ne!(a, b, "nonce must be random per-call");
    }

    #[test]
    fn keyring_active_and_get() {
        let k1 = key32(1);
        let k2 = key32(2);
        let mut kr = SseKeyring::new(1, Arc::clone(&k1));
        kr.add(2, Arc::clone(&k2));
        let (id, active) = kr.active();
        assert_eq!(id, 1);
        assert_eq!(active.bytes, [1u8; 32]);
        assert!(kr.get(2).is_some());
        assert!(kr.get(3).is_none());
    }
}
