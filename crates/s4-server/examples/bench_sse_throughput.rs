//! v0.8 #50: SSE throughput bench (AES-NI vs software fallback).
//!
//! Measures `encrypt_v2` / `decrypt` (the S4E2 frame, which is what
//! `--sse-s4-key`-backed PUT / GET traffic exercises) at three body
//! sizes: 64 KiB / 1 MiB / 100 MiB. Reports both throughput (MB/s,
//! mebi-per-second) and per-call latency (microseconds) for each leg.
//!
//! Build with `--release` for fair numbers — debug builds suffer from
//! `aes-gcm`'s lack of inlining and report ~1/30th of the production
//! throughput.
//!
//! ## Reproducing both backends
//!
//! AES-NI (default on x86_64 hosts with the `aes` + `pclmulqdq` CPU features):
//!
//! ```sh
//! cargo run --release -p s4-server --example bench_sse_throughput
//! ```
//!
//! Software fallback (force the `aes-gcm` crate's pure-Rust path by
//! disabling the runtime cpufeatures probe; needs a clean `target/`
//! so the cached AES-NI build doesn't get reused):
//!
//! ```sh
//! RUSTFLAGS="--cfg aes_force_soft --cfg polyval_force_soft" \
//!     cargo run --release -p s4-server --example bench_sse_throughput
//! ```
//!
//! Plain `-C target-feature=-aes,-pclmulqdq` is *not* enough — the
//! `aes` + `polyval` crates do their own runtime CPU probe via the
//! `cpufeatures` crate and will still pick AES-NI on any host that
//! has it. Forcing the software backend requires the explicit
//! `aes_force_soft` / `polyval_force_soft` cfg flags above.

use s4_server::sse::{SseKey, SseKeyring, decrypt, encrypt_v2};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let key = Arc::new(SseKey::from_bytes(&[0x42u8; 32]).expect("32-byte key from raw bytes"));
    let keyring = SseKeyring::new(1, key);

    #[cfg(target_arch = "x86_64")]
    {
        println!(
            "AES-NI runtime detect: aes={}, pclmulqdq={}",
            std::is_x86_feature_detected!("aes"),
            std::is_x86_feature_detected!("pclmulqdq"),
        );
    }
    #[cfg(target_arch = "aarch64")]
    {
        // aarch64 runtime probes are still nightly-only in std, so we
        // report the compile-time target_feature snapshot instead. This
        // is what `aes-gcm` actually uses to pick its backend.
        println!(
            "aarch64 target features (compile-time): aes={}, pmull={}",
            cfg!(target_feature = "aes"),
            cfg!(target_feature = "pmull"),
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        println!("AES feature detection: not implemented for this target_arch");
    }

    println!();
    println!("{:>10}  {:>21}  {:>21}", "size", "encrypt", "decrypt");
    println!(
        "{:>10}  {:>10}  {:>9}  {:>10}  {:>9}",
        "(bytes)", "MB/s", "(µs/op)", "MB/s", "(µs/op)"
    );

    for &size in &[64 * 1024usize, 1024 * 1024, 100 * 1024 * 1024] {
        let plaintext = vec![0u8; size];
        // Heuristic: keep total wall-clock per case under ~5s on AES-NI
        // hardware. 100 MiB at ~5 GB/s ≈ 20 ms / iter, so 5 iters = 0.1s
        // (encrypt+decrypt). 64 KiB at the same rate is ~13 µs / iter,
        // so 100 iters = 1.3 ms — bump to 100 to dampen noise.
        let n: usize = if size > 10_000_000 { 5 } else { 100 };

        let mut encrypted = bytes::Bytes::new();
        let start = Instant::now();
        for _ in 0..n {
            encrypted = encrypt_v2(&plaintext, &keyring);
        }
        let encrypt_secs = start.elapsed().as_secs_f64() / n as f64;
        let encrypt_mbps = (size as f64) / encrypt_secs / 1_048_576.0;

        let start = Instant::now();
        for _ in 0..n {
            let _ = decrypt(&encrypted, &keyring).expect("roundtrip decrypt must succeed");
        }
        let decrypt_secs = start.elapsed().as_secs_f64() / n as f64;
        let decrypt_mbps = (size as f64) / decrypt_secs / 1_048_576.0;

        println!(
            "{:>10}  {:>10.1}  {:>9.3}  {:>10.1}  {:>9.3}",
            size,
            encrypt_mbps,
            encrypt_secs * 1e6,
            decrypt_mbps,
            decrypt_secs * 1e6,
        );
    }
}
