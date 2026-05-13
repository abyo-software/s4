//! MFA Delete enforcement (v0.6 #42).
//!
//! AWS S3 MFA Delete: when a bucket is `Versioning = Enabled` AND
//! `MfaDelete = Enabled`, every DELETE / DELETE-version / delete-marker
//! producing request must carry the `x-amz-mfa: <serial> <code>` header,
//! where `code` is a 6-digit RFC 6238 TOTP value computed against the
//! authentication device's secret. Same gate applies to the
//! `PutBucketVersioning` request itself when it tries to flip the MfaDelete
//! state on or off (S3 spec).
//!
//! ## scope (v0.6 #42)
//!
//! - in-memory only (single-instance scope) with optional JSON snapshot for
//!   restart-recoverable state — same shape as `versioning.rs`'s
//!   `--versioning-state-file` and `object_lock.rs`'s
//!   `--object-lock-state-file`.
//! - one shared "default" secret that applies to every bucket whose
//!   `MfaDelete` is `Enabled` and that has no per-bucket override
//!   ([`MfaDeleteManager::set_default_secret`]).
//! - per-bucket override is supported via [`MfaDeleteManager::set_bucket_secret`]
//!   so an operator can isolate bucket families behind separate hardware
//!   tokens.
//! - **not in scope** for v0.6 #42: per-IAM-user secrets, hardware token
//!   serial validation (we only match the serial string the client sends
//!   against the configured one — we do not verify the token model /
//!   device class), FIDO2 / WebAuthn (S3 MFA Delete is TOTP only on AWS
//!   itself).
//!
//! ## semantics
//!
//! - `is_enabled(bucket)` → `false` for buckets that have never been
//!   configured (S3 default — MFA Delete must be opt-in per bucket via
//!   `PutBucketVersioning` with `MfaDelete = Enabled`).
//! - `lookup_secret(bucket)` → per-bucket override if present, else the
//!   default; `None` only when neither has been set (in which case any
//!   `is_enabled(bucket) = true` request is rejected as `Missing` /
//!   `InvalidCode` because there's no secret to verify against).
//! - [`verify_totp`] uses RFC 6238 SHA-1, 6 digits, 30-second step, with
//!   `±1` step skew tolerance (the AWS / Authenticator-app default — a
//!   user typing the code at second 28 still works against the next
//!   step, and a clock-drifted server still validates a freshly-minted
//!   code).

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use totp_rs::{Algorithm, TOTP};

/// One TOTP authentication device's worth of state. The base32 secret is
/// shared between client and server and must be at least 128 bits (16
/// bytes raw → 26 chars un-padded base32, RFC 6238 requirement) — shorter
/// secrets are rejected by [`verify_totp`] when the underlying TOTP
/// constructor refuses them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfaSecret {
    /// Base32-encoded TOTP secret (RFC 4648, un-padded). Length is
    /// provider-defined (typically 16 / 32 chars).
    pub secret_base32: String,
    /// Serial — opaque identifier the client sends in `x-amz-mfa`. Used
    /// only for matching; we do not validate it as a hardware-token serial
    /// (AWS itself doesn't either at the protocol level — the serial is
    /// a free-form string a sysadmin types into IAM).
    pub serial: String,
}

/// Top-level manager. Owns the gateway-wide "default" secret + per-bucket
/// overrides + per-bucket MFA-Delete enabled/disabled state. All public
/// operations go through `RwLock` for thread safety; an `Arc<MfaDeleteManager>`
/// is the expected handle shape (same pattern as `VersioningManager` /
/// `ObjectLockManager`).
#[derive(Debug, Default)]
pub struct MfaDeleteManager {
    /// Default secret applied to every bucket whose MFA Delete is
    /// `Enabled` and that has no per-bucket override.
    default_secret: RwLock<Option<MfaSecret>>,
    /// Per-bucket override.
    by_bucket: RwLock<HashMap<String, MfaSecret>>,
    /// Per-bucket MFA-Delete state (Enabled / Disabled). When the entry
    /// is absent the bucket inherits Disabled (S3 default — MFA Delete
    /// must be opt-in per bucket).
    enabled: RwLock<HashMap<String, bool>>,
}

