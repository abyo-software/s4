//! S3 Lifecycle execution — per-bucket rule evaluation + manager skeleton
//! (v0.6 #37).
//!
//! AWS S3 Lifecycle attaches a **list of rules** to a bucket; each rule may
//! request that S3
//!
//! 1. **Expire** an object once its age (or the calendar date) crosses a
//!    threshold (`Expiration { Days | Date }`),
//! 2. **Transition** an object to a different storage class (`Transition
//!    { Days, StorageClass }` — `STANDARD_IA`, `GLACIER_IR`, ...),
//! 3. **Expire noncurrent versions** in a versioning-enabled bucket
//!    (`NoncurrentVersionExpiration { NoncurrentDays }`).
//!
//! Until v0.6 #37 the matching `PutBucketLifecycleConfiguration` /
//! `GetBucketLifecycleConfiguration` / `DeleteBucketLifecycle` handlers
//! in `crates/s4-server/src/service.rs` were pure passthroughs (the s3s
//! framework's default backend stored them but nothing read the rules).
//! This module owns the in-memory configuration store + the rule
//! evaluator that decides, for any single object, whether an action
//! should fire **right now**.
//!
//! ## responsibilities (v0.6 #37)
//!
//! - in-memory `bucket -> LifecycleConfig` map with JSON snapshot
//!   round-trip (mirroring `versioning.rs` / `object_lock.rs` /
//!   `inventory.rs`'s shape so `--lifecycle-state-file` is a one-line
//!   addition in `main.rs`).
//! - per-bucket action counters (`actions_total`) — bumped by the
//!   future scanner when an Expiration / Transition /
//!   NoncurrentExpiration action is taken, surfaced via Prometheus
//!   (`s4_lifecycle_actions_total`, see `metrics.rs`).
//! - [`LifecycleManager::evaluate`] — given one (bucket, key, age,
//!   size, tags) tuple, walk the bucket's rules in declaration order
//!   and return the first matching action. Returns `None` when no
//!   rule matches (or when the matching rule is `Disabled`).
//! - [`evaluate_batch`] — batched form for the test path: walks a
//!   slice of `(key, age, size, tags)` tuples and returns the (key,
//!   action) pairs that should fire. The actual backend invocation
//!   (S3.delete_object / metadata rewrite) is the caller's job.
//!
//! ## scope limitations (v0.6 #37)
//!
//! - **Background scanner is a skeleton only.** `main.rs`'s
//!   `--lifecycle-scan-interval-hours` flag spawns a tokio task that
//!   logs the bucket list and stamps a "would-have-run" marker;
//!   walking the source bucket via `list_objects_v2` and actually
//!   invoking `delete_object` / metadata rewrite for each evaluated
//!   action is deferred to v0.7+. Wiring the scheduler to walk a real
//!   bucket end-to-end requires a back-reference from the scheduler
//!   into `S4Service` for the `list_objects_v2` walk and that
//!   reshuffle is out of scope for this issue. The
//!   [`crate::S4Service::run_lifecycle_once_for_test`] entry covers
//!   the in-memory equivalent so the unit + E2E tests exercise the
//!   evaluator end-to-end.
//! - **`AbortIncompleteMultipartUpload`** is parsed and stored on the
//!   `LifecycleRule` (so PutBucketLifecycleConfiguration round-trips
//!   the field) but not enforced — multipart abort sweeping is a
//!   separate scanner that lives next to the multipart upload manager
//!   (v0.7+).
//! - **`expiration_date` (calendar date)** is supported in the
//!   evaluator: a rule with `expiration_date` past `now` fires
//!   Expiration immediately. Same wire form as AWS S3.
//! - **Multi-instance replication.** All state is single-instance
//!   in-memory; `--lifecycle-state-file <PATH>` provides restart
//!   recovery via JSON snapshot, matching the
//!   `--versioning-state-file` shape.
//! - **Object Lock interplay**: the evaluator does NOT consult the
//!   `ObjectLockManager` directly (the evaluator API is
//!   object-tags-and-size only); the scanner caller is expected to
//!   skip locked objects — see the `evaluate_batch_skips_locked` test
//!   for the canonical pattern. Locking always wins over Lifecycle.
//! - **Versioning interplay**: the evaluator treats noncurrent
//!   versions as a separate input — pass `is_noncurrent = true` to
//!   [`LifecycleManager::evaluate_with_flags`] for noncurrent version
//!   expiration matching. The legacy `evaluate` shorthand defaults
//!   `is_noncurrent = false` (current version) so existing call sites
//!   stay one-liners.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use s3s::S3;
use s3s::S3Request;
use s3s::dto::*;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Whether a rule is currently being applied. Mirrors AWS S3
/// `ExpirationStatus` (`"Enabled"` / `"Disabled"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleStatus {
    Enabled,
    Disabled,
}

impl LifecycleStatus {
    /// Wire form used by AWS S3 (`"Enabled"` / `"Disabled"`).
    #[must_use]
    pub fn as_aws_str(self) -> &'static str {
        match self {
            Self::Enabled => "Enabled",
            Self::Disabled => "Disabled",
        }
    }

    /// Parse the AWS wire form (case-insensitive). Falls back to `Disabled`
    /// on unrecognised input — this matches AWS conservative behaviour
    /// (an unparseable status is treated as "off" so a typo doesn't silently
    /// expire data).
    #[must_use]
    pub fn from_aws_str(s: &str) -> Self {
        if s.eq_ignore_ascii_case("Enabled") {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

/// Per-rule object filter. AWS S3 represents the filter as one of `Prefix`,
/// `Tag`, `ObjectSizeGreaterThan`, `ObjectSizeLessThan`, or `And` (= AND of
/// any subset of those predicates). For internal storage we flatten the
/// "And" form into a struct of optional fields plus a vector of (key, value)
/// tags — every present field must match (logical AND). An empty filter (all
/// fields `None` / empty `tags`) matches every object in the bucket.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleFilter {
    /// Object key prefix (empty / `None` = no prefix gating).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Logical AND across every entry: every (key, value) must match the
    /// object's own tag set.
    #[serde(default)]
    pub tags: Vec<(String, String)>,
    /// Object must be *strictly greater than* this size in bytes.
    #[serde(default)]
    pub object_size_greater_than: Option<u64>,
    /// Object must be *strictly less than* this size in bytes.
    #[serde(default)]
    pub object_size_less_than: Option<u64>,
}

impl LifecycleFilter {
    /// `true` when this filter accepts the candidate. Empty filter accepts
    /// every object. Tag matching is AND of all listed tags (each present in
    /// `object_tags` with the matching value).
    #[must_use]
    pub fn matches(&self, key: &str, size: u64, object_tags: &[(String, String)]) -> bool {
        if let Some(p) = &self.prefix
            && !key.starts_with(p)
        {
            return false;
        }
        if let Some(min) = self.object_size_greater_than
            && size <= min
        {
            return false;
        }
        if let Some(max) = self.object_size_less_than
            && size >= max
        {
            return false;
        }
        for (tk, tv) in &self.tags {
            let matched = object_tags.iter().any(|(ok, ov)| ok == tk && ov == tv);
            if !matched {
                return false;
            }
        }
        true
    }
}

/// A single transition step (object age threshold + target storage class).
/// `days` is days since the object was created. AWS S3 also accepts `Date`
/// for transitions but Lifecycle deployments overwhelmingly use `Days`; the
/// `Date` form is omitted here on purpose to keep the evaluator narrow
/// (operators wanting calendar transitions can synthesise a one-shot rule
/// at the cadence of their scanner).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRule {
    pub days: u32,
    /// Target storage class (`"STANDARD_IA"` / `"GLACIER_IR"` /
    /// `"GLACIER"` / `"DEEP_ARCHIVE"` / `"INTELLIGENT_TIERING"` /
    /// `"ONEZONE_IA"`). Stored as the AWS wire string so PutBucket /
    /// GetBucket round-trip is a no-op.
    pub storage_class: String,
}

/// One lifecycle rule. AWS S3's `LifecycleRule` flattened into the subset
/// the v0.6 #37 evaluator handles. `id` is the operator-supplied label and
/// makes Get / Put round-trips non-lossy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRule {
    pub id: String,
    pub status: LifecycleStatus,
    #[serde(default)]
    pub filter: LifecycleFilter,
    /// Days since the object was created. Mutually exclusive with
    /// [`Self::expiration_date`] in AWS — both fields are accepted here on
    /// input (the evaluator picks `expiration_days` first, then
    /// `expiration_date`) so a malformed rule with both set still evaluates
    /// deterministically rather than silently dropping the action.
    #[serde(default)]
    pub expiration_days: Option<u32>,
    /// Calendar date past which matching objects are expired (AWS wire form
    /// is ISO 8601; here we keep it as a `DateTime<Utc>` so round-trips
    /// through `serde_json` survive without re-parsing).
    #[serde(default)]
    pub expiration_date: Option<DateTime<Utc>>,
    /// Transition steps in declaration order. The evaluator picks the
    /// deepest transition (largest `days` ≤ object age) and resolves any
    /// conflict with expiration in [`LifecycleManager::evaluate_with_flags`].
    #[serde(default)]
    pub transitions: Vec<TransitionRule>,
    /// Days an object has been noncurrent before the noncurrent-version
    /// expiration fires. Only consulted when the evaluator is asked about
    /// a noncurrent object (`is_noncurrent = true`).
    #[serde(default)]
    pub noncurrent_version_expiration_days: Option<u32>,
    /// Days after a multipart upload is initiated before the abort fires.
    /// Stored so PutBucket round-trips, but **not enforced** in the
    /// v0.6 #37 evaluator — multipart sweeping lives elsewhere.
    #[serde(default)]
    pub abort_incomplete_multipart_upload_days: Option<u32>,
}

impl LifecycleRule {
    /// Convenience constructor for a "expire after N days" rule. Useful in
    /// tests + operator scripts.
    #[must_use]
    pub fn expire_after_days(id: impl Into<String>, days: u32) -> Self {
        Self {
            id: id.into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: Some(days),
            expiration_date: None,
            transitions: Vec::new(),
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: None,
        }
    }
}

/// Per-bucket lifecycle configuration (ordered list of rules — first match
/// wins, matching AWS S3 semantics).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleConfig {
    pub rules: Vec<LifecycleRule>,
}

/// The action a single rule wants to take **right now** for a candidate
/// object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Delete the object (`Expiration` / `NoncurrentVersionExpiration`).
    Expire,
    /// Move the object to a different storage class (`Transition`). The
    /// inner string is the AWS wire form (e.g. `"GLACIER_IR"`).
    Transition { storage_class: String },
    /// v0.8.3 #69 (audit M-2): abort an in-flight multipart upload that
    /// has been initiated longer ago than the rule's
    /// `abort_incomplete_multipart_upload_days`. The inner string is the
    /// backend-issued `upload_id` (so the scanner can route the
    /// `AbortMultipartUpload` call without re-listing). Same wire
    /// semantic as AWS S3 `AbortIncompleteMultipartUpload`.
    AbortMultipartUpload { upload_id: String },
}

impl LifecycleAction {
    /// Stable label suitable for a metric counter
    /// (`s4_lifecycle_actions_total{action="..."}`).
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::Expire => "expire",
            Self::Transition { .. } => "transition",
            Self::AbortMultipartUpload { .. } => "abort_incomplete_multipart",
        }
    }
}

