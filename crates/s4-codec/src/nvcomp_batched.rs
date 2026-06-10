//! nvCOMP batched small-object compression (v1.2 GPU small-PUT batching).
//!
//! ## Why this module exists
//!
//! The per-object [`crate::nvcomp::NvcompZstdCodec`] already drives the
//! nvCOMP **batched** API internally — but one `compress()` call equals one
//! kernel launch + one PCIe round-trip, so for small objects (4 KiB – 1 MiB)
//! the fixed overhead dominates and CPU zstd wins (`--gpu-min-bytes` default
//! 1 MiB exists for exactly this reason). This module amortises that fixed
//! cost by compressing **many independent objects in a single
//! `nvcompBatchedZstdCompressAsync` launch**: all chunks of all objects are
//! placed into one chunk table, one kernel launch + one H2D/D2H pair covers
//! the whole batch.
//!
//! ## Wire-format invariant (the non-negotiable part)
//!
//! The output for each item is **byte-layout-identical** to what the
//! existing per-object `NvcompZstdCodec::compress` produces: an FCG1 frame
//! (`crate::ferro_compress::nvcomp::write_header`) with 64 KiB chunking and
//! per-chunk nvCOMP-zstd payloads, stamped with the existing
//! [`CodecKind::NvcompZstd`] manifest. No new codec id, no new metadata.
//! The proof obligation — "an object compressed via the batch path is
//! readable by the unmodified per-object GET/decompress path" — is held by
//! the `#[ignore]`-gated GPU tests at the bottom of this file
//! (`batched_compress_decompresses_via_existing_per_object_path`), which
//! round-trip batch output through `NvcompZstdCodec::decompress`.
//!
//! Because the per-chunk compressed blob is produced by the very same
//! `nvcompBatchedZstdCompressAsync` entry point the per-object path uses
//! (just with a bigger chunk table), there is no cross-API compatibility
//! question — only the framing around the chunks matters, and we emit it
//! with the **same** `write_header` helper.
//!
//! ## Concurrency model
//!
//! One CUDA stream + one buffer pool per encoder, serialised by a `Mutex`
//! (mirrors `ferro_compress::NvcompCodec`). The s4-server batch aggregator
//! owns a single encoder and calls [`NvcompZstdBatchEncoder::compress_batch`]
//! from `spawn_blocking`, so the mutex is effectively uncontended.

