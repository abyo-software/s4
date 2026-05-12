//! Phase F-1.5 — nvCOMP HLIF (High-Level Interface) backend.
//!
//! [`NvcompCodec`](crate::NvcompCodec) drives the **batched** nvCOMP API
//! directly with our own `FCG1` framing. The HLIF backend in this module
//! is the *other* nvCOMP API: a managed `nvcompManagerBase` instance that
//! produces NVCOMP_NATIVE self-describing frames. The HLIF call path is
//! exactly what the Phase F-0 #3 (PCIe E2E) + #5 (segment pipeline)
//! benchmarks measured — so wiring it into a Rust `Codec` impl is the
//! shortest path to making the gen4 L4 / L40S cross-validated **1300-1800×
//! ingestion vs CPU zstd-22** number reproducible from inside ferro-* tests.
//!
//! ## Two backends
//!
//! - [`BitcompHlifBackend`] — Bitcomp algo 0 (default), typed by
//!   [`crate::BitcompDataType`]. Reference codec for posting lists and
//!   typed numeric columns. Phase F-0 #1 measured **3.60×** ratio + 280
//!   GB/s internal compress on 4070 Ti SUPER on `postings.bin (u32)`.
//! - [`ZstdHlifBackend`] — nvCOMP zstd with default opts (no dict,
//!   default level). Reference codec for text / keyword / JSON
//!   columns. Phase F-0 #5 measured **L40S production target = 4.83
//!   GB/s E2E** on the segment-pipeline mix.
//!
//! ## Why HLIF over the existing batched [`NvcompCodec`]?
//!
//! - **Self-describing frames**: HLIF produces NVCOMP_NATIVE bitstreams
//!   that carry their own metadata. `decompress` doesn't need an external
//!   `(chunk_size, num_chunks, per-chunk-sizes)` sidecar like the
//!   `FCG1`-framed codec. This is what lets `ferro-storage` round-trip a
//!   compressed segment without holding ferro-internal framing tables.
//! - **NVIDIA frame-format interop**: the produced bitstreams can be
//!   round-tripped by any other nvCOMP HLIF consumer (e.g. RAPIDS,
//!   third-party readers). Useful for Frozen-tier publication / data
//!   exchange with downstream tools.
//! - **C++ stack already validated**: Phase F-0 ran exactly this
//!   `BitcompManager` / `ZstdManager` pair through 15 cells × 3 GPUs.
//!   Re-using the same managers from Rust avoids a "did we measure the
//!   same code path" debate during Phase F-1 acceptance.
//!
//! ## Implementation
//!
//! The HLIF managers are C++-only (`std::shared_ptr<nvcompManagerBase>`,
//! `std::unique_ptr<CompressionConfig>`). We expose them via a thin
//! C-ABI shim (`src/cuda_kernels/nvcomp_hlif_shim.cpp`) and bind to it
//! with manual `extern "C"` declarations — no bindgen, no cxx, no
//! generated code in the tree.
//!
//! ## Constructor surface (for Phase F-1.6 wiring)
//!
//! Both backends are constructed with a single owned `cudaStream_t` and
//! an algo-specific options struct. The Rust side owns the stream
//! lifetime: the C++ shim borrows it for the manager's lifetime but does
//! NOT destroy it. Drop order:
//!
//! 1. The backend's `Drop` runs `ferro_nvcomp_hlif_destroy` which
//!    sequence-destroys the cached config → the manager → the handle.
//! 2. Then the backend's `Drop` runs `cudaStreamDestroy` on the owned
//!    stream.
//!
//! This is the order the Phase F-0 #3 reference harness used (manager
//! scope `{}` then `cudaStreamDestroy`); deviating from it segfaults.

#![cfg(feature = "nvcomp-gpu")]

use std::ffi::{c_int, c_void};
use std::ptr::null_mut;
use std::sync::Mutex;

