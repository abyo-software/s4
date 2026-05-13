//! KMS backend abstraction for SSE-KMS envelope encryption (v0.5 #28).
//!
//! Per-object DEK (Data Encryption Key, 256-bit AES) is wrapped by a
//! KEK (Key Encryption Key) held in a pluggable KMS backend. The
//! plaintext DEK is used in-memory only — only the wrapped form is
//! persisted alongside the ciphertext (in the S4E4 frame written by
//! [`crate::sse::encrypt_with_source`]).
//!
//! ## Why envelope encryption?
//!
//! - **Per-object key** = blast radius of a key compromise is one
//!   object, not the whole tenant.
//! - **KEK never leaves the KMS** = the plaintext bytes of the master
//!   key are not memory-resident in the gateway. Only DEKs are.
//! - **Server-side rotation cheap** = rotate the KEK in KMS, re-wrap
//!   DEKs lazily on next PUT/GET. The ciphertext bodies don't move.
//!
//! ## Backends
//!
//! - [`LocalKms`] — file-backed KEK store for dev / on-prem / air-gap.
//!   Default-features. AES-256-GCM wrap with a fresh 12-byte nonce per
//!   call; the wrapped form is `nonce || ciphertext || tag`.
//! - [`aws::AwsKms`] — AWS KMS via `aws-sdk-kms`. Behind the
//!   `aws-kms` cargo feature (off by default to keep the default build
//!   from pulling the entire aws-sdk-kms tree). Calls `GenerateDataKey`
//!   for fresh DEKs and `Decrypt` for unwrap.
//!
//! ## Async-ness
//!
//! Both methods on [`KmsBackend`] are `async` — even the file-backed
//! `LocalKms` returns a future, because real KMS backends do
//! network I/O and we want the trait shape to stay compatible. The
//! `LocalKms` futures resolve immediately.

use std::collections::HashMap;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use async_trait::async_trait;
use rand::RngCore;

const KEK_LEN: usize = 32;
const DEK_LEN: usize = 32;
const WRAP_NONCE_LEN: usize = 12;
const WRAP_TAG_LEN: usize = 16;
/// Minimum size of a `WrappedDek::ciphertext` produced by [`LocalKms`]:
/// 12-byte nonce + at least the 16-byte AES-GCM tag (DEK is 32 bytes,
/// so the actual minimum is 12 + 32 + 16 = 60, but we check the floor
/// at 12 + 16 = 28 to give a clearer error than a panic on slice
/// overflow).
const LOCAL_WRAP_MIN_LEN: usize = WRAP_NONCE_LEN + WRAP_TAG_LEN;

#[derive(Debug, thiserror::Error)]
pub enum KmsError {
    #[error("KMS key id {key_id:?} not found in backend")]
    KeyNotFound { key_id: String },
    #[error("KMS KEK file {path:?}: {source}")]
    KekFileIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("KMS KEK file {path:?} must be exactly {expected} raw bytes; got {got}")]
    KekBadLength {
        path: PathBuf,
        expected: usize,
        got: usize,
    },
    #[error("KMS KEK directory {path:?}: {source}")]
    KekDirIo {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `LocalKms` saw a wrapped-DEK ciphertext shorter than the
    /// minimum (nonce + tag). Surface as a distinct error so audit
    /// logs can tell "metadata corruption / truncation" apart from
    /// "wrong key" / "tampered with".
    #[error("KMS wrapped DEK too short ({got} bytes; need at least {min})")]
    WrappedDekTooShort { got: usize, min: usize },
    /// AES-GCM authentication failure on unwrap. Either the wrapped
    /// DEK was tampered with, or it was wrapped under a different
    /// KEK than the one we're holding for `key_id`.
    #[error("KMS unwrap failed (wrapped DEK auth tag mismatch for key_id {key_id:?})")]
    UnwrapFailed { key_id: String },
    /// Backend-specific transport error (network, AWS SDK, etc).
    /// `source` is type-erased so the trait stays object-safe.
    #[error("KMS backend unavailable: {message}")]
    BackendUnavailable { message: String },
}

