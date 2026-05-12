//! Phase 2 D-4 v3 — device-direct Bitcomp codec for VRAM-resident
//! compressed CHT.
//!
//! [`NvcompCodec`](crate::NvcompCodec) takes host slices, internally
//! H→D the input, runs nvcomp's batched device kernel, and D→H the
//! compressed bytes back. That's the right shape for "compress this
//! file on disk", but the v3 cache (Phase 2 D-4 in
//! `docs/phase2_d_cht_design.md`) needs the inverse:
//! **input is already on device** (the per-term flat BitmapContainer
//! buffer from Wave 9 v2's [`vram_cht`] insert path), and the
//! **output must stay on device** (the cached compressed entry that
//! subsequent queries decompress in place to a workbench buffer).
//!
//! [`BitcompDeviceCodec`] is the thin wrapper that calls
//! `nvcompBatchedBitcompCompressAsync` /
//! `nvcompBatchedBitcompDecompressAsync` directly with device pointers,
//! amortising the metadata-singleton allocations across all calls
//! (one persistent `(d_uncomp_ptrs, d_uncomp_sizes, d_comp_ptrs,
//! d_comp_sizes, d_statuses)` quintuple per codec instance — sized for
//! single-chunk operation, the hot path for cached terms).
//!
//! ## Single-chunk per operation
//!
//! Each cached term in v3 is one Bitcomp blob. nvcomp's batched API is
//! `num_chunks` parameterised; we set `num_chunks = 1` for the per-call
//! interface and let the chunked higher level (in `vram_cht_v3` or
//! similar) split very large terms across multiple
//! [`compress_one`](BitcompDeviceCodec::compress_one) calls. With the
//! Wave 9 v2 typical cache entry being 80 KiB - 800 KiB (10-100 buckets
//! × 8 KiB each), single-chunk fits well within nvcomp's 16 MiB chunk
//! ceiling.
//!
//! ## Persistent buffers
//!
//! - `d_uncomp_ptrs`: 1 device-pointer slot (8 bytes)
//! - `d_uncomp_sizes`: 1 size_t slot (8 bytes)
//! - `d_comp_ptrs`: 1 device-pointer slot (8 bytes)
//! - `d_comp_sizes`: 1 size_t slot (8 bytes), reused for both compress
//!   (output: actual compressed size) and decompress (input: bound)
//! - `d_uncomp_buffer_sizes`: 1 size_t slot (8 bytes), decompress only
//!   (output buffer bound)
//! - `d_statuses`: 1 nvcompStatus_t slot (4 bytes)
//! - `d_temp`: persistent scratch buffer, grows on demand (init 64 KiB)
//!
//! Total per-codec persistent device footprint: ~64 KiB + grow.
//!
//! ## Multi-chunk batch decompress (Phase 2 D level-A Priority 1)
//!
//! For v3 CHT cohort fold the per-term [`Self::decompress_one`] loop
//! was N sync-points + 5N H→D memcpys (one per metadata array element).
//! [`Self::decompress_batch`] replaces N calls with a single batched
//! `nvcompBatchedBitcompDecompressAsync(num_chunks=N)` followed by a
//! single `cudaStreamSynchronize`. The codec maintains a second set of
//! batch-sized device metadata arrays (`d_batch_*`) sized to the
//! largest cohort observed so far, growing on demand. Empirically a
//! 12-term cohort drops from ~36 µs/cohort (per-term loop) to ~4 µs
//! (one launch + one sync) on RTX 4070 Ti SUPER.

#![cfg(feature = "nvcomp")]

use std::ffi::c_void;
use std::ptr::null_mut;

use crate::algo::BitcompDataType;
use crate::error::{Error, Result};
use crate::nvcomp_sys::cuda::{
    cudaError_t, cudaFree, cudaGetErrorString, cudaMalloc, cudaMemcpy, cudaMemcpyKind,
    cudaStream_t, cudaStreamSynchronize, CUDA_SUCCESS,
};
use crate::slab_alloc::SlabAllocator;
use crate::nvcomp_sys::nvcomp::{
    nvcompBatchedBitcompCompressAsync, nvcompBatchedBitcompCompressGetMaxOutputChunkSize,
    nvcompBatchedBitcompCompressGetTempSizeSync, nvcompBatchedBitcompDecompressAsync,
    nvcompBatchedBitcompDecompressGetTempSizeAsync, nvcompBatchedBitcompDecompressOpts_t,
    nvcompBatchedBitcompFormatOpts, nvcompStatus_t, nvcompSuccess, nvcompType_t,
    NVCOMP_BITCOMP_FORMAT_DEFAULT, NVCOMP_TYPE_CHAR, NVCOMP_TYPE_DOUBLE, NVCOMP_TYPE_FLOAT,
    NVCOMP_TYPE_BFLOAT16, NVCOMP_TYPE_INT, NVCOMP_TYPE_LONGLONG, NVCOMP_TYPE_SHORT,
    NVCOMP_TYPE_UCHAR, NVCOMP_TYPE_UINT, NVCOMP_TYPE_ULONGLONG, NVCOMP_TYPE_USHORT,
};

/// Device-direct Bitcomp codec for v3 VRAM-resident cache.
///
/// Owns a persistent CUDA stream (or borrows the caller's), persistent
/// device-side metadata singletons, and a growable device temp scratch.
/// Drop releases everything.
pub struct BitcompDeviceCodec {
    /// CUDA stream for the batched compress / decompress kernels.
    stream: cudaStream_t,
    /// Owned vs borrowed flag — when `true`, [`Drop`] calls
    /// `cudaStreamDestroy(self.stream)`. When `false` (codec
    /// constructed via [`Self::with_stream`]), the stream's lifetime
    /// is the caller's responsibility.
    owns_stream: bool,
    /// Bitcomp algorithm + data-type opts. Frozen at construction so
    /// the persistent metadata layout is stable.
    format_opts: nvcompBatchedBitcompFormatOpts,
    /// Decompress opts. Default-zero is the canonical value per
    /// nvcomp's `_t` definition (raw `[u8; 64]` zero-init).
    decompress_opts: nvcompBatchedBitcompDecompressOpts_t,
    /// Persistent device metadata singletons (one element each — we
    /// run the batched API with `num_chunks = 1`).
    d_uncomp_ptrs: *mut c_void,
    d_uncomp_sizes: *mut c_void,
    d_comp_ptrs: *mut c_void,
    d_comp_sizes: *mut c_void,
    d_uncomp_buffer_sizes: *mut c_void,
    d_statuses: *mut c_void,
    /// Persistent scratch for nvcomp's temp work (grows on demand).
    d_temp: *mut c_void,
    d_temp_cap: usize,
    /// Batch metadata arrays (decompress only). Sized for the largest
    /// cohort observed so far; grow on demand via
    /// [`Self::ensure_batch_capacity`]. Each holds N elements where N
    /// is `d_batch_cap`. Separate from the singletons so the per-call
    /// [`Self::compress_one`] / [`Self::decompress_one`] paths keep
    /// their amortised 1-element layout.
    d_batch_comp_ptrs: *mut c_void,
    d_batch_comp_sizes: *mut c_void,
    d_batch_uncomp_buffer_sizes: *mut c_void,
    d_batch_uncomp_sizes: *mut c_void,
    d_batch_uncomp_ptrs: *mut c_void,
    d_batch_statuses: *mut c_void,
    /// Element count the batch arrays are allocated for.
    d_batch_cap: usize,
    /// Optional size-class slab allocator for workbench output
    /// buffers used by [`Self::decompress_batch_into_slab`]. `None`
    /// for codecs constructed via [`Self::new`] / [`Self::with_stream`];
    /// `Some` for codecs constructed via [`Self::new_with_slab`] /
    /// [`Self::with_stream_and_slab`] (Wave Z-1 #6). The presence of a
    /// slab does NOT change the behaviour of the existing
    /// [`Self::decompress_batch`] (caller-supplied outputs) — it only
    /// powers the new slab-backed entry point.
    slab: Option<SlabAllocator>,
}