use super::algo::BitcompDataType;
use super::error::{Error, Result};
use super::nvcomp_sys::cuda::{
    cudaError_t, cudaFree, cudaGetDeviceCount, cudaGetErrorString, cudaMalloc, cudaMemcpyAsync,
    cudaMemcpyKind, cudaStreamCreate, cudaStreamDestroy, cudaStreamSynchronize, cudaStream_t,
    CUDA_SUCCESS,
};
use super::nvcomp_sys::nvcomp::{
    nvcompType_t, NVCOMP_TYPE_BFLOAT16, NVCOMP_TYPE_CHAR, NVCOMP_TYPE_DOUBLE, NVCOMP_TYPE_FLOAT,
    NVCOMP_TYPE_INT, NVCOMP_TYPE_LONGLONG, NVCOMP_TYPE_SHORT, NVCOMP_TYPE_UCHAR, NVCOMP_TYPE_UINT,
    NVCOMP_TYPE_ULONGLONG, NVCOMP_TYPE_USHORT,
};
use super::{Algo, Codec};

// ---------- C ABI bindings to nvcomp_hlif_shim.cpp ----------
//
// All entry points return `c_int` where 0 == success. On non-zero return,
// `ferro_nvcomp_hlif_last_error_message` provides a human-readable detail
// string for the thread that took the error.

unsafe extern "C" {
    fn ferro_nvcomp_hlif_create_bitcomp(
        chunk_size: usize,
        algorithm: c_int,
        data_type: nvcompType_t,
        user_stream: cudaStream_t,
        out_handle: *mut *mut c_void,
    ) -> c_int;

    fn ferro_nvcomp_hlif_create_zstd(
        chunk_size: usize,
        user_stream: cudaStream_t,
        out_handle: *mut *mut c_void,
    ) -> c_int;

    fn ferro_nvcomp_hlif_destroy(handle: *mut c_void);

    fn ferro_nvcomp_hlif_max_compressed_size(
        handle: *mut c_void,
        uncomp_bytes: usize,
        out_max_bytes: *mut usize,
    ) -> c_int;

    fn ferro_nvcomp_hlif_compress(
        handle: *mut c_void,
        d_in: *const u8,
        uncomp_bytes: usize,
        d_out: *mut u8,
        out_comp_bytes: *mut usize,
    ) -> c_int;

    fn ferro_nvcomp_hlif_decompress(
        handle: *mut c_void,
        d_comp: *const u8,
        d_out: *mut u8,
        out_decomp_bytes: *mut usize,
    ) -> c_int;

    fn ferro_nvcomp_hlif_get_decompressed_output_size(
        handle: *mut c_void,
        d_comp: *const u8,
        out_decomp_bytes: *mut usize,
    ) -> c_int;

    fn ferro_nvcomp_hlif_last_error_message(
        buf: *mut std::ffi::c_char,
        buf_capacity: usize,
    ) -> usize;
}

// Default HLIF chunk size — matches Phase F-0 #3 / #5 canonical setting
// (64 KiB) which is also nvcomp's recommended default for zstd. Bitcomp
// is robust across 16 KiB-256 KiB but 64 KiB was the cell used in every
// 3-GPU comparison so we keep it as the default for evidence continuity.
pub const DEFAULT_HLIF_CHUNK_SIZE: usize = 65_536;