/// Wrapped DEK as stored in the S4E4 frame.
///
/// `key_id` identifies which KEK in the backend was used to wrap
/// `ciphertext`. Both fields are AAD-authenticated by the outer
/// AES-GCM tag in the S4E4 frame, so an attacker can't substitute a
/// different `key_id` to make the gateway try a different KEK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedDek {
    /// KEK identifier, caller-meaningful. For [`LocalKms`] this is
    /// the basename of the `.kek` file (without extension); for
    /// [`aws::AwsKms`] it is the KMS key ARN or alias.
    pub key_id: String,
    /// Encrypted DEK bytes. Format is backend-defined — for
    /// `LocalKms` it is `nonce(12) || ciphertext(32) || tag(16)`;
    /// for AWS KMS it is the opaque blob returned by `GenerateDataKey`.
    pub ciphertext: Vec<u8>,
}

#[async_trait]
pub trait KmsBackend: Send + Sync + std::fmt::Debug {
    /// Generate a fresh 32-byte DEK and return both the plaintext
    /// (used immediately for AES-GCM encryption of the object body)
    /// and the wrapped form (persisted in the S4E4 frame).
    ///
    /// `key_id` selects which KEK to wrap under. For `LocalKms` an
    /// unknown id is [`KmsError::KeyNotFound`]; for AWS KMS an unknown
    /// ARN surfaces as [`KmsError::BackendUnavailable`] (the AWS SDK
    /// returns NotFound but we don't want callers leaking ARN existence
    /// to clients).
    async fn generate_dek(&self, key_id: &str) -> Result<(Vec<u8>, WrappedDek), KmsError>;

    /// Unwrap a stored DEK ciphertext back to plaintext for the
    /// decrypt path. The 32-byte plaintext must be zeroed by the
    /// caller after use (callers in this crate hold it in a stack
    /// `[u8; 32]` for the duration of one GET).
    async fn decrypt_dek(&self, wrapped: &WrappedDek) -> Result<Vec<u8>, KmsError>;
}

/// File-based KEK store for dev / on-prem deployments.
///
/// ## Layout
///
/// ```text
/// <dir>/
///   alpha.kek         # 32 raw bytes — KEK for key_id "alpha"
///   beta.kek          # 32 raw bytes — KEK for key_id "beta"
/// ```
///
/// Files are loaded eagerly at [`LocalKms::open`] time; subsequent
/// adds/removals require a restart. KEK files MUST be exactly 32
/// bytes (other formats — hex / base64 — are intentionally not
/// accepted here, unlike [`crate::sse::SseKey`], because operators
/// generating KEKs for KMS use should produce raw randomness from
/// `/dev/urandom` rather than human-edited files).
///
/// ## Wrap algorithm
///
/// `LocalKms` wraps DEKs with AES-256-GCM using the KEK as the cipher
/// key. The wrapped form is `nonce(12) || ciphertext(32) || tag(16)`
/// = 60 bytes for a 32-byte DEK. The nonce is fresh per wrap, drawn
/// from `OsRng`; the AAD is the UTF-8 `key_id` so a wrap under one id
/// can't be replayed under another.
pub struct LocalKms {
    dir: PathBuf,
    keks: HashMap<String, [u8; KEK_LEN]>,
}

