//! Minimal CUDA runtime FFI — just what ferro-compress needs.

use std::ffi::{c_char, c_int, c_uint, c_void};

pub type cudaError_t = c_uint;
pub type cudaStream_t = *mut c_void;

pub const CUDA_SUCCESS: cudaError_t = 0;

// cudaMemcpyKind enum values (from cuda_runtime_api.h).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum cudaMemcpyKind {
    cudaMemcpyHostToHost = 0,
    cudaMemcpyHostToDevice = 1,
    cudaMemcpyDeviceToHost = 2,
    cudaMemcpyDeviceToDevice = 3,
    cudaMemcpyDefault = 4,
}

// cudaHostAllocFlags (from cuda_runtime_api.h).
pub const cudaHostAllocDefault: c_uint = 0x00;
pub const cudaHostAllocPortable: c_uint = 0x01;
pub const cudaHostAllocMapped: c_uint = 0x02;
pub const cudaHostAllocWriteCombined: c_uint = 0x04;

unsafe extern "C" {
    pub fn cudaMalloc(devPtr: *mut *mut c_void, size: usize) -> cudaError_t;
    pub fn cudaFree(devPtr: *mut c_void) -> cudaError_t;

    pub fn cudaHostAlloc(pHost: *mut *mut c_void, size: usize, flags: c_uint) -> cudaError_t;
    pub fn cudaFreeHost(ptr: *mut c_void) -> cudaError_t;

    pub fn cudaMemcpy(
        dst: *mut c_void,
        src: *const c_void,
        count: usize,
        kind: cudaMemcpyKind,
    ) -> cudaError_t;
    pub fn cudaMemcpyAsync(
        dst: *mut c_void,
        src: *const c_void,
        count: usize,
        kind: cudaMemcpyKind,
        stream: cudaStream_t,
    ) -> cudaError_t;
    pub fn cudaMemsetAsync(
        devPtr: *mut c_void,
        value: c_int,
        count: usize,
        stream: cudaStream_t,
    ) -> cudaError_t;

    pub fn cudaStreamCreate(pStream: *mut cudaStream_t) -> cudaError_t;
    pub fn cudaStreamDestroy(stream: cudaStream_t) -> cudaError_t;
    pub fn cudaStreamSynchronize(stream: cudaStream_t) -> cudaError_t;

    pub fn cudaDeviceSynchronize() -> cudaError_t;
    pub fn cudaGetDeviceCount(count: *mut c_int) -> cudaError_t;
    pub fn cudaSetDevice(device: c_int) -> cudaError_t;

    pub fn cudaGetLastError() -> cudaError_t;
    pub fn cudaGetErrorString(error: cudaError_t) -> *const c_char;
}

// ---------- CUDA driver API (libcuda.so) ----------
//
// Phase 2 C-2 needs the *driver* API so we can load a PTX module at runtime
// and launch kernels from it (`cuModuleLoadData` / `cuModuleGetFunction` /
// `cuLaunchKernel`). The runtime API has no equivalent for runtime-loaded
// PTX; runtime kernels live in compiled host code via `cudaLaunchKernel`,
// which would require an nvcc-compiled host stub object linked into the
// crate. PTX + driver-API loading keeps the build self-contained and lets
// us co-exist with the existing nvCOMP runtime-API setup on a single device.
//
// The driver and runtime APIs share a primary context, so cudaMalloc'd
// device pointers and cudaStream_t handles are valid for cuLaunchKernel
// without any explicit context migration. We retain the runtime's primary
// context once at `BitmapOpKernel` construction so the driver API has a
// current context for the calling thread.

pub type CUresult = c_uint;
pub const CUDA_SUCCESS_DRIVER: CUresult = 0;

pub type CUdevice = c_int;
pub type CUcontext = *mut c_void;
pub type CUmodule = *mut c_void;
pub type CUfunction = *mut c_void;
pub type CUstream = cudaStream_t;

unsafe extern "C" {
    pub fn cuInit(flags: c_uint) -> CUresult;
    pub fn cuDeviceGet(device: *mut CUdevice, ordinal: c_int) -> CUresult;
    pub fn cuDevicePrimaryCtxRetain(pctx: *mut CUcontext, dev: CUdevice) -> CUresult;
    pub fn cuDevicePrimaryCtxRelease(dev: CUdevice) -> CUresult;
    pub fn cuCtxSetCurrent(ctx: CUcontext) -> CUresult;
    pub fn cuCtxGetCurrent(pctx: *mut CUcontext) -> CUresult;

    pub fn cuModuleLoadData(module: *mut CUmodule, image: *const c_void) -> CUresult;
    pub fn cuModuleUnload(hmod: CUmodule) -> CUresult;
    pub fn cuModuleGetFunction(
        hfunc: *mut CUfunction,
        hmod: CUmodule,
        name: *const c_char,
    ) -> CUresult;

    pub fn cuLaunchKernel(
        f: CUfunction,
        gridDimX: c_uint,
        gridDimY: c_uint,
        gridDimZ: c_uint,
        blockDimX: c_uint,
        blockDimY: c_uint,
        blockDimZ: c_uint,
        sharedMemBytes: c_uint,
        hStream: CUstream,
        kernelParams: *mut *mut c_void,
        extra: *mut *mut c_void,
    ) -> CUresult;

    pub fn cuGetErrorName(error: CUresult, pStr: *mut *const c_char) -> CUresult;
    pub fn cuGetErrorString(error: CUresult, pStr: *mut *const c_char) -> CUresult;
}
