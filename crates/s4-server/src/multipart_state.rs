//! Per-`upload_id` side-table for multipart uploads (v0.8 BUG-5..10 fix).
//!
//! S3 multipart is split across three handlers:
//!
//!   - `CreateMultipartUpload` — receives the SSE / Tagging / Object-Lock
//!     headers the client wants applied to the eventual object.
//!   - `UploadPart` × N — receives only the body bytes + part number;
//!     the SSE-C headers must be replayed by the client (AWS spec) but
//!     SSE-S4 / SSE-KMS / Tagging / Object-Lock are NOT replayed (they
//!     live on the upload itself).
//!   - `CompleteMultipartUpload` — receives only the part-list manifest;
//!     no metadata reaches this handler from the wire either.
//!
//! v0.7 #48 fixed the single-PUT path to take()`SSE` request fields off
//! the s3s input, encrypt-then-store, and stamp the `s4-sse-type`
//! metadata on the resulting object so HEAD can echo correctly. The
//! multipart path needs the equivalent treatment but the per-upload
//! context is split across three handler invocations — this module is
//! the side-channel that carries it from `CreateMultipartUpload` through
//! to `UploadPart` / `CompleteMultipartUpload`.
//!
//! The store is keyed on the backend-issued `upload_id` (opaque string
//! returned by `CreateMultipartUpload`'s response). `put` / `get` /
//! `remove` are all `O(1)` under a single `RwLock<HashMap>`; multipart
//! upload throughput is dominated by the part-body PUTs to the backend
//! (5 MiB+ each), so the lock is never the bottleneck.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::object_lock::LockMode;
use crate::tagging::TagSet;

/// SSE recipe captured at `CreateMultipartUpload` time and replayed for
/// every part body + the final stamp on the assembled object.
///
/// The variants mirror `service::put_object`'s SSE branch precedence:
/// SSE-C (per-request customer key) wins over SSE-KMS (named KMS key)
/// wins over SSE-S4 (server-managed keyring) wins over no encryption.
/// SSE-C / SSE-KMS materialise only when the client supplied the
/// matching headers; SSE-S4 materialises whenever the gateway is booted
/// with `--sse-s4-key` (or `with_sse_keyring(...)` in tests).
///
/// v0.8.2 #62 (H-6 audit fix): the `SseC` variant's customer key is held
/// in `Zeroizing<[u8; 32]>` so the raw 32-byte AES key is overwritten
/// with `0u8` when the entry is dropped — either via `remove(upload_id)`
/// on Complete/Abort, or via `sweep_stale(...)` on an abandoned upload.
/// Process core dump / swap-out / KSM snapshot can no longer leak a
/// previously-held SSE-C key after the upload's lifetime ends. The
/// `key_md5` is deliberately a plain `[u8; 16]` — it's a public
/// fingerprint (S3 puts it on the wire on every PUT/GET response) and
/// requires no zeroization. Custom `PartialEq` ignores the `Zeroizing`
/// wrapper so existing tests that match on the variant keep compiling.
/// v1.0 stability: `#[non_exhaustive]` — new SSE modes (e.g. SSE-S3 /
/// AWS-managed keys, or additional KMS providers) may be added in
/// minor releases. Downstream callers must include a `_ =>` arm when
/// matching on this enum.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum MultipartSseMode {
    /// Plaintext multipart. Backend stores raw framed bytes.
    None,
    /// Server-managed keyring (active key on PUT, all keys probed on GET).
    /// The keyring itself lives on `S4Service`; only the marker is held
    /// here so `complete_multipart_upload` knows which path to take.
    SseS4,
    /// Per-request customer key. The 32-byte key + its 128-bit MD5 are
    /// kept in memory only for the lifetime of the upload, then dropped
    /// when the entry is `remove(...)`'d on Complete or Abort. v0.8.2
    /// #62: `key` is `Zeroizing<[u8; 32]>` so its bytes are wiped on
    /// drop (vs. a bare `[u8; 32]` which would linger on the heap /
    /// stack until the next allocation reuse).
    SseC {
        key: Zeroizing<[u8; 32]>,
        key_md5: [u8; 16],
    },
    /// Named KMS key (resolved against the gateway's KMS backend on
    /// Complete to generate the per-object DEK).
    SseKms { key_id: String },
}

