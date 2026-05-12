// build.rs — emits cargo link metadata when the `nvcomp-gpu` feature is on,
// and compiles the nvCOMP HLIF C-ABI shim via nvcc + ar.
//
// Without `nvcomp-gpu` the crate builds on any machine (no CUDA needed).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=NVCOMP_HOME");
    println!("cargo:rerun-if-env-changed=CUDA_HOME");
    println!("cargo:rerun-if-env-changed=NVCC");
    println!("cargo:rerun-if-changed=src/cuda_kernels/nvcomp_hlif_shim.cpp");

    if std::env::var_os("CARGO_FEATURE_NVCOMP_GPU").is_none() {
        return;
    }

    // ---------- Link metadata for nvCOMP / CUDA ----------

    let nvcomp_home = std::env::var("NVCOMP_HOME").unwrap_or_else(|_| "/opt/nvcomp".to_string());
    let nvcomp_lib = format!("{nvcomp_home}/lib");
    println!("cargo:rustc-link-search=native={nvcomp_lib}");
    println!("cargo:rustc-link-lib=dylib=nvcomp");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{nvcomp_lib}");

    let cuda_home = std::env::var("CUDA_HOME").unwrap_or_else(|_| "/usr/local/cuda".to_string());
    let cuda_lib_candidates = [
        format!("{cuda_home}/lib64"),
        format!("{cuda_home}/lib"),
        format!("{cuda_home}/targets/x86_64-linux/lib"),
        format!("{cuda_home}/lib64/stubs"),
        format!("{cuda_home}/targets/x86_64-linux/lib/stubs"),
    ];
    for path in &cuda_lib_candidates {
        if std::path::Path::new(path).is_dir() {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
    println!("cargo:rustc-link-lib=dylib=cudart");
    println!("cargo:rustc-link-lib=dylib=cuda");

    // ---------- HLIF C-ABI shim (C++ → static lib via nvcc + ar) ----------

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset under cargo"),
    );
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR unset under cargo"));

    let nvcc = std::env::var("NVCC").ok().or_else(|| {
        let candidate = format!("{cuda_home}/bin/nvcc");
        if std::path::Path::new(&candidate).exists() {
            Some(candidate)
        } else if Command::new("nvcc").arg("--version").output().is_ok() {
            Some("nvcc".to_string())
        } else {
            None
        }
    });

    let nvcomp_include = format!("{nvcomp_home}/include");
    let shim_cpp = manifest_dir.join("src/cuda_kernels/nvcomp_hlif_shim.cpp");
    let shim_obj = out_dir.join("nvcomp_hlif_shim.o");
    let shim_lib = out_dir.join("libferro_nvcomp_hlif_shim.a");
    if let Some(nvcc_path) = nvcc.as_deref() {
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
                let ar_status = Command::new("ar")
                    .arg("crs")
                    .arg(&shim_lib)
                    .arg(&shim_obj)
                    .status();
                match ar_status {
                    Ok(s) if s.success() => {
                        println!(
                            "cargo:warning=s4-codec: built HLIF shim ({}) via nvcc + ar",
                            shim_lib.display()
                        );
                        println!("cargo:rustc-link-search=native={}", out_dir.display());
                        println!("cargo:rustc-link-lib=static=ferro_nvcomp_hlif_shim");
                        println!("cargo:rustc-link-lib=dylib=stdc++");
                    }
                    Ok(s) => {
                        println!(
                            "cargo:warning=s4-codec: ar failed (exit {}) on {}; HLIF backend will not link",
                            s,
                            shim_lib.display()
                        );
                    }
                    Err(e) => {
                        println!(
                            "cargo:warning=s4-codec: ar spawn failed: {}; HLIF backend will not link",
                            e
                        );
                    }
                }
            }
            Ok(s) => {
                println!(
                    "cargo:warning=s4-codec: nvcc HLIF shim build failed (exit {}); HLIF backend disabled",
                    s
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=s4-codec: nvcc HLIF shim spawn failed: {}; HLIF backend disabled",
                    e
                );
            }
        }
    } else {
        println!(
            "cargo:warning=s4-codec: nvcc unavailable; HLIF shim not built. Backend will return BackendUnavailable at runtime."
        );
    }
}
