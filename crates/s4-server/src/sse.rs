//! Server-side encryption (SSE-S4) — AES-256-GCM (v0.4 #21, v0.5 #29, v0.5 #27).
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
//! ### S4E3 (v0.5 #27) — SSE-C, customer-provided key
//!
//! ```text
//! [magic:   "S4E3" 4B]
//! [algo:    u8]            # 1 = AES-256-GCM
//! [key_md5: 16B]           # MD5 fingerprint of the customer key
//! [nonce:   12B]           # random per-object
//! [tag:     16B]           # AES-GCM authentication tag
//! [ciphertext: variable]
//! ```
//!
//! Overhead: 49 bytes (`4 + 1 + 16 + 12 + 16`). Unlike S4E1/S4E2 the
//! gateway does **not** persist the key — the client supplies it on
//! every PUT/GET via `x-amz-server-side-encryption-customer-{algorithm,
//! key,key-MD5}` headers. We store only the 16-byte MD5 in the on-disk
//! frame so a GET with the wrong key surfaces as
//! [`SseError::WrongCustomerKey`] before AES-GCM is even tried (saves a
//! useless decrypt + gives operators a distinct error from generic auth
//! failure).
//!
//! The `key_md5` is included in the AAD so flipping a single byte of
//! the stored fingerprint also breaks AES-GCM auth — i.e. an attacker
//! who tampered with the metadata can't sneak a different key past the
//! check.
//!
//! ## v0.5 rotation flow (SSE-S4 only)
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
//! - `S4E3`: keyring is **not** consulted. Caller must supply
//!   [`SseSource::CustomerKey`] with the matching key + md5.
//!
//! ## Open follow-ups
//!
//! - **Server-managed key only** (for SSE-S4): keys come from local
//!   files via `--sse-s4-key` / `--sse-s4-key-rotated`. KMS / vault
//!   integration is a separate issue.
//! - **SSE-KMS** (S3 calls it `aws:kms`) is a separate frame variant.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use bytes::Bytes;
use md5::{Digest as Md5Digest, Md5};
use rand::RngCore;
use thiserror::Error;

pub const SSE_MAGIC_V1: &[u8; 4] = b"S4E1";
pub const SSE_MAGIC_V2: &[u8; 4] = b"S4E2";
pub const SSE_MAGIC_V3: &[u8; 4] = b"S4E3";
/// Back-compat alias — v0.4 callers that imported `SSE_MAGIC` mean S4E1.
pub const SSE_MAGIC: &[u8; 4] = SSE_MAGIC_V1;