fn pull_shim_error() -> String {
    // 1 KiB is enough for every message the shim produces. If a future
    // message exceeds this, the buffer is truncated with a trailing NUL.
    let mut buf = [0u8; 1024];
    let written = unsafe {
        ferro_nvcomp_hlif_last_error_message(buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len())
    };
    let copied = written.min(buf.len() - 1);
    // Trim at the NUL terminator the shim writes when we have room.
    let end = buf[..copied].iter().position(|&b| b == 0).unwrap_or(copied);
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn check_shim(rc: c_int, what: &'static str) -> Result<()> {
    if rc == 0 {
        return Ok(());
    }
    let msg = pull_shim_error();
    Err(Error::Compress(format!(
        "HLIF shim error in {what}: rc=0x{rc:x} msg={msg}"
    )))
}

fn check_cuda(rc: cudaError_t, what: &'static str) -> Result<()> {
    if rc == CUDA_SUCCESS {
        return Ok(());
    }
    let msg = unsafe {
        let s = cudaGetErrorString(rc);
        if s.is_null() {
            "unknown".to_string()
        } else {
            std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    };
    Err(Error::Compress(format!(
        "CUDA error in {what}: code={rc} ({msg})"
    )))
}

/// Detect whether the runtime CUDA driver sees at least one device.
/// Cheap call — cudaGetDeviceCount is non-blocking when no context exists.
pub fn cuda_available() -> bool {
    let mut count: c_int = 0;
    let rc = unsafe { cudaGetDeviceCount(&mut count) };
    rc == CUDA_SUCCESS && count > 0
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

// ---------- Inner state shared by both backends ----------
//
// The shim handle is `*mut c_void` pointing at an opaque C++ ManagerHandle.
// Both backends own one of these + a CUDA stream. We share the access
// pattern via a small inner struct rather than two copies of the same
// Drop / unsafe-send-sync boilerplate.

struct HlifInner {
    handle: *mut c_void,
    stream: cudaStream_t,
    // Persistent device-buffer pool (mirrors the existing batched
    // [`crate::NvcompCodec`] pattern). Per-call `cudaMalloc` of multi-MiB
    // buffers eats ~10 ms per allocation on RTX 4070 Ti SUPER, which
    // dominates the timed region for a 4 MiB ingest call and pins the
    // observed E2E throughput at ~3 GB/s vs the Phase F-0 reference 10
    // GB/s. Holding the buffers across calls (grow-only, free on Drop)
    // closes that gap.
    //
    // Capacities are tracked separately so we can `cudaFree + cudaMalloc`
    // exactly when an input outgrows what we have. Rounded up to 1 MiB
    // increments to amortise future growth.
    d_in: *mut c_void,
    d_in_cap: usize,
    d_out: *mut c_void,
    d_out_cap: usize,
    d_decomp: *mut c_void,
    d_decomp_cap: usize,
}

impl Drop for HlifInner {
    fn drop(&mut self) {
        // Order: manager → device pool → stream. The manager's destructor
        // synchronises on `self.stream`, so the stream MUST still be alive
        // when `ferro_nvcomp_hlif_destroy` runs. The Phase F-0 #3 harness
        // captured this in its inner-block scope around the manager.
        if !self.handle.is_null() {
            unsafe { ferro_nvcomp_hlif_destroy(self.handle) };
            self.handle = null_mut();
        }
        for slot in [&mut self.d_in, &mut self.d_out, &mut self.d_decomp] {
            if !slot.is_null() {
                unsafe {
                    let _ = cudaFree(*slot);
                }
                *slot = null_mut();
            }
        }
        if !self.stream.is_null() {
            unsafe { cudaStreamDestroy(self.stream) };
            self.stream = null_mut();
        }
    }
}

impl HlifInner {
    fn new(handle: *mut c_void, stream: cudaStream_t) -> Self {
        Self {
            handle,
            stream,
            d_in: null_mut(),
            d_in_cap: 0,
            d_out: null_mut(),
            d_out_cap: 0,
            d_decomp: null_mut(),
            d_decomp_cap: 0,
        }
    }

    /// Grow a slot to at least `needed` bytes, freeing the old buffer
    /// first if necessary. Capacities are rounded up to 1 MiB to amortise
    /// future growth. No-op when the slot already fits.
    fn ensure_buf(slot: &mut *mut c_void, cap: &mut usize, needed: usize) -> Result<()> {
        if needed == 0 {
            return Ok(());
        }
        if *cap >= needed {
            return Ok(());
        }
        if !slot.is_null() {
            unsafe {
                let _ = cudaFree(*slot);
            }
            *slot = null_mut();
            *cap = 0;
        }
        let alloc_size = needed.div_ceil(1 << 20).max(1) << 20;
        check_cuda(
            unsafe { cudaMalloc(slot, alloc_size) },
            "cudaMalloc(hlif pool)",
        )?;
        *cap = alloc_size;
        Ok(())
    }
}

// SAFETY: The HLIF shim handle is owned by exactly one backend. Concurrent
// calls into the same handle are serialised by the outer Mutex in each
// backend (see [`BitcompHlifBackend`] / [`ZstdHlifBackend`]). Different
// instances run on different streams and don't share state — Send + Sync
// are sound at the inner-struct level.
unsafe impl Send for HlifInner {}
unsafe impl Sync for HlifInner {}

// ---------- Bitcomp HLIF backend ----------

/// Bitcomp HLIF codec — Phase F-0 採択 codec for posting lists / typed
/// numeric columns.
///
/// Construct with a [`BitcompDataType`] hint and an optional chunk size
/// (defaults to [`DEFAULT_HLIF_CHUNK_SIZE`] = 64 KiB to match Phase F-0
/// canonical cells). The chunk size is internal to nvcomp's chunked
/// pipeline; the codec's `Codec::compress` / `decompress` accept inputs
/// of arbitrary size.
pub struct BitcompHlifBackend {
    inner: Mutex<HlifInner>,
    data_type: BitcompDataType,
    chunk_size: usize,
}

impl std::fmt::Debug for BitcompHlifBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitcompHlifBackend")
            .field("data_type", &self.data_type)
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

impl BitcompHlifBackend {
    /// Construct with the default chunk size (64 KiB).
    pub fn new(data_type: BitcompDataType) -> Result<Self> {
        Self::with_chunk_size(data_type, DEFAULT_HLIF_CHUNK_SIZE)
    }

    /// Construct with an explicit `chunk_size` in bytes. Must be in
    /// `(0, 16 MiB]` — the shim rejects anything outside that range with
    /// `Error::Compress`.
    pub fn with_chunk_size(data_type: BitcompDataType, chunk_size: usize) -> Result<Self> {
        if !cuda_available() {
            return Err(Error::BackendUnavailable("no CUDA device available"));
        }
        let mut stream: cudaStream_t = null_mut();
        check_cuda(
            unsafe { cudaStreamCreate(&mut stream) },
            "cudaStreamCreate(BitcompHlifBackend)",
        )?;
        let mut handle: *mut c_void = null_mut();
        // Phase F-0 closure pins algorithm to 0. The shim still accepts
        // an int so a future Bitcomp algo-1 (sparse) variant can land
        // without re-stamping the C ABI.
        let rc = unsafe {
            ferro_nvcomp_hlif_create_bitcomp(
                chunk_size,
                0,
                bitcomp_to_nvcomp_type(data_type),
                stream,
                &mut handle,
            )
        };
        if rc != 0 {
            // The stream is alive but the manager construction failed —
            // destroy the stream before bubbling the error.
            unsafe { cudaStreamDestroy(stream) };
            return Err(Error::Compress(format!(
                "ferro_nvcomp_hlif_create_bitcomp failed: rc=0x{rc:x} msg={}",
                pull_shim_error()
            )));
        }
        Ok(Self {
            inner: Mutex::new(HlifInner::new(handle, stream)),
            data_type,
            chunk_size,
        })
    }

    /// Stream this backend was constructed against. Exposed so a hybrid
    /// caller (e.g. `BitmapOpKernel`) can share it for fence-free pipeline
    /// stages on the same stream. The stream is owned by this backend
    /// and MUST NOT be destroyed by the caller.
    pub fn cuda_stream(&self) -> cudaStream_t {
        // Borrow the stream from the inner state. Caller must respect
        // the "do not destroy" contract documented above.
        self.inner
            .lock()
            .expect("BitcompHlifBackend inner poisoned")
            .stream
    }
}

impl Codec for BitcompHlifBackend {
    fn algo(&self) -> Algo {
        Algo::Bitcomp {
            data_type: self.data_type,
        }
    }

    fn compress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .expect("BitcompHlifBackend inner poisoned");
        compress_via_hlif(&mut inner, input, output)
    }

    fn decompress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .expect("BitcompHlifBackend inner poisoned");
        decompress_via_hlif(&mut inner, input, output)
    }

    fn max_compressed_len(&self, uncompressed_len: usize) -> usize {
        // HLIF's `configure_compression(N).max_compressed_buffer_size`
        // requires a live shim call. The caller of `max_compressed_len`
        // expects a synchronous, infallible answer — so we approximate
        // with an upper bound derived from the same logic the shim uses
        // internally: pass-through bytes + nvcomp NVCOMP_NATIVE framing
        // overhead. The shim itself enforces the actual bound at
        // compress time, so this is a sizing hint, not a contract.
        //
        // The conservative estimate below is identical to what the
        // existing `NvcompCodec` uses for Bitcomp:
        //   payload <= uncompressed_len + uncompressed_len / 64 + 64
        // plus an NVCOMP_NATIVE header of <= 512 bytes (HLIF actual
        // measured at 256 bytes for both Bitcomp and zstd on Phase F-0).
        let num_chunks = uncompressed_len.div_ceil(self.chunk_size).max(1);
        let per_chunk_overhead = 64usize;
        uncompressed_len + uncompressed_len / 64 + per_chunk_overhead * num_chunks + 512
    }
}

// ---------- zstd HLIF backend ----------

/// nvCOMP zstd HLIF codec — Phase F-0 採択 codec for text / keyword /
/// JSON columns.
///
/// Uses `nvcompBatchedZstdCompressDefaultOpts` (no dict, default level) —
/// pinned by Phase F-0 #2 + #2.5 dict-loses-on-whole-file verification.
/// Construct with an optional chunk size (default 64 KiB to match
/// Phase F-0 canonical cells; nvcomp recommends 64-128 KiB for zstd).
pub struct ZstdHlifBackend {
    inner: Mutex<HlifInner>,
    chunk_size: usize,
}

impl std::fmt::Debug for ZstdHlifBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZstdHlifBackend")
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

impl ZstdHlifBackend {
    pub fn new() -> Result<Self> {
        Self::with_chunk_size(DEFAULT_HLIF_CHUNK_SIZE)
    }

    pub fn with_chunk_size(chunk_size: usize) -> Result<Self> {
        if !cuda_available() {
            return Err(Error::BackendUnavailable("no CUDA device available"));
        }
        let mut stream: cudaStream_t = null_mut();
        check_cuda(
            unsafe { cudaStreamCreate(&mut stream) },
            "cudaStreamCreate(ZstdHlifBackend)",
        )?;
        let mut handle: *mut c_void = null_mut();
        let rc = unsafe { ferro_nvcomp_hlif_create_zstd(chunk_size, stream, &mut handle) };
        if rc != 0 {
            unsafe { cudaStreamDestroy(stream) };
            return Err(Error::Compress(format!(
                "ferro_nvcomp_hlif_create_zstd failed: rc=0x{rc:x} msg={}",
                pull_shim_error()
            )));
        }
        Ok(Self {
            inner: Mutex::new(HlifInner::new(handle, stream)),
            chunk_size,
        })
    }

    pub fn cuda_stream(&self) -> cudaStream_t {
        self.inner
            .lock()
            .expect("ZstdHlifBackend inner poisoned")
            .stream
    }
}

impl Codec for ZstdHlifBackend {
    fn algo(&self) -> Algo {
        Algo::Zstd
    }

    fn compress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self.inner.lock().expect("ZstdHlifBackend inner poisoned");
        compress_via_hlif(&mut inner, input, output)
    }

    fn decompress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self.inner.lock().expect("ZstdHlifBackend inner poisoned");
        decompress_via_hlif(&mut inner, input, output)
    }

    fn max_compressed_len(&self, uncompressed_len: usize) -> usize {
        // Same sizing rationale as Bitcomp — see BitcompHlifBackend::max_compressed_len.
        // zstd default level has a tighter pass-through bound (chunk_size +
        // chunk_size/200 + 64) than Bitcomp; we mirror that here.
        let num_chunks = uncompressed_len.div_ceil(self.chunk_size).max(1);
        let per_chunk_overhead = 64usize;
        uncompressed_len + uncompressed_len / 200 + per_chunk_overhead * num_chunks + 512
    }
}

