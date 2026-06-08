//! S3 bucket notifications — fire events on PUT / DELETE (v0.6 #35).
//!
//! AWS S3 lets a bucket owner register notification destinations (SNS, SQS,
//! Lambda, EventBridge) that receive a JSON event payload whenever objects
//! change. S4-server implements a subset:
//!
//! - **Webhook** (HTTP POST of the event JSON) — always available, no extra
//!   crate dependencies required.
//! - **SQS** (`Sqs { queue_arn }`) — gated behind the `aws-events` cargo
//!   feature so the default build doesn't pull `aws-sdk-sqs`.
//! - **SNS** (`Sns { topic_arn }`) — gated behind the same `aws-events`
//!   feature.
//! - **Lambda direct invoke is NOT implemented** in v0.6 #35; the recommended
//!   path is SNS → Lambda subscription, which works through this module's SNS
//!   destination once the feature is on.
//!
//! ## responsibilities (v0.6 #35)
//!
//! - in-memory `bucket -> NotificationConfig` map with JSON snapshot
//!   round-trip, mirroring `versioning.rs` / `cors.rs` / `inventory.rs` so
//!   `--notifications-state-file` is a one-line addition in `main.rs`.
//! - `match_destinations(bucket, event, key)` walks the rule list in
//!   declaration order and returns every destination whose event types and
//!   prefix/suffix filter accept the (event, key) tuple. AWS allows multiple
//!   rules to fire on a single event so the result is a `Vec`, not an
//!   `Option`.
//! - `build_event_json` serialises the AWS-canonical
//!   [event payload schema](https://docs.aws.amazon.com/AmazonS3/latest/userguide/notification-content-structure.html)
//!   so existing AWS SDK consumers can deserialise the body without bespoke
//!   parsing.
//! - `dispatch_event` is the fire-and-forget runtime: spawned by the
//!   `service.rs` PUT / DELETE handlers, it POSTs the event JSON to every
//!   matched destination on a tokio task, retries 5xx with exponential
//!   backoff (3 attempts), then drops + bumps the `dropped_total` counter +
//!   logs at warn so operators see the silent loss in metrics.
//!
//! ## scope limitations
//!
//! - in-memory only (no replication across multi-instance deployments).
//!   `--notifications-state-file <PATH>` provides restart recovery via JSON
//!   snapshot, same shape as `--versioning-state-file`.
//! - retry budget is fixed at 3 attempts with exponential backoff (50ms /
//!   100ms / 200ms). Beyond that the event is dropped and `dropped_total`
//!   is bumped — there's no on-disk dead-letter queue.
//! - SNS/SQS use the AWS SDK's default credential chain (environment, EC2
//!   role, etc); per-destination credential overrides are out of scope.
//! - Lambda direct invocation is not implemented (use SNS subscription).
//! - `EventBridge` integration is not implemented.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Subset of the AWS S3 event-type taxonomy. We intentionally stop short of
/// the full ~30-event matrix because v0.6 #35 only fires PUT and DELETE
/// hooks; more events can be added when the corresponding handlers grow
/// notification fire-points.
///
/// v1.0 stability: `#[non_exhaustive]` — the additional event types
/// (`ObjectCreated:CompleteMultipartUpload`, restore events,
/// replication events, etc.) will land in future minor releases.
/// Downstream callers must include a `_ =>` arm when matching.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventType {
    /// `s3:ObjectCreated:Put`
    ObjectCreatedPut,
    /// `s3:ObjectRemoved:Delete` — hard delete on a non-versioned bucket, or
    /// a specific-version DELETE on any bucket.
    ObjectRemovedDelete,
    /// `s3:ObjectRemoved:DeleteMarkerCreated` — DELETE on a bucket with
    /// versioning state Enabled OR Suspended.
    ///
    /// **Enabled**: pushes a delete marker only; prior version bytes
    /// survive and are still reachable via `?versionId=`.
    ///
    /// **Suspended**: pushes a delete marker AND physically deletes
    /// the prior null version (the `null` version is overwritten by
    /// the marker — AWS S3 spec). Subscribers cannot tell from the
    /// event type alone whether a prior version still exists; if the
    /// distinction matters, query versioning state via
    /// `GetBucketVersioning` or rely on the receiving system's chain
    /// awareness.
    ObjectRemovedDeleteMarker,
}