// FFI-heavy module: the workspace-wide `unsafe_code = deny` is overridden
// here exactly like `crate::ferro_compress` does for the same nvCOMP / CUDA
// call surface. Every `unsafe` block carries its own SAFETY comment.
#[cfg(feature = "nvcomp-gpu")]
#[allow(unsafe_code)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::Mutex;

    use bytes::Bytes;

    use crate::ferro_compress::nvcomp::{
        DEFAULT_CHUNK_SIZE, check_cuda, compress_get_max_output_chunk_size, compress_get_temp_size,
        dispatch_compress, write_header,
    };
    use crate::ferro_compress::nvcomp_sys::cuda::*;
    use crate::ferro_compress::nvcomp_sys::nvcomp::{nvcompStatus_t, nvcompSuccess, status_str};
    use crate::ferro_compress::{Algo, Error as FerroError};
    use crate::{ChunkManifest, CodecError, CodecKind};

    /// Device/pinned base-pointer alignment used for both the per-item
    /// input offsets and the per-chunk compressed-output stride. 256 is the
    /// `cudaMalloc` base alignment and satisfies every nvCOMP per-algo
    /// alignment requirement (same constant the per-object path rounds its
    /// compressed-chunk stride to).
    const ALIGN: usize = 256;

    fn ferr(e: FerroError) -> CodecError {
        CodecError::Backend(anyhow::anyhow!("nvcomp batched zstd: {e}"))
    }

    /// Batched nvCOMP zstd encoder for small objects.
    ///
    /// Output frames are bit-layout-identical to per-object
    /// [`crate::nvcomp::NvcompZstdCodec`] output (FCG1 framing, 64 KiB
    /// chunks) — see the module docs for the compatibility contract.
    pub struct NvcompZstdBatchEncoder {
        stream: cudaStream_t,
        inner: Mutex<BatchInner>,
    }

    // SAFETY: same contract as `ferro_compress::NvcompCodec` — the CUDA
    // runtime + nvCOMP entry points are thread-safe given per-call streams;
    // all mutable pool state is behind the Mutex, so a single encoder
    // serialises its batches.
    unsafe impl Send for NvcompZstdBatchEncoder {}
    unsafe impl Sync for NvcompZstdBatchEncoder {}

    impl std::fmt::Debug for NvcompZstdBatchEncoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NvcompZstdBatchEncoder").finish()
        }
    }

    /// Grow-only device + pinned-host buffer pool (mirrors
    /// `ferro_compress::nvcomp::NvcompCodecInner`, but laid out for a
    /// many-objects-one-launch chunk table).
    #[derive(Default)]
    struct BatchInner {
        // Bulk device buffers.
        d_uncomp: *mut c_void,
        d_uncomp_cap: usize,
        d_comp: *mut c_void,
        d_comp_cap: usize,
        d_temp: *mut c_void,
        d_temp_cap: usize,

        // Per-chunk metadata (device).
        d_uncomp_ptrs: *mut c_void,
        d_uncomp_sizes: *mut c_void,
        d_comp_ptrs: *mut c_void,
        d_comp_sizes: *mut c_void,
        d_statuses: *mut c_void,
        chunks_cap: usize,

        // Pinned host staging.
        h_pinned_input: *mut c_void,
        h_pinned_input_cap: usize,
        h_pinned_output: *mut c_void,
        h_pinned_output_cap: usize,
        h_pinned_meta: *mut c_void,
        h_pinned_meta_cap: usize,

        // Reusable host-side tables.
        h_uncomp_ptrs: Vec<*const c_void>,
        h_uncomp_sizes: Vec<usize>,
        h_comp_ptrs: Vec<*mut c_void>,
        h_comp_sizes: Vec<usize>,
        h_statuses: Vec<nvcompStatus_t>,
    }

    impl Drop for BatchInner {
        fn drop(&mut self) {
            unsafe {
                for p in [
                    &mut self.d_uncomp,
                    &mut self.d_comp,
                    &mut self.d_temp,
                    &mut self.d_uncomp_ptrs,
                    &mut self.d_uncomp_sizes,
                    &mut self.d_comp_ptrs,
                    &mut self.d_comp_sizes,
                    &mut self.d_statuses,
                ] {
                    if !p.is_null() {
                        cudaFree(*p);
                        *p = null_mut();
                    }
                }
                for p in [
                    &mut self.h_pinned_input,
                    &mut self.h_pinned_output,
                    &mut self.h_pinned_meta,
                ] {
                    if !p.is_null() {
                        cudaFreeHost(*p);
                        *p = null_mut();
                    }
                }
            }
        }
    }

    impl BatchInner {
        fn ensure_dev(
            slot: &mut *mut c_void,
            cap: &mut usize,
            needed: usize,
        ) -> Result<(), FerroError> {
            if needed == 0 || *cap >= needed {
                return Ok(());
            }
            if !slot.is_null() {
                unsafe { cudaFree(*slot) };
                *slot = null_mut();
                *cap = 0;
            }
            let alloc = needed.div_ceil(1 << 20).max(1) << 20;
            check_cuda(unsafe { cudaMalloc(slot, alloc) }, "cudaMalloc(batch buf)")?;
            *cap = alloc;
            Ok(())
        }

        fn ensure_pinned(
            slot: &mut *mut c_void,
            cap: &mut usize,
            needed: usize,
        ) -> Result<(), FerroError> {
            if needed == 0 || *cap >= needed {
                return Ok(());
            }
            if !slot.is_null() {
                unsafe { cudaFreeHost(*slot) };
                *slot = null_mut();
                *cap = 0;
            }
            let alloc = needed.div_ceil(1 << 20).max(1) << 20;
            check_cuda(
                unsafe { cudaHostAlloc(slot, alloc, cudaHostAllocDefault) },
                "cudaHostAlloc(batch)",
            )?;
            *cap = alloc;
            Ok(())
        }

        fn ensure_metadata(&mut self, chunks: usize) -> Result<(), FerroError> {
            if self.chunks_cap >= chunks {
                return Ok(());
            }
            unsafe {
                for p in [
                    &mut self.d_uncomp_ptrs,
                    &mut self.d_uncomp_sizes,
                    &mut self.d_comp_ptrs,
                    &mut self.d_comp_sizes,
                    &mut self.d_statuses,
                ] {
                    if !p.is_null() {
                        cudaFree(*p);
                        *p = null_mut();
                    }
                }
            }
            let target = chunks.next_power_of_two().max(64);
            let ptr_bytes = target * std::mem::size_of::<*const c_void>();
            let size_bytes = target * std::mem::size_of::<usize>();
            let status_bytes = target * std::mem::size_of::<nvcompStatus_t>();
            check_cuda(
                unsafe { cudaMalloc(&mut self.d_uncomp_ptrs, ptr_bytes) },
                "cudaMalloc(batch uncomp_ptrs)",
            )?;
            check_cuda(
                unsafe { cudaMalloc(&mut self.d_uncomp_sizes, size_bytes) },
                "cudaMalloc(batch uncomp_sizes)",
            )?;
            check_cuda(
                unsafe { cudaMalloc(&mut self.d_comp_ptrs, ptr_bytes) },
                "cudaMalloc(batch comp_ptrs)",
            )?;
            check_cuda(
                unsafe { cudaMalloc(&mut self.d_comp_sizes, size_bytes) },
                "cudaMalloc(batch comp_sizes)",
            )?;
            check_cuda(
                unsafe { cudaMalloc(&mut self.d_statuses, status_bytes) },
                "cudaMalloc(batch statuses)",
            )?;
            self.chunks_cap = target;
            self.h_uncomp_ptrs.resize(target, std::ptr::null());
            self.h_uncomp_sizes.resize(target, 0);
            self.h_comp_ptrs.resize(target, std::ptr::null_mut());
            self.h_comp_sizes.resize(target, 0);
            self.h_statuses.resize(target, 0);
            Ok(())
        }
    }

    /// Per-item layout plan inside the batch.
    struct ItemPlan {
        /// Byte offset of this item in the (256-aligned-per-item) bulk
        /// device input buffer.
        dev_off: usize,
        len: usize,
        /// Index of this item's first chunk in the global chunk table.
        first_chunk: usize,
        n_chunks: usize,
    }

    impl NvcompZstdBatchEncoder {
        pub fn new() -> Result<Self, CodecError> {
            let mut stream: cudaStream_t = null_mut();
            check_cuda(unsafe { cudaStreamCreate(&mut stream) }, "cudaStreamCreate")
                .map_err(ferr)?;
            Ok(Self {
                stream,
                inner: Mutex::new(BatchInner::default()),
            })
        }

        /// CUDA-capable GPU present at runtime? (Same probe the per-object
        /// codec uses.)
        pub fn is_gpu_available() -> bool {
            crate::ferro_compress::NvcompCodec::is_available()
        }

        /// Compress `items` with **one** batched nvCOMP zstd kernel launch.
        ///
        /// Returns one `Result` per item, in input order. Per-chunk nvCOMP
        /// failures are mapped to a per-item `Err` (the rest of the batch
        /// survives); batch-level failures (CUDA alloc, H2D/D2H, launch)
        /// surface as the outer `Err` and the caller is expected to fall
        /// back to its per-object path for the whole batch.
        ///
        /// Each `Ok` carries `(frame, manifest)` where `frame` is an FCG1
        /// frame identical in layout to `NvcompZstdCodec::compress` output
        /// and `manifest` is the standard `CodecKind::NvcompZstd` manifest
        /// (crc32c over the original item bytes).
        #[allow(clippy::type_complexity)]
        pub fn compress_batch(
            &self,
            items: &[Bytes],
        ) -> Result<Vec<Result<(Bytes, ChunkManifest), CodecError>>, CodecError> {
            let mut inner = self
                .inner
                .lock()
                .expect("nvcomp batch encoder inner poisoned");
            self.compress_batch_locked(&mut inner, items).map_err(ferr)
        }

        #[allow(clippy::type_complexity)]
        fn compress_batch_locked(
            &self,
            inner: &mut BatchInner,
            items: &[Bytes],
        ) -> Result<Vec<Result<(Bytes, ChunkManifest), CodecError>>, FerroError> {
            let chunk_size = DEFAULT_CHUNK_SIZE; // 64 KiB — MUST match the per-object path.

            // ---- Plan the batch layout ----
            let mut plans: Vec<ItemPlan> = Vec::with_capacity(items.len());
            let mut dev_off = 0usize;
            let mut chunk_cursor = 0usize;
            for it in items {
                let n_chunks = if it.is_empty() {
                    0
                } else {
                    it.len().div_ceil(chunk_size)
                };
                plans.push(ItemPlan {
                    dev_off,
                    len: it.len(),
                    first_chunk: chunk_cursor,
                    n_chunks,
                });
                // Align the *next* item so every chunk pointer in the table
                // is 256-byte aligned (item base aligned + j*64Ki offsets).
                dev_off += it.len().div_ceil(ALIGN) * ALIGN;
                chunk_cursor += n_chunks;
            }
            let total_chunks = chunk_cursor;
            let total_input_padded = dev_off;

            // All-empty batch: no GPU work at all.
            if total_chunks == 0 {
                return Ok(items
                    .iter()
                    .zip(&plans)
                    .map(|(it, _)| Ok(empty_frame_and_manifest(it, chunk_size)))
                    .collect());
            }

            let raw_max = compress_get_max_output_chunk_size(Algo::Zstd, chunk_size)?;
            let comp_stride = raw_max.div_ceil(ALIGN) * ALIGN;
            let comp_buf_bytes = comp_stride * total_chunks;

            // ---- Grow buffers ----
            BatchInner::ensure_dev(
                &mut inner.d_uncomp,
                &mut inner.d_uncomp_cap,
                total_input_padded,
            )?;
            BatchInner::ensure_dev(&mut inner.d_comp, &mut inner.d_comp_cap, comp_buf_bytes)?;
            inner.ensure_metadata(total_chunks)?;
            BatchInner::ensure_pinned(
                &mut inner.h_pinned_input,
                &mut inner.h_pinned_input_cap,
                total_input_padded,
            )?;
            BatchInner::ensure_pinned(
                &mut inner.h_pinned_output,
                &mut inner.h_pinned_output_cap,
                comp_buf_bytes,
            )?;
            let ptr_bytes = total_chunks * std::mem::size_of::<*const c_void>();
            let size_bytes = total_chunks * std::mem::size_of::<usize>();
            let status_bytes = total_chunks * std::mem::size_of::<nvcompStatus_t>();
            BatchInner::ensure_pinned(
                &mut inner.h_pinned_meta,
                &mut inner.h_pinned_meta_cap,
                // staging for: uncomp_ptrs + uncomp_sizes + comp_ptrs on the
                // way up; comp_sizes + statuses reuse it on the way down.
                (ptr_bytes * 2 + size_bytes).max(size_bytes + status_bytes),
            )?;

            // ---- Stage input + build the global chunk table ----
            let pinned_in = inner.h_pinned_input as *mut u8;
            for (it, plan) in items.iter().zip(&plans) {
                if it.is_empty() {
                    continue;
                }
                // SAFETY: `pinned_in` has `total_input_padded` bytes of
                // capacity and `plan.dev_off + plan.len <= total_input_padded`
                // by construction of the layout plan above.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        it.as_ptr(),
                        pinned_in.add(plan.dev_off),
                        it.len(),
                    );
                }
                for j in 0..plan.n_chunks {
                    let g = plan.first_chunk + j;
                    let off = j * chunk_size;
                    let end = (off + chunk_size).min(plan.len);
                    // SAFETY: pointer arithmetic stays inside the device
                    // allocations sized above; the pointers are never
                    // dereferenced on the host.
                    inner.h_uncomp_ptrs[g] = unsafe {
                        (inner.d_uncomp as *const u8).add(plan.dev_off + off) as *const c_void
                    };
                    inner.h_uncomp_sizes[g] = end - off;
                    inner.h_comp_ptrs[g] =
                        unsafe { (inner.d_comp as *mut u8).add(g * comp_stride) as *mut c_void };
                }
            }

            // ---- Stage metadata into pinned buffer, queue H2D copies ----
            let meta_base = inner.h_pinned_meta as *mut u8;
            // SAFETY: pinned meta buffer was sized for ptr_bytes*2 + size_bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    inner.h_uncomp_ptrs.as_ptr() as *const u8,
                    meta_base,
                    ptr_bytes,
                );
                std::ptr::copy_nonoverlapping(
                    inner.h_uncomp_sizes.as_ptr() as *const u8,
                    meta_base.add(ptr_bytes),
                    size_bytes,
                );
                std::ptr::copy_nonoverlapping(
                    inner.h_comp_ptrs.as_ptr() as *const u8,
                    meta_base.add(ptr_bytes + size_bytes),
                    ptr_bytes,
                );
            }
            check_cuda(
                unsafe {
                    cudaMemcpyAsync(
                        inner.d_uncomp,
                        inner.h_pinned_input,
                        total_input_padded,
                        cudaMemcpyKind::cudaMemcpyHostToDevice,
                        self.stream,
                    )
                },
                "cudaMemcpyAsync(batch input H2D)",
            )?;
            for (dst, src_off, n) in [
                (inner.d_uncomp_ptrs, 0usize, ptr_bytes),
                (inner.d_uncomp_sizes, ptr_bytes, size_bytes),
                (inner.d_comp_ptrs, ptr_bytes + size_bytes, ptr_bytes),
            ] {
                check_cuda(
                    unsafe {
                        cudaMemcpyAsync(
                            dst,
                            meta_base.add(src_off) as *const c_void,
                            n,
                            cudaMemcpyKind::cudaMemcpyHostToDevice,
                            self.stream,
                        )
                    },
                    "cudaMemcpyAsync(batch meta H2D)",
                )?;
            }

            // ---- One kernel launch for the whole batch ----
            let total_uncomp: usize = items.iter().map(|i| i.len()).sum();
            let temp_bytes = compress_get_temp_size(
                Algo::Zstd,
                inner.d_uncomp_ptrs as *const *const c_void,
                inner.d_uncomp_sizes as *const usize,
                total_chunks,
                chunk_size,
                total_uncomp,
                self.stream,
            )?;
            BatchInner::ensure_dev(&mut inner.d_temp, &mut inner.d_temp_cap, temp_bytes)?;
            dispatch_compress(
                Algo::Zstd,
                inner.d_uncomp_ptrs as *const *const c_void,
                inner.d_uncomp_sizes as *const usize,
                chunk_size,
                total_chunks,
                inner.d_temp,
                temp_bytes,
                inner.d_comp_ptrs as *const *mut c_void,
                inner.d_comp_sizes as *mut usize,
                inner.d_statuses as *mut nvcompStatus_t,
                self.stream,
            )?;

            // ---- D2H: sizes + statuses + bulk compressed, single sync ----
            let pinned_out = inner.h_pinned_output as *mut u8;
            check_cuda(
                unsafe {
                    cudaMemcpyAsync(
                        meta_base as *mut c_void,
                        inner.d_comp_sizes,
                        size_bytes,
                        cudaMemcpyKind::cudaMemcpyDeviceToHost,
                        self.stream,
                    )
                },
                "cudaMemcpyAsync(batch comp_sizes D2H)",
            )?;
            check_cuda(
                unsafe {
                    cudaMemcpyAsync(
                        meta_base.add(size_bytes) as *mut c_void,
                        inner.d_statuses,
                        status_bytes,
                        cudaMemcpyKind::cudaMemcpyDeviceToHost,
                        self.stream,
                    )
                },
                "cudaMemcpyAsync(batch statuses D2H)",
            )?;
            check_cuda(
                unsafe {
                    cudaMemcpyAsync(
                        pinned_out as *mut c_void,
                        inner.d_comp as *const c_void,
                        comp_buf_bytes,
                        cudaMemcpyKind::cudaMemcpyDeviceToHost,
                        self.stream,
                    )
                },
                "cudaMemcpyAsync(batch bulk D2H)",
            )?;
            check_cuda(
                unsafe { cudaStreamSynchronize(self.stream) },
                "cudaStreamSynchronize(batch compress)",
            )?;
            // SAFETY: D2H copies above completed at the sync; both regions
            // were written with exactly `total_chunks` entries.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    meta_base as *const usize,
                    inner.h_comp_sizes.as_mut_ptr(),
                    total_chunks,
                );
                std::ptr::copy_nonoverlapping(
                    meta_base.add(size_bytes) as *const nvcompStatus_t,
                    inner.h_statuses.as_mut_ptr(),
                    total_chunks,
                );
            }

            // ---- Assemble per-item FCG1 frames (identical layout to the
            //      per-object path) + per-item manifests ----
            let mut out: Vec<Result<(Bytes, ChunkManifest), CodecError>> =
                Vec::with_capacity(items.len());
            for (it, plan) in items.iter().zip(&plans) {
                if plan.n_chunks == 0 {
                    out.push(Ok(empty_frame_and_manifest(it, chunk_size)));
                    continue;
                }
                let statuses =
                    &inner.h_statuses[plan.first_chunk..plan.first_chunk + plan.n_chunks];
                if let Some((j, st)) = statuses
                    .iter()
                    .enumerate()
                    .find(|(_, st)| **st != nvcompSuccess)
                {
                    out.push(Err(CodecError::Backend(anyhow::anyhow!(
                        "nvcomp batched zstd per-chunk failure at item chunk {j}: status={st} ({})",
                        status_str(*st)
                    ))));
                    continue;
                }
                let sizes = &inner.h_comp_sizes[plan.first_chunk..plan.first_chunk + plan.n_chunks];
                let total_comp: usize = sizes.iter().sum();
                let mut frame: Vec<u8> = Vec::with_capacity(24 + 4 * plan.n_chunks + total_comp);
                write_header(Algo::Zstd, chunk_size, plan.len, sizes, &mut frame);
                let start = frame.len();
                frame.resize(start + total_comp, 0);
                let mut cursor = start;
                for (j, sz) in sizes.iter().enumerate() {
                    let g = plan.first_chunk + j;
                    // SAFETY: `pinned_out` holds `comp_buf_bytes` =
                    // `total_chunks * comp_stride` bytes; chunk `g`'s
                    // compressed size `sz` <= raw_max <= comp_stride.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            pinned_out.add(g * comp_stride),
                            frame.as_mut_ptr().add(cursor),
                            *sz,
                        );
                    }
                    cursor += sz;
                }
                let manifest = ChunkManifest {
                    codec: CodecKind::NvcompZstd,
                    original_size: plan.len as u64,
                    compressed_size: frame.len() as u64,
                    crc32c: crc32c::crc32c(it),
                };
                out.push(Ok((Bytes::from(frame), manifest)));
            }
            Ok(out)
        }
    }

    impl Drop for NvcompZstdBatchEncoder {
        fn drop(&mut self) {
            if !self.stream.is_null() {
                unsafe { cudaStreamDestroy(self.stream) };
            }
        }
    }

    /// FCG1 frame + manifest for a zero-byte object — same shape the
    /// per-object `compress_chunked` emits for empty input (header with
    /// `num_chunks = 0`).
    fn empty_frame_and_manifest(it: &Bytes, chunk_size: usize) -> (Bytes, ChunkManifest) {
        let mut frame = Vec::with_capacity(24);
        write_header(Algo::Zstd, chunk_size, 0, &[], &mut frame);
        let manifest = ChunkManifest {
            codec: CodecKind::NvcompZstd,
            original_size: 0,
            compressed_size: frame.len() as u64,
            crc32c: crc32c::crc32c(it),
        };
        (Bytes::from(frame), manifest)
    }
}

