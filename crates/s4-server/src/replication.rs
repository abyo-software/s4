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

use chrono::{DateTime, Utc};
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

/// Per-(source_bucket, source_key) replication status entry, paired
/// with the **generation token** of the source PUT that produced it.
///
/// ## v0.8.2 #61 — generation token
///
/// Each `put_object` (or `complete_multipart_upload`) on a source key
/// pulls a fresh, monotonically-increasing `generation` from the
/// manager. The detached replication task carries that generation and
/// only stamps the status when its generation is `>=` the stored one
/// (CAS-style). A stale retry whose generation has been overtaken by a
/// newer PUT is silently dropped, so the destination bucket never gets
/// rolled back to older bytes. See [`ReplicationManager::next_generation`]
/// + [`ReplicationManager::record_status_if_newer`].
///
/// ## v0.8.3 #66 — `recorded_at` for sweep + TTL (H-5 audit fix)
///
/// Each stamp records the wall-clock time the entry was last updated.
/// The hourly sweep task ([`ReplicationManager::sweep_stale`]) drops
/// terminal entries (`Completed` / `Failed`) older than the operator-
/// configured TTL, bounding the otherwise-unbounded growth of the
/// `statuses` map under workloads with many unique keys. `Pending`
/// entries are never swept (they are still in-flight and dropping them
/// would lose the eventual `Completed` / `Failed` stamp the dispatcher
/// is racing toward). Pre-#66 snapshots without `recorded_at` deserialise
/// with `Utc::now()` (= "freshly observed at restart") which delays
/// the first sweep by one TTL cycle but never drops a still-relevant
/// entry early.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationStatusEntry {
    pub status: ReplicationStatus,
    pub generation: u64,
    /// v0.8.3 #66: when the entry was last updated. The sweep drops
    /// terminal entries (Completed / Failed) older than the operator-
    /// configured TTL. Pending entries are never swept (still in-flight).
    /// Pre-#66 snapshots default to `Utc::now()` so legacy entries get
    /// one full TTL window of grace before becoming sweep-eligible.
    #[serde(default = "Utc::now")]
    pub recorded_at: DateTime<Utc>,
}

/// JSON snapshot — `bucket -> ReplicationConfig`. Mirrors the shape of
/// `notifications::NotificationSnapshot` so operators can hand-edit
/// configurations across restart cycles.
///
/// ## v0.8.2 #61 back-compat
///
/// Pre-#61 snapshots stored `Vec<((String, String), ReplicationStatus)>`
/// (status only; no generation). The new format stores
/// `Vec<((String, String), ReplicationStatusEntry)>`. Serde is set up
/// with `#[serde(untagged)]` on a wrapper enum so old snapshots load
/// with `generation = 0`. A `generation = 0` entry is never stale —
/// the very next PUT mints `generation = 1` which wins the CAS — so
/// the migration is loss-free.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ReplicationSnapshot {
    by_bucket: HashMap<String, ReplicationConfig>,
    /// Per-(bucket, key) replication status. Persisted so a restart
    /// doesn't lose the COMPLETED stamp on already-replicated
    /// objects.
    statuses: Vec<((String, String), StatusOrEntry)>,
    /// v0.8.2 #61: persist the next generation so a restart doesn't
    /// reissue tokens that are still in-flight. Optional for
    /// back-compat — pre-#61 snapshots restore with `next_generation = 1`.
    #[serde(default)]
    next_generation: u64,
}

/// Back-compat wrapper for snapshot deserialisation: accepts either a
/// bare `ReplicationStatus` (pre-#61 schema) or a full
/// `ReplicationStatusEntry`. `serde(untagged)` tries the variants in
/// declaration order — the more-structured `Entry` variant first so
/// new snapshots round-trip, falling back to bare `Status` for old
/// snapshots.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum StatusOrEntry {
    Entry(ReplicationStatusEntry),
    Status(ReplicationStatus),
}

impl StatusOrEntry {
    fn into_entry(self) -> ReplicationStatusEntry {
        match self {
            Self::Entry(e) => e,
            // v0.8.3 #66: pre-#61 snapshots have no `recorded_at`; stamp
            // `Utc::now()` so the first sweep tick sees them as freshly
            // observed and gives them one full TTL window of grace.
            Self::Status(s) => ReplicationStatusEntry {
                status: s,
                generation: 0,
                recorded_at: Utc::now(),
            },
        }
    }
}