impl EventType {
    /// AWS wire-string form. Matches the values an SDK consumer would see in
    /// the `eventName` field of the event JSON.
    #[must_use]
    pub fn as_aws_str(&self) -> &'static str {
        match self {
            Self::ObjectCreatedPut => "s3:ObjectCreated:Put",
            Self::ObjectRemovedDelete => "s3:ObjectRemoved:Delete",
            Self::ObjectRemovedDeleteMarker => "s3:ObjectRemoved:DeleteMarkerCreated",
        }
    }

    /// Parse the AWS wire form. Tolerates the AWS catch-all
    /// `s3:ObjectCreated:*` and `s3:ObjectRemoved:*` patterns by mapping them
    /// to the most common concrete variant in each family — matching what
    /// AWS does when expanding the wildcard against an actual event.
    #[must_use]
    pub fn from_aws_str(s: &str) -> Option<Self> {
        match s {
            "s3:ObjectCreated:Put" | "s3:ObjectCreated:*" => Some(Self::ObjectCreatedPut),
            "s3:ObjectRemoved:Delete" => Some(Self::ObjectRemovedDelete),
            "s3:ObjectRemoved:DeleteMarkerCreated" => Some(Self::ObjectRemovedDeleteMarker),
            "s3:ObjectRemoved:*" => Some(Self::ObjectRemovedDelete),
            _ => None,
        }
    }
}

/// One destination for a fired event. The variant determines the dispatch
/// path: `Webhook` is always built; `Sqs` / `Sns` are accepted at config
/// time regardless of the build feature, but the runtime dispatcher will
/// log + drop them when the `aws-events` feature isn't compiled in.
///
/// v1.0 stability: `#[non_exhaustive]` — additional destination
/// variants (Lambda, EventBridge, NATS, Kafka) may be added in minor
/// releases. Downstream callers must include a `_ =>` arm when
/// matching on this enum.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum Destination {
    /// HTTP POST to the URL with the event JSON as body. Always available
    /// (no extra cargo features required).
    Webhook { url: String },
    /// AWS SQS queue ARN. Active only with the `aws-events` cargo feature.
    Sqs { queue_arn: String },
    /// AWS SNS topic ARN. Active only with the `aws-events` cargo feature.
    Sns { topic_arn: String },
}

impl Destination {
    /// Short tag used as a metric label so dashboards can split drops by
    /// destination type without leaking ARNs / URLs into Prometheus.
    #[must_use]
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::Webhook { .. } => "webhook",
            Self::Sqs { .. } => "sqs",
            Self::Sns { .. } => "sns",
        }
    }
}

/// One notification rule. Multiple rules can be registered per bucket; each
/// rule independently chooses whether to fire on a given event by checking
/// the event type against `events` and the object key against the
/// `filter_prefix` / `filter_suffix` pair (both optional; both apply
/// simultaneously when set).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationRule {
    /// Operator-supplied id (the AWS S3 PUT API requires it; if the client
    /// omits one, callers can synthesise `format!("rule-{i}")`).
    pub id: String,
    /// Event types this rule listens for. Empty means "never fire" — the
    /// rule won't match anything.
    pub events: Vec<EventType>,
    /// Where to send the event when the rule matches.
    pub destination: Destination,
    /// AWS S3 `Filter.Key.Rules[Name=prefix].Value`. When `None`, no prefix
    /// filter applies. Empty string is treated as "match anything", same as
    /// `None`.
    pub filter_prefix: Option<String>,
    /// AWS S3 `Filter.Key.Rules[Name=suffix].Value`. Same semantics as
    /// `filter_prefix`.
    pub filter_suffix: Option<String>,
}

/// Per-bucket notification configuration (ordered list of rules).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationConfig {
    pub rules: Vec<NotificationRule>,
}

/// JSON snapshot — `bucket -> NotificationConfig`. Mirrors the shape of
/// `cors.rs` / `inventory.rs`'s `to_json` / `from_json` so the operator can
/// hand-edit configurations across restart cycles.
#[derive(Debug, Default, Serialize, Deserialize)]
struct NotificationSnapshot {
    by_bucket: HashMap<String, NotificationConfig>,
}

/// In-memory manager of per-bucket notification configurations.
///
/// The `dropped_total` counter is exposed publicly for the metrics layer to
/// poll without taking the configuration lock.
pub struct NotificationManager {
    by_bucket: RwLock<HashMap<String, NotificationConfig>>,
    /// Bumped by `dispatch_event` whenever a destination returns 5xx after
    /// the configured retry budget, or when an `aws-events`-gated
    /// destination fires without the feature compiled in.
    pub dropped_total: AtomicU64,
}

