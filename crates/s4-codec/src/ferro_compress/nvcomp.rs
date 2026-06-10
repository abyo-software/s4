//! Safe wrapper around the nvCOMP batched API.
//!
//! [`NvcompCodec`] implements [`Codec`] for Snappy, LZ4, zstd, and Bitcomp on
//! the GPU. Bitcomp is selected via [`Algo::Bitcomp { data_type }`] and is
//! the typed-numeric-column codec — Phase 0 measured 3.59× ratio + 366
//! GB/s decomp on `postings.bin` with the `Uint32` hint, the strongest of
//! every tested algo on numeric data. The hint matters: `Char` degenerates
//! to ~1.2× on the same data.
//!
//! It owns:
//!
//! - one CUDA stream for the lifetime of the codec
//! - a pool of persistent device buffers (input / compressed / temp /
//!   per-chunk metadata) that grow but never shrink
//! - a pool of pinned host buffers (`cudaHostAlloc`'d) for staging H2D and
//!   D2H so PCIe transfers run at the ~25 GB/s ceiling instead of the
//!   ~12 GB/s pageable-memory path
//!
//! Phase 1 used fresh `cudaMalloc` / `cudaFree` on every call; that pinned
//! the PCIe-included end-to-end throughput at ~4-5 GB/s (20-50× below the
//! GPU-internal numbers from Phase 0). Phase 1.5 keeps the device buffers
//! around between calls and stages H2D copies through pinned host memory,
//! which closes most of that gap.
//!
//! Wire format produced by [`NvcompCodec::compress`]:
//! ```text
//! [magic 4B "FCG1"][algo u8][reserved 3B]
//! [orig_size u64 LE][chunk_size u32 LE][num_chunks u32 LE]
//! [per-chunk compressed size u32 LE × num_chunks]
//! [concatenated compressed chunks, in order]
//! ```
//! The total overhead is `24 + 4 * num_chunks` bytes. This is *not* the raw
//! nvCOMP wire format — it's our own framing that captures everything needed
//! for the inverse path. Files written by `NvcompCodec` are only readable by
//! `NvcompCodec` (or by a CPU re-implementation of the framing on top of the
//! per-chunk Snappy/LZ4/zstd block format).

use std::ffi::{CStr, c_void};
use std::ptr::null_mut;
use std::sync::Mutex;

use super::nvcomp_sys::cuda::*;
use super::nvcomp_sys::nvcomp::*;
use super::{Algo, BitcompDataType, Codec, Error, Result};

const FRAME_MAGIC: [u8; 4] = *b"FCG1";
const HEADER_FIXED_BYTES: usize = 4 + 1 + 3 + 8 + 4 + 4;
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

pub struct NvcompCodec {
    algo: Algo,
    chunk_size: usize,
    stream: cudaStream_t,
    inner: Mutex<NvcompCodecInner>,
}

// SAFETY: cudaStream_t is opaque; nvCOMP and the CUDA runtime are safe to
// invoke concurrently from multiple threads as long as each call gets a
// stream. The inner state (device buffers + pinned host staging) is gated
// by a Mutex, so a single codec instance serializes its calls — multiple
// instances run independently.
unsafe impl Send for NvcompCodec {}
unsafe impl Sync for NvcompCodec {}

impl std::fmt::Debug for NvcompCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvcompCodec")
            .field("algo", &self.algo)
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

impl NvcompCodec {
    /// Create a codec for `algo` with the default chunk size (64 KB).
    pub fn new(algo: Algo) -> Result<Self> {
        Self::with_chunk_size(algo, DEFAULT_CHUNK_SIZE)
    }

    pub fn with_chunk_size(algo: Algo, chunk_size: usize) -> Result<Self> {
        match algo {
            Algo::Snappy | Algo::Lz4 | Algo::Zstd | Algo::GDeflate | Algo::Bitcomp { .. } => {}
            other => return Err(Error::UnsupportedAlgo(other)),
        }
        if chunk_size == 0 || chunk_size > (1 << 24) {
            return Err(Error::Compress(format!(
                "nvcomp chunk_size must be in (0, 16 MiB]; got {chunk_size}"
            )));
        }
        let mut stream: cudaStream_t = null_mut();
        let rc = unsafe { cudaStreamCreate(&mut stream) };
        check_cuda(rc, "cudaStreamCreate")?;
        Ok(Self {
            algo,
            chunk_size,
            stream,
            inner: Mutex::new(NvcompCodecInner::default()),
        })
    }

    /// Detect whether a usable CUDA device is present. Cheap call; can be
    /// used by [`Backend::Auto`] to fall back to CPU when no GPU exists.
    pub fn is_available() -> bool {
        let mut count = 0i32;
        let rc = unsafe { cudaGetDeviceCount(&mut count) };
        rc == CUDA_SUCCESS && count > 0
    }

    /// Borrow the codec's CUDA stream for **Hybrid backend bridge** use —
    /// e.g. so a [`crate::BitmapOpKernel`] can run on the same stream as
    /// `Bitcomp decompress` and the chunk-batched pipeline (decompress →
    /// bit-op → readback) stays inside one CUDA stream with no host
    /// synchronisation in between.
    ///
    /// The returned handle is borrowed: do **not** destroy it. Phase 2 C
    /// design doc § 3.1 / § 4.2 motivate the bridge.
    pub fn cuda_stream(&self) -> cudaStream_t {
        self.stream
    }
}

impl Drop for NvcompCodec {
    fn drop(&mut self) {
        if !self.stream.is_null() {
            unsafe { cudaStreamDestroy(self.stream) };
        }
    }
}

