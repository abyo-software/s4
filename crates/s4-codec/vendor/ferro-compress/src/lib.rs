//! Vendored subset of `ferro-compress` (originally
//! `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress`,
//! Apache-2.0 OR MIT, `publish=false`) — kept here so S4 has no path-dep on
//! the Ferro repo and stays cleanly separable for any future M&A scenario.
//!
//! ## What is included
//!
//! Only the GPU/nvCOMP path needed by S4's [`crate::nvcomp`](../../../src/nvcomp.rs)
//! codec adapter. Everything specific to FerroSearch (Tantivy bitmap-op kernel,
//! posting-list stats reduction, CPU codec features, in-tree benches) is
//! intentionally NOT vendored.
//!
//! Vendored:
//! - `algo` / `error` — algorithm enum + error type (1.5 KB total)
//! - `nvcomp_sys` — raw CUDA + nvCOMP FFI bindings (bindgen output, 640 LOC)
//! - `nvcomp` — batched nvCOMP codec (Snappy/LZ4/zstd/Bitcomp, 1300 LOC)
//! - `nvcomp_hlif` — HLIF self-describing frame codec (Bitcomp/zstd, 710 LOC)
//! - `bitcomp_device` — device-resident Bitcomp pipeline (1300 LOC)
//! - `slab_alloc` — `cudaMalloc` slab allocator used by `bitcomp_device`
//! - `cuda_kernels/nvcomp_hlif_shim.cpp` — C-ABI shim for nvCOMP HLIF C++ API
//!
//! NOT vendored (Ferro-search-specific, replace with S4 equivalents if needed):
//! - `cpu` — CPU codec (S4 has its own `cpu_zstd` module)
//! - `bitmap` + `cuda_kernels/bitmap_op.cu` — Tantivy posting-list bitmap kernel
//! - `stats_kernel` + `cuda_kernels/stats_op.cu` — column stats reduction
//! - `backend` — Backend / BackendKind / Tier dispatcher (S4 dispatches via
//!   `s4_codec::CodecKind` / `Codec` trait, not Tier)
//! - all bins / examples / tests
//!
//! ## Modifications from upstream
//!
//! - `lib.rs` (this file): rewritten to the trimmed module set above.
//!   `BitmapOpKernel` / `StatsOpKernel` / `Backend` re-exports removed.
//! - `nvcomp.rs` doc comment references to `BitmapOpKernel` are now stale doc
//!   text but still compile (they are only `[`crate::Foo`]` rustdoc links to
//!   types that no longer exist; warning-only, will be cleaned on integration).
//! - All other files copied verbatim — diff against
//!   `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress/src/`
//!   should be byte-identical.
//!
//! ## License
//!
//! See `vendor/ferro-compress/LICENSE` and `vendor/ferro-compress/NOTICE`.
//! `s4-codec` itself is `Proprietary` (S4 workspace license); the vendored
//! files retain their `Apache-2.0 OR MIT` dual-license. Any redistribution
//! of an S4 binary that links this code must carry both LICENSE and NOTICE.

#![cfg_attr(not(feature = "nvcomp"), forbid(unsafe_code))]
#![cfg_attr(feature = "nvcomp", deny(unsafe_op_in_unsafe_fn))]

mod algo;
mod error;

#[cfg(feature = "nvcomp")]
pub mod nvcomp_sys;

#[cfg(feature = "nvcomp")]
mod nvcomp;

#[cfg(feature = "nvcomp")]
mod nvcomp_hlif;

#[cfg(feature = "nvcomp")]
mod bitcomp_device;

#[cfg(feature = "nvcomp")]
mod slab_alloc;

pub use algo::{Algo, BitcompDataType, Tier};
pub use error::{Error, Result};

#[cfg(feature = "nvcomp")]
pub use nvcomp::NvcompCodec;

#[cfg(feature = "nvcomp")]
pub use nvcomp_hlif::{
    cuda_available, BitcompHlifBackend, ZstdHlifBackend, DEFAULT_HLIF_CHUNK_SIZE,
};

#[cfg(feature = "nvcomp")]
pub use bitcomp_device::BitcompDeviceCodec;

#[cfg(feature = "nvcomp")]
pub use slab_alloc::{SlabAllocator, SLAB_MAX_BUCKET_BYTES, SLAB_MIN_BUCKET_BYTES};

/// Compression / decompression interface (verbatim from upstream
/// `ferro_compress::Codec` — kept here so the vendored `NvcompCodec` /
/// `BitcompHlifBackend` / `BitcompDeviceCodec` impls compile unchanged).
///
/// S4's higher-level [`s4_codec::Codec`] trait (async, `bytes::Bytes`-based)
/// wraps this synchronous, `&[u8]`-based interface in
/// `s4-codec/src/nvcomp.rs`.
pub trait Codec: Send + Sync {
    fn algo(&self) -> Algo;

    fn compress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()>;

    fn decompress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()>;

    fn compress_batch(&self, inputs: &[&[u8]], outputs: &mut [Vec<u8>]) -> Result<()> {
        if inputs.len() != outputs.len() {
            return Err(Error::BatchLenMismatch {
                inputs: inputs.len(),
                outputs: outputs.len(),
            });
        }
        for (i, out) in inputs.iter().zip(outputs.iter_mut()) {
            self.compress(i, out)?;
        }
        Ok(())
    }

    fn decompress_batch(&self, inputs: &[&[u8]], outputs: &mut [Vec<u8>]) -> Result<()> {
        if inputs.len() != outputs.len() {
            return Err(Error::BatchLenMismatch {
                inputs: inputs.len(),
                outputs: outputs.len(),
            });
        }
        for (i, out) in inputs.iter().zip(outputs.iter_mut()) {
            self.decompress(i, out)?;
        }
        Ok(())
    }

    fn max_compressed_len(&self, uncompressed_len: usize) -> usize;
}
