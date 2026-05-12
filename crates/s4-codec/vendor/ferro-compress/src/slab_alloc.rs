//! Wave Z-1 #6 — size-class pooled GPU memory allocator for workbench /
//! staging buffers used by [`BitcompDeviceCodec::decompress_batch`].
//!
//! ## Why
//!
//! [`BitcompDeviceCodec::decompress_batch`] takes caller-supplied output
//! buffers, one per chunk. The workbench memory cost therefore grew from
//! `max(per_term)` (per-term loop, reusing one slot) to `sum(per_term)`
//! (whole cohort decompressed in one launch, all slots live at once).
//!
//! On a 12-term cohort of 8 KiB BitmapContainers that's 8 KiB → 96 KiB —
//! tractable in steady state, but the **path that gets there** (per-call
//! `cudaMalloc` × 12 + `cudaFree` × 12 around every `decompress_batch`
//! call) is **N alloc/free per call**. The Phase 2 D level-A bench
//! showed `decompress_batch` reducing wall time from 498 µs → 41 µs
//! (12×); per-call `cudaMalloc` overhead (typically 10-30 µs each on
//! recent CUDA drivers) would erase that win in production loops.
//!
//! ## What
//!
//! [`SlabAllocator`] is a size-class free-list pool backed by
//! `cudaMalloc` / `cudaFree`. `alloc(size)` rounds `size` up to the next
//! power-of-two bucket (clamped to a 4 KiB floor and a configurable
//! ceiling), pops a pointer from the bucket's free list or `cudaMalloc`s
//! a fresh one. `release(ptr, size)` rounds the same way and pushes the
//! pointer back to the bucket's free list. The buckets grow on first
//! use and **never shrink** — `Drop` releases everything.
//!
//! For sizes above the per-bucket ceiling, the allocator falls back to a
//! transient `cudaMalloc` / `cudaFree` pair: oversize buffers are not
//! pooled (bounds the steady-state VRAM footprint). The fallback path is
//! reported via [`SlabAllocator::oversize_fallback_count`].
//!
//! ## Safety boundary
//!
//! - `release(ptr, size)` MUST be called with the **same `size`** that
//!   was passed to `alloc`. The bucket is computed from `size`, not from
//!   the pointer; passing a different size leaks the pool or corrupts
//!   the bucket on Drop.
//! - Pointers returned by `alloc` are valid **only for the lifetime of
//!   the [`SlabAllocator`] instance**. Dropping the allocator
//!   `cudaFree`s everything still on the free lists.
//! - The allocator is not internally synchronised; callers wrap in a
//!   `Mutex` if shared between threads.

#![cfg(feature = "nvcomp")]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use crate::error::{Error, Result};
use crate::nvcomp_sys::cuda::{cudaFree, cudaMalloc, CUDA_SUCCESS};

/// Power-of-two floor for size-class buckets. Smaller requests round up
/// to this bucket so the very smallest cohort metadata slots (1-2 KiB)
/// don't fragment the pool.
pub const SLAB_MIN_BUCKET_BYTES: usize = 4 * 1024;

/// Power-of-two ceiling for size-class buckets. Requests above this
/// bypass the pool (transient `cudaMalloc` / `cudaFree`) to bound
/// steady-state VRAM growth. 2 MiB covers Wave 11 v3 CHT cohorts (12 ×
/// 8 KiB) with headroom for ~16 MiB nvcomp single-chunk ceiling.
pub const SLAB_MAX_BUCKET_BYTES: usize = 2 * 1024 * 1024;

/// Size-class slab allocator backed by `cudaMalloc` / `cudaFree` free
/// lists. See module docs for usage contract.
///
/// One instance is typically owned per [`BitcompDeviceCodec`] (steady-
/// state pool across `decompress_batch` calls); higher-level
/// orchestration (e.g. a v3 CHT cohort fold worker) can also own one
/// directly when it needs slab-backed transient staging.
pub struct SlabAllocator {
    /// `bucket_bytes -> Vec<device_ptr>`. The key is always a power of
    /// two ≥ [`SLAB_MIN_BUCKET_BYTES`] and ≤ [`SLAB_MAX_BUCKET_BYTES`].
    free_lists: HashMap<usize, Vec<*mut c_void>>,
    /// Live allocations *out* of the pool, by bucket. Used purely for
    /// observability (`high_water_bytes` / `pool_bytes`); not safety-
    /// critical.
    live_counts: HashMap<usize, usize>,
    /// Total bytes ever `cudaMalloc`ed by this allocator (sum of
    /// bucket-rounded sizes), high-water mark.
    high_water_bytes: usize,
    /// Total bytes currently sitting on free lists (pooled but not
    /// handed out). Updated on every alloc / release.
    pool_bytes: usize,
    /// Count of `alloc` calls that fell back to transient `cudaMalloc`
    /// because the requested size exceeded [`SLAB_MAX_BUCKET_BYTES`].
    oversize_fallback_count: u64,
    /// Count of `alloc` calls served from the pool (free-list hit).
    pool_hits: u64,
    /// Count of `alloc` calls that triggered a fresh `cudaMalloc` (free-
    /// list miss within bucketed range).
    pool_misses: u64,
}