impl Codec for NvcompCodec {
    fn algo(&self) -> Algo {
        self.algo
    }

    fn compress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self.inner.lock().expect("nvcomp codec inner poisoned");
        compress_chunked(
            self.algo,
            self.chunk_size,
            self.stream,
            &mut inner,
            input,
            output,
        )
    }

    fn decompress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        let mut inner = self.inner.lock().expect("nvcomp codec inner poisoned");
        decompress_chunked(self.stream, &mut inner, input, output)
    }

    fn max_compressed_len(&self, uncompressed_len: usize) -> usize {
        let num_chunks = uncompressed_len.div_ceil(self.chunk_size).max(1);
        let max_per_chunk = match self.algo {
            Algo::Snappy => 32 + self.chunk_size + self.chunk_size / 6,
            Algo::Lz4 => self.chunk_size + self.chunk_size / 255 + 16,
            Algo::Zstd => self.chunk_size + self.chunk_size / 200 + 64,
            // GDeflate (DEFLATE-family GPU codec). DEFLATE worst-case is
            // input-size + ~5 bytes per 16 KiB block (zlib RFC 1951 §3.2.4).
            // We give it the same generous margin as zstd.
            Algo::GDeflate => self.chunk_size + self.chunk_size / 200 + 64,
            // Bitcomp is highly data-dependent — Phase 0 saw 2.79× to
            // 3.59× on numeric columns. Worst-case the codec falls back
            // to pass-through with a small frame header (~16 B/chunk).
            Algo::Bitcomp { .. } => self.chunk_size + self.chunk_size / 64 + 64,
            _ => self.chunk_size,
        };
        HEADER_FIXED_BYTES + 4 * num_chunks + max_per_chunk * num_chunks
    }
}

// ---------- Persistent buffer pool ----------

#[derive(Default)]
struct NvcompCodecInner {
    // Device buffers — capacities grow monotonically.
    d_uncomp: *mut c_void,
    d_uncomp_cap: usize,
    d_comp: *mut c_void,
    d_comp_cap: usize,
    d_temp: *mut c_void,
    d_temp_cap: usize,

    // Per-chunk metadata arrays. `chunks_cap` is the max chunk count we've
    // sized these arrays for; metadata for fewer chunks is a non-issue.
    d_uncomp_ptrs: *mut c_void,
    d_uncomp_sizes: *mut c_void,
    d_comp_ptrs: *mut c_void,
    d_comp_sizes: *mut c_void,
    d_uncomp_buf_sizes: *mut c_void,
    d_uncomp_actual_sizes: *mut c_void,
    d_statuses: *mut c_void,
    chunks_cap: usize,

    // Pinned host staging buffers. Pinned memory makes H2D / D2H run at the
    // PCIe ceiling rather than the pageable-memory ~12 GB/s path.
    h_pinned_input: *mut c_void,
    h_pinned_input_cap: usize,
    h_pinned_output: *mut c_void,
    h_pinned_output_cap: usize,
    h_pinned_meta: *mut c_void,
    h_pinned_meta_cap: usize,

    // Reusable host vectors for building per-chunk pointer / size tables.
    h_uncomp_ptrs: Vec<*const c_void>,
    h_uncomp_sizes: Vec<usize>,
    h_comp_ptrs: Vec<*mut c_void>,
    h_comp_sizes: Vec<usize>,
    h_uncomp_buf_sizes: Vec<usize>,
    h_statuses: Vec<nvcompStatus_t>,
}

impl Drop for NvcompCodecInner {
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
                &mut self.d_uncomp_buf_sizes,
                &mut self.d_uncomp_actual_sizes,
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

impl NvcompCodecInner {
    /// Resize a single device buffer to at least `needed`. The buffer is
    /// freed and re-allocated rather than grown in place — `cudaRealloc`
    /// doesn't exist, and copy-then-free is rarely worth it for transient
    /// scratch space.
    fn ensure_d_buf(&mut self, kind: BufKind, needed: usize) -> Result<()> {
        if needed == 0 {
            return Ok(());
        }
        let (slot, cap) = match kind {
            BufKind::Uncomp => (&mut self.d_uncomp, &mut self.d_uncomp_cap),
            BufKind::Comp => (&mut self.d_comp, &mut self.d_comp_cap),
            BufKind::Temp => (&mut self.d_temp, &mut self.d_temp_cap),
        };
        if *cap >= needed {
            return Ok(());
        }
        if !slot.is_null() {
            unsafe { cudaFree(*slot) };
            *slot = null_mut();
            *cap = 0;
        }
        // Round up to the next 1 MB to amortize future growth.
        let alloc_size = needed.div_ceil(1 << 20).max(1) << 20;
        check_cuda(unsafe { cudaMalloc(slot, alloc_size) }, "cudaMalloc(buf)")?;
        *cap = alloc_size;
        Ok(())
    }