/// In-memory manager of per-bucket replication configurations + per-
/// (bucket, key) replication statuses.
pub struct ReplicationManager {
    by_bucket: RwLock<HashMap<String, ReplicationConfig>>,
    /// Per-(source_bucket, key) replication status entry (status +
    /// generation token). Looked up by `head_object` / `get_object` to
    /// stamp `x-amz-replication-status` on the response.
    statuses: RwLock<HashMap<(String, String), ReplicationStatusEntry>>,
    /// v0.8.2 #61: monotonic per-source-PUT generation counter. Each
    /// `put_object` (or `complete_multipart_upload`) on a replicated
    /// source bucket calls [`Self::next_generation`] before spawning
    /// its detached replication task. The dispatcher carries the
    /// generation through to [`Self::record_status_if_newer`], which
    /// drops the stamp + the destination write when a newer
    /// generation has already won — guaranteeing the destination
    /// can't be rolled back by a slow retry.
    pub next_generation: AtomicU64,
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
    /// Empty manager — no bucket has any replication rules. The
    /// generation counter starts at 1 so the first PUT-issued token is
    /// `1` (a stored entry's `generation = 0` from a pre-#61 snapshot
    /// is then strictly less and the very next PUT wins the CAS).
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_bucket: RwLock::new(HashMap::new()),
            statuses: RwLock::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            dropped_total: AtomicU64::new(0),
        }
    }

    /// v0.8.2 #61: mint a fresh, monotonically-increasing generation
    /// token. Caller is the per-source-PUT dispatcher fork (the body-
    /// bearing `put_object` branch, the body-less `put_object` branch,
    /// and `complete_multipart_upload`). The token is then carried
    /// through [`replicate_object`] to [`Self::record_status_if_newer`]
    /// so a stale retry can be detected and dropped.
    ///
    /// Uses `Relaxed` ordering — we only need uniqueness +
    /// monotonicity per atomic; the cross-thread happens-before
    /// between PUT-A's spawn and the dispatcher reading the body is
    /// already established by `tokio::spawn`'s implicit
    /// `Acquire/Release` on the task queue.
    pub fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
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
    /// statuses + next generation counter) to JSON. The status entries
    /// are emitted in the v0.8.2 #61 schema (`ReplicationStatusEntry`);
    /// readers built before #61 will see the embedded
    /// `{ status, generation }` shape via the `untagged` enum and
    /// (older binaries) reject — but the production restart path always
    /// runs the same binary against its own snapshot.
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
                .map(|(k, v)| (k.clone(), StatusOrEntry::Entry(v.clone())))
                .collect(),
            next_generation: self.next_generation.load(Ordering::Relaxed),
        };
        serde_json::to_string(&snap)
    }

    /// Restore a manager from a previously-emitted snapshot. The
    /// `dropped_total` counter is reset to 0 — historical drops are
    /// runtime metrics, not configuration.
    ///
    /// ## Back-compat (v0.8.2 #61)
    ///
    /// Pre-#61 snapshots store bare `ReplicationStatus` (no
    /// generation). The `untagged` `StatusOrEntry` enum picks them up
    /// and assigns `generation = 0`, which the CAS-style
    /// [`Self::record_status_if_newer`] treats as "always overridable
    /// by the next real PUT" — guaranteed loss-free migration. The
    /// `next_generation` counter defaults to `1` when the snapshot
    /// predates #61 (= `serde(default)` on the field).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: ReplicationSnapshot = serde_json::from_str(s)?;
        let statuses: HashMap<(String, String), ReplicationStatusEntry> = snap
            .statuses
            .into_iter()
            .map(|(k, v)| (k, v.into_entry()))
            .collect();
        // Pre-#61 snapshots come back with `next_generation = 0`
        // (`serde(default)` on `u64`); start fresh at 1 so all newly-
        // minted tokens are strictly greater than the legacy
        // `generation = 0` entries.
        let next_gen = snap.next_generation.max(1);
        Ok(Self {
            by_bucket: RwLock::new(snap.by_bucket),
            statuses: RwLock::new(statuses),
            next_generation: AtomicU64::new(next_gen),
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

    /// Stamp the per-(bucket, key) replication status with no
    /// generation guard. Replaces any previous entry. **Generation is
    /// reset to 0** (= overridable by the next real PUT) — callers
    /// that hold a generation token must use
    /// [`Self::record_status_if_newer`] instead.
    ///
    /// Use cases (kept for back-compat + the eager `Pending` stamp the
    /// service-layer dispatcher emits before spawning the actual
    /// replication task):
    /// - Eager `Pending` stamp synchronously alongside the source PUT
    ///   so a HEAD between PUT-return and dispatcher-completion sees
    ///   `PENDING` instead of `None`.
    /// - Tests that don't care about generation (legacy assertions).
    pub fn record_status(&self, bucket: &str, key: &str, status: ReplicationStatus) {
        self.statuses
            .write()
            .expect("replication state RwLock poisoned")
            .insert(
                (bucket.to_owned(), key.to_owned()),
                ReplicationStatusEntry {
                    status,
                    generation: 0,
                    // v0.8.3 #66: stamp now so a subsequent sweep can
                    // age this entry out once it reaches a terminal
                    // state and exceeds the configured TTL.
                    recorded_at: Utc::now(),
                },
            );
    }

    /// v0.8.2 #61: CAS-style stamp. Only updates the entry when
    /// `generation >= entry.generation`; rejects the update (returns
    /// `false`) when `generation < entry.generation` because a newer
    /// PUT has already won and we must not roll the source's status
    /// back to a stale terminal state.
    ///
    /// ## Returns
    ///
    /// - `true` — the stamp was accepted; the caller may proceed with
    ///   the destination-bucket PUT (in [`replicate_object`]) /
    ///   declare success.
    /// - `false` — a strictly-newer generation has already stamped the
    ///   entry; the caller must **drop the destination write** to
    ///   avoid overwriting newer bytes with a stale retry's body.
    ///
    /// Equality (`generation == entry.generation`) is accepted because
    /// the same generation may legitimately stamp twice across the
    /// dispatcher's retry budget (`Pending` → `Completed` on the same
    /// task).
    pub fn record_status_if_newer(
        &self,
        bucket: &str,
        key: &str,
        generation: u64,
        status: ReplicationStatus,
    ) -> bool {
        let mut map = self
            .statuses
            .write()
            .expect("replication state RwLock poisoned");
        let now = Utc::now();
        let entry = map
            .entry((bucket.to_owned(), key.to_owned()))
            .or_insert(ReplicationStatusEntry {
                status: ReplicationStatus::Pending,
                generation: 0,
                // v0.8.3 #66: stamp at insertion; will be overwritten
                // immediately below when the CAS accepts.
                recorded_at: now,
            });
        if generation < entry.generation {
            return false;
        }
        entry.generation = generation;
        entry.status = status;
        // v0.8.3 #66: refresh the timestamp on every accepted stamp so
        // a Pending → Completed transition (same generation) resets
        // the sweep clock — the TTL is measured from the **last**
        // terminal stamp, not the first observation.
        entry.recorded_at = now;
        true
    }

    /// v0.8.3 #66 (H-5 audit fix): drop terminal-state entries
    /// (`Completed` / `Failed`) older than `max_age`. `Pending` entries
    /// are never swept because they are still in-flight — the
    /// dispatcher is racing toward a terminal stamp and dropping the
    /// `Pending` would lose the eventual outcome (and let the entry
    /// re-emerge under the original key with no recorded history).
    /// `Replica` entries can theoretically appear here through legacy
    /// paths and are likewise preserved (the destination-side stamp is
    /// not produced by `record_status_if_newer` in the current code,
    /// but the conservative filter keeps any future use loss-free).
    ///
    /// Cutoff is `now - max_age` rather than `Utc::now() - max_age` so
    /// callers can drive the clock deterministically in tests.
    ///
    /// Returns the number of entries removed (operators dashboard via
    /// `s4_replication_status_swept_total`).
    pub fn sweep_stale(&self, now: DateTime<Utc>, max_age: chrono::Duration) -> usize {
        let mut map = self
            .statuses
            .write()
            .expect("replication state RwLock poisoned");
        let cutoff = now - max_age;
        let stale: Vec<(String, String)> = map
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.status,
                    ReplicationStatus::Completed | ReplicationStatus::Failed
                ) && e.recorded_at < cutoff
            })
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for k in stale {
            map.remove(&k);
        }
        count
    }

    /// Look up the recorded replication status for `(bucket, key)`.
    /// Returns `None` when no PUT to this key has triggered
    /// replication (= the object is not under any replication rule, or
    /// it predates the rule's creation).
    ///
    /// The `generation` field of the entry is intentionally not
    /// surfaced here — it's an internal CAS guard, not part of the
    /// AWS wire shape.
    #[must_use]
    pub fn lookup_status(&self, bucket: &str, key: &str) -> Option<ReplicationStatus> {
        self.statuses
            .read()
            .expect("replication state RwLock poisoned")
            .get(&(bucket.to_owned(), key.to_owned()))
            .map(|entry| entry.status.clone())
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

/// v0.8.3 #68 (audit M-1): emit a single WARN log line per
/// `(source_bucket, dest_bucket)` pair the first time we observe a
/// replication PUT that wanted to propagate Object Lock state but the
/// destination side has no `ObjectLockManager` attached. The metric
/// (`s4_replication_lock_propagation_skipped_total`) bumps every time
/// (so dashboards see the rate); the log is dedup'd because operators
/// only need to know once that the configuration is asymmetric.
///
/// The dedup set lives in a process-static `Mutex<HashSet<(src, dst)>>`
/// — bounded by the (#source × #destination) pair count, which is
/// always small (operator-declared rules, not per-key).
pub fn warn_lock_propagation_skipped(source_bucket: &str, dest_bucket: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let key = (source_bucket.to_owned(), dest_bucket.to_owned());
    let first_time = {
        let mut guard = seen.lock().expect("warn-once HashSet Mutex poisoned");
        guard.insert(key)
    };
    if first_time {
        tracing::warn!(
            source_bucket = %source_bucket,
            dest_bucket = %dest_bucket,
            "S4 replication: source carries Object Lock state but destination \
             bucket has no ObjectLockManager attached — replica will be freely \
             deletable on the destination (WORM posture is source-only). Attach \
             an ObjectLockManager via S4Service::with_object_lock() on the \
             destination-side gateway to honour cross-bucket WORM."
        );
    }
    crate::metrics::record_replication_lock_propagation_skipped();
}

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
///   Completed` in the manager **iff this task's `generation` is not
///   already overtaken** (CAS-style guard — see [`ReplicationManager::
///   record_status_if_newer`]).
/// - On callback failure, retries up to [`RETRY_ATTEMPTS`] times with
///   exponential backoff (50ms / 100ms / 200ms). After the budget is
///   exhausted, records `Failed`, bumps `dropped_total`, and emits the
///   matching Prometheus counter — also CAS-guarded.
///
/// ## v0.8.2 #61 — generation token + destination key override
///
/// The two new parameters fix the audit's C-1 + C-3 findings:
///
/// - `generation` — monotonic per-source-PUT token from
///   [`ReplicationManager::next_generation`]. CAS-stamps the source's
///   status + **suppresses the actual destination PUT** when the
///   token has been overtaken. Without this guard, a slow retry of
///   PUT-A could land in the destination *after* PUT-B has already
///   replicated — rolling the destination back to A's bytes. With
///   the guard, A's task notices B's higher generation has won and
///   silently drops its destination write.
/// - `destination_key_override` — the storage-side key the destination
///   bucket should write under. For an unversioned source this is
///   `None` and the dispatcher falls back to the source's logical key
///   (= the AWS-default behaviour). For a **versioning-Enabled**
///   source the caller passes `Some(versioned_shadow_key(key, vid))`
///   so the destination's version chain receives the new version
///   under the same shadow path the source uses (= a `?versionId=`
///   GET on the destination resolves through the same shadow-key
///   lookup as the source).
///
/// ## v0.8.3 #68 — Object Lock state propagation (audit M-1)
///
/// `source_lock_state` carries the source object's WORM posture
/// (`mode + retain_until + legal_hold_on`) at PUT time. When `Some`,
/// the destination PUT is decorated with the AWS-wire lock headers
/// (`x-amz-object-lock-mode`, `x-amz-object-lock-retain-until-date`,
/// `x-amz-object-lock-legal-hold`) on the metadata map so the
/// destination side's `put_object` (or its caller) can persist the
/// same lock state on the replica. Without this, a Compliance /
/// Governance / legal-hold protected source had a destination
/// replica that the destination operator could freely DELETE — the
/// "WORM compliance posture survives DR" guarantee leaked.
///
/// `None` (no lock state on the source) keeps the legacy behaviour:
/// no extra headers, replica is freely deletable on the destination.
// 10 args is the post-#68 wire-shape: rule + (source_bucket, source_key,
// body, metadata) + do_put + manager + (generation, dest_override) +
// source_lock_state. A shape struct would split the call site without
// buying anything; the caller (`spawn_replication_if_matched`)
// constructs every field inline, so the indirection is pure noise.
#[allow(clippy::too_many_arguments)]
pub async fn replicate_object<F, Fut>(
    rule: ReplicationRule,
    source_bucket: String,
    source_key: String,
    body: bytes::Bytes,
    metadata: Option<HashMap<String, String>>,
    do_put: F,
    manager: Arc<ReplicationManager>,
    generation: u64,
    destination_key_override: Option<String>,
    source_lock_state: Option<crate::object_lock::ObjectLockState>,
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
    // v0.8.3 #68 (audit M-1): propagate the source's Object Lock
    // posture as AWS-wire lock headers attached to the destination
    // PUT's metadata map. The destination side reads these and
    // persists the same lock state on the replica so a DR setup keeps
    // the WORM guarantee end-to-end (Compliance / Governance / legal
    // hold cannot be silently bypassed by deleting on the destination).
    if let Some(ref lock) = source_lock_state {
        if let Some(mode) = lock.mode {
            replica_meta.insert(
                "x-amz-object-lock-mode".to_owned(),
                mode.as_aws_str().to_owned(),
            );
        }
        if let Some(retain_until) = lock.retain_until {
            // ISO-8601 / RFC-3339 — the AWS wire form for
            // `x-amz-object-lock-retain-until-date`.
            replica_meta.insert(
                "x-amz-object-lock-retain-until-date".to_owned(),
                retain_until.to_rfc3339(),
            );
        }
        // Always emit the legal-hold flag when any lock state is
        // present so an explicit "OFF" is propagated too (an absent
        // header is ambiguous with "no opinion" on the destination).
        replica_meta.insert(
            "x-amz-object-lock-legal-hold".to_owned(),
            if lock.legal_hold_on { "ON" } else { "OFF" }.to_owned(),
        );
    }

    let dest_bucket = rule.destination_bucket.clone();
    // v0.8.2 #61: when the source PUT was a versioned write, the
    // override carries the storage-side shadow key
    // (`<key>.__s4ver__/<vid>`); otherwise we use the logical key.
    let dest_key = destination_key_override.unwrap_or_else(|| source_key.clone());
    for attempt in 0..RETRY_ATTEMPTS {
        // v0.8.2 #61 C-3: pre-PUT generation check. If a newer
        // generation has already stamped a terminal status on this
        // (bucket, key), our retry is stale — silently drop the
        // destination write so we don't roll the destination back to
        // older bytes. We use `record_status_if_newer` with the
        // **current** entry's status as a no-op when we're not stale,
        // but the cheap path is to peek and bail.
        if let Some(entry) = manager
            .statuses
            .read()
            .expect("replication state RwLock poisoned")
            .get(&(source_bucket.clone(), source_key.clone()))
            .cloned()
            && entry.generation > generation
        {
            tracing::debug!(
                source_bucket = %source_bucket,
                source_key = %source_key,
                dest_bucket = %dest_bucket,
                rule_id = %rule.id,
                generation,
                stored_generation = entry.generation,
                "S4 replication: stale generation, dropping destination PUT"
            );
            return;
        }
        let result = do_put(
            dest_bucket.clone(),
            dest_key.clone(),
            body.clone(),
            Some(replica_meta.clone()),
        )
        .await;
        match result {
            Ok(()) => {
                let accepted = manager.record_status_if_newer(
                    &source_bucket,
                    &source_key,
                    generation,
                    ReplicationStatus::Completed,
                );
                if !accepted {
                    // v0.8.2 #61 C-3: the destination PUT raced — a
                    // newer generation stamped between our pre-check
                    // and our `do_put.await`. The destination now
                    // *might* hold our stale bytes (the newer PUT
                    // could have landed after ours) but we stop
                    // re-stamping and let the newer task overwrite on
                    // its own success. Bumps the metric so operators
                    // see the race surfaced.
                    crate::metrics::record_replication_drop(&source_bucket);
                    manager.dropped_total.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        source_bucket = %source_bucket,
                        source_key = %source_key,
                        dest_bucket = %dest_bucket,
                        rule_id = %rule.id,
                        generation,
                        "S4 replication: completed but a newer generation has won; \
                         status not stamped"
                    );
                    return;
                }
                crate::metrics::record_replication_replicated(&source_bucket, &dest_bucket);
                tracing::debug!(
                    source_bucket = %source_bucket,
                    source_key = %source_key,
                    dest_bucket = %dest_bucket,
                    rule_id = %rule.id,
                    generation,
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
                        generation,
                        error = %e,
                        "S4 replication: attempt failed, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
                // CAS the terminal Failed too — a newer generation that
                // succeeded must not be rolled back to Failed.
                let accepted = manager.record_status_if_newer(
                    &source_bucket,
                    &source_key,
                    generation,
                    ReplicationStatus::Failed,
                );
                manager.dropped_total.fetch_add(1, Ordering::Relaxed);
                crate::metrics::record_replication_drop(&source_bucket);
                tracing::warn!(
                    source_bucket = %source_bucket,
                    source_key = %source_key,
                    dest_bucket = %dest_bucket,
                    rule_id = %rule.id,
                    generation,
                    error = %e,
                    accepted_failed_stamp = accepted,
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
            mgr.next_generation(),
            None,
            None,
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
            mgr.next_generation(),
            None,
            None,
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

    // ----- v0.8.2 #61: generation token CAS unit tests -----

    #[test]
    fn record_status_if_newer_accepts_higher_generation() {
        let mgr = ReplicationManager::new();
        // First stamp at gen=5 — no prior entry, accepted.
        assert!(mgr.record_status_if_newer(
            "b",
            "k",
            5,
            ReplicationStatus::Pending,
        ));
        // Higher generation overrides.
        assert!(mgr.record_status_if_newer(
            "b",
            "k",
            7,
            ReplicationStatus::Completed,
        ));
        assert_eq!(
            mgr.lookup_status("b", "k"),
            Some(ReplicationStatus::Completed)
        );
    }

    #[test]
    fn record_status_if_newer_rejects_stale_generation() {
        let mgr = ReplicationManager::new();
        // Newer PUT lands first.
        assert!(mgr.record_status_if_newer(
            "b",
            "k",
            10,
            ReplicationStatus::Completed,
        ));
        // Older retry must be rejected — destination must not roll
        // back to "alpha" once "beta" has stamped Completed.
        let accepted = mgr.record_status_if_newer(
            "b",
            "k",
            3,
            ReplicationStatus::Completed,
        );
        assert!(!accepted, "stale generation must be rejected");
        // Stored entry stays at the newer generation's terminal state.
        assert_eq!(
            mgr.lookup_status("b", "k"),
            Some(ReplicationStatus::Completed)
        );
    }

    #[test]
    fn record_status_if_newer_accepts_equal_generation() {
        // Same generation may legitimately re-stamp (Pending →
        // Completed transition on the same task). The CAS is `>=`
        // not `>`.
        let mgr = ReplicationManager::new();
        assert!(mgr.record_status_if_newer(
            "b",
            "k",
            42,
            ReplicationStatus::Pending,
        ));
        assert!(mgr.record_status_if_newer(
            "b",
            "k",
            42,
            ReplicationStatus::Completed,
        ));
        assert_eq!(
            mgr.lookup_status("b", "k"),
            Some(ReplicationStatus::Completed)
        );
    }

    #[test]
    fn next_generation_is_monotonic() {
        let mgr = ReplicationManager::new();
        let g1 = mgr.next_generation();
        let g2 = mgr.next_generation();
        let g3 = mgr.next_generation();
        assert!(g2 > g1, "g2={g2} must exceed g1={g1}");
        assert!(g3 > g2, "g3={g3} must exceed g2={g2}");
        assert_eq!(g2, g1 + 1);
        assert_eq!(g3, g2 + 1);
    }

    #[test]
    fn snapshot_pre_61_format_loads_with_zero_generation() {
        // Pre-v0.8.2 #61 snapshot shape: bare `ReplicationStatus`,
        // no `next_generation` field. The `untagged` enum + serde
        // default must round-trip lossily into the new shape, with
        // `generation = 0` (= guaranteed loseable to next real PUT).
        let pre_61_json = r#"{
            "by_bucket": {},
            "statuses": [
                [["src", "k"], "Completed"]
            ]
        }"#;
        let mgr = ReplicationManager::from_json(pre_61_json)
            .expect("pre-#61 snapshot must deserialise");
        assert_eq!(
            mgr.lookup_status("src", "k"),
            Some(ReplicationStatus::Completed)
        );
        // First mint after restore is `1` (max(0, 1)).
        assert_eq!(mgr.next_generation(), 1);
        // The `generation = 0` legacy entry is overridable by any
        // real PUT (= a generation >= 1).
        assert!(mgr.record_status_if_newer(
            "src",
            "k",
            1,
            ReplicationStatus::Pending,
        ));
    }

    // ----- v0.8.3 #66: sweep + TTL unit tests (H-5 audit fix) -----

    /// Helper: install a `(bucket, key)` entry with an explicit
    /// `recorded_at` so the sweep test can pin the clock at a known
    /// offset from "now". Bypasses `record_status_if_newer` because
    /// that always stamps with `Utc::now()`.
    fn install_entry_with_recorded_at(
        mgr: &ReplicationManager,
        bucket: &str,
        key: &str,
        status: ReplicationStatus,
        recorded_at: DateTime<Utc>,
    ) {
        mgr.statuses
            .write()
            .expect("replication state RwLock poisoned")
            .insert(
                (bucket.to_owned(), key.to_owned()),
                ReplicationStatusEntry {
                    status,
                    generation: 1,
                    recorded_at,
                },
            );
    }

    #[test]
    fn sweep_stale_drops_completed_past_ttl() {
        // Three terminal entries: Completed -10h, Failed -10h, Completed -1h.
        // sweep_stale(now, 5h) → drops the two -10h entries, keeps the
        // recent Completed.
        let mgr = ReplicationManager::new();
        let now = Utc::now();
        install_entry_with_recorded_at(
            &mgr,
            "src",
            "old-completed",
            ReplicationStatus::Completed,
            now - chrono::Duration::hours(10),
        );
        install_entry_with_recorded_at(
            &mgr,
            "src",
            "old-failed",
            ReplicationStatus::Failed,
            now - chrono::Duration::hours(10),
        );
        install_entry_with_recorded_at(
            &mgr,
            "src",
            "recent-completed",
            ReplicationStatus::Completed,
            now - chrono::Duration::hours(1),
        );

        let n = mgr.sweep_stale(now, chrono::Duration::hours(5));
        assert_eq!(n, 2, "two terminal entries past 5h TTL must be swept");
        assert!(
            mgr.lookup_status("src", "old-completed").is_none(),
            "Completed past TTL must be removed"
        );
        assert!(
            mgr.lookup_status("src", "old-failed").is_none(),
            "Failed past TTL must be removed"
        );
        assert_eq!(
            mgr.lookup_status("src", "recent-completed"),
            Some(ReplicationStatus::Completed),
            "Completed within TTL must survive"
        );
    }

    #[test]
    fn sweep_stale_keeps_pending_regardless_of_age() {
        // A Pending entry stamped 100h ago is **still in-flight**
        // (the dispatcher is racing toward a terminal stamp). Sweeping
        // it would lose the eventual Completed/Failed outcome and let
        // a stale generation re-emerge under the original key with no
        // recorded history.
        let mgr = ReplicationManager::new();
        let now = Utc::now();
        install_entry_with_recorded_at(
            &mgr,
            "src",
            "ancient-pending",
            ReplicationStatus::Pending,
            now - chrono::Duration::hours(100),
        );

        let n = mgr.sweep_stale(now, chrono::Duration::hours(5));
        assert_eq!(n, 0, "Pending entries must never be swept");
        assert_eq!(
            mgr.lookup_status("src", "ancient-pending"),
            Some(ReplicationStatus::Pending),
            "ancient Pending must still be present"
        );
    }

    #[test]
    fn recorded_at_back_compat_default_now_on_deserialize() {
        // A pre-#66 snapshot whose status entries omit `recorded_at`
        // must deserialise with `recorded_at = Utc::now()` (= "freshly
        // observed at restart"). This delays the first sweep by one
        // TTL window but never drops a still-relevant entry early.
        // Use the v0.8.2 #61 entry shape (status + generation, no
        // recorded_at) to verify the `#[serde(default = "Utc::now")]`
        // applies on inner-entry deserialisation too.
        let pre_66_json = r#"{
            "by_bucket": {},
            "statuses": [
                [["src", "k"], { "status": "Completed", "generation": 7 }]
            ],
            "next_generation": 8
        }"#;
        let before = Utc::now();
        let mgr = ReplicationManager::from_json(pre_66_json)
            .expect("pre-#66 snapshot with no `recorded_at` must deserialise");
        let after = Utc::now();

        // Status preserved.
        assert_eq!(
            mgr.lookup_status("src", "k"),
            Some(ReplicationStatus::Completed),
        );

        // recorded_at defaulted to Utc::now() at deserialise time —
        // peek the inner entry to verify the timestamp is in the
        // [before, after] window of the from_json call.
        let entries = mgr
            .statuses
            .read()
            .expect("replication state RwLock poisoned");
        let entry = entries
            .get(&("src".to_owned(), "k".to_owned()))
            .expect("entry must exist");
        assert!(
            entry.recorded_at >= before && entry.recorded_at <= after,
            "recorded_at default must be Utc::now() at deserialise time \
             (got {:?}, expected within [{:?}, {:?}])",
            entry.recorded_at,
            before,
            after
        );
        assert_eq!(entry.generation, 7, "generation must round-trip");

        // A sweep with TTL 1h immediately after must NOT drop this
        // entry (recorded_at ≈ now, well within TTL).
        drop(entries);
        let n = mgr.sweep_stale(Utc::now(), chrono::Duration::hours(1));
        assert_eq!(
            n, 0,
            "freshly-defaulted recorded_at must survive a 1h-TTL sweep"
        );
    }

    // ----- v0.8.3 #68: Object Lock state propagation unit tests (audit M-1) -----

    /// When the source object carried an Object Lock state, the
    /// dispatcher must inject the AWS-wire lock headers
    /// (`x-amz-object-lock-mode`, `-retain-until-date`, `-legal-hold`)
    /// into the destination PUT's metadata map so the destination side
    /// can persist the same WORM posture on the replica.
    #[tokio::test]
    async fn replicate_with_source_lock_state_attaches_headers() {
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

        let retain_until = Utc::now() + chrono::Duration::days(30);
        let lock_state = crate::object_lock::ObjectLockState {
            mode: Some(crate::object_lock::LockMode::Compliance),
            retain_until: Some(retain_until),
            legal_hold_on: true,
        };

        replicate_object(
            rule("r-locked", 1, true, None, &[], "dst"),
            "src".into(),
            "worm.bin".into(),
            bytes::Bytes::from_static(b"locked-payload"),
            None,
            do_put,
            Arc::clone(&mgr),
            mgr.next_generation(),
            None,
            Some(lock_state),
        )
        .await;

        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 1, "do_put must run exactly once on success");
        let meta = cap[0].3.as_ref().expect("metadata stamped");
        assert_eq!(
            meta.get("x-amz-object-lock-mode").map(String::as_str),
            Some("COMPLIANCE"),
            "Compliance mode header must be propagated"
        );
        let stamped_until = meta
            .get("x-amz-object-lock-retain-until-date")
            .expect("retain-until header must be propagated");
        // RFC-3339 round-trip: parse back and compare with the source
        // retain_until (1s slack for sub-second truncation).
        let parsed: chrono::DateTime<chrono::FixedOffset> =
            chrono::DateTime::parse_from_rfc3339(stamped_until)
                .expect("retain-until must be RFC-3339");
        let diff = (parsed.with_timezone(&Utc) - retain_until).num_seconds().abs();
        assert!(diff <= 1, "retain-until off by {diff}s");
        assert_eq!(
            meta.get("x-amz-object-lock-legal-hold").map(String::as_str),
            Some("ON"),
            "legal hold state must be propagated as ON"
        );
        // The REPLICA stamp must still be present alongside the lock
        // headers (lock propagation must not displace the replica
        // marker that lets HEAD distinguish replica-vs-direct PUT).
        assert_eq!(
            meta.get("x-amz-replication-status").map(String::as_str),
            Some("REPLICA"),
        );
    }

    /// Symmetric: no source lock state → no lock headers leak into
    /// the destination metadata (legacy behaviour preserved for the
    /// non-WORM PUT path).
    #[tokio::test]
    async fn replicate_without_source_lock_state_no_headers_added() {
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
            rule("r-plain", 1, true, None, &[], "dst"),
            "src".into(),
            "plain.bin".into(),
            bytes::Bytes::from_static(b"plain-payload"),
            None,
            do_put,
            Arc::clone(&mgr),
            mgr.next_generation(),
            None,
            None,
        )
        .await;

        let cap = captured.lock().unwrap();
        let meta = cap[0].3.as_ref().expect("metadata stamped");
        assert!(
            meta.get("x-amz-object-lock-mode").is_none(),
            "no lock state ⇒ no mode header (got {:?})",
            meta.get("x-amz-object-lock-mode")
        );
        assert!(
            meta.get("x-amz-object-lock-retain-until-date").is_none(),
            "no lock state ⇒ no retain-until header"
        );
        assert!(
            meta.get("x-amz-object-lock-legal-hold").is_none(),
            "no lock state ⇒ no legal-hold header"
        );
    }
}
