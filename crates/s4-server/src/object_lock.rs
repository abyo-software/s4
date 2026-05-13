//! Object Lock (WORM) enforcement layer (v0.5 #30).
//!
//! AWS S3 Object Lock holds objects in a "Write Once Read Many" state by
//! attaching a *retention configuration* (mode + retain-until date) and/or a
//! *legal hold* flag to each version. While locked, DELETE / overwrite must
//! be refused with HTTP 403 `AccessDenied`. Two retention modes exist:
//!
//! * **Governance** — a privileged caller can override the lock by sending
//!   `x-amz-bypass-governance-retention: true` (paired in real AWS with the
//!   `s3:BypassGovernanceRetention` IAM permission; in S4 we honour the
//!   header alone because policy gating is the operator's responsibility).
//! * **Compliance** — never overridable until the retain-until date has
//!   passed. Even root/admin cannot delete, including via the bypass header.
//!
//! Legal hold is independent of either mode: while `legal_hold_on == true`
//! the object is locked, regardless of retain-until / mode. Setting it back
//! to `false` is permitted at any time.
//!
//! ## scope (v0.5 #30)
//!
//! - in-memory only (single-instance scope) with optional JSON snapshot for
//!   restart-recoverable state — same shape as `versioning.rs`'s
//!   `--versioning-state-file`.
//! - per-object lock state is keyed by `(bucket, key)` — version-id granular
//!   locking is deferred (current behaviour: a lock on a key blocks DELETE
//!   regardless of version-id; v0.6+ may attach state per (bucket, key,
//!   version-id) to mirror AWS exactly).
//! - per-bucket default config, when set, auto-applies to **new** objects on
//!   PUT (existing key with state already present is left alone).

use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Retention mode for an object lock. Mirrors AWS S3 (`GOVERNANCE` /
/// `COMPLIANCE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockMode {
    /// Override-able with `x-amz-bypass-governance-retention: true`.
    Governance,
    /// Never overridable until `retain_until` expires (immutable: once set,
    /// the mode cannot be downgraded to Governance and `retain_until` cannot
    /// be shortened).
    Compliance,
}

impl LockMode {
    /// Wire format used by the S3 API (`"GOVERNANCE"` / `"COMPLIANCE"`).
    #[must_use]
    pub fn as_aws_str(self) -> &'static str {
        match self {
            Self::Governance => "GOVERNANCE",
            Self::Compliance => "COMPLIANCE",
        }
    }

    /// Parse the AWS wire string back into a [`LockMode`]. Case-insensitive
    /// (AWS accepts both `GOVERNANCE` / `governance`).
    #[must_use]
    pub fn from_aws_str(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("GOVERNANCE") {
            Some(Self::Governance)
        } else if s.eq_ignore_ascii_case("COMPLIANCE") {
            Some(Self::Compliance)
        } else {
            None
        }
    }
}

/// Per-object lock state. All fields are optional so a "legal hold only"
/// state (`mode = None`, `retain_until = None`, `legal_hold_on = true`) is
/// representable, matching S3 semantics where a legal hold can exist without
/// any retention.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLockState {
    pub mode: Option<LockMode>,
    pub retain_until: Option<DateTime<Utc>>,
    pub legal_hold_on: bool,
}

impl ObjectLockState {
    /// `true` when the object is presently locked from delete / overwrite.
    /// Legal hold flips this regardless of the retention clock; otherwise
    /// `mode + retain_until` is what gates.
    #[must_use]
    pub fn is_locked(&self, now: DateTime<Utc>) -> bool {
        if self.legal_hold_on {
            return true;
        }
        match (self.mode, self.retain_until) {
            (Some(_), Some(until)) => until > now,
            _ => false,
        }
    }

    /// `true` when the caller is permitted to DELETE / overwrite the object.
    ///
    /// - Legal hold ON → always denied (cannot be bypassed).
    /// - Compliance + future retain → always denied (cannot be bypassed).
    /// - Governance + future retain + `bypass_governance == true` → allowed.
    /// - Governance + future retain + `bypass_governance == false` → denied.
    /// - No mode, no retain, no legal hold → allowed.
    /// - retain_until in the past → allowed (lock expired).
    #[must_use]
    pub fn can_delete(&self, now: DateTime<Utc>, bypass_governance: bool) -> bool {
        if self.legal_hold_on {
            return false;
        }
        match (self.mode, self.retain_until) {
            (Some(LockMode::Compliance), Some(until)) if until > now => false,
            (Some(LockMode::Governance), Some(until)) if until > now => bypass_governance,
            _ => true,
        }
    }
}