/// Header layout matches between S4E1 and S4E2 (both 36 bytes total)
/// because S4E2 reuses the 3-byte reserved slot to fit `key_id (2B) +
/// reserved (1B)`. Keeping them the same length means the rest of the
/// pipeline (sidecar offsets, multipart math) doesn't care which
/// frame variant is in flight.
pub const SSE_HEADER_BYTES: usize = 4 + 1 + 3 + 12 + 16; // = 36
/// S4E3 (SSE-C) replaces the 3-byte reserved area with a 16-byte
/// customer-key MD5 fingerprint, so the header is 49 bytes total.
/// `magic 4 + algo 1 + key_md5 16 + nonce 12 + tag 16`.
pub const SSE_HEADER_BYTES_V3: usize = 4 + 1 + KEY_MD5_LEN + 12 + 16; // = 49
pub const ALGO_AES_256_GCM: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const KEY_MD5_LEN: usize = 16;
/// AWS S3 SSE-C only allows AES256 in the
/// `x-amz-server-side-encryption-customer-algorithm` header, so we
/// match that exact spelling for parity with real S3 clients.
pub const SSE_C_ALGORITHM: &str = "AES256";

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
    // --- v0.5 #27: SSE-C specific errors ---
    /// The MD5 fingerprint stored in the S4E3 frame doesn't match the
    /// MD5 of the customer key the client supplied. This is the
    /// "wrong customer key on GET" signal — distinct from
    /// `DecryptFailed` so service.rs can map it to AWS S3's
    /// `403 AccessDenied` (S3 returns AccessDenied when the supplied
    /// SSE-C key doesn't match the one used at PUT time).
    #[error("SSE-C key MD5 fingerprint mismatch — client supplied a different key than PUT")]
    WrongCustomerKey,
    /// `parse_customer_key_headers` saw a malformed input. `reason` is
    /// a short human string ("base64 decode of key", "key length",
    /// "md5 length", "md5 mismatch") for operator log lines — never
    /// echoed to the client (would leak crypto details).
    #[error("SSE-C customer-key headers invalid: {reason}")]
    InvalidCustomerKey { reason: &'static str },
    /// Client asked for an SSE-C algorithm the gateway doesn't speak.
    /// AWS S3 only ever defines `AES256` here; surfacing the offending
    /// string lets us 400 with a useful message.
    #[error("SSE-C algorithm {algo:?} unsupported (only {SSE_C_ALGORITHM:?} is allowed)")]
    CustomerKeyAlgorithmUnsupported { algo: String },
    /// S4E3 body lacks an SSE-C key — caller passed `SseSource::Keyring`
    /// when decrypting an SSE-C-encrypted object. service.rs should
    /// translate this into the same "missing customer key" 400 that
    /// AWS S3 returns when SSE-C headers are absent on a GET.
    #[error("S4E3 frame requires SseSource::CustomerKey; got Keyring")]
    CustomerKeyRequired,
    /// Inverse: client sent SSE-C headers on a GET for an object stored
    /// without SSE-C. The supplied key has no role in decryption, but
    /// AWS S3 actually 400s in this case ("expected an unencrypted
    /// object" / "extraneous SSE-C headers"), so we mirror that.
    #[error("S4E1/S4E2 frame stored without SSE-C; SseSource::CustomerKey is unexpected")]
    CustomerKeyUnexpected,
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

/// AAD for S4E3 = magic (4) + algo (1) + key_md5 (16). Putting the
/// fingerprint in the AAD means tampering with the stored MD5 (e.g. an
/// attacker rewriting the header to match a *different* key they
/// happen to know) breaks the AES-GCM tag — the wrong-key check isn't
/// just a plain `==` we could be tricked past.
fn aad_v3(key_md5: &[u8; KEY_MD5_LEN]) -> [u8; 4 + 1 + KEY_MD5_LEN] {
    let mut aad = [0u8; 4 + 1 + KEY_MD5_LEN];
    aad[..4].copy_from_slice(SSE_MAGIC_V3);
    aad[4] = ALGO_AES_256_GCM;
    aad[5..5 + KEY_MD5_LEN].copy_from_slice(key_md5);
    aad
}

/// Parsed + verified SSE-C key material from the three customer
/// headers. `key_md5` is the MD5 of `key` (we recompute and compare in
/// [`parse_customer_key_headers`] — clients send their own to catch
/// transport corruption, but we *trust* our own computation as the
/// canonical fingerprint in the S4E3 frame).
#[derive(Clone)]
pub struct CustomerKeyMaterial {
    pub key: [u8; KEY_LEN],
    pub key_md5: [u8; KEY_MD5_LEN],
}

impl std::fmt::Debug for CustomerKeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the key into logs. The MD5 is a public fingerprint
        // (S3 puts it on the wire), so that's safe to show.
        f.debug_struct("CustomerKeyMaterial")
            .field("key", &"<redacted>")
            .field("key_md5_hex", &hex_lower(&self.key_md5))
            .finish()
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Source of the encryption key for [`encrypt_with_source`] /
/// [`decrypt`]. SSE-S4 (server-managed, rotation-aware) goes through
/// `Keyring`; SSE-C (customer-supplied) goes through `CustomerKey`.
///
/// Borrowed (not owned) so the caller can hold a long-lived
/// `CustomerKeyMaterial` next to the request and just lend it for the
/// duration of one PUT/GET.
#[derive(Debug, Clone, Copy)]
pub enum SseSource<'a> {
    /// Server-managed keyring path → produces / consumes S4E1 (legacy)
    /// or S4E2 (rotation-aware) frames.
    Keyring(&'a SseKeyring),
    /// Client-supplied AES-256 key + its MD5 fingerprint → produces /
    /// consumes S4E3 frames. The server never persists the key; it
    /// stores `key_md5` only.
    CustomerKey {
        key: &'a [u8; KEY_LEN],
        key_md5: &'a [u8; KEY_MD5_LEN],
    },
}

/// Back-compat coercion: existing call sites pass `&SseKeyring`
/// directly to [`decrypt`]. With this `From` impl the generic bound
/// `Into<SseSource>` accepts `&SseKeyring` without the caller writing
/// `.into()`, keeping v0.4 / v0.5 #29 service.rs callers compiling
/// untouched while v0.5 #27 SSE-C callers pass `SseSource::CustomerKey`
/// explicitly.
impl<'a> From<&'a SseKeyring> for SseSource<'a> {
    fn from(kr: &'a SseKeyring) -> Self {
        SseSource::Keyring(kr)
    }
}

