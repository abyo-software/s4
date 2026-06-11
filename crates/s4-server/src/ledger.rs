//! v1.2 `--savings-ledger-state-file` — **savings ledger**: measured
//! (not estimated) per-bucket compression savings for everything the
//! gateway has written.
//!
//! `s4 estimate` answers "how much *would* S4 save?" before deployment;
//! this module is the after-deployment twin: it answers "how much *is*
//! S4 actually saving right now?" from cumulative counters the gateway
//! maintains as it serves writes.
//!
//! ## What is counted
//!
//! Per bucket (plus a derived global total):
//!
//! - `original_bytes` — logical bytes the client PUT (pre-compression).
//! - `stored_bytes` — bytes the gateway actually wrote to the backend:
//!   compressed/framed body **including** the S4F2 frame headers, SSE
//!   envelope overhead, and any `<key>.s4index` sidecar the gateway
//!   emitted alongside the object.
//! - `objects` — number of currently-stored objects the gateway has
//!   written (on versioning-Enabled buckets each stored *version*
//!   counts — every version occupies backend bytes).
//!
//! ## Honesty constraints (read before editing the report text)
//!
//! The ledger observes **gateway-traversing writes only**:
//!
//! - Writes that bypass the gateway (backend-direct PUTs, `s4 migrate`,
//!   `s4 recompact` — both talk to the backend directly) are NOT
//!   reflected. `recompact` shrinking an object shows up only after the
//!   gateway itself next rewrites that object.
//! - Multipart `UploadPart` bytes for uploads that are later **aborted**
//!   are never counted (the ledger records at Complete time only); the
//!   abandoned part bytes the backend may briefly hold are invisible
//!   here.
//! - Cross-bucket replication replicas (written by the detached
//!   dispatcher, not the S3 handler path) are not counted.
//! - DELETE / overwrite subtraction relies on a HEAD probe of the
//!   to-be-removed object (`s4-original-size` metadata, falling back to
//!   the sidecar for multipart objects, falling back to
//!   `original = stored` for objects without S4 metadata). Probes are
//!   best-effort: a raced probe leaves the counters slightly stale
//!   rather than failing the client's request.
//!
//! These caveats are repeated as fixed notes in the `s4 savings` report
//! ([`SavingsReport::notes`]) and in the README so the numbers are
//! never quoted without their scope.
//!
//! ## Persistence
//!
//! Same `--*-state-file` conventions as the versioning / tagging /
//! lifecycle managers: the snapshot is loaded at boot through
//! [`crate::state_loader::load_or_fresh`] (corrupted file ⇒ WARN +
//! fresh counters + file left in place), it is dumped on SIGUSR1 like
//! every other attached manager, and — because savings counters are
//! useless if they evaporate on a crash — every mutation additionally
//! flushes the snapshot to disk via the same atomic
//! tmp-write-then-rename pattern `main.rs` uses for SIGUSR1 dumps.
//! Crash tolerance is therefore "at most the in-flight event is lost",
//! which is at least as strong as the existing state files (restart /
//! SIGUSR1 to persist) without inventing a new durability mechanism.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};

/// Cumulative totals for one bucket. All counters are clamped at zero
/// on subtraction (a best-effort probe race can otherwise drive a
/// counter negative; clamping keeps the snapshot sane and the drift is
/// disclosed in the report notes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketTotals {
    /// Logical bytes the client PUT (pre-compression).
    pub original_bytes: u64,
    /// Bytes actually written to the backend (frames + SSE envelope +
    /// sidecars).
    pub stored_bytes: u64,
    /// Currently-stored gateway-written objects (versions count).
    pub objects: u64,
}

/// On-disk snapshot shape (`--savings-ledger-state-file` JSON). Also
/// what `s4 savings` deserializes — the CLI never needs the live
/// gateway, only this file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    /// Per-bucket cumulative totals. `BTreeMap` for deterministic
    /// serialization + report ordering.
    pub buckets: BTreeMap<String, BucketTotals>,
}

impl LedgerSnapshot {
    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Restore from a JSON snapshot produced by [`Self::to_json`].
    /// Signature matches what [`crate::state_loader::load_or_fresh`]
    /// expects.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Sum across buckets (the "global" row of the report).
    pub fn global_totals(&self) -> BucketTotals {
        let mut g = BucketTotals::default();
        for t in self.buckets.values() {
            g.original_bytes = g.original_bytes.saturating_add(t.original_bytes);
            g.stored_bytes = g.stored_bytes.saturating_add(t.stored_bytes);
            g.objects = g.objects.saturating_add(t.objects);
        }
        g
    }
}