    /// Resize the per-chunk metadata arrays to fit at least `chunks` entries.
    fn ensure_metadata(&mut self, chunks: usize) -> Result<()> {
        if self.chunks_cap >= chunks {
            return Ok(());
        }
        // Free old.
        unsafe {
            for p in [
                &mut self.d_uncomp_ptrs,
                &mut self.d_uncomp_sizes,
                &mut self.d_comp_ptrs,
                &mut self.d_comp_sizes,
                &mut self.d_uncomp_buf_sizes,
                &mut self.d_uncomp_actual_sizes,
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
            "cudaMalloc(uncomp_ptrs)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_uncomp_sizes, size_bytes) },
            "cudaMalloc(uncomp_sizes)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_comp_ptrs, ptr_bytes) },
            "cudaMalloc(comp_ptrs)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_comp_sizes, size_bytes) },
            "cudaMalloc(comp_sizes)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_uncomp_buf_sizes, size_bytes) },
            "cudaMalloc(uncomp_buf_sizes)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_uncomp_actual_sizes, size_bytes) },
            "cudaMalloc(uncomp_actual_sizes)",
        )?;
        check_cuda(
            unsafe { cudaMalloc(&mut self.d_statuses, status_bytes) },
            "cudaMalloc(statuses)",
        )?;
        self.chunks_cap = target;
        // Resize host vectors so they can be filled in place.
        self.h_uncomp_ptrs.resize(target, std::ptr::null());
        self.h_uncomp_sizes.resize(target, 0);
        self.h_comp_ptrs.resize(target, std::ptr::null_mut());
        self.h_comp_sizes.resize(target, 0);
        self.h_uncomp_buf_sizes.resize(target, 0);
        self.h_statuses.resize(target, 0);
        Ok(())
    }

    /// Resize a pinned host buffer to at least `needed` bytes.
    fn ensure_pinned(&mut self, kind: PinnedKind, needed: usize) -> Result<()> {
        if needed == 0 {
            return Ok(());
        }
        let (slot, cap) = match kind {
            PinnedKind::Input => (&mut self.h_pinned_input, &mut self.h_pinned_input_cap),
            PinnedKind::Output => (&mut self.h_pinned_output, &mut self.h_pinned_output_cap),
            PinnedKind::Meta => (&mut self.h_pinned_meta, &mut self.h_pinned_meta_cap),
        };
        if *cap >= needed {
            return Ok(());
        }
        if !slot.is_null() {
            unsafe { cudaFreeHost(*slot) };
            *slot = null_mut();
            *cap = 0;
        }
        let alloc_size = needed.div_ceil(1 << 20).max(1) << 20;
        check_cuda(
            unsafe { cudaHostAlloc(slot, alloc_size, cudaHostAllocDefault) },
            "cudaHostAlloc",
        )?;
        *cap = alloc_size;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum BufKind {
    Uncomp,
    Comp,
    Temp,
}

#[derive(Clone, Copy)]
enum PinnedKind {
    Input,
    Output,
    Meta,
}

// ---------- Error helpers ----------

pub(crate) fn check_cuda(rc: cudaError_t, what: &'static str) -> Result<()> {
    if rc == CUDA_SUCCESS {
        return Ok(());
    }
    let msg = unsafe {
        let s = cudaGetErrorString(rc);
        if s.is_null() {
            "unknown".to_string()
        } else {
            CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    };
    Err(Error::Compress(format!(
        "CUDA error in {what}: code={rc} ({msg})"
    )))
}

pub(crate) fn check_nvcomp(status: nvcompStatus_t, what: &'static str) -> Result<()> {
    if status == nvcompSuccess {
        Ok(())
    } else {
        Err(Error::Compress(format!(
            "nvCOMP error in {what}: code={status} ({})",
            status_str(status)
        )))
    }
}

// ---------- Algo dispatch helpers ----------

pub(crate) fn compress_get_max_output_chunk_size(algo: Algo, max_chunk: usize) -> Result<usize> {
    let mut out = 0usize;
    let status = unsafe {
        match algo {
            Algo::Snappy => nvcompBatchedSnappyCompressGetMaxOutputChunkSize(
                max_chunk,
                Default::default(),
                &mut out,
            ),
            Algo::Lz4 => nvcompBatchedLZ4CompressGetMaxOutputChunkSize(
                max_chunk,
                Default::default(),
                &mut out,
            ),
            Algo::Zstd => nvcompBatchedZstdCompressGetMaxOutputChunkSize(
                max_chunk,
                Default::default(),
                &mut out,
            ),
            Algo::GDeflate => nvcompBatchedGdeflateCompressGetMaxOutputChunkSize(
                max_chunk,
                Default::default(),
                &mut out,
            ),
            Algo::Bitcomp { data_type } => nvcompBatchedBitcompCompressGetMaxOutputChunkSize(
                max_chunk,
                bitcomp_format_opts(data_type),
                &mut out,
            ),
            _ => return Err(Error::UnsupportedAlgo(algo)),
        }
    };
    check_nvcomp(status, "GetMaxOutputChunkSize")?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compress_get_temp_size(
    algo: Algo,
    d_uncomp_ptrs: *const *const c_void,
    d_uncomp_sizes: *const usize,
    num_chunks: usize,
    max_chunk: usize,
    total_uncomp: usize,
    stream: cudaStream_t,
) -> Result<usize> {
    let mut out = 0usize;
    let status = unsafe {
        match algo {
            // Phase 1.5 critical fix: the *Sync* temp-size variants
            // "may synchronize the stream internally" (per nvCOMP header)
            // and we measured a multi-millisecond stall per call attributable
            // to that. The Async variants are pure pre-flight size queries
            // (no device work, no stream wait) so swap to them for the
            // first-class codecs. Bitcomp keeps Sync because its temp size
            // depends on the typed-input layout.
            Algo::Snappy => nvcompBatchedSnappyCompressGetTempSizeAsync(
                num_chunks,
                max_chunk,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Lz4 => nvcompBatchedLZ4CompressGetTempSizeAsync(
                num_chunks,
                max_chunk,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Zstd => nvcompBatchedZstdCompressGetTempSizeAsync(
                num_chunks,
                max_chunk,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::GDeflate => nvcompBatchedGdeflateCompressGetTempSizeAsync(
                num_chunks,
                max_chunk,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Bitcomp { data_type } => nvcompBatchedBitcompCompressGetTempSizeSync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                num_chunks,
                max_chunk,
                bitcomp_format_opts(data_type),
                &mut out,
                total_uncomp,
                stream,
            ),
            _ => return Err(Error::UnsupportedAlgo(algo)),
        }
    };
    check_nvcomp(status, "CompressGetTempSize")?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_compress(
    algo: Algo,
    d_uncomp_ptrs: *const *const c_void,
    d_uncomp_sizes: *const usize,
    max_chunk: usize,
    num_chunks: usize,
    d_temp: *mut c_void,
    temp_bytes: usize,
    d_comp_ptrs: *const *mut c_void,
    d_comp_sizes: *mut usize,
    d_statuses: *mut nvcompStatus_t,
    stream: cudaStream_t,
) -> Result<()> {
    let status = unsafe {
        match algo {
            Algo::Snappy => nvcompBatchedSnappyCompressAsync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                max_chunk,
                num_chunks,
                d_temp,
                temp_bytes,
                d_comp_ptrs,
                d_comp_sizes,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Lz4 => nvcompBatchedLZ4CompressAsync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                max_chunk,
                num_chunks,
                d_temp,
                temp_bytes,
                d_comp_ptrs,
                d_comp_sizes,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Zstd => nvcompBatchedZstdCompressAsync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                max_chunk,
                num_chunks,
                d_temp,
                temp_bytes,
                d_comp_ptrs,
                d_comp_sizes,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::GDeflate => nvcompBatchedGdeflateCompressAsync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                max_chunk,
                num_chunks,
                d_temp,
                temp_bytes,
                d_comp_ptrs,
                d_comp_sizes,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Bitcomp { data_type } => nvcompBatchedBitcompCompressAsync(
                d_uncomp_ptrs,
                d_uncomp_sizes,
                max_chunk,
                num_chunks,
                d_temp,
                temp_bytes,
                d_comp_ptrs,
                d_comp_sizes,
                bitcomp_format_opts(data_type),
                d_statuses,
                stream,
            ),
            _ => return Err(Error::UnsupportedAlgo(algo)),
        }
    };
    check_nvcomp(status, "CompressAsync")
}

fn decompress_get_temp_size(
    algo: Algo,
    num_chunks: usize,
    chunk_size: usize,
    total_uncomp: usize,
) -> Result<usize> {
    let mut out = 0usize;
    let status = unsafe {
        match algo {
            Algo::Snappy => nvcompBatchedSnappyDecompressGetTempSizeAsync(
                num_chunks,
                chunk_size,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Lz4 => nvcompBatchedLZ4DecompressGetTempSizeAsync(
                num_chunks,
                chunk_size,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Zstd => nvcompBatchedZstdDecompressGetTempSizeAsync(
                num_chunks,
                chunk_size,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::GDeflate => nvcompBatchedGdeflateDecompressGetTempSizeAsync(
                num_chunks,
                chunk_size,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            Algo::Bitcomp { .. } => nvcompBatchedBitcompDecompressGetTempSizeAsync(
                num_chunks,
                chunk_size,
                Default::default(),
                &mut out,
                total_uncomp,
            ),
            _ => return Err(Error::UnsupportedAlgo(algo)),
        }
    };
    check_nvcomp(status, "DecompressGetTempSize")?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_decompress(
    algo: Algo,
    d_comp_ptrs: *const *const c_void,
    d_comp_sizes: *const usize,
    d_uncomp_buf_sizes: *const usize,
    d_uncomp_actual_sizes: *mut usize,
    num_chunks: usize,
    d_temp: *mut c_void,
    temp_bytes: usize,
    d_uncomp_ptrs: *const *mut c_void,
    d_statuses: *mut nvcompStatus_t,
    stream: cudaStream_t,
) -> Result<()> {
    let status = unsafe {
        match algo {
            Algo::Snappy => nvcompBatchedSnappyDecompressAsync(
                d_comp_ptrs,
                d_comp_sizes,
                d_uncomp_buf_sizes,
                d_uncomp_actual_sizes,
                num_chunks,
                d_temp,
                temp_bytes,
                d_uncomp_ptrs,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Lz4 => nvcompBatchedLZ4DecompressAsync(
                d_comp_ptrs,
                d_comp_sizes,
                d_uncomp_buf_sizes,
                d_uncomp_actual_sizes,
                num_chunks,
                d_temp,
                temp_bytes,
                d_uncomp_ptrs,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Zstd => nvcompBatchedZstdDecompressAsync(
                d_comp_ptrs,
                d_comp_sizes,
                d_uncomp_buf_sizes,
                d_uncomp_actual_sizes,
                num_chunks,
                d_temp,
                temp_bytes,
                d_uncomp_ptrs,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::GDeflate => nvcompBatchedGdeflateDecompressAsync(
                d_comp_ptrs,
                d_comp_sizes,
                d_uncomp_buf_sizes,
                d_uncomp_actual_sizes,
                num_chunks,
                d_temp,
                temp_bytes,
                d_uncomp_ptrs,
                Default::default(),
                d_statuses,
                stream,
            ),
            Algo::Bitcomp { .. } => nvcompBatchedBitcompDecompressAsync(
                d_comp_ptrs,
                d_comp_sizes,
                d_uncomp_buf_sizes,
                d_uncomp_actual_sizes,
                num_chunks,
                d_temp,
                temp_bytes,
                d_uncomp_ptrs,
                Default::default(),
                d_statuses,
                stream,
            ),
            _ => return Err(Error::UnsupportedAlgo(algo)),
        }
    };
    check_nvcomp(status, "DecompressAsync")
}

// ---------- Compression ----------

fn compress_chunked(
    algo: Algo,
    chunk_size: usize,
    stream: cudaStream_t,
    inner: &mut NvcompCodecInner,
    input: &[u8],
    output: &mut Vec<u8>,
) -> Result<()> {
    if input.is_empty() {
        write_header(algo, chunk_size, 0, &[], output);
        return Ok(());
    }

    let num_chunks = input.len().div_ceil(chunk_size);
    let max_chunk_bytes = chunk_size;
    // Round per-chunk slot size up to 256 bytes so every chunk pointer
    // (`d_comp + i * max_comp_chunk_bytes`) honours nvCOMP's per-algo
    // alignment requirements without us having to query
    // `nvcompBatchedXxxCompressGetRequiredAlignments` for each algo.
    // Bitcomp on Uint32 is the strictest (16-byte output alignment); 256
    // is also the cudaMalloc base-pointer alignment, so this matches the
    // strongest alignment guarantee on the GPU.
    let raw_max = compress_get_max_output_chunk_size(algo, max_chunk_bytes)?;
    let max_comp_chunk_bytes = raw_max.div_ceil(256) * 256;
    let comp_buf_bytes = max_comp_chunk_bytes * num_chunks;

    // Grow persistent buffers to fit.
    inner.ensure_d_buf(BufKind::Uncomp, input.len())?;
    inner.ensure_d_buf(BufKind::Comp, comp_buf_bytes)?;
    inner.ensure_metadata(num_chunks)?;
    inner.ensure_pinned(PinnedKind::Input, input.len())?;
    let meta_bytes_each = num_chunks
        * std::mem::size_of::<usize>()
            .max(std::mem::size_of::<*const c_void>())
            .max(std::mem::size_of::<nvcompStatus_t>());
    let meta_total = meta_bytes_each * 4 + num_chunks * std::mem::size_of::<nvcompStatus_t>();
    inner.ensure_pinned(PinnedKind::Meta, meta_total)?;

    // The bulk input is read straight from the caller's pageable buffer.
    // We tried staging through pinned host memory (cudaHostAlloc'd), but at
    // 256 MB-1 GB inputs the extra host memcpy ate the PCIe-pinned win:
    // measured 4.32 GB/s decomp throughput vs 5.59 GB/s on direct pageable.
    // CUDA's driver does its own pinned-staging pipeline for pageable
    // sources and beats user-space staging for these sizes.

    // Build per-chunk pointer + size arrays in the resizable host vectors.
    for i in 0..num_chunks {
        let off = i * chunk_size;
        let end = (off + chunk_size).min(input.len());
        inner.h_uncomp_ptrs[i] = unsafe { (inner.d_uncomp as *const u8).add(off) as *const c_void };
        inner.h_uncomp_sizes[i] = end - off;
        inner.h_comp_ptrs[i] =
            unsafe { (inner.d_comp as *mut u8).add(i * max_comp_chunk_bytes) as *mut c_void };
    }

    let ptr_bytes = num_chunks * std::mem::size_of::<*const c_void>();
    let size_bytes = num_chunks * std::mem::size_of::<usize>();
    let status_bytes = num_chunks * std::mem::size_of::<nvcompStatus_t>();

    // Stage all metadata into the pinned meta buffer in one contiguous block.
    let meta_base = inner.h_pinned_meta as *mut u8;
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

    // H2D bulk input — direct from the caller's pageable buffer. CUDA
    // driver pipelines through its internal pinned staging.
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_uncomp,
                input.as_ptr() as *const c_void,
                input.len(),
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            )
        },
        "cudaMemcpyAsync(input H2D)",
    )?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_uncomp_ptrs,
                inner.h_pinned_meta,
                ptr_bytes,
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            )
        },
        "cudaMemcpyAsync(uncomp_ptrs H2D)",
    )?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_uncomp_sizes,
                (meta_base as *const u8).add(ptr_bytes) as *const c_void,
                size_bytes,
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            )
        },
        "cudaMemcpyAsync(uncomp_sizes H2D)",
    )?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_comp_ptrs,
                (meta_base as *const u8).add(ptr_bytes + size_bytes) as *const c_void,
                ptr_bytes,
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            )
        },
        "cudaMemcpyAsync(comp_ptrs H2D)",
    )?;

    let temp_bytes = compress_get_temp_size(
        algo,
        inner.d_uncomp_ptrs as *const *const c_void,
        inner.d_uncomp_sizes as *const usize,
        num_chunks,
        max_chunk_bytes,
        input.len(),
        stream,
    )?;
    inner.ensure_d_buf(BufKind::Temp, temp_bytes)?;

    dispatch_compress(
        algo,
        inner.d_uncomp_ptrs as *const *const c_void,
        inner.d_uncomp_sizes as *const usize,
        max_chunk_bytes,
        num_chunks,
        inner.d_temp,
        temp_bytes,
        inner.d_comp_ptrs as *const *mut c_void,
        inner.d_comp_sizes as *mut usize,
        inner.d_statuses as *mut nvcompStatus_t,
        stream,
    )?;

    // Phase 1.5 critical fix: queue **everything** the iteration needs to
    // pull back from the device — comp_sizes, statuses, and the bulk d_comp
    // buffer — onto the same stream and synchronise *once* at the end.
    // Previously we synced after the metadata D2H to read comp_sizes on
    // host, then queued the bulk D2H and synced again. That doubled the
    // PCIe round-trip cost of the iteration. The bulk-D2H size doesn't
    // depend on comp_sizes (it's `num_chunks * max_comp_chunk_bytes`,
    // computed from constants), so issuing it before the sync is sound.
    let bulk_d2h_bytes = num_chunks * max_comp_chunk_bytes;
    inner.ensure_pinned(PinnedKind::Output, bulk_d2h_bytes.max(1))?;
    let pinned_post = inner.h_pinned_meta as *mut u8;
    let pinned_out = inner.h_pinned_output as *mut u8;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                pinned_post as *mut c_void,
                inner.d_comp_sizes,
                size_bytes,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream,
            )
        },
        "cudaMemcpyAsync(comp_sizes D2H)",
    )?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                pinned_post.add(size_bytes) as *mut c_void,
                inner.d_statuses,
                status_bytes,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream,
            )
        },
        "cudaMemcpyAsync(statuses D2H)",
    )?;
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                pinned_out as *mut c_void,
                inner.d_comp as *const c_void,
                bulk_d2h_bytes,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream,
            )
        },
        "cudaMemcpyAsync(bulk d_comp D2H)",
    )?;
    // Single sync covers metadata + bulk; all D2H copies finish here.
    check_cuda(
        unsafe { cudaStreamSynchronize(stream) },
        "cudaStreamSynchronize(compress)",
    )?;

    // Read back metadata from pinned host buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            pinned_post as *const usize,
            inner.h_comp_sizes.as_mut_ptr(),
            num_chunks,
        );
        std::ptr::copy_nonoverlapping(
            pinned_post.add(size_bytes) as *const nvcompStatus_t,
            inner.h_statuses.as_mut_ptr(),
            num_chunks,
        );
    }
    for (i, st) in inner.h_statuses[..num_chunks].iter().enumerate() {
        if *st != nvcompSuccess {
            return Err(Error::Compress(format!(
                "nvcomp per-chunk failure at chunk {i}: status={st} ({})",
                status_str(*st)
            )));
        }
    }

    let total_comp: usize = inner.h_comp_sizes[..num_chunks].iter().sum();

    // Write framed output: header + per-chunk extract from the bulk buffer.
    write_header(
        algo,
        chunk_size,
        input.len(),
        &inner.h_comp_sizes[..num_chunks],
        output,
    );
    let start = output.len();
    output.resize(start + total_comp, 0);
    let dst_base = output[start..].as_mut_ptr();
    let mut cursor = 0usize;
    for i in 0..num_chunks {
        let sz = inner.h_comp_sizes[i];
        unsafe {
            std::ptr::copy_nonoverlapping(
                pinned_out.add(i * max_comp_chunk_bytes),
                dst_base.add(cursor),
                sz,
            );
        }
        cursor += sz;
    }
    Ok(())
}