/// service.rs holds keyring as `Option<Arc<SseKeyring>>` and unwraps to
/// `&Arc<SseKeyring>` — let that coerce too, otherwise every existing
/// call site needs `.as_ref()` boilerplate.
impl<'a> From<&'a Arc<SseKeyring>> for SseSource<'a> {
    fn from(kr: &'a Arc<SseKeyring>) -> Self {
        SseSource::Keyring(kr.as_ref())
    }
}

impl<'a> From<&'a CustomerKeyMaterial> for SseSource<'a> {
    fn from(m: &'a CustomerKeyMaterial) -> Self {
        SseSource::CustomerKey {
            key: &m.key,
            key_md5: &m.key_md5,
        }
    }
}

/// Parse the three AWS SSE-C headers and return verified key material.
///
/// Validates, in order:
/// 1. `algorithm == "AES256"` (the only value AWS S3 defines).
/// 2. `key_base64` decodes to exactly 32 bytes (AES-256 key length).
/// 3. `key_md5_base64` decodes to exactly 16 bytes (MD5 digest length).
/// 4. The actual MD5 of the decoded key matches the supplied MD5.
///
/// Step 4 catches transport corruption *and* a class of programming
/// bugs where the client signs with one key but uploads another. AWS
/// S3 also performs this check.
pub fn parse_customer_key_headers(
    algorithm: &str,
    key_base64: &str,
    key_md5_base64: &str,
) -> Result<CustomerKeyMaterial, SseError> {
    use base64::Engine as _;
    if algorithm != SSE_C_ALGORITHM {
        return Err(SseError::CustomerKeyAlgorithmUnsupported {
            algo: algorithm.to_string(),
        });
    }
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_base64.trim().as_bytes())
        .map_err(|_| SseError::InvalidCustomerKey {
            reason: "base64 decode of key",
        })?;
    if key_bytes.len() != KEY_LEN {
        return Err(SseError::InvalidCustomerKey {
            reason: "key length (must be 32 bytes after base64 decode)",
        });
    }
    let supplied_md5 = base64::engine::general_purpose::STANDARD
        .decode(key_md5_base64.trim().as_bytes())
        .map_err(|_| SseError::InvalidCustomerKey {
            reason: "base64 decode of key MD5",
        })?;
    if supplied_md5.len() != KEY_MD5_LEN {
        return Err(SseError::InvalidCustomerKey {
            reason: "key MD5 length (must be 16 bytes after base64 decode)",
        });
    }
    let actual_md5 = compute_key_md5(&key_bytes);
    // Constant-time compare — paranoia, MD5 is non-secret but the key
    // it identifies is, so we don't want a timing oracle.
    if !constant_time_eq(&actual_md5, &supplied_md5) {
        return Err(SseError::InvalidCustomerKey {
            reason: "supplied MD5 does not match MD5 of supplied key",
        });
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&key_bytes);
    let mut key_md5 = [0u8; KEY_MD5_LEN];
    key_md5.copy_from_slice(&actual_md5);
    Ok(CustomerKeyMaterial { key, key_md5 })
}

