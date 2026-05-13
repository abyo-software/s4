//! 複数 Codec を保持し、`CodecKind` ベースで dispatch するレジストリ。
//!
//! S4Service は単一 codec ではなく `Arc<CodecRegistry>` を持つことで、
//!
//! - PUT 時: dispatcher が選んだ `CodecKind` で `compress` を呼ぶ
//! - GET 時: object metadata から復元した manifest.codec で `decompress` を呼ぶ
//!
//! を可能にする。これによりひとつの S4 インスタンスが複数 codec の混在オブジェクトを
//! 透過的に扱えるようになり、Phase 1 で抱えていた「codec mismatch エラー」を解消する。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

use crate::{ChunkManifest, Codec, CodecError, CodecKind, CompressTelemetry, looks_like_oom};

/// v0.8 #55: which `CodecKind` values are GPU-backed, for the telemetry
/// path's `gpu_seconds: Some(...)` decision. Hard-coded here (not on
/// `CodecKind`) so adding a CPU codec doesn't accidentally flip it on,
/// and adding a GPU codec is a deliberate one-line edit reviewers can
/// catch in diff.
fn is_gpu_kind(kind: CodecKind) -> bool {
    matches!(
        kind,
        CodecKind::NvcompZstd
            | CodecKind::NvcompBitcomp
            | CodecKind::NvcompGans
            | CodecKind::NvcompGDeflate
            | CodecKind::DietGpuAns
    )
}

/// codec dispatch レジストリ。`Arc` 越しに S4Service / 複数タスクから共有する想定。
pub struct CodecRegistry {
    codecs: HashMap<CodecKind, Arc<dyn Codec>>,
    default: CodecKind,
}

impl CodecRegistry {
    /// `default` で指定した codec が PUT 時の codec として使われる
    /// (dispatcher が別 kind を選んだ場合は、その kind が登録されていれば優先)。
    pub fn new(default: CodecKind) -> Self {
        Self {
            codecs: HashMap::new(),
            default,
        }
    }

    /// codec を登録。同じ kind を 2 度登録すると後勝ち。
    pub fn register(&mut self, codec: Arc<dyn Codec>) -> &mut Self {
        self.codecs.insert(codec.kind(), codec);
        self
    }

    /// `register` の chain 用 builder
    #[must_use]
    pub fn with(mut self, codec: Arc<dyn Codec>) -> Self {
        self.register(codec);
        self
    }