/// Snapshot wrapper used by [`MfaDeleteManager::to_json`] /
/// [`MfaDeleteManager::from_json`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct MfaSnapshot {
    default_secret: Option<MfaSecret>,
    by_bucket: HashMap<String, MfaSecret>,
    enabled: HashMap<String, bool>,
}

impl MfaDeleteManager {
    /// Empty manager — no default secret, no per-bucket overrides, no
    /// bucket has MFA Delete enabled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the gateway-wide default secret. Buckets with
    /// `is_enabled(bucket) == true` and no per-bucket override use this
    /// secret to verify the client-supplied TOTP code.
    pub fn set_default_secret(&self, secret: MfaSecret) {
        *crate::lock_recovery::recover_write(&self.default_secret, "mfa.default_secret") =
            Some(secret);
    }

    /// Install (or replace) a per-bucket override.
    pub fn set_bucket_secret(&self, bucket: &str, secret: MfaSecret) {
        crate::lock_recovery::recover_write(&self.by_bucket, "mfa.by_bucket")
            .insert(bucket.to_owned(), secret);
    }

    /// Toggle MFA Delete on `bucket`. `true` enables enforcement (every
    /// subsequent DELETE / DELETE-version / delete-marker request needs
    /// `x-amz-mfa`); `false` disables (the bucket falls back to the
    /// regular versioning DELETE flow).
    pub fn set_bucket_state(&self, bucket: &str, enabled: bool) {
        crate::lock_recovery::recover_write(&self.enabled, "mfa.enabled")
            .insert(bucket.to_owned(), enabled);
    }

    /// `true` when `bucket` has explicitly enabled MFA Delete (default
    /// `false` for never-configured buckets, matching S3 spec).
    #[must_use]
    pub fn is_enabled(&self, bucket: &str) -> bool {
        crate::lock_recovery::recover_read(&self.enabled, "mfa.enabled")
            .get(bucket)
            .copied()
            .unwrap_or(false)
    }

    /// Lookup the MFA secret to use when verifying a request against
    /// `bucket`: per-bucket override takes precedence over the default.
    /// Returns `None` when neither has been configured.
    #[must_use]
    pub fn lookup_secret(&self, bucket: &str) -> Option<MfaSecret> {
        if let Some(s) = crate::lock_recovery::recover_read(&self.by_bucket, "mfa.by_bucket")
            .get(bucket)
            .cloned()
        {
            return Some(s);
        }
        crate::lock_recovery::recover_read(&self.default_secret, "mfa.default_secret").clone()
    }

    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let snap = MfaSnapshot {
            default_secret: crate::lock_recovery::recover_read(
                &self.default_secret,
                "mfa.default_secret",
            )
            .clone(),
            by_bucket: crate::lock_recovery::recover_read(&self.by_bucket, "mfa.by_bucket").clone(),
            enabled: crate::lock_recovery::recover_read(&self.enabled, "mfa.enabled").clone(),
        };
        serde_json::to_string(&snap)
    }

    /// Restore from a JSON snapshot produced by [`Self::to_json`].
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: MfaSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            default_secret: RwLock::new(snap.default_secret),
            by_bucket: RwLock::new(snap.by_bucket),
            enabled: RwLock::new(snap.enabled),
        })
    }
}

/// Errors surfaced by [`check_mfa`] / [`parse_mfa_header`].
#[derive(Debug, thiserror::Error)]
pub enum MfaError {
    #[error("missing x-amz-mfa header (MFA Delete is Enabled on this bucket)")]
    Missing,
    #[error("malformed x-amz-mfa header")]
    Malformed,
    #[error("MFA serial does not match configured device")]
    SerialMismatch,
    #[error("invalid MFA code")]
    InvalidCode,
}

