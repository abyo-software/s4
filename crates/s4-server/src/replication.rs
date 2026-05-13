//! Bucket-to-bucket asynchronous replication (v0.6 #40).
//!
//! AWS S3 Cross-Region Replication (CRR) lets a bucket owner declare a
//! `ReplicationConfiguration` whose rules say "for every PUT to this
//! bucket that matches `<filter>`, asynchronously copy the new object to
//! `<destination_bucket>`". The source object grows an
//! `x-amz-replication-status` of `PENDING` → `COMPLETED` (or `FAILED`),
//! the replica gets stamped `REPLICA`, and consumers can poll either
//! HEAD to see how the replication is going.
//!
//! ## v0.6 #40 scope (single-instance only)
//!
//! - **Same S4 endpoint** — the source bucket and the destination bucket
//!   live on the same `S4Service`. True cross-region (multi-instance,
//!   wire-replicated) replication is a v0.7+ follow-up that needs a
//!   `aws-sdk-s3` PUT to a remote endpoint with its own credentials.
//! - **Async only** — the originating `put_object` returns as soon as
//!   the source backend write is done. The replica PUT happens on a
//!   detached `tokio::spawn` task and never blocks the client. There is
//!   no synchronous `replication_required` mode (would defeat the whole
//!   point of CRR being asynchronous in the first place).
//! - **Retry budget = 3 attempts** with exponential backoff (50ms,
//!   100ms, 200ms). On exhaustion the per-(bucket, key) status flips to
//!   `Failed` and `dropped_total` is bumped + a warn-level log line is
//!   emitted so operators see the loss in `s4_replication_dropped_total`.
//! - **Highest-priority rule wins** when multiple rules match a single
//!   object key (S3 spec). Ties are broken by declaration order
//!   (deterministic for tests).
//! - **`status_enabled = false` rules never match**, mirroring the AWS
//!   `ReplicationRuleStatus::Disabled` semantics — the rule sits in the
//!   configuration document but is inert.
//! - **Replica is full-body** — there is no delta replication, no
//!   incremental fetch, no batching. Every matching PUT triggers one
//!   independent destination PUT.
//!
//! ## what is NOT in v0.6 #40
//!
//! - Delete-marker replication (S3's `DeleteMarkerReplication` block) —
//!   v0.7+. Right now `delete_object` does not fan out a destination
//!   delete; the replica drifts on the source's deletion.
//! - Replication of multipart-completed objects through the per-part
//!   copy path. The whole compose-then-PUT result of CMU is replicated
//!   as a single PUT, which is fine for single-instance and matches
//!   what AWS does for source objects ≤ 5 GiB.
//! - SSE-KMS-encrypted replicas with KMS-key-id rewriting per the
//!   `SourceSelectionCriteria` block (the source's wrapped DEK is
//!   replicated as-is — fine for single-instance because the same KMS
//!   backend unwraps both copies).
//! - Replication metrics (RTC) — a v0.7+ follow-up that wires a
//!   `replication_lag_seconds` histogram.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Per-(bucket, key) replication state, surfaced as the
/// `x-amz-replication-status` HEAD/GET response header. Values match the
/// AWS wire form exactly.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplicationStatus {
    /// Replication has been queued (a matching rule fired and the
    /// dispatcher task has been spawned) but the destination PUT has
    /// not yet succeeded.
    Pending,
    /// Replication has succeeded — the replica exists in the
    /// destination bucket.
    Completed,
    /// Replication failed permanently (retry budget exhausted).
    Failed,
    /// Stamped on the destination side so the replica is
    /// distinguishable from a normal PUT, matching AWS CRR's
    /// "replica stamp" behaviour.
    Replica,
}

impl ReplicationStatus {
    /// AWS wire-string form. Caller stamps it on the response as the
    /// `x-amz-replication-status` header.
    #[must_use]
    pub fn as_aws_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Replica => "REPLICA",
        }
    }
}

