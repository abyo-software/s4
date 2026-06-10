//! GPU small-PUT batch aggregator (v1.2 `--gpu-batch-small-puts`).
//!
//! ## What this does
//!
//! Per-object GPU compression loses to CPU zstd below `--gpu-min-bytes`
//! (default 1 MiB) because each call pays a fixed kernel-launch + PCIe
//! round-trip cost. This module amortises that cost across **concurrent**
//! small PUTs: bodies are queued into an mpsc channel, a single tokio task
//! drains them into batches of up to `--gpu-batch-max-items` (or whatever
//! arrived within the `--gpu-batch-window-ms` window), and one
//! `s4_codec::nvcomp_batched::NvcompZstdBatchEncoder::compress_batch` call
//! compresses the whole batch in a single kernel launch.
//!
//! ## Wire-format invariant
//!
//! The batch path changes **how** the encoder runs, never **what** it
//! writes: each item comes back as a standard `CodecKind::NvcompZstd` body
//! and manifest, byte-layout-identical to the per-object GPU path.
//! GET-side code is completely unaware of batching.
//!
//! ## Failure / backpressure semantics
//!
//! Everything degrades to the caller's per-object CPU path:
//!
//! - queue full ([`GpuBatchError::QueueFull`]) — backpressure; caller falls
//!   back immediately instead of waiting,
//! - worker gone ([`GpuBatchError::Closed`]),
//! - per-item or batch-level codec failure ([`GpuBatchError::Codec`]).
//!
//! The PUT handler in `service.rs` maps **any** error to its existing
//! cpu-zstd framed path, so a misbehaving GPU can never fail a PUT that
//! would have succeeded without the flag.
//!
//! ## Ordering
//!
//! Each queued request carries its own oneshot sender; the worker zips the
//! batch's request vector with the encoder's result vector (both in queue
//! order), so responses can never be cross-delivered between concurrent
//! PUTs. `aggregator_preserves_request_response_pairing` below locks this
//! in with a mock compressor that tags outputs with their input bytes.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use s4_codec::{ChunkManifest, CodecError};
use tokio::sync::{mpsc, oneshot};

/// One compressed-item result: the body to store + its manifest
/// (`CodecKind::NvcompZstd`, crc32c over the original bytes).
pub type BatchItemResult = Result<(Bytes, ChunkManifest), CodecError>;

/// Batch compressor signature: all items in, one result per item out (in
/// order). The outer `Err` is a batch-level failure (every caller falls
/// back). Boxed so the aggregator core can be unit-tested without a GPU —
/// production wires `NvcompZstdBatchEncoder::compress_batch` in via
/// [`spawn`]. Runs inside `spawn_blocking` (one batched CUDA call blocks
/// for the kernel + PCIe round-trip).
pub type BatchCompressFn =
    Arc<dyn Fn(&[Bytes]) -> Result<Vec<BatchItemResult>, CodecError> + Send + Sync>;

/// Why a queued PUT could not be batch-compressed. The PUT handler treats
/// every variant the same way — fall back to the per-object CPU path —
/// but the labels keep the log line / debugging story precise.
#[derive(Debug)]
pub enum GpuBatchError {
    /// Channel at capacity: more concurrent small PUTs than the GPU batch
    /// queue absorbs. Backpressure by design — fall back, don't wait.
    QueueFull,
    /// Worker task / channel gone (shutdown or panic).
    Closed,
    /// The batched compress ran but this item failed (or the whole batch
    /// failed at the CUDA level).
    Codec(CodecError),
}

impl std::fmt::Display for GpuBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "gpu batch queue full"),
            Self::Closed => write!(f, "gpu batch worker unavailable"),
            Self::Codec(e) => write!(f, "gpu batch compress failed: {e}"),
        }
    }
}

/// Aggregator tuning. Built in `main.rs` from the `--gpu-batch-*` flags.
#[derive(Debug, Clone)]
pub struct GpuBatchConfig {
    /// Flush when this many requests are pending (`--gpu-batch-max-items`).
    pub max_items: usize,
    /// Flush when the oldest pending request has waited this long
    /// (`--gpu-batch-window-ms`).
    pub window: Duration,
    /// Inclusive lower bound for eligible body sizes
    /// (`--gpu-batch-floor-bytes`).
    pub floor_bytes: u64,
    /// Exclusive upper bound for eligible body sizes (= `--gpu-min-bytes`;
    /// at and above this the dispatcher already routes to the per-object
    /// GPU path).
    pub max_bytes: u64,
    /// mpsc channel capacity (backpressure threshold).
    pub queue_depth: usize,
}

