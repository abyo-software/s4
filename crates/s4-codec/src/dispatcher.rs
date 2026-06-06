//! PUT 時にどの codec で圧縮するかを選ぶ dispatcher。
//!
//! Phase 1 では「常に同じ codec を選ぶ」`AlwaysDispatcher` を提供。
//! Phase 1 後半で `SamplingDispatcher` を追加し、入力先頭の sampling で
//! integer 主体 / text 主体 / 既圧縮 を判定して codec を切り替える。

use crate::CodecKind;

/// PUT body の先頭 sample から codec を選ぶ trait。
///
/// v0.8 #56: 呼び出し側が `Content-Length` を知っている場合 (chunked transfer
/// でない通常 PUT)、`pick_with_size_hint` 経由で total body size を渡せる。
/// `SamplingDispatcher` は GPU upload overhead が compress 時間を上回る小オブ
/// ジェクトで CPU codec を選び、十分大きい (>= `gpu_min_bytes`) ものでだけ
/// GPU codec へ昇格させる。size hint が `None` (chunked transfer) の場合は
/// 保守的に CPU 側に倒す。
///
/// 既定実装は `pick_with_size_hint(sample, None)` を `pick(sample)` に委譲する
/// — 既存 implementor は `pick` だけ実装すれば従来通り動く。
#[async_trait::async_trait]
pub trait CodecDispatcher: Send + Sync {
    async fn pick(&self, sample: &[u8]) -> CodecKind;

    /// v0.8 #56: size-hint aware pick. 既定実装は `pick(sample)` に委譲する
    /// ので、追加情報を活用する dispatcher (`SamplingDispatcher`) のみ override
    /// すればよい。`total_size = None` は「chunked transfer で content-length
    /// が無い」ケースを表す。
    async fn pick_with_size_hint(&self, sample: &[u8], _total_size: Option<u64>) -> CodecKind {
        self.pick(sample).await
    }
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
///
/// ## v0.8 #56: GPU auto-detect at boot
///
/// `with_gpu_preference(true, gpu_min_bytes)` を呼ぶと、boot 時に
/// `s4_codec::nvcomp::is_gpu_available()` が true を返した場合に限り、
/// 「default が `CpuZstd` でかつ total size >= `gpu_min_bytes` の object」を
/// `NvcompZstd` に昇格させる。size hint が `None` (chunked transfer)、
/// または閾値未満の小オブジェクトでは GPU upload overhead を避けるため
/// CPU codec のままにする。
///
/// `nvcomp-gpu` feature が build-time で off の場合、`NvcompZstd` への昇格は
/// 行わない (registry に居ない codec を指すと dispatch 時に
/// `UnregisteredCodec` で fail するため)。orchestrator は main.rs 側で
/// `prefer_gpu = false` を強制することでこれを担保する。
#[derive(Debug, Clone)]
pub struct SamplingDispatcher {
    pub default: CodecKind,
    pub entropy_threshold: f64,
    /// v0.8 #56: when set, route large `CpuZstd` picks through `NvcompZstd`.
    pub prefer_gpu: bool,
    /// v0.8 #56: GPU promotion only fires when the caller can prove
    /// `total_size >= gpu_min_bytes` via `pick_with_size_hint`. Below this
    /// threshold the GPU upload overhead exceeds the compress time so CPU
    /// wins; the default 1 MiB is the empirical break-even point on common
    /// text / log payloads with PCIe 4.0 + an A10G-class GPU.
    pub gpu_min_bytes: usize,
    /// v0.8.12 #125: when set, sample-based columnar-integer detection
    /// promotes a `CpuZstd` pick to `NvcompBitcomp` instead of
    /// `NvcompZstd` for Parquet / postings / time-series payloads.
    /// Requires the same `prefer_gpu = true` and
    /// `total_size >= gpu_min_bytes` preconditions — the columnar
    /// promotion adds *targeting* on top of the GPU-promotion gate,
    /// it doesn't loosen it. When `false` (default), large CpuZstd
    /// picks always go to NvcompZstd, matching v0.8.11 behaviour.
    pub prefer_columnar_gpu: bool,
}

impl SamplingDispatcher {
    pub const DEFAULT_ENTROPY_THRESHOLD: f64 = 7.5;
    pub const MIN_SAMPLE_BYTES: usize = 128;
    /// v0.8 #56: 1 MiB. The empirical break-even point — below this, the
    /// PCIe upload + kernel launch overhead dominates the GPU's compress
    /// throughput advantage.
    pub const DEFAULT_GPU_MIN_BYTES: usize = 1_048_576;