/// Convenience wrapper — compute the MD5 fingerprint of a 32-byte
/// customer key. Callers that already have the bytes (e.g. derived
/// from a KMS unwrap) can use this to construct a
/// [`CustomerKeyMaterial`] directly.
pub fn compute_key_md5(key: &[u8]) -> [u8; KEY_MD5_LEN] {
    let mut h = Md5::new();
    h.update(key);
    let out = h.finalize();
    let mut md5 = [0u8; KEY_MD5_LEN];
    md5.copy_from_slice(&out);
    md5
}

/// `subtle`-free constant-time byte slice equality. We only need this
/// at one site (MD5 verification) so pulling `subtle` in feels excessive.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// v0.5 #27: encrypt under whichever source the caller picked.
///
/// - `SseSource::Keyring` → delegates to [`encrypt_v2`] (S4E2 frame).
/// - `SseSource::CustomerKey` → writes an S4E3 frame (no key persisted,
///   just the MD5 fingerprint for GET-side verification).
///
/// service.rs picks the source per-request: SSE-C headers present →
/// `CustomerKey`, otherwise (and only when `--sse-s4-key` is wired) →
/// `Keyring`. Plaintext objects skip this function entirely.
pub fn encrypt_with_source(plaintext: &[u8], source: SseSource<'_>) -> Bytes {
    match source {
        SseSource::Keyring(kr) => encrypt_v2(plaintext, kr),
        SseSource::CustomerKey { key, key_md5 } => encrypt_v3(plaintext, key, key_md5),
    }
}

fn encrypt_v3(
    plaintext: &[u8],
    key: &[u8; KEY_LEN],
    key_md5: &[u8; KEY_MD5_LEN],
) -> Bytes {
    let aes_key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(aes_key);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = aad_v3(key_md5);
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

    let mut out = Vec::with_capacity(SSE_HEADER_BYTES_V3 + ct.len());
    out.extend_from_slice(SSE_MAGIC_V3);
    out.push(ALGO_AES_256_GCM);
    out.extend_from_slice(key_md5);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    Bytes::from(out)
}

/// v0.5 #29 + v0.5 #27: dispatch on the body's magic and decrypt under
/// whichever source the caller supplied.
///
/// - `S4E1` / `S4E2` require `SseSource::Keyring` (return
///   [`SseError::CustomerKeyRequired`] for `CustomerKey` — service.rs
///   should map this to "extraneous SSE-C headers" 400).
/// - `S4E3` requires `SseSource::CustomerKey` (return
///   [`SseError::CustomerKeyUnexpected`] for `Keyring` — service.rs
///   should map this to "missing SSE-C headers" 400).
///
/// Generic over `Into<SseSource>` so existing `decrypt(body, &keyring)`
/// call sites compile unchanged via the `From<&SseKeyring>` impl above
/// — only the new SSE-C path needs to type out
/// `SseSource::CustomerKey { .. }`.
///
/// Distinct errors (`KeyNotInKeyring`, `DecryptFailed`,
/// `WrongCustomerKey`) let operators tell rotation gaps, ciphertext
/// tampering, and SSE-C key mismatch apart in audit logs.
pub fn decrypt<'a, S: Into<SseSource<'a>>>(body: &[u8], source: S) -> Result<Bytes, SseError> {
    let source = source.into();
    // Outer short-check uses the smaller of the two header sizes
    // (S4E1/S4E2 = 36 bytes). Anything below this can't be any valid
    // SSE frame regardless of magic — keeps back-compat with v0.4 /
    // v0.5 #29 callers that expected `TooShort` for absurdly short
    // inputs even when the magic is garbage.
    if body.len() < SSE_HEADER_BYTES {
        return Err(SseError::TooShort { got: body.len() });
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&body[..4]);
    match &magic {
        m if m == SSE_MAGIC_V1 || m == SSE_MAGIC_V2 => {
            let keyring = match source {
                SseSource::Keyring(kr) => kr,
                SseSource::CustomerKey { .. } => return Err(SseError::CustomerKeyUnexpected),
            };
            if m == SSE_MAGIC_V1 {
                decrypt_v1_with_keyring(body, keyring)
            } else {
                decrypt_v2_with_keyring(body, keyring)
            }
        }
        m if m == SSE_MAGIC_V3 => {
            // S4E3 has a larger 49-byte header, so re-check.
            if body.len() < SSE_HEADER_BYTES_V3 {
                return Err(SseError::TooShort { got: body.len() });
            }
            let (key, key_md5) = match source {
                SseSource::CustomerKey { key, key_md5 } => (key, key_md5),
                SseSource::Keyring(_) => return Err(SseError::CustomerKeyRequired),
            };
            decrypt_v3(body, key, key_md5)
        }
        _ => Err(SseError::BadMagic { got: magic }),
    }
}