pub(crate) fn write_header(
    algo: Algo,
    chunk_size: usize,
    orig_size: usize,
    chunk_sizes: &[usize],
    output: &mut Vec<u8>,
) {
    output.extend_from_slice(&FRAME_MAGIC);
    output.push(algo_tag(algo));
    // Reserved bytes 5..8 — Bitcomp embeds its data-type tag at offset 5
    // so frames are self-describing. Other algos leave the bytes zero.
    let reserved = match algo {
        Algo::Bitcomp { data_type } => [bitcomp_data_type_tag(data_type), 0, 0],
        _ => [0u8; 3],
    };
    output.extend_from_slice(&reserved);
    output.extend_from_slice(&(orig_size as u64).to_le_bytes());
    output.extend_from_slice(&(chunk_size as u32).to_le_bytes());
    output.extend_from_slice(&(chunk_sizes.len() as u32).to_le_bytes());
    for sz in chunk_sizes {
        output.extend_from_slice(&(*sz as u32).to_le_bytes());
    }
}

fn algo_tag(algo: Algo) -> u8 {
    match algo {
        Algo::Snappy => 1,
        Algo::Lz4 => 2,
        Algo::Zstd => 3,
        Algo::Bitcomp { .. } => 4,
        Algo::GDeflate => 5,
        _ => 0xff,
    }
}