// SAFETY: device pointers + cudaStream_t are opaque process-globals
// owned by the codec for its lifetime. Cross-thread access is
// callers' responsibility to serialise (the codec is not internally
// synchronised; callers wrap in a `Mutex` if shared between threads).
unsafe impl Send for BitcompDeviceCodec {}
unsafe impl Sync for BitcompDeviceCodec {}

impl std::fmt::Debug for BitcompDeviceCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitcompDeviceCodec")
            .field("stream_is_null", &self.stream.is_null())
            .field("owns_stream", &self.owns_stream)
            .field("data_type", &self.format_opts.data_type)
            .field("d_temp_cap", &self.d_temp_cap)
            .finish()
    }
}

impl BitcompDeviceCodec {
    /// Construct a codec on a freshly-created CUDA stream. The stream
    /// is destroyed in [`Drop`]. No slab allocator (Wave Z-1 #6); the
    /// codec only supports caller-supplied output buffers. For
    /// slab-backed workbench pooling use [`Self::new_with_slab`].
    pub fn new(data_type: BitcompDataType) -> Result<Self> {
        Self::new_inner(data_type, /*with_slab=*/ false)
    }

    /// Construct a codec on a caller-owned CUDA stream. The caller
    /// must keep the stream alive for the codec's lifetime; [`Drop`]
    /// does NOT destroy a borrowed stream. No slab allocator.
    pub fn with_stream(data_type: BitcompDataType, stream: cudaStream_t) -> Result<Self> {
        Self::with_stream_internal(data_type, stream, /*owns_stream=*/ false, /*with_slab=*/ false)
    }

    /// Wave Z-1 #6 — construct a codec with a built-in size-class slab
    /// allocator for [`Self::decompress_batch_into_slab`] workbench
    /// outputs. The slab is owned by the codec and reused across all
    /// `decompress_batch_into_slab` calls, amortising per-call
    /// `cudaMalloc` to the first cohort only.
    ///
    /// Existing entry points ([`Self::compress_one`],
    /// [`Self::decompress_one`], [`Self::decompress_batch`]) are
    /// unchanged — they don't touch the slab.
    pub fn new_with_slab(data_type: BitcompDataType) -> Result<Self> {
        Self::new_inner(data_type, /*with_slab=*/ true)
    }

    /// Wave Z-1 #6 — borrowed-stream variant of [`Self::new_with_slab`].
    pub fn with_stream_and_slab(
        data_type: BitcompDataType,
        stream: cudaStream_t,
    ) -> Result<Self> {
        Self::with_stream_internal(data_type, stream, /*owns_stream=*/ false, /*with_slab=*/ true)
    }

    fn new_inner(data_type: BitcompDataType, with_slab: bool) -> Result<Self> {
        let mut stream: cudaStream_t = null_mut();
        // SAFETY: cudaStreamCreate writes a valid stream handle on
        // success; left untouched on failure.
        let rc = unsafe {
            crate::nvcomp_sys::cuda::cudaStreamCreate(&mut stream)
        };
        check_cuda(rc, "cudaStreamCreate(BitcompDeviceCodec)")?;
        match Self::with_stream_internal(data_type, stream, /*owns_stream=*/ true, with_slab) {
            Ok(c) => Ok(c),
            Err(e) => {
                // SAFETY: stream was successfully created above and is
                // exclusively owned by this failed-construction path;
                // safe to destroy.
                unsafe {
                    let _ = crate::nvcomp_sys::cuda::cudaStreamDestroy(stream);
                }
                Err(e)
            }
        }
    }

    fn with_stream_internal(
        data_type: BitcompDataType,
        stream: cudaStream_t,
        owns_stream: bool,
        with_slab: bool,
    ) -> Result<Self> {
        let format_opts = bitcomp_format_opts(data_type);
        let decompress_opts = nvcompBatchedBitcompDecompressOpts_t::default();
        let mut codec = Self {
            stream,
            owns_stream,
            format_opts,
            decompress_opts,
            d_uncomp_ptrs: null_mut(),
            d_uncomp_sizes: null_mut(),
            d_comp_ptrs: null_mut(),
            d_comp_sizes: null_mut(),
            d_uncomp_buffer_sizes: null_mut(),
            d_statuses: null_mut(),
            d_temp: null_mut(),
            d_temp_cap: 0,
            d_batch_comp_ptrs: null_mut(),
            d_batch_comp_sizes: null_mut(),
            d_batch_uncomp_buffer_sizes: null_mut(),
            d_batch_uncomp_sizes: null_mut(),
            d_batch_uncomp_ptrs: null_mut(),
            d_batch_statuses: null_mut(),
            d_batch_cap: 0,
            slab: if with_slab { Some(SlabAllocator::new()) } else { None },
        };
        codec.alloc_metadata_singletons()?;
        Ok(codec)
    }

    /// Compute the maximum compressed size for a single chunk of
    /// `uncompressed_size` bytes. Useful for sizing the cache entry's
    /// `d_compressed` allocation up front.
    pub fn max_compressed_size(
        uncompressed_size: usize,
        data_type: BitcompDataType,
    ) -> Result<usize> {
        let format_opts = bitcomp_format_opts(data_type);
        let mut max_out: usize = 0;
        // SAFETY: nvcomp writes the result to `&mut max_out` on success;
        // arguments are read-only otherwise.
        let status = unsafe {
            nvcompBatchedBitcompCompressGetMaxOutputChunkSize(
                uncompressed_size,
                format_opts,
                &mut max_out,
            )
        };
        check_nvcomp(
            status,
            "nvcompBatchedBitcompCompressGetMaxOutputChunkSize",
        )?;
        // 256-byte alignment matches the existing `compress_chunked`
        // pattern in `nvcomp.rs` (cudaMalloc base alignment + Bitcomp
        // 16-byte output alignment requirement).
        Ok(max_out.div_ceil(256) * 256)
    }