#[cfg(feature = "nvcomp-gpu")]
pub use imp::NvcompZstdBatchEncoder;

#[cfg(all(test, feature = "nvcomp-gpu"))]
mod tests {
    use super::*;
    use crate::Codec;
    use crate::nvcomp::{NvcompZstdCodec, is_gpu_available};
    use bytes::Bytes;

    fn mixed_items() -> Vec<Bytes> {
        // Deterministic pseudo-random bytes (incompressible-ish) without
        // pulling a rand dep.
        fn noise(n: usize, seed: u64) -> Bytes {
            let mut state = seed;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                v.push((state >> 33) as u8);
            }
            Bytes::from(v)
        }
        vec![
            Bytes::from(vec![b'a'; 8 * 1024]),       // compressible 8 KiB
            Bytes::new(),                            // empty
            Bytes::from_static(b"x"),                // 1 byte
            Bytes::from(vec![b'b'; 64 * 1024]),      // exactly one chunk
            Bytes::from(vec![b'c'; 64 * 1024 + 1]),  // chunk boundary + 1
            noise(100 * 1024, 42),                   // multi-chunk noise
            Bytes::from(vec![0u8; 1024 * 1024 - 1]), // just under gpu-min default
            noise(4 * 1024, 7),                      // floor-sized noise
        ]
    }

    /// The compatibility experiment demanded by the design contract:
    /// batched compress output MUST be readable by the **unmodified**
    /// per-object decompress path, byte-for-byte.
    #[tokio::test]
    #[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
    async fn batched_compress_decompresses_via_existing_per_object_path() {
        if !is_gpu_available() {
            eprintln!("skipping: no CUDA GPU detected at runtime");
            return;
        }
        let enc = NvcompZstdBatchEncoder::new().expect("encoder init");
        let per_object = NvcompZstdCodec::new().expect("per-object codec init");
        let items = mixed_items();
        let results = enc.compress_batch(&items).expect("batch compress");
        assert_eq!(results.len(), items.len());
        for (i, (item, res)) in items.iter().zip(results).enumerate() {
            let (frame, manifest) = res.unwrap_or_else(|e| panic!("item {i} compress: {e}"));
            assert_eq!(manifest.codec, crate::CodecKind::NvcompZstd, "item {i}");
            assert_eq!(manifest.original_size, item.len() as u64, "item {i}");
            assert_eq!(manifest.compressed_size, frame.len() as u64, "item {i}");
            // The decompress side is the EXISTING per-object path with no
            // batch-awareness whatsoever.
            let roundtripped = per_object
                .decompress(frame, &manifest)
                .await
                .unwrap_or_else(|e| panic!("item {i} per-object decompress: {e}"));
            assert_eq!(&roundtripped, item, "item {i} byte mismatch");
        }
    }

    /// Frame-layout identity: the batched encoder's output for a single
    /// item parses under the same FCG1 expectations (magic / algo tag /
    /// chunk size / num_chunks) as the per-object encoder's output for the
    /// same input.
    #[tokio::test]
    #[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
    async fn batched_frame_header_matches_per_object_layout() {
        if !is_gpu_available() {
            eprintln!("skipping: no CUDA GPU detected at runtime");
            return;
        }
        let enc = NvcompZstdBatchEncoder::new().expect("encoder init");
        let per_object = NvcompZstdCodec::new().expect("per-object codec init");
        let input = Bytes::from(vec![b'z'; 200 * 1024]);
        let batched = enc
            .compress_batch(std::slice::from_ref(&input))
            .expect("batch")
            .remove(0)
            .expect("item ok");
        let (per_obj_frame, _) = per_object
            .compress(input.clone())
            .await
            .expect("per-object");
        // Fixed header (magic + algo + reserved + orig_size + chunk_size +
        // num_chunks) must be byte-identical between the two paths.
        assert_eq!(
            &batched.0[..24],
            &per_obj_frame[..24],
            "FCG1 fixed header diverged"
        );
    }

    /// Per-item error isolation: a batch where one item is fine must not
    /// be poisoned by metadata of others (all-empty batch exercises the
    /// no-GPU-work path).
    #[tokio::test]
    #[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
    async fn batched_all_empty_batch_short_circuits() {
        if !is_gpu_available() {
            eprintln!("skipping: no CUDA GPU detected at runtime");
            return;
        }
        let enc = NvcompZstdBatchEncoder::new().expect("encoder init");
        let per_object = NvcompZstdCodec::new().expect("per-object codec init");
        let items = vec![Bytes::new(), Bytes::new()];
        let results = enc.compress_batch(&items).expect("batch compress");
        for (i, res) in results.into_iter().enumerate() {
            let (frame, manifest) = res.unwrap_or_else(|e| panic!("item {i}: {e}"));
            let rt = per_object
                .decompress(frame, &manifest)
                .await
                .unwrap_or_else(|e| panic!("item {i} decompress: {e}"));
            assert!(rt.is_empty(), "item {i}");
        }
    }
}