/// Bitcomp embeds its `data_type` hint into the first reserved byte of
/// the FCG1 header so a self-described frame can be re-parsed without
/// the caller knowing which `BitcompDataType` was used to compress.
fn bitcomp_data_type_tag(dt: BitcompDataType) -> u8 {
    match dt {
        BitcompDataType::Char => 0,
        BitcompDataType::Uint8 => 1,
        BitcompDataType::Uint16 => 2,
        BitcompDataType::Uint32 => 3,
        BitcompDataType::Uint64 => 4,
        BitcompDataType::Int8 => 5,
        BitcompDataType::Int16 => 6,
        BitcompDataType::Int32 => 7,
        BitcompDataType::Int64 => 8,
        BitcompDataType::Float32 => 9,
        BitcompDataType::Float64 => 10,
        BitcompDataType::BFloat16 => 11,
    }
}

fn bitcomp_data_type_from_tag(tag: u8) -> Result<BitcompDataType> {
    match tag {
        0 => Ok(BitcompDataType::Char),
        1 => Ok(BitcompDataType::Uint8),
        2 => Ok(BitcompDataType::Uint16),
        3 => Ok(BitcompDataType::Uint32),
        4 => Ok(BitcompDataType::Uint64),
        5 => Ok(BitcompDataType::Int8),
        6 => Ok(BitcompDataType::Int16),
        7 => Ok(BitcompDataType::Int32),
        8 => Ok(BitcompDataType::Int64),
        9 => Ok(BitcompDataType::Float32),
        10 => Ok(BitcompDataType::Float64),
        11 => Ok(BitcompDataType::BFloat16),
        _ => Err(Error::Decompress(format!(
            "unknown bitcomp data-type tag: {tag}"
        ))),
    }
}