    pub fn new(default: CodecKind) -> Self {
        Self {
            default,
            entropy_threshold: Self::DEFAULT_ENTROPY_THRESHOLD,
            prefer_gpu: false,
            gpu_min_bytes: Self::DEFAULT_GPU_MIN_BYTES,
            prefer_columnar_gpu: false,
        }
    }

    /// v0.8.12 #125: enable Bitcomp routing for columnar-integer
    /// payloads. Composes with `with_gpu_preference` — both must be
    /// on for any promotion to fire, and the columnar branch picks
    /// `NvcompBitcomp` instead of `NvcompZstd` when the sample
    /// matches the per-position-entropy signature of a u32 / u64 LE
    /// integer column (Parquet, postings, time-series). When this
    /// flag is off (default) the README's "integer/columnar →
    /// Bitcomp" pitch is honoured manually via `--codec
    /// nvcomp-bitcomp`; turning it on makes the SamplingDispatcher
    /// pick Bitcomp automatically.
    #[must_use]
    pub fn with_columnar_gpu_preference(mut self, on: bool) -> Self {
        self.prefer_columnar_gpu = on;
        self
    }

    #[must_use]
    pub fn with_entropy_threshold(mut self, t: f64) -> Self {
        self.entropy_threshold = t;
        self
    }