/// Filter on a `ReplicationRule` — the AND of a key-prefix predicate
/// and a tag predicate. AWS S3's wire form uses a sum type
/// (`Prefix | Tag | And { Prefix, Tags }`); we collapse those into
/// the single representation that the in-memory matcher needs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationFilter {
    /// Empty / `None` means "any prefix".
    pub prefix: Option<String>,
    /// AND of every tag pair — every entry here must be present in the
    /// object's tag set for the rule to fire. Empty means "no tag
    /// predicate".
    pub tags: Vec<(String, String)>,
}

/// One replication rule. Each rule independently decides whether to
/// copy an object based on the (key, tags) tuple; the replication
/// manager picks the highest-priority matching rule when multiple
/// fire on the same object.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationRule {
    /// Operator-supplied id (max 255 chars per AWS).
    pub id: String,
    /// Higher number = higher priority (S3 spec). When two rules match
    /// the same key, the higher `priority` wins; ties broken by
    /// declaration order.
    pub priority: u32,
    /// `false` makes the rule inert without removing it from the
    /// configuration document — mirrors AWS's `Disabled` status.
    pub status_enabled: bool,
    /// Subset of source-bucket objects this rule applies to.
    pub filter: ReplicationFilter,
    /// Where to copy matching objects. Plain bucket name (no ARN) for
    /// the v0.6 #40 single-instance scope.
    pub destination_bucket: String,
    /// Optional storage-class override on the replica. `None` = keep
    /// the source's class (S3 default).
    pub destination_storage_class: Option<String>,
}

/// Per-bucket replication configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationConfig {
    /// Placeholder ARN — not consumed by S4 itself, kept for AWS wire
    /// compatibility (the `Role` field is mandatory in the
    /// `PutBucketReplication` XML payload).
    pub role: String,
    pub rules: Vec<ReplicationRule>,
}

/// JSON snapshot — `bucket -> ReplicationConfig`. Mirrors the shape of
/// `notifications::NotificationSnapshot` so operators can hand-edit
/// configurations across restart cycles.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ReplicationSnapshot {
    by_bucket: HashMap<String, ReplicationConfig>,
    /// Per-(bucket, key) replication status. Persisted so a restart
    /// doesn't lose the COMPLETED stamp on already-replicated
    /// objects.
    statuses: Vec<((String, String), ReplicationStatus)>,
}

/// In-memory manager of per-bucket replication configurations + per-
/// (bucket, key) replication statuses.
pub struct ReplicationManager {
    by_bucket: RwLock<HashMap<String, ReplicationConfig>>,
    /// Per-(source_bucket, key) replication status. Looked up by
    /// `head_object` / `get_object` to stamp `x-amz-replication-status`
    /// on the response.
    statuses: RwLock<HashMap<(String, String), ReplicationStatus>>,
    /// Bumped each time the dispatcher exhausts its retry budget on a
    /// destination PUT. Exposed publicly so the metrics layer can poll
    /// without taking the configuration lock.
    pub dropped_total: AtomicU64,
}