/// v0.8.3 #69: one in-flight multipart upload the lifecycle scanner
/// considers for abort. Mirrors the (subset of) `MultipartUpload` fields
/// the rule evaluator needs (key, upload_id, initiated). `tags` is kept
/// in the shape the existing object-path evaluator uses
/// (`Vec<(String, String)>`) so a future enhancement that surfaces
/// upload-time tags from `MultipartStateStore` can flow through the
/// same filter check without API churn — AWS S3 itself does not attach
/// tags to in-flight multipart uploads, so for the scanner-driven path
/// the slice is always empty (the filter's prefix / size predicates
/// still apply via [`LifecycleFilter::matches`], passing size = 0).
#[derive(Clone, Debug)]
pub struct MultipartUploadCandidate {
    pub upload_id: String,
    pub key: String,
    pub initiated: DateTime<Utc>,
    pub tags: Vec<(String, String)>,
}

/// snapshot のシリアライズ format。`to_json` / `from_json` 用。
#[derive(Debug, Default, Serialize, Deserialize)]
struct LifecycleSnapshot {
    by_bucket: HashMap<String, LifecycleConfig>,
}

/// Per-bucket lifecycle configuration manager.
///
/// All read / write operations go through `RwLock` for thread safety;
/// clones are cheap (`Arc<LifecycleManager>` is the expected handle shape).
/// `actions_total` is a parallel `RwLock<HashMap<...>>` of `(bucket,
/// action_label) -> count` so the future background scanner can stamp
/// successful actions and operators can `GET /metrics` to see the running
/// totals (the metric is also surfaced via `metrics::counter!` — see
/// [`crate::metrics::record_lifecycle_action`]).
#[derive(Debug, Default)]
pub struct LifecycleManager {
    by_bucket: RwLock<HashMap<String, LifecycleConfig>>,
    /// `(bucket, action_label) -> count`. Bumped by the scanner via
    /// [`Self::record_action`]. Action labels are the
    /// [`LifecycleAction::metric_label`] values
    /// (`"expire"` / `"transition"`).
    actions_total: RwLock<HashMap<(String, String), u64>>,
}

impl LifecycleManager {
    /// Empty manager — no bucket has rules.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace (or create) the lifecycle configuration for `bucket`. Drops
    /// any previously-attached rules in one shot — matches AWS S3
    /// `PutBucketLifecycleConfiguration` (full replace, no merge).
    pub fn put(&self, bucket: &str, config: LifecycleConfig) {
        crate::lock_recovery::recover_write(&self.by_bucket, "lifecycle.by_bucket")
            .insert(bucket.to_owned(), config);
    }

    /// Return a clone of the bucket's configuration, if any.
    #[must_use]
    pub fn get(&self, bucket: &str) -> Option<LifecycleConfig> {
        crate::lock_recovery::recover_read(&self.by_bucket, "lifecycle.by_bucket")
            .get(bucket)
            .cloned()
    }

    /// Drop the bucket's lifecycle configuration (idempotent — missing
    /// bucket is OK).
    pub fn delete(&self, bucket: &str) {
        crate::lock_recovery::recover_write(&self.by_bucket, "lifecycle.by_bucket").remove(bucket);
    }

    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let by_bucket =
            crate::lock_recovery::recover_read(&self.by_bucket, "lifecycle.by_bucket").clone();
        let snap = LifecycleSnapshot { by_bucket };
        serde_json::to_string(&snap)
    }

    /// Restore from a JSON snapshot produced by [`Self::to_json`]. Action
    /// counters are intentionally not snapshotted — they're transient
    /// observability data and should reset across process restarts so
    /// `rate(s4_lifecycle_actions_total[1h])` doesn't double-count.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: LifecycleSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            by_bucket: RwLock::new(snap.by_bucket),
            actions_total: RwLock::new(HashMap::new()),
        })
    }

    /// Evaluate which rule (if any) applies to a single **current-version**
    /// object right now. Walks the bucket's rules in declaration order;
    /// returns the first matching action. Returns `None` when no rule
    /// matches (or when the matching rule is `Disabled`, or when the
    /// bucket has no lifecycle configuration).
    ///
    /// Within a single rule the precedence is:
    ///
    /// 1. Pick the deepest transition whose `days` threshold is currently
    ///    met (= largest `days ≤ object age`).
    /// 2. Conflict with expiration: if `expiration_days <=
    ///    transition_days` for the chosen transition, expiration wins
    ///    (the rule wants the object gone before it would have been
    ///    transitioned). Otherwise transition wins (e.g. transition at
    ///    30d, expiration at 365d, age 60d → transition fires now,
    ///    expiration is future).
    /// 3. `expiration_date` matches when `now >= expiration_date` and no
    ///    transition is currently applicable.
    ///
    /// `object_age` is "now - created_at" supplied by the caller — keeping
    /// the evaluator pure of the wall clock makes deterministic testing
    /// trivial.
    #[must_use]
    pub fn evaluate(
        &self,
        bucket: &str,
        key: &str,
        object_age: Duration,
        object_size: u64,
        object_tags: &[(String, String)],
    ) -> Option<LifecycleAction> {
        self.evaluate_with_flags(
            bucket,
            key,
            object_age,
            object_size,
            object_tags,
            EvaluateFlags::default(),
        )
    }

    /// Full-form evaluator with flags for noncurrent-version handling.
    /// Use this when the scanner is walking a versioning-enabled bucket;
    /// pass `is_noncurrent = true` for entries that are not the latest
    /// non-delete-marker version.
    #[must_use]
    pub fn evaluate_with_flags(
        &self,
        bucket: &str,
        key: &str,
        object_age: Duration,
        object_size: u64,
        object_tags: &[(String, String)],
        flags: EvaluateFlags,
    ) -> Option<LifecycleAction> {
        let cfg = self.get(bucket)?;
        let now_for_date = flags.now.unwrap_or_else(Utc::now);
        let age_days = object_age.num_days().max(0);
        let age_days_u32 = u32::try_from(age_days).unwrap_or(u32::MAX);
        for rule in &cfg.rules {
            if rule.status != LifecycleStatus::Enabled {
                continue;
            }
            if !rule.filter.matches(key, object_size, object_tags) {
                continue;
            }
            // Noncurrent-version expiration: only consulted when the
            // caller explicitly flags this entry as noncurrent. The
            // current-version expiration / transition rules do not fire
            // for noncurrent versions in AWS S3 semantics.
            if flags.is_noncurrent {
                if let Some(days) = rule.noncurrent_version_expiration_days
                    && age_days_u32 >= days
                {
                    return Some(LifecycleAction::Expire);
                }
                continue;
            }
            // Current-version path.
            let exp_days_match = rule.expiration_days.filter(|d| age_days_u32 >= *d);
            let exp_date_match = rule.expiration_date.filter(|d| now_for_date >= *d);
            // Pick the deepest transition whose threshold is at or
            // below the object's age. Transitions are typically
            // declaration-ordered by ascending `days`, but we don't
            // require it — taking the largest threshold means an
            // object aged 90d gets `GLACIER` over `STANDARD_IA` even
            // if `STANDARD_IA(30d)` was declared first.
            let chosen_transition = rule
                .transitions
                .iter()
                .filter(|t| age_days_u32 >= t.days)
                .max_by_key(|t| t.days);
            // Conflict resolution: when `expiration_days` fires AND a
            // transition fires, expiration wins iff
            // `expiration_days <= transition_days` (rule wants object
            // gone before / at the same time it would have been
            // transitioned). Otherwise the transition wins.
            if let Some(exp_threshold) = exp_days_match {
                let trans_threshold = chosen_transition.map(|t| t.days).unwrap_or(u32::MAX);
                if exp_threshold <= trans_threshold {
                    return Some(LifecycleAction::Expire);
                }
            }
            if let Some(t) = chosen_transition {
                return Some(LifecycleAction::Transition {
                    storage_class: t.storage_class.clone(),
                });
            }
            // Calendar-date expiration (no transition currently
            // applicable, but the rule's expiration_date is past).
            if exp_date_match.is_some() {
                return Some(LifecycleAction::Expire);
            }
            // Fall through to the next rule when no action fires for
            // this rule — first-match-wins applies only to *firing*
            // rules, matching AWS semantics where overlapping rules
            // with disjoint thresholds compose.
        }
        None
    }

    /// v0.8.3 #69 (audit M-2): evaluate one in-flight multipart upload
    /// against the bucket's rules. Returns
    /// [`LifecycleAction::AbortMultipartUpload`] when at least one
    /// `Enabled` rule (a) accepts the upload's key via its filter and
    /// (b) carries an `abort_incomplete_multipart_upload_days`
    /// threshold whose age (`now - initiated`) is currently met.
    /// Returns `None` otherwise (no matching rule, no
    /// abort-multipart-upload-days set, or the upload is too young).
    ///
    /// Filter matching reuses [`LifecycleFilter::matches`] with
    /// `object_size = 0` — in-flight uploads have no assembled size
    /// yet (the parts are stored independently in the backend), so
    /// any rule whose filter sets `object_size_greater_than` /
    /// `object_size_less_than` is treated as if the upload were
    /// 0 bytes. AWS S3 itself does not gate
    /// `AbortIncompleteMultipartUpload` on size; this matches the
    /// AWS semantic (size predicates simply do not apply to the
    /// abort path) for the typical filter shape (no size predicate).
    /// Operators wanting size-gated abort can carry the upload's
    /// declared part length on the `MultipartUploadCandidate` in a
    /// follow-up issue — the API extension is additive.
    #[must_use]
    pub fn evaluate_in_flight_multipart(
        &self,
        bucket: &str,
        upload: &MultipartUploadCandidate,
        now: DateTime<Utc>,
    ) -> Option<LifecycleAction> {
        let cfg = self.get(bucket)?;
        for rule in &cfg.rules {
            if rule.status != LifecycleStatus::Enabled {
                continue;
            }
            if !rule.filter.matches(&upload.key, 0, &upload.tags) {
                continue;
            }
            if let Some(days) = rule.abort_incomplete_multipart_upload_days {
                let age = now.signed_duration_since(upload.initiated);
                if age >= Duration::days(i64::from(days)) {
                    return Some(LifecycleAction::AbortMultipartUpload {
                        upload_id: upload.upload_id.clone(),
                    });
                }
            }
        }
        None
    }

    /// Stamp the per-bucket action counter and bump the matching
    /// Prometheus counter. Called by the future scanner after a successful
    /// delete / metadata rewrite.
    pub fn record_action(&self, bucket: &str, action: &LifecycleAction) {
        let label = action.metric_label();
        let key = (bucket.to_owned(), label.to_owned());
        let mut guard =
            crate::lock_recovery::recover_write(&self.actions_total, "lifecycle.actions_total");
        let entry = guard.entry(key).or_insert(0);
        *entry = entry.saturating_add(1);
        crate::metrics::record_lifecycle_action(bucket, label);
    }

    /// Read-only snapshot of the per-(bucket, action) counter map.
    /// Useful for tests + introspection (`/admin/lifecycle/stats` style
    /// endpoints in the future).
    #[must_use]
    pub fn actions_snapshot(&self) -> HashMap<(String, String), u64> {
        crate::lock_recovery::recover_read(&self.actions_total, "lifecycle.actions_total").clone()
    }

    /// All buckets with a lifecycle configuration attached. Sorted for
    /// stable scanner ordering.
    #[must_use]
    pub fn buckets(&self) -> Vec<String> {
        let map = crate::lock_recovery::recover_read(&self.by_bucket, "lifecycle.by_bucket");
        let mut out: Vec<String> = map.keys().cloned().collect();
        out.sort();
        out
    }
}

