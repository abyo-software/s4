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
//! - **DELETE / overwrite subtraction is marker-gated (v1.2 audit R1
//!   P2)**: every write the gateway *adds* to the ledger is stamped
//!   with the internal `s4-ledger: 1` object metadata (clients cannot
//!   forge it — the PUT/Create paths strip all client-supplied `s4-*`
//!   keys first). The DELETE / overwrite HEAD probe subtracts **only
//!   objects that carry the marker**. Objects without it (backend-
//!   direct PUTs, `s4fs` writes, `s4 migrate` / `s4 recompact` output,
//!   or gateway writes from before the marker was introduced) are
//!   never subtracted — they were never added, and an asymmetric
//!   subtraction would drive the bucket's ratio/$ negative or
//!   meaningless. Each skipped removal is tallied per bucket in
//!   [`LedgerSnapshot::skipped_unaccounted`] and disclosed as a report
//!   note.
//! - **The marker means "the ledger was enabled when this object was
//!   written"**, NOT "this object's bytes are currently in the
//!   counters" (v1.2 audit R2 P3). Two known gaps: (a) a multipart
//!   Complete whose assembled body cannot be fetched or exceeds
//!   `--max-body-bytes` keeps the Create-time marker but skips the add
//!   (WARN-logged at Complete); (b) toggling the flag off→on across an
//!   object's lifetime leaves marker-carrying objects from an earlier
//!   ledger epoch with no matching add in the current state file. In
//!   both cases a later DELETE subtracts through the marker gate bytes
//!   that were never added — the zero-clamp on subtraction plus the
//!   report's ratio floor + drift note are the disclosed guard rails
//!   (counters under-claim, never over-claim).
//! - Cross-bucket replication replicas are written with the marker
//!   **stripped** (the dispatcher hands the destination a marker-less
//!   metadata snapshot), keeping "replicas are not counted" symmetric:
//!   a gateway-routed DELETE of a replica is a tallied skip, not a
//!   phantom subtraction.
//! - For marker-carrying objects the probe resolves `original_bytes`
//!   from `s4-original-size` metadata, falling back to the sidecar for
//!   multipart objects, falling back to `original = stored`. Probes
//!   are best-effort: a raced probe leaves the counters slightly stale
//!   rather than failing the client's request.
//! - Internal S4 objects (`.s4dict/<id>` dictionaries, `<key>.s4index`
//!   sidecars) are **excluded from the ledger as standalone objects**:
//!   sidecar bytes are folded into their main object's stored-bytes
//!   delta (add and subtract symmetrically), and dictionary objects —
//!   including the cross-bucket CopyObject `.s4dict` propagation PUT —
//!   are deliberately not counted at all (they are operator-managed
//!   shared assets, not client data; counting them on copy-in but
//!   never on backend-direct `train-dict` write would be asymmetric).
//!   The probe refuses internal keys outright as defence-in-depth.
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
//!
//! **SIGUSR1 contract (v1.2 audit R1 P3)**: the `main.rs` SIGUSR1
//! dump walk MUST call [`SavingsLedger::flush`] for this manager
//! instead of rendering [`SavingsLedger::to_json`] and writing the
//! file itself. `flush` takes the same `flush_lock` the event-driven
//! writes take, so a dump can never interleave with (or be clobbered
//! by) a concurrent PUT-triggered flush; a bypassing writer could
//! persist an older snapshot over a newer one.

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

impl BucketTotals {
    /// Net saved bytes: `original_bytes - stored_bytes`, floored at 0.
    /// This is the byte-accurate figure — the same quantity the
    /// `--marketplace-metered-savings` hourly meter bills as
    /// GBSavedHours — and it stays exact even when the individual
    /// counters diverge from a point-in-time bucket listing (#151: a
    /// marker-gated overwrite swap subtracts equal amounts from both
    /// sides, so the difference is preserved while the per-column
    /// split is not).
    pub fn saved_bytes(&self) -> u64 {
        self.original_bytes.saturating_sub(self.stored_bytes)
    }
}