    /// 登録済 kind 一覧
    pub fn kinds(&self) -> impl Iterator<Item = CodecKind> + '_ {
        self.codecs.keys().copied()
    }

    /// default kind
    pub fn default_kind(&self) -> CodecKind {
        self.default
    }

    fn lookup(&self, kind: CodecKind) -> Result<&Arc<dyn Codec>, CodecError> {
        self.codecs
            .get(&kind)
            .ok_or(CodecError::UnregisteredCodec(kind))
    }

    /// 指定 kind の codec で compress
    pub async fn compress(
        &self,
        input: Bytes,
        kind: CodecKind,
    ) -> Result<(Bytes, ChunkManifest), CodecError> {
        let codec = self.lookup(kind)?;
        codec.compress(input).await
    }

    /// manifest が指す codec で decompress (本命の dispatch path)
    pub async fn decompress(
        &self,
        input: Bytes,
        manifest: &ChunkManifest,
    ) -> Result<Bytes, CodecError> {
        let codec = self.lookup(manifest.codec)?;
        codec.decompress(input, manifest).await
    }

    /// v0.8 #55: same as [`Self::compress`] but additionally returns a
    /// [`CompressTelemetry`] describing the operation (codec name,
    /// input/output size, GPU wall-clock seconds for GPU codecs, OOM
    /// flag on failure). Lets `s4-server` stamp Prometheus metrics
    /// (`s4_gpu_compress_seconds`, `s4_gpu_throughput_bytes_per_sec`,
    /// `s4_gpu_oom_total`) without `s4-codec` itself depending on the
    /// `metrics` crate (callback / return-value pattern, keeps the
    /// codec dep tree slim).
    ///
    /// On `Ok`, telemetry has the measured `bytes_in` / `bytes_out` and
    /// `gpu_seconds = Some(secs)` for GPU kinds, `None` for CPU. On
    /// `Err`, telemetry has `bytes_in = input.len() as u64` and
    /// `bytes_out = 0`, with `oom = true` iff the error string matches
    /// the OOM heuristic ([`crate::looks_like_oom`]).
    pub async fn compress_with_telemetry(
        &self,
        input: Bytes,
        kind: CodecKind,
    ) -> (
        Result<(Bytes, ChunkManifest), CodecError>,
        CompressTelemetry,
    ) {
        let bytes_in = input.len() as u64;
        let codec = match self.lookup(kind) {
            Ok(c) => c,
            Err(e) => {
                let tel = CompressTelemetry {
                    codec: kind.as_str(),
                    bytes_in,
                    bytes_out: 0,
                    gpu_seconds: None,
                    oom: false,
                };
                return (Err(e), tel);
            }
        };
        let is_gpu = is_gpu_kind(kind);
        let started = Instant::now();
        let result = codec.compress(input).await;
        let elapsed = started.elapsed().as_secs_f64();
        match &result {
            Ok((out, _manifest)) => {
                let bytes_out = out.len() as u64;
                let tel = if is_gpu {
                    CompressTelemetry::gpu(kind.as_str(), bytes_in, bytes_out, elapsed)
                } else {
                    CompressTelemetry::cpu(kind.as_str(), bytes_in, bytes_out)
                };
                (result, tel)
            }
            Err(e) => {
                let mut tel = if is_gpu {
                    CompressTelemetry::gpu(kind.as_str(), bytes_in, 0, elapsed)
                } else {
                    CompressTelemetry::cpu(kind.as_str(), bytes_in, 0)
                };
                if looks_like_oom(e) {
                    tel = tel.with_oom();
                }
                (result, tel)
            }
        }
    }

    /// v0.8 #55: telemetry-returning decompress. Mirrors
    /// [`Self::compress_with_telemetry`] for the GET / decompress side
    /// so operators can dashboard GPU decompress p99 separately from
    /// the compress histogram.
    pub async fn decompress_with_telemetry(
        &self,
        input: Bytes,
        manifest: &ChunkManifest,
    ) -> (Result<Bytes, CodecError>, CompressTelemetry) {
        let bytes_in = input.len() as u64;
        let kind = manifest.codec;
        let codec = match self.lookup(kind) {
            Ok(c) => c,
            Err(e) => {
                let tel = CompressTelemetry {
                    codec: kind.as_str(),
                    bytes_in,
                    bytes_out: 0,
                    gpu_seconds: None,
                    oom: false,
                };
                return (Err(e), tel);
            }
        };
        let is_gpu = is_gpu_kind(kind);
        let started = Instant::now();
        let result = codec.decompress(input, manifest).await;
        let elapsed = started.elapsed().as_secs_f64();
        match &result {
            Ok(out) => {
                let bytes_out = out.len() as u64;
                let tel = if is_gpu {
                    CompressTelemetry::gpu(kind.as_str(), bytes_in, bytes_out, elapsed)
                } else {
                    CompressTelemetry::cpu(kind.as_str(), bytes_in, bytes_out)
                };
                (result, tel)
            }
            Err(e) => {
                let mut tel = if is_gpu {
                    CompressTelemetry::gpu(kind.as_str(), bytes_in, 0, elapsed)
                } else {
                    CompressTelemetry::cpu(kind.as_str(), bytes_in, 0)
                };
                if looks_like_oom(e) {
                    tel = tel.with_oom();
                }
                (result, tel)
            }
        }
    }
}

