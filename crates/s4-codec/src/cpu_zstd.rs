//! CPU zstd backend — GPU 非搭載環境向け究極の fallback。
//!
//! ## 設計方針
//!
//! - `zstd` crate (Apache-2.0 OR MIT) を使う直球実装
//! - GPU が使えない環境でも S4 が動作することを保証 (テスト容易性向上)
//! - production では nvCOMP より遅いが、機能の test bed として常に走らせる

// TODO Phase 1: zstd crate を使った Codec impl
