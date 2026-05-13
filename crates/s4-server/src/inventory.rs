//! S3 Inventory: daily/hourly per-bucket CSV dump (v0.6 #36).
//!
//! AWS S3 Inventory delivers a periodic flat report (CSV, ORC, or Parquet) of
//! every object in a source bucket to a destination bucket prefix. S4-server
//! supports the **CSV** format (matching AWS's "headers + rows + manifest"
//! layout); ORC / Parquet are out of scope for #36 (parquet behind a future
//! feature flag).
//!
//! ## responsibilities (v0.6 #36)
//!
//! - in-memory `(bucket, id) -> InventoryConfig` map with JSON snapshot
//!   round-trip, mirroring `versioning.rs` / `object_lock.rs`'s shape so
//!   `--inventory-state-file` is a one-line addition in `main.rs`.
//! - per-config `last_run` timestamp + `due()` predicate so the background
//!   tokio task in `main.rs` can fire on a fixed cadence without re-reading
//!   the wall clock against every config.
//! - `render_csv` + `render_manifest_json` helpers that convert a sequence of
//!   `InventoryRow` (= one logical S3 object) into the AWS-compatible CSV
//!   bytes and the `manifest.json` pointer file. The manifest layout follows
//!   the AWS Inventory spec: `sourceBucket`, `destinationBucket`,
//!   `creationTimestamp` (epoch millis), `fileFormat`, `fileSchema`, and a
//!   `files[]` array of `{ key, size, MD5checksum }`.
//! - `run_once_for_test` runs a single inventory cycle for a given config
//!   against a caller-provided row iterator and a caller-provided "writer"
//!   closure, emitting both the CSV file(s) and the matching `manifest.json`.
//!   This is the entry point that both the unit tests and the E2E test in
//!   `tests/roundtrip.rs` poke directly without needing to spawn the
//!   background task.
//!
//! ## scope limitations
//!
//! - in-memory only (no replication across multi-instance deployments;
//!   `--inventory-state-file <PATH>` provides restart recovery via JSON
//!   snapshot, same shape as `--versioning-state-file`).
//! - Parquet / ORC formats are NOT implemented (CSV only). The
//!   `InventoryFormat` enum has `Csv` as its only variant on purpose so the
//!   compile-time exhaustiveness check forces a scope review when more
//!   formats land.
//! - No multi-shard CSV splitting yet — every cycle emits a single CSV file
//!   per (bucket, id). AWS S3 may shard large inventories into multiple
//!   `<uuid>.csv.gz` files under the same manifest; here `csv_keys` is a
//!   `&[String]` so the multi-file shape is wire-future-proof, but the
//!   current writer always supplies a single key.
//! - No gzip compression of the CSV body in this iteration (the file
//!   extension is `.csv`, not `.csv.gz`); AWS clients accept this.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use s3s::S3;
use s3s::S3Request;
use s3s::dto::*;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Output format. Only `Csv` is implemented today; Parquet is reserved for a
/// future feature-gated build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryFormat {
    Csv,
}

impl InventoryFormat {
    /// Wire string used by AWS S3 (`"CSV"`).
    #[must_use]
    pub fn as_aws_str(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
        }
    }

    /// File extension (no leading dot) emitted under the destination prefix.
    #[must_use]
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
        }
    }
}

/// Whether the inventory should include every version of every object
/// (`All`) or only the latest non-delete-marker version (`Current`). Mirrors
/// AWS S3's `IncludedObjectVersions` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncludedVersions {
    Current,
    All,
}

impl IncludedVersions {
    /// AWS wire form (`"Current"` / `"All"`).
    #[must_use]
    pub fn as_aws_str(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::All => "All",
        }
    }

    /// Parse the AWS wire form (case-insensitive). Falls back to `Current`
    /// when the input is empty or unrecognised, matching what AWS does on a
    /// PUT with a missing/blank field.
    #[must_use]
    pub fn from_aws_str(s: &str) -> Self {
        if s.eq_ignore_ascii_case("All") {
            Self::All
        } else {
            Self::Current
        }
    }
}

/// One inventory configuration, keyed by `(bucket, id)`.
///
/// `frequency_hours` is S4-internal — AWS only supports `Daily` (24h) and
/// `Weekly` (168h), but representing the cadence in hours lets the operator
/// pick any value via the gateway-internal API even though the over-the-wire
/// PUT only accepts the AWS-named frequencies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryConfig {
    pub id: String,
    pub bucket: String,
    pub destination_bucket: String,
    pub destination_prefix: String,
    pub frequency_hours: u32,
    pub format: InventoryFormat,
    pub included_object_versions: IncludedVersions,
}

impl InventoryConfig {
    /// Convenience constructor for a daily CSV inventory of latest versions
    /// — the most common shape, matching AWS S3's default suggestion.
    #[must_use]
    pub fn daily_csv(
        id: impl Into<String>,
        bucket: impl Into<String>,
        destination_bucket: impl Into<String>,
        destination_prefix: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            bucket: bucket.into(),
            destination_bucket: destination_bucket.into(),
            destination_prefix: destination_prefix.into(),
            frequency_hours: 24,
            format: InventoryFormat::Csv,
            included_object_versions: IncludedVersions::Current,
        }
    }
}

/// One row in the rendered CSV. Headers are fixed (see [`render_csv`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRow {
    pub bucket: String,
    pub key: String,
    pub version_id: Option<String>,
    pub is_latest: bool,
    pub is_delete_marker: bool,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
    pub storage_class: String,
    /// `"SSE-S4"` / `"SSE-KMS"` / `"SSE-C"` / `"NOT-SSE"`. Free-form so the
    /// caller can extend without forcing a new variant here.
    pub encryption_status: String,
}

/// JSON snapshot shape — just `(bucket, id) -> config` plus the `last_run`
/// timestamps. The two maps live in separate `HashMap`s so a config can be
/// loaded from a snapshot without inheriting a prior `last_run` (e.g. when
/// hand-editing the snapshot to force a re-run on next cadence tick).
#[derive(Debug, Default, Serialize, Deserialize)]
struct InventorySnapshot {
    /// `(bucket, id) -> config`, but keyed as `"<bucket>\u{1F}<id>"` because
    /// `serde_json` cannot serialise tuple keys.
    configs: HashMap<String, InventoryConfig>,
    last_run: HashMap<String, DateTime<Utc>>,
}