/// Parse the `x-amz-mfa` header value, format: `<serial> <code>` where
/// `code` is a 6-digit ASCII numeric string. Whitespace runs of more than
/// one ASCII space between serial and code are rejected; trailing /
/// leading whitespace likewise. AWS itself accepts a single ASCII space
/// here — clients always emit exactly one — so we keep the parser strict
/// to surface caller bugs early.
pub fn parse_mfa_header(value: &str) -> Result<(String, String), MfaError> {
    let mut parts = value.splitn(2, ' ');
    let serial = parts.next().ok_or(MfaError::Malformed)?;
    let code = parts.next().ok_or(MfaError::Malformed)?;
    if serial.is_empty() || code.is_empty() {
        return Err(MfaError::Malformed);
    }
    // No further unsplit chunk allowed.
    if value.split(' ').count() != 2 {
        return Err(MfaError::Malformed);
    }
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(MfaError::Malformed);
    }
    Ok((serial.to_owned(), code.to_owned()))
}

/// Verify a 6-digit TOTP `code` against the base32-encoded `secret_base32`
/// at the wall-clock time `now_unix_secs`. Allows ±1 30-second step for
/// clock skew (RFC 6238 default). Returns `false` when the secret is
/// shorter than RFC 6238's 128-bit minimum, when the base32 fails to
/// decode, or when the code does not match any of the three checked
/// windows.
#[must_use]
pub fn verify_totp(secret_base32: &str, code: &str, now_unix_secs: u64) -> bool {
    let Some(raw) = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_base32)
    else {
        return false;
    };
    let Ok(totp) = TOTP::new(Algorithm::SHA1, 6, 1, 30, raw) else {
        return false;
    };
    totp.check(code, now_unix_secs)
}