/// Flags for [`LifecycleManager::evaluate_with_flags`]. Default is
/// "current-version object, evaluator picks `Utc::now()` for the date
/// comparison". Tests override `now` for determinism.
#[derive(Clone, Copy, Debug, Default)]
pub struct EvaluateFlags {
    pub is_noncurrent: bool,
    pub now: Option<DateTime<Utc>>,
}

/// One object the evaluator considers in a batch:
/// `(key, object_age, object_size, object_tags)`. Defined as a type alias
/// so [`evaluate_batch`] / [`crate::S4Service::run_lifecycle_once_for_test`]
/// don't trip clippy's `type-complexity` lint, and so callers building the
/// list have a single canonical shape to reach for.
pub type EvaluateBatchEntry = (String, Duration, u64, Vec<(String, String)>);

/// Test-driven scan entry: walks a list of [`EvaluateBatchEntry`] tuples
/// and produces (key, action) pairs for every object that should fire an
/// action **right now**. The actual backend invocation (S3.delete_object /
/// metadata rewrite) is the caller's job. Used by both unit tests and the
/// E2E test in `tests/roundtrip.rs`; the future background scanner will
/// reuse the same entry once the bucket-walk is wired through the backend.
#[must_use]
pub fn evaluate_batch(
    manager: &LifecycleManager,
    bucket: &str,
    objects: &[EvaluateBatchEntry],
) -> Vec<(String, LifecycleAction)> {
    let mut out = Vec::with_capacity(objects.len());
    for (key, age, size, tags) in objects {
        if let Some(action) = manager.evaluate(bucket, key, *age, *size, tags) {
            out.push((key.clone(), action));
        }
    }
    out
}

/// Per-invocation scanner counters returned by [`run_scan_once`]. Useful
/// for tests, the `--lifecycle-scan-interval-hours` log line, and any
/// future `/admin/lifecycle/scan` introspection endpoint. Operators see
/// the same numbers via Prometheus
/// (`s4_lifecycle_actions_total{action="expire"|"transition"}`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Number of buckets the scanner walked (= buckets with a lifecycle
    /// configuration attached at the moment the scanner ran).
    pub buckets_scanned: usize,
    /// Number of distinct keys the scanner evaluated. Multi-page lists
    /// count one key once even if the listing was paginated.
    pub objects_evaluated: usize,
    /// Number of objects deleted as a result of an Expiration action.
    pub expired: usize,
    /// Number of objects whose `x-amz-storage-class` was rewritten as a
    /// result of a Transition action.
    pub transitioned: usize,
    /// Number of objects skipped because an Object Lock (Compliance,
    /// Governance, or legal hold) was in effect. The Lock always wins
    /// over Lifecycle, matching AWS S3 semantics.
    pub skipped_locked: usize,
    /// v0.8.3 #69 (audit M-2): number of in-flight multipart uploads
    /// the scanner aborted as a result of an
    /// `AbortIncompleteMultipartUpload` action. Pair with the
    /// Prometheus counter
    /// `s4_lifecycle_actions_total{action="abort_incomplete_multipart"}`.
    /// Only counts successful aborts — a backend
    /// `abort_multipart_upload` failure bumps `action_errors` instead
    /// (matching the existing Expire / Transition error-path).
    pub aborted_multipart: usize,
    /// Number of objects the evaluator wanted to act on but the action
    /// failed (e.g. backend `delete_object` returned an error). Logged
    /// individually at WARN level; this counter exists so tests / metrics
    /// can assert no silent loss.
    pub action_errors: usize,
}

/// Convert an s3s `Timestamp` (`time::OffsetDateTime` underneath) into a
/// `chrono::DateTime<Utc>` via the RFC3339 wire form. Used by the scanner
/// to compute object age (= `now - last_modified`). Returns `None` when
/// the stamp is unparseable, in which case the caller falls back to
/// treating the object as freshly created (age = 0).
fn timestamp_to_chrono_utc(ts: &Timestamp) -> Option<DateTime<Utc>> {
    let mut buf = Vec::new();
    ts.format(s3s::dto::TimestampFormat::DateTime, &mut buf)
        .ok()?;
    let s = std::str::from_utf8(&buf).ok()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Build a synthetic `S3Request` with the minimum metadata the
/// scanner-internal calls need. The lifecycle scanner is a
/// system-internal caller (no end-user credentials, no real HTTP method
/// / URI), so policy gates downstream see `credentials = None` /
/// `region = None` and treat the call as anonymous-internal. Backends
/// that do not gate internal traffic ignore these fields entirely.
fn synthetic_request<T>(input: T, method: http::Method, uri_path: &str) -> S3Request<T> {
    S3Request {
        input,
        method,
        uri: uri_path.parse().unwrap_or_else(|_| "/".parse().expect("/")),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

/// Walk every bucket that has a lifecycle configuration attached, list
/// its objects via `list_objects_v2` (continuation-token pagination), and
/// for each object evaluate the rule set + execute the matching
/// Expiration / Transition action. Object-Lock-protected objects are
/// **skipped** (the Lock always wins over Lifecycle). Versioning chains
/// are intentionally out of scope for v0.7 #45 — see the module-level
/// limitations note.
///
/// ## error handling
///
/// Per-bucket / per-object failures are logged at WARN level and bumped
/// in `ScanReport::action_errors`; the scanner does NOT abort early on a
/// single bad object so one slow / faulty bucket can't starve every
/// other bucket's lifecycle. The function only returns `Err(_)` when the
/// scanner cannot make progress at all (no current usage — kept for the
/// future case where the manager itself becomes unavailable).
///
/// ## scope (v0.7 #45)
///
/// - Current-version objects only (Versioning-enabled chains rely on
///   `evaluate_with_flags(is_noncurrent = true)`, but walking the
///   shadow keys requires the version chain access pattern from
///   `versioning.rs` and is deferred to a follow-up issue).
/// - `head_object`'s `last_modified` is used to compute age. When the
///   backend omits the field (some S3-compatible backends do), the
///   object is treated as age 0 and skipped — matches AWS conservative
///   behaviour where a malformed timestamp must not silently expire data.
/// - Tags are looked up via the attached
///   [`crate::tagging::TagManager`] (when wired). Buckets without a
///   tag manager pass an empty tag list to the evaluator.
/// - Transition rewrites the object's `x-amz-storage-class` via
///   `copy_object` (same bucket / same key, `MetadataDirective: COPY`,
///   new `StorageClass`). Backends that ignore the storage class
///   header silently no-op the transition; the counter still bumps to
///   reflect "the scanner asked for a transition" (matching AWS where
///   a no-op transition still costs a request).
pub async fn run_scan_once<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
) -> Result<ScanReport, String> {
    let Some(mgr) = s4.lifecycle_manager().cloned() else {
        // No lifecycle manager attached (e.g. operator did not set
        // `--lifecycle-state-file`). Scan is a no-op.
        return Ok(ScanReport::default());
    };
    let buckets = mgr.buckets();
    if buckets.is_empty() {
        return Ok(ScanReport::default());
    }
    let now = Utc::now();
    let mut report = ScanReport {
        buckets_scanned: buckets.len(),
        ..ScanReport::default()
    };
    for bucket in buckets {
        scan_bucket(s4, &mgr, &bucket, now, &mut report).await;
        // v0.8.3 #69 (audit M-2): walk in-flight multipart uploads for
        // the same bucket and abort any whose `Initiated` time is past
        // the rule's `abort_incomplete_multipart_upload_days` threshold.
        // Run after the object walk so the (typically smaller) multipart
        // pass still happens even if the object walk hit a transient
        // backend error mid-stream (per-bucket failure isolation —
        // matching the existing one-bad-bucket-doesn't-starve-others
        // policy).
        scan_in_flight_multipart_uploads(s4, &mgr, &bucket, now, &mut report).await;
    }
    Ok(report)
}

/// Walk one bucket end-to-end. Pagination uses the `continuation_token`
/// loop documented in
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html>.
async fn scan_bucket<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    mgr: &Arc<LifecycleManager>,
    bucket: &str,
    now: DateTime<Utc>,
    report: &mut ScanReport,
) {
    let mut continuation: Option<String> = None;
    loop {
        let list_input = ListObjectsV2Input {
            bucket: bucket.to_owned(),
            continuation_token: continuation.clone(),
            ..Default::default()
        };
        let list_req = synthetic_request(
            list_input,
            http::Method::GET,
            &format!("/{bucket}?list-type=2"),
        );
        let resp = match s4.as_ref().list_objects_v2(list_req).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    bucket = %bucket,
                    error = %e,
                    "S4 lifecycle: list_objects_v2 failed; skipping bucket for this scan",
                );
                report.action_errors = report.action_errors.saturating_add(1);
                return;
            }
        };
        let output = resp.output;
        let contents = output.contents.unwrap_or_default();
        for obj in &contents {
            let Some(key) = obj.key.as_deref() else {
                continue;
            };
            // Filter out S4-internal sidecars / shadow versions early so
            // the lifecycle scanner mirrors the same "client-visible
            // object set" the customer sees through `list_objects_v2`.
            // (The S4Service.list_objects_v2 handler already drops them
            // before returning, but this is a belt-and-braces guard for
            // any future bypass that builds the list elsewhere.)
            if key.ends_with(".s4index") {
                continue;
            }
            report.objects_evaluated = report.objects_evaluated.saturating_add(1);
            let size = obj.size.unwrap_or(0).max(0) as u64;
            let age = match obj.last_modified.as_ref().and_then(timestamp_to_chrono_utc) {
                Some(lm) => now.signed_duration_since(lm),
                None => Duration::zero(),
            };
            let tags: Vec<(String, String)> = s4
                .as_ref()
                .tag_manager()
                .and_then(|m| m.get_object_tags(bucket, key))
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();
            let Some(action) = mgr.evaluate(bucket, key, age, size, &tags) else {
                continue;
            };
            // Object-Lock-protected objects are skipped before any
            // backend-mutating call. Lock wins over Lifecycle, full
            // stop — matches AWS behaviour where an Expiration on a
            // locked object is dropped, not retried.
            //
            // v0.8.3 #65 (audit C-2): in addition to bumping the
            // in-report counter, emit a Prometheus
            // `s4_lifecycle_actions_total{action="skipped_locked"}`
            // sample so operator dashboards can alert on the
            // "lifecycle wanted to act but Object Lock vetoed" path
            // (previously a silent skip — the scanner's
            // `list_objects_v2` walked the key and `evaluate(...)`
            // returned an action, but no observable signal fired
            // when the backend would have refused the DELETE).
            if let Some(lock_mgr) = s4.as_ref().object_lock_manager()
                && let Some(state) = lock_mgr.get(bucket, key)
                && state.is_locked(now)
            {
                report.skipped_locked = report.skipped_locked.saturating_add(1);
                crate::metrics::record_lifecycle_action(bucket, "skipped_locked");
                continue;
            }
            match action {
                LifecycleAction::Expire => match execute_expire(s4, bucket, key).await {
                    Ok(()) => {
                        mgr.record_action(bucket, &LifecycleAction::Expire);
                        report.expired = report.expired.saturating_add(1);
                    }
                    Err(e) => {
                        warn!(
                            bucket = %bucket,
                            key = %key,
                            error = %e,
                            "S4 lifecycle: Expire action failed",
                        );
                        report.action_errors = report.action_errors.saturating_add(1);
                    }
                },
                LifecycleAction::Transition { storage_class } => {
                    match execute_transition(s4, bucket, key, &storage_class).await {
                        Ok(()) => {
                            mgr.record_action(
                                bucket,
                                &LifecycleAction::Transition {
                                    storage_class: storage_class.clone(),
                                },
                            );
                            report.transitioned = report.transitioned.saturating_add(1);
                        }
                        Err(e) => {
                            warn!(
                                bucket = %bucket,
                                key = %key,
                                storage_class = %storage_class,
                                error = %e,
                                "S4 lifecycle: Transition action failed",
                            );
                            report.action_errors = report.action_errors.saturating_add(1);
                        }
                    }
                }
                // v0.8.3 #69 (audit M-2): the per-key path's
                // `evaluate(...)` only ever returns Expire /
                // Transition (the AbortMultipartUpload variant comes
                // from the in-flight multipart walker further down,
                // which uses `evaluate_in_flight_multipart`). Match
                // exhaustiveness still requires an arm; logging at
                // warn keeps the control-flow honest if the
                // evaluator ever grows a path that surfaces an
                // abort here (e.g. someone wires a future evaluator
                // that returns abort for a regular object key — the
                // arm prevents silent dispatch and the warn surfaces
                // the misuse).
                LifecycleAction::AbortMultipartUpload { upload_id } => {
                    warn!(
                        bucket = %bucket,
                        key = %key,
                        upload_id = %upload_id,
                        "S4 lifecycle: AbortMultipartUpload returned for a key path; \
                         this is unexpected — the per-key evaluator should only \
                         emit Expire / Transition. Dropping action.",
                    );
                    report.action_errors = report.action_errors.saturating_add(1);
                }
            }
        }
        if output.is_truncated.unwrap_or(false) {
            // v0.8.4 #78 (audit M3): pagination guard hardening. AWS
            // guarantees `NextContinuationToken` when `is_truncated=true`
            // and that the token always advances, but a malformed
            // backend (or a third-party S3 emulator with a buggy
            // listing implementation) can break either invariant. Two
            // failure modes are now caught explicitly with a
            // `tracing::warn` so operators see the divergence rather
            // than spinning forever:
            //   1. `is_truncated=true` with `next_continuation_token=None`
            //   2. `next_continuation_token` repeats the previous value
            //      (caller would re-issue the identical request → same
            //      page → infinite loop)
            // Either condition exits the pagination loop for this scan
            // tick; the next scheduled tick re-enters from the start
            // marker, so transient backend bugs self-recover.
            let next = output.next_continuation_token.clone();
            if next.is_none() {
                warn!(
                    bucket = %bucket,
                    "S4 lifecycle: list_objects_v2 pagination stuck — \
                     is_truncated=true but next_continuation_token \
                     missing; breaking loop to avoid spin",
                );
                break;
            }
            if next == continuation {
                warn!(
                    bucket = %bucket,
                    token = ?continuation,
                    "S4 lifecycle: list_objects_v2 pagination stuck — \
                     same continuation_token repeated; breaking loop \
                     to avoid spin",
                );
                break;
            }
            continuation = next;
        } else {
            break;
        }
    }
}