impl std::fmt::Debug for CodecRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut kinds: Vec<&CodecKind> = self.codecs.keys().collect();
        kinds.sort_unstable_by_key(|k| k.as_str());
        f.debug_struct("CodecRegistry")
            .field("default", &self.default)
            .field("registered", &kinds)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_zstd::CpuZstd;
    use crate::passthrough::Passthrough;

    fn registry() -> CodecRegistry {
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default()))
    }

    #[tokio::test]
    async fn dispatches_compress_by_kind() {
        let r = registry();
        let input = Bytes::from(vec![b'a'; 1024]);

        let (compressed_pt, manifest_pt) = r
            .compress(input.clone(), CodecKind::Passthrough)
            .await
            .unwrap();
        assert_eq!(manifest_pt.codec, CodecKind::Passthrough);
        assert_eq!(compressed_pt.len(), input.len());

        let (compressed_zstd, manifest_zstd) =
            r.compress(input.clone(), CodecKind::CpuZstd).await.unwrap();
        assert_eq!(manifest_zstd.codec, CodecKind::CpuZstd);
        assert!(compressed_zstd.len() < input.len() / 5);
    }

    #[tokio::test]
    async fn dispatches_decompress_by_manifest() {
        let r = registry();
        let input = Bytes::from(vec![b'a'; 1024]);
        let (compressed, manifest) = r.compress(input.clone(), CodecKind::CpuZstd).await.unwrap();
        let decompressed = r.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn unregistered_codec_yields_error() {
        let r = registry();
        let bogus_manifest = ChunkManifest {
            codec: CodecKind::NvcompBitcomp,
            original_size: 10,
            compressed_size: 10,
            crc32c: 0,
        };
        let err = r
            .decompress(Bytes::from_static(b"0123456789"), &bogus_manifest)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CodecError::UnregisteredCodec(CodecKind::NvcompBitcomp)
        ));
    }

    /// v0.8 #55: telemetry on a CPU codec should set `gpu_seconds = None`
    /// (no GPU-metric stamp at the call site) and report the correct
    /// input/output sizes, even though the timing measurement still runs.
    #[tokio::test]
    async fn compress_with_telemetry_cpu_marks_gpu_seconds_none() {
        let r = registry();
        let input = Bytes::from(vec![b'a'; 1024]);
        let (res, tel) = r
            .compress_with_telemetry(input.clone(), CodecKind::CpuZstd)
            .await;
        let (out, _manifest) = res.expect("compress ok");
        assert_eq!(tel.codec, "cpu-zstd");
        assert_eq!(tel.bytes_in, input.len() as u64);
        assert_eq!(tel.bytes_out, out.len() as u64);
        assert!(
            tel.gpu_seconds.is_none(),
            "CPU codec must report gpu_seconds=None, got {:?}",
            tel.gpu_seconds
        );
        assert!(!tel.oom);
    }

    /// v0.8 #55: telemetry on an unregistered codec should surface the
    /// `UnregisteredCodec` error AND a populated telemetry shell so the
    /// caller's stamp helper can still increment a generic `requests
    /// _total{result="err"}` if it wants to (no panic-on-error path).
    #[tokio::test]
    async fn compress_with_telemetry_unregistered_returns_telemetry() {
        let r = registry();
        let input = Bytes::from(vec![b'b'; 32]);
        let (res, tel) = r
            .compress_with_telemetry(input.clone(), CodecKind::NvcompBitcomp)
            .await;
        assert!(matches!(
            res,
            Err(CodecError::UnregisteredCodec(CodecKind::NvcompBitcomp))
        ));
        assert_eq!(tel.codec, "nvcomp-bitcomp");
        assert_eq!(tel.bytes_in, input.len() as u64);
        assert_eq!(tel.bytes_out, 0);
        assert!(tel.gpu_seconds.is_none());
        assert!(!tel.oom);
    }

    /// v0.8 #55: telemetry-returning decompress on a CPU codec round
    /// trips and reports the decompressed (output) byte count.
    #[tokio::test]
    async fn decompress_with_telemetry_cpu_reports_output_size() {
        let r = registry();
        let input = Bytes::from(vec![b'c'; 1024]);
        let (compressed, manifest) = r.compress(input.clone(), CodecKind::CpuZstd).await.unwrap();
        let (res, tel) = r
            .decompress_with_telemetry(compressed.clone(), &manifest)
            .await;
        let out = res.expect("decompress ok");
        assert_eq!(out, input);
        assert_eq!(tel.codec, "cpu-zstd");
        assert_eq!(tel.bytes_in, compressed.len() as u64);
        assert_eq!(tel.bytes_out, input.len() as u64);
        assert!(tel.gpu_seconds.is_none());
    }
}