    /// v0.8 #56: enable GPU promotion. When `prefer_gpu = true`, a `CpuZstd`
    /// pick on a body whose `total_size >= gpu_min_bytes` is rewritten to
    /// `NvcompZstd`. Pass `prefer_gpu = false` (the default) to disable.
    /// The threshold is in bytes; `1_048_576` (1 MiB) is the recommended
    /// default for PCIe 4.0 hosts.
    #[must_use]
    pub fn with_gpu_preference(mut self, prefer_gpu: bool, gpu_min_bytes: usize) -> Self {
        self.prefer_gpu = prefer_gpu;
        self.gpu_min_bytes = gpu_min_bytes;
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

/// v0.8.12 #125: minimum sample size at which the columnar-integer
/// signature is statistically meaningful. Below this we'd be reading
/// noise into the per-stride-position byte histogram. 512 bytes =
/// 128 u32-stride samples per position, ~64 u64-stride samples.
const COLUMNAR_MIN_SAMPLE: usize = 512;
/// v0.8.12 #125: per-stride-position entropy gap that flags a sample
/// as columnar-integer. Random data has near-uniform per-position
/// entropy (gap ≈ 0); a u32 LE column of bounded values
/// (`value < 2^24`) has full entropy on the low byte and ~0 entropy
/// on the high byte (gap > 6). 4.0 bits is a conservative middle
/// ground that catches u32 / u64 monotonic-id and timestamp columns
/// without false-positives on text or mixed binary records.
const COLUMNAR_ENTROPY_GAP: f64 = 4.0;
/// v0.8.12 #125: per-position byte-histogram entropy. Reused for
/// each stride position in [`looks_columnar_integer`]; same `[u8; 256]`
/// shape as [`shannon_entropy`] for the whole sample.
fn entropy_at_stride_position(sample: &[u8], stride: usize, pos: usize) -> f64 {
    debug_assert!(pos < stride);
    debug_assert!(stride > 0);
    let mut counts = [0u32; 256];
    let mut n = 0u32;
    let mut i = pos;
    while i < sample.len() {
        counts[sample[i] as usize] += 1;
        n += 1;
        i += stride;
    }
    if n == 0 {
        return 0.0;
    }
    let nf = f64::from(n);
    let mut e = 0.0;
    for c in counts {
        if c == 0 {
            continue;
        }
        let p = f64::from(c) / nf;
        e -= p * p.log2();
    }
    e
}

/// v0.8.12 #125: detect a u32 / u64 little-endian integer column in
/// the sample. Returns `true` when one stride's per-position entropy
/// gap exceeds [`COLUMNAR_ENTROPY_GAP`] — the signature of a column
/// whose high bytes are mostly zero (bounded ints) while the low
/// bytes vary freely (counts / timestamps / sorted ids). Conservative
/// by design: tested against Parquet u32 / u64 columns
/// (`apache-parquet/test/data/`), pseudo-random bytes, English text,
/// and DNA reads — only the integer columns trip the gap.
fn looks_columnar_integer(sample: &[u8]) -> bool {
    if sample.len() < COLUMNAR_MIN_SAMPLE {
        return false;
    }
    for &stride in &[4usize, 8usize] {
        // Need ≥ 64 strides for the per-position histogram to be
        // stable; below that, even random data shows large gaps.
        if sample.len() < stride * 64 {
            continue;
        }
        let mut min_e = f64::INFINITY;
        let mut max_e = f64::NEG_INFINITY;
        for pos in 0..stride {
            let e = entropy_at_stride_position(sample, stride, pos);
            if e < min_e {
                min_e = e;
            }
            if e > max_e {
                max_e = e;
            }
        }
        if max_e - min_e >= COLUMNAR_ENTROPY_GAP {
            return true;
        }
    }
    false
}

/// v0.8.15 M-7 / v0.8.16 F-12: confirm that the bytes *after* the
/// magic-byte prefix look like compressed data (high entropy), not
/// benign text whose leading 2-3 bytes happen to spell the magic.
/// Returns `true` when the post-magic window has entropy `>= threshold`
/// (default 7.5). Operates on `sample[16..]` ── 16 bytes of skip is
/// enough to clear every magic this dispatcher knows about while
/// leaving plenty of runway for the entropy estimate to be statistically
/// meaningful.
///
/// v0.8.16 F-12 fix: small samples now default to `false` (= "don't
/// trust the magic byte alone on short samples"). The v0.8.15 M-7
/// motivation was a 40-byte `BZh:loglog:` user log file — but the
/// pre-F-12 `<= SKIP+32` short-circuit returned `true`, so
/// passthrough still fired on exactly the case M-7 was meant to
/// catch. Real bzip2 / gzip / zstd objects are essentially never <
/// 48 bytes; rejecting the magic on a short sample is the safer
/// default. Operators who really want passthrough on tiny inputs
/// can run `--codec passthrough` explicitly.
fn post_magic_entropy_high(sample: &[u8], threshold: f64) -> bool {
    const SKIP: usize = 16;
    if sample.len() <= SKIP + 32 {
        return false;
    }
    shannon_entropy(&sample[SKIP..]) >= threshold
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

impl SamplingDispatcher {
    /// Core sample-only decision shared by `pick` and `pick_with_size_hint`.
    /// Returns the pre-GPU-promotion choice; the size-hint-aware caller may
    /// rewrite a `CpuZstd` result to `NvcompZstd` when the body is big enough.
    ///
    /// # Adversarial limitations (v0.8.15 M-6 / M-7)
    ///
    /// The sample is just the prefix the listener captured (typically
    /// the first 4 KiB). An attacker who controls the upload bytes
    /// can:
    ///
    /// - **Trick passthrough into firing** by prefixing a gzip / zstd
    ///   magic and following it with 10 GiB of zeros, costing the
    ///   gateway disk space the operator expected to save. Mitigated
    ///   by requiring the post-magic window to *also* show high
    ///   entropy — real compressed bytes have both, an unscrupulous
    ///   text payload won't.
    /// - **Trick passthrough into NOT firing** by prefixing 4 KiB of
    ///   zeros to an already-compressed body, costing CPU on a
    ///   useless compress pass. The dispatcher cannot defend against
    ///   this without re-sampling other windows (a v0.8.15 follow-up;
    ///   would require listener-side changes to capture multiple
    ///   windows, not just the prefix).
    ///
    /// The sample-only path is "best-effort", not "adversarial".
    /// Operators who need an adversarial guarantee should set
    /// `--dispatcher always --codec cpu-zstd` (compress everything)
    /// or `--codec passthrough` (compress nothing) and bypass the
    /// sampler entirely.
    fn pick_from_sample(&self, sample: &[u8]) -> CodecKind {
        // v0.8.17 G-3: run the magic-byte + post-magic-entropy
        // check FIRST, regardless of `MIN_SAMPLE_BYTES`. The
        // v0.8.16 F-12 guard inside `post_magic_entropy_high`
        // was never reachable because the upstream
        // `< MIN_SAMPLE_BYTES (=128)` short-circuit subsumed the
        // `<= 48` short-sample case the comment cited. Promote
        // the magic-byte arm above the short-circuit and let
        // `post_magic_entropy_high` decide for itself how to
        // handle short samples — that's the only place where the
        // F-12 `false` default actually matters and where the
        // `BZh:loglog:` motivation gets caught.
        if looks_already_compressed(sample)
            && post_magic_entropy_high(sample, self.entropy_threshold)
        {
            return CodecKind::Passthrough;
        }
        if sample.len() < Self::MIN_SAMPLE_BYTES {
            return self.default;
        }
        if shannon_entropy(sample) >= self.entropy_threshold {
            return CodecKind::Passthrough;
        }
        self.default
    }

    /// v0.8 #56 / v0.8.12 #125: rewrite a `CpuZstd` pick to a GPU
    /// codec when GPU preference is on AND the caller proved a total
    /// body size >= `gpu_min_bytes`. v0.8.12 adds the columnar-integer
    /// branch: when `prefer_columnar_gpu = true` AND the sample
    /// matches the per-stride-position entropy signature of a
    /// u32 / u64 LE integer column, route to `NvcompBitcomp` instead
    /// of `NvcompZstd`. Passthrough / non-CpuZstd picks are left
    /// alone — already-compressed bodies don't benefit from GPU
    /// compression, and other CPU codecs (CpuGzip) imply the
    /// operator wants wire-compatible output that the nvCOMP codecs
    /// can't provide.
    fn maybe_promote_to_gpu(
        &self,
        chosen: CodecKind,
        sample: &[u8],
        total_size: Option<u64>,
    ) -> CodecKind {
        if !self.prefer_gpu {
            return chosen;
        }
        if chosen != CodecKind::CpuZstd {
            return chosen;
        }
        let big_enough = match total_size {
            Some(n) => n >= self.gpu_min_bytes as u64,
            // No size hint (chunked transfer) → conservative, keep CpuZstd.
            None => return chosen,
        };
        if !big_enough {
            return chosen;
        }
        if self.prefer_columnar_gpu && looks_columnar_integer(sample) {
            CodecKind::NvcompBitcomp
        } else {
            CodecKind::NvcompZstd
        }
    }
}

#[async_trait::async_trait]
impl CodecDispatcher for SamplingDispatcher {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        // No size hint available → never promote to GPU.
        self.pick_from_sample(sample)
    }

    async fn pick_with_size_hint(&self, sample: &[u8], total_size: Option<u64>) -> CodecKind {
        let chosen = self.pick_from_sample(sample);
        self.maybe_promote_to_gpu(chosen, sample, total_size)
    }
}

/// `Box<dyn CodecDispatcher>` からも `CodecDispatcher` として使えるようにする blanket impl
#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for Box<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }

    async fn pick_with_size_hint(&self, sample: &[u8], total_size: Option<u64>) -> CodecKind {
        (**self).pick_with_size_hint(sample, total_size).await
    }
}

#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for std::sync::Arc<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }

    async fn pick_with_size_hint(&self, sample: &[u8], total_size: Option<u64>) -> CodecKind {
        (**self).pick_with_size_hint(sample, total_size).await
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
        // v0.8.15 M-7: the post-magic window must also look like
        // compressed bytes (high entropy) for passthrough to fire.
        // Use random-ish bytes instead of repeating `a` so the
        // post-magic check passes.
        let mut payload = vec![0x1f, 0x8b, 0x08]; // gzip magic + DEFLATE method
        let mut state: u64 = 0xdead_c0de_feed_beef;
        for _ in 0..512 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    /// v0.8.15 M-7: a user log file starting with `BZh` followed by
    /// English text (low entropy) MUST NOT trigger passthrough — the
    /// pre-M-7 magic-byte check fired on that prefix alone, silently
    /// skipping compression on customer logs that happened to begin
    /// with bzip2's 3-byte magic.
    #[tokio::test]
    async fn sampling_magic_prefix_but_low_entropy_body_compresses() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        let mut payload = b"BZh just a log line\n".to_vec();
        // Append low-entropy English text to fill the sample window.
        payload.extend(
            "the quick brown fox jumps over the lazy dog. "
                .repeat(20)
                .into_bytes(),
        );
        assert_eq!(d.pick(&payload).await, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn sampling_png_magic_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        // v0.8.15 M-7: real PNG bytes have high entropy after the
        // magic — pseudo-random fill exercises the new "magic +
        // post-magic high entropy" branch.
        let mut payload = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        let mut state: u64 = 0xc0de_f00d_dead_face;
        for _ in 0..512 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn sampling_mp4_ftyp_picks_passthrough() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        // v0.8.15 M-7: same shape — magic at bytes 4..8 plus a
        // high-entropy body after for the post-magic check.
        let mut payload = vec![0u8; 8];
        payload[4..8].copy_from_slice(b"ftyp");
        let mut state: u64 = 0x1234_5678_dead_beef;
        for _ in 0..512 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        assert_eq!(d.pick(&payload).await, CodecKind::Passthrough);
    }

    #[test]
    fn entropy_zero_for_uniform() {
        let zeros = vec![0u8; 1024];
        assert_eq!(shannon_entropy(&zeros), 0.0);
    }

    // ===========================================================
    // v0.8 #56: GPU auto-detect / size-hint promotion
    // ===========================================================

    /// Build a 1 KiB low-entropy text sample (repeats a sentence) — the
    /// post-magic-byte / post-entropy decision falls through to `default`,
    /// which the v0.8 #56 promotion logic then either keeps as `CpuZstd`
    /// or rewrites to `NvcompZstd`.
    fn text_sample() -> Vec<u8> {
        "the quick brown fox jumps over the lazy dog. "
            .repeat(30)
            .into_bytes()
    }

    #[tokio::test]
    async fn gpu_pref_promotes_large_text_to_nvcomp_zstd() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd).with_gpu_preference(true, 1_048_576);
        let sample = text_sample();
        // 2 MiB total body — past the 1 MiB threshold → GPU promotion.
        let kind = d.pick_with_size_hint(&sample, Some(2 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::NvcompZstd);
    }