impl std::fmt::Debug for LocalKms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalKms")
            .field("dir", &self.dir)
            .field("key_count", &self.keks.len())
            .field("key_ids", &self.keks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl LocalKms {
    /// Open a KEK directory. Reads every `*.kek` file; each must be
    /// exactly 32 raw bytes. The basename (sans `.kek`) becomes the
    /// `key_id` used in [`KmsBackend::generate_dek`] / [`WrappedDek`].
    ///
    /// An empty directory is a valid (but useless) state — callers
    /// that haven't loaded any KEKs will still see all `generate_dek`
    /// calls return [`KmsError::KeyNotFound`].
    pub fn open(dir: PathBuf) -> Result<Self, KmsError> {
        let read_dir = std::fs::read_dir(&dir).map_err(|source| KmsError::KekDirIo {
            path: dir.clone(),
            source,
        })?;
        let mut keks = HashMap::new();
        for entry in read_dir {
            let entry = entry.map_err(|source| KmsError::KekDirIo {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("kek") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let key_id = stem.to_string();
            let bytes = std::fs::read(&path).map_err(|source| KmsError::KekFileIo {
                path: path.clone(),
                source,
            })?;
            if bytes.len() != KEK_LEN {
                return Err(KmsError::KekBadLength {
                    path: path.clone(),
                    expected: KEK_LEN,
                    got: bytes.len(),
                });
            }
            let mut k = [0u8; KEK_LEN];
            k.copy_from_slice(&bytes);
            keks.insert(key_id, k);
        }
        Ok(Self { dir, keks })
    }

    /// Construct a `LocalKms` directly from in-memory KEKs. Useful
    /// for tests and for callers that load KEKs out of band (e.g.
    /// from a sealed config blob). Production deployments should
    /// prefer [`LocalKms::open`].
    pub fn from_keks(dir: PathBuf, keks: HashMap<String, [u8; KEK_LEN]>) -> Self {
        Self { dir, keks }
    }

    /// Sorted list of key ids present in this backend. Used by the
    /// CLI `--list-kms-keys` flag (orchestrator wires that) and by
    /// readiness probes that want to assert a specific key is loaded.
    pub fn key_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.keks.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn kek(&self, key_id: &str) -> Result<&[u8; KEK_LEN], KmsError> {
        self.keks.get(key_id).ok_or_else(|| KmsError::KeyNotFound {
            key_id: key_id.to_string(),
        })
    }
}

#[async_trait]
impl KmsBackend for LocalKms {
    async fn generate_dek(&self, key_id: &str) -> Result<(Vec<u8>, WrappedDek), KmsError> {
        let kek = self.kek(key_id)?;
        let mut dek = vec![0u8; DEK_LEN];
        rand::rngs::OsRng.fill_bytes(&mut dek);

        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
        let mut nonce_bytes = [0u8; WRAP_NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = key_id.as_bytes();
        let ct_with_tag = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: &dek,
                    aad,
                },
            )
            .expect("aes-gcm encrypt cannot fail with a 32-byte key");

        // Layout: nonce || ct_with_tag (the latter already contains
        // the 16-byte trailing tag from the aes-gcm crate).
        let mut wrapped = Vec::with_capacity(WRAP_NONCE_LEN + ct_with_tag.len());
        wrapped.extend_from_slice(&nonce_bytes);
        wrapped.extend_from_slice(&ct_with_tag);

        Ok((
            dek,
            WrappedDek {
                key_id: key_id.to_string(),
                ciphertext: wrapped,
            },
        ))
    }

    async fn decrypt_dek(&self, wrapped: &WrappedDek) -> Result<Vec<u8>, KmsError> {
        let kek = self.kek(&wrapped.key_id)?;
        if wrapped.ciphertext.len() < LOCAL_WRAP_MIN_LEN {
            return Err(KmsError::WrappedDekTooShort {
                got: wrapped.ciphertext.len(),
                min: LOCAL_WRAP_MIN_LEN,
            });
        }
        let (nonce_bytes, ct_with_tag) = wrapped.ciphertext.split_at(WRAP_NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
        let nonce = Nonce::from_slice(nonce_bytes);
        let aad = wrapped.key_id.as_bytes();
        let dek = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ct_with_tag,
                    aad,
                },
            )
            .map_err(|_| KmsError::UnwrapFailed {
                key_id: wrapped.key_id.clone(),
            })?;
        Ok(dek)
    }
}

// ----------------------------------------------------------------------------
// AWS KMS backend (feature-gated)
// ----------------------------------------------------------------------------

#[cfg(feature = "aws-kms")]
pub mod aws {
    //! AWS KMS-backed [`KmsBackend`]. Off by default — enable with
    //! `--features aws-kms`. The backend forwards `generate_dek` to
    //! `GenerateDataKey` (with `KeySpec=AES_256`) and `decrypt_dek`
    //! to `Decrypt`; the wrapped DEK ciphertext is exactly the opaque
    //! blob AWS returns, so we don't double-wrap.
    use super::{KmsBackend, KmsError, WrappedDek};
    use async_trait::async_trait;

    /// AWS KMS-backed KEK store. The `key_id` passed to
    /// [`KmsBackend::generate_dek`] is forwarded as `KeyId` to AWS —
    /// callers can use a key ARN, alias ARN, or alias name. For
    /// [`KmsBackend::decrypt_dek`] AWS re-derives the KEK from
    /// `CiphertextBlob` so the `key_id` field on `WrappedDek` is
    /// effectively a label / audit signal.
    pub struct AwsKms {
        client: aws_sdk_kms::Client,
    }

    impl std::fmt::Debug for AwsKms {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AwsKms").finish()
        }
    }

    impl AwsKms {
        /// Construct an [`AwsKms`] from a pre-built SDK client. Allows
        /// callers to share an SDK config (region, retry, endpoint
        /// override for LocalStack) with the rest of the gateway.
        pub fn new(client: aws_sdk_kms::Client) -> Self {
            Self { client }
        }

        /// Convenience: build a client from the ambient
        /// `aws_config::load_defaults` (env, profile, IMDS, etc).
        pub async fn from_default_env() -> Self {
            let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_kms::Client::new(&cfg);
            Self { client }
        }
    }

    #[async_trait]
    impl KmsBackend for AwsKms {
        async fn generate_dek(&self, key_id: &str) -> Result<(Vec<u8>, WrappedDek), KmsError> {
            let resp = self
                .client
                .generate_data_key()
                .key_id(key_id)
                .key_spec(aws_sdk_kms::types::DataKeySpec::Aes256)
                .send()
                .await
                .map_err(|e| KmsError::BackendUnavailable {
                    message: format!("GenerateDataKey({key_id}): {e}"),
                })?;
            let dek = resp
                .plaintext
                .ok_or_else(|| KmsError::BackendUnavailable {
                    message: format!("GenerateDataKey({key_id}): missing Plaintext in response"),
                })?
                .into_inner();
            let ciphertext = resp
                .ciphertext_blob
                .ok_or_else(|| KmsError::BackendUnavailable {
                    message: format!("GenerateDataKey({key_id}): missing CiphertextBlob in response"),
                })?
                .into_inner();
            // Use the response's KeyId (canonical ARN) when present so
            // we record the resolved key, not the alias the caller
            // passed. Falls back to the original on the unlikely
            // chance AWS doesn't echo it.
            let stored_id = resp.key_id.unwrap_or_else(|| key_id.to_string());
            Ok((
                dek,
                WrappedDek {
                    key_id: stored_id,
                    ciphertext,
                },
            ))
        }

        async fn decrypt_dek(&self, wrapped: &WrappedDek) -> Result<Vec<u8>, KmsError> {
            let resp = self
                .client
                .decrypt()
                .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(
                    wrapped.ciphertext.clone(),
                ))
                .key_id(&wrapped.key_id)
                .send()
                .await
                .map_err(|e| KmsError::BackendUnavailable {
                    message: format!("Decrypt({}): {e}", wrapped.key_id),
                })?;
            let dek = resp
                .plaintext
                .ok_or_else(|| KmsError::BackendUnavailable {
                    message: format!("Decrypt({}): missing Plaintext in response", wrapped.key_id),
                })?
                .into_inner();
            Ok(dek)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_kek(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(format!("{name}.kek")), bytes).unwrap();
    }

    #[tokio::test]
    async fn open_empty_dir_is_ok() {
        let tmp = TempDir::new().unwrap();
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        assert!(kms.key_ids().is_empty());
        // generate_dek with no keys → KeyNotFound.
        let err = kms.generate_dek("missing").await.unwrap_err();
        assert!(
            matches!(err, KmsError::KeyNotFound { ref key_id } if key_id == "missing"),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn open_loads_kek_files_and_skips_others() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "alpha", &[1u8; KEK_LEN]);
        write_kek(tmp.path(), "beta", &[2u8; KEK_LEN]);
        // Non-`.kek` files must be ignored (sidecar metadata, README,
        // editor swap files, etc).
        std::fs::write(tmp.path().join("README"), b"hello").unwrap();
        std::fs::write(tmp.path().join("alpha.kek.bak"), [9u8; 99]).unwrap();
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let ids = kms.key_ids();
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn open_rejects_truncated_kek_file() {
        let tmp = TempDir::new().unwrap();
        // 31 bytes — one short of a valid KEK.
        write_kek(tmp.path(), "short", &[7u8; KEK_LEN - 1]);
        let err = LocalKms::open(tmp.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(
                err,
                KmsError::KekBadLength { expected, got, .. } if expected == KEK_LEN && got == KEK_LEN - 1
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn generate_then_decrypt_roundtrip() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "main", &[42u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let (dek, wrapped) = kms.generate_dek("main").await.unwrap();
        assert_eq!(dek.len(), DEK_LEN);
        assert_eq!(wrapped.key_id, "main");
        // Wrapped form: 12-byte nonce + 32-byte ciphertext + 16-byte
        // tag = 60 bytes.
        assert_eq!(wrapped.ciphertext.len(), WRAP_NONCE_LEN + DEK_LEN + WRAP_TAG_LEN);

        let unwrapped = kms.decrypt_dek(&wrapped).await.unwrap();
        assert_eq!(unwrapped, dek);
    }

    #[tokio::test]
    async fn generate_uses_random_dek_and_nonce() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "k", &[5u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let (dek1, w1) = kms.generate_dek("k").await.unwrap();
        let (dek2, w2) = kms.generate_dek("k").await.unwrap();
        assert_ne!(dek1, dek2, "DEK must be random per call");
        assert_ne!(w1.ciphertext, w2.ciphertext, "wrap nonce must be random per call");
    }

    #[tokio::test]
    async fn decrypt_unknown_key_id_errors() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "real", &[1u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let bogus = WrappedDek {
            key_id: "phantom".to_string(),
            ciphertext: vec![0u8; LOCAL_WRAP_MIN_LEN + DEK_LEN],
        };
        let err = kms.decrypt_dek(&bogus).await.unwrap_err();
        assert!(
            matches!(err, KmsError::KeyNotFound { ref key_id } if key_id == "phantom"),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn decrypt_tampered_ciphertext_fails_unwrap() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "k", &[3u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let (_dek, mut wrapped) = kms.generate_dek("k").await.unwrap();
        // Flip a byte in the encrypted DEK area (not the nonce, not
        // the tag — but AES-GCM auths the whole thing, so any flip
        // anywhere fails).
        let mid = wrapped.ciphertext.len() / 2;
        wrapped.ciphertext[mid] ^= 0xFF;
        let err = kms.decrypt_dek(&wrapped).await.unwrap_err();
        assert!(
            matches!(err, KmsError::UnwrapFailed { ref key_id } if key_id == "k"),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn decrypt_short_ciphertext_errors() {
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "k", &[8u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let bogus = WrappedDek {
            key_id: "k".to_string(),
            ciphertext: vec![0u8; 5], // too small for nonce + tag
        };
        let err = kms.decrypt_dek(&bogus).await.unwrap_err();
        assert!(
            matches!(err, KmsError::WrappedDekTooShort { got: 5, .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn decrypt_wrong_key_id_aad_fails_unwrap() {
        // Wrap under "alpha", then forge a WrappedDek that claims
        // "beta" with the same ciphertext bytes. AAD includes key_id
        // so AES-GCM auth must fail under "beta"'s KEK + "beta" AAD,
        // even if the bytes are the wrap of a real DEK.
        let tmp = TempDir::new().unwrap();
        write_kek(tmp.path(), "alpha", &[1u8; KEK_LEN]);
        write_kek(tmp.path(), "beta", &[2u8; KEK_LEN]);
        let kms = LocalKms::open(tmp.path().to_path_buf()).unwrap();
        let (_dek, wrapped) = kms.generate_dek("alpha").await.unwrap();
        let forged = WrappedDek {
            key_id: "beta".to_string(),
            ciphertext: wrapped.ciphertext.clone(),
        };
        let err = kms.decrypt_dek(&forged).await.unwrap_err();
        assert!(
            matches!(err, KmsError::UnwrapFailed { ref key_id } if key_id == "beta"),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn from_keks_constructor_works() {
        let mut keks = HashMap::new();
        keks.insert("inline".to_string(), [9u8; KEK_LEN]);
        let kms = LocalKms::from_keks(PathBuf::from("/tmp/none"), keks);
        let (_dek, wrapped) = kms.generate_dek("inline").await.unwrap();
        assert_eq!(wrapped.key_id, "inline");
        let _back = kms.decrypt_dek(&wrapped).await.unwrap();
    }

    // -----------------------------------------------------------------
    // AwsKms tests — only compiled with --features aws-kms, and
    // ignored by default since they require live AWS credentials +
    // a real KMS key. Run locally with:
    //   AWS_PROFILE=... S4_KMS_TEST_KEY_ID=arn:... \
    //     cargo test --features aws-kms aws_kms_ -- --ignored
    // CI runs them nightly via .github/workflows/aws-kms-e2e.yml when
    // the AWS_KMS_* repo variables are configured (v0.8.1 #60).
    // -----------------------------------------------------------------

    /// v0.8.1 #60: Real AWS KMS round-trip — exercises GenerateDataKey
    /// followed by Decrypt against an actual KMS key, asserting the
    /// 32-byte DEK survives the wrap/unwrap byte-for-byte. Wrapped form
    /// must NOT equal the plaintext (defends against an `AwsKms` impl
    /// that accidentally stored plaintext in `WrappedDek::ciphertext`).
    /// The canonical-key-id check guards against the AWS SDK silently
    /// dropping `KeyId` from the response — we want the resolved ARN
    /// stored, not whatever alias the caller passed.
    #[cfg(feature = "aws-kms")]
    #[tokio::test]
    #[ignore = "requires AWS credentials and a real KMS key (set S4_KMS_TEST_KEY_ID)"]
    async fn aws_kms_roundtrip() {
        let key_id = std::env::var("S4_KMS_TEST_KEY_ID")
            .expect("S4_KMS_TEST_KEY_ID env var required (real AWS KMS key ARN or alias)");
        let kms = super::aws::AwsKms::from_default_env().await;

        // GenerateDataKey
        let (plaintext_dek, wrapped) = kms
            .generate_dek(&key_id)
            .await
            .expect("generate_dek should succeed against real KMS");
        assert_eq!(
            plaintext_dek.len(),
            DEK_LEN,
            "DEK should be 32 bytes (AES-256)"
        );

        // Wrapped form must differ from plaintext — a wrapper that
        // accidentally returned the plaintext as ciphertext would
        // catastrophically leak the DEK at rest.
        assert_ne!(
            wrapped.ciphertext, plaintext_dek,
            "wrapped DEK must NOT equal plaintext DEK"
        );

        // Decrypt round-trip — must byte-equal the original DEK.
        let unwrapped = kms
            .decrypt_dek(&wrapped)
            .await
            .expect("decrypt_dek should succeed");
        assert_eq!(unwrapped, plaintext_dek, "round-trip DEK must byte-equal");

        // KMS returns the canonical ARN even when an alias was passed
        // in. We accept either the canonical ARN form or — as a fallback
        // — the original key id string the caller supplied (for the
        // unlikely case AWS doesn't echo `KeyId`).
        assert!(
            wrapped.key_id.starts_with("arn:aws:kms:") || wrapped.key_id == key_id,
            "wrapped key_id should be canonical ARN or original input: {}",
            wrapped.key_id
        );
    }

    /// v0.8.1 #60: Unwrap of a syntactically valid but bogus ciphertext
    /// must surface a backend / unwrap error rather than silently
    /// returning bytes. The point is to defend against future
    /// refactors that might unwrap `Result::ok()` and zero-fill the DEK
    /// — that would still pass `aws_kms_roundtrip` (because real
    /// ciphertexts decrypt fine) but would let a corrupt DEK through.
    #[cfg(feature = "aws-kms")]
    #[tokio::test]
    #[ignore = "requires AWS credentials (no specific key needed; uses a synthetic bogus ARN)"]
    async fn aws_kms_unwrap_unknown_arn_fails() {
        let kms = super::aws::AwsKms::from_default_env().await;
        let bogus = WrappedDek {
            // Syntactically valid ARN format, all-zero account + key —
            // KMS will reject either NotFound or InvalidCiphertext.
            key_id: "arn:aws:kms:us-east-1:000000000000:key/00000000-0000-0000-0000-000000000000"
                .to_string(),
            ciphertext: vec![0u8; 100],
        };
        let err = kms
            .decrypt_dek(&bogus)
            .await
            .expect_err("decrypt with bogus ciphertext must fail");
        assert!(
            matches!(
                err,
                KmsError::BackendUnavailable { .. } | KmsError::UnwrapFailed { .. }
            ),
            "expected BackendUnavailable or UnwrapFailed, got {err:?}"
        );
    }
}