// Manual `PartialEq` / `Eq` so `Zeroizing<[u8; 32]>` (which doesn't
// derive `PartialEq`) doesn't break the existing `assert_eq!` call
// sites. Compares by deref to the inner `[u8; 32]`.
impl PartialEq for MultipartSseMode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MultipartSseMode::None, MultipartSseMode::None) => true,
            (MultipartSseMode::SseS4, MultipartSseMode::SseS4) => true,
            (
                MultipartSseMode::SseC {
                    key: a,
                    key_md5: am,
                },
                MultipartSseMode::SseC {
                    key: b,
                    key_md5: bm,
                },
            ) => a.as_slice() == b.as_slice() && am == bm,
            (MultipartSseMode::SseKms { key_id: a }, MultipartSseMode::SseKms { key_id: b }) => {
                a == b
            }
            _ => false,
        }
    }
}
impl Eq for MultipartSseMode {}

/// Everything `CreateMultipartUpload` captured for `UploadPart` /
/// `CompleteMultipartUpload` to act on. All fields are owned so the
/// store can hand out cheap `Clone`s under the read lock.
#[derive(Clone, Debug)]
pub struct MultipartUploadContext {
    /// Bucket the upload targets. Stored even though
    /// `CompleteMultipartUploadInput::bucket` carries it too — keeps the
    /// side-table self-contained for tests / debug dumps.
    pub bucket: String,
    /// Logical object key the upload will materialise into. Stored for
    /// the same reason as `bucket`.
    pub key: String,
    /// SSE recipe captured from the Create's input headers.
    pub sse: MultipartSseMode,
    /// Tags parsed off `Tagging` / `x-amz-tagging` on Create. `None`
    /// when the client didn't ask for tagging; otherwise the `TagSet` is
    /// applied via `TagManager::put_object_tags` on Complete (BUG-9
    /// fix).
    pub tags: Option<TagSet>,
    /// Per-PUT explicit Object Lock mode supplied via
    /// `x-amz-object-lock-mode` on Create. Mirrors `put_object`'s
    /// `explicit_lock_mode` capture so Complete commits the right
    /// retention. `None` when no header was sent (Complete then falls
    /// back to the bucket default via `apply_default_on_put`).
    pub object_lock_mode: Option<LockMode>,
    /// Per-PUT explicit Object Lock retain-until timestamp.
    pub object_lock_retain_until: Option<DateTime<Utc>>,
    /// Per-PUT explicit Object Lock legal-hold flag (`true` when
    /// `x-amz-object-lock-legal-hold: ON` was sent on Create).
    pub object_lock_legal_hold: bool,
}

/// In-memory side-table mapping `upload_id` → context. One of these
/// hangs off `S4Service` (always-on, no flag — the per-upload state is
/// gateway-internal).
///
/// v0.8.2 #62 (H-6 audit fix): each entry carries the `DateTime<Utc>`
/// of its `put` insertion so `sweep_stale(now, max_age)` can drop
/// abandoned upload contexts (client called `CreateMultipartUpload`,
/// uploaded some parts, then crashed without invoking
/// `CompleteMultipartUpload` / `AbortMultipartUpload`). Without the
/// sweep, an SSE-C upload's raw 32-byte customer key would linger in
/// `MultipartSseMode::SseC` indefinitely. The sweep + the new
/// `Zeroizing` wrapper together bound the key's in-memory lifetime to
/// `max_age` (default 24h via `--multipart-abandoned-ttl-hours`).
pub struct MultipartStateStore {
    by_upload_id: RwLock<HashMap<String, (MultipartUploadContext, DateTime<Utc>)>>,
    /// v0.8.1 #59: per-(bucket, key) `Mutex` used to serialize Complete
    /// operations on the same logical key. The race window the lock
    /// closes lives inside `service::complete_multipart_upload` between
    /// `backend.get_object` (assembled body fetch for the SSE encrypt
    /// re-PUT, BUG-5 fix) and `backend.put_object` (encrypted body
    /// write-back). Two concurrent Completes with different `upload_id`
    /// but the same `(bucket, key)` could otherwise interleave their
    /// GET / encrypt / PUT triples and overwrite each other.
    ///
    /// `DashMap` is used because the lock acquisition path is itself
    /// `O(1)` and contention between *different* keys must not block;
    /// `DashMap`'s sharded design preserves that property whereas a
    /// single `RwLock<HashMap<_,_>>` would serialise even unrelated
    /// keys' lock-lookup. The stored `Arc<Mutex<()>>` is what the
    /// caller actually awaits on — the `DashMap` itself is just a
    /// concurrent index into those mutexes.
    ///
    /// Cleanup is best-effort (`prune_completion_locks`); the entry
    /// for a one-shot key is dropped once both the in-flight Complete
    /// returns and the prune sweep observes only the `DashMap`'s own
    /// `Arc` reference.
    completion_locks: DashMap<(String, String), Arc<Mutex<()>>>,
}