// ---------- Shared host/device staging ----------
//
// `Codec::compress` / `decompress` take HOST slices but HLIF operates on
// DEVICE pointers. We hold persistent device buffers in `HlifInner` (grow-
// only, freed on Drop) and copy through them. Per-call cudaMalloc was
// observed at ~10 ms of fixed overhead on 4 MiB inputs (RTX 4070 Ti SUPER,
// debug-mode KVM host), pinning the E2E throughput at 3 GB/s vs the Phase
// F-0 reference 10 GB/s. The pool closes that gap.
//
// Pinned-host staging on the bulk H2D / D2H path is intentionally omitted:
// Phase 1.5 measurements showed the CUDA driver's internal pinned pipeline
// beat user-space pinned staging at 256 MB-1 GB sizes (4.32 GB/s vs 5.59
// GB/s for decompress). Smaller inputs are dominated by the kernel itself
// either way.

fn compress_via_hlif(inner: &mut HlifInner, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
    if input.is_empty() {
        // HLIF refuses zero-byte input; return an empty output as the
        // identity-frame so round-trip works without a special case.
        // We mark it with a 1-byte sentinel `0x00` so decompress can
        // detect and short-circuit. NOTE: this is local to ferro-compress's
        // wrapping — a real HLIF NVCOMP_NATIVE frame is always >= 16 bytes
        // for any non-empty payload.
        output.push(0x00);
        return Ok(());
    }
    // Size the output buffer using the cached HLIF config (one shim
    // round-trip per size shift; the shim itself caches by uncomp_bytes).
    let mut max_comp_bytes: usize = 0;
    let rc = unsafe {
        ferro_nvcomp_hlif_max_compressed_size(inner.handle, input.len(), &mut max_comp_bytes)
    };
    check_shim(rc, "max_compressed_size")?;

    // Grow the persistent input + output pools to fit this call.
    HlifInner::ensure_buf(&mut inner.d_in, &mut inner.d_in_cap, input.len())?;
    HlifInner::ensure_buf(
        &mut inner.d_out,
        &mut inner.d_out_cap,
        max_comp_bytes.max(1),
    )?;

    // H2D bulk input — async on the manager's stream so HLIF compress
    // can begin as soon as the H2D pipelines through the driver's
    // internal pinned-staging path. Phase F-0 #3 used the same pattern
    // (cudaMemcpyAsync on a single stream). The shim's compress() call
    // synchronises the stream internally before returning, so no
    // explicit fence needed between the H2D and the kernel.
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_in,
                input.as_ptr() as *const c_void,
                input.len(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                inner.stream,
            )
        },
        "cudaMemcpyAsync(hlif H2D)",
    )?;

    // HLIF compress on the manager's stream. The shim synchronises the
    // stream before returning the compressed size, so no extra fence
    // needed before the D2H below.
    let mut comp_bytes: usize = 0;
    let rc = unsafe {
        ferro_nvcomp_hlif_compress(
            inner.handle,
            inner.d_in as *const u8,
            input.len(),
            inner.d_out as *mut u8,
            &mut comp_bytes,
        )
    };
    check_shim(rc, "hlif compress")?;
    if comp_bytes > max_comp_bytes {
        return Err(Error::Compress(format!(
            "HLIF reported comp_bytes={comp_bytes} > pre-computed max={max_comp_bytes}"
        )));
    }

    // D2H — append the framed payload to `output` so callers can
    // accumulate (matches `Codec::compress` contract). Async on the
    // manager's stream + a single sync at the end keeps the bulk D2H
    // pipelined with the kernel's tail. The shim already synchronised
    // once to read comp_bytes; this second sync covers the user's bytes.
    let start = output.len();
    output.resize(start + comp_bytes, 0);
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                output[start..].as_mut_ptr() as *mut c_void,
                inner.d_out as *const c_void,
                comp_bytes,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                inner.stream,
            )
        },
        "cudaMemcpyAsync(hlif D2H)",
    )?;
    check_cuda(
        unsafe { cudaStreamSynchronize(inner.stream) },
        "cudaStreamSynchronize(hlif compress D2H)",
    )?;
    Ok(())
}