/// Convenience: parse + verify in one call. Drives the full
/// `is_enabled(bucket) ⇒ require header ⇒ parse ⇒ serial-match ⇒
/// TOTP-verify` flow against `manager`. Returns `Ok(())` when the
/// bucket has MFA Delete disabled (no-op) OR when every check passes;
/// otherwise the first error encountered.
pub fn check_mfa(
    bucket: &str,
    header_value: Option<&str>,
    manager: &MfaDeleteManager,
    now_unix_secs: u64,
) -> Result<(), MfaError> {
    if !manager.is_enabled(bucket) {
        return Ok(());
    }
    let header = header_value.ok_or(MfaError::Missing)?;
    let (serial, code) = parse_mfa_header(header)?;
    let secret = manager.lookup_secret(bucket).ok_or(MfaError::InvalidCode)?;
    if serial != secret.serial {
        return Err(MfaError::SerialMismatch);
    }
    if !verify_totp(&secret.secret_base32, &code, now_unix_secs) {
        return Err(MfaError::InvalidCode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-byte raw secret encoded as un-padded base32. 26 chars (RFC 4648
    /// without padding) — the minimum length the TOTP constructor will
    /// accept. `JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP` is the standard
    /// "Hello!" test secret padded out by repetition; any 16+ byte raw
    /// string works.
    const TEST_SECRET_B32: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

    fn raw_secret() -> Vec<u8> {
        base32::decode(
            base32::Alphabet::Rfc4648 { padding: false },
            TEST_SECRET_B32,
        )
        .expect("decode test secret")
    }

    fn totp_at(time: u64) -> String {
        let totp = TOTP::new(Algorithm::SHA1, 6, 1, 30, raw_secret()).expect("totp");
        totp.generate(time)
    }

    #[test]
    fn parse_mfa_header_happy_path() {
        let (serial, code) = parse_mfa_header("SERIAL 123456").expect("parse");
        assert_eq!(serial, "SERIAL");
        assert_eq!(code, "123456");
    }

    #[test]
    fn parse_mfa_header_rejects_no_space() {
        let err = parse_mfa_header("SERIAL123456").expect_err("must fail");
        assert!(matches!(err, MfaError::Malformed));
    }

    #[test]
    fn parse_mfa_header_rejects_extra_token() {
        let err = parse_mfa_header("SERIAL 123456 trailing").expect_err("must fail");
        assert!(matches!(err, MfaError::Malformed));
    }

    #[test]
    fn parse_mfa_header_rejects_non_digit_code() {
        let err = parse_mfa_header("SERIAL 12345A").expect_err("must fail");
        assert!(matches!(err, MfaError::Malformed));
    }

    #[test]
    fn parse_mfa_header_rejects_wrong_length_code() {
        for bad in ["SERIAL 12345", "SERIAL 1234567"] {
            let err = parse_mfa_header(bad).expect_err("must fail");
            assert!(matches!(err, MfaError::Malformed));
        }
    }

    #[test]
    fn parse_mfa_header_rejects_empty_serial_or_code() {
        let err = parse_mfa_header(" 123456").expect_err("empty serial");
        assert!(matches!(err, MfaError::Malformed));
        let err = parse_mfa_header("SERIAL ").expect_err("empty code");
        assert!(matches!(err, MfaError::Malformed));
    }

    #[test]
    fn verify_totp_happy_path() {
        let now = 1_700_000_000_u64;
        let code = totp_at(now);
        assert!(verify_totp(TEST_SECRET_B32, &code, now));
    }

    #[test]
    fn verify_totp_clock_skew_within_one_step_ok() {
        // Generate at t-30, verify at t → still within ±1 step skew.
        let now = 1_700_000_000_u64;
        let code_prev = totp_at(now - 30);
        assert!(
            verify_totp(TEST_SECRET_B32, &code_prev, now),
            "previous 30s window must validate"
        );
        let code_next = totp_at(now + 30);
        assert!(
            verify_totp(TEST_SECRET_B32, &code_next, now),
            "next 30s window must validate"
        );
    }

    #[test]
    fn verify_totp_clock_skew_beyond_window_fails() {
        // Generate at t-90 (= 3 steps in the past), verify at t → outside
        // the ±1 skew tolerance.
        let now = 1_700_000_000_u64;
        let code_old = totp_at(now - 90);
        assert!(!verify_totp(TEST_SECRET_B32, &code_old, now));
    }

    #[test]
    fn verify_totp_wrong_code_fails() {
        let now = 1_700_000_000_u64;
        assert!(!verify_totp(TEST_SECRET_B32, "000000", now));
    }

    #[test]
    fn verify_totp_short_secret_rejected() {
        // 8 bytes = below RFC 6238's 128-bit minimum.
        let short_b32 = "JBSWY3DP";
        let now = 1_700_000_000_u64;
        assert!(!verify_totp(short_b32, "000000", now));
    }

    #[test]
    fn check_mfa_disabled_bucket_is_noop() {
        let m = MfaDeleteManager::new();
        // No state set → is_enabled = false → check returns Ok regardless
        // of header.
        assert!(check_mfa("b", None, &m, 0).is_ok());
        assert!(check_mfa("b", Some("garbage"), &m, 0).is_ok());
    }

    #[test]
    fn check_mfa_enabled_correct_code_ok() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "SERIAL-A".to_owned(),
        });
        m.set_bucket_state("b", true);
        let now = 1_700_000_000_u64;
        let code = totp_at(now);
        let header = format!("SERIAL-A {code}");
        assert!(check_mfa("b", Some(&header), &m, now).is_ok());
    }

    #[test]
    fn check_mfa_enabled_wrong_code_fails() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "SERIAL-A".to_owned(),
        });
        m.set_bucket_state("b", true);
        let now = 1_700_000_000_u64;
        let err = check_mfa("b", Some("SERIAL-A 000000"), &m, now).expect_err("must fail");
        assert!(matches!(err, MfaError::InvalidCode), "got {err:?}");
    }

    #[test]
    fn check_mfa_enabled_missing_header_fails() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "SERIAL-A".to_owned(),
        });
        m.set_bucket_state("b", true);
        let err = check_mfa("b", None, &m, 0).expect_err("must fail");
        assert!(matches!(err, MfaError::Missing), "got {err:?}");
    }

    #[test]
    fn check_mfa_enabled_serial_mismatch_fails() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "SERIAL-A".to_owned(),
        });
        m.set_bucket_state("b", true);
        let now = 1_700_000_000_u64;
        let code = totp_at(now);
        let header = format!("SERIAL-OTHER {code}");
        let err = check_mfa("b", Some(&header), &m, now).expect_err("must fail");
        assert!(matches!(err, MfaError::SerialMismatch), "got {err:?}");
    }

    #[test]
    fn check_mfa_per_bucket_override_takes_precedence() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "DEFAULT".to_owned(),
        });
        m.set_bucket_secret(
            "b",
            MfaSecret {
                secret_base32: TEST_SECRET_B32.to_owned(),
                serial: "BUCKET-OVERRIDE".to_owned(),
            },
        );
        m.set_bucket_state("b", true);
        let now = 1_700_000_000_u64;
        let code = totp_at(now);
        // Default serial must NOT validate any more.
        let header_default = format!("DEFAULT {code}");
        assert!(matches!(
            check_mfa("b", Some(&header_default), &m, now).expect_err("must fail"),
            MfaError::SerialMismatch
        ));
        // Bucket-override serial does.
        let header_override = format!("BUCKET-OVERRIDE {code}");
        assert!(check_mfa("b", Some(&header_override), &m, now).is_ok());
    }

    #[test]
    fn snapshot_roundtrip() {
        let m = MfaDeleteManager::new();
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "DEFAULT".to_owned(),
        });
        m.set_bucket_secret(
            "b1",
            MfaSecret {
                secret_base32: TEST_SECRET_B32.to_owned(),
                serial: "B1-OVR".to_owned(),
            },
        );
        m.set_bucket_state("b1", true);
        m.set_bucket_state("b2", false);
        let json = m.to_json().expect("to_json");
        let m2 = MfaDeleteManager::from_json(&json).expect("from_json");
        assert!(m2.is_enabled("b1"));
        assert!(!m2.is_enabled("b2"));
        let s = m2.lookup_secret("b1").expect("override survives");
        assert_eq!(s.serial, "B1-OVR");
        // Bucket without an override falls back to the default.
        let s = m2.lookup_secret("other").expect("default survives");
        assert_eq!(s.serial, "DEFAULT");
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `enabled` write
    /// guard poisons the lock. `to_json` must recover via
    /// [`crate::lock_recovery::recover_read`] and surface the data
    /// instead of re-panicking on the SIGUSR1 dump-back path.
    #[test]
    fn mfa_to_json_after_panic_recovers_via_poison() {
        let m = std::sync::Arc::new(MfaDeleteManager::new());
        m.set_default_secret(MfaSecret {
            secret_base32: TEST_SECRET_B32.to_owned(),
            serial: "DEFAULT".to_owned(),
        });
        m.set_bucket_state("b", true);
        let m_cl = std::sync::Arc::clone(&m);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m_cl.enabled.write().expect("clean lock");
            g.insert("b2".into(), true);
            panic!("force-poison");
        }));
        assert!(
            m.enabled.is_poisoned(),
            "write panic must poison enabled lock"
        );
        let json = m.to_json().expect("to_json after poison must succeed");
        let m2 = MfaDeleteManager::from_json(&json).expect("from_json");
        assert!(m2.is_enabled("b"), "recovered snapshot keeps enabled flag");
        let secret = m2.lookup_secret("b").expect("default secret survives");
        assert_eq!(secret.serial, "DEFAULT");
    }
}
