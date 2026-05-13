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
use std::sync::RwLock;

use chrono::{DateTime, Utc};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultipartSseMode {
    /// Plaintext multipart. Backend stores raw framed bytes.
    None,
    /// Server-managed keyring (active key on PUT, all keys probed on GET).
    /// The keyring itself lives on `S4Service`; only the marker is held
    /// here so `complete_multipart_upload` knows which path to take.
    SseS4,
    /// Per-request customer key. The 32-byte key + its 128-bit MD5 are
    /// kept in memory only for the lifetime of the upload, then dropped
    /// when the entry is `remove(...)`'d on Complete or Abort.
    SseC {
        key: [u8; 32],
        key_md5: [u8; 16],
    },
    /// Named KMS key (resolved against the gateway's KMS backend on
    /// Complete to generate the per-object DEK).
    SseKms {
        key_id: String,
    },
}

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
pub struct MultipartStateStore {
    by_upload_id: RwLock<HashMap<String, MultipartUploadContext>>,
}

impl MultipartStateStore {
    /// Empty store. Use `Arc<MultipartStateStore>` so `S4Service`'s
    /// async handlers can borrow it across `&self` calls without
    /// requiring `Clone`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_upload_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new upload under `upload_id`. If `upload_id` is
    /// already present (extremely unlikely — backend issues fresh ids)
    /// the previous entry is overwritten silently to mirror
    /// `HashMap::insert`'s replace-on-collision semantics.
    pub fn put(&self, upload_id: &str, ctx: MultipartUploadContext) {
        self.by_upload_id
            .write()
            .expect("multipart-state by_upload_id RwLock poisoned")
            .insert(upload_id.to_owned(), ctx);
    }

    /// Snapshot the context for `upload_id`. `None` when no entry was
    /// registered (e.g. Complete arrived for an upload that the gateway
    /// has no record of — passes through to the backend untouched, which
    /// in turn surfaces `NoSuchUpload`).
    #[must_use]
    pub fn get(&self, upload_id: &str) -> Option<MultipartUploadContext> {
        self.by_upload_id
            .read()
            .expect("multipart-state by_upload_id RwLock poisoned")
            .get(upload_id)
            .cloned()
    }

    /// Drop the entry. Called by Complete / Abort to release the SSE-C
    /// key bytes and the tag-set memory promptly.
    pub fn remove(&self, upload_id: &str) {
        self.by_upload_id
            .write()
            .expect("multipart-state by_upload_id RwLock poisoned")
            .remove(upload_id);
    }

    /// Test-only: how many in-flight uploads the store is currently
    /// tracking. Used by the assertion in `concurrent_put_lookup_race_free`.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_upload_id
            .read()
            .expect("multipart-state by_upload_id RwLock poisoned")
            .len()
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
        ctx.sse = MultipartSseMode::SseC { key, key_md5 };
        store.put("u-sse-c", ctx);
        let got = store.get("u-sse-c").expect("entry must be present");
        match got.sse {
            MultipartSseMode::SseC { key: k, key_md5: m } => {
                assert_eq!(k, key, "SSE-C key bytes must round-trip");
                assert_eq!(m, key_md5, "SSE-C MD5 must round-trip");
            }
            other => panic!("expected SseC variant, got {other:?}"),
        }
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
}