/// On-disk snapshot shape (`--savings-ledger-state-file` JSON). Also
/// what `s4 savings` deserializes — the CLI never needs the live
/// gateway, only this file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    /// Per-bucket cumulative totals. `BTreeMap` for deterministic
    /// serialization + report ordering.
    pub buckets: BTreeMap<String, BucketTotals>,
    /// v1.2 audit R1 P2: per-bucket count of gateway-observed removals
    /// / overwrites of objects that carry **no** `s4-ledger` marker
    /// (backend-direct / `s4fs` / `migrate` / `recompact` / pre-marker
    /// writes). Those objects were never added to the counters, so
    /// their removal is *not* subtracted — this tally discloses how
    /// many such events the report's numbers exclude. Additive field:
    /// `#[serde(default)]` keeps pre-v1.2 state files loading clean.
    #[serde(default)]
    pub skipped_unaccounted: BTreeMap<String, u64>,
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

    /// Sum of [`Self::skipped_unaccounted`] across buckets.
    pub fn total_skipped_unaccounted(&self) -> u64 {
        self.skipped_unaccounted
            .values()
            .fold(0u64, |acc, n| acc.saturating_add(*n))
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
        {
            let mut guard =
                crate::lock_recovery::recover_write(&self.inner, "savings_ledger.buckets");
            let t = guard.buckets.entry(bucket.to_owned()).or_default();
            t.original_bytes = add_clamped(t.original_bytes, original_delta);
            t.stored_bytes = add_clamped(t.stored_bytes, stored_delta);
            t.objects = add_clamped(t.objects, objects_delta);
            // v1.2 audit R1 P3: stamp the Prometheus gauges *inside*
            // the write lock. Stamping after release let two racing
            // mutations publish their gauge sets in the opposite order
            // of their counter commits, leaving /metrics on the older
            // value until the next event.
            crate::metrics::record_ledger_bucket(bucket, t);
        }
        self.flush_best_effort();
    }

    /// v1.2 audit R1 P2: tally one removal / overwrite of an object
    /// that carries no `s4-ledger` marker (so its bytes were never
    /// added and are deliberately not subtracted). Persisted with the
    /// snapshot and surfaced as a report note.
    pub fn record_skipped_unaccounted(&self, bucket: &str) {
        {
            let mut guard =
                crate::lock_recovery::recover_write(&self.inner, "savings_ledger.buckets");
            let n = guard
                .skipped_unaccounted
                .entry(bucket.to_owned())
                .or_default();
            *n = n.saturating_add(1);
        }
        self.flush_best_effort();
    }

    /// Render + atomically write the snapshot to the state-file path.
    /// Serialized via `flush_lock`; render happens inside the lock so
    /// flushes are monotonic (a newer snapshot can never be clobbered
    /// by a slower, older writer).
    ///
    /// v1.2 audit R1 P3: public so the `main.rs` SIGUSR1 dump walk can
    /// persist this manager **through the same lock** the event-driven
    /// flushes take — the SIGUSR1 handler must call this instead of
    /// `to_json()` + its own file write, otherwise a dump racing a PUT
    /// flush can overwrite a newer snapshot with an older render.
    pub fn flush(&self) -> std::io::Result<()> {
        let _guard = crate::lock_recovery::recover_mutex(&self.flush_lock, "savings_ledger.flush");
        let json = self.to_json().map_err(std::io::Error::other)?;
        atomic_write(&self.path, &json)
    }

    /// Event-driven flush wrapper: an I/O / serialize error is logged
    /// (WARN) and the in-memory counters keep serving — same
    /// degradation posture as a failed SIGUSR1 dump.
    fn flush_best_effort(&self) {
        if let Err(e) = self.flush() {
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
    /// #151: `original_bytes - stored_bytes` (floored at 0) — the net
    /// saved bytes. Byte-accurate even when the per-column split is
    /// distorted by a marker-gated overwrite swap (see the fixed
    /// column-semantics report note); the same quantity
    /// `--marketplace-metered-savings` bills as GBSavedHours.
    pub saved_bytes: u64,
    /// `1 - stored/original` (0.0 when `original_bytes == 0`).
    /// v1.2 audit R1: floored at 0.0 — `stored > original` (counter
    /// drift or negative compression gain) renders as 0% saved with a
    /// drift note, never as a negative percentage.
    pub savings_ratio: f64,
    /// `(original - stored) / GiB × price` — what the bucket would
    /// additionally cost per month if the same logical bytes were
    /// stored uncompressed. Floored at 0.0 (same drift rule as
    /// [`Self::savings_ratio`]).
    pub monthly_savings_usd: f64,
    /// v1.2 audit R1 P2: removals / overwrites of non-ledger-managed
    /// objects (no `s4-ledger` marker) observed in this bucket and
    /// deliberately not subtracted from the counters.
    pub skipped_unaccounted: u64,
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
    /// #151: global `original - stored` (floored at 0) — the
    /// byte-accurate net figure. Computed from the global totals, NOT
    /// as the sum of the per-bucket floors, so it matches exactly how
    /// the `--marketplace-metered-savings` quantity is derived (drift
    /// in one bucket nets against savings in another).
    pub total_saved_bytes: u64,
    /// `1 - total_stored/total_original` (0.0 when nothing recorded;
    /// floored at 0.0 on drift — see [`BucketSavings::savings_ratio`]).
    pub total_savings_ratio: f64,
    pub price_per_gb_month: f64,
    pub total_monthly_savings_usd: f64,
    /// v1.2 audit R1 P2: sum of [`BucketSavings::skipped_unaccounted`].
    pub total_skipped_unaccounted: u64,
    /// Fixed honesty notes — always read these before quoting the
    /// numbers anywhere.
    pub notes: Vec<String>,
}

/// v1.2 audit R1: floored at 0.0 — a `stored > original` bucket
/// (counter drift from probe races, or honest negative compression
/// gain on incompressible data) must never render a negative
/// percentage / negative dollar figure; the clamp is disclosed via a
/// dedicated drift note in [`build_savings_report`].
fn savings_ratio(original: u64, stored: u64) -> f64 {
    if original == 0 {
        0.0
    } else {
        (1.0 - (stored as f64 / original as f64)).max(0.0)
    }
}

fn monthly_savings_usd(original: u64, stored: u64, price_per_gb_month: f64) -> f64 {
    ((original as f64 - stored as f64) / BYTES_PER_GB * price_per_gb_month).max(0.0)
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
            saved_bytes: t.saved_bytes(),
            savings_ratio: savings_ratio(t.original_bytes, t.stored_bytes),
            monthly_savings_usd: monthly_savings_usd(
                t.original_bytes,
                t.stored_bytes,
                price_per_gb_month,
            ),
            skipped_unaccounted: snapshot
                .skipped_unaccounted
                .get(bucket)
                .copied()
                .unwrap_or(0),
        })
        .collect();
    let g = snapshot.global_totals();
    let total_skipped = snapshot.total_skipped_unaccounted();
    let mut notes = vec![
        "the ledger observes gateway-traversing writes only: backend-direct writes, \
         `s4 migrate`, and `s4 recompact` (both backend-direct) are not reflected; \
         `recompact` savings appear only after the gateway next rewrites the object"
            .to_owned(),
        "aborted multipart uploads are never counted (parts are recorded at Complete \
         time only); cross-bucket replication replicas are not counted"
            .to_owned(),
        "DELETE / overwrite subtraction applies only to objects the gateway itself \
         accounted (internal `s4-ledger` marker); removals of non-ledger-managed \
         objects are skipped and tallied separately. The HEAD probe is best-effort — \
         a raced probe leaves the counters slightly stale rather than failing the \
         request. The marker records that the ledger was enabled at write time, not \
         that the bytes are in the counters: a multipart Complete skipped for an \
         oversized/unfetchable body, or a flag toggled off->on, can leave \
         marker-carrying objects that were never added — their later removal \
         subtracts with clamping at zero (under-claim, surfaced by the drift note \
         when it floors a bucket)"
            .to_owned(),
        "storage bytes only: request, egress, and (on GPU deployments) compute costs \
         are unchanged by S4"
            .to_owned(),
        // #151: the live Metered Savings E2E (2026-07-08) showed a
        // bucket at objects=0 / original=net-delta / stored=sidecar-only
        // after retried multipart Completes — the counters were correct
        // as counters, but the columns read like an inventory listing.
        "column semantics: `objects` / `original` / `stored` are cumulative \
         accounted-write counters, not a point-in-time bucket inventory — an \
         overwrite of an already-accounted object adds 0 objects and only the \
         footprint delta, so after churn (notably retried multipart Completes) \
         the per-column split can diverge from what a bucket listing shows. \
         `saved` (= original - stored) is the byte-accurate net figure — the \
         `--marketplace-metered-savings` billing quantity — and is the number \
         to quote"
            .to_owned(),
    ];
    if total_skipped > 0 {
        notes.push(format!(
            "{total_skipped} deletion(s)/overwrite(s) of non-ledger-managed objects \
             (no `s4-ledger` marker: backend-direct / s4fs / migrate / recompact / \
             pre-v1.2 writes) were observed and NOT subtracted from these counters"
        ));
    }
    // v1.2 audit R1: disclose every bucket whose raw counters would
    // have produced a negative ratio/$ (floored to 0 above) — counter
    // drift or honest negative compression gain, either way the 0% is
    // a clamp, not a measurement.
    let drifted: Vec<&str> = snapshot
        .buckets
        .iter()
        .filter(|(_, t)| t.stored_bytes > t.original_bytes)
        .map(|(b, _)| b.as_str())
        .collect();
    if !drifted.is_empty() {
        notes.push(format!(
            "bucket(s) {}: stored_bytes exceeds original_bytes — savings ratio and \
             $/month are floored at 0 (counter drift from probe races / unaccounted \
             writes / marker-carrying objects that were never added (multipart adds \
             skipped over --max-body-bytes, ledger flag toggled off->on), or negative \
             compression gain on incompressible data)",
            drifted.join(", ")
        ));
    }
    // #151: disclose buckets carrying bytes with zero accounted objects
    // — the live signature of a multipart Complete that was interrupted
    // after the backend commit (the `s4-ledger` marker is stamped at
    // Create time; the counter add is owed at Complete) and then
    // retried by the client: the retry is accounted as an overwrite of
    // the uncounted first attempt, so `objects` gains 0, `stored` gains
    // only the sidecar bytes, and `original` absorbs the difference.
    // Net `saved` stays exact through the swap.
    let zero_object_bytes: Vec<&str> = snapshot
        .buckets
        .iter()
        .filter(|(_, t)| t.objects == 0 && (t.original_bytes > 0 || t.stored_bytes > 0))
        .map(|(b, _)| b.as_str())
        .collect();
    if !zero_object_bytes.is_empty() {
        notes.push(format!(
            "bucket(s) {}: bytes recorded with 0 accounted objects — typically a \
             multipart Complete interrupted after the backend commit (crash/OOM) \
             and then retried by the client; the retry is accounted as an overwrite \
             of the uncounted first attempt, so `objects`/`original`/`stored` do \
             not reflect the live bucket for these bucket(s). `saved` is exact",
            zero_object_bytes.join(", ")
        ));
    }
    SavingsReport {
        buckets,
        total_objects: g.objects,
        total_original_bytes: g.original_bytes,
        total_stored_bytes: g.stored_bytes,
        total_saved_bytes: g.saved_bytes(),
        total_savings_ratio: savings_ratio(g.original_bytes, g.stored_bytes),
        price_per_gb_month,
        total_monthly_savings_usd: monthly_savings_usd(
            g.original_bytes,
            g.stored_bytes,
            price_per_gb_month,
        ),
        total_skipped_unaccounted: total_skipped,
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
    let (snapshot, state_file_missing) = match raw {
        // Missing / empty file = a gateway that has the flag but hasn't
        // served a write yet — report zeros, not an error, but say so
        // (v1.2 audit R1 P3: a silent all-zero report for a mistyped
        // path is indistinguishable from "no savings").
        None => (LedgerSnapshot::default(), true),
        Some(json) => (
            LedgerSnapshot::from_json(&json).map_err(|e| SavingsError::StateFileParse {
                path: path.display().to_string(),
                cause: e.to_string(),
            })?,
            false,
        ),
    };
    let mut report = build_savings_report(&snapshot, price_per_gb_month);
    if state_file_missing {
        report.notes.insert(
            0,
            format!(
                "no ledger state found at {} — reporting zeros; check the path and \
                 that the gateway runs with --savings-ledger-state-file",
                path.display()
            ),
        );
    }
    Ok(report)
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
    // #151: `saved` (bytes) is the byte-accurate column (= the metered
    // quantity); `objects` / `original` / `stored` are cumulative
    // accounted-write counters and can diverge from a bucket listing
    // after churn — the fixed column-semantics note below says so.
    let _ = writeln!(
        out,
        "  {:<20} {:>8} {:>14} {:>14} {:>14} {:>7} {:>12}",
        "bucket", "objects", "original", "stored", "saved", "saved%", "$/month",
    );
    for b in &report.buckets {
        let _ = writeln!(
            out,
            "  {:<20} {:>8} {:>14} {:>14} {:>14} {:>6.1}% {:>12.2}",
            b.bucket,
            b.objects,
            human_bytes(b.original_bytes),
            human_bytes(b.stored_bytes),
            human_bytes(b.saved_bytes),
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

    /// v1.2 audit R1 P2: a removal of a non-ledger-managed object (no
    /// `s4-ledger` marker) is NOT subtracted — the service calls
    /// `record_skipped_unaccounted` instead, the tally persists with
    /// the snapshot, and the report discloses it as a note.
    #[test]
    fn skipped_unaccounted_tally_persists_and_reports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.json");
        let ledger = SavingsLedger::attach(LedgerSnapshot::default(), path.clone());
        ledger.apply_delta("a", 1000, 100, 1);
        ledger.record_skipped_unaccounted("a");
        ledger.record_skipped_unaccounted("a");
        ledger.record_skipped_unaccounted("b");
        // Counters untouched by the skips.
        let snap = ledger.snapshot();
        assert_eq!(
            snap.buckets["a"],
            BucketTotals {
                original_bytes: 1000,
                stored_bytes: 100,
                objects: 1
            }
        );
        assert_eq!(snap.skipped_unaccounted["a"], 2);
        assert_eq!(snap.skipped_unaccounted["b"], 1);
        assert_eq!(snap.total_skipped_unaccounted(), 3);
        // Persists through the state file (event-driven flush).
        let reloaded = LedgerSnapshot::from_json(&std::fs::read_to_string(&path).expect("read"))
            .expect("parse");
        assert_eq!(reloaded, snap);
        // ...and the report carries the per-bucket tally + a note.
        let report = build_savings_report(&snap, DEFAULT_PRICE_PER_GB_MONTH);
        assert_eq!(report.total_skipped_unaccounted, 3);
        let a = report
            .buckets
            .iter()
            .find(|b| b.bucket == "a")
            .expect("bucket a");
        assert_eq!(a.skipped_unaccounted, 2);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("NOT subtracted") && n.contains("3 deletion(s)")),
            "skip disclosure note missing: {:?}",
            report.notes
        );
    }

    /// Pre-v1.2 state files (no `skipped_unaccounted` field) must load
    /// clean — the field is additive with `#[serde(default)]`.
    #[test]
    fn pre_marker_state_file_loads_with_default_skip_tally() {
        let json = r#"{"buckets":{"a":{"original_bytes":10,"stored_bytes":5,"objects":1}}}"#;
        let snap = LedgerSnapshot::from_json(json).expect("legacy snapshot parses");
        assert!(snap.skipped_unaccounted.is_empty());
        assert_eq!(snap.buckets["a"].objects, 1);
    }

    /// v1.2 audit R1 P3: `flush()` is the public SIGUSR1 entry point —
    /// it must persist the exact current snapshot through the same
    /// lock the event-driven flushes use.
    #[test]
    fn public_flush_persists_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.json");
        let ledger = SavingsLedger::attach(LedgerSnapshot::default(), path.clone());
        ledger.apply_delta("a", 500, 50, 1);
        // Wipe the file to prove flush() rewrites it.
        std::fs::remove_file(&path).expect("remove");
        ledger.flush().expect("flush");
        let reloaded = LedgerSnapshot::from_json(&std::fs::read_to_string(&path).expect("read"))
            .expect("parse");
        assert_eq!(reloaded, ledger.snapshot());
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
    /// with the flag but no writes yet) instead of erroring — and
    /// (v1.2 audit R1 P3) says so in the first note, so a mistyped
    /// `--state-file` path is never a silent all-zero report.
    #[test]
    fn run_savings_missing_file_is_zero_report_with_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("never-written.json");
        let report = run_savings(&missing, DEFAULT_PRICE_PER_GB_MONTH)
            .expect("missing file is not an error");
        assert_eq!(report.total_objects, 0);
        assert_eq!(report.total_original_bytes, 0);
        assert!(report.buckets.is_empty());
        let first = report.notes.first().expect("notes non-empty");
        assert!(
            first.contains("no ledger state found")
                && first.contains(missing.to_str().expect("utf8"))
                && first.contains("--savings-ledger-state-file"),
            "missing-file note must lead the notes: {first}"
        );
        // The note also renders in the human table.
        let txt = render_savings_human(&report);
        assert!(txt.contains("no ledger state found"));
    }

    /// v1.2 audit R1: drifted counters (stored > original) floor the
    /// ratio / $ at zero and add a drift note — never a negative
    /// percentage or negative dollars.
    #[test]
    fn drifted_counters_floor_at_zero_with_note() {
        let mut snap = LedgerSnapshot::default();
        snap.buckets.insert(
            "drifty".into(),
            BucketTotals {
                original_bytes: 100,
                stored_bytes: 250, // stored > original
                objects: 1,
            },
        );
        let report = build_savings_report(&snap, DEFAULT_PRICE_PER_GB_MONTH);
        let b = &report.buckets[0];
        assert_eq!(b.savings_ratio, 0.0, "ratio must be floored, not negative");
        assert_eq!(
            b.monthly_savings_usd, 0.0,
            "$ must be floored, not negative"
        );
        assert_eq!(report.total_savings_ratio, 0.0);
        assert_eq!(report.total_monthly_savings_usd, 0.0);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("drifty") && n.contains("floored at 0")),
            "drift note missing: {:?}",
            report.notes
        );
        // Healthy snapshots carry no drift note.
        let mut healthy = LedgerSnapshot::default();
        healthy.buckets.insert(
            "ok".into(),
            BucketTotals {
                original_bytes: 100,
                stored_bytes: 50,
                objects: 1,
            },
        );
        let healthy_report = build_savings_report(&healthy, DEFAULT_PRICE_PER_GB_MONTH);
        assert!(
            !healthy_report
                .notes
                .iter()
                .any(|n| n.contains("floored at 0")),
            "no drift note expected for healthy counters"
        );
    }

    /// #151: live repro (2026-07-08, v1.5.0 Metered Savings E2E) — two
    /// 2 GiB multipart uploads whose first Complete attempt died after
    /// the backend commit (gateway OOM, #148) and were then retried by
    /// the client. The retry is accounted as a marker-gated overwrite
    /// of the uncounted first attempt, so the recorded counters are:
    /// `objects` 0, `original` = the net swap delta
    /// (2 × (2 GiB − 160 MiB padded)), `stored` = sidecar bytes only
    /// (2 × 1,100). Net saved is byte-accurate; the per-column split is
    /// not a bucket inventory. The report must (a) expose the accurate
    /// `saved` figure as its own column/field, (b) carry a fixed
    /// column-semantics note, and (c) call out the
    /// bytes-with-zero-objects signature per bucket.
    #[test]
    fn multipart_retry_swap_report_discloses_column_semantics() {
        const TWO_GIB: u64 = 2 * 1024 * 1024 * 1024; // 2,147,483,648
        const PADDED: u64 = 160 * 1024 * 1024; // 167,772,160 (32 × 5 MiB parts)
        const SIDECAR: u64 = 1_100;
        let mut snap = LedgerSnapshot::default();
        snap.buckets.insert(
            "s4-meter-e2e".into(),
            BucketTotals {
                original_bytes: 2 * (TWO_GIB - PADDED), // 3,959,422,976
                stored_bytes: 2 * SIDECAR,              // 2,200
                objects: 0,
            },
        );
        let report = build_savings_report(&snap, DEFAULT_PRICE_PER_GB_MONTH);
        let saved = 2 * (TWO_GIB - PADDED - SIDECAR); // 3,959,420,776
        let b = &report.buckets[0];
        // The one always-accurate number (the GBSavedHours metering
        // source, verified live to the byte).
        assert_eq!(b.saved_bytes, saved);
        assert_eq!(report.total_saved_bytes, saved);
        // Fixed column-semantics note — always present, not just on
        // pathological snapshots.
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("cumulative accounted-write counters")),
            "column-semantics note missing: {:?}",
            report.notes
        );
        // Targeted symptom note naming the bucket.
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("s4-meter-e2e") && n.contains("0 accounted objects")),
            "zero-object symptom note missing: {:?}",
            report.notes
        );
        // Human table: a `saved` bytes column (accurate) next to the
        // ratio column, which is explicitly labelled as a percentage.
        let txt = render_savings_human(&report);
        assert!(
            txt.contains("saved%"),
            "ratio header must say saved%: {txt}"
        );
        assert!(
            txt.contains("3.7 GiB"),
            "saved column must render the net bytes: {txt}"
        );
        // JSON contract: additive fields.
        let v = serde_json::to_value(&report).expect("serialize");
        assert_eq!(v["buckets"][0]["saved_bytes"], saved);
        assert_eq!(v["total_saved_bytes"], saved);
    }

    /// #151 guard rails: the zero-object symptom note fires only on
    /// buckets that actually carry bytes with no accounted objects —
    /// healthy buckets and genuinely-empty buckets stay note-free, and
    /// drifted counters (stored > original) floor `saved_bytes` at 0.
    #[test]
    fn zero_object_note_scope_and_saved_bytes_floor() {
        let mut snap = LedgerSnapshot::default();
        snap.buckets.insert(
            "healthy".into(),
            BucketTotals {
                original_bytes: 2048,
                stored_bytes: 1024,
                objects: 2,
            },
        );
        snap.buckets.insert("empty".into(), BucketTotals::default());
        snap.buckets.insert(
            "drifty".into(),
            BucketTotals {
                original_bytes: 100,
                stored_bytes: 250,
                objects: 1,
            },
        );
        let report = build_savings_report(&snap, DEFAULT_PRICE_PER_GB_MONTH);
        assert!(
            !report
                .notes
                .iter()
                .any(|n| n.contains("0 accounted objects")),
            "no zero-object note expected: {:?}",
            report.notes
        );
        let healthy = report
            .buckets
            .iter()
            .find(|b| b.bucket == "healthy")
            .expect("healthy bucket");
        assert_eq!(healthy.saved_bytes, 1024);
        let drifty = report
            .buckets
            .iter()
            .find(|b| b.bucket == "drifty")
            .expect("drifty bucket");
        assert_eq!(
            drifty.saved_bytes, 0,
            "saved_bytes must floor at 0 on drift"
        );
        // The total is `global original - global stored` (2148 - 1274),
        // NOT the sum of per-bucket floors — mirroring exactly how the
        // metered-savings quantity is computed from the global totals
        // (drift in one bucket nets against savings in another).
        assert_eq!(report.total_saved_bytes, 874);
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
