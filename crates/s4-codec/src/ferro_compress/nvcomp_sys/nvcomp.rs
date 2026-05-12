//! Raw FFI for nvCOMP 5.x batched API — Snappy, LZ4, zstd.
//!
//! Each algorithm exposes the same shape:
//!   - CompressGetTempSizeSync (or async) — size the temp buffer
//!   - CompressGetMaxOutputChunkSize       — size the per-chunk output buffer
//!   - CompressAsync                       — kick off the work on a stream
//!   - GetDecompressSizeAsync              — read the original size from header
//!   - DecompressGetTempSizeAsync          — size temp buffer for decompress
//!   - DecompressAsync                     — kick off decompress on a stream
//!
//! All "device_*_ptrs" are pointers to GPU memory (cudaMalloc'd). All "stream"
//! arguments are `cudaStream_t`. nvcompStatus_t mirrors the C enum.

use std::ffi::{c_int, c_uint, c_void};

use super::cuda::cudaStream_t;

// ---------- Shared enums / types ----------

pub type nvcompStatus_t = c_uint;
pub const nvcompSuccess: nvcompStatus_t = 0;
pub const nvcompErrorInvalidValue: nvcompStatus_t = 10;
pub const nvcompErrorNotSupported: nvcompStatus_t = 11;
pub const nvcompErrorCannotDecompress: nvcompStatus_t = 12;
pub const nvcompErrorBadChecksum: nvcompStatus_t = 13;
pub const nvcompErrorCannotVerifyChecksums: nvcompStatus_t = 14;
pub const nvcompErrorOutputBufferTooSmall: nvcompStatus_t = 15;
pub const nvcompErrorWrongHeaderLength: nvcompStatus_t = 16;
pub const nvcompErrorAlignment: nvcompStatus_t = 17;
pub const nvcompErrorChunkSizeTooLarge: nvcompStatus_t = 18;
pub const nvcompErrorCannotCompress: nvcompStatus_t = 19;
pub const nvcompErrorWrongInputLength: nvcompStatus_t = 20;
pub const nvcompErrorBatchSizeTooLarge: nvcompStatus_t = 21;
pub const nvcompErrorCudaError: nvcompStatus_t = 1000;
pub const nvcompErrorInternal: nvcompStatus_t = 10000;

pub type nvcompType_t = c_uint;
pub const NVCOMP_TYPE_CHAR: nvcompType_t = 0;
pub const NVCOMP_TYPE_UCHAR: nvcompType_t = 1;
pub const NVCOMP_TYPE_SHORT: nvcompType_t = 2;
pub const NVCOMP_TYPE_USHORT: nvcompType_t = 3;
pub const NVCOMP_TYPE_INT: nvcompType_t = 4;
pub const NVCOMP_TYPE_UINT: nvcompType_t = 5;
pub const NVCOMP_TYPE_LONGLONG: nvcompType_t = 6;
pub const NVCOMP_TYPE_ULONGLONG: nvcompType_t = 7;
// FLOAT (32-bit), FLOAT16, DOUBLE, BFLOAT16 — values per nvCOMP 5.2.0
// `shared_types.h`. Bitcomp accepts the integer-and-float typed hints and
// uses them to drive its internal bit-packing / delta layout.
pub const NVCOMP_TYPE_FLOAT: nvcompType_t = 8;
pub const NVCOMP_TYPE_FLOAT16: nvcompType_t = 9;
pub const NVCOMP_TYPE_DOUBLE: nvcompType_t = 10;
pub const NVCOMP_TYPE_BFLOAT16: nvcompType_t = 11;
pub const NVCOMP_TYPE_BITS: nvcompType_t = 0xff;

pub type nvcompDecompressBackend_t = c_uint;
pub const NVCOMP_DECOMPRESS_BACKEND_DEFAULT: nvcompDecompressBackend_t = 0;
pub const NVCOMP_DECOMPRESS_BACKEND_HARDWARE: nvcompDecompressBackend_t = 1;
pub const NVCOMP_DECOMPRESS_BACKEND_CUDA: nvcompDecompressBackend_t = 2;

pub type nvcompBitshuffleMode_t = c_uint;
pub const NVCOMP_BITSHUFFLE_NONE: nvcompBitshuffleMode_t = 0;
pub const NVCOMP_BITSHUFFLE_INT8: nvcompBitshuffleMode_t = 1;
pub const NVCOMP_BITSHUFFLE_INT16: nvcompBitshuffleMode_t = 2;