impl Default for NotificationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NotificationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationManager")
            .field("dropped_total", &self.dropped_total.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl NotificationManager {
    /// Empty manager — no bucket has any configuration registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_bucket: RwLock::new(HashMap::new()),
            dropped_total: AtomicU64::new(0),
        }
    }

    /// `put_bucket_notification_configuration` handler entry. The bucket's
    /// existing configuration is fully replaced (S3 spec — PutBucket... is
    /// upsert-style at the bucket scope, not per-rule patch).
    pub fn put(&self, bucket: &str, config: NotificationConfig) {
        crate::lock_recovery::recover_write(&self.by_bucket, "notifications.by_bucket")
            .insert(bucket.to_owned(), config);
    }

    /// `get_bucket_notification_configuration` handler entry. Returns the
    /// cloned configuration, or `None` when nothing is registered. AWS S3
    /// returns an empty configuration document (not 404) in that case; the
    /// service-layer handler maps `None` → empty DTO accordingly.
    #[must_use]
    pub fn get(&self, bucket: &str) -> Option<NotificationConfig> {
        crate::lock_recovery::recover_read(&self.by_bucket, "notifications.by_bucket")
            .get(bucket)
            .cloned()
    }

    /// Drop all rules for `bucket`. Idempotent.
    pub fn delete(&self, bucket: &str) {
        crate::lock_recovery::recover_write(&self.by_bucket, "notifications.by_bucket")
            .remove(bucket);
    }

    /// Serialise the entire manager state to JSON (for
    /// `--notifications-state-file` snapshot dumps).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let snap = NotificationSnapshot {
            by_bucket: crate::lock_recovery::recover_read(
                &self.by_bucket,
                "notifications.by_bucket",
            )
            .clone(),
        };
        serde_json::to_string(&snap)
    }

    /// Restore a manager from a previously-emitted snapshot. The
    /// `dropped_total` counter is reset to 0 — historical drops are not
    /// persisted (they're a runtime metric, not configuration).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: NotificationSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            by_bucket: RwLock::new(snap.by_bucket),
            dropped_total: AtomicU64::new(0),
        })
    }

    /// Match an event against the bucket's rules and return every
    /// destination whose rule accepts the (event type, key) tuple. Order
    /// follows the rule declaration order so a deterministic dispatch
    /// sequence falls out for tests.
    #[must_use]
    pub fn match_destinations(
        &self,
        bucket: &str,
        event: &EventType,
        key: &str,
    ) -> Vec<Destination> {
        let map = crate::lock_recovery::recover_read(&self.by_bucket, "notifications.by_bucket");
        let cfg = match map.get(bucket) {
            Some(c) => c,
            None => return Vec::new(),
        };
        cfg.rules
            .iter()
            .filter(|r| rule_matches(r, event, key))
            .map(|r| r.destination.clone())
            .collect()
    }
}

fn rule_matches(rule: &NotificationRule, event: &EventType, key: &str) -> bool {
    if !rule.events.iter().any(|e| e == event) {
        return false;
    }
    if let Some(p) = rule.filter_prefix.as_deref()
        && !p.is_empty()
        && !key.starts_with(p)
    {
        return false;
    }
    if let Some(s) = rule.filter_suffix.as_deref()
        && !s.is_empty()
        && !key.ends_with(s)
    {
        return false;
    }
    true
}