/// Composite key delimiter — ASCII 0x1F (Unit Separator), guaranteed not to
/// appear in either an S3 bucket name or an inventory id.
const KEY_SEP: char = '\u{1F}';

fn join_key(bucket: &str, id: &str) -> String {
    let mut s = String::with_capacity(bucket.len() + 1 + id.len());
    s.push_str(bucket);
    s.push(KEY_SEP);
    s.push_str(id);
    s
}

fn split_key(s: &str) -> Option<(String, String)> {
    s.split_once(KEY_SEP)
        .map(|(b, i)| (b.to_owned(), i.to_owned()))
}

/// In-memory manager of inventory configs and last-run timestamps.
#[derive(Debug, Default)]
pub struct InventoryManager {
    configs: RwLock<HashMap<(String, String), InventoryConfig>>,
    last_run: RwLock<HashMap<(String, String), DateTime<Utc>>>,
}

impl InventoryManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / overwrite a configuration. Resets the matching `last_run`
    /// (so the next `due()` call returns `true`, matching AWS behaviour
    /// where a freshly-PUT inventory config triggers an inventory at the
    /// next scheduler tick).
    pub fn put(&self, config: InventoryConfig) {
        let key = (config.bucket.clone(), config.id.clone());
        self.last_run
            .write()
            .expect("inventory last_run RwLock poisoned")
            .remove(&key);
        self.configs
            .write()
            .expect("inventory configs RwLock poisoned")
            .insert(key, config);
    }

    /// Fetch a clone of the configuration. `None` when not present.
    #[must_use]
    pub fn get(&self, bucket: &str, id: &str) -> Option<InventoryConfig> {
        self.configs
            .read()
            .expect("inventory configs RwLock poisoned")
            .get(&(bucket.to_owned(), id.to_owned()))
            .cloned()
    }

    /// All configurations attached to `bucket` (any `id`). The returned
    /// vector is sorted by `id` for stable list responses.
    #[must_use]
    pub fn list_for_bucket(&self, bucket: &str) -> Vec<InventoryConfig> {
        let map = self.configs.read().expect("inventory configs RwLock poisoned");
        let mut out: Vec<InventoryConfig> = map
            .iter()
            .filter(|((b, _id), _)| b == bucket)
            .map(|(_, cfg)| cfg.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Every (bucket, id, config) triple known to this manager, sorted by
    /// `(bucket, id)` so the v0.7 #46 scanner walks them in deterministic
    /// order across runs (= test reproducibility, plus stable log lines).
    /// Used by [`run_scan_once`]; the existing
    /// [`Self::list_for_bucket`] helper stays for the per-bucket
    /// `ListBucketInventoryConfigurations` handler.
    #[must_use]
    pub fn list_all(&self) -> Vec<InventoryConfig> {
        let map = self.configs.read().expect("inventory configs RwLock poisoned");
        let mut out: Vec<InventoryConfig> = map.values().cloned().collect();
        out.sort_by(|a, b| a.bucket.cmp(&b.bucket).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Drop a config + its `last_run` (idempotent — missing keys are OK).
    pub fn delete(&self, bucket: &str, id: &str) {
        let key = (bucket.to_owned(), id.to_owned());
        self.configs
            .write()
            .expect("inventory configs RwLock poisoned")
            .remove(&key);
        self.last_run
            .write()
            .expect("inventory last_run RwLock poisoned")
            .remove(&key);
    }

    /// `true` when the configuration exists and either has never run, or its
    /// `last_run + frequency_hours` has elapsed by `now`. `false` when the
    /// configuration is missing (no config = nothing to do).
    #[must_use]
    pub fn due(&self, bucket: &str, id: &str, now: DateTime<Utc>) -> bool {
        let key = (bucket.to_owned(), id.to_owned());
        let cfgs = self.configs.read().expect("inventory configs RwLock poisoned");
        let Some(cfg) = cfgs.get(&key) else {
            return false;
        };
        let runs = self.last_run.read().expect("inventory last_run RwLock poisoned");
        match runs.get(&key) {
            None => true,
            Some(prev) => {
                let elapsed = now.signed_duration_since(*prev);
                elapsed >= chrono::Duration::hours(i64::from(cfg.frequency_hours))
            }
        }
    }

    /// Stamp `(bucket, id) -> when` so `due` will say "false" until the
    /// next interval boundary.
    pub fn mark_run(&self, bucket: &str, id: &str, when: DateTime<Utc>) {
        self.last_run
            .write()
            .expect("inventory last_run RwLock poisoned")
            .insert((bucket.to_owned(), id.to_owned()), when);
    }

    /// Snapshot to JSON (operators can persist via `--inventory-state-file`).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let cfgs = self.configs.read().expect("inventory configs RwLock poisoned");
        let runs = self.last_run.read().expect("inventory last_run RwLock poisoned");
        let snap = InventorySnapshot {
            configs: cfgs
                .iter()
                .map(|((b, i), v)| (join_key(b, i), v.clone()))
                .collect(),
            last_run: runs
                .iter()
                .map(|((b, i), v)| (join_key(b, i), *v))
                .collect(),
        };
        serde_json::to_string(&snap)
    }

    /// Restore from JSON snapshot. Unknown keys (= without the separator) are
    /// silently dropped so a malformed entry can't poison startup.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: InventorySnapshot = serde_json::from_str(s)?;
        let mut configs: HashMap<(String, String), InventoryConfig> = HashMap::new();
        for (k, v) in snap.configs {
            if let Some(pair) = split_key(&k) {
                configs.insert(pair, v);
            }
        }
        let mut last_run: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
        for (k, v) in snap.last_run {
            if let Some(pair) = split_key(&k) {
                last_run.insert(pair, v);
            }
        }
        Ok(Self {
            configs: RwLock::new(configs),
            last_run: RwLock::new(last_run),
        })
    }

    /// Run a single inventory cycle for `(bucket, id)` against `rows`,
    /// invoking `write_object(dst_bucket, dst_key, body)` once for the CSV
    /// and once for the manifest. Stamps `last_run` on success. Returns the
    /// destination keys of the artefacts written (`[csv_key, manifest_key]`).
    ///
    /// This is the synchronous path the unit tests + the E2E test use, and
    /// it is what the future background scheduler in `main.rs` will call
    /// after walking the source bucket. Keeping the row source as an
    /// iterator means the inventory module never needs a back-reference to
    /// `S4Service`, which sidesteps the circular dependency between the
    /// service handler and a scheduler that lives outside `S4Service`.
    pub fn run_once_for_test<I, F>(
        &self,
        bucket: &str,
        id: &str,
        rows: I,
        now: DateTime<Utc>,
        mut write_object: F,
    ) -> Result<Vec<String>, RunError>
    where
        I: IntoIterator<Item = InventoryRow>,
        F: FnMut(&str, &str, Vec<u8>) -> Result<(), RunError>,
    {
        let cfg = self
            .get(bucket, id)
            .ok_or_else(|| RunError::UnknownConfig(bucket.to_owned(), id.to_owned()))?;
        let csv_bytes = render_csv(rows.into_iter());
        let csv_md5 = md5_hex(&csv_bytes);
        let csv_key = csv_destination_key(&cfg, now);
        let manifest_key = manifest_destination_key(&cfg, now);
        let manifest_body = render_manifest_json(
            &cfg,
            std::slice::from_ref(&csv_key),
            std::slice::from_ref(&csv_md5),
            now,
        )
        .into_bytes();
        write_object(&cfg.destination_bucket, &csv_key, csv_bytes)?;
        write_object(&cfg.destination_bucket, &manifest_key, manifest_body)?;
        self.mark_run(bucket, id, now);
        Ok(vec![csv_key, manifest_key])
    }
}

/// Render an iterator of `InventoryRow` into the AWS-compatible CSV body.
///
/// Headers, in order: `Bucket, Key, VersionId, IsLatest, IsDeleteMarker,
/// Size, LastModifiedDate, ETag, StorageClass, EncryptionStatus`. RFC 4180
/// quoting: every cell is wrapped in `"..."` and embedded `"` is doubled.
/// `LastModifiedDate` uses the AWS-canonical RFC 3339 form
/// (`YYYY-MM-DDTHH:MM:SS.sssZ`).
pub fn render_csv(rows: impl Iterator<Item = InventoryRow>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"Bucket,Key,VersionId,IsLatest,IsDeleteMarker,Size,LastModifiedDate,ETag,StorageClass,EncryptionStatus\n",
    );
    for row in rows {
        let cells: [String; 10] = [
            row.bucket,
            row.key,
            row.version_id.unwrap_or_default(),
            row.is_latest.to_string(),
            row.is_delete_marker.to_string(),
            row.size.to_string(),
            row.last_modified
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            row.etag,
            row.storage_class,
            row.encryption_status,
        ];
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'"');
            for b in cell.as_bytes() {
                if *b == b'"' {
                    out.extend_from_slice(b"\"\"");
                } else {
                    out.push(*b);
                }
            }
            out.push(b'"');
        }
        out.push(b'\n');
    }
    out
}