// ---------- Opts structs (treated as opaque 64-byte buffers) ----------
//
// nvCOMP guarantees the structs are 64 bytes (with reserved padding). Treating
// them opaquely lets us avoid binding the exact field layout, which has shifted
// between minor releases. Default-initialise with zeros, except where a non-
// zero default field needs to be set explicitly (LZ4/zstd data_type stays at
// CHAR=0 anyway).

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedSnappyCompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedSnappyCompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedSnappyDecompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedSnappyDecompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedLZ4CompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedLZ4CompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedLZ4DecompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedLZ4DecompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedZstdCompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedZstdCompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedZstdDecompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedZstdDecompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

// ---------- Bitcomp ----------
//
// Bitcomp (NVIDIA proprietary GPU codec, distributed inside nvCOMP) is the
// strongest codec on typed numeric columns: Phase 0 measured 3.59× ratio +
// 419 GB/s compress + 366 GB/s decompress on `postings.bin` with the UINT
// data-type hint, beating Snappy / LZ4 / zstd on every axis. With the CHAR
// hint it degenerates to ~1.2× ratio, so the data-type hint matters and is
// surfaced through `Algo::Bitcomp { data_type }` in the safe wrapper.
//
// `bitcomp.h` exposes a format selector (`nvcompBitcompFormat_t`) for the
// internal layout. `Default = 0` matches the header's BITCOMP_DEFAULT_OPTS
// macro and is what every public sample uses.