/// Live ledger attached to the gateway via
/// `S4Service::with_savings_ledger`. Holds the in-memory counters, the
/// flush target path, and a flush-serialization mutex (two concurrent
/// PUT completions must not interleave writes to the same `.tmp`
/// sibling).
#[derive(Debug)]
pub struct SavingsLedger {
    inner: RwLock<LedgerSnapshot>,
    path: PathBuf,
    /// Serializes flushes so a slower writer can never overwrite a
    /// newer snapshot with an older one: the JSON is rendered *inside*
    /// the critical section, after acquiring this lock.
    flush_lock: Mutex<()>,
}

impl SavingsLedger {
    /// Wrap a boot-loaded snapshot (see
    /// [`crate::state_loader::load_or_fresh`]) with its flush path.
    /// Stamps the Prometheus gauges for every restored bucket so the
    /// first `/metrics` scrape after a restart already shows the
    /// persisted totals.
    pub fn attach(snapshot: LedgerSnapshot, path: PathBuf) -> Self {
        for (bucket, t) in &snapshot.buckets {
            crate::metrics::record_ledger_bucket(bucket, t);
        }
        Self {
            inner: RwLock::new(snapshot),
            path,
            flush_lock: Mutex::new(()),
        }
    }

    /// Owned copy of the current counters (report / test introspection).
    pub fn snapshot(&self) -> LedgerSnapshot {
        crate::lock_recovery::recover_read(&self.inner, "savings_ledger.buckets").clone()
    }

    /// JSON of the current counters — same shape SIGUSR1 dump-back and
    /// the event-driven flush write to `--savings-ledger-state-file`.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        crate::lock_recovery::recover_read(&self.inner, "savings_ledger.buckets").to_json()
    }

    /// Apply one logical mutation to `bucket`'s counters and flush the
    /// snapshot to disk. Deltas may be negative (DELETE / overwrite
    /// subtraction); results are clamped at zero. The flush is
    /// best-effort: an I/O error is logged (WARN) and the in-memory
    /// counters keep serving — same degradation posture as a failed
    /// SIGUSR1 dump.
    pub fn apply_delta(
        &self,
        bucket: &str,
        original_delta: i64,
        stored_delta: i64,
        objects_delta: i64,
    ) {
        let updated = {
            let mut guard =
                crate::lock_recovery::recover_write(&self.inner, "savings_ledger.buckets");
            let t = guard.buckets.entry(bucket.to_owned()).or_default();
            t.original_bytes = add_clamped(t.original_bytes, original_delta);
            t.stored_bytes = add_clamped(t.stored_bytes, stored_delta);
            t.objects = add_clamped(t.objects, objects_delta);
            *t
        };
        crate::metrics::record_ledger_bucket(bucket, &updated);
        self.flush();
    }

    /// Render + atomically write the snapshot to the state-file path.
    /// Serialized via `flush_lock`; render happens inside the lock so
    /// flushes are monotonic (a newer snapshot can never be clobbered
    /// by a slower, older writer).
    fn flush(&self) {
        let _guard = crate::lock_recovery::recover_mutex(&self.flush_lock, "savings_ledger.flush");
        let json = match self.to_json() {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "S4 savings ledger: snapshot serialize failed; state file left stale"
                );
                return;
            }
        };
        if let Err(e) = atomic_write(&self.path, &json) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "S4 savings ledger: state-file flush failed; counters stay in memory \
                 (SIGUSR1 or the next successful write event will retry)"
            );
        }
    }
}

/// `new - old` as a saturating `i64` delta — the building block the
/// service handlers use to turn "replace footprint X with footprint Y"
/// into one [`SavingsLedger::apply_delta`] call without wrapping on
/// pathological (>= 2^63) byte counts.
pub fn signed_delta(new: u64, old: u64) -> i64 {
    if new >= old {
        i64::try_from(new - old).unwrap_or(i64::MAX)
    } else {
        i64::try_from(old - new).map(|v| -v).unwrap_or(i64::MIN)
    }
}