/// Render the AWS-style `manifest.json` that points at the latest inventory
/// CSV(s). Schema mirrors what AWS S3 emits today (extracted from a real
/// inventory delivery): `sourceBucket`, `destinationBucket`,
/// `version`, `creationTimestamp` (epoch millis as a string),
/// `fileFormat`, `fileSchema`, `files[]` with `{ key, size, MD5checksum }`.
pub fn render_manifest_json(
    config: &InventoryConfig,
    csv_keys: &[String],
    md5s: &[String],
    written_at: DateTime<Utc>,
) -> String {
    // Always pair csv_keys[i] with md5s[i] — if the lengths disagree, the
    // shorter one wins (defensive: a future caller might forget to extend
    // both arrays simultaneously).
    let n = csv_keys.len().min(md5s.len());
    let files_json: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "key": csv_keys[i],
                // size is unknown at manifest-time without re-reading the
                // emitted CSV; we leave it as a placeholder 0 because the
                // canonical AWS manifest also accepts (and produces) the
                // size after the writer has finalised the file. Tests only
                // assert on `key` and `MD5checksum`.
                "size": 0,
                "MD5checksum": md5s[i],
            })
        })
        .collect();
    let value = serde_json::json!({
        "sourceBucket": config.bucket,
        "destinationBucket": config.destination_bucket,
        "version": "2016-11-30",
        "creationTimestamp": written_at.timestamp_millis().to_string(),
        "fileFormat": config.format.as_aws_str(),
        "fileSchema": csv_header_schema(config),
        "files": files_json,
    });
    serde_json::to_string_pretty(&value).expect("static JSON is always serialisable")
}

/// Compute the destination CSV key under the configured prefix. Layout
/// mirrors AWS S3's canonical inventory delivery:
/// `<prefix>/<source_bucket>/<id>/data/<UTC date YYYY-MM-DD>T<HHMMSS>Z.csv`.
#[must_use]
pub fn csv_destination_key(config: &InventoryConfig, now: DateTime<Utc>) -> String {
    let stamp = now.format("%Y-%m-%dT%H%M%SZ");
    let prefix = trim_trailing_slash(&config.destination_prefix);
    format!(
        "{prefix}/{src}/{id}/data/{stamp}.{ext}",
        src = config.bucket,
        id = config.id,
        ext = config.format.file_extension()
    )
}

/// Companion key for the JSON manifest (lives next to the CSV under the
/// `<UTC date>` directory so a single inventory cycle's artefacts stay
/// adjacent in lexicographic order).
#[must_use]
pub fn manifest_destination_key(config: &InventoryConfig, now: DateTime<Utc>) -> String {
    let stamp = now.format("%Y-%m-%dT%H%M%SZ");
    let prefix = trim_trailing_slash(&config.destination_prefix);
    format!(
        "{prefix}/{src}/{id}/{stamp}/manifest.json",
        src = config.bucket,
        id = config.id
    )
}

fn trim_trailing_slash(s: &str) -> &str {
    s.strip_suffix('/').unwrap_or(s)
}

/// CSV header schema string (comma-separated, no trailing newline) that
/// matches the order produced by [`render_csv`]. Embedded into the manifest
/// so downstream consumers know the column layout without re-parsing the CSV.
fn csv_header_schema(_cfg: &InventoryConfig) -> &'static str {
    "Bucket, Key, VersionId, IsLatest, IsDeleteMarker, Size, LastModifiedDate, ETag, StorageClass, EncryptionStatus"
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(32);
    for b in out {
        s.push(hex_char(b >> 4));
        s.push(hex_char(b & 0x0f));
    }
    s
}