/// Per-bucket default retention. Applied automatically to new objects on PUT
/// (only when no explicit per-object retention was supplied and no state
/// already exists for the (bucket, key)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketObjectLockDefault {
    pub mode: LockMode,
    pub retention_days: u32,
}

/// Snapshot wrapper used by [`ObjectLockManager::to_json`] /
/// [`ObjectLockManager::from_json`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct ObjectLockSnapshot {
    /// `(bucket, key) → state` flattened into a `Vec` so JSON stays
    /// human-readable (tuple keys can't roundtrip through `HashMap` JSON).
    states: Vec<((String, String), ObjectLockState)>,
    bucket_defaults: HashMap<String, BucketObjectLockDefault>,
}

/// Top-level manager. Owns per-(bucket, key) lock state and per-bucket
/// default configuration. All read / write operations go through `RwLock`
/// for thread safety; clones are cheap (`Arc<ObjectLockManager>` is the
/// expected handle shape).
#[derive(Debug, Default)]
pub struct ObjectLockManager {
    states: RwLock<HashMap<(String, String), ObjectLockState>>,
    bucket_defaults: RwLock<HashMap<String, BucketObjectLockDefault>>,
}

impl ObjectLockManager {
    /// Empty manager — no objects locked, no bucket defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace (or create) the lock state for `(bucket, key)`. `service.rs`'s
    /// `put_object_retention` handler calls this directly after validating
    /// the immutability rules (Compliance is one-way; once set, mode cannot
    /// be downgraded and retain-until cannot be shortened — the caller
    /// validates, this method just persists).
    pub fn set(&self, bucket: &str, key: &str, state: ObjectLockState) {
        crate::lock_recovery::recover_write(&self.states, "object_lock.states")
            .insert((bucket.to_owned(), key.to_owned()), state);
    }

    /// Return a clone of the current state for `(bucket, key)`, if any.
    #[must_use]
    pub fn get(&self, bucket: &str, key: &str) -> Option<ObjectLockState> {
        crate::lock_recovery::recover_read(&self.states, "object_lock.states")
            .get(&(bucket.to_owned(), key.to_owned()))
            .cloned()
    }

    /// Toggle the legal-hold flag on `(bucket, key)`. Creates a default-empty
    /// state if no entry exists yet (legal hold is allowed even without
    /// retention).
    pub fn set_legal_hold(&self, bucket: &str, key: &str, on: bool) {
        let mut guard = crate::lock_recovery::recover_write(&self.states, "object_lock.states");
        let entry = guard
            .entry((bucket.to_owned(), key.to_owned()))
            .or_default();
        entry.legal_hold_on = on;
    }

    /// Install (or replace) the bucket-default retention config. New PUTs to
    /// this bucket without explicit retention pick this up via
    /// [`Self::apply_default_on_put`].
    pub fn set_bucket_default(&self, bucket: &str, default: BucketObjectLockDefault) {
        crate::lock_recovery::recover_write(&self.bucket_defaults, "object_lock.bucket_defaults")
            .insert(bucket.to_owned(), default);
    }

    /// Look up the bucket-default retention config, if any.
    #[must_use]
    pub fn bucket_default(&self, bucket: &str) -> Option<BucketObjectLockDefault> {
        crate::lock_recovery::recover_read(&self.bucket_defaults, "object_lock.bucket_defaults")
            .get(bucket)
            .copied()
    }

    /// On PUT: when the bucket has a default config and no per-object state
    /// already exists for this key, materialise a fresh state with
    /// `retain_until = now + retention_days`. Existing state (e.g. an
    /// earlier explicit `put_object_retention`) is left unchanged so we
    /// don't accidentally re-arm a cleared retention on overwrite.
    pub fn apply_default_on_put(&self, bucket: &str, key: &str, now: DateTime<Utc>) {
        let Some(default) = self.bucket_default(bucket) else {
            return;
        };
        let mut guard = crate::lock_recovery::recover_write(&self.states, "object_lock.states");
        let key_pair = (bucket.to_owned(), key.to_owned());
        // Skip if any retention is already in effect — auto-apply must not
        // shorten an existing Compliance lock or wipe a legal hold.
        if let Some(existing) = guard.get(&key_pair)
            && (existing.mode.is_some() || existing.retain_until.is_some())
        {
            return;
        }
        let retain_until = now + Duration::days(i64::from(default.retention_days));
        let entry = guard.entry(key_pair).or_default();
        entry.mode = Some(default.mode);
        entry.retain_until = Some(retain_until);
    }

