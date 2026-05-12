//! PUT 時にどの codec で圧縮するかを選ぶ dispatcher。
//!
//! Phase 1 では「常に同じ codec を選ぶ」`AlwaysDispatcher` を提供。
//! Phase 1 後半で `SamplingDispatcher` を追加し、入力先頭の sampling で
//! integer 主体 / text 主体 / 既圧縮 を判定して codec を切り替える。

use crate::CodecKind;

/// PUT body の先頭 sample から codec を選ぶ trait。
#[async_trait::async_trait]
pub trait CodecDispatcher: Send + Sync {
    async fn pick(&self, sample: &[u8]) -> CodecKind;
}

/// 常に同じ kind を返す dispatcher (固定 codec 運用)。
#[derive(Debug, Clone, Copy)]
pub struct AlwaysDispatcher(pub CodecKind);

#[async_trait::async_trait]
impl CodecDispatcher for AlwaysDispatcher {
    async fn pick(&self, _sample: &[u8]) -> CodecKind {
        self.0
    }
}

/// 入力 sample を見て codec を選ぶ dispatcher。
///
/// 判定順 (上位優先):
/// 1. 短すぎる入力 (<128 byte) → `default`
/// 2. magic bytes が既圧縮フォーマット (gzip / zstd / png / jpeg / mp4 / zip / pdf
///    / 7z / xz / bzip2) → `Passthrough` (再圧縮しても意味がない)
/// 3. Shannon entropy が `entropy_threshold` (default 7.5 bits/byte) 以上 → `Passthrough`
///    (高エントロピー = ほぼランダム = 圧縮余地なし)
/// 4. それ以外 → `default` (text / log / parquet 数値列等、圧縮余地あり)
///
/// Phase 1 では `default = CpuZstd` 想定。Phase 1 後半で integer-column 検出を加え、
/// `default` 分岐を「数値列なら NvcompBitcomp、そうでなければ CpuZstd」に拡張する。
#[derive(Debug, Clone)]
pub struct SamplingDispatcher {
    pub default: CodecKind,
    pub entropy_threshold: f64,
}

impl SamplingDispatcher {
    pub const DEFAULT_ENTROPY_THRESHOLD: f64 = 7.5;
    pub const MIN_SAMPLE_BYTES: usize = 128;

    pub fn new(default: CodecKind) -> Self {
        Self {
            default,
            entropy_threshold: Self::DEFAULT_ENTROPY_THRESHOLD,
        }
    }

    #[must_use]
    pub fn with_entropy_threshold(mut self, t: f64) -> Self {
        self.entropy_threshold = t;
        self
    }
}

/// Shannon entropy (bits per byte) を sample から推定。0..=8 の範囲。
fn shannon_entropy(sample: &[u8]) -> f64 {
    if sample.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in sample {
        counts[b as usize] += 1;
    }
    let n = sample.len() as f64;
    let mut entropy = 0.0;
    for c in counts {
        if c == 0 {
            continue;
        }
        let p = f64::from(c) / n;
        entropy -= p * p.log2();
    }
    entropy
}

/// 既圧縮データの magic bytes 検出。検出した場合は true を返す。
fn looks_already_compressed(sample: &[u8]) -> bool {
    // gzip
    if sample.starts_with(&[0x1f, 0x8b]) {
        return true;
    }
    // zstd
    if sample.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return true;
    }
    // PNG
    if sample.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return true;
    }
    // JPEG (FF D8 FF)
    if sample.len() >= 3 && sample[0] == 0xff && sample[1] == 0xd8 && sample[2] == 0xff {
        return true;
    }
    // PDF
    if sample.starts_with(b"%PDF-") {
        return true;
    }
    // ZIP / docx / jar / apk
    if sample.starts_with(&[0x50, 0x4b, 0x03, 0x04]) {
        return true;
    }
    // 7z
    if sample.starts_with(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]) {
        return true;
    }
    // xz
    if sample.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return true;
    }
    // bzip2
    if sample.starts_with(b"BZh") {
        return true;
    }
    // mp4 / m4a / mov (ISO Base Media): bytes 4..8 == "ftyp"
    if sample.len() >= 8 && &sample[4..8] == b"ftyp" {
        return true;
    }
    // webm / mkv (EBML)
    if sample.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return true;
    }
    // webp (RIFF .... WEBP)
    if sample.len() >= 12 && sample.starts_with(b"RIFF") && &sample[8..12] == b"WEBP" {
        return true;
    }
    false
}

#[async_trait::async_trait]
impl CodecDispatcher for SamplingDispatcher {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        if sample.len() < Self::MIN_SAMPLE_BYTES {
            return self.default;
        }
        if looks_already_compressed(sample) {
            return CodecKind::Passthrough;
        }
        if shannon_entropy(sample) >= self.entropy_threshold {
            return CodecKind::Passthrough;
        }
        self.default
    }
}

/// `Box<dyn CodecDispatcher>` からも `CodecDispatcher` として使えるようにする blanket impl
#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for Box<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }
}

#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for std::sync::Arc<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn always_dispatcher_returns_configured_kind() {
        let d = AlwaysDispatcher(CodecKind::CpuZstd);
        assert_eq!(d.pick(b"any input").await, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn boxed_dispatcher_works() {
        let d: Box<dyn CodecDispatcher> = Box::new(AlwaysDispatcher(CodecKind::Passthrough));
        assert_eq!(d.pick(b"x").await, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn sampling_short_sample_uses_default() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        assert_eq!(d.pick(b"short").await, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn sampling_text_picks_default() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        // 1 KB の英語っぽい text (低エントロピー)
        let text: Vec<u8> = "the quick brown fox jumps over the lazy dog. "
            .repeat(30)
            .into_bytes();
        assert_eq!(d.pick(&text).await, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn sampling_random_bytes_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        // 1 KB の高エントロピー (擬似ランダムデータを作る — XOR-shift で uniformish に)
        let mut state: u64 = 0xfeed_beef_dead_c0de;
        let mut payload = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        // entropy が default threshold (7.5) 以上のはず
        let e = shannon_entropy(&payload);
        assert!(
            e > 7.5,
            "expected high entropy on pseudo-random bytes, got {e}"
        );
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn sampling_gzip_magic_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        let mut payload = vec![0x1f, 0x8b, 0x08]; // gzip magic + DEFLATE method
        payload.extend(std::iter::repeat_n(b'a', 256));
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn sampling_png_magic_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        let mut payload = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        payload.extend(std::iter::repeat_n(b'b', 256));
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn sampling_mp4_ftyp_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        let mut payload = vec![0u8; 256];
        payload[4..8].copy_from_slice(b"ftyp");
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[test]
    fn entropy_zero_for_uniform() {
        let zeros = vec![0u8; 1024];
        assert_eq!(shannon_entropy(&zeros), 0.0);
    }

    #[test]
    fn entropy_full_8_for_each_byte_once() {
        // 0..256 を 1 度ずつ → 各 byte の確率 1/256 → entropy = 8 bits
        let mut payload: Vec<u8> = (0..=255).collect();
        // 256 byte は最小 sample 未満になりうるので 1024 まで複製 (entropy は不変)
        let copy = payload.clone();
        for _ in 0..3 {
            payload.extend_from_slice(&copy);
        }
        let e = shannon_entropy(&payload);
        assert!((e - 8.0).abs() < 0.0001, "expected 8.0, got {e}");
    }
}