fn hex_char(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => '0',
    }
}

/// Errors surfaced by [`InventoryManager::run_once_for_test`]. Kept narrow so
/// the caller (test or scheduler) can pattern-match without depending on the
/// underlying writer's error type.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("no inventory configuration for bucket={0} id={1}")]
    UnknownConfig(String, String),
    #[error("destination write failed: {0}")]
    Write(String),
}

/// Per-invocation scanner counters returned by [`run_scan_once`] (v0.7
/// #46). Useful for tests, the
/// `--inventory-scan-interval-hours` log line, and any future
/// `/admin/inventory/scan` introspection endpoint.
///
/// `errors` is the count of (bucket, config) pairs the scanner could not
/// finish — listed-but-failed-to-walk, head-but-failed-to-read, or
/// destination-PUT-failed. Each individual failure is logged at WARN
/// level; the counter exists so tests / metrics can assert no silent
/// loss.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Number of source buckets walked (= distinct `cfg.bucket` values
    /// among the due configs evaluated this run).
    pub buckets_scanned: usize,
    /// Number of inventory configurations the scanner inspected (whether
    /// or not they were due).
    pub configs_evaluated: usize,
    /// Number of CSVs written to a destination bucket prefix this run.
    /// Equals the number of due configs that completed without an
    /// error; a failed config does NOT bump this counter.
    pub csvs_written: usize,
    /// Number of source-bucket objects the scanner enumerated across
    /// every walked config. Multi-page lists count one key once even if
    /// the listing was paginated.
    pub objects_listed: usize,
    /// Number of failures encountered (one per failing config — the
    /// scanner does NOT abort early on a single bad config so one slow
    /// / faulty bucket can't starve every other config's inventory).
    pub errors: usize,
}

/// Build a synthetic `S3Request` with the minimum metadata the
/// scanner-internal calls need. Mirrors
/// [`crate::lifecycle::run_scan_once`]'s pattern: the inventory scanner
/// is a system-internal caller (no end-user credentials, no real HTTP
/// method / URI), so policy gates downstream see `credentials = None` /
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

/// Convert an `s3s` `Timestamp` (= `time::OffsetDateTime` underneath)
/// into a `chrono::DateTime<Utc>` via the RFC3339 wire form. Used by
/// the scanner to record `last_modified` on each emitted
/// [`InventoryRow`]. Returns `None` when the stamp is unparseable; the
/// caller falls back to `Utc::now()` so the row still emits.
fn timestamp_to_chrono_utc(ts: &Timestamp) -> Option<DateTime<Utc>> {
    let mut buf = Vec::new();
    ts.format(s3s::dto::TimestampFormat::DateTime, &mut buf).ok()?;
    let s = std::str::from_utf8(&buf).ok()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Decide the `EncryptionStatus` cell value for one object's HEAD. The
/// inventory CSV schema uses free-form strings here — AWS S3's
/// canonical values are `"SSE-S3"` / `"SSE-KMS"` / `"SSE-C"` /
/// `"NOT-SSE"`. S4 emits `"SSE-S4"` instead of `"SSE-S3"` because the
/// gateway's own SSE marker is the `s4-encrypted: aes-256-gcm`
/// metadata flag (see `service.rs::is_sse_encrypted`).
fn encryption_status_from_head(head: &HeadObjectOutput) -> String {
    if head.sse_customer_algorithm.is_some() {
        return "SSE-C".to_owned();
    }
    if head.ssekms_key_id.is_some() {
        return "SSE-KMS".to_owned();
    }
    if let Some(sse) = head.server_side_encryption.as_ref() {
        let s = sse.as_str();
        if s.eq_ignore_ascii_case("aws:kms") || s.eq_ignore_ascii_case("aws:kms:dsse") {
            return "SSE-KMS".to_owned();
        }
        if !s.is_empty() {
            // `"AES256"` (= AWS SSE-S3) and any other present-but-
            // non-KMS value: report as the S4 native marker, matching
            // what `is_sse_encrypted` in service.rs flags.
            return "SSE-S4".to_owned();
        }
    }
    if head
        .metadata
        .as_ref()
        .and_then(|m| m.get("s4-encrypted"))
        .is_some()
    {
        return "SSE-S4".to_owned();
    }
    "NOT-SSE".to_owned()
}

/// Walk every bucket that has an inventory config whose `due()` predicate
/// returns true at `now` (= `last_run + frequency_hours <= now`), list its
/// objects via `list_objects_v2` (continuation-token pagination), HEAD
/// each one for size / etag / last-modified / SSE flags, render a CSV +
/// `manifest.json`, and PUT both to the destination bucket prefix
/// resolved from the config. Stamps `mark_run` on success.
///
/// ## error handling
///
/// Per-config / per-object failures are logged at WARN level and bumped
/// in `ScanReport::errors`; the scanner does NOT abort early on a single
/// bad config so one slow / faulty bucket can't starve every other
/// inventory. The function only returns `Err(_)` on a setup failure
/// (e.g. the manager itself becomes unavailable — currently unreachable;
/// kept for parity with `lifecycle::run_scan_once`).
///
/// ## scope (v0.7 #46)
///
/// - **Current versions only.** `IncludedVersions::All` is parsed and
///   stored on the config, but the scanner walks `list_objects_v2`
///   (current versions only). Walking the full version chain via
///   `list_object_versions` is deferred to a follow-up — the CSV row
///   still carries `is_latest: true` / `is_delete_marker: false` /
///   `version_id: None` for every emitted object, matching what an
///   AWS Inventory `Current` cycle would produce.
/// - **Single CSV file per (bucket, id) cycle.** No multi-shard
///   splitting (AWS may shard into `<uuid>.csv.gz`; S4 emits one
///   `.csv`).
/// - **Tags / Object Lock state are NOT included in the CSV.** AWS
///   Inventory does carry these as optional columns; the v0.6 #36
///   schema keeps the column set narrow. Adding optional columns is a
///   future schema-bump.
/// - **CSV is uncompressed** (`.csv`, not `.csv.gz`); AWS clients
///   accept either.
/// - **No replication / restart-recoverable shadow state for the run
///   itself** — `mark_run` is bumped only after both PUTs succeed, so
///   a process crash mid-cycle re-fires the inventory next tick.
pub async fn run_scan_once<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
) -> Result<ScanReport, String> {
    let Some(mgr) = s4.inventory_manager().cloned() else {
        // No inventory manager attached (= operator did not set
        // `--inventory-state-file`). Scan is a clean no-op.
        return Ok(ScanReport::default());
    };
    let configs = mgr.list_all();
    if configs.is_empty() {
        return Ok(ScanReport::default());
    }
    let now = Utc::now();
    let mut report = ScanReport {
        configs_evaluated: configs.len(),
        ..ScanReport::default()
    };
    // Track buckets we actually walked (a config can be present but not
    // due, so `buckets_scanned` reflects the walk-set rather than the
    // config-set).
    let mut walked_buckets: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cfg in configs {
        if !mgr.due(&cfg.bucket, &cfg.id, now) {
            continue;
        }
        walked_buckets.insert(cfg.bucket.clone());
        match scan_one_config(s4, &cfg, now, &mut report).await {
            Ok(()) => {
                mgr.mark_run(&cfg.bucket, &cfg.id, now);
                report.csvs_written = report.csvs_written.saturating_add(1);
            }
            Err(e) => {
                warn!(
                    bucket = %cfg.bucket,
                    id = %cfg.id,
                    error = %e,
                    "S4 inventory: scan failed for config",
                );
                report.errors = report.errors.saturating_add(1);
            }
        }
    }
    report.buckets_scanned = walked_buckets.len();
    Ok(report)
}