impl Default for ReplicationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ReplicationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicationManager")
            .field("dropped_total", &self.dropped_total.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ReplicationManager {
    /// Empty manager — no bucket has any replication rules.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_bucket: RwLock::new(HashMap::new()),
            statuses: RwLock::new(HashMap::new()),
            dropped_total: AtomicU64::new(0),
        }
    }

    /// `put_bucket_replication` handler entry. The bucket's existing
    /// configuration is fully replaced (S3 spec — `PutBucketReplication`
    /// is upsert-style at the bucket scope, not per-rule patch).
    pub fn put(&self, bucket: &str, config: ReplicationConfig) {
        self.by_bucket
            .write()
            .expect("replication state RwLock poisoned")
            .insert(bucket.to_owned(), config);
    }

    /// `get_bucket_replication` handler entry. Returns `None` when
    /// nothing is registered (AWS S3 returns
    /// `ReplicationConfigurationNotFoundError` in that case; the
    /// service-layer handler maps `None` accordingly).
    #[must_use]
    pub fn get(&self, bucket: &str) -> Option<ReplicationConfig> {
        self.by_bucket
            .read()
            .expect("replication state RwLock poisoned")
            .get(bucket)
            .cloned()
    }

    /// Drop the configuration for `bucket`. Idempotent.
    pub fn delete(&self, bucket: &str) {
        self.by_bucket
            .write()
            .expect("replication state RwLock poisoned")
            .remove(bucket);
    }

    /// Serialise the entire manager state (configurations + per-key
    /// statuses) to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let snap = ReplicationSnapshot {
            by_bucket: self
                .by_bucket
                .read()
                .expect("replication state RwLock poisoned")
                .clone(),
            statuses: self
                .statuses
                .read()
                .expect("replication state RwLock poisoned")
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        serde_json::to_string(&snap)
    }

    /// Restore a manager from a previously-emitted snapshot. The
    /// `dropped_total` counter is reset to 0 — historical drops are
    /// runtime metrics, not configuration.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: ReplicationSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            by_bucket: RwLock::new(snap.by_bucket),
            statuses: RwLock::new(snap.statuses.into_iter().collect()),
            dropped_total: AtomicU64::new(0),
        })
    }

    /// Match an object against the bucket's rules and return the
    /// highest-priority enabled rule whose filter matches. Returns
    /// `None` when no rule matches (or no configuration is registered
    /// for the bucket). Ties on `priority` are broken by declaration
    /// order — the first such rule wins.
    #[must_use]
    pub fn match_rule(
        &self,
        bucket: &str,
        key: &str,
        object_tags: &[(String, String)],
    ) -> Option<ReplicationRule> {
        let map = self
            .by_bucket
            .read()
            .expect("replication state RwLock poisoned");
        let cfg = map.get(bucket)?;
        let mut best: Option<&ReplicationRule> = None;
        for rule in &cfg.rules {
            if !rule.status_enabled {
                continue;
            }
            if !filter_matches(&rule.filter, key, object_tags) {
                continue;
            }
            best = match best {
                None => Some(rule),
                Some(prev) if rule.priority > prev.priority => Some(rule),
                Some(prev) => Some(prev),
            };
        }
        best.cloned()
    }

    /// Stamp the per-(bucket, key) replication status. Replaces any
    /// previous entry — a `Failed` follows `Pending`, etc.
    pub fn record_status(&self, bucket: &str, key: &str, status: ReplicationStatus) {
        self.statuses
            .write()
            .expect("replication state RwLock poisoned")
            .insert((bucket.to_owned(), key.to_owned()), status);
    }

    /// Look up the recorded replication status for `(bucket, key)`.
    /// Returns `None` when no PUT to this key has triggered
    /// replication (= the object is not under any replication rule, or
    /// it predates the rule's creation).
    #[must_use]
    pub fn lookup_status(&self, bucket: &str, key: &str) -> Option<ReplicationStatus> {
        self.statuses
            .read()
            .expect("replication state RwLock poisoned")
            .get(&(bucket.to_owned(), key.to_owned()))
            .cloned()
    }
}

/// AND of (prefix predicate, every tag pair). An empty / `None` prefix
/// means "any prefix"; an empty tag list means "no tag predicate".
fn filter_matches(
    filter: &ReplicationFilter,
    key: &str,
    object_tags: &[(String, String)],
) -> bool {
    if let Some(p) = filter.prefix.as_deref()
        && !p.is_empty()
        && !key.starts_with(p)
    {
        return false;
    }
    for (tk, tv) in &filter.tags {
        if !object_tags
            .iter()
            .any(|(ok, ov)| ok == tk && ov == tv)
        {
            return false;
        }
    }
    true
}

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_MS: u64 = 50;