fn decompress_via_hlif(inner: &mut HlifInner, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
    if input.is_empty() {
        return Err(Error::Decompress(
            "HLIF decompress: empty input".to_string(),
        ));
    }
    // Identity sentinel for the empty-input frame written by compress.
    if input.len() == 1 && input[0] == 0x00 {
        return Ok(());
    }

    // The compressed payload uses the same `d_out` slot as the compress
    // path: it's the "things we move into device memory" buffer. The
    // decompressed output uses the separate `d_decomp` slot. We chose
    // not to alias `d_in` with the compressed input because Phase F-1.6
    // wiring is likely to issue interleaved compress + decompress on the
    // same backend and aliasing would force a sync between them.
    HlifInner::ensure_buf(&mut inner.d_out, &mut inner.d_out_cap, input.len())?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_out,
                input.as_ptr() as *const c_void,
                input.len(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                inner.stream,
            )
        },
        "cudaMemcpyAsync(hlif compressed H2D)",
    )?;

    // Parse the NVCOMP_NATIVE header on device to learn the decompressed
    // size. The shim synchronises the stream internally because HLIF has
    // to read the header bytes from device memory.
    let mut decomp_bytes: usize = 0;
    let rc = unsafe {
        ferro_nvcomp_hlif_get_decompressed_output_size(
            inner.handle,
            inner.d_out as *const u8,
            &mut decomp_bytes,
        )
    };
    check_shim(rc, "hlif get_decompressed_output_size")?;

    HlifInner::ensure_buf(
        &mut inner.d_decomp,
        &mut inner.d_decomp_cap,
        decomp_bytes.max(1),
    )?;

    let mut decomp_actual: usize = 0;
    let rc = unsafe {
        ferro_nvcomp_hlif_decompress(
            inner.handle,
            inner.d_out as *const u8,
            inner.d_decomp as *mut u8,
            &mut decomp_actual,
        )
    };
    check_shim(rc, "hlif decompress")?;
    if decomp_actual != decomp_bytes {
        return Err(Error::Decompress(format!(
            "HLIF reported decomp_actual={decomp_actual} != header decomp_bytes={decomp_bytes}"
        )));
    }

    // HLIF decompress + D2H are both queued on the manager's stream;
    // CUDA orders them implicitly. We sync exactly once at the end
    // after the D2H, which covers the kernel's completion *and* the
    // bytes being available to the host.
    let start = output.len();
    output.resize(start + decomp_bytes, 0);
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                output[start..].as_mut_ptr() as *mut c_void,
                inner.d_decomp as *const c_void,
                decomp_bytes,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                inner.stream,
            )
        },
        "cudaMemcpyAsync(hlif decomp D2H)",
    )?;
    check_cuda(
        unsafe { cudaStreamSynchronize(inner.stream) },
        "cudaStreamSynchronize(hlif decompress D2H)",
    )?;
    Ok(())
}