/// v0.8.3 #69 (audit M-2): walk every in-flight multipart upload for
/// `bucket` via `list_multipart_uploads` (key-marker / upload-id-marker
/// pagination) and abort any whose `Initiated` time is older than the
/// rule's `abort_incomplete_multipart_upload_days` threshold. Successful
/// aborts bump `report.aborted_multipart` AND (`mgr.record_action`)
/// `s4_lifecycle_actions_total{action="abort_incomplete_multipart"}` so
/// operator dashboards see the same signal whether they look at
/// in-process counters or Prometheus.
///
/// On a successful abort the entry in `MultipartStateStore` (which
/// holds the per-upload SSE-C key bytes / tag set / object-lock recipe
/// captured at `CreateMultipartUpload` time) is also dropped — same
/// shape as the user-facing `abort_multipart_upload` handler in
/// `service.rs`. Without the drop the abandoned upload's `Zeroizing<[u8;
/// 32]>` SSE-C key would linger in `multipart_state` until the
/// `sweep_stale` background tick (v0.8.2 #62) reaped it on TTL.
///
/// Per-page / per-upload backend failures are logged at WARN and bumped
/// in `report.action_errors`; the loop does NOT abort the bucket — one
/// bad upload must not prevent the rest of the bucket's stale uploads
/// from being cleaned up. Mirrors the same isolation policy
/// `scan_bucket` uses for `list_objects_v2` failures.
async fn scan_in_flight_multipart_uploads<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    mgr: &Arc<LifecycleManager>,
    bucket: &str,
    now: DateTime<Utc>,
    report: &mut ScanReport,
) {
    let mut key_marker: Option<String> = None;
    let mut upload_id_marker: Option<String> = None;
    loop {
        let list_input = ListMultipartUploadsInput {
            bucket: bucket.to_owned(),
            key_marker: key_marker.clone(),
            upload_id_marker: upload_id_marker.clone(),
            ..Default::default()
        };
        let list_req =
            synthetic_request(list_input, http::Method::GET, &format!("/{bucket}?uploads"));
        let resp = match s4.as_ref().list_multipart_uploads(list_req).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    bucket = %bucket,
                    error = %e,
                    "S4 lifecycle: list_multipart_uploads failed; \
                     skipping bucket multipart sweep for this scan",
                );
                report.action_errors = report.action_errors.saturating_add(1);
                return;
            }
        };
        let output = resp.output;
        let uploads = output.uploads.unwrap_or_default();
        for upload in &uploads {
            let Some(upload_id) = upload.upload_id.as_deref() else {
                continue;
            };
            let Some(key) = upload.key.as_deref() else {
                continue;
            };
            // `Initiated` is `Option<Timestamp>`; absent or
            // unparseable → treat as "freshly initiated" (age 0)
            // and skip. Matches the conservative `last_modified`
            // handling in `scan_bucket` — never abort an upload
            // whose age we cannot determine.
            let Some(initiated) = upload.initiated.as_ref().and_then(timestamp_to_chrono_utc)
            else {
                continue;
            };
            let candidate = MultipartUploadCandidate {
                upload_id: upload_id.to_owned(),
                key: key.to_owned(),
                initiated,
                tags: Vec::new(),
            };
            let Some(action) = mgr.evaluate_in_flight_multipart(bucket, &candidate, now) else {
                continue;
            };
            let LifecycleAction::AbortMultipartUpload {
                upload_id: action_upload_id,
            } = action
            else {
                // The evaluator is contractually
                // AbortMultipartUpload-only on this path; this arm
                // exists only to satisfy match exhaustiveness if a
                // future rev returns a different variant. Treat as
                // an error so the divergence is observable.
                warn!(
                    bucket = %bucket,
                    key = %key,
                    upload_id = %upload_id,
                    "S4 lifecycle: evaluate_in_flight_multipart returned \
                     non-Abort action; dropping",
                );
                report.action_errors = report.action_errors.saturating_add(1);
                continue;
            };
            match execute_abort_multipart(s4, bucket, key, &action_upload_id).await {
                Ok(()) => {
                    mgr.record_action(
                        bucket,
                        &LifecycleAction::AbortMultipartUpload {
                            upload_id: action_upload_id.clone(),
                        },
                    );
                    report.aborted_multipart = report.aborted_multipart.saturating_add(1);
                    // Drop the per-upload state so the
                    // (Zeroizing-wrapped) SSE-C key bytes / tag
                    // recipe / object-lock recipe go away
                    // immediately rather than waiting for the
                    // hourly `sweep_stale` tick. Idempotent —
                    // `remove(...)` on a missing key is a no-op
                    // (some uploads may not have been registered
                    // here, e.g. a server restart between Create
                    // and the lifecycle sweep).
                    s4.as_ref().multipart_state().remove(&action_upload_id);
                }
                Err(e) => {
                    warn!(
                        bucket = %bucket,
                        key = %key,
                        upload_id = %action_upload_id,
                        error = %e,
                        "S4 lifecycle: AbortMultipartUpload action failed",
                    );
                    report.action_errors = report.action_errors.saturating_add(1);
                }
            }
        }
        if output.is_truncated.unwrap_or(false) {
            // v0.8.4 #78 (audit M3): pagination guard hardening — same
            // shape as the `scan_bucket` continuation-token guard above
            // but for the (key_marker, upload_id_marker) pair. AWS
            // guarantees `NextKeyMarker` (and `NextUploadIdMarker` when
            // an in-flight upload boundary lands inside a key) on
            // truncated responses, but a malformed backend can:
            //   1. set `is_truncated=true` and omit BOTH next markers
            //      (the next iteration re-issues the same request →
            //      same page → infinite loop), or
            //   2. echo the same marker pair back (same outcome).
            // Both modes warn-log and break — the next scheduled scan
            // re-enters from the original markers, so a transient
            // backend bug self-recovers.
            let next_key = output.next_key_marker.clone();
            let next_upload_id = output.next_upload_id_marker.clone();
            if next_key.is_none() && next_upload_id.is_none() {
                warn!(
                    bucket = %bucket,
                    "S4 lifecycle: list_multipart_uploads pagination \
                     stuck — is_truncated=true but both \
                     next_key_marker and next_upload_id_marker \
                     missing; breaking loop to avoid spin",
                );
                break;
            }
            if next_key == key_marker && next_upload_id == upload_id_marker {
                warn!(
                    bucket = %bucket,
                    key_marker = ?key_marker,
                    upload_id_marker = ?upload_id_marker,
                    "S4 lifecycle: list_multipart_uploads pagination \
                     stuck — same (key_marker, upload_id_marker) pair \
                     repeated; breaking loop to avoid spin",
                );
                break;
            }
            key_marker = next_key;
            upload_id_marker = next_upload_id;
        } else {
            break;
        }
    }
}