// SAFETY: device pointers held by `free_lists` are opaque
// process-globals managed by CUDA; cross-thread access is the caller's
// responsibility (the allocator is not internally synchronised).
unsafe impl Send for SlabAllocator {}
unsafe impl Sync for SlabAllocator {}

impl std::fmt::Debug for SlabAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlabAllocator")
            .field("buckets", &self.free_lists.len())
            .field("high_water_bytes", &self.high_water_bytes)
            .field("pool_bytes", &self.pool_bytes)
            .field("pool_hits", &self.pool_hits)
            .field("pool_misses", &self.pool_misses)
            .field("oversize_fallback_count", &self.oversize_fallback_count)
            .finish()
    }
}

impl Default for SlabAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl SlabAllocator {
    /// Empty pool, no allocations until first `alloc` call.
    pub fn new() -> Self {
        Self {
            free_lists: HashMap::new(),
            live_counts: HashMap::new(),
            high_water_bytes: 0,
            pool_bytes: 0,
            oversize_fallback_count: 0,
            pool_hits: 0,
            pool_misses: 0,
        }
    }

    /// Round `size` up to its bucket. Returns `None` if `size` exceeds
    /// [`SLAB_MAX_BUCKET_BYTES`] (caller falls back to transient alloc).
    fn bucket_of(size: usize) -> Option<usize> {
        if size == 0 {
            return Some(SLAB_MIN_BUCKET_BYTES);
        }
        let bucket = size.next_power_of_two().max(SLAB_MIN_BUCKET_BYTES);
        if bucket > SLAB_MAX_BUCKET_BYTES {
            None
        } else {
            Some(bucket)
        }
    }

    /// Return a device buffer of at least `size` bytes (rounded up to
    /// the next power-of-two bucket ≥ [`SLAB_MIN_BUCKET_BYTES`]).
    ///
    /// The caller MUST call [`Self::release`] with **the same `size`**
    /// before the [`SlabAllocator`] is dropped, or the buffer leaks
    /// (it's released on Drop, but the per-call accounting drifts).
    ///
    /// For `size > SLAB_MAX_BUCKET_BYTES`, the call falls through to a
    /// transient `cudaMalloc` (no pooling); the matching
    /// [`Self::release`] will `cudaFree` it directly. This bounds
    /// steady-state VRAM at `Σ pool_bytes(bucket ≤ ceiling)` and is
    /// reported via [`Self::oversize_fallback_count`].
    pub fn alloc(&mut self, size: usize) -> Result<*mut c_void> {
        match Self::bucket_of(size) {
            Some(bucket) => {
                // Pool path.
                if let Some(list) = self.free_lists.get_mut(&bucket) {
                    if let Some(p) = list.pop() {
                        self.pool_bytes -= bucket;
                        *self.live_counts.entry(bucket).or_insert(0) += 1;
                        self.pool_hits += 1;
                        return Ok(p);
                    }
                }
                // Miss: fresh cudaMalloc, account against high-water.
                let mut p: *mut c_void = null_mut();
                // SAFETY: cudaMalloc writes a valid device pointer on
                // success; untouched on failure. We check rc.
                let rc = unsafe { cudaMalloc(&mut p, bucket) };
                if rc != CUDA_SUCCESS {
                    return Err(Error::Compress(format!(
                        "SlabAllocator::alloc: cudaMalloc({bucket} bytes) failed: code={rc}"
                    )));
                }
                self.high_water_bytes += bucket;
                *self.live_counts.entry(bucket).or_insert(0) += 1;
                self.pool_misses += 1;
                Ok(p)
            }
            None => {
                // Oversize fallback: transient cudaMalloc, no pooling.
                // round up to 256-byte alignment for cudaMalloc/Bitcomp
                // consistency with `max_compressed_size` rounding.
                let alloc_size = size.div_ceil(256) * 256;
                let mut p: *mut c_void = null_mut();
                // SAFETY: see above.
                let rc = unsafe { cudaMalloc(&mut p, alloc_size) };
                if rc != CUDA_SUCCESS {
                    return Err(Error::Compress(format!(
                        "SlabAllocator::alloc: oversize cudaMalloc({alloc_size} bytes) \
                         failed: code={rc}"
                    )));
                }
                self.high_water_bytes += alloc_size;
                self.oversize_fallback_count += 1;
                Ok(p)
            }
        }
    }