/// Map [`BitcompDataType`] → nvCOMP's `nvcompType_t`. The two enums are
/// 1:1 by intent, but the surface API doesn't expose the C value to
/// callers so they don't have to depend on the exact integer constants.
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

fn algo_from_header(tag: u8, reserved: [u8; 3]) -> Result<Algo> {
    match tag {
        1 => Ok(Algo::Snappy),
        2 => Ok(Algo::Lz4),
        3 => Ok(Algo::Zstd),
        4 => {
            let dt = bitcomp_data_type_from_tag(reserved[0])?;
            Ok(Algo::Bitcomp { data_type: dt })
        }
        5 => Ok(Algo::GDeflate),
        _ => Err(Error::Decompress(format!("unknown algo tag: {tag}"))),
    }
}

// ---------- Decompression ----------

fn decompress_chunked(
    stream: cudaStream_t,
    inner: &mut NvcompCodecInner,
    input: &[u8],
    output: &mut Vec<u8>,
) -> Result<()> {
    if input.len() < HEADER_FIXED_BYTES {
        return Err(Error::Decompress(format!(
            "nvcomp frame too short: {} bytes",
            input.len()
        )));
    }
    if input[0..4] != FRAME_MAGIC {
        return Err(Error::Decompress("missing FCG1 magic".into()));
    }
    let reserved = [input[5], input[6], input[7]];
    let algo = algo_from_header(input[4], reserved)?;
    let orig_size = u64::from_le_bytes(input[8..16].try_into().unwrap()) as usize;
    let chunk_size = u32::from_le_bytes(input[16..20].try_into().unwrap()) as usize;
    let num_chunks = u32::from_le_bytes(input[20..24].try_into().unwrap()) as usize;

    if num_chunks == 0 {
        return Ok(());
    }

    let sizes_off = HEADER_FIXED_BYTES;
    let payload_off = sizes_off + 4 * num_chunks;
    if input.len() < payload_off {
        return Err(Error::Decompress(format!(
            "nvcomp frame truncated: need {payload_off} bytes for sizes table, got {}",
            input.len()
        )));
    }
    inner.ensure_metadata(num_chunks)?;
    for i in 0..num_chunks {
        let s = sizes_off + 4 * i;
        inner.h_comp_sizes[i] = u32::from_le_bytes(input[s..s + 4].try_into().unwrap()) as usize;
    }
    let total_comp: usize = inner.h_comp_sizes[..num_chunks].iter().sum();
    if input.len() < payload_off + total_comp {
        return Err(Error::Decompress(format!(
            "nvcomp frame truncated: need {} bytes of payload, got {}",
            total_comp,
            input.len() - payload_off
        )));
    }
    let payload = &input[payload_off..payload_off + total_comp];

    // Per-chunk pointer alignment requirement on decompress:
    //   - Snappy / LZ4 / zstd: 1 byte (tight-packed payload is fine, the
    //     payload-on-disk format streams compressed chunks back-to-back
    //     and `d_comp + cumulative_offset` is acceptable to nvCOMP).
    //   - Bitcomp: 16-byte minimum on the input side. Tight packing fails
    //     when the previous chunks' compressed sizes don't sum to a
    //     16-byte boundary — observed as `cudaErrorMisalignedAddress` on
    //     the multi-chunk Uint32 path. For Bitcomp, copy the payload to
    //     a strided staging layout that mirrors what compress wrote.
    //
    // Tight is the fast path: it transfers `total_comp` bytes (the actual
    // compressed size) instead of the worst-case `num_chunks * stride`,
    // which avoids ~3× of PCIe traffic on Snappy at ratio 3.
    let needs_strided_layout = matches!(algo, Algo::Bitcomp { .. });
    let (comp_buf_bytes, stride) = if needs_strided_layout {
        let raw_max = compress_get_max_output_chunk_size(algo, chunk_size)?;
        let stride = raw_max.div_ceil(256) * 256;
        (stride * num_chunks, stride)
    } else {
        (total_comp, 0)
    };

    inner.ensure_d_buf(BufKind::Comp, comp_buf_bytes.max(1))?;
    inner.ensure_d_buf(BufKind::Uncomp, orig_size.max(1))?;
    let ptr_bytes = num_chunks * std::mem::size_of::<*const c_void>();
    let size_bytes = num_chunks * std::mem::size_of::<usize>();
    let status_bytes = num_chunks * std::mem::size_of::<nvcompStatus_t>();
    let meta_total = ptr_bytes * 2 + size_bytes * 3 + status_bytes;
    inner.ensure_pinned(PinnedKind::Meta, meta_total)?;

    if needs_strided_layout {
        // Bitcomp: re-pack the tight-on-disk payload into a strided device
        // layout so `d_comp + i * stride` honours the 16-byte alignment the
        // GPU kernel needs. We do this with a pinned host staging buffer
        // because the H2D has to deliver the same strided layout. (Bitcomp
        // is the only branch that pays for staging — the regular codecs
        // skip it entirely.)
        inner.ensure_pinned(PinnedKind::Input, comp_buf_bytes)?;
        let pinned_in = inner.h_pinned_input as *mut u8;
        let mut payload_cursor = 0usize;
        for i in 0..num_chunks {
            let sz = inner.h_comp_sizes[i];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr().add(payload_cursor),
                    pinned_in.add(i * stride),
                    sz,
                );
            }
            payload_cursor += sz;
        }
        for i in 0..num_chunks {
            inner.h_comp_ptrs[i] =
                unsafe { (inner.d_comp as *mut u8).add(i * stride) as *mut c_void };
            let off = i * chunk_size;
            inner.h_uncomp_ptrs[i] =
                unsafe { (inner.d_uncomp as *const u8).add(off) as *const c_void };
            let end = (off + chunk_size).min(orig_size);
            inner.h_uncomp_buf_sizes[i] = end - off;
        }
    } else {
        // Snappy / LZ4 / zstd: send the payload straight from the caller's
        // pageable buffer. CUDA driver's internal pinned-staging pipeline
        // beats user-space staging at 256 MB-1 GB sizes (Phase 1.5
        // measurements: 5.59 GB/s direct vs 4.32 GB/s via pinned staging
        // on json_logs decompress).
        let mut comp_cursor = 0usize;
        for i in 0..num_chunks {
            inner.h_comp_ptrs[i] =
                unsafe { (inner.d_comp as *mut u8).add(comp_cursor) as *mut c_void };
            comp_cursor += inner.h_comp_sizes[i];
            let off = i * chunk_size;
            inner.h_uncomp_ptrs[i] =
                unsafe { (inner.d_uncomp as *const u8).add(off) as *const c_void };
            let end = (off + chunk_size).min(orig_size);
            inner.h_uncomp_buf_sizes[i] = end - off;
        }
    }

    // Pack metadata into pinned meta buffer.
    let meta_base = inner.h_pinned_meta as *mut u8;
    let mut moff = 0usize;
    unsafe {
        std::ptr::copy_nonoverlapping(
            inner.h_comp_ptrs.as_ptr() as *const u8,
            meta_base.add(moff),
            ptr_bytes,
        );
        moff += ptr_bytes;
        std::ptr::copy_nonoverlapping(
            inner.h_comp_sizes.as_ptr() as *const u8,
            meta_base.add(moff),
            size_bytes,
        );
        moff += size_bytes;
        std::ptr::copy_nonoverlapping(
            inner.h_uncomp_ptrs.as_ptr() as *const u8,
            meta_base.add(moff),
            ptr_bytes,
        );
        moff += ptr_bytes;
        std::ptr::copy_nonoverlapping(
            inner.h_uncomp_buf_sizes.as_ptr() as *const u8,
            meta_base.add(moff),
            size_bytes,
        );
    }

    // H2D: payload + four metadata segments.
    //   - Bitcomp (strided): source is the pinned host staging buffer
    //     because the kernel needs the strided layout.
    //   - Snappy / LZ4 / zstd (tight): straight from the caller's
    //     pageable input slice — driver-pipelined PCIe is faster than
    //     user-space pinned staging at production sizes.
    let (h2d_src, h2d_bytes) = if needs_strided_layout {
        (inner.h_pinned_input as *const c_void, comp_buf_bytes)
    } else {
        (payload.as_ptr() as *const c_void, total_comp)
    };
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                inner.d_comp,
                h2d_src,
                h2d_bytes,
                cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            )
        },
        "cudaMemcpyAsync(payload H2D)",
    )?;
    let mut moff = 0usize;
    for (dst, n) in [
        (inner.d_comp_ptrs, ptr_bytes),
        (inner.d_comp_sizes, size_bytes),
        (inner.d_uncomp_ptrs, ptr_bytes),
        (inner.d_uncomp_buf_sizes, size_bytes),
    ] {
        check_cuda(
            unsafe {
                cudaMemcpyAsync(
                    dst,
                    meta_base.add(moff) as *const c_void,
                    n,
                    cudaMemcpyKind::cudaMemcpyHostToDevice,
                    stream,
                )
            },
            "cudaMemcpyAsync(meta H2D)",
        )?;
        moff += n;
    }

    let temp_bytes = decompress_get_temp_size(algo, num_chunks, chunk_size, orig_size)?;
    inner.ensure_d_buf(BufKind::Temp, temp_bytes)?;

    dispatch_decompress(
        algo,
        inner.d_comp_ptrs as *const *const c_void,
        inner.d_comp_sizes as *const usize,
        inner.d_uncomp_buf_sizes as *const usize,
        inner.d_uncomp_actual_sizes as *mut usize,
        num_chunks,
        inner.d_temp,
        temp_bytes,
        inner.d_uncomp_ptrs as *const *mut c_void,
        inner.d_statuses as *mut nvcompStatus_t,
        stream,
    )?;

    // D2H — write straight into the user's pageable Vec (resized in place).
    // Same reasoning as the H2D side: skip user-space pinned staging on
    // the bulk path because CUDA's internal pipeline is faster.
    let start = output.len();
    output.resize(start + orig_size, 0);
    check_cuda(
        unsafe {
            cudaMemcpyAsync(
                output[start..].as_mut_ptr() as *mut c_void,
                inner.d_uncomp,
                orig_size,
                cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream,
            )
        },
        "cudaMemcpyAsync(uncomp D2H)",
    )?;
    check_cuda(
        unsafe { cudaStreamSynchronize(stream) },
        "cudaStreamSynchronize(decompress)",
    )?;
    Ok(())
}