/// `base + delta`, clamped to `0..=u64::MAX`. A clamp at the bottom
/// means a probe race under-counted earlier (disclosed in the report
/// notes); we log at debug level rather than warn to keep a steady
/// drift from spamming operators.
fn add_clamped(base: u64, delta: i64) -> u64 {
    if delta >= 0 {
        base.saturating_add(delta as u64)
    } else {
        let sub = delta.unsigned_abs();
        if sub > base {
            tracing::debug!(
                base,
                delta,
                "S4 savings ledger: subtraction clamped at zero (probe race drift)"
            );
        }
        base.saturating_sub(sub)
    }
}

/// Atomic write: `<path>.tmp` then rename. Same contract as the
/// SIGUSR1 `atomic_write` in `main.rs` — on a power loss the worst
/// case is a `.tmp` orphan, never a truncated state file. (Duplicated
/// here because the `main.rs` helper is binary-private and
/// `#[cfg(unix)]`-gated; this one must work wherever the library
/// builds.)
fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// === `s4 savings` report (CLI-side, reads the state file only) ===

/// Default `--price-per-gb-month` for the savings report: AWS S3
/// Standard us-east-1 first-50TB tier. Same constant value as
/// [`crate::estimate::DEFAULT_PRICE_PER_GB_MONTH`] — kept as a
/// re-export so the two subcommands can't silently diverge.
pub const DEFAULT_PRICE_PER_GB_MONTH: f64 = crate::estimate::DEFAULT_PRICE_PER_GB_MONTH;

/// Bytes per GB for the $/month conversion — binary gigabytes (GiB),
/// matching AWS billing and `s4 estimate`.
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// v1.2 stability: `#[non_exhaustive]` — new savings-report failure
/// modes may be added in minor releases. Downstream callers must
/// include a `_ =>` arm when matching on this enum.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SavingsError {
    #[error("savings ledger state file {path}: {cause}")]
    StateFileRead { path: String, cause: String },
    #[error("savings ledger state file {path} is not a valid ledger snapshot: {cause}")]
    StateFileParse { path: String, cause: String },
}

/// One bucket row of the savings report.
#[derive(Debug, Clone, Serialize)]
pub struct BucketSavings {
    pub bucket: String,
    pub objects: u64,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    /// `1 - stored/original` (0.0 when `original_bytes == 0`).
    pub savings_ratio: f64,
    /// `(original - stored) / GiB × price` — what the bucket would
    /// additionally cost per month if the same logical bytes were
    /// stored uncompressed.
    pub monthly_savings_usd: f64,
}

/// Full result of one `s4 savings` run. Serializes to the
/// `--format json` output verbatim (same convention as
/// [`crate::estimate::EstimateReport`]).
#[derive(Debug, Clone, Serialize)]
pub struct SavingsReport {
    pub buckets: Vec<BucketSavings>,
    pub total_objects: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    /// `1 - total_stored/total_original` (0.0 when nothing recorded).
    pub total_savings_ratio: f64,
    pub price_per_gb_month: f64,
    pub total_monthly_savings_usd: f64,
    /// Fixed honesty notes — always read these before quoting the
    /// numbers anywhere.
    pub notes: Vec<String>,
}

fn savings_ratio(original: u64, stored: u64) -> f64 {
    if original == 0 {
        0.0
    } else {
        1.0 - (stored as f64 / original as f64)
    }
}

fn monthly_savings_usd(original: u64, stored: u64, price_per_gb_month: f64) -> f64 {
    (original as f64 - stored as f64) / BYTES_PER_GB * price_per_gb_month
}