    /// Drop any lock state attached to `(bucket, key)`. Called by
    /// `service.rs` after a successful (= permitted) physical delete so the
    /// freed key can be re-armed by a future PUT under the bucket default.
    pub fn clear(&self, bucket: &str, key: &str) {
        crate::lock_recovery::recover_write(&self.states, "object_lock.states")
            .remove(&(bucket.to_owned(), key.to_owned()));
    }

    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let states: Vec<((String, String), ObjectLockState)> =
            crate::lock_recovery::recover_read(&self.states, "object_lock.states")
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        let bucket_defaults = crate::lock_recovery::recover_read(
            &self.bucket_defaults,
            "object_lock.bucket_defaults",
        )
        .clone();
        let snap = ObjectLockSnapshot {
            states,
            bucket_defaults,
        };
        serde_json::to_string(&snap)
    }

    /// Restore from a JSON snapshot produced by [`Self::to_json`].
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: ObjectLockSnapshot = serde_json::from_str(s)?;
        let mut states = HashMap::with_capacity(snap.states.len());
        for (k, v) in snap.states {
            states.insert(k, v);
        }
        Ok(Self {
            states: RwLock::new(states),
            bucket_defaults: RwLock::new(snap.bucket_defaults),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn is_locked_future_retain_until() {
        let s = ObjectLockState {
            mode: Some(LockMode::Governance),
            retain_until: Some(now() + Duration::hours(1)),
            legal_hold_on: false,
        };
        assert!(s.is_locked(now()));
    }

    #[test]
    fn is_locked_past_retain_until_is_unlocked() {
        let s = ObjectLockState {
            mode: Some(LockMode::Governance),
            retain_until: Some(now() - Duration::hours(1)),
            legal_hold_on: false,
        };
        assert!(!s.is_locked(now()));
    }

    #[test]
    fn compliance_cannot_be_bypassed() {
        let s = ObjectLockState {
            mode: Some(LockMode::Compliance),
            retain_until: Some(now() + Duration::days(7)),
            legal_hold_on: false,
        };
        // Even with bypass=true, Compliance refuses delete until expiry.
        assert!(!s.can_delete(now(), true));
        assert!(!s.can_delete(now(), false));
    }

    #[test]
    fn governance_can_be_bypassed_with_header() {
        let s = ObjectLockState {
            mode: Some(LockMode::Governance),
            retain_until: Some(now() + Duration::days(7)),
            legal_hold_on: false,
        };
        assert!(
            s.can_delete(now(), true),
            "bypass=true should permit delete"
        );
        assert!(
            !s.can_delete(now(), false),
            "bypass=false should refuse delete"
        );
    }

    #[test]
    fn legal_hold_blocks_delete_independent_of_retention() {
        // No retention at all, just a legal hold → still locked.
        let s = ObjectLockState {
            mode: None,
            retain_until: None,
            legal_hold_on: true,
        };
        assert!(s.is_locked(now()));
        assert!(!s.can_delete(now(), true), "legal hold cannot be bypassed");
        assert!(!s.can_delete(now(), false));
    }

    #[test]
    fn legal_hold_overrides_governance_bypass() {
        // Governance retention with bypass=true would normally permit delete,
        // but a legal hold present at the same time blocks it.
        let s = ObjectLockState {
            mode: Some(LockMode::Governance),
            retain_until: Some(now() + Duration::days(7)),
            legal_hold_on: true,
        };
        assert!(!s.can_delete(now(), true));
    }

    #[test]
    fn no_lock_no_block() {
        let s = ObjectLockState::default();
        assert!(!s.is_locked(now()));
        assert!(s.can_delete(now(), false));
    }

    #[test]
    fn apply_default_materialises_state_on_first_put() {
        let m = ObjectLockManager::new();
        m.set_bucket_default(
            "b",
            BucketObjectLockDefault {
                mode: LockMode::Governance,
                retention_days: 30,
            },
        );
        let now = now();
        m.apply_default_on_put("b", "k", now);
        let state = m.get("b", "k").expect("state must be materialised");
        assert_eq!(state.mode, Some(LockMode::Governance));
        let until = state.retain_until.expect("retain_until must be set");
        let target = now + Duration::days(30);
        // Allow 1s slack for clock granularity.
        let diff = (until - target).num_seconds().abs();
        assert!(diff <= 1, "retain_until off by {diff}s");
    }

    #[test]
    fn apply_default_does_not_overwrite_existing_retention() {
        let m = ObjectLockManager::new();
        let custom_until = now() + Duration::days(365);
        m.set(
            "b",
            "k",
            ObjectLockState {
                mode: Some(LockMode::Compliance),
                retain_until: Some(custom_until),
                legal_hold_on: false,
            },
        );
        m.set_bucket_default(
            "b",
            BucketObjectLockDefault {
                mode: LockMode::Governance,
                retention_days: 1,
            },
        );
        m.apply_default_on_put("b", "k", now());
        let state = m.get("b", "k").unwrap();
        // Existing Compliance + 365-day retain must be preserved.
        assert_eq!(state.mode, Some(LockMode::Compliance));
        assert_eq!(state.retain_until, Some(custom_until));
    }

    #[test]
    fn apply_default_no_op_without_bucket_default() {
        let m = ObjectLockManager::new();
        m.apply_default_on_put("b", "k", now());
        assert!(m.get("b", "k").is_none());
    }

    #[test]
    fn set_legal_hold_creates_state_when_missing() {
        let m = ObjectLockManager::new();
        m.set_legal_hold("b", "k", true);
        let s = m.get("b", "k").expect("state created");
        assert!(s.legal_hold_on);
        assert!(s.mode.is_none());
        assert!(s.retain_until.is_none());
        m.set_legal_hold("b", "k", false);
        let s2 = m.get("b", "k").unwrap();
        assert!(!s2.legal_hold_on);
    }

    #[test]
    fn snapshot_roundtrip() {
        let m = ObjectLockManager::new();
        m.set(
            "b1",
            "k1",
            ObjectLockState {
                mode: Some(LockMode::Compliance),
                retain_until: Some(Utc::now() + Duration::days(10)),
                legal_hold_on: true,
            },
        );
        m.set_bucket_default(
            "b1",
            BucketObjectLockDefault {
                mode: LockMode::Governance,
                retention_days: 7,
            },
        );
        let json = m.to_json().expect("to_json");
        let m2 = ObjectLockManager::from_json(&json).expect("from_json");
        let s = m2.get("b1", "k1").expect("state survives roundtrip");
        assert_eq!(s.mode, Some(LockMode::Compliance));
        assert!(s.legal_hold_on);
        let d = m2.bucket_default("b1").expect("default survives roundtrip");
        assert_eq!(d.mode, LockMode::Governance);
        assert_eq!(d.retention_days, 7);
    }

    #[test]
    fn lock_mode_aws_string_roundtrip() {
        assert_eq!(
            LockMode::from_aws_str(LockMode::Governance.as_aws_str()),
            Some(LockMode::Governance)
        );
        assert_eq!(
            LockMode::from_aws_str(LockMode::Compliance.as_aws_str()),
            Some(LockMode::Compliance)
        );
        assert_eq!(
            LockMode::from_aws_str("governance"),
            Some(LockMode::Governance)
        );
        assert!(LockMode::from_aws_str("nope").is_none());
    }

    #[test]
    fn clear_removes_state() {
        let m = ObjectLockManager::new();
        m.set(
            "b",
            "k",
            ObjectLockState {
                mode: Some(LockMode::Governance),
                retain_until: Some(Utc::now() + Duration::days(1)),
                legal_hold_on: false,
            },
        );
        assert!(m.get("b", "k").is_some());
        m.clear("b", "k");
        assert!(m.get("b", "k").is_none());
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `states` write guard
    /// poisons the lock. `to_json` must recover via
    /// [`crate::lock_recovery::recover_read`] and surface the data
    /// instead of re-panicking.
    #[test]
    fn object_lock_to_json_after_panic_recovers_via_poison() {
        let m = ObjectLockManager::new();
        m.set(
            "b",
            "k",
            ObjectLockState {
                mode: Some(LockMode::Compliance),
                retain_until: Some(Utc::now() + Duration::days(7)),
                legal_hold_on: false,
            },
        );
        let m = std::sync::Arc::new(m);
        let m_cl = std::sync::Arc::clone(&m);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m_cl.states.write().expect("clean lock");
            g.entry(("b".into(), "k2".into())).or_default();
            panic!("force-poison");
        }));
        assert!(
            m.states.is_poisoned(),
            "write panic must poison states lock"
        );
        let json = m.to_json().expect("to_json after poison must succeed");
        let m2 = ObjectLockManager::from_json(&json).expect("from_json");
        assert!(
            m2.get("b", "k").is_some(),
            "recovered snapshot keeps original entry"
        );
    }
}
