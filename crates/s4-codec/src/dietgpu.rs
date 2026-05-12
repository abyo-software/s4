//! DietGPU (Meta, MIT) backend ラッパー — nvCOMP ライセンス障害時の OSS fallback。
//!
//! ## 設計方針
//!
//! - DietGPU (https://github.com/facebookresearch/dietgpu) は ANS-only entropy codec、
//!   A100 で 250-410 GB/s 実測。Bitcomp の代替にはならないが、整数 / float の
//!   entropy 圧縮では nvCOMP ANS と拮抗。
//! - MIT ライセンスのため AMI 同梱の法務リスクなし。
//! - 実装は Phase 2 想定 (nvCOMP 採用が固まらなかった場合の保険)。

// TODO Phase 2: DietGPU C++ を Rust から呼ぶ薄い FFI を実装