/// Build the report from a (deserialized) snapshot. Pure function so
/// the e2e test can compare the CLI's output against the state file
/// without spawning anything.
pub fn build_savings_report(snapshot: &LedgerSnapshot, price_per_gb_month: f64) -> SavingsReport {
    let buckets: Vec<BucketSavings> = snapshot
        .buckets
        .iter()
        .map(|(bucket, t)| BucketSavings {
            bucket: bucket.clone(),
            objects: t.objects,
            original_bytes: t.original_bytes,
            stored_bytes: t.stored_bytes,
            savings_ratio: savings_ratio(t.original_bytes, t.stored_bytes),
            monthly_savings_usd: monthly_savings_usd(
                t.original_bytes,
                t.stored_bytes,
                price_per_gb_month,
            ),
        })
        .collect();
    let g = snapshot.global_totals();
    let notes = vec![
        "the ledger observes gateway-traversing writes only: backend-direct writes, \
         `s4 migrate`, and `s4 recompact` (both backend-direct) are not reflected; \
         `recompact` savings appear only after the gateway next rewrites the object"
            .to_owned(),
        "aborted multipart uploads are never counted (parts are recorded at Complete \
         time only); cross-bucket replication replicas are not counted"
            .to_owned(),
        "DELETE / overwrite subtraction uses a best-effort HEAD probe of the removed \
         object — a raced probe leaves the counters slightly stale rather than \
         failing the request"
            .to_owned(),
        "storage bytes only: request, egress, and (on GPU deployments) compute costs \
         are unchanged by S4"
            .to_owned(),
    ];
    SavingsReport {
        buckets,
        total_objects: g.objects,
        total_original_bytes: g.original_bytes,
        total_stored_bytes: g.stored_bytes,
        total_savings_ratio: savings_ratio(g.original_bytes, g.stored_bytes),
        price_per_gb_month,
        total_monthly_savings_usd: monthly_savings_usd(
            g.original_bytes,
            g.stored_bytes,
            price_per_gb_month,
        ),
        notes,
    }
}

/// Read + parse the state file and build the report (the whole
/// `s4 savings` pipeline minus rendering). Read-only — the gateway
/// can keep running; the event-driven flush makes the file a complete
/// point-in-time snapshot.
pub fn run_savings(
    path: &std::path::Path,
    price_per_gb_month: f64,
) -> Result<SavingsReport, SavingsError> {
    let raw = crate::state_loader::read_state_file_or_fresh(path).map_err(|e| {
        SavingsError::StateFileRead {
            path: path.display().to_string(),
            cause: e.to_string(),
        }
    })?;
    let snapshot = match raw {
        // Missing / empty file = a gateway that has the flag but hasn't
        // served a write yet — report zeros, not an error.
        None => LedgerSnapshot::default(),
        Some(json) => {
            LedgerSnapshot::from_json(&json).map_err(|e| SavingsError::StateFileParse {
                path: path.display().to_string(),
                cause: e.to_string(),
            })?
        }
    };
    Ok(build_savings_report(&snapshot, price_per_gb_month))
}