    #[tokio::test]
    async fn gpu_pref_keeps_small_object_on_cpu() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd).with_gpu_preference(true, 1_048_576);
        let sample = text_sample();
        // 100 KiB total body — under the 1 MiB threshold → GPU upload
        // overhead would exceed compress savings, stay on CPU.
        let kind = d.pick_with_size_hint(&sample, Some(100 * 1024)).await;
        assert_eq!(kind, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn gpu_pref_off_keeps_cpu_even_for_large_object() {
        // Default — no `with_gpu_preference` call → prefer_gpu = false.
        let d = SamplingDispatcher::new(CodecKind::CpuZstd);
        let sample = text_sample();
        let kind = d.pick_with_size_hint(&sample, Some(10 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn gpu_pref_does_not_override_passthrough_on_high_entropy() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd).with_gpu_preference(true, 1_048_576);
        // High-entropy pseudo-random payload → entropy filter wins,
        // returns Passthrough; GPU promotion is skipped because
        // already-compressed data won't compress further on GPU either.
        let mut state: u64 = 0xfeed_beef_dead_c0de;
        let mut payload = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        let kind = d.pick_with_size_hint(&payload, Some(8 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::Passthrough);
    }

    #[tokio::test]
    async fn gpu_pref_with_no_size_hint_stays_conservative() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd).with_gpu_preference(true, 1_048_576);
        let sample = text_sample();
        // Chunked transfer: caller has no Content-Length, so total_size =
        // None. We can't safely commit to GPU because the body might be
        // tiny — stay on CPU.
        let kind = d.pick_with_size_hint(&sample, None).await;
        assert_eq!(kind, CodecKind::CpuZstd);
    }

    // ===========================================================
    // v0.8.12 #125: columnar-integer detection + Bitcomp routing
    // ===========================================================

    /// 1 KiB of u32 LE monotonic counts (postings / sorted ids). The
    /// low byte cycles 0..256, the middle bytes barely move, and the
    /// high byte stays at 0 — exactly the per-position-entropy
    /// signature `looks_columnar_integer` is built to catch.
    fn u32_monotonic_postings() -> Vec<u8> {
        let mut buf = Vec::with_capacity(4096);
        for i in 0u32..1024 {
            buf.extend_from_slice(&i.to_le_bytes());
        }
        buf
    }

    /// 4 KiB of u64 LE near-monotonic timestamps (Unix epoch nanos —
    /// stride 8, the high 3 bytes are nearly constant, the bottom 5
    /// drift slowly).
    fn u64_timestamps() -> Vec<u8> {
        let base: u64 = 1_700_000_000_000_000_000;
        let mut buf = Vec::with_capacity(4096);
        for i in 0u64..512 {
            buf.extend_from_slice(&(base + i * 137).to_le_bytes());
        }
        buf
    }

    #[test]
    fn columnar_detect_flags_u32_postings() {
        assert!(looks_columnar_integer(&u32_monotonic_postings()));
    }

    #[test]
    fn columnar_detect_flags_u64_timestamps() {
        assert!(looks_columnar_integer(&u64_timestamps()));
    }

    #[test]
    fn columnar_detect_rejects_english_text() {
        let text: Vec<u8> = "the quick brown fox jumps over the lazy dog. "
            .repeat(50)
            .into_bytes();
        // English text has reasonably uniform per-stride-position
        // entropy — no single byte position dominates the entropy.
        assert!(!looks_columnar_integer(&text));
    }

    #[test]
    fn columnar_detect_rejects_random_bytes() {
        let mut state: u64 = 0xa5a5_5a5a_dead_beef;
        let mut payload = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.push((state & 0xff) as u8);
        }
        assert!(!looks_columnar_integer(&payload));
    }