    /// Compress `uncomp_size` bytes from `d_uncompressed` into
    /// `d_compressed` (single chunk). Returns the actual compressed
    /// size in bytes (≤ `max_comp_size`, where `max_comp_size` should
    /// be at least the value returned by [`Self::max_compressed_size`]).
    ///
    /// # Safety
    /// - `d_uncompressed` must point to ≥ `uncomp_size` bytes of
    ///   readable device memory accessible from `self.stream`.
    /// - `d_compressed` must point to ≥ `max_comp_size` bytes of
    ///   writable device memory accessible from `self.stream`.
    /// - `uncomp_size` must be > 0 and ≤ 16 MiB
    ///   (nvcomp Bitcomp single-chunk ceiling).
    pub unsafe fn compress_one(
        &mut self,
        d_uncompressed: *const c_void,
        uncomp_size: usize,
        d_compressed: *mut c_void,
        max_comp_size: usize,
    ) -> Result<usize> {
        if uncomp_size == 0 {
            return Err(Error::Compress(
                "BitcompDeviceCodec::compress_one: uncomp_size must be > 0".into(),
            ));
        }
        if uncomp_size > (1 << 24) {
            return Err(Error::Compress(format!(
                "BitcompDeviceCodec::compress_one: uncomp_size {uncomp_size} exceeds 16 MiB \
                 single-chunk limit; split across multiple calls"
            )));
        }
        // 1) Compute the temp buffer size for this single-chunk batch.
        let mut temp_bytes: usize = 0;
        // SAFETY: GetTempSizeSync reads only the host stack-allocated
        // arrays (single-element slot below) + writes to `&mut
        // temp_bytes`. nvcomp documents this as a query-only call.
        let h_uncomp_ptr = d_uncompressed;
        let h_uncomp_size = uncomp_size;
        let status = unsafe {
            nvcompBatchedBitcompCompressGetTempSizeSync(
                &h_uncomp_ptr as *const *const c_void,
                &h_uncomp_size as *const usize,
                /*num_chunks*/ 1,
                /*max_uncompressed_chunk_bytes*/ uncomp_size,
                self.format_opts,
                &mut temp_bytes,
                /*max_total_uncompressed_bytes*/ uncomp_size,
                self.stream,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompCompressGetTempSizeSync")?;

        // 2) Ensure persistent temp buffer is large enough.
        self.ensure_d_temp(temp_bytes)?;

        // 3) Stage the per-chunk metadata onto device singletons.
        //    Each is a 1-element array (8 bytes for ptr/size).
        // SAFETY: each `d_*` slot was allocated with the size of one
        // element in `alloc_metadata_singletons`; cudaMemcpy fits
        // exactly within bounds.
        let h_comp_ptr = d_compressed;
        unsafe {
            self.h2d_singleton_ptr(self.d_uncomp_ptrs, h_uncomp_ptr)?;
            self.h2d_singleton_size(self.d_uncomp_sizes, h_uncomp_size)?;
            self.h2d_singleton_ptr(self.d_comp_ptrs, h_comp_ptr)?;
        }

        // 4) Launch the batched compress on our stream.
        // SAFETY: device pointers + temp buffer are valid for the
        // duration of the call (we hold the codec's mutex if any;
        // `self` is `&mut`). The async kernel completes before we
        // read `d_comp_sizes` because we sync below.
        let status = unsafe {
            nvcompBatchedBitcompCompressAsync(
                self.d_uncomp_ptrs as *const *const c_void,
                self.d_uncomp_sizes as *const usize,
                /*max_uncompressed_chunk_bytes*/ uncomp_size,
                /*num_chunks*/ 1,
                self.d_temp,
                self.d_temp_cap,
                self.d_comp_ptrs as *const *mut c_void,
                self.d_comp_sizes as *mut usize,
                self.format_opts,
                self.d_statuses as *mut nvcompStatus_t,
                self.stream,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompCompressAsync")?;

        // 5) Sync the stream so the async kernel completes before we
        // read d_comp_sizes back to host.
        // SAFETY: stream is valid (we own it or borrow it from the
        // caller).
        let rc = unsafe { cudaStreamSynchronize(self.stream) };
        check_cuda(rc, "cudaStreamSynchronize(compress)")?;

        // 6) Read the actual compressed size from the device singleton.
        let mut actual_comp_size: usize = 0;
        // SAFETY: d_comp_sizes is a 1-element size_t array on device,
        // written by the kernel above; we read 8 bytes back to host.
        let rc = unsafe {
            cudaMemcpy(
                &mut actual_comp_size as *mut usize as *mut c_void,
                self.d_comp_sizes,
                std::mem::size_of::<usize>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_comp_sizes D2H)")?;

        // 7) Read status for nvcomp-side error.
        let mut status_h: nvcompStatus_t = 0;
        // SAFETY: d_statuses is a 1-element nvcompStatus_t array.
        let rc = unsafe {
            cudaMemcpy(
                &mut status_h as *mut nvcompStatus_t as *mut c_void,
                self.d_statuses,
                std::mem::size_of::<nvcompStatus_t>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_statuses D2H)")?;
        check_nvcomp(status_h, "Bitcomp compress per-chunk status")?;

        if actual_comp_size > max_comp_size {
            return Err(Error::Compress(format!(
                "Bitcomp compress wrote {actual_comp_size} bytes but caller bounded at \
                 {max_comp_size}; max_compressed_size() must be honoured"
            )));
        }
        Ok(actual_comp_size)
    }

    /// Decompress `comp_size` bytes from `d_compressed` into
    /// `d_uncompressed` (single chunk). The expected uncompressed size
    /// must be passed by the caller (Bitcomp's framing carries it
    /// internally; we expose it as a parameter so the caller can
    /// pre-size the workbench buffer).
    ///
    /// # Safety
    /// - `d_compressed` must point to ≥ `comp_size` bytes of readable
    ///   device memory accessible from `self.stream`.
    /// - `d_uncompressed` must point to ≥ `expected_uncomp_size` bytes
    ///   of writable device memory accessible from `self.stream`.
    pub unsafe fn decompress_one(
        &mut self,
        d_compressed: *const c_void,
        comp_size: usize,
        d_uncompressed: *mut c_void,
        expected_uncomp_size: usize,
    ) -> Result<()> {
        if comp_size == 0 {
            return Err(Error::Decompress(
                "BitcompDeviceCodec::decompress_one: comp_size must be > 0".into(),
            ));
        }
        if expected_uncomp_size == 0 {
            return Err(Error::Decompress(
                "BitcompDeviceCodec::decompress_one: expected_uncomp_size must be > 0".into(),
            ));
        }

        // 1) Temp size for the decompress batch.
        let mut temp_bytes: usize = 0;
        // SAFETY: GetTempSizeAsync writes to &mut temp_bytes; other
        // args read-only.
        let status = unsafe {
            nvcompBatchedBitcompDecompressGetTempSizeAsync(
                /*num_chunks*/ 1,
                /*max_uncompressed_chunk_bytes*/ expected_uncomp_size,
                self.decompress_opts,
                &mut temp_bytes,
                /*max_total_uncompressed_bytes*/ expected_uncomp_size,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompDecompressGetTempSizeAsync")?;

        self.ensure_d_temp(temp_bytes)?;

        // 2) Stage per-chunk metadata.
        unsafe {
            self.h2d_singleton_ptr(self.d_comp_ptrs, d_compressed)?;
            self.h2d_singleton_size(self.d_comp_sizes, comp_size)?;
            self.h2d_singleton_size(self.d_uncomp_buffer_sizes, expected_uncomp_size)?;
            self.h2d_singleton_ptr(self.d_uncomp_ptrs, d_uncompressed)?;
        }

        // 3) Launch decompress on our stream. Note nvcomp writes to
        //    `device_uncompressed_chunk_bytes` (= self.d_uncomp_sizes
        //    here, which we treat as output), so we point the API
        //    there even though the slot is shared with compress.
        // SAFETY: device pointers valid, stream valid.
        let status = unsafe {
            nvcompBatchedBitcompDecompressAsync(
                self.d_comp_ptrs as *const *const c_void,
                self.d_comp_sizes as *const usize,
                self.d_uncomp_buffer_sizes as *const usize,
                self.d_uncomp_sizes as *mut usize,
                /*num_chunks*/ 1,
                self.d_temp,
                self.d_temp_cap,
                self.d_uncomp_ptrs as *const *mut c_void,
                self.decompress_opts,
                self.d_statuses as *mut nvcompStatus_t,
                self.stream,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompDecompressAsync")?;

        // 4) Sync + check status.
        let rc = unsafe { cudaStreamSynchronize(self.stream) };
        check_cuda(rc, "cudaStreamSynchronize(decompress)")?;

        let mut status_h: nvcompStatus_t = 0;
        // SAFETY: d_statuses is a 1-element array.
        let rc = unsafe {
            cudaMemcpy(
                &mut status_h as *mut nvcompStatus_t as *mut c_void,
                self.d_statuses,
                std::mem::size_of::<nvcompStatus_t>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_statuses D2H)")?;
        check_nvcomp(status_h, "Bitcomp decompress per-chunk status")?;

        // 5) Read the actual uncompressed size and verify it matches.
        let mut actual_uncomp_size: usize = 0;
        // SAFETY: d_uncomp_sizes is a 1-element array, written above.
        let rc = unsafe {
            cudaMemcpy(
                &mut actual_uncomp_size as *mut usize as *mut c_void,
                self.d_uncomp_sizes,
                std::mem::size_of::<usize>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_uncomp_sizes D2H)")?;
        if actual_uncomp_size != expected_uncomp_size {
            return Err(Error::Decompress(format!(
                "Bitcomp decompress produced {actual_uncomp_size} bytes but caller expected \
                 {expected_uncomp_size}; cache entry corruption?"
            )));
        }
        Ok(())
    }

    /// Decompress N independent chunks in a single batched nvCOMP
    /// call. Replaces an N-iteration loop over [`Self::decompress_one`]
    /// for the v3 CHT cohort fold hot path.
    ///
    /// `entries` is parallel arrays of `(d_compressed, comp_size,
    /// expected_uncomp_size, d_uncompressed)`. The kernel decompresses
    /// `entries[i].0..entries[i].0 + entries[i].1` into
    /// `entries[i].3..entries[i].3 + entries[i].2` for each i. Per-chunk
    /// destination buffers may alias the same workbench at different
    /// offsets (caller's responsibility to ensure non-overlap of
    /// `entries[i].3 .. entries[i].3 + entries[i].2`).
    ///
    /// One `nvcompBatchedBitcompDecompressAsync` + one
    /// `cudaStreamSynchronize` for the entire batch. Per-chunk nvcomp
    /// statuses are read back to host and any non-Success aborts the
    /// call with [`Error::Decompress`].
    ///
    /// # Errors
    /// - Empty `entries` is rejected (no work; caller should not call).
    /// - Any chunk with `comp_size == 0` or `expected_uncomp_size == 0`
    ///   is rejected up-front before any device work.
    /// - Any per-chunk nvcomp status != Success after the kernel
    ///   completes returns `Err(Error::Decompress(...))` with the
    ///   chunk index and the nvcomp status name.
    ///
    /// # Safety
    /// - For each i: `entries[i].0` must point to ≥ `entries[i].1`
    ///   bytes of readable device memory on `self.stream`.
    /// - For each i: `entries[i].3` must point to ≥
    ///   `entries[i].2` bytes of writable device memory on
    ///   `self.stream`. Distinct i must not overlap.
    pub unsafe fn decompress_batch(
        &mut self,
        entries: &[(*const c_void, usize, usize, *mut c_void)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Err(Error::Decompress(
                "BitcompDeviceCodec::decompress_batch: entries must be non-empty".into(),
            ));
        }
        // Per-chunk size validation up-front (cheap, no device work).
        for (i, (_, comp_size, expected_uncomp_size, _)) in entries.iter().enumerate() {
            if *comp_size == 0 {
                return Err(Error::Decompress(format!(
                    "BitcompDeviceCodec::decompress_batch: chunk {i} comp_size == 0"
                )));
            }
            if *expected_uncomp_size == 0 {
                return Err(Error::Decompress(format!(
                    "BitcompDeviceCodec::decompress_batch: chunk {i} expected_uncomp_size == 0"
                )));
            }
        }
        let n = entries.len();
        let max_uncomp = entries
            .iter()
            .map(|(_, _, eu, _)| *eu)
            .max()
            .unwrap_or(0);
        let total_uncomp = entries.iter().map(|(_, _, eu, _)| *eu).sum::<usize>();

        // 1) Ensure batch metadata arrays are sized for N elements.
        self.ensure_batch_capacity(n)?;

        // 2) Temp buffer size for an N-chunk batch.
        let mut temp_bytes: usize = 0;
        // SAFETY: GetTempSizeAsync writes to &mut temp_bytes; other
        // args read-only.
        let status = unsafe {
            nvcompBatchedBitcompDecompressGetTempSizeAsync(
                /*num_chunks*/ n,
                /*max_uncompressed_chunk_bytes*/ max_uncomp,
                self.decompress_opts,
                &mut temp_bytes,
                /*max_total_uncompressed_bytes*/ total_uncomp,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompDecompressGetTempSizeAsync(batch)")?;

        self.ensure_d_temp(temp_bytes)?;

        // 3) Build the host-side parallel arrays, then upload to the
        //    device batch arrays in 4 cudaMemcpy calls (one per array).
        let mut h_comp_ptrs: Vec<*const c_void> = Vec::with_capacity(n);
        let mut h_comp_sizes: Vec<usize> = Vec::with_capacity(n);
        let mut h_uncomp_buffer_sizes: Vec<usize> = Vec::with_capacity(n);
        let mut h_uncomp_ptrs: Vec<*mut c_void> = Vec::with_capacity(n);
        for (d_comp, comp_size, expected_uncomp, d_uncomp) in entries {
            h_comp_ptrs.push(*d_comp);
            h_comp_sizes.push(*comp_size);
            h_uncomp_buffer_sizes.push(*expected_uncomp);
            h_uncomp_ptrs.push(*d_uncomp);
        }

        // SAFETY: each `d_batch_*` is sized for ≥ n elements after
        // ensure_batch_capacity; host arrays are populated with n
        // elements; the cudaMemcpy copies exactly n*sizeof bytes.
        unsafe {
            self.h2d_array(
                self.d_batch_comp_ptrs,
                h_comp_ptrs.as_ptr() as *const c_void,
                n * std::mem::size_of::<*const c_void>(),
            )?;
            self.h2d_array(
                self.d_batch_comp_sizes,
                h_comp_sizes.as_ptr() as *const c_void,
                n * std::mem::size_of::<usize>(),
            )?;
            self.h2d_array(
                self.d_batch_uncomp_buffer_sizes,
                h_uncomp_buffer_sizes.as_ptr() as *const c_void,
                n * std::mem::size_of::<usize>(),
            )?;
            self.h2d_array(
                self.d_batch_uncomp_ptrs,
                h_uncomp_ptrs.as_ptr() as *const c_void,
                n * std::mem::size_of::<*mut c_void>(),
            )?;
        }

        // 4) Launch the batched decompress.
        // SAFETY: every device pointer above is valid and sized; the
        // kernel reads N elements from each array and writes per-chunk
        // status + actual_uncompressed_size. Stream is valid (owned or
        // borrowed).
        let status = unsafe {
            nvcompBatchedBitcompDecompressAsync(
                self.d_batch_comp_ptrs as *const *const c_void,
                self.d_batch_comp_sizes as *const usize,
                self.d_batch_uncomp_buffer_sizes as *const usize,
                self.d_batch_uncomp_sizes as *mut usize,
                /*num_chunks*/ n,
                self.d_temp,
                self.d_temp_cap,
                self.d_batch_uncomp_ptrs as *const *mut c_void,
                self.decompress_opts,
                self.d_batch_statuses as *mut nvcompStatus_t,
                self.stream,
            )
        };
        check_nvcomp(status, "nvcompBatchedBitcompDecompressAsync(batch)")?;

        // 5) Sync the stream so per-chunk statuses are observable.
        // SAFETY: stream valid.
        let rc = unsafe { cudaStreamSynchronize(self.stream) };
        check_cuda(rc, "cudaStreamSynchronize(decompress_batch)")?;

        // 6) Read per-chunk statuses back to host and validate. Also
        //    read actual_uncompressed_sizes to verify byte counts match
        //    caller expectations.
        let mut statuses_h: Vec<nvcompStatus_t> = vec![0; n];
        // SAFETY: d_batch_statuses sized for n; copy n*sizeof bytes.
        let rc = unsafe {
            cudaMemcpy(
                statuses_h.as_mut_ptr() as *mut c_void,
                self.d_batch_statuses,
                n * std::mem::size_of::<nvcompStatus_t>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_batch_statuses D2H)")?;

        let mut actual_sizes_h: Vec<usize> = vec![0; n];
        // SAFETY: d_batch_uncomp_sizes sized for n; copy n*sizeof bytes.
        let rc = unsafe {
            cudaMemcpy(
                actual_sizes_h.as_mut_ptr() as *mut c_void,
                self.d_batch_uncomp_sizes,
                n * std::mem::size_of::<usize>(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        check_cuda(rc, "cudaMemcpy(d_batch_uncomp_sizes D2H)")?;

        for (i, status_h) in statuses_h.iter().enumerate() {
            if *status_h != nvcompSuccess {
                return Err(Error::Decompress(format!(
                    "Bitcomp decompress_batch: chunk {i} status={} ({})",
                    *status_h,
                    crate::nvcomp_sys::nvcomp::status_str(*status_h),
                )));
            }
            let expected = entries[i].2;
            if actual_sizes_h[i] != expected {
                return Err(Error::Decompress(format!(
                    "Bitcomp decompress_batch: chunk {i} produced {} bytes but caller expected {}; \
                     cache entry corruption?",
                    actual_sizes_h[i], expected,
                )));
            }
        }
        Ok(())
    }

    /// Wave Z-1 #6 — slab-backed variant of [`Self::decompress_batch`].
    ///
    /// Allocates per-chunk output buffers from the codec's owned
    /// [`SlabAllocator`], decompresses, and returns the (`ptr`, `size`)
    /// pairs to the caller. The caller is responsible for either
    /// consuming the buffers and calling [`Self::release_slab_outputs`]
    /// (returning the pointers to the slab's free lists for reuse on
    /// the next call), or letting the codec drop (which frees every
    /// pooled buffer via `cudaFree`).
    ///
    /// The codec must have been constructed with a slab via
    /// [`Self::new_with_slab`] or [`Self::with_stream_and_slab`].
    /// Returns an error otherwise.
    ///
    /// `entries` is parallel `(d_compressed, comp_size,
    /// expected_uncomp_size)` triples. Output ordering matches input:
    /// `result[i].0` is the device pointer holding the decompressed
    /// bytes of `entries[i]`, valid for at least
    /// `entries[i].2 = result[i].1` bytes.
    ///
    /// # Errors
    /// - Codec not constructed with a slab: returns
    ///   `Err(Error::Compress("...no slab..."))`.
    /// - Empty `entries`, zero-size chunks: same rejection rules as
    ///   [`Self::decompress_batch`].
    /// - On slab `alloc` or nvcomp failure, **any buffers already
    ///   allocated for this call are released back to the pool** so the
    ///   pool stays consistent.
    ///
    /// # Safety
    /// - For each i: `entries[i].0` must point to ≥ `entries[i].1`
    ///   bytes of readable device memory accessible from `self.stream`.
    pub unsafe fn decompress_batch_into_slab(
        &mut self,
        entries: &[(*const c_void, usize, usize)],
    ) -> Result<Vec<(*mut c_void, usize)>> {
        if self.slab.is_none() {
            return Err(Error::Decompress(
                "BitcompDeviceCodec::decompress_batch_into_slab: codec has no slab; \
                 use new_with_slab() or with_stream_and_slab()"
                    .into(),
            ));
        }
        if entries.is_empty() {
            return Err(Error::Decompress(
                "BitcompDeviceCodec::decompress_batch_into_slab: entries must be non-empty"
                    .into(),
            ));
        }
        // Up-front validation (cheap, no device work).
        for (i, (_, comp_size, expected_uncomp_size)) in entries.iter().enumerate() {
            if *comp_size == 0 {
                return Err(Error::Decompress(format!(
                    "BitcompDeviceCodec::decompress_batch_into_slab: chunk {i} comp_size == 0"
                )));
            }
            if *expected_uncomp_size == 0 {
                return Err(Error::Decompress(format!(
                    "BitcompDeviceCodec::decompress_batch_into_slab: \
                     chunk {i} expected_uncomp_size == 0"
                )));
            }
        }

        // 1) Allocate output buffers from the slab. On failure, roll
        //    back: release what we've allocated so far back to the
        //    pool (the pool stays consistent across error paths).
        let mut outputs: Vec<(*mut c_void, usize)> = Vec::with_capacity(entries.len());
        for (i, (_, _, expected_uncomp)) in entries.iter().enumerate() {
            let slab = self.slab.as_mut().expect("checked above");
            match slab.alloc(*expected_uncomp) {
                Ok(p) => outputs.push((p, *expected_uncomp)),
                Err(e) => {
                    // Roll back what we have.
                    // SAFETY: each (p, sz) came from a slab.alloc(sz)
                    // call earlier in this loop on the same slab.
                    for (p, sz) in outputs.drain(..) {
                        unsafe {
                            self.slab.as_mut().expect("checked").release(p, sz);
                        }
                    }
                    return Err(Error::Decompress(format!(
                        "decompress_batch_into_slab: slab alloc for chunk {i} failed: {e}"
                    )));
                }
            }
        }

        // 2) Build the (comp_ptr, comp_size, expected_uncomp, out_ptr)
        //    quadruples and dispatch through the existing
        //    decompress_batch. On nvcomp failure, release outputs back
        //    to the pool before propagating.
        let dispatch_entries: Vec<(*const c_void, usize, usize, *mut c_void)> = entries
            .iter()
            .zip(outputs.iter())
            .map(|((d_comp, cs, eu), (out_ptr, _))| (*d_comp, *cs, *eu, *out_ptr))
            .collect();
        // SAFETY: comp pointers come from the caller and meet the
        // `decompress_batch` contract; output pointers were just slab-
        // allocated with sizes ≥ expected_uncomp.
        let rc = unsafe { self.decompress_batch(&dispatch_entries) };
        if let Err(e) = rc {
            // SAFETY: each (p, sz) was just slab-allocated above.
            for (p, sz) in outputs.drain(..) {
                unsafe {
                    self.slab.as_mut().expect("checked").release(p, sz);
                }
            }
            return Err(e);
        }

        Ok(outputs)
    }

    /// Wave Z-1 #6 — return previously-handed-out slab output buffers
    /// to the codec's slab for reuse on the next
    /// [`Self::decompress_batch_into_slab`] call.
    ///
    /// `outputs` must be the slice returned by a previous
    /// `decompress_batch_into_slab` (or any subset thereof).
    ///
    /// Idempotent on an empty slice. No-op (silently) if the codec was
    /// constructed without a slab — pointers leak via the caller's
    /// drop path in that case (callers that built a slab-less codec
    /// would not be calling this).
    ///
    /// # Safety
    /// - Every `(ptr, size)` in `outputs` MUST have been returned by
    ///   [`Self::decompress_batch_into_slab`] on **this codec
    ///   instance**, with `size` exactly matching the
    ///   `expected_uncomp_size` from the original `entries` triple.
    /// - The buffers MUST NOT be aliased by any pending kernel launches
    ///   or device-side accesses after this call returns; the slab may
    ///   hand the buffer back out on the next `alloc(size)` call.
    pub unsafe fn release_slab_outputs(&mut self, outputs: &[(*mut c_void, usize)]) {
        if let Some(slab) = self.slab.as_mut() {
            for (p, sz) in outputs {
                // SAFETY: forwarded from caller via the safety contract
                // above (ptrs from this codec's slab, correct sizes).
                unsafe {
                    slab.release(*p, *sz);
                }
            }
        }
    }

    /// Wave Z-1 #6 — borrow the codec's slab allocator (if any) for
    /// observability: high-water mark, pool bytes, hit/miss counters.
    /// Returns `None` for slab-less codecs.
    pub fn slab(&self) -> Option<&SlabAllocator> {
        self.slab.as_ref()
    }

    /// Mutable borrow of the codec's slab allocator (if any). Use for
    /// advanced lifetime management; most callers go through
    /// [`Self::decompress_batch_into_slab`] +
    /// [`Self::release_slab_outputs`].
    pub fn slab_mut(&mut self) -> Option<&mut SlabAllocator> {
        self.slab.as_mut()
    }

    /// Ensure the batch metadata arrays are sized for ≥ `n` elements.
    /// Reallocates with geometric growth on undersize.
    fn ensure_batch_capacity(&mut self, n: usize) -> Result<()> {
        if self.d_batch_cap >= n {
            return Ok(());
        }
        // Grow geometrically; floor at 16 (typical cohort sizes 10-12).
        let new_cap = n.max(self.d_batch_cap * 2).max(16);
        // Free old.
        // SAFETY: each batch pointer was allocated by cudaMalloc on a
        // previous ensure_batch_capacity call (or null on first call);
        // freeing null is checked. Replace with null so partial-failure
        // leaves the codec in a consistent (cap=0, ptrs=null) state.
        unsafe {
            let slots: [&mut *mut c_void; 6] = [
                &mut self.d_batch_comp_ptrs,
                &mut self.d_batch_comp_sizes,
                &mut self.d_batch_uncomp_buffer_sizes,
                &mut self.d_batch_uncomp_sizes,
                &mut self.d_batch_uncomp_ptrs,
                &mut self.d_batch_statuses,
            ];
            for slot in slots {
                let p = std::mem::replace(slot, null_mut());
                if !p.is_null() {
                    let _ = cudaFree(p);
                }
            }
            self.d_batch_cap = 0;
        }
        // Alloc new (each array independently for natural alignment).
        let ptr_bytes = new_cap * std::mem::size_of::<*const c_void>();
        let size_bytes = new_cap * std::mem::size_of::<usize>();
        let status_bytes = new_cap * std::mem::size_of::<nvcompStatus_t>();
        macro_rules! alloc_dev {
            ($field:ident, $bytes:expr, $name:literal) => {{
                let mut p: *mut c_void = null_mut();
                // SAFETY: cudaMalloc writes a valid device pointer on
                // success; untouched on failure.
                let rc = unsafe { cudaMalloc(&mut p, $bytes) };
                if rc != CUDA_SUCCESS {
                    return Err(Error::Compress(format!(
                        concat!("cudaMalloc(", $name, ", {} bytes) failed: code={}"),
                        $bytes, rc
                    )));
                }
                self.$field = p;
            }};
        }
        alloc_dev!(d_batch_comp_ptrs, ptr_bytes, "d_batch_comp_ptrs");
        alloc_dev!(d_batch_comp_sizes, size_bytes, "d_batch_comp_sizes");
        alloc_dev!(
            d_batch_uncomp_buffer_sizes,
            size_bytes,
            "d_batch_uncomp_buffer_sizes"
        );
        alloc_dev!(d_batch_uncomp_sizes, size_bytes, "d_batch_uncomp_sizes");
        alloc_dev!(d_batch_uncomp_ptrs, ptr_bytes, "d_batch_uncomp_ptrs");
        alloc_dev!(d_batch_statuses, status_bytes, "d_batch_statuses");
        self.d_batch_cap = new_cap;
        Ok(())
    }

    /// SAFETY: caller guarantees `slot` points to ≥ `bytes` of device
    /// memory; `host_src` points to ≥ `bytes` of readable host memory.
    unsafe fn h2d_array(
        &self,
        slot: *mut c_void,
        host_src: *const c_void,
        bytes: usize,
    ) -> Result<()> {
        // SAFETY: see fn-doc.
        let rc = unsafe {
            cudaMemcpy(
                slot,
                host_src,
                bytes,
                cudaMemcpyKind::cudaMemcpyHostToDevice,
            )
        };
        check_cuda(rc, "cudaMemcpy(h2d_array)")
    }

    /// Allocate the persistent metadata singletons.
    fn alloc_metadata_singletons(&mut self) -> Result<()> {
        // 1 ptr + 1 size_t + 1 ptr + 1 size_t + 1 size_t + 1 status.
        // We allocate each independently so they have natural
        // alignment for cudaMemcpy use.
        macro_rules! alloc_dev {
            ($field:ident, $bytes:expr, $name:literal) => {{
                let mut p: *mut c_void = null_mut();
                // SAFETY: cudaMalloc writes a valid device pointer on
                // success; left untouched on failure.
                let rc = unsafe { cudaMalloc(&mut p, $bytes) };
                if rc != CUDA_SUCCESS {
                    self.free_metadata_singletons();
                    return Err(Error::Compress(format!(
                        concat!("cudaMalloc(", $name, ") failed: code={}"),
                        rc
                    )));
                }
                self.$field = p;
            }};
        }
        alloc_dev!(d_uncomp_ptrs, std::mem::size_of::<*const c_void>(), "d_uncomp_ptrs");
        alloc_dev!(d_uncomp_sizes, std::mem::size_of::<usize>(), "d_uncomp_sizes");
        alloc_dev!(d_comp_ptrs, std::mem::size_of::<*const c_void>(), "d_comp_ptrs");
        alloc_dev!(d_comp_sizes, std::mem::size_of::<usize>(), "d_comp_sizes");
        alloc_dev!(d_uncomp_buffer_sizes, std::mem::size_of::<usize>(), "d_uncomp_buffer_sizes");
        alloc_dev!(d_statuses, std::mem::size_of::<nvcompStatus_t>(), "d_statuses");
        Ok(())
    }

    fn free_metadata_singletons(&mut self) {
        // SAFETY: each pointer was allocated by `cudaMalloc` above and
        // has not been freed elsewhere. We replace with null to make
        // Drop idempotent.
        unsafe {
            let slots: [&mut *mut c_void; 6] = [
                &mut self.d_uncomp_ptrs,
                &mut self.d_uncomp_sizes,
                &mut self.d_comp_ptrs,
                &mut self.d_comp_sizes,
                &mut self.d_uncomp_buffer_sizes,
                &mut self.d_statuses,
            ];
            for slot in slots {
                let p = std::mem::replace(slot, null_mut());
                if !p.is_null() {
                    let _ = cudaFree(p);
                }
            }
        }
    }

    fn ensure_d_temp(&mut self, needed: usize) -> Result<()> {
        if self.d_temp_cap >= needed {
            return Ok(());
        }
        // Grow with a small over-alloc to amortise.
        let new_cap = needed.max(self.d_temp_cap * 2).max(64 * 1024);
        // Free old.
        // SAFETY: d_temp was allocated by cudaMalloc on a previous
        // ensure_d_temp call (or null on first call); freeing null is
        // a no-op (we explicitly check).
        unsafe {
            if !self.d_temp.is_null() {
                let _ = cudaFree(self.d_temp);
                self.d_temp = null_mut();
                self.d_temp_cap = 0;
            }
            let mut p: *mut c_void = null_mut();
            let rc = cudaMalloc(&mut p, new_cap);
            if rc != CUDA_SUCCESS {
                return Err(Error::Compress(format!(
                    "cudaMalloc(d_temp, {new_cap} bytes) failed: code={rc}"
                )));
            }
            self.d_temp = p;
            self.d_temp_cap = new_cap;
        }
        Ok(())
    }

    /// SAFETY: caller guarantees `slot` points to ≥ 8 bytes of
    /// device memory and `value` is a valid pointer (or null) to copy.
    /// The cudaMemcpy is synchronous H→D of sizeof::<*const c_void>()
    /// bytes.
    unsafe fn h2d_singleton_ptr(&self, slot: *mut c_void, value: *const c_void) -> Result<()> {
        let host_value = value;
        // SAFETY: see fn-doc; slot is a 1-element array on device.
        let rc = unsafe {
            cudaMemcpy(
                slot,
                &host_value as *const *const c_void as *const c_void,
                std::mem::size_of::<*const c_void>(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
            )
        };
        check_cuda(rc, "cudaMemcpy(h2d_singleton_ptr)")
    }

    /// SAFETY: caller guarantees `slot` points to ≥ 8 bytes of
    /// device memory.
    unsafe fn h2d_singleton_size(&self, slot: *mut c_void, value: usize) -> Result<()> {
        let host_value = value;
        // SAFETY: see fn-doc; slot is a 1-element usize on device.
        let rc = unsafe {
            cudaMemcpy(
                slot,
                &host_value as *const usize as *const c_void,
                std::mem::size_of::<usize>(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
            )
        };
        check_cuda(rc, "cudaMemcpy(h2d_singleton_size)")
    }

    /// Borrow the codec's stream for the caller's chained kernel
    /// launches (e.g. v3 cohort fold queues a decompress here, then
    /// scatters into the kernel's flat buffer on the same stream).
    pub fn stream(&self) -> cudaStream_t {
        self.stream
    }
}

impl Drop for BitcompDeviceCodec {
    fn drop(&mut self) {
        self.free_metadata_singletons();
        // SAFETY: each batch array was allocated by cudaMalloc on
        // ensure_batch_capacity; null check covers the never-grown case.
        unsafe {
            let slots: [&mut *mut c_void; 6] = [
                &mut self.d_batch_comp_ptrs,
                &mut self.d_batch_comp_sizes,
                &mut self.d_batch_uncomp_buffer_sizes,
                &mut self.d_batch_uncomp_sizes,
                &mut self.d_batch_uncomp_ptrs,
                &mut self.d_batch_statuses,
            ];
            for slot in slots {
                let p = std::mem::replace(slot, null_mut());
                if !p.is_null() {
                    let _ = cudaFree(p);
                }
            }
            self.d_batch_cap = 0;
        }
        // SAFETY: d_temp was allocated by cudaMalloc on ensure_d_temp;
        // null check covers the never-allocated case.
        unsafe {
            if !self.d_temp.is_null() {
                let _ = cudaFree(self.d_temp);
                self.d_temp = null_mut();
            }
        }
        if self.owns_stream && !self.stream.is_null() {
            // SAFETY: stream was created in `new` and is exclusively
            // owned by this codec (owns_stream = true).
            unsafe {
                let _ = crate::nvcomp_sys::cuda::cudaStreamDestroy(self.stream);
            }
            self.stream = null_mut();
        }
    }
}

// ============================================================
// Internal helpers (mirrored from nvcomp.rs to avoid pub-lifting
// host-side functions just for the v3 wrapper).
// ============================================================

fn check_cuda(rc: cudaError_t, what: &'static str) -> Result<()> {
    if rc == CUDA_SUCCESS {
        return Ok(());
    }
    let msg = unsafe {
        let s = cudaGetErrorString(rc);
        if s.is_null() {
            "<null>".to_string()
        } else {
            std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    };
    Err(Error::Compress(format!("CUDA error in {what}: {msg} (code={rc})")))
}

fn check_nvcomp(status: nvcompStatus_t, what: &'static str) -> Result<()> {
    if status == nvcompSuccess {
        Ok(())
    } else {
        Err(Error::Compress(format!("nvCOMP error in {what}: code={status}")))
    }
}

fn bitcomp_to_nvcomp_type(dt: BitcompDataType) -> nvcompType_t {
    match dt {
        BitcompDataType::Char => NVCOMP_TYPE_CHAR,
        BitcompDataType::Uint8 => NVCOMP_TYPE_UCHAR,
        BitcompDataType::Uint16 => NVCOMP_TYPE_USHORT,
        BitcompDataType::Uint32 => NVCOMP_TYPE_UINT,
        BitcompDataType::Uint64 => NVCOMP_TYPE_ULONGLONG,
        BitcompDataType::Int8 => NVCOMP_TYPE_CHAR,
        BitcompDataType::Int16 => NVCOMP_TYPE_SHORT,
        BitcompDataType::Int32 => NVCOMP_TYPE_INT,
        BitcompDataType::Int64 => NVCOMP_TYPE_LONGLONG,
        BitcompDataType::Float32 => NVCOMP_TYPE_FLOAT,
        BitcompDataType::Float64 => NVCOMP_TYPE_DOUBLE,
        BitcompDataType::BFloat16 => NVCOMP_TYPE_BFLOAT16,
    }
}

fn bitcomp_format_opts(dt: BitcompDataType) -> nvcompBatchedBitcompFormatOpts {
    nvcompBatchedBitcompFormatOpts {
        algorithm_type: NVCOMP_BITCOMP_FORMAT_DEFAULT as std::ffi::c_int,
        data_type: bitcomp_to_nvcomp_type(dt),
        reserved: [0; 56],
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvcomp_sys::cuda::cudaMemcpy;

    /// Helper: try to construct the codec; returns `None` on a
    /// driver-missing host so tests skip cleanly.
    fn try_codec() -> Option<BitcompDeviceCodec> {
        BitcompDeviceCodec::new(BitcompDataType::Uint32).ok()
    }

    /// Helper: cudaMalloc + cudaMemcpy host bytes onto device.
    /// Returns the device pointer; caller frees via `cudaFree`.
    unsafe fn upload_to_device(host: &[u8]) -> *mut c_void {
        let mut p: *mut c_void = null_mut();
        let rc = unsafe { cudaMalloc(&mut p, host.len()) };
        assert_eq!(rc, CUDA_SUCCESS);
        let rc = unsafe {
            cudaMemcpy(
                p,
                host.as_ptr() as *const c_void,
                host.len(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
            )
        };
        assert_eq!(rc, CUDA_SUCCESS);
        p
    }

    /// Helper: cudaMalloc + return uninitialised device pointer of
    /// `bytes` size.
    unsafe fn alloc_device(bytes: usize) -> *mut c_void {
        let mut p: *mut c_void = null_mut();
        let rc = unsafe { cudaMalloc(&mut p, bytes) };
        assert_eq!(rc, CUDA_SUCCESS);
        p
    }

    /// Helper: cudaMemcpy device bytes back to host.
    unsafe fn download_from_device(d_src: *const c_void, host: &mut [u8]) {
        let rc = unsafe {
            cudaMemcpy(
                host.as_mut_ptr() as *mut c_void,
                d_src,
                host.len(),
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
            )
        };
        assert_eq!(rc, CUDA_SUCCESS);
    }

    #[test]
    fn roundtrip_small_uint32() {
        let Some(mut codec) = try_codec() else { return };
        // Realistic-ish input: 2048 u32 = 8 KiB (one BitmapContainer's worth).
        let words: Vec<u32> = (0..2048u32).map(|i| (i * 31) & 0xff_ff).collect();
        let input_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4)
        };
        let max_comp = BitcompDeviceCodec::max_compressed_size(
            input_bytes.len(),
            BitcompDataType::Uint32,
        )
        .unwrap();
        // SAFETY: round-trip with proper alloc/free below.
        unsafe {
            let d_uncomp = upload_to_device(input_bytes);
            let d_comp = alloc_device(max_comp);
            let actual_comp = codec
                .compress_one(d_uncomp, input_bytes.len(), d_comp, max_comp)
                .expect("compress");
            assert!(actual_comp > 0);
            assert!(actual_comp <= max_comp);
            // Decompress back.
            let d_decomp = alloc_device(input_bytes.len());
            codec
                .decompress_one(d_comp, actual_comp, d_decomp, input_bytes.len())
                .expect("decompress");
            // Verify byte-equal vs original.
            let mut got = vec![0u8; input_bytes.len()];
            download_from_device(d_decomp, &mut got);
            assert_eq!(got, input_bytes);
            let _ = cudaFree(d_uncomp);
            let _ = cudaFree(d_comp);
            let _ = cudaFree(d_decomp);
        }
    }

    #[test]
    fn compress_zero_size_is_error() {
        let Some(mut codec) = try_codec() else { return };
        // SAFETY: nullable inputs because the function rejects size=0
        // before touching memory.
        let res = unsafe { codec.compress_one(null_mut(), 0, null_mut(), 0) };
        assert!(res.is_err());
    }

    #[test]
    fn decompress_zero_size_is_error() {
        let Some(mut codec) = try_codec() else { return };
        let res = unsafe { codec.decompress_one(null_mut(), 0, null_mut(), 1) };
        assert!(res.is_err());
        let res = unsafe { codec.decompress_one(null_mut(), 1, null_mut(), 0) };
        assert!(res.is_err());
    }

    #[test]
    fn compress_oversize_chunk_is_error() {
        let Some(mut codec) = try_codec() else { return };
        // SAFETY: the function rejects oversize before reading memory.
        let res = unsafe {
            codec.compress_one(null_mut(), (1 << 24) + 1, null_mut(), 1 << 25)
        };
        assert!(res.is_err());
    }

    #[test]
    fn max_compressed_size_round_to_256() {
        let n = BitcompDeviceCodec::max_compressed_size(8192, BitcompDataType::Uint32)
            .expect("max_compressed_size");
        // Should be ≥ 8192 (worst-case is roughly input + small header)
        // and a multiple of 256.
        assert!(n >= 8192);
        assert_eq!(n % 256, 0);
    }
}
