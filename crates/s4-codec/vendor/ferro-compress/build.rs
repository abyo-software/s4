// build.rs — emits cargo link metadata when the `nvcomp` feature is on, and
// compiles the Phase 2 C-2 bitmap-op CUDA kernel to PTX so the Rust runtime
// loader (`cuModuleLoadData`) can pick it up via `include_bytes!`.
//
// Without the `nvcomp` feature, ferro-compress builds on any machine (no CUDA
// needed). With the feature, we additionally:
//
// 1. Link `libnvcomp` from `NVCOMP_HOME` and the system CUDA runtime (cudart)
//    + driver (cuda) libs.
// 2. Try to compile `src/cuda_kernels/bitmap_op.cu` to PTX via `nvcc`. If
//    `nvcc` is not on PATH (or `NVCC` env var not set), we *still* finish the
//    build successfully but emit `cargo:warning` and write an empty PTX
//    placeholder file. Loading the kernel at runtime then fails with a clear
//    error message (`bitmap.rs`).
//
// The PTX gen is a one-shot Rust build helper, no external `cc` crate
// dependency needed.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=NVCOMP_HOME");
    println!("cargo:rerun-if-env-changed=CUDA_HOME");
    println!("cargo:rerun-if-env-changed=NVCC");
    println!("cargo:rerun-if-changed=src/cuda_kernels/nvcomp_hlif_shim.cpp");

    if std::env::var_os("CARGO_FEATURE_NVCOMP").is_none() {
        return;
    }

    // ---------- Link metadata for nvCOMP / CUDA ----------

    let nvcomp_home = std::env::var("NVCOMP_HOME").unwrap_or_else(|_| {
        // Fall back to the nvCOMP redist path that the in-tree phase0-bench
        // Dockerfile installs at; useful for the in-container build.
        "/opt/nvcomp".to_string()
    });
    let nvcomp_lib = format!("{nvcomp_home}/lib");
    println!("cargo:rustc-link-search=native={nvcomp_lib}");
    println!("cargo:rustc-link-lib=dylib=nvcomp");
    // ORIGIN-relative rpath so binaries find libnvcomp.so without LD_LIBRARY_PATH.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{nvcomp_lib}");

    let cuda_home = std::env::var("CUDA_HOME").unwrap_or_else(|_| "/usr/local/cuda".to_string());
    // CUDA layout differs between Linux distros and the NVIDIA dev-image base.
    // Try both lib64 and lib targets/.
    let cuda_lib_candidates = [
        format!("{cuda_home}/lib64"),
        format!("{cuda_home}/lib"),
        format!("{cuda_home}/targets/x86_64-linux/lib"),
        // Driver-stub directory for `libcuda.so` (CUDA driver API). On a
        // GPU host the real `libcuda.so` lives in `/usr/lib/x86_64-linux-gnu/`
        // but the toolkit ships a stub for build-time linking.
        format!("{cuda_home}/lib64/stubs"),
        format!("{cuda_home}/targets/x86_64-linux/lib/stubs"),
    ];
    for path in &cuda_lib_candidates {
        if std::path::Path::new(path).is_dir() {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
    println!("cargo:rustc-link-lib=dylib=cudart");
    // Phase 2 C-2 — driver API for cuModuleLoadData / cuLaunchKernel. The
    // CUDA runtime library is reused for cudaMalloc / streams; the driver
    // library is needed for module + kernel loading.
    println!("cargo:rustc-link-lib=dylib=cuda");

    // ---------- Compile .cu kernels to PTX ----------

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset under cargo"),
    );
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR unset under cargo"));

    let nvcc = std::env::var("NVCC").ok().or_else(|| {
        // Common locations: $CUDA_HOME/bin/nvcc, /usr/local/cuda/bin/nvcc, $PATH.
        let candidate = format!("{cuda_home}/bin/nvcc");
        if std::path::Path::new(&candidate).exists() {
            Some(candidate)
        } else if Command::new("nvcc").arg("--version").output().is_ok() {
            Some("nvcc".to_string())
        } else {
            None
        }
    });

    // Phase 2 C-2 Bool-AND kernel + Phase 2 E-1 stats reduction kernel are
    // intentionally NOT vendored to S4 (they are FerroSearch-specific:
    // Tantivy posting-list bit operations and column statistics reduction).
    // Skip the .cu compile loop entirely — the upstream build.rs would emit
    // empty PTX placeholders + cargo:warning, which is just noise for S4.
    //
    // If a future S4 feature needs custom CUDA kernels, copy the relevant
    // .cu file into src/cuda_kernels/ and re-add an entry here.
    let _: Option<&str> = nvcc.as_deref();

    // ---------- Phase F-1.5: HLIF C-ABI shim (C++ → static lib) ----------
    //
    // The shim is C++ (nvCOMP HLIF is a C++ template stack) but exports a
    // flat extern-"C" surface. We compile it with nvcc so the toolchain
    // matches the rest of the GPU code path (PTX kernels) and so the
    // shim can `#include <cuda_runtime.h>` + the nvcomp C++ headers
    // without standalone CUDA SDK detection logic. The output is a
    // .o object archived into a static lib `libferro_nvcomp_hlif_shim.a`
    // that Rust links via `cargo:rustc-link-lib=static=ferro_nvcomp_hlif_shim`.
    let nvcomp_include = format!("{nvcomp_home}/include");
    let shim_cpp = manifest_dir.join("src/cuda_kernels/nvcomp_hlif_shim.cpp");
    let shim_obj = out_dir.join("nvcomp_hlif_shim.o");
    let shim_lib = out_dir.join("libferro_nvcomp_hlif_shim.a");
    if let Some(nvcc_path) = nvcc.as_deref() {
        // sm_89 matches the PTX kernels above (4070 Ti SUPER target). The
        // HLIF shim itself doesn't emit kernels but nvcc still wants a
        // valid `-arch` for the C++ frontend's __device__-aware checks.
        // `-x c++` is critical: without it, nvcc would treat the source
        // as a .cpp host-only file but still try to invoke `cudafe++`
        // for any kernel-like syntax. The shim has no kernels — it's
        // pure host C++ that calls into libnvcomp — so we use `-Xcompiler`
        // to pass standard host flags through.
        let status = Command::new(nvcc_path)
            .args([
                "-std=c++17",
                "-O3",
                "-arch=sm_89",
                "-Xcompiler",
                "-fPIC",
                "-c",
                "-I",
            ])
            .arg(&nvcomp_include)
            .arg(&shim_cpp)
            .arg("-o")
            .arg(&shim_obj)
            .status();
        match status {
            Ok(s) if s.success() => {
                // Wrap the .o into a static archive (libferro_nvcomp_hlif_shim.a)
                // so cargo's `cargo:rustc-link-lib=static=...` directive can
                // pick it up from the `cargo:rustc-link-search=native=...`
                // directory below.
                let ar_status = Command::new("ar")
                    .arg("crs")
                    .arg(&shim_lib)
                    .arg(&shim_obj)
                    .status();
                match ar_status {
                    Ok(s) if s.success() => {
                        println!(
                            "cargo:warning=ferro-compress: built HLIF shim ({}) via nvcc + ar",
                            shim_lib.display()
                        );
                        println!("cargo:rustc-link-search=native={}", out_dir.display());
                        println!("cargo:rustc-link-lib=static=ferro_nvcomp_hlif_shim");
                        // The shim pulls in libstdc++ symbols (std::string,
                        // std::unique_ptr, exception machinery). nvcomp's
                        // own libnvcomp.so is C++ but its symbols are
                        // already linked via the dylib above; the shim
                        // needs the C++ runtime explicitly.
                        println!("cargo:rustc-link-lib=dylib=stdc++");
                    }
                    Ok(s) => {
                        println!(
                            "cargo:warning=ferro-compress: ar failed (exit {}) on {}; HLIF backend will not link",
                            s,
                            shim_lib.display()
                        );
                    }
                    Err(e) => {
                        println!(
                            "cargo:warning=ferro-compress: ar spawn failed: {}; HLIF backend will not link",
                            e
                        );
                    }
                }
            }
            Ok(s) => {
                println!(
                    "cargo:warning=ferro-compress: nvcc HLIF shim build failed (exit {}); HLIF backend disabled",
                    s
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=ferro-compress: nvcc HLIF shim spawn failed: {}; HLIF backend disabled",
                    e
                );
            }
        }
    } else {
        println!(
            "cargo:warning=ferro-compress: nvcc unavailable; HLIF shim not built. Backend will return BackendUnavailable at runtime."
        );
    }
}