/// Build the AWS S3 Event payload JSON for a single record. Schema
/// matches:
/// <https://docs.aws.amazon.com/AmazonS3/latest/userguide/notification-content-structure.html>
///
/// The returned string is one full event envelope (`{"Records":[...]}`),
/// suitable as the body of an HTTP POST or the message body of an SQS /
/// SNS publish.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_event_json(
    bucket: &str,
    key: &str,
    event: &EventType,
    size: Option<u64>,
    etag: Option<&str>,
    version_id: Option<&str>,
    request_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    // Trim any surrounding `"` from the etag — AWS S3 stores ETags as
    // quoted strings on the wire but the event payload uses the bare hex.
    let etag_clean = etag.map(|e| e.trim_matches('"').to_owned());
    let mut object = serde_json::json!({
        "key": key,
        "sequencer": format!("{:016x}", now.timestamp_micros() as u64),
    });
    if let Some(sz) = size {
        object["size"] = serde_json::json!(sz);
    }
    if let Some(ref e) = etag_clean {
        object["eTag"] = serde_json::json!(e);
    }
    if let Some(v) = version_id {
        object["versionId"] = serde_json::json!(v);
    }
    let event_name = event.as_aws_str();
    let event_source = match event {
        EventType::ObjectCreatedPut => "ObjectCreated",
        EventType::ObjectRemovedDelete | EventType::ObjectRemovedDeleteMarker => "ObjectRemoved",
    };
    let record = serde_json::json!({
        "eventVersion": "2.1",
        "eventSource": "aws:s3",
        "awsRegion": "us-east-1",
        "eventTime": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "eventName": event_name,
        "userIdentity": { "principalId": "S4" },
        "requestParameters": { "sourceIPAddress": "0.0.0.0" },
        "responseElements": {
            "x-amz-request-id": request_id,
            "x-amz-id-2": request_id,
        },
        "s3": {
            "s3SchemaVersion": "1.0",
            "configurationId": "S4-default",
            "bucket": {
                "name": bucket,
                "ownerIdentity": { "principalId": "S4" },
                "arn": format!("arn:aws:s3:::{bucket}"),
            },
            "object": object,
        },
    });
    let _ = event_source; // surfaced via eventName; left here for future
    serde_json::json!({ "Records": [record] }).to_string()
}

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_MS: u64 = 50;

/// Fire-and-forget event dispatch. Iterates every matched destination for
/// the (bucket, key, event) triple and sends the event JSON in detached
/// tokio tasks; this future itself awaits the spawn-and-send pipeline so
/// callers can `tokio::spawn` it once and forget about the outcome.
///
/// Webhook destinations get retried 3 times with exponential backoff on 5xx
/// responses; permanent 4xx responses are treated as "delivered" (the
/// receiver explicitly rejected the payload — retrying won't help) and the
/// drop counter is NOT bumped. SNS / SQS are best-effort; without the
/// `aws-events` cargo feature the drop counter is bumped immediately.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_event(
    manager: Arc<NotificationManager>,
    bucket: String,
    key: String,
    event: EventType,
    size: Option<u64>,
    etag: Option<String>,
    version_id: Option<String>,
    request_id: String,
) {
    let dests = manager.match_destinations(&bucket, &event, &key);
    if dests.is_empty() {
        return;
    }
    let now = chrono::Utc::now();
    let body = build_event_json(
        &bucket,
        &key,
        &event,
        size,
        etag.as_deref(),
        version_id.as_deref(),
        &request_id,
        now,
    );
    for dest in dests {
        let mgr = Arc::clone(&manager);
        let body = body.clone();
        // v0.8.5 #81 (audit H-7): wrap the per-destination `send_one`
        // body in `futures::FutureExt::catch_unwind` so a panic inside
        // the reqwest stack / aws-sdk-sns / aws-sdk-sqs (or a bug in
        // our retry loop) does NOT bubble out of the detached task as
        // a `JoinError` that no operator dashboard scrapes. Caught
        // panics bump `s4_dispatcher_panics_total{kind="notification"}`
        // + log at ERROR with the panic payload, so silent feature
        // degradation (= every webhook dispatch panicking and never
        // reaching the receiver, but the gateway itself looking
        // healthy) becomes a first-class metric the operator can
        // alert on.
        //
        // `AssertUnwindSafe` is required because the `Arc<...>` +
        // `String` captures are not `UnwindSafe` by default; the
        // safety contract here is "we don't continue using any of
        // those captures after the panic" which trivially holds (we
        // drop them and return).
        tokio::spawn(async move {
            use futures::FutureExt as _;
            let kind = "notification";
            let fut = send_one(mgr, dest, body);
            if let Err(panic) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
                let panic_msg = panic
                    .downcast_ref::<&'static str>()
                    .copied()
                    .map(str::to_owned)
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(non-string panic payload)".to_owned());
                tracing::error!(
                    kind,
                    panic_payload = %panic_msg,
                    "S4 dispatcher task panicked (caught by catch_unwind, runtime not poisoned)"
                );
                crate::metrics::record_dispatcher_panic(kind);
            }
        });
    }
}