fn decrypt_v3(
    body: &[u8],
    key: &[u8; KEY_LEN],
    supplied_md5: &[u8; KEY_MD5_LEN],
) -> Result<Bytes, SseError> {
    let algo = body[4];
    if algo != ALGO_AES_256_GCM {
        return Err(SseError::UnsupportedAlgo { tag: algo });
    }
    let mut stored_md5 = [0u8; KEY_MD5_LEN];
    stored_md5.copy_from_slice(&body[5..5 + KEY_MD5_LEN]);
    // Cheap fingerprint check first — if the supplied key has a
    // different MD5 than what was used at PUT, fail fast with a
    // dedicated error. AES-GCM auth would also catch this (different
    // key → bad tag) but the bespoke error gives operators an audit
    // signal distinct from "ciphertext was tampered with".
    if !constant_time_eq(supplied_md5, &stored_md5) {
        return Err(SseError::WrongCustomerKey);
    }
    let nonce_off = 5 + KEY_MD5_LEN;
    let tag_off = nonce_off + NONCE_LEN;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&body[nonce_off..nonce_off + NONCE_LEN]);
    let mut tag_bytes = [0u8; TAG_LEN];
    tag_bytes.copy_from_slice(&body[tag_off..tag_off + TAG_LEN]);
    let ct = &body[SSE_HEADER_BYTES_V3..];

    let aad = aad_v3(&stored_md5);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut ct_with_tag = Vec::with_capacity(ct.len() + TAG_LEN);
    ct_with_tag.extend_from_slice(ct);
    ct_with_tag.extend_from_slice(&tag_bytes);

    let aes_key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(aes_key);
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