    /// Return a previously [`Self::alloc`]ed pointer to its bucket's
    /// free list (or `cudaFree` it directly if it was an oversize
    /// fallback).
    ///
    /// `size` MUST be the same value that was passed to the matching
    /// `alloc` call. Passing a different size puts the pointer into the
    /// wrong bucket (subsequent `alloc(size)` would hand it out for a
    /// request it can't satisfy).
    ///
    /// # Safety
    /// - `ptr` MUST have been returned by [`Self::alloc`] on the same
    ///   [`SlabAllocator`] instance.
    /// - `size` MUST equal the `size` argument passed to the matching
    ///   `alloc` call (the bucket is computed from `size`, not the
    ///   pointer).
    /// - The buffer MUST NOT be aliased by any pending kernel launches
    ///   or device-side accesses after this call returns; the slab may
    ///   hand the buffer back out on the next `alloc(size)` call.
    pub unsafe fn release(&mut self, ptr: *mut c_void, size: usize) {
        if ptr.is_null() {
            return;
        }
        match Self::bucket_of(size) {
            Some(bucket) => {
                self.free_lists.entry(bucket).or_default().push(ptr);
                self.pool_bytes += bucket;
                if let Some(c) = self.live_counts.get_mut(&bucket) {
                    *c = c.saturating_sub(1);
                }
            }
            None => {
                // Oversize: free directly.
                // SAFETY: ptr was returned by cudaMalloc in alloc()'s
                // oversize fallback path; freeing once is well-defined.
                unsafe {
                    let _ = cudaFree(ptr);
                }
            }
        }
    }

    /// High-water mark of total bytes ever allocated by this pool.
    pub fn high_water_bytes(&self) -> usize {
        self.high_water_bytes
    }

    /// Total bytes currently sitting on free lists (pooled, idle).
    pub fn pool_bytes(&self) -> usize {
        self.pool_bytes
    }

    /// Count of allocations served from the free list (no cudaMalloc).
    pub fn pool_hits(&self) -> u64 {
        self.pool_hits
    }

    /// Count of allocations that triggered a fresh cudaMalloc within
    /// the bucketed range (free-list was empty for that bucket).
    pub fn pool_misses(&self) -> u64 {
        self.pool_misses
    }

    /// Count of allocations that bypassed the pool because the request
    /// exceeded [`SLAB_MAX_BUCKET_BYTES`].
    pub fn oversize_fallback_count(&self) -> u64 {
        self.oversize_fallback_count
    }

    /// Number of distinct power-of-two buckets that have at least one
    /// entry on their free list (or have ever been touched).
    pub fn bucket_count(&self) -> usize {
        self.free_lists.len()
    }
}

