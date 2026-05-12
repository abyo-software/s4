//! nvCOMP (NVIDIA proprietary) backend ラッパー — Phase 1 で実装。
//!
//! ## 設計方針 (2026-05-12 調査結果)
//!
//! - **再利用候補**: `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress`
//!   に既存の nvCOMP Rust binding (Apache-2.0 OR MIT、`nvcomp.rs` 1300+ 行、Bitcomp HLIF /
//!   Zstd batched / LZ4 + roundtrip test 済) が揃っている。S4 にどう取り込むかは要決定:
//!   (a) path dep、(b) vendored copy、(c) 共通 crate 抽出のいずれか。
//! - **CUDA 連携**: `cudarc` 0.19+ を `dynamic-loading` feature で採用予定。
//! - **配布形態**: nvCOMP redist は NVIDIA SLA 制約あり。Phase 1 は **BYO 方式**
//!   (顧客が NGC からダウンロード) を default、AMI 同梱は NVIDIA 書面確認後に判断。
//!
//! ## サポート予定 codec
//! - Bitcomp (整数列、3.59-7.48× 圧縮率実測)
//! - gANS (entropy)
//! - zstd-GPU (汎用 text)

// TODO Phase 1: ferro-compress を流用 or 抽出して実装する