/// Detect whether `body` is SSE-S4 encrypted (S4E1, S4E2, or S4E3) by
/// sniffing the first 4 magic bytes. Used by the GET path to decide
/// whether to run decryption before frame parsing.
///
/// We require a length check that's safe for *any* of the three
/// frames — `SSE_HEADER_BYTES` (36) is the smallest valid header
/// (S4E1 / S4E2). S4E3 is 49 bytes; the per-frame decrypt path
/// re-checks the appropriate minimum, so this 36-byte gate is just a
/// fast rejection of obviously-too-short bodies.
pub fn looks_encrypted(body: &[u8]) -> bool {
    if body.len() < SSE_HEADER_BYTES {
        return false;
    }
    let m = &body[..4];
    m == SSE_MAGIC_V1 || m == SSE_MAGIC_V2 || m == SSE_MAGIC_V3
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

    // -----------------------------------------------------------------
    // v0.5 #27 — SSE-C (customer-provided key, S4E3 frame) tests
    // -----------------------------------------------------------------

    use base64::Engine as _;

    fn cust_key(seed: u8) -> CustomerKeyMaterial {
        let key = [seed; KEY_LEN];
        let key_md5 = compute_key_md5(&key);
        CustomerKeyMaterial { key, key_md5 }
    }

    #[test]
    fn s4e3_roundtrip_happy_path() {
        let m = cust_key(42);
        let pt = b"top-secret SSE-C payload";
        let ct = encrypt_with_source(
            pt,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &m.key_md5,
            },
        );
        // Frame inspection.
        assert_eq!(&ct[..4], SSE_MAGIC_V3);
        assert_eq!(ct[4], ALGO_AES_256_GCM);
        assert_eq!(&ct[5..5 + KEY_MD5_LEN], &m.key_md5);
        assert_eq!(ct.len(), SSE_HEADER_BYTES_V3 + pt.len());
        assert!(looks_encrypted(&ct));
        // Decrypt round-trip.
        let plain = decrypt(
            &ct,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &m.key_md5,
            },
        )
        .unwrap();
        assert_eq!(plain.as_ref(), pt);
        // And via the From impl on &CustomerKeyMaterial.
        let plain2 = decrypt(&ct, &m).unwrap();
        assert_eq!(plain2.as_ref(), pt);
    }

    #[test]
    fn s4e3_wrong_key_yields_wrong_customer_key_error() {
        let m = cust_key(1);
        let other = cust_key(2);
        let ct = encrypt_with_source(b"payload", (&m).into());
        let err = decrypt(
            &ct,
            SseSource::CustomerKey {
                key: &other.key,
                key_md5: &other.key_md5,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SseError::WrongCustomerKey), "got {err:?}");
    }

    #[test]
    fn s4e3_tampered_stored_md5_is_caught() {
        // Attacker rewrites the stored MD5 to match a key they know.
        // Even though the supplied (attacker) key matches the rewritten
        // MD5, AES-GCM authenticates the ORIGINAL md5 via AAD, so the
        // tag check fails. Surface: WrongCustomerKey if the supplied
        // md5 != stored md5 (this test), or DecryptFailed if attacker
        // also rewrites their supplied md5 to match.
        let m = cust_key(7);
        let mut ct = encrypt_with_source(b"victim payload", (&m).into()).to_vec();
        // Flip a byte in the stored fingerprint.
        ct[5] ^= 0x55;
        // Client supplies the original (unmodified) key + md5.
        let err = decrypt(
            &ct,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &m.key_md5,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SseError::WrongCustomerKey), "got {err:?}");
    }

    #[test]
    fn s4e3_tampered_md5_with_matching_supplied_md5_fails_aead() {
        // Both stored md5 AND supplied md5 are flipped to the same bogus
        // value. The fingerprint check passes (they match) but AAD
        // authenticates the *original* md5, so AES-GCM fails.
        let m = cust_key(3);
        let mut ct = encrypt_with_source(b"x", (&m).into()).to_vec();
        ct[5] ^= 0xFF;
        let mut bogus_md5 = m.key_md5;
        bogus_md5[0] ^= 0xFF;
        let err = decrypt(
            &ct,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &bogus_md5,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed), "got {err:?}");
    }

    #[test]
    fn s4e3_tampered_ciphertext_fails_aead() {
        let m = cust_key(8);
        let mut ct = encrypt_with_source(b"sealed message", (&m).into()).to_vec();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let err = decrypt(&ct, &m).unwrap_err();
        assert!(matches!(err, SseError::DecryptFailed), "got {err:?}");
    }

    #[test]
    fn s4e3_tampered_algo_byte_rejected() {
        let m = cust_key(9);
        let mut ct = encrypt_with_source(b"x", (&m).into()).to_vec();
        ct[4] = 99;
        let err = decrypt(&ct, &m).unwrap_err();
        assert!(matches!(err, SseError::UnsupportedAlgo { tag: 99 }));
    }

    #[test]
    fn s4e3_uses_random_nonce() {
        let m = cust_key(10);
        let a = encrypt_with_source(b"deterministic input", (&m).into());
        let b = encrypt_with_source(b"deterministic input", (&m).into());
        assert_ne!(a, b, "nonce must be random per-call");
    }

    #[test]
    fn parse_customer_key_headers_happy_path() {
        let key = [11u8; KEY_LEN];
        let md5 = compute_key_md5(&key);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let md5_b64 = base64::engine::general_purpose::STANDARD.encode(md5);
        let m = parse_customer_key_headers("AES256", &key_b64, &md5_b64).unwrap();
        assert_eq!(m.key, key);
        assert_eq!(m.key_md5, md5);
    }

    #[test]
    fn parse_customer_key_headers_rejects_wrong_algorithm() {
        let key = [1u8; KEY_LEN];
        let md5 = compute_key_md5(&key);
        let kb = base64::engine::general_purpose::STANDARD.encode(key);
        let mb = base64::engine::general_purpose::STANDARD.encode(md5);
        let err = parse_customer_key_headers("AES128", &kb, &mb).unwrap_err();
        assert!(
            matches!(err, SseError::CustomerKeyAlgorithmUnsupported { ref algo } if algo == "AES128"),
            "got {err:?}"
        );
        // Lowercase variant still rejected (AWS S3 accepts only "AES256").
        let err2 = parse_customer_key_headers("aes256", &kb, &mb).unwrap_err();
        assert!(
            matches!(err2, SseError::CustomerKeyAlgorithmUnsupported { .. }),
            "got {err2:?}"
        );
    }

    #[test]
    fn parse_customer_key_headers_rejects_wrong_key_length() {
        let short_key = vec![5u8; 16]; // half-length AES key
        let md5 = compute_key_md5(&short_key);
        let kb = base64::engine::general_purpose::STANDARD.encode(&short_key);
        let mb = base64::engine::general_purpose::STANDARD.encode(md5);
        let err = parse_customer_key_headers("AES256", &kb, &mb).unwrap_err();
        assert!(
            matches!(err, SseError::InvalidCustomerKey { reason } if reason.contains("key length")),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_customer_key_headers_rejects_wrong_md5_length() {
        let key = [3u8; KEY_LEN];
        let kb = base64::engine::general_purpose::STANDARD.encode(key);
        // Truncated MD5 (15 bytes instead of 16).
        let bad_md5 = vec![0u8; 15];
        let mb = base64::engine::general_purpose::STANDARD.encode(bad_md5);
        let err = parse_customer_key_headers("AES256", &kb, &mb).unwrap_err();
        assert!(
            matches!(err, SseError::InvalidCustomerKey { reason } if reason.contains("MD5 length")),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_customer_key_headers_rejects_md5_mismatch() {
        let key = [4u8; KEY_LEN];
        let other = [5u8; KEY_LEN];
        let kb = base64::engine::general_purpose::STANDARD.encode(key);
        let wrong_md5 = compute_key_md5(&other);
        let mb = base64::engine::general_purpose::STANDARD.encode(wrong_md5);
        let err = parse_customer_key_headers("AES256", &kb, &mb).unwrap_err();
        assert!(
            matches!(err, SseError::InvalidCustomerKey { reason } if reason.contains("MD5 does not match")),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_customer_key_headers_rejects_bad_base64() {
        let valid_key = [0u8; KEY_LEN];
        let md5 = compute_key_md5(&valid_key);
        let mb = base64::engine::general_purpose::STANDARD.encode(md5);
        let err = parse_customer_key_headers("AES256", "!!!not-base64!!!", &mb).unwrap_err();
        assert!(
            matches!(err, SseError::InvalidCustomerKey { reason } if reason.contains("base64")),
            "got {err:?}"
        );
        // Bad MD5 base64.
        let kb = base64::engine::general_purpose::STANDARD.encode(valid_key);
        let err2 = parse_customer_key_headers("AES256", &kb, "??not-base64??").unwrap_err();
        assert!(
            matches!(err2, SseError::InvalidCustomerKey { reason } if reason.contains("base64")),
            "got {err2:?}"
        );
    }

    #[test]
    fn parse_customer_key_headers_trims_whitespace() {
        // S3 SDKs sometimes pad headers with trailing newlines.
        let key = [12u8; KEY_LEN];
        let md5 = compute_key_md5(&key);
        let kb = format!(
            "  {}\n",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let mb = format!(
            "\t{}  ",
            base64::engine::general_purpose::STANDARD.encode(md5)
        );
        let m = parse_customer_key_headers("AES256", &kb, &mb).unwrap();
        assert_eq!(m.key, key);
    }

    // -----------------------------------------------------------------
    // Back-compat + cross-source mixing
    // -----------------------------------------------------------------

    #[test]
    fn back_compat_decrypt_s4e1_with_keyring_source() {
        let k = key32(33);
        let legacy_ct = encrypt(&k, b"v0.4 vintage object");
        let kr = SseKeyring::new(1, Arc::clone(&k));
        // Both call styles must work — `&kr` (back-compat) and
        // `SseSource::Keyring(&kr)` (explicit).
        let plain = decrypt(&legacy_ct, &kr).unwrap();
        assert_eq!(plain.as_ref(), b"v0.4 vintage object");
        let plain2 = decrypt(&legacy_ct, SseSource::Keyring(&kr)).unwrap();
        assert_eq!(plain2.as_ref(), b"v0.4 vintage object");
    }

    #[test]
    fn back_compat_decrypt_s4e2_with_keyring_source() {
        let kr = keyring_single(34);
        let ct = encrypt_v2(b"v0.5 #29 object", &kr);
        let plain = decrypt(&ct, &kr).unwrap();
        assert_eq!(plain.as_ref(), b"v0.5 #29 object");
        // encrypt_with_source(Keyring) should produce the same wire
        // format (S4E2).
        let ct2 = encrypt_with_source(b"v0.5 #29 object", SseSource::Keyring(&kr));
        assert_eq!(&ct2[..4], SSE_MAGIC_V2);
        let plain2 = decrypt(&ct2, &kr).unwrap();
        assert_eq!(plain2.as_ref(), b"v0.5 #29 object");
    }

    #[test]
    fn s4e2_blob_with_customer_key_source_is_rejected() {
        // An object stored with SSE-S4 (S4E2) but a client sending
        // SSE-C headers on the GET — this is a misuse, surface as
        // CustomerKeyUnexpected so service.rs can return 400.
        let kr = keyring_single(50);
        let ct = encrypt_v2(b"server-managed object", &kr);
        let m = cust_key(99);
        let err = decrypt(
            &ct,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &m.key_md5,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SseError::CustomerKeyUnexpected), "got {err:?}");
    }

    #[test]
    fn s4e3_blob_with_keyring_source_is_rejected() {
        // Inverse: object is SSE-C (S4E3) but client forgot to send
        // SSE-C headers. Service.rs should map this to 400.
        let m = cust_key(60);
        let ct = encrypt_with_source(b"customer-key object", (&m).into());
        let kr = keyring_single(60);
        let err = decrypt(&ct, &kr).unwrap_err();
        assert!(matches!(err, SseError::CustomerKeyRequired), "got {err:?}");
    }

    #[test]
    fn looks_encrypted_detects_s4e3() {
        let m = cust_key(13);
        let ct = encrypt_with_source(b"x", (&m).into());
        assert!(looks_encrypted(&ct));
    }

    #[test]
    fn s4e3_rejects_short_body() {
        // 36 bytes passes the looks_encrypted gate but is shorter than
        // S4E3's 49-byte header.
        let mut short = Vec::new();
        short.extend_from_slice(SSE_MAGIC_V3);
        short.push(ALGO_AES_256_GCM);
        // Padding to 36 bytes (SSE_HEADER_BYTES) so the outer length
        // check passes but the S4E3 inner check fails.
        short.extend_from_slice(&[0u8; SSE_HEADER_BYTES - 5]);
        assert_eq!(short.len(), SSE_HEADER_BYTES);
        let m = cust_key(1);
        let err = decrypt(
            &short,
            SseSource::CustomerKey {
                key: &m.key,
                key_md5: &m.key_md5,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SseError::TooShort { .. }), "got {err:?}");
    }

    #[test]
    fn customer_key_material_debug_redacts_key() {
        let m = cust_key(99);
        let s = format!("{m:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains(&format!("{:?}", m.key.as_slice())));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn compute_key_md5_known_vector() {
        // Empty input MD5 is known: d41d8cd98f00b204e9800998ecf8427e.
        let got = compute_key_md5(b"");
        let expected_hex = "d41d8cd98f00b204e9800998ecf8427e";
        assert_eq!(hex_lower(&got), expected_hex);
    }
}