impl Drop for SlabAllocator {
    fn drop(&mut self) {
        // SAFETY: every pointer on a free list was returned by
        // `cudaMalloc` in `alloc()`'s pool-miss path and has not been
        // freed elsewhere (release puts it back here, oversize never
        // gets here). cudaFree on each is well-defined.
        for (_bucket, list) in self.free_lists.drain() {
            for p in list {
                if !p.is_null() {
                    unsafe {
                        let _ = cudaFree(p);
                    }
                }
            }
        }
        self.pool_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: probe whether a CUDA device is visible. Tests that
    /// touch GPU memory skip cleanly when this returns `false`.
    fn cuda_available() -> bool {
        let mut p: *mut c_void = null_mut();
        // SAFETY: cudaMalloc writes through &mut p on success; on
        // failure we just check rc.
        let rc = unsafe { cudaMalloc(&mut p, 16) };
        if rc == CUDA_SUCCESS {
            // SAFETY: just allocated above.
            unsafe {
                let _ = cudaFree(p);
            }
            true
        } else {
            false
        }
    }

    #[test]
    fn bucket_of_rounds_up_to_power_of_two() {
        // Sub-floor → floor.
        assert_eq!(SlabAllocator::bucket_of(1), Some(SLAB_MIN_BUCKET_BYTES));
        assert_eq!(SlabAllocator::bucket_of(0), Some(SLAB_MIN_BUCKET_BYTES));
        assert_eq!(SlabAllocator::bucket_of(4096), Some(4096));
        // Between buckets: round up.
        assert_eq!(SlabAllocator::bucket_of(5000), Some(8 * 1024));
        assert_eq!(SlabAllocator::bucket_of(8192), Some(8192));
        assert_eq!(SlabAllocator::bucket_of(9000), Some(16 * 1024));
        assert_eq!(SlabAllocator::bucket_of(8 * 1024), Some(8 * 1024));
        // Exactly at ceiling.
        assert_eq!(
            SlabAllocator::bucket_of(SLAB_MAX_BUCKET_BYTES),
            Some(SLAB_MAX_BUCKET_BYTES)
        );
        // Above ceiling: None (oversize fallback).
        assert_eq!(SlabAllocator::bucket_of(SLAB_MAX_BUCKET_BYTES + 1), None);
        assert_eq!(SlabAllocator::bucket_of(16 * 1024 * 1024), None);
    }

    #[test]
    fn empty_pool_initial_state() {
        let slab = SlabAllocator::new();
        assert_eq!(slab.high_water_bytes(), 0);
        assert_eq!(slab.pool_bytes(), 0);
        assert_eq!(slab.pool_hits(), 0);
        assert_eq!(slab.pool_misses(), 0);
        assert_eq!(slab.oversize_fallback_count(), 0);
        assert_eq!(slab.bucket_count(), 0);
    }

    #[test]
    fn slab_alloc_basic_alloc_release_reuse() {
        if !cuda_available() {
            return;
        }
        let mut slab = SlabAllocator::new();
        // First alloc: miss (no free list).
        let p1 = slab.alloc(8 * 1024).expect("alloc 8 KiB");
        assert!(!p1.is_null());
        assert_eq!(slab.pool_misses(), 1);
        assert_eq!(slab.pool_hits(), 0);
        assert_eq!(slab.high_water_bytes(), 8 * 1024);
        assert_eq!(slab.pool_bytes(), 0);

        // Release: pointer goes to bucket free list.
        // SAFETY: p1 was just returned by alloc(8 KiB) above.
        unsafe { slab.release(p1, 8 * 1024) };
        assert_eq!(slab.pool_bytes(), 8 * 1024);
        assert_eq!(slab.high_water_bytes(), 8 * 1024);

        // Second alloc, same size: hit, same pointer.
        let p2 = slab.alloc(8 * 1024).expect("alloc 8 KiB reuse");
        assert_eq!(p1, p2, "reuse must return the same pointer (LIFO)");
        assert_eq!(slab.pool_hits(), 1);
        assert_eq!(slab.pool_misses(), 1);
        assert_eq!(slab.pool_bytes(), 0);
        // high_water unchanged (we reused).
        assert_eq!(slab.high_water_bytes(), 8 * 1024);

        // Release for Drop cleanup.
        // SAFETY: p2 was returned by alloc(8 KiB) above.
        unsafe { slab.release(p2, 8 * 1024) };
    }

    #[test]
    fn slab_alloc_size_class_bucketing() {
        if !cuda_available() {
            return;
        }
        let mut slab = SlabAllocator::new();
        // 5000 bytes → 8 KiB bucket.
        let p_5k = slab.alloc(5000).expect("alloc 5000");
        // 9000 bytes → 16 KiB bucket.
        let p_9k = slab.alloc(9000).expect("alloc 9000");

        // high_water = 8 KiB + 16 KiB.
        assert_eq!(slab.high_water_bytes(), 8 * 1024 + 16 * 1024);

        // SAFETY: each ptr was returned by the matching alloc call above.
        unsafe {
            slab.release(p_5k, 5000);
            slab.release(p_9k, 9000);
        }

        // Pool bytes = 8 KiB + 16 KiB.
        assert_eq!(slab.pool_bytes(), 8 * 1024 + 16 * 1024);

        // Re-allocate from the 8 KiB bucket only.
        let p_5k2 = slab.alloc(4500).expect("alloc 4500 reuse 8 KiB");
        assert_eq!(p_5k, p_5k2, "4500 and 5000 both bucket-of 8 KiB");
        // 16 KiB bucket still has its pointer.
        assert_eq!(slab.pool_bytes(), 16 * 1024);
        assert_eq!(slab.pool_hits(), 1);

        // SAFETY: p_5k2 from alloc(4500) above.
        unsafe { slab.release(p_5k2, 4500) };
    }

    #[test]
    fn slab_alloc_size_class_independence() {
        if !cuda_available() {
            return;
        }
        let mut slab = SlabAllocator::new();
        let p8 = slab.alloc(8 * 1024).expect("alloc 8 KiB");
        let p16 = slab.alloc(16 * 1024).expect("alloc 16 KiB");
        assert_ne!(p8, p16, "different buckets must yield different ptrs");

        // SAFETY: each ptr from its matching alloc above.
        unsafe {
            slab.release(p8, 8 * 1024);
            slab.release(p16, 16 * 1024);
        }

        // Pull only from the 16 KiB bucket.
        let p16_again = slab.alloc(16 * 1024).expect("realloc 16 KiB");
        assert_eq!(p16_again, p16, "16 KiB bucket reuse");

        // 8 KiB bucket still pristine.
        let p8_again = slab.alloc(8 * 1024).expect("realloc 8 KiB");
        assert_eq!(p8_again, p8, "8 KiB bucket reuse independent of 16 KiB");

        // SAFETY: ptrs from alloc above.
        unsafe {
            slab.release(p8_again, 8 * 1024);
            slab.release(p16_again, 16 * 1024);
        }
    }

    #[test]
    fn slab_alloc_above_max_falls_back() {
        if !cuda_available() {
            return;
        }
        let mut slab = SlabAllocator::new();
        // Above ceiling → oversize fallback.
        let oversize = SLAB_MAX_BUCKET_BYTES + 1;
        let p = slab.alloc(oversize).expect("alloc oversize");
        assert!(!p.is_null());
        assert_eq!(slab.oversize_fallback_count(), 1);
        // Pool itself untouched.
        assert_eq!(slab.pool_hits(), 0);
        assert_eq!(slab.pool_misses(), 0);
        assert_eq!(slab.pool_bytes(), 0);
        assert_eq!(slab.bucket_count(), 0);
        // high_water reflects the oversize alloc, rounded up to 256B.
        let alloc_size = oversize.div_ceil(256) * 256;
        assert_eq!(slab.high_water_bytes(), alloc_size);

        // Release: cudaFree direct, no pooling.
        // SAFETY: p was returned by alloc(oversize) above; the
        // oversize path cudaFrees directly with no pooling.
        unsafe { slab.release(p, oversize) };
        // Pool still empty.
        assert_eq!(slab.pool_bytes(), 0);
        assert_eq!(slab.bucket_count(), 0);
    }

    #[test]
    fn slab_alloc_drop_frees_all() {
        if !cuda_available() {
            return;
        }
        // Allocate several distinct buckets, release them, then drop.
        // The cleanup is implicit; the test passes if Drop doesn't
        // double-free / segfault. We additionally verify high-water
        // accounting captured every distinct bucket.
        let mut slab = SlabAllocator::new();
        let p4 = slab.alloc(4 * 1024).expect("4 KiB");
        let p8 = slab.alloc(8 * 1024).expect("8 KiB");
        let p16 = slab.alloc(16 * 1024).expect("16 KiB");
        // SAFETY: ptrs from alloc above.
        unsafe {
            slab.release(p4, 4 * 1024);
            slab.release(p8, 8 * 1024);
            slab.release(p16, 16 * 1024);
        }
        assert_eq!(slab.pool_bytes(), 4 * 1024 + 8 * 1024 + 16 * 1024);
        assert_eq!(slab.high_water_bytes(), 4 * 1024 + 8 * 1024 + 16 * 1024);
        drop(slab);
        // If we got here without segfault, Drop walked the buckets.
    }

    #[test]
    fn slab_alloc_lifo_within_bucket() {
        if !cuda_available() {
            return;
        }
        let mut slab = SlabAllocator::new();
        // Two ptrs same bucket.
        let p1 = slab.alloc(8 * 1024).expect("a");
        let p2 = slab.alloc(8 * 1024).expect("b");
        assert_ne!(p1, p2);

        // SAFETY: p1/p2 from alloc above; q1/q2 below symmetric.
        unsafe {
            slab.release(p1, 8 * 1024);
            slab.release(p2, 8 * 1024);
        }

        // LIFO: most recently released comes back first.
        let q1 = slab.alloc(8 * 1024).expect("c");
        let q2 = slab.alloc(8 * 1024).expect("d");
        assert_eq!(q1, p2, "LIFO top");
        assert_eq!(q2, p1, "LIFO under-top");

        // SAFETY: q1/q2 from alloc above.
        unsafe {
            slab.release(q1, 8 * 1024);
            slab.release(q2, 8 * 1024);
        }
    }
}