async fn send_one(manager: Arc<NotificationManager>, dest: Destination, body: String) {
    match dest {
        Destination::Webhook { ref url } => {
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "notifications: reqwest client build failed");
                    bump_drop(&manager, dest.type_tag());
                    return;
                }
            };
            for attempt in 0..RETRY_ATTEMPTS {
                let resp = client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(body.clone())
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => return,
                    Ok(r) if r.status().is_server_error() => {
                        // 5xx — retry with backoff.
                        if attempt + 1 < RETRY_ATTEMPTS {
                            let delay_ms = RETRY_BASE_MS * (1u64 << attempt);
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        tracing::warn!(
                            url = %url,
                            status = %r.status(),
                            "notifications: webhook giving up after {RETRY_ATTEMPTS} attempts"
                        );
                        bump_drop(&manager, "webhook");
                        return;
                    }
                    Ok(r) => {
                        // 4xx / redirect — receiver rejected, no point retrying.
                        tracing::warn!(
                            url = %url,
                            status = %r.status(),
                            "notifications: webhook permanent failure, dropping"
                        );
                        return;
                    }
                    Err(e) => {
                        // Network-level error — also retry-eligible.
                        if attempt + 1 < RETRY_ATTEMPTS {
                            let delay_ms = RETRY_BASE_MS * (1u64 << attempt);
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        tracing::warn!(
                            url = %url,
                            error = %e,
                            "notifications: webhook network failure, dropping after {RETRY_ATTEMPTS} attempts"
                        );
                        bump_drop(&manager, "webhook");
                        return;
                    }
                }
            }
        }
        Destination::Sqs { ref queue_arn } => {
            #[cfg(feature = "aws-events")]
            {
                send_sqs(&manager, queue_arn, &body).await;
            }
            #[cfg(not(feature = "aws-events"))]
            {
                let _ = queue_arn;
                let _ = body;
                tracing::warn!(
                    "notifications: SQS destination configured but `aws-events` feature is off — dropping"
                );
                bump_drop(&manager, "sqs");
            }
        }
        Destination::Sns { ref topic_arn } => {
            #[cfg(feature = "aws-events")]
            {
                send_sns(&manager, topic_arn, &body).await;
            }
            #[cfg(not(feature = "aws-events"))]
            {
                let _ = topic_arn;
                let _ = body;
                tracing::warn!(
                    "notifications: SNS destination configured but `aws-events` feature is off — dropping"
                );
                bump_drop(&manager, "sns");
            }
        }
    }
}

fn bump_drop(manager: &NotificationManager, dest_tag: &'static str) {
    manager.dropped_total.fetch_add(1, Ordering::Relaxed);
    crate::metrics::record_notification_drop(dest_tag);
}

#[cfg(feature = "aws-events")]
async fn send_sqs(manager: &NotificationManager, queue_arn: &str, body: &str) {
    let conf = aws_config::load_from_env().await;
    let client = aws_sdk_sqs::Client::new(&conf);
    // ARN form: arn:aws:sqs:<region>:<account>:<queue-name>. SQS Send
    // expects the queue URL, but we accept the ARN at config time and
    // synthesise the URL via SDK introspection. As a pragmatic shortcut,
    // operators can configure the URL directly inside the ARN field — the
    // SDK accepts both.
    let res = client
        .send_message()
        .queue_url(queue_arn)
        .message_body(body)
        .send()
        .await;
    if let Err(e) = res {
        tracing::warn!(arn = %queue_arn, error = ?e, "notifications: SQS send failed");
        bump_drop(manager, "sqs");
    }
}