    #[test]
    fn columnar_detect_rejects_too_small_sample() {
        // 256 bytes < COLUMNAR_MIN_SAMPLE (512) — must short-circuit
        // to `false` so we never flag a tiny request as columnar.
        let mut buf = Vec::with_capacity(256);
        for i in 0u32..64 {
            buf.extend_from_slice(&i.to_le_bytes());
        }
        assert!(!looks_columnar_integer(&buf));
    }

    #[tokio::test]
    async fn gpu_pref_columnar_promotes_postings_to_bitcomp() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd)
            .with_gpu_preference(true, 1_048_576)
            .with_columnar_gpu_preference(true);
        let sample = u32_monotonic_postings();
        let kind = d.pick_with_size_hint(&sample, Some(8 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::NvcompBitcomp);
    }

    #[tokio::test]
    async fn gpu_pref_columnar_promotes_timestamps_to_bitcomp() {
        let d = SamplingDispatcher::new(CodecKind::CpuZstd)
            .with_gpu_preference(true, 1_048_576)
            .with_columnar_gpu_preference(true);
        let sample = u64_timestamps();
        let kind = d.pick_with_size_hint(&sample, Some(4 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::NvcompBitcomp);
    }

    #[tokio::test]
    async fn gpu_pref_columnar_falls_through_to_zstd_on_text() {
        // Columnar detector rejects text → Bitcomp routing skipped,
        // existing NvcompZstd promotion (#56) takes over.
        let d = SamplingDispatcher::new(CodecKind::CpuZstd)
            .with_gpu_preference(true, 1_048_576)
            .with_columnar_gpu_preference(true);
        let sample = text_sample();
        let kind = d.pick_with_size_hint(&sample, Some(2 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::NvcompZstd);
    }

    #[tokio::test]
    async fn gpu_pref_columnar_off_keeps_postings_on_zstd() {
        // Default — `with_columnar_gpu_preference` NOT called → the
        // README's "manual `--codec nvcomp-bitcomp`" path is the
        // only way to reach Bitcomp.
        let d = SamplingDispatcher::new(CodecKind::CpuZstd).with_gpu_preference(true, 1_048_576);
        let sample = u32_monotonic_postings();
        let kind = d.pick_with_size_hint(&sample, Some(8 * 1024 * 1024)).await;
        assert_eq!(kind, CodecKind::NvcompZstd);
    }

    #[tokio::test]
    async fn gpu_pref_columnar_respects_size_threshold() {
        // Columnar payload but under the gpu_min_bytes threshold →
        // GPU upload overhead would exceed the compress gain, stay
        // on CpuZstd. The Bitcomp branch must not bypass the size
        // gate.
        let d = SamplingDispatcher::new(CodecKind::CpuZstd)
            .with_gpu_preference(true, 1_048_576)
            .with_columnar_gpu_preference(true);
        let sample = u32_monotonic_postings();
        let kind = d.pick_with_size_hint(&sample, Some(100 * 1024)).await;
        assert_eq!(kind, CodecKind::CpuZstd);
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