/// v0.8.3 #69 (audit M-2): issue `abort_multipart_upload` against the
/// wrapped `S4Service`. The handler in `service.rs` does the
/// `multipart_state.remove(...)` itself before forwarding to the
/// backend; we additionally `remove` from the lifecycle scanner side
/// (in [`scan_in_flight_multipart_uploads`]) to defensively cover the
/// case where the backend abort succeeds but the response routing
/// shortens early.
async fn execute_abort_multipart<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<(), String> {
    let input = AbortMultipartUploadInput {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        upload_id: upload_id.to_owned(),
        ..Default::default()
    };
    let req = synthetic_request(
        input,
        http::Method::DELETE,
        &format!("/{bucket}/{key}?uploadId={upload_id}"),
    );
    s4.as_ref()
        .abort_multipart_upload(req)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

/// Issue `delete_object` against the wrapped `S4Service`. The handler in
/// `service.rs` runs the WORM check itself, so even if the scanner's
/// pre-check missed (race with an MFA-Delete put-bucket-versioning), the
/// backend refuses the delete with `AccessDenied` and the error path
/// above bumps `action_errors` rather than silently losing data.
async fn execute_expire<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    bucket: &str,
    key: &str,
) -> Result<(), String> {
    let input = DeleteObjectInput {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        ..Default::default()
    };
    let req = synthetic_request(input, http::Method::DELETE, &format!("/{bucket}/{key}"));
    s4.as_ref()
        .delete_object(req)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

/// Rewrite the object's storage class via a same-key `copy_object` with
/// `MetadataDirective: COPY` (preserves user metadata) and the new
/// `storage_class`. Backends that ignore storage-class headers
/// effectively no-op; the counter still records the attempt so dashboards
/// reflect the scanner's intent.
async fn execute_transition<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    bucket: &str,
    key: &str,
    storage_class: &str,
) -> Result<(), String> {
    // CopyObjectInput has dozens of `Option` fields plus three required
    // (bucket / key / copy_source); the s3s-generated `builder()` is
    // the path that fills the optional ones with `None` for us. The
    // `set_*` setters return `&mut Self`, so we drive them in
    // statement form rather than as a method chain.
    let mut builder = CopyObjectInput::builder();
    builder.set_bucket(bucket.to_owned());
    builder.set_key(key.to_owned());
    builder.set_copy_source(CopySource::Bucket {
        bucket: bucket.to_owned().into_boxed_str(),
        key: key.to_owned().into_boxed_str(),
        version_id: None,
    });
    builder.set_metadata_directive(Some(MetadataDirective::from_static(
        MetadataDirective::COPY,
    )));
    builder.set_storage_class(Some(StorageClass::from(storage_class.to_owned())));
    let input = builder
        .build()
        .map_err(|e| format!("CopyObjectInput build: {e}"))?;
    let req = synthetic_request(input, http::Method::PUT, &format!("/{bucket}/{key}"));
    s4.as_ref()
        .copy_object(req)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(rule: LifecycleRule) -> LifecycleRule {
        LifecycleRule {
            status: LifecycleStatus::Enabled,
            ..rule
        }
    }

    fn cfg_with(rules: Vec<LifecycleRule>) -> LifecycleConfig {
        LifecycleConfig { rules }
    }

    fn manager_with(bucket: &str, rules: Vec<LifecycleRule>) -> LifecycleManager {
        let m = LifecycleManager::new();
        m.put(bucket, cfg_with(rules));
        m
    }

    #[test]
    fn evaluate_age_past_expiration_returns_expire() {
        let m = manager_with("b", vec![LifecycleRule::expire_after_days("r", 30)]);
        let action = m.evaluate("b", "k", Duration::days(31), 100, &[]);
        assert_eq!(action, Some(LifecycleAction::Expire));
    }

    #[test]
    fn evaluate_age_before_expiration_returns_none() {
        let m = manager_with("b", vec![LifecycleRule::expire_after_days("r", 30)]);
        let action = m.evaluate("b", "k", Duration::days(5), 100, &[]);
        assert_eq!(action, None);
    }

    #[test]
    fn evaluate_prefix_filter_matches() {
        let mut rule = LifecycleRule::expire_after_days("r", 1);
        rule.filter.prefix = Some("logs/".into());
        let m = manager_with("b", vec![rule]);
        assert_eq!(
            m.evaluate("b", "logs/2026/a.log", Duration::days(2), 1, &[]),
            Some(LifecycleAction::Expire)
        );
        assert_eq!(
            m.evaluate("b", "data/keep.bin", Duration::days(2), 1, &[]),
            None
        );
    }

    #[test]
    fn evaluate_tag_filter_requires_all_tags_to_match() {
        let mut rule = LifecycleRule::expire_after_days("r", 1);
        rule.filter.tags = vec![
            ("env".into(), "dev".into()),
            ("expirable".into(), "yes".into()),
        ];
        let m = manager_with("b", vec![rule]);
        // All tags present + matching → fire.
        assert_eq!(
            m.evaluate(
                "b",
                "k",
                Duration::days(2),
                1,
                &[
                    ("env".into(), "dev".into()),
                    ("expirable".into(), "yes".into()),
                    ("owner".into(), "alice".into()),
                ]
            ),
            Some(LifecycleAction::Expire)
        );
        // One tag missing → no fire.
        assert_eq!(
            m.evaluate(
                "b",
                "k",
                Duration::days(2),
                1,
                &[("env".into(), "dev".into())]
            ),
            None
        );
        // Tag present but with the wrong value → no fire.
        assert_eq!(
            m.evaluate(
                "b",
                "k",
                Duration::days(2),
                1,
                &[
                    ("env".into(), "prod".into()),
                    ("expirable".into(), "yes".into()),
                ]
            ),
            None
        );
    }

    #[test]
    fn evaluate_size_filters_gate_action() {
        let mut rule = LifecycleRule::expire_after_days("r", 1);
        rule.filter.object_size_greater_than = Some(1024);
        rule.filter.object_size_less_than = Some(10 * 1024);
        let m = manager_with("b", vec![rule]);
        // Inside the (1024, 10*1024) range → fire.
        assert_eq!(
            m.evaluate("b", "k", Duration::days(2), 4096, &[]),
            Some(LifecycleAction::Expire)
        );
        // At the boundary (size == greater_than) → strict `>`, no fire.
        assert_eq!(m.evaluate("b", "k", Duration::days(2), 1024, &[]), None);
        // Above the upper bound → no fire.
        assert_eq!(
            m.evaluate("b", "k", Duration::days(2), 100 * 1024, &[]),
            None
        );
    }

    #[test]
    fn evaluate_transition_fires_before_expiration() {
        // Transition at 30d, expiration at 365d, age 60d → transition.
        let rule = enabled(LifecycleRule {
            id: "r".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: Some(365),
            expiration_date: None,
            transitions: vec![TransitionRule {
                days: 30,
                storage_class: "GLACIER_IR".into(),
            }],
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: None,
        });
        let m = manager_with("b", vec![rule]);
        let action = m.evaluate("b", "k", Duration::days(60), 1, &[]);
        assert_eq!(
            action,
            Some(LifecycleAction::Transition {
                storage_class: "GLACIER_IR".into(),
            })
        );
    }

    #[test]
    fn evaluate_expiration_wins_when_threshold_is_earlier_than_transition() {
        // Expiration at 30d, transition at 90d, age 100d → expire (the
        // rule wants the object gone *before* it would have transitioned).
        let rule = enabled(LifecycleRule {
            id: "r".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: Some(30),
            expiration_date: None,
            transitions: vec![TransitionRule {
                days: 90,
                storage_class: "GLACIER".into(),
            }],
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: None,
        });
        let m = manager_with("b", vec![rule]);
        let action = m.evaluate("b", "k", Duration::days(100), 1, &[]);
        assert_eq!(action, Some(LifecycleAction::Expire));
    }

    #[test]
    fn evaluate_disabled_rule_never_fires() {
        let mut rule = LifecycleRule::expire_after_days("r", 1);
        rule.status = LifecycleStatus::Disabled;
        let m = manager_with("b", vec![rule]);
        assert_eq!(m.evaluate("b", "k", Duration::days(365), 1, &[]), None);
    }

    #[test]
    fn evaluate_unknown_bucket_returns_none() {
        let m = LifecycleManager::new();
        assert_eq!(m.evaluate("ghost", "k", Duration::days(365), 1, &[]), None);
    }

    #[test]
    fn evaluate_noncurrent_version_expiration() {
        let rule = enabled(LifecycleRule {
            id: "r".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: None,
            expiration_date: None,
            transitions: vec![],
            noncurrent_version_expiration_days: Some(7),
            abort_incomplete_multipart_upload_days: None,
        });
        let m = manager_with("b", vec![rule]);
        // current-version path → no rule matches (no expiration_days set).
        assert_eq!(m.evaluate("b", "k", Duration::days(30), 1, &[]), None);
        // noncurrent path with age past 7d → expire.
        let action = m.evaluate_with_flags(
            "b",
            "k",
            Duration::days(8),
            1,
            &[],
            EvaluateFlags {
                is_noncurrent: true,
                now: None,
            },
        );
        assert_eq!(action, Some(LifecycleAction::Expire));
        // noncurrent path with age before 7d → no fire.
        let action = m.evaluate_with_flags(
            "b",
            "k",
            Duration::days(3),
            1,
            &[],
            EvaluateFlags {
                is_noncurrent: true,
                now: None,
            },
        );
        assert_eq!(action, None);
    }

    #[test]
    fn evaluate_batch_distributes_actions_across_object_ages() {
        // Transition at 30d, expiration at 60d. Conflict resolver picks
        // expire iff `exp_days <= trans_days` for the chosen transition.
        // With exp=60, trans=30: at age 40-59 the transition fires; at
        // age >= 60 expiration wins (because exp_days=60 <= trans_days=30
        // is false, so... wait — re-read: the resolver compares
        // exp_threshold (60) vs trans_threshold (30) and triggers expire
        // ONLY when 60 <= 30, which is false → transition keeps winning
        // until both thresholds met but exp <= trans). For exp=60 trans=30
        // pair, transition always wins regardless of age (rule pattern is
        // "transition first, expire later" — the next scanner pass
        // picks up the expiration). So expect 4 transitions.
        let rule = enabled(LifecycleRule {
            id: "r".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: Some(60),
            expiration_date: None,
            transitions: vec![TransitionRule {
                days: 30,
                storage_class: "STANDARD_IA".into(),
            }],
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: None,
        });
        let m = manager_with("b", vec![rule]);
        let objects = vec![
            ("young".to_string(), Duration::days(10), 1u64, vec![]),
            ("middle".to_string(), Duration::days(40), 1u64, vec![]),
            ("middle2".to_string(), Duration::days(45), 1u64, vec![]),
            ("old".to_string(), Duration::days(90), 1u64, vec![]),
            ("ancient".to_string(), Duration::days(365), 1u64, vec![]),
        ];
        let actions = evaluate_batch(&m, "b", &objects);
        assert_eq!(actions.len(), 4);
        for (_, a) in &actions {
            assert!(matches!(a, LifecycleAction::Transition { .. }));
        }
    }

    #[test]
    fn json_round_trip_preserves_rules() {
        let rule = enabled(LifecycleRule {
            id: "complex".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter {
                prefix: Some("logs/".into()),
                tags: vec![("env".into(), "prod".into())],
                object_size_greater_than: Some(1024),
                object_size_less_than: None,
            },
            expiration_days: Some(365),
            expiration_date: None,
            transitions: vec![TransitionRule {
                days: 30,
                storage_class: "STANDARD_IA".into(),
            }],
            noncurrent_version_expiration_days: Some(7),
            abort_incomplete_multipart_upload_days: Some(3),
        });
        let m = manager_with("b1", vec![rule.clone()]);
        let json = m.to_json().expect("to_json");
        let m2 = LifecycleManager::from_json(&json).expect("from_json");
        let cfg = m2.get("b1").expect("bucket survives roundtrip");
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0], rule);
    }

    #[test]
    fn lifecycle_config_default_is_empty() {
        let cfg = LifecycleConfig::default();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn evaluate_batch_skips_locked_objects_at_caller_layer() {
        // The evaluator itself does not consult ObjectLock; the scanner
        // (and tests) are expected to filter locked keys out before /
        // after calling `evaluate_batch`. This test documents the
        // canonical pattern.
        let m = manager_with("b", vec![LifecycleRule::expire_after_days("r", 1)]);
        let objects = vec![
            ("locked".to_string(), Duration::days(30), 1u64, vec![]),
            ("free".to_string(), Duration::days(30), 1u64, vec![]),
        ];
        let locked_keys: std::collections::HashSet<&str> = ["locked"].into_iter().collect();
        let raw = evaluate_batch(&m, "b", &objects);
        let filtered: Vec<_> = raw
            .into_iter()
            .filter(|(k, _)| !locked_keys.contains(k.as_str()))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "free");
    }

    #[test]
    fn record_action_bumps_per_bucket_counter() {
        let m = LifecycleManager::new();
        m.record_action("b", &LifecycleAction::Expire);
        m.record_action("b", &LifecycleAction::Expire);
        m.record_action(
            "b",
            &LifecycleAction::Transition {
                storage_class: "GLACIER".into(),
            },
        );
        m.record_action(
            "b",
            &LifecycleAction::AbortMultipartUpload {
                upload_id: "u-xyz".into(),
            },
        );
        let snap = m.actions_snapshot();
        assert_eq!(snap.get(&("b".into(), "expire".into())).copied(), Some(2));
        assert_eq!(
            snap.get(&("b".into(), "transition".into())).copied(),
            Some(1)
        );
        assert_eq!(
            snap.get(&("b".into(), "abort_incomplete_multipart".into()))
                .copied(),
            Some(1),
            "v0.8.3 #69: AbortMultipartUpload metric_label must bump \
             `abort_incomplete_multipart` counter",
        );
    }

    // ---- v0.8.3 #69 (audit M-2): AbortIncompleteMultipartUpload --------
    //
    // Three unit tests covering the `evaluate_in_flight_multipart`
    // path: (a) age past threshold → AbortMultipartUpload, (b) age
    // before threshold → None, (c) the rule is `Disabled` → None
    // (a Disabled rule must never fire even on a stale upload).
    //
    // Test fixtures fake `now` and `initiated` so the assertion is
    // deterministic regardless of when the test runs.

    fn abort_rule(id: &str, days: u32) -> LifecycleRule {
        LifecycleRule {
            id: id.into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: None,
            expiration_date: None,
            transitions: Vec::new(),
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: Some(days),
        }
    }

    /// Upload age 8 days, rule threshold 7 days → AbortMultipartUpload
    /// fires with the upload's `upload_id`.
    #[test]
    fn evaluate_in_flight_multipart_aborts_past_threshold() {
        let m = manager_with("b", vec![abort_rule("r", 7)]);
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T00:00:00Z")
            .expect("parse now")
            .with_timezone(&Utc);
        let initiated = now - Duration::days(8);
        let candidate = MultipartUploadCandidate {
            upload_id: "u-stale".into(),
            key: "uploads/big.bin".into(),
            initiated,
            tags: Vec::new(),
        };
        let action = m.evaluate_in_flight_multipart("b", &candidate, now);
        assert_eq!(
            action,
            Some(LifecycleAction::AbortMultipartUpload {
                upload_id: "u-stale".into(),
            }),
        );
    }

    /// Upload age 1 day, rule threshold 7 days → no fire (upload is
    /// fresh enough to keep around).
    #[test]
    fn evaluate_in_flight_multipart_keeps_recent_upload() {
        let m = manager_with("b", vec![abort_rule("r", 7)]);
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T00:00:00Z")
            .expect("parse now")
            .with_timezone(&Utc);
        let initiated = now - Duration::days(1);
        let candidate = MultipartUploadCandidate {
            upload_id: "u-fresh".into(),
            key: "uploads/big.bin".into(),
            initiated,
            tags: Vec::new(),
        };
        let action = m.evaluate_in_flight_multipart("b", &candidate, now);
        assert_eq!(action, None);
    }

    /// `Disabled` rule must never fire even when the upload is well
    /// past the threshold — Disabled means the operator is staging the
    /// rule (preview / dry-run), the action must wait for Enable.
    #[test]
    fn evaluate_in_flight_multipart_skips_disabled_rule() {
        let mut rule = abort_rule("r", 1);
        rule.status = LifecycleStatus::Disabled;
        let m = manager_with("b", vec![rule]);
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T00:00:00Z")
            .expect("parse now")
            .with_timezone(&Utc);
        let initiated = now - Duration::days(365);
        let candidate = MultipartUploadCandidate {
            upload_id: "u-ancient".into(),
            key: "uploads/big.bin".into(),
            initiated,
            tags: Vec::new(),
        };
        let action = m.evaluate_in_flight_multipart("b", &candidate, now);
        assert_eq!(
            action, None,
            "Disabled rule must not abort even a 365-day-old upload",
        );
    }

    // ---- v0.7 #45: scanner runner tests --------------------------------
    //
    // These tests stand up an in-memory `S4Service` over a tiny
    // `ScannerMemBackend` (separate from the larger `MemoryBackend` in
    // `tests/roundtrip.rs` so this module stays self-contained). The
    // backend implements only the four `S3` methods the scanner touches:
    // `put_object`, `head_object`, `delete_object`, `list_objects_v2`.
    // Tags are exercised via the optional `with_tagging(...)` manager.
    //
    // Object age is faked by setting an `expire_after_days(0)` rule, so
    // any object whose backend-recorded `last_modified` is at or before
    // "now" matches — sidesteps the `head_object`/`Timestamp` parsing
    // entirely (and matches the canonical "operator just put the bucket
    // on aggressive expiration" test path).

    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use bytes::Bytes;
    use s3s::dto as dto2;
    use s3s::{S3Error, S3ErrorCode, S3Response, S3Result};
    use s4_codec::dispatcher::AlwaysDispatcher;
    use s4_codec::passthrough::Passthrough;
    use s4_codec::{CodecKind, CodecRegistry};

    use crate::S4Service;
    use crate::object_lock::{LockMode, ObjectLockManager, ObjectLockState};

    #[derive(Default)]
    struct ScannerMemBackend {
        objects: StdMutex<HashMap<(String, String), ScannerStored>>,
        /// v0.8.3 #69: in-flight multipart uploads keyed by
        /// `(bucket, upload_id)`. Tests seed entries via
        /// `put_multipart_upload(...)` so the lifecycle scanner's
        /// `list_multipart_uploads` walk has something to consume.
        multipart_uploads: StdMutex<HashMap<(String, String), ScannerMultipart>>,
    }

    #[derive(Clone)]
    struct ScannerStored {
        body: Bytes,
        last_modified: dto2::Timestamp,
    }

    /// v0.8.3 #69: minimal multipart-upload record the test backend
    /// returns from `list_multipart_uploads`. `initiated` is a
    /// `chrono::DateTime<Utc>` so tests can fake an old upload by
    /// passing `Utc::now() - Duration::days(N)` directly (no
    /// SystemTime arithmetic).
    #[derive(Clone)]
    struct ScannerMultipart {
        key: String,
        initiated: chrono::DateTime<Utc>,
    }

    impl ScannerMemBackend {
        fn put_now(&self, bucket: &str, key: &str, body: Bytes) {
            self.objects.lock().unwrap().insert(
                (bucket.to_owned(), key.to_owned()),
                ScannerStored {
                    body,
                    last_modified: dto2::Timestamp::from(std::time::SystemTime::now()),
                },
            );
        }

        /// v0.8.3 #69: seed an in-flight multipart upload the
        /// lifecycle scanner can then walk + abort.
        fn put_multipart_upload(
            &self,
            bucket: &str,
            upload_id: &str,
            key: &str,
            initiated: chrono::DateTime<Utc>,
        ) {
            self.multipart_uploads.lock().unwrap().insert(
                (bucket.to_owned(), upload_id.to_owned()),
                ScannerMultipart {
                    key: key.to_owned(),
                    initiated,
                },
            );
        }
    }

    #[async_trait::async_trait]
    impl S3 for ScannerMemBackend {
        async fn put_object(
            &self,
            req: S3Request<dto2::PutObjectInput>,
        ) -> S3Result<S3Response<dto2::PutObjectOutput>> {
            self.put_now(&req.input.bucket, &req.input.key, Bytes::new());
            Ok(S3Response::new(dto2::PutObjectOutput::default()))
        }

        async fn head_object(
            &self,
            req: S3Request<dto2::HeadObjectInput>,
        ) -> S3Result<S3Response<dto2::HeadObjectOutput>> {
            let key = (req.input.bucket.clone(), req.input.key.clone());
            let lock = self.objects.lock().unwrap();
            let stored = lock
                .get(&key)
                .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
            Ok(S3Response::new(dto2::HeadObjectOutput {
                content_length: Some(stored.body.len() as i64),
                last_modified: Some(stored.last_modified.clone()),
                ..Default::default()
            }))
        }

        async fn delete_object(
            &self,
            req: S3Request<dto2::DeleteObjectInput>,
        ) -> S3Result<S3Response<dto2::DeleteObjectOutput>> {
            let key = (req.input.bucket.clone(), req.input.key.clone());
            self.objects.lock().unwrap().remove(&key);
            Ok(S3Response::new(dto2::DeleteObjectOutput::default()))
        }

        async fn list_objects_v2(
            &self,
            req: S3Request<dto2::ListObjectsV2Input>,
        ) -> S3Result<S3Response<dto2::ListObjectsV2Output>> {
            let prefix = req.input.bucket.clone();
            let lock = self.objects.lock().unwrap();
            let mut contents: Vec<dto2::Object> = lock
                .iter()
                .filter(|((b, _), _)| b == &prefix)
                .map(|((_, k), v)| dto2::Object {
                    key: Some(k.clone()),
                    size: Some(v.body.len() as i64),
                    last_modified: Some(v.last_modified.clone()),
                    ..Default::default()
                })
                .collect();
            contents.sort_by(|a, b| a.key.cmp(&b.key));
            let key_count = i32::try_from(contents.len()).unwrap_or(i32::MAX);
            Ok(S3Response::new(dto2::ListObjectsV2Output {
                name: Some(prefix),
                contents: Some(contents),
                key_count: Some(key_count),
                is_truncated: Some(false),
                ..Default::default()
            }))
        }

        async fn copy_object(
            &self,
            _req: S3Request<dto2::CopyObjectInput>,
        ) -> S3Result<S3Response<dto2::CopyObjectOutput>> {
            // Transition path: scanner copies same-key with new
            // storage_class. The mem backend doesn't track storage
            // class, so it's a no-op success — exactly the AWS-side
            // behaviour for a backend that ignores the field.
            Ok(S3Response::new(dto2::CopyObjectOutput::default()))
        }

        // ---- v0.8.3 #69: multipart abort path -----------------------
        //
        // The lifecycle scanner walks `list_multipart_uploads` per
        // bucket and calls `abort_multipart_upload` on every upload
        // whose `Initiated` time is past the rule's threshold. The
        // test backend returns the seeded entries on listing and
        // drops them on abort so post-conditions are observable.

        async fn list_multipart_uploads(
            &self,
            req: S3Request<dto2::ListMultipartUploadsInput>,
        ) -> S3Result<S3Response<dto2::ListMultipartUploadsOutput>> {
            let bucket = req.input.bucket.clone();
            let lock = self.multipart_uploads.lock().unwrap();
            let mut uploads: Vec<dto2::MultipartUpload> = lock
                .iter()
                .filter(|((b, _), _)| b == &bucket)
                .map(|((_, upload_id), v)| {
                    let st: std::time::SystemTime = v.initiated.into();
                    dto2::MultipartUpload {
                        upload_id: Some(upload_id.clone()),
                        key: Some(v.key.clone()),
                        initiated: Some(dto2::Timestamp::from(st)),
                        ..Default::default()
                    }
                })
                .collect();
            // Stable order so test assertions on count + post-condition
            // do not race on the HashMap iteration order.
            uploads.sort_by(|a, b| a.upload_id.cmp(&b.upload_id));
            Ok(S3Response::new(dto2::ListMultipartUploadsOutput {
                bucket: Some(bucket),
                uploads: Some(uploads),
                is_truncated: Some(false),
                ..Default::default()
            }))
        }

        async fn abort_multipart_upload(
            &self,
            req: S3Request<dto2::AbortMultipartUploadInput>,
        ) -> S3Result<S3Response<dto2::AbortMultipartUploadOutput>> {
            let bucket = req.input.bucket.clone();
            let upload_id = req.input.upload_id.clone();
            self.multipart_uploads
                .lock()
                .unwrap()
                .remove(&(bucket, upload_id));
            Ok(S3Response::new(dto2::AbortMultipartUploadOutput::default()))
        }
    }

    fn make_service() -> Arc<S4Service<ScannerMemBackend>> {
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        Arc::new(S4Service::new(
            ScannerMemBackend::default(),
            registry,
            dispatcher,
        ))
    }

    #[tokio::test]
    async fn run_scan_once_no_lifecycle_manager_returns_empty_report() {
        // Service has no lifecycle manager attached — scanner must
        // no-op cleanly (operator might not have set
        // `--lifecycle-state-file`). Also covers the empty-buckets
        // path in `run_scan_once`.
        let s4 = make_service();
        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report, ScanReport::default());

        // And: lifecycle manager attached but no buckets configured.
        let mgr = Arc::new(LifecycleManager::new());
        let backend = ScannerMemBackend::default();
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let s4_empty = Arc::new(S4Service::new(backend, registry, dispatcher).with_lifecycle(mgr));
        let report = run_scan_once(&s4_empty).await.expect("scan empty");
        assert_eq!(report, ScanReport::default());
    }

    #[tokio::test]
    async fn run_scan_once_expires_matching_objects_via_backend() {
        // Three objects: only "stale.log" matches the rule (prefix
        // gating). The other two are written but not under the prefix,
        // so the evaluator returns None for them.
        let backend = ScannerMemBackend::default();
        backend.put_now("b", "stale.log", Bytes::from_static(b"x"));
        backend.put_now("b", "data/keep1.bin", Bytes::from_static(b"y"));
        backend.put_now("b", "data/keep2.bin", Bytes::from_static(b"z"));
        // Rule: any object under `stale.` prefix is expired immediately
        // (`expire_after_days(0)` matches age >= 0d, which is every
        // backend object).
        let mgr = Arc::new(LifecycleManager::new());
        let mut rule = LifecycleRule::expire_after_days("r", 0);
        rule.filter.prefix = Some("stale.".into());
        mgr.put("b", LifecycleConfig { rules: vec![rule] });
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher).with_lifecycle(Arc::clone(&mgr)),
        );

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.objects_evaluated, 3);
        assert_eq!(report.expired, 1);
        assert_eq!(report.transitioned, 0);
        assert_eq!(report.skipped_locked, 0);
        assert_eq!(report.action_errors, 0);
        // Backend post-condition: the matching key is gone, the others
        // remain. Read back through the service's own list_objects_v2
        // path (which is also what the customer-visible HTTP layer
        // serves) so we exercise the same code the scanner walked.
        let req = synthetic_request(
            ListObjectsV2Input {
                bucket: "b".into(),
                ..Default::default()
            },
            http::Method::GET,
            "/b?list-type=2",
        );
        let resp = s4
            .as_ref()
            .list_objects_v2(req)
            .await
            .expect("post-scan list");
        let keys: Vec<String> = resp
            .output
            .contents
            .unwrap_or_default()
            .into_iter()
            .filter_map(|o| o.key)
            .collect();
        assert!(!keys.contains(&"stale.log".to_string()));
        assert!(keys.contains(&"data/keep1.bin".to_string()));
        assert!(keys.contains(&"data/keep2.bin".to_string()));
        // Lifecycle action counter: one Expire bumped on bucket "b".
        let snap = mgr.actions_snapshot();
        assert_eq!(snap.get(&("b".into(), "expire".into())).copied(), Some(1));
    }

    #[tokio::test]
    async fn run_scan_once_skips_object_lock_protected_keys() {
        let backend = ScannerMemBackend::default();
        backend.put_now("b", "locked.log", Bytes::from_static(b"x"));
        backend.put_now("b", "free.log", Bytes::from_static(b"y"));
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let mgr = Arc::new(LifecycleManager::new());
        // Aggressive: every object expires immediately.
        mgr.put(
            "b",
            LifecycleConfig {
                rules: vec![LifecycleRule::expire_after_days("r", 0)],
            },
        );
        let lock_mgr = Arc::new(ObjectLockManager::new());
        // Lock retains "locked.log" until the year 2099 — Compliance
        // mode means even Governance bypass cannot delete it.
        let retain_until = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .expect("parse")
            .with_timezone(&Utc);
        lock_mgr.set(
            "b",
            "locked.log",
            ObjectLockState {
                mode: Some(LockMode::Compliance),
                retain_until: Some(retain_until),
                legal_hold_on: false,
            },
        );
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher)
                .with_lifecycle(Arc::clone(&mgr))
                .with_object_lock(Arc::clone(&lock_mgr)),
        );

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.objects_evaluated, 2);
        assert_eq!(report.expired, 1, "free.log should have been expired");
        assert_eq!(report.skipped_locked, 1, "locked.log must be skipped");
        assert_eq!(report.action_errors, 0);
    }

    /// v0.8.3 #65 (audit C-2): full scanner walk with a mix of free
    /// and locked objects must (a) leave outer/free objects expired,
    /// (b) skip the middle locked object, (c) bump
    /// `ScanReport::skipped_locked`, and (d) emit a Prometheus
    /// `s4_lifecycle_actions_total{action="skipped_locked"}` sample.
    /// Previously (v0.7 #45) the skip path bumped only the in-report
    /// counter — operator dashboards saw no signal when Object Lock
    /// vetoed a Lifecycle Expiration, which is the silent-failure
    /// observability gap audit C-2 called out.
    #[tokio::test]
    async fn scan_one_config_skips_locked_objects_and_bumps_metric() {
        // The Prometheus recorder is a process-global slot. Multiple
        // tests in the same binary race on `install_recorder()`, so
        // we route through `crate::metrics::test_metrics_handle()`
        // which is OnceLock-guarded and shared with the
        // `metrics::tests::install_and_render_basic_counters` test.
        // Use a unique bucket label so this test's sample line is
        // identifiable even when other tests in the binary also bump
        // the lifecycle counter under different bucket labels.
        let metrics_handle = crate::metrics::test_metrics_handle();

        let bucket = "lc-locked-metric-65";
        let backend = ScannerMemBackend::default();
        // Three objects; the middle one ("middle.log") will be
        // Object-Lock-Compliance-locked until 2099. The two outer
        // objects ("outer-a.log", "outer-c.log") have no lock state
        // attached, so the aggressive `expire_after_days(0)` rule
        // matches and the scanner's `delete_object` actually fires.
        backend.put_now(bucket, "outer-a.log", Bytes::from_static(b"a"));
        backend.put_now(bucket, "middle.log", Bytes::from_static(b"m"));
        backend.put_now(bucket, "outer-c.log", Bytes::from_static(b"c"));

        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let mgr = Arc::new(LifecycleManager::new());
        mgr.put(
            bucket,
            LifecycleConfig {
                rules: vec![LifecycleRule::expire_after_days("r", 1)],
            },
        );
        // Object-Lock Compliance retain until far in the future (2099).
        // `is_locked(now)` then returns `true` regardless of when the
        // test actually runs.
        let lock_mgr = Arc::new(ObjectLockManager::new());
        let retain_until = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .expect("parse retain_until")
            .with_timezone(&Utc);
        lock_mgr.set(
            bucket,
            "middle.log",
            ObjectLockState {
                mode: Some(LockMode::Compliance),
                retain_until: Some(retain_until),
                legal_hold_on: false,
            },
        );
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher)
                .with_lifecycle(Arc::clone(&mgr))
                .with_object_lock(Arc::clone(&lock_mgr)),
        );

        // The objects above were `put_now(...)` with `last_modified =
        // SystemTime::now()`, so their computed `age` is roughly zero
        // and the `expire_after_days(1)` rule alone would NOT match.
        // Force the rule threshold down to zero days so all three
        // objects qualify for Expiration — the test is about the Lock
        // veto, not the age math.
        mgr.put(
            bucket,
            LifecycleConfig {
                rules: vec![LifecycleRule::expire_after_days("r", 0)],
            },
        );

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.objects_evaluated, 3);
        assert_eq!(
            report.expired, 2,
            "outer-a.log + outer-c.log must be DELETEd; got {report:?}"
        );
        assert_eq!(
            report.skipped_locked, 1,
            "middle.log is Compliance-locked → scanner must skip; got {report:?}"
        );
        assert_eq!(report.transitioned, 0);
        assert_eq!(report.action_errors, 0);

        // Render the Prometheus exporter and assert that a sample line
        // for `s4_lifecycle_actions_total{...action="skipped_locked",
        // bucket="lc-locked-metric-65"...}` is present with value >= 1.
        // The metrics-exporter-prometheus crate sorts labels
        // alphabetically (`bucket` appears before `action` in the
        // rendered output), so we substring-match both label fragments
        // rather than rely on a fixed ordering. We use `>=` (not
        // `==`) because the recorder is process-global and a parallel
        // run of the same test in a future session could legitimately
        // bump it again — but since the bucket label embeds an
        // issue-unique suffix, no other test in this binary touches
        // this specific (action, bucket) pair.
        let rendered = metrics_handle.render();
        let bucket_frag = format!("bucket=\"{bucket}\"");
        let action_frag = "action=\"skipped_locked\"";
        let line = rendered
            .lines()
            .find(|l| {
                l.starts_with("s4_lifecycle_actions_total{")
                    && l.contains(&bucket_frag)
                    && l.contains(action_frag)
            })
            .unwrap_or_else(|| {
                panic!(
                    "Prometheus output missing skipped_locked sample for {bucket}; \
                     full render:\n{rendered}"
                )
            });
        // Parse the trailing counter value (whitespace-separated).
        let value: u64 = line
            .split_whitespace()
            .next_back()
            .expect("counter value column")
            .parse()
            .expect("counter value is u64");
        assert!(
            value >= 1,
            "skipped_locked counter must be >= 1 after scan; line: {line}"
        );
    }

    /// v0.8.3 #69 (audit M-2): end-to-end test of the multipart sweep.
    /// Two in-flight uploads are seeded — `u-stale` initiated 8 days
    /// ago, `u-fresh` initiated 1 hour ago. The lifecycle rule sets
    /// `abort_incomplete_multipart_upload_days = 7`. The scanner must
    /// abort `u-stale` (bumping `report.aborted_multipart`) but leave
    /// `u-fresh` alone. Object walk is a no-op (no objects seeded), so
    /// the report's expire / transition counters stay at zero.
    #[tokio::test]
    async fn run_scan_once_aborts_stale_multipart_upload() {
        let backend = ScannerMemBackend::default();
        let bucket = "lc-mp-69";
        let now = Utc::now();
        backend.put_multipart_upload(
            bucket,
            "u-stale",
            "uploads/big.bin",
            now - Duration::days(8),
        );
        backend.put_multipart_upload(
            bucket,
            "u-fresh",
            "uploads/fresh.bin",
            now - Duration::hours(1),
        );

        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let mgr = Arc::new(LifecycleManager::new());
        let mut rule = LifecycleRule {
            id: "abort-7d".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: None,
            expiration_date: None,
            transitions: Vec::new(),
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: Some(7),
        };
        rule.filter.prefix = Some("uploads/".into());
        mgr.put(bucket, LifecycleConfig { rules: vec![rule] });
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher).with_lifecycle(Arc::clone(&mgr)),
        );

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(
            report.aborted_multipart, 1,
            "u-stale must be aborted; got {report:?}",
        );
        assert_eq!(report.action_errors, 0);
        assert_eq!(report.expired, 0);
        assert_eq!(report.transitioned, 0);

        // Backend post-condition via the wire-side
        // `list_multipart_uploads` path: only the fresh upload
        // (`u-fresh`) survives — `u-stale` was aborted by the
        // scanner.
        let post_req = synthetic_request(
            ListMultipartUploadsInput {
                bucket: bucket.into(),
                ..Default::default()
            },
            http::Method::GET,
            &format!("/{bucket}?uploads"),
        );
        let post = s4
            .as_ref()
            .list_multipart_uploads(post_req)
            .await
            .expect("post-scan list_multipart_uploads");
        let remaining_ids: Vec<String> = post
            .output
            .uploads
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| u.upload_id)
            .collect();
        assert_eq!(
            remaining_ids,
            vec!["u-fresh".to_string()],
            "exactly u-fresh must remain after the sweep; got {remaining_ids:?}",
        );

        // Counter snapshot agrees with the report.
        let snap = mgr.actions_snapshot();
        assert_eq!(
            snap.get(&(bucket.into(), "abort_incomplete_multipart".into()))
                .copied(),
            Some(1),
            "v0.8.3 #69: abort_incomplete_multipart counter must be 1",
        );
    }

    // ---- v0.8.4 #78 (audit M3): pagination guard hardening ------------
    //
    // The two backends below intentionally return malformed truncated
    // responses on the FIRST call to the offending list endpoint:
    //   * `MalformedListObjectsBackend.list_objects_v2` returns
    //     `is_truncated=true, next_continuation_token=None`
    //   * `MalformedListMultipartBackend.list_multipart_uploads`
    //     returns `is_truncated=true, next_key_marker=None,
    //     next_upload_id_marker=None`
    // Each backend tracks call count via a `StdMutex<u32>`. If the
    // pagination guard fails to break, the second iteration re-issues
    // the same request and the backend `panic!()`s — turning the
    // infinite loop into a deterministic test failure (and avoiding
    // the alternative "test hangs forever" outcome which would block
    // CI).
    //
    // Both backends pair the malformed list with a benign no-op for
    // the OTHER list endpoint so `run_scan_once` (which always invokes
    // both `scan_bucket` and `scan_in_flight_multipart_uploads`) does
    // not collide with the path under test.

    use std::sync::atomic::{AtomicU32, Ordering};

    /// Backend whose `list_objects_v2` is malformed (`is_truncated=true`
    /// with `next_continuation_token=None`); `list_multipart_uploads`
    /// is a benign empty non-truncated response. A second
    /// `list_objects_v2` call panics — proving the v0.8.4 #78 guard
    /// short-circuits the pagination loop.
    #[derive(Default)]
    struct MalformedListObjectsBackend {
        list_calls: AtomicU32,
    }

    #[async_trait::async_trait]
    impl S3 for MalformedListObjectsBackend {
        async fn list_objects_v2(
            &self,
            req: S3Request<dto2::ListObjectsV2Input>,
        ) -> S3Result<S3Response<dto2::ListObjectsV2Output>> {
            let n = self.list_calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                n == 0,
                "v0.8.4 #78: list_objects_v2 must be called exactly \
                 once when the guard fires; got call #{} which means \
                 the pagination loop did not break on a missing \
                 next_continuation_token",
                n + 1
            );
            Ok(S3Response::new(dto2::ListObjectsV2Output {
                name: Some(req.input.bucket.clone()),
                contents: Some(Vec::new()),
                key_count: Some(0),
                is_truncated: Some(true),
                next_continuation_token: None,
                ..Default::default()
            }))
        }

        async fn list_multipart_uploads(
            &self,
            req: S3Request<dto2::ListMultipartUploadsInput>,
        ) -> S3Result<S3Response<dto2::ListMultipartUploadsOutput>> {
            // Benign: empty + not-truncated. The multipart path is not
            // under test here; we just need it to no-op cleanly so
            // `run_scan_once` walks both and the assertion isolates
            // the object-list guard.
            Ok(S3Response::new(dto2::ListMultipartUploadsOutput {
                bucket: Some(req.input.bucket),
                uploads: Some(Vec::new()),
                is_truncated: Some(false),
                ..Default::default()
            }))
        }
    }

    /// Backend whose `list_multipart_uploads` is malformed
    /// (`is_truncated=true` with both `next_key_marker=None` and
    /// `next_upload_id_marker=None`); `list_objects_v2` is benign.
    /// Second `list_multipart_uploads` call panics.
    #[derive(Default)]
    struct MalformedListMultipartBackend {
        mp_calls: AtomicU32,
    }

    #[async_trait::async_trait]
    impl S3 for MalformedListMultipartBackend {
        async fn list_objects_v2(
            &self,
            req: S3Request<dto2::ListObjectsV2Input>,
        ) -> S3Result<S3Response<dto2::ListObjectsV2Output>> {
            // Benign: empty + not-truncated.
            Ok(S3Response::new(dto2::ListObjectsV2Output {
                name: Some(req.input.bucket),
                contents: Some(Vec::new()),
                key_count: Some(0),
                is_truncated: Some(false),
                ..Default::default()
            }))
        }

        async fn list_multipart_uploads(
            &self,
            req: S3Request<dto2::ListMultipartUploadsInput>,
        ) -> S3Result<S3Response<dto2::ListMultipartUploadsOutput>> {
            let n = self.mp_calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                n == 0,
                "v0.8.4 #78: list_multipart_uploads must be called \
                 exactly once when the guard fires; got call #{} \
                 which means the pagination loop did not break on \
                 missing (next_key_marker, next_upload_id_marker)",
                n + 1
            );
            Ok(S3Response::new(dto2::ListMultipartUploadsOutput {
                bucket: Some(req.input.bucket),
                uploads: Some(Vec::new()),
                is_truncated: Some(true),
                next_key_marker: None,
                next_upload_id_marker: None,
                ..Default::default()
            }))
        }
    }

    /// v0.8.4 #78 (audit M3): a backend that lies about
    /// `list_objects_v2` truncation (sets `is_truncated=true` but omits
    /// `next_continuation_token`) must NOT cause the lifecycle scanner
    /// to spin. The guard in `scan_bucket` warn-logs and breaks the
    /// loop after the first malformed page; the backend's call counter
    /// + assertion turns any regression into an immediate panic
    /// instead of a test hang. The scan completes cleanly with
    /// `buckets_scanned = 1` and no actions taken (the malformed page
    /// has zero contents).
    #[tokio::test]
    async fn scan_handles_truncated_with_missing_marker_without_infinite_loop() {
        let backend = MalformedListObjectsBackend::default();
        let bucket = "lc-malformed-list-78";
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let mgr = Arc::new(LifecycleManager::new());
        // Any rule will do — the malformed listing returns zero
        // contents so the evaluator never sees a key. We just need
        // the bucket to be in `mgr.buckets()` so `scan_bucket` runs.
        mgr.put(
            bucket,
            LifecycleConfig {
                rules: vec![LifecycleRule::expire_after_days("r", 0)],
            },
        );
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher).with_lifecycle(Arc::clone(&mgr)),
        );

        // The decisive assertion is "this future completes" — if the
        // guard regressed, the second `list_objects_v2` call panics
        // (per the backend's `assert!`) and the test fails. We also
        // sanity-check the report shape: scanner saw the bucket but
        // took no actions (zero contents in the malformed page).
        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.objects_evaluated, 0);
        assert_eq!(report.expired, 0);
        assert_eq!(report.transitioned, 0);
        assert_eq!(report.action_errors, 0);
    }

    /// v0.8.4 #78 (audit M3): same guarantee for the multipart sweep
    /// — a backend that returns `is_truncated=true` with both
    /// `next_key_marker=None` and `next_upload_id_marker=None` must
    /// NOT cause `scan_in_flight_multipart_uploads` to spin. Second
    /// `list_multipart_uploads` call panics if the guard regresses.
    #[tokio::test]
    async fn scan_multipart_handles_truncated_with_missing_marker_without_infinite_loop() {
        let backend = MalformedListMultipartBackend::default();
        let bucket = "lc-malformed-mp-78";
        let registry =
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
        let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
        let mgr = Arc::new(LifecycleManager::new());
        // Rule with `abort_incomplete_multipart_upload_days = Some(7)`
        // so the multipart-evaluator path is reachable (the rule body
        // is otherwise irrelevant — the malformed listing has zero
        // uploads).
        let mut rule = LifecycleRule {
            id: "abort-7d".into(),
            status: LifecycleStatus::Enabled,
            filter: LifecycleFilter::default(),
            expiration_days: None,
            expiration_date: None,
            transitions: Vec::new(),
            noncurrent_version_expiration_days: None,
            abort_incomplete_multipart_upload_days: Some(7),
        };
        rule.filter.prefix = Some("uploads/".into());
        mgr.put(bucket, LifecycleConfig { rules: vec![rule] });
        let s4 = Arc::new(
            S4Service::new(backend, registry, dispatcher).with_lifecycle(Arc::clone(&mgr)),
        );

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.aborted_multipart, 0);
        assert_eq!(report.action_errors, 0);
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `by_bucket` write
    /// guard poisons the lock. `to_json` must recover via
    /// [`crate::lock_recovery::recover_read`] and surface the data
    /// instead of re-panicking on the SIGUSR1 dump-back path.
    #[test]
    fn lifecycle_to_json_after_panic_recovers_via_poison() {
        let mgr = std::sync::Arc::new(LifecycleManager::new());
        mgr.put(
            "b",
            LifecycleConfig {
                rules: vec![LifecycleRule {
                    id: "r1".into(),
                    status: LifecycleStatus::Enabled,
                    filter: LifecycleFilter::default(),
                    expiration_days: Some(30),
                    expiration_date: None,
                    transitions: Vec::new(),
                    noncurrent_version_expiration_days: None,
                    abort_incomplete_multipart_upload_days: None,
                }],
            },
        );
        let mgr_cl = std::sync::Arc::clone(&mgr);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = mgr_cl.by_bucket.write().expect("clean lock");
            g.entry("b2".into()).or_default();
            panic!("force-poison");
        }));
        assert!(
            mgr.by_bucket.is_poisoned(),
            "write panic must poison by_bucket lock"
        );
        let json = mgr.to_json().expect("to_json after poison must succeed");
        let mgr2 = LifecycleManager::from_json(&json).expect("from_json");
        assert!(
            mgr2.get("b").is_some(),
            "recovered snapshot keeps original config"
        );
    }
}