pub type nvcompBitcompFormat_t = c_uint;
pub const NVCOMP_BITCOMP_FORMAT_DEFAULT: nvcompBitcompFormat_t = 0;
pub const NVCOMP_BITCOMP_FORMAT_SPARSE: nvcompBitcompFormat_t = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedBitcompFormatOpts {
    pub algorithm_type: c_int,
    pub data_type: nvcompType_t,
    // The struct in nvCOMP 5.x carries reserved padding so its total size is
    // 64 bytes for ABI stability. We model the trailing bytes opaquely.
    pub reserved: [u8; 56],
}
impl Default for nvcompBatchedBitcompFormatOpts {
    fn default() -> Self {
        Self {
            algorithm_type: NVCOMP_BITCOMP_FORMAT_DEFAULT as c_int,
            data_type: NVCOMP_TYPE_UCHAR,
            reserved: [0; 56],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvcompBatchedBitcompDecompressOpts_t {
    pub raw: [u8; 64],
}
impl Default for nvcompBatchedBitcompDecompressOpts_t {
    fn default() -> Self {
        Self { raw: [0; 64] }
    }
}

unsafe extern "C" {
    pub fn nvcompBatchedBitcompCompressGetTempSizeSync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        format_opts: nvcompBatchedBitcompFormatOpts,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedBitcompCompressGetMaxOutputChunkSize(
        max_uncompressed_chunk_bytes: usize,
        format_opts: nvcompBatchedBitcompFormatOpts,
        max_compressed_chunk_bytes: *mut usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedBitcompCompressAsync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        max_uncompressed_chunk_bytes: usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_compressed_chunk_ptrs: *const *mut c_void,
        device_compressed_chunk_bytes: *mut usize,
        format_opts: nvcompBatchedBitcompFormatOpts,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedBitcompDecompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        decompress_opts: nvcompBatchedBitcompDecompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedBitcompGetDecompressSizeAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedBitcompDecompressAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_buffer_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_uncompressed_chunk_ptrs: *const *mut c_void,
        decompress_opts: nvcompBatchedBitcompDecompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;
}

// ---------- Snappy ----------

unsafe extern "C" {
    pub fn nvcompBatchedSnappyCompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedSnappyCompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyCompressGetTempSizeSync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedSnappyCompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyCompressGetMaxOutputChunkSize(
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedSnappyCompressOpts_t,
        max_compressed_chunk_bytes: *mut usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyCompressAsync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        max_uncompressed_chunk_bytes: usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_compressed_chunk_ptrs: *const *mut c_void,
        device_compressed_chunk_bytes: *mut usize,
        compress_opts: nvcompBatchedSnappyCompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyDecompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        decompress_opts: nvcompBatchedSnappyDecompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyGetDecompressSizeAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedSnappyDecompressAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_buffer_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_uncompressed_chunk_ptrs: *const *mut c_void,
        decompress_opts: nvcompBatchedSnappyDecompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;
}

// ---------- LZ4 ----------

unsafe extern "C" {
    pub fn nvcompBatchedLZ4CompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedLZ4CompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4CompressGetTempSizeSync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedLZ4CompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4CompressGetMaxOutputChunkSize(
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedLZ4CompressOpts_t,
        max_compressed_chunk_bytes: *mut usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4CompressAsync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        max_uncompressed_chunk_bytes: usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_compressed_chunk_ptrs: *const *mut c_void,
        device_compressed_chunk_bytes: *mut usize,
        compress_opts: nvcompBatchedLZ4CompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4DecompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        decompress_opts: nvcompBatchedLZ4DecompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4GetDecompressSizeAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedLZ4DecompressAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_buffer_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_uncompressed_chunk_ptrs: *const *mut c_void,
        decompress_opts: nvcompBatchedLZ4DecompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;
}

// ---------- zstd ----------

unsafe extern "C" {
    pub fn nvcompBatchedZstdCompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedZstdCompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdCompressGetTempSizeSync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedZstdCompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdCompressGetMaxOutputChunkSize(
        max_uncompressed_chunk_bytes: usize,
        compress_opts: nvcompBatchedZstdCompressOpts_t,
        max_compressed_chunk_bytes: *mut usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdCompressAsync(
        device_uncompressed_chunk_ptrs: *const *const c_void,
        device_uncompressed_chunk_bytes: *const usize,
        max_uncompressed_chunk_bytes: usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_compressed_chunk_ptrs: *const *mut c_void,
        device_compressed_chunk_bytes: *mut usize,
        compress_opts: nvcompBatchedZstdCompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdDecompressGetTempSizeAsync(
        num_chunks: usize,
        max_uncompressed_chunk_bytes: usize,
        decompress_opts: nvcompBatchedZstdDecompressOpts_t,
        temp_bytes: *mut usize,
        max_total_uncompressed_bytes: usize,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdGetDecompressSizeAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;

    pub fn nvcompBatchedZstdDecompressAsync(
        device_compressed_chunk_ptrs: *const *const c_void,
        device_compressed_chunk_bytes: *const usize,
        device_uncompressed_buffer_bytes: *const usize,
        device_uncompressed_chunk_bytes: *mut usize,
        num_chunks: usize,
        device_temp_ptr: *mut c_void,
        temp_bytes: usize,
        device_uncompressed_chunk_ptrs: *const *mut c_void,
        decompress_opts: nvcompBatchedZstdDecompressOpts_t,
        device_statuses: *mut nvcompStatus_t,
        stream: cudaStream_t,
    ) -> nvcompStatus_t;
}

// nvCOMP returns a single status from each batched API. This helper turns a
// non-zero status into a printable string via the inline error names above.
pub fn status_str(status: nvcompStatus_t) -> &'static str {
    match status {
        x if x == nvcompSuccess => "Success",
        x if x == nvcompErrorInvalidValue => "InvalidValue",
        x if x == nvcompErrorNotSupported => "NotSupported",
        x if x == nvcompErrorCannotDecompress => "CannotDecompress",
        x if x == nvcompErrorBadChecksum => "BadChecksum",
        x if x == nvcompErrorCannotVerifyChecksums => "CannotVerifyChecksums",
        x if x == nvcompErrorOutputBufferTooSmall => "OutputBufferTooSmall",
        x if x == nvcompErrorWrongHeaderLength => "WrongHeaderLength",
        x if x == nvcompErrorAlignment => "Alignment",
        x if x == nvcompErrorChunkSizeTooLarge => "ChunkSizeTooLarge",
        x if x == nvcompErrorCannotCompress => "CannotCompress",
        x if x == nvcompErrorWrongInputLength => "WrongInputLength",
        x if x == nvcompErrorBatchSizeTooLarge => "BatchSizeTooLarge",
        x if x == nvcompErrorCudaError => "CudaError",
        x if x == nvcompErrorInternal => "Internal",
        _ => "Unknown",
    }
}

// Suppress unused-warning when only some of the algos are linked.
#[allow(dead_code)]
fn _unused() {
    let _: c_int = 0;
}