struct BatchRequest {
    body: Bytes,
    resp: oneshot::Sender<BatchItemResult>,
}

/// Cheap-to-clone handle held by `S4Service`. `None` on the service =
/// feature flag off = zero behaviour change.
#[derive(Clone)]
pub struct GpuBatchHandle {
    tx: mpsc::Sender<BatchRequest>,
    floor_bytes: u64,
    max_bytes: u64,
}

impl std::fmt::Debug for GpuBatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuBatchHandle")
            .field("floor_bytes", &self.floor_bytes)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl GpuBatchHandle {
    /// Size gate: `floor_bytes <= n < max_bytes`. The caller additionally
    /// requires that the dispatcher picked `CpuZstd` and no dictionary
    /// matched (see `service.rs` PUT path).
    pub fn eligible_size(&self, n: u64) -> bool {
        n >= self.floor_bytes && n < self.max_bytes
    }

    /// Queue `body` for batched GPU compression and await the result.
    /// Non-blocking enqueue: a full queue returns
    /// [`GpuBatchError::QueueFull`] immediately (the caller falls back to
    /// its CPU path) instead of adding latency under overload.
    pub async fn try_compress(&self, body: Bytes) -> Result<(Bytes, ChunkManifest), GpuBatchError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.tx.try_send(BatchRequest {
            body,
            resp: resp_tx,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(GpuBatchError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(GpuBatchError::Closed),
        }
        match resp_rx.await {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(GpuBatchError::Codec(e)),
            Err(_) => Err(GpuBatchError::Closed),
        }
    }
}

/// Spawn the aggregator worker around an arbitrary batch compressor.
/// Production callers use [`spawn`]; tests inject a mock compressor so the
/// flush / ordering / fallback logic runs without a GPU.
pub fn spawn_with_compressor(compress: BatchCompressFn, cfg: GpuBatchConfig) -> GpuBatchHandle {
    let (tx, mut rx) = mpsc::channel::<BatchRequest>(cfg.queue_depth.max(1));
    let max_items = cfg.max_items.max(1);
    let window = cfg.window;
    tokio::spawn(async move {
        // One worker task: recv the batch head, then drain until max_items
        // or the window deadline (measured from the head — bounded added
        // latency for the first PUT in every batch).
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + window;
            while batch.len() < max_items {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(req)) => batch.push(req),
                    // Channel closed: flush what we have, outer loop exits
                    // on the next recv.
                    Ok(None) => break,
                    // Window elapsed.
                    Err(_) => break,
                }
            }
            let bodies: Vec<Bytes> = batch.iter().map(|r| r.body.clone()).collect();
            let compress = Arc::clone(&compress);
            let joined = tokio::task::spawn_blocking(move || compress(&bodies)).await;
            match joined {
                Ok(Ok(results)) if results.len() == batch.len() => {
                    // In-order zip: result i belongs to request i. A
                    // receiver that gave up (client disconnect) just drops
                    // the oneshot — ignore the send error.
                    for (req, res) in batch.into_iter().zip(results) {
                        let _ = req.resp.send(res);
                    }
                }
                Ok(Ok(results)) => {
                    // Defensive: encoder broke its length contract — fail
                    // the whole batch rather than risk cross-pairing.
                    tracing::error!(
                        got = results.len(),
                        expected = batch.len(),
                        "gpu batch encoder returned wrong result count; failing batch"
                    );
                    for req in batch {
                        let _ = req.resp.send(Err(CodecError::Backend(anyhow::anyhow!(
                            "gpu batch result-count mismatch"
                        ))));
                    }
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    tracing::warn!(error = %msg, "gpu batch compress failed; batch falls back");
                    for req in batch {
                        let _ = req.resp.send(Err(CodecError::Backend(anyhow::anyhow!(
                            "gpu batch compress failed: {msg}"
                        ))));
                    }
                }
                Err(join_err) => {
                    let msg = join_err.to_string();
                    tracing::error!(error = %msg, "gpu batch blocking task panicked / cancelled");
                    for req in batch {
                        let _ = req.resp.send(Err(CodecError::Backend(anyhow::anyhow!(
                            "gpu batch task failed: {msg}"
                        ))));
                    }
                }
            }
        }
        tracing::debug!("gpu batch aggregator worker exiting (handle dropped)");
    });
    GpuBatchHandle {
        tx,
        floor_bytes: cfg.floor_bytes,
        max_bytes: cfg.max_bytes,
    }
}