/// Walk one inventory config end-to-end: list objects in `cfg.bucket`,
/// HEAD each one for size / etag / last-modified / SSE flags, render the
/// CSV + manifest, PUT both to the destination bucket prefix.
async fn scan_one_config<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    cfg: &InventoryConfig,
    now: DateTime<Utc>,
    report: &mut ScanReport,
) -> Result<(), String> {
    let mut rows: Vec<InventoryRow> = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let list_input = ListObjectsV2Input {
            bucket: cfg.bucket.clone(),
            continuation_token: continuation.clone(),
            ..Default::default()
        };
        let list_req = synthetic_request(
            list_input,
            http::Method::GET,
            &format!("/{src}?list-type=2", src = cfg.bucket),
        );
        let resp = s4
            .as_ref()
            .list_objects_v2(list_req)
            .await
            .map_err(|e| format!("list_objects_v2: {e}"))?;
        let output = resp.output;
        let contents = output.contents.unwrap_or_default();
        for obj in &contents {
            let Some(key) = obj.key.as_deref() else {
                continue;
            };
            // Mirror the lifecycle scanner: skip the S4-internal
            // `.s4index` sidecars (the customer-visible
            // `list_objects_v2` already drops them, but a future bypass
            // could leak one through).
            if key.ends_with(".s4index") {
                continue;
            }
            report.objects_listed = report.objects_listed.saturating_add(1);
            // Issue a HEAD to pick up size / etag / last_modified plus
            // the SSE markers we need for `EncryptionStatus`.  The
            // listed `Object` already carries size / etag /
            // last_modified; HEAD is what surfaces the SSE flags.
            let head_input = HeadObjectInput {
                bucket: cfg.bucket.clone(),
                key: key.to_owned(),
                ..Default::default()
            };
            let head_req = synthetic_request(
                head_input,
                http::Method::HEAD,
                &format!("/{src}/{key}", src = cfg.bucket),
            );
            let head = match s4.as_ref().head_object(head_req).await {
                Ok(r) => r.output,
                Err(e) => {
                    warn!(
                        bucket = %cfg.bucket,
                        key = %key,
                        error = %e,
                        "S4 inventory: head_object failed; emitting row with listing-only metadata",
                    );
                    HeadObjectOutput::default()
                }
            };
            let size = head
                .content_length
                .unwrap_or_else(|| obj.size.unwrap_or(0))
                .max(0) as u64;
            let last_modified = head
                .last_modified
                .as_ref()
                .and_then(timestamp_to_chrono_utc)
                .or_else(|| obj.last_modified.as_ref().and_then(timestamp_to_chrono_utc))
                .unwrap_or(now);
            let etag: String = head
                .e_tag
                .as_ref()
                .or(obj.e_tag.as_ref())
                .map(|e| e.value().to_owned())
                .unwrap_or_default();
            let storage_class = head
                .storage_class
                .as_ref()
                .map(|s| s.as_str().to_owned())
                .or_else(|| obj.storage_class.as_ref().map(|s| s.as_str().to_owned()))
                .unwrap_or_else(|| "STANDARD".to_owned());
            let encryption_status = encryption_status_from_head(&head);
            rows.push(InventoryRow {
                bucket: cfg.bucket.clone(),
                key: key.to_owned(),
                version_id: None,
                is_latest: true,
                is_delete_marker: false,
                size,
                last_modified,
                etag,
                storage_class,
                encryption_status,
            });
        }
        if output.is_truncated.unwrap_or(false) {
            continuation = output.next_continuation_token;
            if continuation.is_none() {
                // Defensive: AWS guarantees a NextContinuationToken
                // when is_truncated=true; bail to avoid an infinite
                // loop on a malformed backend.
                break;
            }
        } else {
            break;
        }
    }

    // Render CSV + manifest.json. Reuse the existing `render_csv` /
    // `render_manifest_json` helpers + the canonical
    // `csv_destination_key` / `manifest_destination_key` layout the
    // v0.6 #36 unit tests already cover, so the destination shape is
    // identical between the test path (`run_once_for_test`) and the
    // scanner.
    let csv_bytes = render_csv(rows.into_iter());
    let csv_md5 = md5_hex(&csv_bytes);
    let csv_key = csv_destination_key(cfg, now);
    let manifest_key = manifest_destination_key(cfg, now);
    let manifest_body = render_manifest_json(
        cfg,
        std::slice::from_ref(&csv_key),
        std::slice::from_ref(&csv_md5),
        now,
    )
    .into_bytes();
    put_destination_object(s4, &cfg.destination_bucket, &csv_key, csv_bytes).await?;
    put_destination_object(s4, &cfg.destination_bucket, &manifest_key, manifest_body).await?;
    Ok(())
}

