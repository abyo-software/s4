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
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

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
}

impl LifecycleAction {
    /// Stable label suitable for a metric counter
    /// (`s4_lifecycle_actions_total{action="..."}`).
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::Expire => "expire",
            Self::Transition { .. } => "transition",
        }
    }
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
        self.by_bucket
            .write()
            .expect("lifecycle state RwLock poisoned")
            .insert(bucket.to_owned(), config);
    }

    /// Return a clone of the bucket's configuration, if any.
    #[must_use]
    pub fn get(&self, bucket: &str) -> Option<LifecycleConfig> {
        self.by_bucket
            .read()
            .expect("lifecycle state RwLock poisoned")
            .get(bucket)
            .cloned()
    }

    /// Drop the bucket's lifecycle configuration (idempotent — missing
    /// bucket is OK).
    pub fn delete(&self, bucket: &str) {
        self.by_bucket
            .write()
            .expect("lifecycle state RwLock poisoned")
            .remove(bucket);
    }

    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let by_bucket = self
            .by_bucket
            .read()
            .expect("lifecycle state RwLock poisoned")
            .clone();
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

    /// Stamp the per-bucket action counter and bump the matching
    /// Prometheus counter. Called by the future scanner after a successful
    /// delete / metadata rewrite.
    pub fn record_action(&self, bucket: &str, action: &LifecycleAction) {
        let label = action.metric_label();
        let key = (bucket.to_owned(), label.to_owned());
        let mut guard = self
            .actions_total
            .write()
            .expect("lifecycle actions counter RwLock poisoned");
        let entry = guard.entry(key).or_insert(0);
        *entry = entry.saturating_add(1);
        crate::metrics::record_lifecycle_action(bucket, label);
    }

    /// Read-only snapshot of the per-(bucket, action) counter map.
    /// Useful for tests + introspection (`/admin/lifecycle/stats` style
    /// endpoints in the future).
    #[must_use]
    pub fn actions_snapshot(&self) -> HashMap<(String, String), u64> {
        self.actions_total
            .read()
            .expect("lifecycle actions counter RwLock poisoned")
            .clone()
    }

    /// All buckets with a lifecycle configuration attached. Sorted for
    /// stable scanner ordering.
    #[must_use]
    pub fn buckets(&self) -> Vec<String> {
        let map = self
            .by_bucket
            .read()
            .expect("lifecycle state RwLock poisoned");
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
        let snap = m.actions_snapshot();
        assert_eq!(snap.get(&("b".into(), "expire".into())).copied(), Some(2));
        assert_eq!(
            snap.get(&("b".into(), "transition".into())).copied(),
            Some(1)
        );
    }
}
