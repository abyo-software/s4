//! nvCOMP (NVIDIA proprietary) backend ラッパー — Phase 1 で実装。
//!
//! ## 設計方針 (2026-05-12 調査結果)
//!
//! - **再利用候補 (vendored 済 2026-05-12)**: `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress`
//!   の nvCOMP-only サブセット (Apache-2.0 OR MIT、nvcomp.rs / nvcomp_hlif.rs /
//!   bitcomp_device.rs / slab_alloc.rs / nvcomp_sys / HLIF shim) を
//!   `crates/s4-codec/vendor/ferro-compress/` に footprint としてコピー済。
//!   FerroSearch 固有 (BitmapOpKernel / StatsOpKernel / CPU codec / Backend
//!   dispatcher) は意図的に除外。詳細は `vendor/ferro-compress/src/lib.rs` 参照。
//! - **wiring 戦略**: Phase 1 で vendor crate を workspace member に昇格させ、
//!   この module から `ferro_compress_vendored::NvcompCodec` を呼ぶ。M&A 時に
//!   Ferro 側 ferro-compress と分岐しても S4 単独で build 可能。
//! - **CUDA 連携**: `cudarc` 0.19+ を `dynamic-loading` feature で採用予定。
//! - **配布形態**: nvCOMP redist は NVIDIA SLA 制約あり。Phase 1 は **BYO 方式**
//!   (顧客が NGC からダウンロード) を default、AMI 同梱は NVIDIA 書面確認後に判断。
//!
//! ## サポート予定 codec
//! - Bitcomp (整数列、3.59-7.48× 圧縮率実測)
//! - gANS (entropy)
//! - zstd-GPU (汎用 text)

// TODO Phase 1: ferro-compress を流用 or 抽出して実装する