#[allow(dead_code)]
fn compile_cu_to_ptx(cu: &std::path::Path, ptx: &std::path::Path, nvcc: Option<&str>) {
    match nvcc {
        Some(nvcc_path) => {
            // sm_89 = 4070 Ti SUPER (Phase 0 target). PTX is forward-portable
            // via JIT to newer arches (Hopper sm_90, Blackwell sm_100, ...);
            // older arches (sm_75 / sm_80) accept it too as long as the .cu
            // doesn't use sm_89-only intrinsics — bitwise AND/OR/XOR + reduction
            // are compatible with every CUDA-capable GPU.
            let status = Command::new(nvcc_path)
                .args(["--ptx", "--use_fast_math", "-arch=sm_89", "-o"])
                .arg(ptx)
                .arg(cu)
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!(
                        "cargo:warning=ferro-compress: built PTX ({}) via nvcc",
                        ptx.display()
                    );
                }
                Ok(s) => {
                    println!(
                        "cargo:warning=ferro-compress: nvcc {} failed (exit {}) compiling {}; writing empty PTX placeholder. Set NVCC=/path/to/nvcc or install CUDA toolkit to fix.",
                        nvcc_path,
                        s,
                        cu.display()
                    );
                    write_empty_ptx(ptx);
                }
                Err(e) => {
                    println!(
                        "cargo:warning=ferro-compress: failed to spawn nvcc {}: {} (compiling {}); writing empty PTX placeholder. Set NVCC=/path/to/nvcc or install CUDA toolkit to fix.",
                        nvcc_path,
                        e,
                        cu.display()
                    );
                    write_empty_ptx(ptx);
                }
            }
        }
        None => {
            println!(
                "cargo:warning=ferro-compress: nvcc not found (looked at $NVCC, $CUDA_HOME/bin/nvcc, $PATH). Writing empty PTX placeholder for {}; runtime will return a clear error on kernel construction. Set NVCC=/path/to/nvcc or install CUDA toolkit to enable.",
                cu.display()
            );
            write_empty_ptx(ptx);
        }
    }
}

#[allow(dead_code)]
fn write_empty_ptx(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(path, b"").is_err() {
        println!(
            "cargo:warning=ferro-compress: could not write empty PTX placeholder at {}",
            path.display()
        );
    }
}