/// Format a byte count as a short human string (binary units). Same
/// rendering as `estimate::human_bytes` (private there; duplicated to
/// avoid widening the estimate module's surface).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Render the default human-readable table for `--format table`.
pub fn render_savings_human(report: &SavingsReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "S4 measured savings (gateway-written objects)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {:<20} {:>8} {:>14} {:>14} {:>8} {:>12}",
        "bucket", "objects", "original", "stored", "saved", "$/month",
    );
    for b in &report.buckets {
        let _ = writeln!(
            out,
            "  {:<20} {:>8} {:>14} {:>14} {:>7.1}% {:>12.2}",
            b.bucket,
            b.objects,
            human_bytes(b.original_bytes),
            human_bytes(b.stored_bytes),
            b.savings_ratio * 100.0,
            b.monthly_savings_usd,
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  total: {} objects, {} original -> {} stored ({:.1}% saved, {} bytes)",
        report.total_objects,
        human_bytes(report.total_original_bytes),
        human_bytes(report.total_stored_bytes),
        report.total_savings_ratio * 100.0,
        report
            .total_original_bytes
            .saturating_sub(report.total_stored_bytes),
    );
    let _ = writeln!(
        out,
        "  monthly savings: ${:.2} (at ${}/GB-month, storage bytes only)",
        report.total_monthly_savings_usd, report.price_per_gb_month,
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Notes:");
    for n in &report.notes {
        let _ = writeln!(out, "  - {n}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_ledger(dir: &tempfile::TempDir) -> SavingsLedger {
        SavingsLedger::attach(LedgerSnapshot::default(), dir.path().join("ledger.json"))
    }

    #[test]
    fn add_and_subtract_per_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = tmp_ledger(&dir);
        // Two PUTs into bucket a, one into bucket b.
        ledger.apply_delta("a", 1000, 100, 1);
        ledger.apply_delta("a", 500, 50, 1);
        ledger.apply_delta("b", 200, 200, 1);
        let snap = ledger.snapshot();
        assert_eq!(
            snap.buckets["a"],
            BucketTotals {
                original_bytes: 1500,
                stored_bytes: 150,
                objects: 2
            }
        );
        assert_eq!(
            snap.buckets["b"],
            BucketTotals {
                original_bytes: 200,
                stored_bytes: 200,
                objects: 1
            }
        );
        // DELETE one object from a.
        ledger.apply_delta("a", -500, -50, -1);
        let snap = ledger.snapshot();
        assert_eq!(
            snap.buckets["a"],
            BucketTotals {
                original_bytes: 1000,
                stored_bytes: 100,
                objects: 1
            }
        );
        // Global sums both buckets.
        let g = snap.global_totals();
        assert_eq!(g.original_bytes, 1200);
        assert_eq!(g.stored_bytes, 300);
        assert_eq!(g.objects, 2);
    }

    /// Overwrite (PUT onto an existing key / same-bucket REPLACE copy)
    /// = one combined delta: subtract the old footprint, add the new,
    /// objects unchanged.
    #[test]
    fn replace_is_swap_not_double_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = tmp_ledger(&dir);
        ledger.apply_delta("a", 1000, 100, 1);
        // Overwrite with a bigger object: -old +new in one delta.
        ledger.apply_delta("a", 2000 - 1000, 250 - 100, 0);
        let snap = ledger.snapshot();
        assert_eq!(
            snap.buckets["a"],
            BucketTotals {
                original_bytes: 2000,
                stored_bytes: 250,
                objects: 1
            }
        );
    }

    /// Non-S4 object delete (no `s4-*` metadata): the service probes
    /// `original = stored = Content-Length` and the ledger just sees a
    /// symmetric subtraction — net savings contribution zero.
    #[test]
    fn non_s4_object_delete_is_symmetric() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = tmp_ledger(&dir);
        ledger.apply_delta("a", 1000, 100, 1);
        // A backend-direct (non-S4) 300-byte object deleted via the
        // gateway: original == stored == 300 — but it was never added,
        // so the subtraction clamps stored at 0 only if it would go
        // negative. Seed it first to model "added as non-S4" symmetry.
        ledger.apply_delta("a", 300, 300, 1);
        ledger.apply_delta("a", -300, -300, -1);
        let snap = ledger.snapshot();
        assert_eq!(
            snap.buckets["a"],
            BucketTotals {
                original_bytes: 1000,
                stored_bytes: 100,
                objects: 1
            }
        );
    }

    #[test]
    fn signed_delta_math() {
        assert_eq!(signed_delta(100, 40), 60);
        assert_eq!(signed_delta(40, 100), -60);
        assert_eq!(signed_delta(7, 7), 0);
        assert_eq!(signed_delta(u64::MAX, 0), i64::MAX);
        assert_eq!(signed_delta(0, u64::MAX), i64::MIN);
    }

    /// Subtraction below zero clamps instead of wrapping (probe-race
    /// drift must never produce a 2^64-ish counter).
    #[test]
    fn subtraction_clamps_at_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = tmp_ledger(&dir);
        ledger.apply_delta("a", 100, 10, 1);
        ledger.apply_delta("a", -500, -500, -5);
        let snap = ledger.snapshot();
        assert_eq!(snap.buckets["a"], BucketTotals::default());
    }

    /// load → mutate → flush → reload must round-trip exactly (the
    /// state-file contract behind `--savings-ledger-state-file`).
    #[test]
    fn state_file_roundtrip_load_mutate_flush_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.json");
        // Seed: a snapshot already on disk (previous gateway run).
        let mut seed = LedgerSnapshot::default();
        seed.buckets.insert(
            "old".into(),
            BucketTotals {
                original_bytes: 42,
                stored_bytes: 21,
                objects: 1,
            },
        );
        std::fs::write(&path, seed.to_json().expect("seed json")).expect("seed write");

        // Boot path: load_or_fresh + attach (exactly what main.rs does).
        let loaded: LedgerSnapshot =
            crate::state_loader::load_or_fresh("savings_ledger", &path, LedgerSnapshot::from_json);
        assert_eq!(loaded, seed, "boot load must restore the seed snapshot");
        let ledger = SavingsLedger::attach(loaded, path.clone());

        // Mutate — apply_delta flushes event-driven, no explicit call.
        ledger.apply_delta("new", 1_000_000, 30_000, 3);
        let in_memory = ledger.snapshot();

        // Reload from disk: must equal the in-memory state.
        let raw = std::fs::read_to_string(&path).expect("reload read");
        let reloaded = LedgerSnapshot::from_json(&raw).expect("reload parse");
        assert_eq!(reloaded, in_memory, "flush must persist every mutation");
        assert_eq!(reloaded.buckets["old"].original_bytes, 42);
        assert_eq!(reloaded.buckets["new"].objects, 3);
    }

    /// Corrupted state file: `load_or_fresh` falls back to fresh
    /// counters (per-manager fault isolation, v0.8.4 #72) and leaves
    /// the bytes in place.
    #[test]
    fn corrupted_state_file_starts_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, "{not json").expect("write");
        let loaded: LedgerSnapshot =
            crate::state_loader::load_or_fresh("savings_ledger", &path, LedgerSnapshot::from_json);
        assert_eq!(loaded, LedgerSnapshot::default());
        assert_eq!(
            std::fs::read_to_string(&path).expect("file kept"),
            "{not json",
            "operator's bytes must be left in place for inspection"
        );
    }

    #[test]
    fn report_math_and_shapes() {
        let mut snap = LedgerSnapshot::default();
        snap.buckets.insert(
            "logs".into(),
            BucketTotals {
                original_bytes: 10 * 1024 * 1024 * 1024, // 10 GiB
                stored_bytes: 1024 * 1024 * 1024,        // 1 GiB
                objects: 100,
            },
        );
        snap.buckets.insert(
            "media".into(),
            BucketTotals {
                original_bytes: 0,
                stored_bytes: 0,
                objects: 0,
            },
        );
        let report = build_savings_report(&snap, 0.023);
        assert_eq!(report.buckets.len(), 2);
        let logs = &report.buckets[0];
        assert_eq!(logs.bucket, "logs");
        assert!((logs.savings_ratio - 0.9).abs() < 1e-12);
        // 9 GiB saved × $0.023/GiB-month = $0.207.
        assert!((logs.monthly_savings_usd - 9.0 * 0.023).abs() < 1e-9);
        // Zero-original bucket: ratio 0.0, not NaN.
        let media = &report.buckets[1];
        assert_eq!(media.savings_ratio, 0.0);
        assert_eq!(media.monthly_savings_usd, 0.0);
        // Totals.
        assert_eq!(report.total_objects, 100);
        assert!((report.total_savings_ratio - 0.9).abs() < 1e-12);
        assert!(!report.notes.is_empty(), "honesty notes must be present");

        // JSON shape (the `--format json` contract).
        let v = serde_json::to_value(&report).expect("serialize");
        assert_eq!(v["buckets"][0]["bucket"], "logs");
        assert_eq!(v["buckets"][0]["objects"], 100);
        assert_eq!(v["total_original_bytes"], 10u64 * 1024 * 1024 * 1024);
        assert_eq!(v["total_stored_bytes"], 1024u64 * 1024 * 1024);
        assert_eq!(v["price_per_gb_month"], 0.023);
        assert!(v["notes"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn render_human_mentions_key_figures() {
        let mut snap = LedgerSnapshot::default();
        snap.buckets.insert(
            "b1".into(),
            BucketTotals {
                original_bytes: 2048,
                stored_bytes: 1024,
                objects: 2,
            },
        );
        let report = build_savings_report(&snap, DEFAULT_PRICE_PER_GB_MONTH);
        let txt = render_savings_human(&report);
        assert!(txt.contains("S4 measured savings"));
        assert!(txt.contains("b1"));
        assert!(txt.contains("50.0%"));
        assert!(txt.contains("Notes:"));
        assert!(txt.contains("gateway-traversing writes only"));
    }

    /// `run_savings` on a missing file reports zeros (gateway booted
    /// with the flag but no writes yet) instead of erroring.
    #[test]
    fn run_savings_missing_file_is_zero_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run_savings(
            &dir.path().join("never-written.json"),
            DEFAULT_PRICE_PER_GB_MONTH,
        )
        .expect("missing file is not an error");
        assert_eq!(report.total_objects, 0);
        assert_eq!(report.total_original_bytes, 0);
        assert!(report.buckets.is_empty());
    }

    /// `run_savings` on a corrupt file is a typed parse error (the CLI
    /// must not silently report zeros for a real-but-broken ledger).
    #[test]
    fn run_savings_corrupt_file_is_parse_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, "][").expect("write");
        let err = run_savings(&path, DEFAULT_PRICE_PER_GB_MONTH)
            .expect_err("corrupt ledger must surface");
        assert!(matches!(err, SavingsError::StateFileParse { .. }));
    }
}