/// Replicate one source-bucket object to the rule's destination bucket.
///
/// The caller supplies a `do_put` callback that performs the actual
/// destination-bucket PUT (so unit tests can drive the dispatcher
/// without needing a full backend). The callback receives:
/// `(destination_bucket, key, body, metadata)` and returns a
/// `Result<(), String>` whose `Err` triggers the retry / failure path.
///
/// Behaviour:
/// - Stamps the destination metadata with `x-amz-replication-status:
///   REPLICA` so a HEAD on the replica is distinguishable.
/// - On callback success, records `(source_bucket, source_key) →
///   Completed` in the manager.
/// - On callback failure, retries up to [`RETRY_ATTEMPTS`] times with
///   exponential backoff (50ms / 100ms / 200ms). After the budget is
///   exhausted, records `Failed`, bumps `dropped_total`, and emits the
///   matching Prometheus counter.
pub async fn replicate_object<F, Fut>(
    rule: ReplicationRule,
    source_bucket: String,
    source_key: String,
    body: bytes::Bytes,
    metadata: Option<HashMap<String, String>>,
    do_put: F,
    manager: Arc<ReplicationManager>,
) where
    F: Fn(String, String, bytes::Bytes, Option<HashMap<String, String>>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // Replica metadata = source metadata + `x-amz-replication-status:
    // REPLICA` stamp. Keeping the source's compression / encryption
    // metadata intact means a GET on the replica decodes through the
    // same path the source would.
    let mut replica_meta = metadata.unwrap_or_default();
    replica_meta.insert(
        "x-amz-replication-status".to_owned(),
        ReplicationStatus::Replica.as_aws_str().to_owned(),
    );
    if let Some(ref sc) = rule.destination_storage_class {
        replica_meta.insert("x-amz-storage-class".to_owned(), sc.clone());
    }

    let dest_bucket = rule.destination_bucket.clone();
    for attempt in 0..RETRY_ATTEMPTS {
        let result = do_put(
            dest_bucket.clone(),
            source_key.clone(),
            body.clone(),
            Some(replica_meta.clone()),
        )
        .await;
        match result {
            Ok(()) => {
                manager.record_status(
                    &source_bucket,
                    &source_key,
                    ReplicationStatus::Completed,
                );
                crate::metrics::record_replication_replicated(&source_bucket, &dest_bucket);
                tracing::debug!(
                    source_bucket = %source_bucket,
                    source_key = %source_key,
                    dest_bucket = %dest_bucket,
                    rule_id = %rule.id,
                    "S4 replication: COMPLETED"
                );
                return;
            }
            Err(e) => {
                if attempt + 1 < RETRY_ATTEMPTS {
                    let delay_ms = RETRY_BASE_MS * (1u64 << attempt);
                    tracing::warn!(
                        source_bucket = %source_bucket,
                        source_key = %source_key,
                        dest_bucket = %dest_bucket,
                        attempt = attempt + 1,
                        error = %e,
                        "S4 replication: attempt failed, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
                manager.record_status(
                    &source_bucket,
                    &source_key,
                    ReplicationStatus::Failed,
                );
                manager.dropped_total.fetch_add(1, Ordering::Relaxed);
                crate::metrics::record_replication_drop(&source_bucket);
                tracing::warn!(
                    source_bucket = %source_bucket,
                    source_key = %source_key,
                    dest_bucket = %dest_bucket,
                    rule_id = %rule.id,
                    error = %e,
                    "S4 replication: FAILED after {RETRY_ATTEMPTS} attempts (drop counter bumped)"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn rule(
        id: &str,
        priority: u32,
        enabled: bool,
        prefix: Option<&str>,
        tags: &[(&str, &str)],
        dest: &str,
    ) -> ReplicationRule {
        ReplicationRule {
            id: id.to_owned(),
            priority,
            status_enabled: enabled,
            filter: ReplicationFilter {
                prefix: prefix.map(str::to_owned),
                tags: tags
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            },
            destination_bucket: dest.to_owned(),
            destination_storage_class: None,
        }
    }

    #[test]
    fn match_rule_prefix_filter_match_and_miss() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn:aws:iam::000:role/s4-test".into(),
                rules: vec![rule("r1", 1, true, Some("logs/"), &[], "dst")],
            },
        );
        assert!(mgr.match_rule("src", "logs/2026/01/01.log", &[]).is_some());
        assert!(mgr.match_rule("src", "uploads/foo.bin", &[]).is_none());
    }

    #[test]
    fn match_rule_no_config_for_bucket() {
        let mgr = ReplicationManager::new();
        assert!(mgr.match_rule("ghost", "k", &[]).is_none());
    }

    #[test]
    fn match_rule_priority_picks_highest() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![
                    rule("low", 1, true, Some(""), &[], "dst-low"),
                    rule("high", 10, true, Some(""), &[], "dst-high"),
                    rule("mid", 5, true, Some(""), &[], "dst-mid"),
                ],
            },
        );
        let picked = mgr.match_rule("src", "any.bin", &[]).expect("match");
        assert_eq!(picked.id, "high");
        assert_eq!(picked.destination_bucket, "dst-high");
    }

    #[test]
    fn match_rule_priority_tie_breaker_is_declaration_order() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![
                    rule("first", 5, true, Some(""), &[], "dst-first"),
                    rule("second", 5, true, Some(""), &[], "dst-second"),
                ],
            },
        );
        let picked = mgr.match_rule("src", "k", &[]).expect("match");
        assert_eq!(picked.id, "first", "tie on priority must keep the earlier rule");
    }

    #[test]
    fn match_rule_tag_filter_and_of_all_tags() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![rule(
                    "r-tags",
                    1,
                    true,
                    None,
                    &[("env", "prod"), ("tier", "gold")],
                    "dst",
                )],
            },
        );
        // Both tags present → match.
        assert!(
            mgr.match_rule(
                "src",
                "k",
                &[
                    ("env".into(), "prod".into()),
                    ("tier".into(), "gold".into()),
                    ("extra".into(), "ignored".into())
                ]
            )
            .is_some(),
            "all required tags present (extras OK) must match"
        );
        // Only one tag → AND fails.
        assert!(
            mgr.match_rule(
                "src",
                "k",
                &[("env".into(), "prod".into())]
            )
            .is_none(),
            "missing one of the required tags must not match"
        );
        // Wrong tag value → AND fails.
        assert!(
            mgr.match_rule(
                "src",
                "k",
                &[
                    ("env".into(), "dev".into()),
                    ("tier".into(), "gold".into())
                ]
            )
            .is_none(),
            "wrong value on a required tag must not match"
        );
    }

    #[test]
    fn match_rule_status_disabled_never_matches() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![rule("disabled", 100, false, None, &[], "dst")],
            },
        );
        assert!(
            mgr.match_rule("src", "anything", &[]).is_none(),
            "status_enabled=false must not match even at high priority"
        );
    }

    #[test]
    fn record_and_lookup_status_round_trip() {
        let mgr = ReplicationManager::new();
        assert!(mgr.lookup_status("b", "k").is_none());
        mgr.record_status("b", "k", ReplicationStatus::Pending);
        assert_eq!(
            mgr.lookup_status("b", "k"),
            Some(ReplicationStatus::Pending)
        );
        mgr.record_status("b", "k", ReplicationStatus::Completed);
        assert_eq!(
            mgr.lookup_status("b", "k"),
            Some(ReplicationStatus::Completed)
        );
    }

    #[test]
    fn json_round_trip_preserves_config_and_statuses() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "src",
            ReplicationConfig {
                role: "arn:aws:iam::000:role/s4".into(),
                rules: vec![rule("r1", 7, true, Some("docs/"), &[("env", "prod")], "dst")],
            },
        );
        mgr.record_status("src", "docs/a.pdf", ReplicationStatus::Completed);
        let json = mgr.to_json().expect("to_json");
        let mgr2 = ReplicationManager::from_json(&json).expect("from_json");
        assert_eq!(mgr.get("src"), mgr2.get("src"));
        assert_eq!(
            mgr2.lookup_status("src", "docs/a.pdf"),
            Some(ReplicationStatus::Completed)
        );
    }

    #[test]
    fn delete_is_idempotent() {
        let mgr = ReplicationManager::new();
        mgr.delete("never-existed");
        mgr.put(
            "b",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![rule("r1", 1, true, None, &[], "dst")],
            },
        );
        mgr.delete("b");
        assert!(mgr.get("b").is_none());
    }

    #[test]
    fn put_replaces_previous_config() {
        let mgr = ReplicationManager::new();
        mgr.put(
            "b",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![rule("old", 1, true, None, &[], "dst-old")],
            },
        );
        mgr.put(
            "b",
            ReplicationConfig {
                role: "arn".into(),
                rules: vec![rule("new", 1, true, None, &[], "dst-new")],
            },
        );
        let cfg = mgr.get("b").expect("config");
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].id, "new");
        assert_eq!(cfg.rules[0].destination_bucket, "dst-new");
    }

    #[tokio::test]
    async fn replicate_object_happy_path_marks_completed() {
        type Captured = Vec<(String, String, bytes::Bytes, Option<HashMap<String, String>>)>;
        let mgr = Arc::new(ReplicationManager::new());
        let captured: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Vec::new()));
        let captured_cl = Arc::clone(&captured);

        let do_put = move |dest: String,
                           key: String,
                           body: bytes::Bytes,
                           meta: Option<HashMap<String, String>>| {
            let captured = Arc::clone(&captured_cl);
            async move {
                captured.lock().unwrap().push((dest, key, body, meta));
                Ok::<(), String>(())
            }
        };

        replicate_object(
            rule("r1", 1, true, None, &[], "dst"),
            "src".into(),
            "obj.bin".into(),
            bytes::Bytes::from_static(b"hello"),
            Some(HashMap::from([("content-type".into(), "text/plain".into())])),
            do_put,
            Arc::clone(&mgr),
        )
        .await;

        assert_eq!(
            mgr.lookup_status("src", "obj.bin"),
            Some(ReplicationStatus::Completed)
        );
        assert_eq!(mgr.dropped_total.load(Ordering::Relaxed), 0);
        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 1, "do_put must run exactly once on success");
        assert_eq!(cap[0].0, "dst");
        assert_eq!(cap[0].1, "obj.bin");
        assert_eq!(cap[0].2.as_ref(), b"hello");
        let meta = cap[0].3.as_ref().expect("metadata stamped");
        assert_eq!(
            meta.get("x-amz-replication-status").map(String::as_str),
            Some("REPLICA"),
            "destination meta must carry the REPLICA stamp"
        );
        assert_eq!(meta.get("content-type").map(String::as_str), Some("text/plain"));
    }

    #[tokio::test]
    async fn replicate_object_failure_after_retry_budget_marks_failed_and_bumps_drop() {
        let mgr = Arc::new(ReplicationManager::new());
        let attempts: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let attempts_cl = Arc::clone(&attempts);

        let do_put = move |_dest: String,
                           _key: String,
                           _body: bytes::Bytes,
                           _meta: Option<HashMap<String, String>>| {
            let attempts = Arc::clone(&attempts_cl);
            async move {
                *attempts.lock().unwrap() += 1;
                Err::<(), String>("simulated destination 5xx".into())
            }
        };

        replicate_object(
            rule("r-fail", 1, true, None, &[], "dst"),
            "src".into(),
            "doomed.bin".into(),
            bytes::Bytes::from_static(b"x"),
            None,
            do_put,
            Arc::clone(&mgr),
        )
        .await;

        assert_eq!(
            *attempts.lock().unwrap(),
            RETRY_ATTEMPTS,
            "must retry exactly the configured budget"
        );
        assert_eq!(
            mgr.lookup_status("src", "doomed.bin"),
            Some(ReplicationStatus::Failed)
        );
        assert_eq!(
            mgr.dropped_total.load(Ordering::Relaxed),
            1,
            "drop counter must bump exactly once after retry budget exhausted"
        );
    }

    #[test]
    fn replication_status_aws_strings_match_spec() {
        assert_eq!(ReplicationStatus::Pending.as_aws_str(), "PENDING");
        assert_eq!(ReplicationStatus::Completed.as_aws_str(), "COMPLETED");
        assert_eq!(ReplicationStatus::Failed.as_aws_str(), "FAILED");
        assert_eq!(ReplicationStatus::Replica.as_aws_str(), "REPLICA");
    }
}