/// Spawn the aggregator around the real nvCOMP batched-zstd encoder. The
/// encoder owns one CUDA stream + a grow-only buffer pool; the worker is
/// the only caller so its internal mutex never contends.
#[cfg(feature = "nvcomp-gpu")]
pub fn spawn(
    encoder: Arc<s4_codec::nvcomp_batched::NvcompZstdBatchEncoder>,
    cfg: GpuBatchConfig,
) -> GpuBatchHandle {
    spawn_with_compressor(
        Arc::new(move |items: &[Bytes]| encoder.compress_batch(items)),
        cfg,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cfg(max_items: usize, window_ms: u64) -> GpuBatchConfig {
        GpuBatchConfig {
            max_items,
            window: Duration::from_millis(window_ms),
            floor_bytes: 4096,
            max_bytes: 1024 * 1024,
            queue_depth: 64,
        }
    }

    fn manifest_for(body: &Bytes) -> ChunkManifest {
        ChunkManifest {
            codec: s4_codec::CodecKind::NvcompZstd,
            original_size: body.len() as u64,
            compressed_size: body.len() as u64,
            crc32c: crc32c::crc32c(body),
        }
    }

    /// Mock compressor that records observed batch sizes and "compresses"
    /// each item by echoing it (so pairing is verifiable byte-for-byte).
    fn echo_compressor(batch_sizes: Arc<Mutex<Vec<usize>>>) -> BatchCompressFn {
        Arc::new(move |items: &[Bytes]| {
            batch_sizes
                .lock()
                .expect("batch_sizes lock")
                .push(items.len());
            Ok(items
                .iter()
                .map(|b| Ok((b.clone(), manifest_for(b))))
                .collect())
        })
    }

    #[test]
    fn eligible_size_bounds_are_floor_inclusive_max_exclusive() {
        let handle = spawn_handle_for_bounds_test();
        assert!(!handle.eligible_size(4095));
        assert!(handle.eligible_size(4096));
        assert!(handle.eligible_size(1024 * 1024 - 1));
        assert!(!handle.eligible_size(1024 * 1024));
    }

    fn spawn_handle_for_bounds_test() -> GpuBatchHandle {
        // Build a handle without a runtime-backed worker: the bounds check
        // never touches the channel.
        let (tx, _rx) = mpsc::channel(1);
        GpuBatchHandle {
            tx,
            floor_bytes: 4096,
            max_bytes: 1024 * 1024,
        }
    }

    /// max-items flush: max_items concurrent requests with a long window
    /// must come back from a single batch call.
    #[tokio::test]
    async fn aggregator_flushes_on_max_items() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_with_compressor(echo_compressor(Arc::clone(&sizes)), cfg(4, 10_000));
        let bodies: Vec<Bytes> = (0..4u8).map(|i| Bytes::from(vec![i; 8 * 1024])).collect();
        let futs: Vec<_> = bodies
            .iter()
            .map(|b| handle.try_compress(b.clone()))
            .collect();
        let results = futures::future::join_all(futs).await;
        for (body, res) in bodies.iter().zip(results) {
            let (out, m) = res.expect("batched ok");
            assert_eq!(&out, body);
            assert_eq!(m.original_size, body.len() as u64);
        }
        // All 4 must have flushed as one batch well before the 10 s window.
        assert_eq!(*sizes.lock().expect("lock"), vec![4]);
    }

    /// window flush: fewer than max_items requests still complete once the
    /// window elapses.
    #[tokio::test]
    async fn aggregator_flushes_on_window_expiry() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_with_compressor(echo_compressor(Arc::clone(&sizes)), cfg(32, 5));
        let body = Bytes::from(vec![7u8; 8 * 1024]);
        let started = std::time::Instant::now();
        let (out, _) = handle.try_compress(body.clone()).await.expect("ok");
        assert_eq!(out, body);
        // Must have completed via the window timer (not max_items), and
        // not hung anywhere near the test timeout.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(*sizes.lock().expect("lock"), vec![1]);
    }

    /// Concurrent PUT pairing: every response must carry the bytes of the
    /// request that asked for it — no oneshot cross-delivery inside or
    /// across batches.
    #[tokio::test]
    async fn aggregator_preserves_request_response_pairing() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        // Small max_items so the 64 concurrent requests split into many
        // batches and cross-batch pairing is exercised too.
        let handle = spawn_with_compressor(echo_compressor(sizes), cfg(8, 50));
        let futs: Vec<_> = (0..64u64)
            .map(|i| {
                let handle = handle.clone();
                // Unique body per request: the index baked into every byte.
                let body = Bytes::from(
                    (0..4096 + i as usize)
                        .map(|j| (i as usize + j) as u8)
                        .collect::<Vec<u8>>(),
                );
                async move {
                    let expect = body.clone();
                    let (out, m) = handle.try_compress(body).await.expect("ok");
                    assert_eq!(out, expect, "response paired with wrong request");
                    assert_eq!(m.crc32c, crc32c::crc32c(&expect));
                }
            })
            .collect();
        futures::future::join_all(futs).await;
    }

    /// Batch-level failure: every member of the failed batch gets an Err
    /// (and the caller falls back) — nobody hangs.
    #[tokio::test]
    async fn aggregator_fails_whole_batch_on_batch_level_error() {
        let compress: BatchCompressFn = Arc::new(|_items: &[Bytes]| {
            Err(CodecError::Backend(anyhow::anyhow!("simulated CUDA OOM")))
        });
        let handle = spawn_with_compressor(compress, cfg(2, 10_000));
        let f1 = handle.try_compress(Bytes::from(vec![1u8; 4096]));
        let f2 = handle.try_compress(Bytes::from(vec![2u8; 4096]));
        let (r1, r2) = tokio::join!(f1, f2);
        assert!(matches!(r1, Err(GpuBatchError::Codec(_))));
        assert!(matches!(r2, Err(GpuBatchError::Codec(_))));
    }

    /// Per-item failure isolation: item 0 fails, item 1 still succeeds.
    #[tokio::test]
    async fn aggregator_isolates_per_item_failures() {
        let compress: BatchCompressFn = Arc::new(|items: &[Bytes]| {
            Ok(items
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if i == 0 {
                        Err(CodecError::Backend(anyhow::anyhow!("chunk failure")))
                    } else {
                        Ok((b.clone(), manifest_for(b)))
                    }
                })
                .collect())
        });
        let handle = spawn_with_compressor(compress, cfg(2, 10_000));
        let b0 = Bytes::from(vec![0u8; 4096]);
        let b1 = Bytes::from(vec![1u8; 4096]);
        let (r0, r1) = tokio::join!(handle.try_compress(b0), handle.try_compress(b1.clone()));
        assert!(matches!(r0, Err(GpuBatchError::Codec(_))));
        let (out, _) = r1.expect("item 1 ok");
        assert_eq!(out, b1);
    }

    /// Backpressure: once the queue is full, try_compress returns
    /// QueueFull instantly instead of blocking the PUT handler.
    #[tokio::test]
    async fn aggregator_returns_queue_full_under_backpressure() {
        // Compressor that parks forever so the queue can only drain into
        // the in-flight batch.
        let blocked = Arc::new(AtomicUsize::new(0));
        let blocked2 = Arc::clone(&blocked);
        let compress: BatchCompressFn = Arc::new(move |_items: &[Bytes]| {
            blocked2.fetch_add(1, Ordering::SeqCst);
            // Block the (dedicated blocking) thread long enough for the
            // assertions below; the test runtime drops the worker after.
            std::thread::sleep(Duration::from_secs(5));
            Err(CodecError::Backend(anyhow::anyhow!(
                "never reached in assertions"
            )))
        });
        let handle = spawn_with_compressor(
            compress,
            GpuBatchConfig {
                max_items: 1,
                window: Duration::from_millis(1),
                floor_bytes: 4096,
                max_bytes: 1024 * 1024,
                queue_depth: 2,
            },
        );
        // First request: dequeued into the (blocked) batch.
        let _inflight = tokio::spawn({
            let h = handle.clone();
            async move {
                let _ = h.try_compress(Bytes::from(vec![0u8; 4096])).await;
            }
        });
        // Wait for the worker to actually start the blocking compress.
        while blocked.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Fill the queue (depth 2)...
        let _q1 = {
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = h.try_compress(Bytes::from(vec![1u8; 4096])).await;
            })
        };
        let _q2 = {
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = h.try_compress(Bytes::from(vec![2u8; 4096])).await;
            })
        };
        // Give the two fillers a moment to enqueue.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // ...then the next must bounce with QueueFull, immediately.
        let started = std::time::Instant::now();
        let res = handle.try_compress(Bytes::from(vec![3u8; 4096])).await;
        assert!(
            matches!(res, Err(GpuBatchError::QueueFull)),
            "expected QueueFull, got {res:?}"
        );
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
