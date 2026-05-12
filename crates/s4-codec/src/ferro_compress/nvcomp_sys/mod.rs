//! Raw FFI bindings for the CUDA runtime + nvCOMP batched API.
//!
//! Only the functions ferro-compress actually calls are bound. Everything is
//! `extern "C"` and matches the exact prototypes in
//! `<NVCOMP_HOME>/include/nvcomp/{shared_types.h, snappy.h, lz4.h, zstd.h}`
//! and the CUDA runtime header (`<cuda_runtime.h>`).
//!
//! These bindings are gated behind the `nvcomp` cargo feature. Without it the
//! crate has no link-time dependency on CUDA or nvCOMP — `ferro-compress`
//! still builds and the CPU codecs still work.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
// FFI bindings mirror CUDA C names verbatim. clippy's enum-variant-names
// lint flags `cudaMemcpyKind` because every variant starts with `cudaMemcpy`,
// but renaming would diverge from the C ABI source-of-truth.
#![allow(clippy::enum_variant_names)]

pub mod cuda;
pub mod nvcomp;