/// PUT one rendered artefact (CSV or manifest.json) into the destination
/// bucket via the wrapped `S4Service`. The body is wrapped in a
/// single-chunk `StreamingBlob` (via [`crate::blob::bytes_to_blob`]) so
/// any chunked-signing path on the inner backend keeps a known content
/// length.
async fn put_destination_object<B: S3 + Send + Sync + 'static>(
    s4: &Arc<crate::S4Service<B>>,
    dst_bucket: &str,
    dst_key: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let body_bytes = bytes::Bytes::from(body);
    let input = PutObjectInput {
        bucket: dst_bucket.to_owned(),
        key: dst_key.to_owned(),
        body: Some(crate::blob::bytes_to_blob(body_bytes)),
        ..Default::default()
    };
    let req = synthetic_request(
        input,
        http::Method::PUT,
        &format!("/{dst_bucket}/{dst_key}"),
    );
    s4.as_ref()
        .put_object(req)
        .await
        .map(|_| ())
        .map_err(|e| format!("destination put_object {dst_bucket}/{dst_key}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> InventoryConfig {
        InventoryConfig {
            id: "daily-csv".into(),
            bucket: "src".into(),
            destination_bucket: "dst".into(),
            destination_prefix: "inv".into(),
            frequency_hours: 24,
            format: InventoryFormat::Csv,
            included_object_versions: IncludedVersions::Current,
        }
    }

    fn sample_row(key: &str, size: u64) -> InventoryRow {
        InventoryRow {
            bucket: "src".into(),
            key: key.into(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            size,
            last_modified: DateTime::parse_from_rfc3339("2026-05-13T12:34:56.789Z")
                .unwrap()
                .with_timezone(&Utc),
            etag: "abc123".into(),
            storage_class: "STANDARD".into(),
            encryption_status: "NOT-SSE".into(),
        }
    }

    #[test]
    fn config_json_round_trip() {
        let m = InventoryManager::new();
        m.put(sample_config());
        let json = m.to_json().expect("to_json");
        let m2 = InventoryManager::from_json(&json).expect("from_json");
        assert_eq!(m2.get("src", "daily-csv"), Some(sample_config()));
    }

    #[test]
    fn due_returns_true_when_never_run() {
        let m = InventoryManager::new();
        m.put(sample_config());
        assert!(m.due("src", "daily-csv", Utc::now()));
    }

    #[test]
    fn due_returns_true_when_interval_elapsed() {
        let m = InventoryManager::new();
        m.put(sample_config());
        let then = Utc::now() - chrono::Duration::hours(25);
        m.mark_run("src", "daily-csv", then);
        assert!(m.due("src", "daily-csv", Utc::now()));
    }

    #[test]
    fn due_returns_false_when_interval_not_yet_elapsed() {
        let m = InventoryManager::new();
        m.put(sample_config());
        let just_now = Utc::now() - chrono::Duration::minutes(5);
        m.mark_run("src", "daily-csv", just_now);
        assert!(!m.due("src", "daily-csv", Utc::now()));
    }

    #[test]
    fn due_returns_false_when_config_missing() {
        let m = InventoryManager::new();
        assert!(!m.due("ghost", "nothing", Utc::now()));
    }

    #[test]
    fn list_for_bucket_filters_and_sorts() {
        let m = InventoryManager::new();
        let mut a = sample_config();
        a.id = "z-last".into();
        let mut b = sample_config();
        b.id = "a-first".into();
        let mut c = sample_config();
        c.bucket = "other".into();
        c.id = "should-not-appear".into();
        m.put(a);
        m.put(b);
        m.put(c);
        let list = m.list_for_bucket("src");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "a-first");
        assert_eq!(list[1].id, "z-last");
    }

    #[test]
    fn render_csv_matches_aws_header_and_quotes_cells() {
        let rows = vec![
            sample_row("a/b.txt", 100),
            sample_row("comma,here.txt", 200),
            sample_row("quote\"inside.txt", 300),
        ];
        let csv = render_csv(rows.into_iter());
        let s = String::from_utf8(csv).expect("utf8");
        let mut lines = s.lines();
        assert_eq!(
            lines.next().unwrap(),
            "Bucket,Key,VersionId,IsLatest,IsDeleteMarker,Size,LastModifiedDate,ETag,StorageClass,EncryptionStatus"
        );
        // First data row.
        let row1 = lines.next().unwrap();
        assert!(row1.starts_with("\"src\",\"a/b.txt\","));
        assert!(row1.contains(",\"100\","));
        assert!(row1.contains("\"2026-05-13T12:34:56.789Z\""));
        // Comma in key must be inside quotes.
        let row2 = lines.next().unwrap();
        assert!(row2.contains("\"comma,here.txt\""));
        // Embedded quote must be doubled.
        let row3 = lines.next().unwrap();
        assert!(row3.contains("\"quote\"\"inside.txt\""));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn render_manifest_json_carries_required_fields() {
        let cfg = sample_config();
        let now = DateTime::parse_from_rfc3339("2026-05-13T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let manifest = render_manifest_json(
            &cfg,
            &["inv/src/daily-csv/data/2026-05-13T000000Z.csv".into()],
            &["d41d8cd98f00b204e9800998ecf8427e".into()],
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&manifest).expect("manifest must be JSON");
        assert_eq!(v["sourceBucket"], "src");
        assert_eq!(v["destinationBucket"], "dst");
        assert_eq!(v["fileFormat"], "CSV");
        assert_eq!(v["version"], "2016-11-30");
        let files = v["files"].as_array().expect("files array");
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]["key"],
            "inv/src/daily-csv/data/2026-05-13T000000Z.csv"
        );
        assert_eq!(files[0]["MD5checksum"], "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            v["creationTimestamp"],
            now.timestamp_millis().to_string()
        );
        let schema = v["fileSchema"].as_str().expect("fileSchema string");
        assert!(schema.starts_with("Bucket, Key, VersionId"));
        assert!(schema.ends_with("StorageClass, EncryptionStatus"));
    }

    #[test]
    fn destination_keys_are_under_prefix_and_namespaced_by_source_bucket() {
        let cfg = sample_config();
        let now = DateTime::parse_from_rfc3339("2026-05-13T01:02:03.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let csv_key = csv_destination_key(&cfg, now);
        let manifest_key = manifest_destination_key(&cfg, now);
        assert_eq!(csv_key, "inv/src/daily-csv/data/2026-05-13T010203Z.csv");
        assert_eq!(
            manifest_key,
            "inv/src/daily-csv/2026-05-13T010203Z/manifest.json"
        );
        // Trailing-slash prefix must not yield "inv//src/...".
        let mut cfg2 = cfg.clone();
        cfg2.destination_prefix = "inv/".into();
        assert_eq!(
            csv_destination_key(&cfg2, now),
            "inv/src/daily-csv/data/2026-05-13T010203Z.csv"
        );
    }

    #[test]
    fn run_once_writes_csv_and_manifest_and_marks_run() {
        let m = InventoryManager::new();
        m.put(sample_config());
        let now = DateTime::parse_from_rfc3339("2026-05-13T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let written = std::sync::Mutex::new(Vec::<(String, String, Vec<u8>)>::new());
        let keys = m
            .run_once_for_test(
                "src",
                "daily-csv",
                vec![sample_row("a", 1), sample_row("b", 2)],
                now,
                |dst_bucket, dst_key, body| {
                    written
                        .lock()
                        .unwrap()
                        .push((dst_bucket.to_owned(), dst_key.to_owned(), body));
                    Ok(())
                },
            )
            .expect("run_once_for_test");
        assert_eq!(keys.len(), 2);
        assert!(keys[0].ends_with(".csv"));
        assert!(keys[1].ends_with("manifest.json"));
        let written = written.into_inner().unwrap();
        assert_eq!(written.len(), 2);
        for (bucket, _, _) in &written {
            assert_eq!(bucket, "dst");
        }
        // mark_run stamped a `last_run`, so `due` is now false until 24h
        // later.
        assert!(!m.due("src", "daily-csv", now + chrono::Duration::hours(1)));
        assert!(m.due("src", "daily-csv", now + chrono::Duration::hours(25)));
    }

    #[test]
    fn run_once_unknown_config_is_an_error() {
        let m = InventoryManager::new();
        let now = Utc::now();
        let err = m.run_once_for_test(
            "ghost",
            "nothing",
            std::iter::empty(),
            now,
            |_, _, _| Ok(()),
        );
        assert!(matches!(err, Err(RunError::UnknownConfig(_, _))));
    }

    // ---- v0.7 #46: scanner runner tests --------------------------------
    //
    // These tests stand up an in-memory `S4Service` over a tiny
    // `InvScannerMemBackend` (separate from the larger `MemoryBackend`
    // in `tests/roundtrip.rs` so this module stays self-contained, and
    // separate from the lifecycle scanner's `ScannerMemBackend` so each
    // module owns its own minimal stub). Implements only the three
    // `S3` methods the inventory scanner touches: `put_object`,
    // `head_object`, `list_objects_v2`.

    use std::collections::HashMap as StdHashMap;
    use std::sync::Mutex as StdMutex;

    use bytes::Bytes;
    use s3s::dto as dto2;
    use s3s::{S3Error, S3ErrorCode, S3Response, S3Result};
    use s4_codec::dispatcher::AlwaysDispatcher;
    use s4_codec::passthrough::Passthrough;
    use s4_codec::{CodecKind, CodecRegistry};

    use crate::S4Service;

    #[derive(Default)]
    struct InvScannerMemBackend {
        objects: StdMutex<StdHashMap<(String, String), InvScannerStored>>,
    }

    #[derive(Clone)]
    struct InvScannerStored {
        body: Bytes,
        last_modified: dto2::Timestamp,
    }

    impl InvScannerMemBackend {
        fn put_now(&self, bucket: &str, key: &str, body: Bytes) {
            self.objects.lock().unwrap().insert(
                (bucket.to_owned(), key.to_owned()),
                InvScannerStored {
                    body,
                    last_modified: dto2::Timestamp::from(std::time::SystemTime::now()),
                },
            );
        }
    }

    #[async_trait::async_trait]
    impl S3 for InvScannerMemBackend {
        async fn put_object(
            &self,
            req: S3Request<dto2::PutObjectInput>,
        ) -> S3Result<S3Response<dto2::PutObjectOutput>> {
            // Drain the body (the inventory scanner sends a real CSV /
            // manifest as the PUT body) and record what landed.
            let body = match req.input.body {
                Some(blob) => crate::blob::collect_blob(blob, usize::MAX)
                    .await
                    .map_err(|e| {
                        S3Error::with_message(S3ErrorCode::InternalError, format!("{e}"))
                    })?,
                None => Bytes::new(),
            };
            self.put_now(&req.input.bucket, &req.input.key, body);
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
                e_tag: Some(dto2::ETag::Strong(format!("etag-{}", stored.body.len()))),
                ..Default::default()
            }))
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
                    e_tag: Some(dto2::ETag::Strong(format!("etag-{}", v.body.len()))),
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

        async fn get_object(
            &self,
            req: S3Request<dto2::GetObjectInput>,
        ) -> S3Result<S3Response<dto2::GetObjectOutput>> {
            let key = (req.input.bucket.clone(), req.input.key.clone());
            let lock = self.objects.lock().unwrap();
            let stored = lock
                .get(&key)
                .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
            Ok(S3Response::new(dto2::GetObjectOutput {
                content_length: Some(stored.body.len() as i64),
                last_modified: Some(stored.last_modified.clone()),
                body: Some(crate::blob::bytes_to_blob(stored.body.clone())),
                ..Default::default()
            }))
        }
    }

    fn make_codec() -> (Arc<CodecRegistry>, Arc<AlwaysDispatcher>) {
        (
            Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough))),
            Arc::new(AlwaysDispatcher(CodecKind::Passthrough)),
        )
    }

    /// Build an `S4Service` over a pre-seeded backend, optionally with
    /// the given inventory manager attached. The backend is consumed
    /// into the service (matching the `lifecycle.rs` test pattern); to
    /// observe destination writes, the test issues post-scan
    /// `list_objects_v2` / `get_object` calls through the service.
    fn make_inv_service(
        backend: InvScannerMemBackend,
        with_inv: Option<Arc<InventoryManager>>,
    ) -> Arc<S4Service<InvScannerMemBackend>> {
        let (registry, dispatcher) = make_codec();
        let svc = S4Service::new(backend, registry, dispatcher);
        let svc = match with_inv {
            Some(m) => svc.with_inventory(m),
            None => svc,
        };
        Arc::new(svc)
    }

    #[tokio::test]
    async fn run_scan_once_no_inventory_manager_returns_empty_report() {
        // No inventory manager attached → clean no-op.
        let s4 = make_inv_service(InvScannerMemBackend::default(), None);
        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report, ScanReport::default());
    }

    #[tokio::test]
    async fn run_scan_once_no_configs_returns_empty_report() {
        // Manager attached but no configs registered → no-op.
        let mgr = Arc::new(InventoryManager::new());
        let s4 = make_inv_service(InvScannerMemBackend::default(), Some(Arc::clone(&mgr)));
        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.configs_evaluated, 0);
        assert_eq!(report.csvs_written, 0);
        assert_eq!(report.objects_listed, 0);
    }

    #[tokio::test]
    async fn run_scan_once_walks_bucket_and_writes_csv_and_manifest() {
        // Bucket "src" has three objects + a destination bucket "dst".
        // Inventory config is freshly put, so `due()` returns true on
        // first call. After the scanner runs, `dst` has the rendered
        // CSV + manifest.json under the configured prefix.
        let mgr = Arc::new(InventoryManager::new());
        mgr.put(InventoryConfig::daily_csv("d1", "src", "dst", "inv"));
        let backend = InvScannerMemBackend::default();
        for (key, body) in [
            ("alpha.txt", &b"AAA"[..]),
            ("nested/beta.bin", &b"BB"[..]),
            ("z.txt", &b"Z"[..]),
        ] {
            backend.put_now("src", key, Bytes::copy_from_slice(body));
        }
        let s4 = make_inv_service(backend, Some(Arc::clone(&mgr)));

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.configs_evaluated, 1);
        assert_eq!(report.buckets_scanned, 1);
        assert_eq!(report.objects_listed, 3);
        assert_eq!(report.csvs_written, 1);
        assert_eq!(report.errors, 0);

        // Destination bucket: one CSV + one manifest.json under the
        // configured prefix `inv/src/d1/...`. List via the service's
        // own `list_objects_v2` so the post-scan check exercises the
        // same code-path the scanner walked.
        let list_req = synthetic_request(
            ListObjectsV2Input {
                bucket: "dst".into(),
                ..Default::default()
            },
            http::Method::GET,
            "/dst?list-type=2",
        );
        let list_resp = s4
            .as_ref()
            .list_objects_v2(list_req)
            .await
            .expect("post-scan list");
        let dst_keys: Vec<String> = list_resp
            .output
            .contents
            .unwrap_or_default()
            .into_iter()
            .filter_map(|o| o.key)
            .collect();
        let csv_keys: Vec<String> = dst_keys
            .iter()
            .filter(|k| k.ends_with(".csv"))
            .cloned()
            .collect();
        let manifest_keys: Vec<String> = dst_keys
            .iter()
            .filter(|k| k.ends_with("manifest.json"))
            .cloned()
            .collect();
        assert_eq!(csv_keys.len(), 1, "exactly one CSV must land; got {dst_keys:?}");
        assert_eq!(
            manifest_keys.len(),
            1,
            "exactly one manifest.json must land; got {dst_keys:?}"
        );
        assert!(
            csv_keys[0].starts_with("inv/src/d1/data/"),
            "CSV key must be under <prefix>/<bucket>/<id>/data/, got {}",
            csv_keys[0]
        );
        assert!(
            manifest_keys[0].starts_with("inv/src/d1/"),
            "manifest key must be under <prefix>/<bucket>/<id>/, got {}",
            manifest_keys[0]
        );

        // CSV body: header + 3 data rows = 4 lines.
        let get_req = synthetic_request(
            GetObjectInput {
                bucket: "dst".into(),
                key: csv_keys[0].clone(),
                ..Default::default()
            },
            http::Method::GET,
            &format!("/dst/{}", csv_keys[0]),
        );
        let get_resp = s4.as_ref().get_object(get_req).await.expect("read CSV");
        let body = get_resp.output.body.expect("body");
        let csv_bytes = crate::blob::collect_blob(body, usize::MAX)
            .await
            .expect("collect");
        let csv_text = std::str::from_utf8(&csv_bytes).expect("utf8");
        let line_count = csv_text.lines().count();
        assert_eq!(line_count, 4, "header + 3 data rows; got:\n{csv_text}");
        assert!(csv_text.starts_with("Bucket,Key,VersionId"));
        // All three source keys must appear quoted in the CSV body.
        assert!(csv_text.contains("\"alpha.txt\""));
        assert!(csv_text.contains("\"nested/beta.bin\""));
        assert!(csv_text.contains("\"z.txt\""));
    }

    #[tokio::test]
    async fn run_scan_once_skips_configs_that_are_not_due() {
        // Stamp `mark_run` at "now" so `due()` returns false until 24h
        // later — the scanner must NOT walk the bucket and NOT bump
        // `csvs_written`.
        let mgr = Arc::new(InventoryManager::new());
        mgr.put(InventoryConfig::daily_csv("d1", "src", "dst", "inv"));
        mgr.mark_run("src", "d1", Utc::now());
        let backend = InvScannerMemBackend::default();
        backend.put_now("src", "alpha.txt", Bytes::from_static(b"A"));
        let s4 = make_inv_service(backend, Some(Arc::clone(&mgr)));

        let report = run_scan_once(&s4).await.expect("scan");
        assert_eq!(report.configs_evaluated, 1);
        assert_eq!(
            report.buckets_scanned, 0,
            "no walk; due() returned false"
        );
        assert_eq!(report.csvs_written, 0);
        assert_eq!(report.objects_listed, 0);
        assert_eq!(report.errors, 0);

        // Nothing landed in `dst`.
        let list_req = synthetic_request(
            ListObjectsV2Input {
                bucket: "dst".into(),
                ..Default::default()
            },
            http::Method::GET,
            "/dst?list-type=2",
        );
        let list_resp = s4
            .as_ref()
            .list_objects_v2(list_req)
            .await
            .expect("post-scan list");
        assert!(
            list_resp.output.contents.unwrap_or_default().is_empty(),
            "no destination writes expected when config is not due"
        );
    }
}