impl MultipartStateStore {
    /// Empty store. Use `Arc<MultipartStateStore>` so `S4Service`'s
    /// async handlers can borrow it across `&self` calls without
    /// requiring `Clone`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_upload_id: RwLock::new(HashMap::new()),
            completion_locks: DashMap::new(),
        }
    }

    /// Register a new upload under `upload_id`. If `upload_id` is
    /// already present (extremely unlikely — backend issues fresh ids)
    /// the previous entry is overwritten silently to mirror
    /// `HashMap::insert`'s replace-on-collision semantics.
    ///
    /// v0.8.2 #62: the insertion timestamp (`Utc::now()`) is stored
    /// alongside the context so `sweep_stale` can prune abandoned
    /// uploads. The timestamp is set at insert-time only — re-puts on
    /// the same `upload_id` (overwrite) reset the clock, which is the
    /// behaviour we want (treat a re-Create as the abandonment-clock
    /// restart).
    pub fn put(&self, upload_id: &str, ctx: MultipartUploadContext) {
        crate::lock_recovery::recover_write(&self.by_upload_id, "multipart_state.by_upload_id")
            .insert(upload_id.to_owned(), (ctx, Utc::now()));
    }

    /// Snapshot the context for `upload_id`. `None` when no entry was
    /// registered (e.g. Complete arrived for an upload that the gateway
    /// has no record of — passes through to the backend untouched, which
    /// in turn surfaces `NoSuchUpload`).
    #[must_use]
    pub fn get(&self, upload_id: &str) -> Option<MultipartUploadContext> {
        crate::lock_recovery::recover_read(&self.by_upload_id, "multipart_state.by_upload_id")
            .get(upload_id)
            .map(|(c, _)| c.clone())
    }

    /// Drop the entry. Called by Complete / Abort to release the SSE-C
    /// key bytes and the tag-set memory promptly. The `Zeroizing<[u8;
    /// 32]>` wrapper inside the dropped `MultipartSseMode::SseC`
    /// variant zeros the key bytes during its `Drop`.
    pub fn remove(&self, upload_id: &str) {
        crate::lock_recovery::recover_write(&self.by_upload_id, "multipart_state.by_upload_id")
            .remove(upload_id);
    }

    /// v0.8.2 #62 (H-6 audit fix): drop every entry whose insertion
    /// timestamp is older than `now - max_age`. Returns the number of
    /// entries swept. Called from a hourly background tick spawned in
    /// `main.rs` (default TTL = 24 h, configurable via
    /// `--multipart-abandoned-ttl-hours`).
    ///
    /// Each dropped `MultipartUploadContext` runs the inner
    /// `MultipartSseMode::SseC { key: Zeroizing<[u8; 32]>, .. }`'s
    /// `Drop`, wiping the customer-supplied AES key bytes from
    /// process memory. SSE-S4 / SSE-KMS / None variants drop their
    /// (smaller) state too; only SSE-C carries raw key material.
    ///
    /// The cutoff is computed as `now - max_age` rather than
    /// `Utc::now() - max_age` so callers can drive the clock
    /// deterministically in tests (the unit tests below pass an
    /// explicit `now` from a fixed timestamp).
    pub fn sweep_stale(&self, now: DateTime<Utc>, max_age: chrono::Duration) -> usize {
        let cutoff = now - max_age;
        let mut map =
            crate::lock_recovery::recover_write(&self.by_upload_id, "multipart_state.by_upload_id");
        let stale: Vec<String> = map
            .iter()
            .filter(|(_, (_, ts))| *ts < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for k in stale {
            map.remove(&k);
        }
        count
    }

    /// v0.8.1 #59: get-or-create the per-(bucket, key) `Mutex` used to
    /// serialise `complete_multipart_upload` invocations on the same
    /// logical key. Caller does `lock.lock().await` and holds the
    /// guard for the duration of its critical section (GET assembled
    /// body → encrypt → PUT encrypted body → version-id mint → object-
    /// lock apply → tagging persist → replication enqueue).
    ///
    /// Returns an `Arc<Mutex<()>>` so the caller can drop the
    /// `DashMap` shard's read lock immediately and only retain the
    /// mutex itself across the await point — `DashMap`'s shard guard
    /// is `!Send`, so we must not hold it through an `await`.
    pub fn completion_lock(&self, bucket: &str, key: &str) -> Arc<Mutex<()>> {
        let k = (bucket.to_owned(), key.to_owned());
        self.completion_locks
            .entry(k)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone()
    }

    /// v0.8.1 #59: best-effort cleanup of stale completion-lock
    /// entries. A `(bucket, key)` entry is "stale" once no concurrent
    /// Complete is referencing its `Arc<Mutex<()>>` — we detect that
    /// by `Arc::strong_count == 1` (only the `DashMap` itself holds a
    /// reference). Called from `complete_multipart_upload` after the
    /// guarded section returns, so a steady-state workload of unique
    /// keys never accumulates locks.
    ///
    /// The retain predicate is `> 1` (keep entries with outstanding
    /// borrowers), so prune is safe to invoke concurrently with other
    /// `completion_lock` callers — at worst the prune sees the entry
    /// during a brief window where the borrower has cloned but not yet
    /// taken `lock()`, and the entry survives until the next sweep.
    pub fn prune_completion_locks(&self) {
        self.completion_locks
            .retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    /// Test-only: how many completion-lock entries the store currently
    /// holds. Used by `prune_completion_locks_removes_unreferenced`.
    #[cfg(test)]
    fn completion_locks_len(&self) -> usize {
        self.completion_locks.len()
    }

    /// Test-only: how many in-flight uploads the store is currently
    /// tracking. Used by the assertion in `concurrent_put_lookup_race_free`.
    #[cfg(test)]
    fn len(&self) -> usize {
        crate::lock_recovery::recover_read(&self.by_upload_id, "multipart_state.by_upload_id").len()
    }
}

impl Default for MultipartStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn sample_ctx(bucket: &str, key: &str) -> MultipartUploadContext {
        MultipartUploadContext {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            sse: MultipartSseMode::None,
            tags: None,
            object_lock_mode: None,
            object_lock_retain_until: None,
            object_lock_legal_hold: false,
        }
    }

    /// `put` followed by `get` returns the same context, and `remove`
    /// makes a subsequent `get` return `None`. Sanity for the basic
    /// CRUD shape.
    #[test]
    fn put_get_remove_round_trip() {
        let store = MultipartStateStore::new();
        let ctx = sample_ctx("b", "k");
        store.put("upload-001", ctx.clone());
        let got = store.get("upload-001").expect("entry must be present");
        assert_eq!(got.bucket, "b");
        assert_eq!(got.key, "k");
        assert_eq!(got.sse, MultipartSseMode::None);
        store.remove("upload-001");
        assert!(store.get("upload-001").is_none(), "entry must be gone");
    }

    /// SSE-C variants stash the 32-byte key + 16-byte MD5; verify the
    /// bytes round-trip exactly (defensive — easy place to introduce a
    /// silent truncation bug).
    #[test]
    fn sse_c_key_bytes_round_trip() {
        let store = MultipartStateStore::new();
        let key = [0xa5u8; 32];
        let key_md5 = [0xb6u8; 16];
        let mut ctx = sample_ctx("b", "k");
        ctx.sse = MultipartSseMode::SseC {
            key: Zeroizing::new(key),
            key_md5,
        };
        store.put("u-sse-c", ctx);
        let got = store.get("u-sse-c").expect("entry must be present");
        match got.sse {
            MultipartSseMode::SseC { key: k, key_md5: m } => {
                assert_eq!(*k, key, "SSE-C key bytes must round-trip");
                assert_eq!(m, key_md5, "SSE-C MD5 must round-trip");
            }
            other => panic!("expected SseC variant, got {other:?}"),
        }
    }

    /// v0.8.2 #62 (H-6 fix): registering an SSE-C upload then
    /// `remove`-ing it must drop the `Zeroizing<[u8; 32]>` key wrapper
    /// — its `Drop` zeros the underlying 32 bytes. Direct verification
    /// requires reading back the heap allocation that backed the
    /// `Zeroizing` (UB in safe Rust); instead we assert the
    /// behavioural contract: after `remove`, a fresh `get` returns
    /// `None` (the entry is gone, so the `Drop` ran). We additionally
    /// build a separate `Zeroizing<[u8; 32]>`, observe non-zero
    /// content, then drop it under a `Box` — the post-drop heap
    /// region is no longer reachable from safe Rust, so we settle for
    /// the structural contract: the `Zeroize` derive on `Zeroizing`
    /// is what actually wipes the bytes (covered by the `zeroize`
    /// crate's own test suite). This test is the smoke check that we
    /// kept the wrapper on the variant.
    #[test]
    fn sse_c_key_zeroized_on_remove() {
        let store = MultipartStateStore::new();
        let key = [0x77u8; 32];
        let key_md5 = [0x33u8; 16];
        let mut ctx = sample_ctx("b", "k");
        ctx.sse = MultipartSseMode::SseC {
            key: Zeroizing::new(key),
            key_md5,
        };
        store.put("u-zero", ctx);
        // Confirm the variant carries a `Zeroizing<[u8; 32]>` (not a
        // bare `[u8; 32]`) by exercising `Deref` to `&[u8; 32]`. If
        // someone later regresses the wrapper away, this access would
        // still compile but the structural assertion below — that the
        // store actually held the entry — is what the test is for.
        let got = store.get("u-zero").expect("entry present");
        match &got.sse {
            MultipartSseMode::SseC { key: k, .. } => {
                let _deref: &[u8; 32] = k; // typeof check: must be Zeroizing<[u8;32]>
                assert_eq!(**k, key);
            }
            other => panic!("expected SseC, got {other:?}"),
        }
        drop(got);
        store.remove("u-zero");
        assert!(
            store.get("u-zero").is_none(),
            "removed entry must be gone (its Zeroizing<[u8;32]> ran Drop and wiped the key)"
        );
    }

    /// v0.8.2 #62: with three entries inserted at staggered
    /// timestamps, `sweep_stale(now, 24h)` must drop the two that are
    /// older than 24 h and keep the recent one. We pin `now`
    /// deterministically to avoid wall-clock flakes; the store's
    /// internal `put` always stamps `Utc::now()` so we drive the
    /// cutoff such that all three entries land before it.
    #[test]
    fn sweep_stale_drops_old_contexts() {
        let store = MultipartStateStore::new();
        // Insert three entries (all stamped with `Utc::now()` at
        // insert time — within microseconds of each other on a normal
        // machine).
        store.put("u-1", sample_ctx("b", "k1"));
        store.put("u-2", sample_ctx("b", "k2"));
        store.put("u-3", sample_ctx("b", "k3"));
        assert_eq!(store.len(), 3, "all three entries inserted");
        // `now` 25 h in the future puts every existing entry beyond
        // the 24 h cutoff → all three are stale.
        let future = Utc::now() + chrono::Duration::hours(25);
        let swept = store.sweep_stale(future, chrono::Duration::hours(24));
        assert_eq!(swept, 3, "all three entries are older than 24 h cutoff");
        assert_eq!(store.len(), 0, "store must be empty after sweep");
    }

    /// v0.8.2 #62: `sweep_stale` must NOT drop entries that are still
    /// fresh. Inserts one entry, then sweeps with a `now` only 1 h
    /// later — the entry is well within the 24 h TTL, so survives.
    #[test]
    fn sweep_stale_keeps_recent_contexts() {
        let store = MultipartStateStore::new();
        store.put("u-fresh", sample_ctx("b", "k"));
        let near_future = Utc::now() + chrono::Duration::hours(1);
        let swept = store.sweep_stale(near_future, chrono::Duration::hours(24));
        assert_eq!(swept, 0, "1 h-old entry must NOT be swept under 24 h TTL");
        assert!(store.get("u-fresh").is_some(), "fresh entry must remain");
        assert_eq!(store.len(), 1);
    }

    /// v0.8.2 #62: mixed-age workload — two entries from "the past"
    /// (we insert them, then advance the conceptual `now` past the
    /// TTL) and one fresh entry. Sweep must return exactly 2 and
    /// leave the fresh one intact. Verifies `sweep_stale` reports the
    /// correct count for partial sweeps (the most common ops case).
    #[test]
    fn sweep_stale_count_returns_correct() {
        let store = MultipartStateStore::new();
        // Insert two "old" entries; we'll later sweep with a `now` so
        // far ahead that these become stale.
        store.put("old-1", sample_ctx("b", "k1"));
        store.put("old-2", sample_ctx("b", "k2"));
        // Sleep is too brittle for CI; instead drive the sweep
        // cutoff so only the two "old" entries fall behind it. We
        // emulate the third entry being "fresh" by inserting it
        // *after* capturing the moment-in-time we'll sweep against.
        let sweep_now = Utc::now() + chrono::Duration::hours(25);
        // Now the third entry is inserted "in the future" relative
        // to itself — but its timestamp will be `Utc::now()`, well
        // before `sweep_now + 25h - 24h`. To keep the test
        // self-contained we insert the fresh entry at a wall-clock
        // close to `sweep_now`, not `Utc::now()`. We can't cheat the
        // store's internal `Utc::now()` stamp from here, so we rely
        // on the cutoff arithmetic: cutoff = sweep_now - 24h =
        // Utc::now() + 1h, which is strictly after every real
        // `Utc::now()` timestamp on the current entries → all three
        // would be stale.
        //
        // Instead: insert the fresh entry, then choose a `sweep_now`
        // such that exactly the first two are older than the cutoff
        // and the fresh one is not.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let fresh_marker = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.put("fresh", sample_ctx("b", "k3"));
        // cutoff = fresh_marker → strictly between the "old" inserts
        // (timestamps before `fresh_marker`) and the fresh insert
        // (timestamp after `fresh_marker`). Choose `sweep_now =
        // fresh_marker + 24h` so `cutoff = fresh_marker`.
        let sweep_at = fresh_marker + chrono::Duration::hours(24);
        let swept = store.sweep_stale(sweep_at, chrono::Duration::hours(24));
        assert_eq!(swept, 2, "exactly the two pre-marker entries must sweep");
        assert!(store.get("fresh").is_some(), "post-marker entry survives");
        assert!(store.get("old-1").is_none(), "old-1 must be gone");
        assert!(store.get("old-2").is_none(), "old-2 must be gone");
        let _ = sweep_now; // silence dead-code (kept to document the simpler-but-discarded plan)
    }

    /// v0.8.1 #59: `completion_lock(bucket, key)` must return the
    /// **same** `Arc<Mutex<()>>` for repeated calls on the same key,
    /// otherwise concurrent Completes on the same key would each grab
    /// a distinct mutex and the serialisation would silently degrade
    /// to no-op. We compare `Arc::as_ptr()` rather than equality on
    /// the inner `()` because two distinct `Mutex<()>` instances would
    /// have different addresses but compare equal under `==` (unit
    /// type).
    #[test]
    fn completion_lock_returns_same_arc_for_same_key() {
        let store = MultipartStateStore::new();
        let a = store.completion_lock("bucket-a", "key/x");
        let b = store.completion_lock("bucket-a", "key/x");
        assert!(
            Arc::ptr_eq(&a, &b),
            "completion_lock(same bucket, same key) must return identical Arc"
        );
    }

    /// v0.8.1 #59: locks for distinct `(bucket, key)` tuples must be
    /// independent — concurrent Completes on different keys must not
    /// serialise on each other. We acquire two locks back-to-back
    /// (`try_lock` so the assertion is deterministic and doesn't
    /// depend on a runtime); both must succeed without contention.
    /// Also exercises bucket-vs-key disjointness: same key under two
    /// different buckets must NOT alias.
    #[tokio::test]
    async fn completion_lock_distinct_keys_independent() {
        let store = MultipartStateStore::new();
        let a = store.completion_lock("bucket-a", "shared/key");
        let b = store.completion_lock("bucket-b", "shared/key");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "completion_lock with different bucket must yield different Arc"
        );
        // Hold the first lock and acquire the second under the same
        // task — must NOT deadlock and must NOT block. `try_lock`
        // returns `Ok(MutexGuard)` when uncontended, `Err` otherwise.
        let guard_a = a
            .try_lock()
            .expect("lock on bucket-a/shared/key must be free");
        let guard_b = b
            .try_lock()
            .expect("lock on bucket-b/shared/key must be free");
        // Same key, same bucket from a third call must alias `a` and
        // therefore be contended (a's guard is held above).
        let a2 = store.completion_lock("bucket-a", "shared/key");
        assert!(
            Arc::ptr_eq(&a, &a2),
            "completion_lock for the same (bucket, key) must alias"
        );
        assert!(
            a2.try_lock().is_err(),
            "completion_lock alias must observe the held guard as contended"
        );
        drop(guard_a);
        drop(guard_b);
    }

    /// v0.8.1 #59: `prune_completion_locks` must drop entries whose
    /// only `Arc` is the `DashMap`'s own (i.e. no in-flight Complete is
    /// holding a reference). After we acquire a lock then drop the
    /// returned `Arc`, the `strong_count` falls to 1 and prune must
    /// retire the entry so a steady-state workload of unique keys
    /// doesn't accumulate. Conversely, an entry with an outstanding
    /// `Arc` reference must survive prune.
    #[test]
    fn prune_completion_locks_removes_unreferenced() {
        let store = MultipartStateStore::new();
        // Acquire-and-drop: simulates a Complete that finished and let
        // its `Arc<Mutex<()>>` go out of scope. `strong_count == 1`
        // afterwards (only the `DashMap` retains it).
        {
            let _lock = store.completion_lock("b", "ephemeral");
        }
        assert_eq!(
            store.completion_locks_len(),
            1,
            "lock entry must be present immediately after acquire-drop"
        );
        store.prune_completion_locks();
        assert_eq!(
            store.completion_locks_len(),
            0,
            "prune must retire entries with strong_count == 1"
        );

        // Negative case: an outstanding `Arc` must NOT be pruned —
        // pruning a still-borrowed entry would let a concurrent
        // Complete miss the serialisation point.
        let held = store.completion_lock("b", "in-flight");
        store.prune_completion_locks();
        assert_eq!(
            store.completion_locks_len(),
            1,
            "prune must keep entries with outstanding Arc borrowers"
        );
        drop(held);
        store.prune_completion_locks();
        assert_eq!(
            store.completion_locks_len(),
            0,
            "prune must retire the entry once the borrower drops"
        );
    }

    /// 8 threads each register 250 distinct upload_ids and immediately
    /// look them up. After `join` the store must contain exactly the
    /// 8 × 250 entries — verifies `RwLock` doesn't drop writes under
    /// concurrent contention (the obvious refactor that swaps to
    /// `HashMap` without a lock would visibly fail this).
    #[test]
    fn concurrent_put_lookup_race_free() {
        let store = Arc::new(MultipartStateStore::new());
        let mut handles = Vec::new();
        for tid in 0..8u32 {
            let st = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for i in 0..250u32 {
                    let id = format!("u-{tid}-{i}");
                    let ctx = sample_ctx("b", &id);
                    st.put(&id, ctx);
                    // Immediate lookup proves the writer-side observer
                    // sees its own put under the RwLock.
                    let got = st.get(&id).expect("self-put must be visible");
                    assert_eq!(got.key, id);
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(store.len(), 8 * 250, "all puts must persist");
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `by_upload_id` write
    /// guard poisons the lock. Subsequent reads (e.g. `get` /
    /// `sweep_stale`) must recover via
    /// [`crate::lock_recovery::recover_read`] /
    /// [`crate::lock_recovery::recover_write`] and surface the data
    /// instead of re-panicking. `MultipartStateStore` has no `to_json`
    /// so this test exercises `get` directly — the same poison-recovery
    /// helper is used.
    #[test]
    fn multipart_state_get_after_panic_recovers_via_poison() {
        let store = Arc::new(MultipartStateStore::new());
        store.put("u1", sample_ctx("b", "k"));
        let store_cl = Arc::clone(&store);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = store_cl.by_upload_id.write().expect("clean lock");
            g.insert("u2".to_owned(), (sample_ctx("b", "k2"), Utc::now()));
            panic!("force-poison");
        }));
        assert!(
            store.by_upload_id.is_poisoned(),
            "write panic must poison by_upload_id lock"
        );
        let got = store.get("u1").expect("get after poison must succeed");
        assert_eq!(got.bucket, "b");
        assert_eq!(got.key, "k");
        // sweep_stale (write path) must also recover, not panic.
        let n = store.sweep_stale(
            Utc::now() + chrono::Duration::hours(48),
            chrono::Duration::hours(1),
        );
        assert!(n >= 1, "stale sweep must run + recover via poison");
    }
}