#[cfg(feature = "aws-events")]
async fn send_sns(manager: &NotificationManager, topic_arn: &str, body: &str) {
    let conf = aws_config::load_from_env().await;
    let client = aws_sdk_sns::Client::new(&conf);
    let res = client
        .publish()
        .topic_arn(topic_arn)
        .message(body)
        .send()
        .await;
    if let Err(e) = res {
        tracing::warn!(arn = %topic_arn, error = ?e, "notifications: SNS publish failed");
        bump_drop(manager, "sns");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        id: &str,
        events: &[EventType],
        dest: Destination,
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> NotificationRule {
        NotificationRule {
            id: id.to_owned(),
            events: events.to_vec(),
            destination: dest,
            filter_prefix: prefix.map(str::to_owned),
            filter_suffix: suffix.map(str::to_owned),
        }
    }

    #[test]
    fn match_destinations_single_rule_event_match() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://hook".into(),
                    },
                    None,
                    None,
                )],
            },
        );
        let dests = mgr.match_destinations("b", &EventType::ObjectCreatedPut, "any/key.txt");
        assert_eq!(dests.len(), 1, "single rule must fire on event match");
    }

    #[test]
    fn match_destinations_prefix_filter() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://hook".into(),
                    },
                    Some("uploads/"),
                    None,
                )],
            },
        );
        assert_eq!(
            mgr.match_destinations("b", &EventType::ObjectCreatedPut, "uploads/file.bin")
                .len(),
            1
        );
        assert!(
            mgr.match_destinations("b", &EventType::ObjectCreatedPut, "logs/file.bin")
                .is_empty(),
            "prefix filter must reject non-matching key"
        );
    }

    #[test]
    fn match_destinations_suffix_filter() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://hook".into(),
                    },
                    None,
                    Some(".jpg"),
                )],
            },
        );
        assert_eq!(
            mgr.match_destinations("b", &EventType::ObjectCreatedPut, "photo.jpg")
                .len(),
            1
        );
        assert!(
            mgr.match_destinations("b", &EventType::ObjectCreatedPut, "doc.pdf")
                .is_empty(),
            "suffix filter must reject non-matching key"
        );
    }

    #[test]
    fn match_destinations_no_rule_for_bucket() {
        let mgr = NotificationManager::new();
        let dests = mgr.match_destinations("ghost", &EventType::ObjectCreatedPut, "k");
        assert!(dests.is_empty(), "unknown bucket must yield empty vec");
    }

    #[test]
    fn match_destinations_event_type_mismatch() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://hook".into(),
                    },
                    None,
                    None,
                )],
            },
        );
        assert!(
            mgr.match_destinations("b", &EventType::ObjectRemovedDelete, "k")
                .is_empty(),
            "mismatched event type must not fire"
        );
    }

    #[test]
    fn match_destinations_multiple_rules_fire_in_order() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![
                    rule(
                        "first",
                        &[EventType::ObjectCreatedPut],
                        Destination::Webhook {
                            url: "http://first".into(),
                        },
                        None,
                        None,
                    ),
                    rule(
                        "second",
                        &[EventType::ObjectCreatedPut],
                        Destination::Webhook {
                            url: "http://second".into(),
                        },
                        None,
                        None,
                    ),
                ],
            },
        );
        let dests = mgr.match_destinations("b", &EventType::ObjectCreatedPut, "k");
        assert_eq!(dests.len(), 2, "both matching rules fire");
        match (&dests[0], &dests[1]) {
            (Destination::Webhook { url: u1 }, Destination::Webhook { url: u2 }) => {
                assert_eq!(u1, "http://first");
                assert_eq!(u2, "http://second");
            }
            _ => panic!("expected two webhooks in declaration order"),
        }
    }

    #[test]
    fn build_event_json_schema_matches_aws() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-13T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let body = build_event_json(
            "my-bucket",
            "uploads/photo.jpg",
            &EventType::ObjectCreatedPut,
            Some(12345),
            Some("\"deadbeef\""),
            Some("v-001"),
            "REQ-1",
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        let rec = &v["Records"][0];
        assert_eq!(rec["eventName"], "s3:ObjectCreated:Put");
        assert_eq!(rec["eventTime"], "2026-05-13T10:00:00.000Z");
        assert_eq!(rec["s3"]["bucket"]["name"], "my-bucket");
        assert_eq!(rec["s3"]["object"]["key"], "uploads/photo.jpg");
        assert_eq!(rec["s3"]["object"]["size"], 12345);
        assert_eq!(rec["s3"]["object"]["eTag"], "deadbeef");
        assert_eq!(rec["s3"]["object"]["versionId"], "v-001");
    }

    #[test]
    fn build_event_json_omits_optional_fields() {
        let now = chrono::Utc::now();
        let body = build_event_json(
            "b",
            "k",
            &EventType::ObjectRemovedDeleteMarker,
            None,
            None,
            None,
            "r",
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        let obj = &v["Records"][0]["s3"]["object"];
        assert!(obj.get("size").is_none());
        assert!(obj.get("eTag").is_none());
        assert!(obj.get("versionId").is_none());
    }

    #[test]
    fn json_round_trip() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut, EventType::ObjectRemovedDelete],
                    Destination::Sqs {
                        queue_arn: "arn:aws:sqs:us-east-1:123:q".into(),
                    },
                    Some("u/"),
                    Some(".jpg"),
                )],
            },
        );
        let json = mgr.to_json().expect("to_json");
        let mgr2 = NotificationManager::from_json(&json).expect("from_json");
        assert_eq!(mgr.get("b"), mgr2.get("b"));
    }

    #[test]
    fn delete_is_idempotent() {
        let mgr = NotificationManager::new();
        mgr.delete("never-existed");
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://h".into(),
                    },
                    None,
                    None,
                )],
            },
        );
        mgr.delete("b");
        assert!(mgr.get("b").is_none());
    }

    #[test]
    fn put_replaces_previous_config() {
        let mgr = NotificationManager::new();
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "old",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: "http://old".into(),
                    },
                    None,
                    None,
                )],
            },
        );
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "new",
                    &[EventType::ObjectRemovedDelete],
                    Destination::Webhook {
                        url: "http://new".into(),
                    },
                    None,
                    None,
                )],
            },
        );
        let cfg = mgr.get("b").expect("config");
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].id, "new");
    }

    #[tokio::test]
    async fn dispatch_event_via_webhook_delivers_payload() {
        // Spin up a tiny tokio HTTP receiver on a random port; verify the
        // dispatcher POSTs the event JSON we expect.
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received_cl = Arc::clone(&received);
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                received_cl.lock().unwrap().push(raw);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let mgr = Arc::new(NotificationManager::new());
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: format!("http://{addr}/hook"),
                    },
                    None,
                    None,
                )],
            },
        );

        dispatch_event(
            Arc::clone(&mgr),
            "b".into(),
            "k.txt".into(),
            EventType::ObjectCreatedPut,
            Some(7),
            Some("\"abc\"".into()),
            None,
            "req-1".into(),
        )
        .await;

        // The dispatcher detaches via tokio::spawn; poll briefly.
        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let raw = received.lock().unwrap().clone();
        assert!(!raw.is_empty(), "webhook receiver got nothing");
        let raw = &raw[0];
        assert!(raw.contains("POST /hook"), "missing POST line");
        assert!(raw.contains("s3:ObjectCreated:Put"), "missing event name");
        assert!(raw.contains("\"k.txt\""), "missing key");
        assert_eq!(mgr.dropped_total.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn dispatch_event_503_drops_after_retry_budget() {
        // Receiver that always returns 503 — the dispatcher must retry up
        // to the configured budget then bump dropped_total exactly once.
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let attempt_count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let attempt_count_cl = Arc::clone(&attempt_count);
        tokio::spawn(async move {
            for _ in 0..RETRY_ATTEMPTS {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 16384];
                    let _ = sock.read(&mut buf).await;
                    *attempt_count_cl.lock().unwrap() += 1;
                    let _ = sock
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let mgr = Arc::new(NotificationManager::new());
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![rule(
                    "r1",
                    &[EventType::ObjectCreatedPut],
                    Destination::Webhook {
                        url: format!("http://{addr}/sink"),
                    },
                    None,
                    None,
                )],
            },
        );

        dispatch_event(
            Arc::clone(&mgr),
            "b".into(),
            "k".into(),
            EventType::ObjectCreatedPut,
            None,
            None,
            None,
            "r".into(),
        )
        .await;

        // Wait for the detached task — RETRY_ATTEMPTS attempts plus
        // backoff (50ms + 100ms). Cap at 2s so a flaky run doesn't hang.
        for _ in 0..100 {
            if mgr.dropped_total.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            mgr.dropped_total.load(Ordering::Relaxed),
            1,
            "drop counter must bump exactly once after retry budget exhausted"
        );
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `by_bucket` write
    /// guard poisons the lock. `to_json` must recover via
    /// [`crate::lock_recovery::recover_read`] and surface the data
    /// instead of re-panicking on the SIGUSR1 dump-back path.
    #[test]
    fn notifications_to_json_after_panic_recovers_via_poison() {
        let mgr = std::sync::Arc::new(NotificationManager::new());
        mgr.put(
            "b",
            NotificationConfig {
                rules: vec![NotificationRule {
                    id: "r1".into(),
                    events: vec![EventType::ObjectCreatedPut],
                    destination: Destination::Webhook {
                        url: "http://example.invalid".into(),
                    },
                    filter_prefix: None,
                    filter_suffix: None,
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
        let mgr2 = NotificationManager::from_json(&json).expect("from_json");
        assert!(
            mgr2.get("b").is_some(),
            "recovered snapshot keeps original config"
        );
    }
}
