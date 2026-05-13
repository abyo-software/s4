//! Streaming compression / decompression helpers。
//!
//! `s4_codec::Codec` の bytes-in / bytes-out API は memory cap (5 GiB) を持つため
//! 大規模オブジェクトで OOM 危険。本 module は `async-compression` 経由で zstd を
//! AsyncRead/AsyncWrite に差し込み、`StreamingBlob` (= `futures::Stream<Bytes>`)
//! ↔ AsyncRead を `tokio_util::io` で橋渡しする。
//!
//! ## 対応 codec
//!
//! - **CpuZstd**: `async_compression::tokio::bufread::ZstdDecoder` で完全 streaming
//! - **Passthrough**: 入力 stream をそのまま返す (ゼロコスト streaming)
//! - **NvcompZstd / NvcompBitcomp**: nvCOMP は batch API のため per-chunk batch 処理
//!   (Phase 2.1 で追加予定、現状は default の bytes-based に fallback)
//!
//! ## 整合性検証
//!
//! Streaming GET では bytes 全体の CRC32C をオンザフライで計算しつつ stream を
//! 流す `Crc32cVerifier` adapter を被せる。最後の chunk が yield された時点で
//! manifest.crc32c と比較し、不一致なら error として伝播 (= client 側で
//! body parse 失敗として現れる)。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::Level;
use async_compression::tokio::bufread::ZstdDecoder;
use async_compression::tokio::write::ZstdEncoder;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use s3s::StdError;
use s3s::dto::StreamingBlob;
use s3s::stream::{ByteStream, RemainingLength};
use s4_codec::multipart::{FrameHeader, write_frame};
use s4_codec::{ChunkManifest, CodecError, CodecKind, CodecRegistry};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf};
use tokio_util::io::{ReaderStream, StreamReader};

/// `StreamingBlob` を AsyncRead として扱えるラッパ。
///
/// `s3s::dto::StreamingBlob` は `futures::Stream<Item = Result<Bytes, StdError>>`
/// なので、`tokio_util::io::StreamReader` を使うと `tokio::io::AsyncRead` に変換できる。
/// ただし StreamReader は `std::io::Error` を期待するので、StdError → io::Error への
/// 変換層を挟む必要がある。
pub fn blob_to_async_read(
    blob: StreamingBlob,
) -> impl AsyncRead + Unpin + Send + Sync + 'static {
    let mapped = blob.map(|chunk| chunk.map_err(|e| io::Error::other(e.to_string())));
    StreamReader::new(mapped)
}

/// `AsyncRead` を 1 chunk = 64 KiB の `StreamingBlob` に変換 (size 不明の chunked stream)。
pub fn async_read_to_blob<R: AsyncRead + Unpin + Send + Sync + 'static>(
    reader: R,
) -> StreamingBlob {
    let stream = ReaderStream::new(reader).map(|res| res.map_err(|e| Box::new(e) as StdError));
    StreamingBlob::new(StreamWrapper { inner: stream })
}

pin_project_lite::pin_project! {
    /// Stream<Item=Result<Bytes, StdError>> に ByteStream impl を生やすラッパ。
    /// remaining_length は unknown を返す (streaming = size 未知)。
    struct StreamWrapper<S> { #[pin] inner: S }
}

impl<S> Stream for StreamWrapper<S>
where
    S: Stream<Item = Result<Bytes, StdError>>,
{
    type Item = Result<Bytes, StdError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S> ByteStream for StreamWrapper<S>
where
    S: Stream<Item = Result<Bytes, StdError>> + Send + Sync,
{
    fn remaining_length(&self) -> RemainingLength {
        // streaming output: size unknown
        RemainingLength::unknown()
    }
}

/// CpuZstd で `body` を **streaming** に decompress した `StreamingBlob` を返す。
///
/// memory peak は zstd window size + chunk size 程度 (typically <10 MiB)。
/// TTFB は最初の chunk が decompress された時点で client に渡る。
///
/// **multi-frame 対応**: zstd 仕様で「複数 frame の連結 = 1 つの valid な zstd
/// stream」と定義されているため、`streaming_compress_cpu_zstd` のような per-chunk
/// 圧縮された連結出力もそのまま decode 可能。`async_compression` の default は
/// single-frame のため、明示的に `multiple_members(true)` を有効化。
pub fn cpu_zstd_decompress_stream(body: StreamingBlob) -> StreamingBlob {
    let read = blob_to_async_read(body);
    let mut decoder = ZstdDecoder::new(BufReader::new(read));
    decoder.multiple_members(true);
    async_read_to_blob(decoder)
}

/// v0.8.4 #73 H-1: streaming GET integrity guard. Wraps an inner `AsyncRead`
/// (typically the output of [`cpu_zstd_decompress_stream`]) and computes a
/// rolling CRC32C as bytes flow through. On EOF the rolling CRC and the
/// observed byte count are compared against the manifest-declared values; a
/// mismatch surfaces as `io::ErrorKind::InvalidData` so the HTTP body stream
/// fails — the client sees a truncated / aborted response rather than silent
/// corruption.
///
/// The wrapper is **bytes-pass-through**: the entire payload reaches the
/// client as soon as each chunk is produced (no buffering of the plaintext),
/// preserving the streaming TTFB property that the unwrapped CpuZstd path
/// already has. The integrity decision lands at EOF, which on a corrupted
/// body shows up as a streaming error tail (HTTP/1.1 chunked: an aborted
/// final chunk; HTTP/2: RST_STREAM with INTERNAL_ERROR).
///
/// Why a custom wrapper instead of, say, a `tokio_util` adapter: the rolling
/// CRC needs both the per-chunk bytes (to fold into the running checksum)
/// and the EOF signal (to issue the final compare); the existing wrappers
/// in `tokio-util` (`StreamReader`, `InspectReader`) only expose pre-EOF
/// byte hooks and would require a separate end-of-stream reactor.
pub struct Crc32cVerifyingReader<R> {
    inner: R,
    expected_crc: u32,
    expected_size: u64,
    rolling_crc: u32,
    bytes_read: u64,
    /// Once a verify-failure has been emitted we keep returning EOF on
    /// subsequent polls so callers that don't immediately stop after the
    /// error don't get a fresh CRC value (which would be the rolling CRC
    /// from a partial stream — meaningless after the failure was reported).
    failed: bool,
}

impl<R> Crc32cVerifyingReader<R> {
    pub fn new(inner: R, expected_crc: u32, expected_size: u64) -> Self {
        Self {
            inner,
            expected_crc,
            expected_size,
            rolling_crc: 0,
            bytes_read: 0,
            failed: false,
        }
    }

    /// Test-only inspection of the rolling CRC at the current point in the
    /// stream. Useful from unit tests that drive the reader manually.
    #[cfg(test)]
    pub fn rolling_crc(&self) -> u32 {
        self.rolling_crc
    }

    #[cfg(test)]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

impl<R> AsyncRead for Crc32cVerifyingReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.failed {
            // Once we've reported the corruption, behave like a closed
            // stream — no further bytes, no further error. (Re-issuing
            // the error on every poll would also be defensible; we pick
            // "EOF after error" so callers that loop on `Ok(0)` cleanly
            // exit instead of spinning on `Err`.)
            return Poll::Ready(Ok(()));
        }
        let pre_filled = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                let new_filled = buf.filled().len();
                if new_filled > pre_filled {
                    let chunk = &buf.filled()[pre_filled..new_filled];
                    self.rolling_crc = crc32c::crc32c_append(self.rolling_crc, chunk);
                    self.bytes_read = self.bytes_read.saturating_add(chunk.len() as u64);
                    Poll::Ready(Ok(()))
                } else {
                    // EOF — verify both invariants. Size mismatch comes
                    // first because a short stream often signals the same
                    // root cause (truncation) more clearly than the CRC
                    // mismatch derived from partial bytes.
                    if self.bytes_read != self.expected_size {
                        self.failed = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "S4 streaming GET size mismatch: \
                                 expected {} bytes, got {}",
                                self.expected_size, self.bytes_read
                            ),
                        )));
                    }
                    if self.rolling_crc != self.expected_crc {
                        self.failed = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "S4 streaming GET crc32c mismatch: \
                                 expected {:#010x}, got {:#010x}",
                                self.expected_crc, self.rolling_crc
                            ),
                        )));
                    }
                    Poll::Ready(Ok(()))
                }
            }
        }
    }
}

/// codec が streaming-aware かを判定 (S4Service 側で fast path 分岐に使う)。
pub fn supports_streaming_decompress(codec: CodecKind) -> bool {
    // NvcompZstd の出力は zstd frame の連結 (zstd 仕様で valid な single stream) なので
    // CPU zstd decoder で stream decompress 可能。NvcompBitcomp は per-chunk metadata が
    // 別形式なので未対応 (HLIF self-describing 化を待つ)。
    matches!(
        codec,
        CodecKind::Passthrough | CodecKind::CpuZstd | CodecKind::NvcompZstd
    )
}

pub fn supports_streaming_compress(codec: CodecKind) -> bool {
    #[cfg(feature = "nvcomp-gpu")]
    {
        matches!(
            codec,
            CodecKind::Passthrough | CodecKind::CpuZstd | CodecKind::NvcompZstd
        )
    }
    #[cfg(not(feature = "nvcomp-gpu"))]
    {
        matches!(codec, CodecKind::Passthrough | CodecKind::CpuZstd)
    }
}

/// `body` を CPU zstd で **input-streaming** 圧縮し、(compressed bytes, manifest)
/// を返す。memory peak は zstd window + 64 KiB read buffer + 圧縮済 output ≈
/// `compressed_size + ~100 MB`。**入力 5 GB を 100 MB に圧縮するケースで peak は
/// ~200 MB** (vs. naive bytes-buffered だと peak 5 GB)。
///
/// 圧縮済 output 全体を Bytes として保持する理由は backend (aws-sdk-s3) が
/// chunked-without-content-length を SigV4 chunked encoding interceptor で reject
/// するため。Phase 2.1 で SigV4 streaming mode に対応すれば true streaming PUT
/// (compressed output も streaming) に拡張可。
pub async fn streaming_compress_cpu_zstd(
    body: StreamingBlob,
    level: i32,
) -> Result<(Bytes, ChunkManifest), CodecError> {
    let mut read = blob_to_async_read(body);
    let mut compressed_buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut crc: u32 = 0;
    let mut total_in: u64 = 0;
    let mut in_buf = vec![0u8; 64 * 1024];

    {
        let mut encoder = ZstdEncoder::with_quality(&mut compressed_buf, Level::Precise(level));
        loop {
            let n = read.read(&mut in_buf).await.map_err(CodecError::Io)?;
            if n == 0 {
                break;
            }
            crc = crc32c::crc32c_append(crc, &in_buf[..n]);
            total_in += n as u64;
            encoder
                .write_all(&in_buf[..n])
                .await
                .map_err(CodecError::Io)?;
        }
        encoder.shutdown().await.map_err(CodecError::Io)?;
    }

    let compressed_len = compressed_buf.len() as u64;
    Ok((
        Bytes::from(compressed_buf),
        ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: total_in,
            compressed_size: compressed_len,
            crc32c: crc,
        },
    ))
}

/// `streaming_compress_to_frames` の default chunk size。
///
/// 4 MiB を選んだ根拠:
/// - Range GET 1 件の最小帯域 ~= compressed_size_per_chunk (~数百 KB-1 MB)
/// - chunk 数が現実的 (1 GiB object → 256 frames、sidecar < 10 KB)
/// - CPU/GPU codec の per-call overhead が amortized
pub const DEFAULT_S4F2_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// v0.4 #16: pick a chunk size for `streaming_compress_to_frames` based on
/// the request's `Content-Length` (when known). Smaller objects get smaller
/// chunks (avoids carrying multi-MiB framing infrastructure on a 64 KiB
/// upload); large objects get larger chunks (amortises GPU launch overhead
/// and keeps the sidecar small).
///
/// Thresholds:
/// - `None` (chunked transfer-encoding, size unknown): default 4 MiB
/// - `<= 1 MiB`:           1 MiB (single chunk for small uploads)
/// - `1 MiB ..= 100 MiB`:  4 MiB (the v0.2 #4 default; balanced)
/// - `> 100 MiB`:          16 MiB (fewer frames → less sidecar / GPU overhead)
pub fn pick_chunk_size(content_length: Option<u64>) -> usize {
    match content_length {
        None => DEFAULT_S4F2_CHUNK_SIZE,
        Some(len) if len <= 1024 * 1024 => 1024 * 1024,
        Some(len) if len <= 100 * 1024 * 1024 => DEFAULT_S4F2_CHUNK_SIZE,
        Some(_) => 16 * 1024 * 1024,
    }
}

/// `streaming_compress_to_frames` の default in-flight depth (v0.3 #12)。
///
/// 同時に走らせる per-chunk compress task の数。chunk K-1 の compress 中に
/// chunk K の host-side read + crc 計算 + spawn を走らせ、完了したら順次
/// frame を書き出す pipeline で、CPU codec / GPU codec 両方で大物入力の
/// total throughput を 2-4× 改善 (issue #12 acceptance)。
///
/// 3 を選んだ根拠:
/// - 1 (= sequential) より明確に速い、4+ にしても reader / writer が
///   bottleneck で improvement diminishing
/// - host RAM peak が `N * chunk_size + accumulating output` で予測可能
///   (3 × 4 MiB = 12 MiB の input buffering vs 1 chunk = 4 MiB)
pub const DEFAULT_S4F2_INFLIGHT: usize = 3;

/// 入力 `body` を **chunked + framed + pipelined** に圧縮した output を返す
/// (v0.2 #4 + v0.3 #12)。
///
/// 各 chunk を `registry.compress(chunk_kind)` に投げ、最大
/// [`DEFAULT_S4F2_INFLIGHT`] 件まで in-flight に保つ。 結果は元の chunk 順を
/// 保持して S4F2 frame として連結。
///
/// **wire format**: `[S4F2 frame][S4F2 frame]...[S4F2 frame]` の連結。
/// 各 frame は self-describing なので reader 側は `multipart::FrameIter` で
/// そのまま parse 可能 (= 既存 multipart decompress 経路と同じ機構)。
///
/// **why chunked single-PUT**:
/// - Range GET partial-fetch を sidecar で活用可能 (issue #4)
/// - per-frame CRC で局所 corruption を検出可能
/// - per-frame codec dispatch (将来 mixed-codec 対応)
///
/// **memory peak**: `inflight × chunk_size` (in-flight chunks の input/output) +
/// `compressed_size` (output buffer accumulating)。
/// 入力 5 GB を 200 MB に圧縮する case で peak ≈ 12 MiB + 200 MB = ~212 MB
/// (vs sequential `chunk_size + compressed_size` = ~204 MB)。
/// `expected_size`: v0.8.4 #73 M2 — when the caller knows how many input
/// bytes the body is supposed to deliver (e.g. an HTTP `Content-Length`),
/// pass `Some(n)` and the function fails fast with [`CodecError::TruncatedStream`]
/// if the input stream returns EOF before `n` bytes were consumed. Without
/// this guard, a mid-chunk client disconnect would silently turn into a
/// "successful" PUT of a truncated payload — the rolling CRC would be
/// computed against the partial input and the GET would happily return the
/// truncated body. Pass `None` for chunked Transfer-Encoding requests where
/// the size is genuinely unknown (the upstream backend will still reject a
/// short chunked body via its own framing).
pub async fn streaming_compress_to_frames(
    body: StreamingBlob,
    registry: Arc<CodecRegistry>,
    codec_kind: CodecKind,
    chunk_size: usize,
    expected_size: Option<u64>,
) -> Result<(Bytes, ChunkManifest), CodecError> {
    streaming_compress_to_frames_with(
        body,
        registry,
        codec_kind,
        chunk_size,
        DEFAULT_S4F2_INFLIGHT,
        expected_size,
    )
    .await
}

/// Like [`streaming_compress_to_frames`] but lets callers tune the in-flight
/// depth — useful in the bench harness, and as the building block any
/// `streaming_compress_to_frames` callers extend if their workload needs a
/// non-default pipelining depth.
pub async fn streaming_compress_to_frames_with(
    body: StreamingBlob,
    registry: Arc<CodecRegistry>,
    codec_kind: CodecKind,
    chunk_size: usize,
    inflight: usize,
    expected_size: Option<u64>,
) -> Result<(Bytes, ChunkManifest), CodecError> {
    use bytes::BytesMut;
    use futures::StreamExt as _;
    use futures::stream::FuturesOrdered;

    let inflight = inflight.max(1);
    let mut read = blob_to_async_read(body);
    let mut framed = BytesMut::with_capacity(chunk_size);
    let mut rolling_crc: u32 = 0;
    let mut total_in: u64 = 0;
    let mut chunk_buf = vec![0u8; chunk_size];

    // Each in-flight task carries the per-chunk frame header (computed
    // synchronously when the chunk was read) and a JoinHandle that resolves
    // to the codec output. Ordering is preserved by FuturesOrdered.
    type InFlight = futures::future::BoxFuture<'static, Result<(FrameHeader, Bytes), CodecError>>;
    let mut queue: FuturesOrdered<InFlight> = FuturesOrdered::new();
    let mut eof = false;

    loop {
        // Refill the in-flight queue.
        while !eof && queue.len() < inflight {
            let mut filled = 0;
            while filled < chunk_size {
                let n = read
                    .read(&mut chunk_buf[filled..])
                    .await
                    .map_err(CodecError::Io)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                eof = true;
                break;
            }

            let chunk_slice = &chunk_buf[..filled];
            let chunk_crc = crc32c::crc32c(chunk_slice);
            rolling_crc = crc32c::crc32c_append(rolling_crc, chunk_slice);
            total_in += filled as u64;

            let header = FrameHeader {
                codec: codec_kind,
                original_size: filled as u64,
                compressed_size: 0, // patched after compress completes
                crc32c: chunk_crc,
            };
            let original_chunk = Bytes::copy_from_slice(chunk_slice);
            let registry = Arc::clone(&registry);
            queue.push_back(Box::pin(async move {
                let (compressed_chunk, _per_chunk_manifest) =
                    registry.compress(original_chunk, codec_kind).await?;
                let mut header = header;
                header.compressed_size = compressed_chunk.len() as u64;
                Ok::<_, CodecError>((header, compressed_chunk))
            }));
        }

        // Drain the next ready frame in chunk order.
        match queue.next().await {
            Some(Ok((header, compressed_chunk))) => {
                write_frame(&mut framed, header, &compressed_chunk);
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }

    // v0.8.4 #73 M2: truncation guard. We're about to declare the produced
    // bytes as the canonical compressed object — if the caller advertised a
    // Content-Length and we got fewer bytes (mid-chunk client disconnect,
    // half-uploaded body, etc.), surface the truncation NOW so the caller
    // can return 400 to the client. Without this branch, the rolling CRC
    // would be computed against the partial input, the manifest would
    // look internally consistent, and a future GET would happily return
    // the truncated body — silent data loss.
    if let Some(expected) = expected_size
        && total_in < expected
    {
        return Err(CodecError::TruncatedStream {
            expected,
            got: total_in,
        });
    }

    let total_framed = framed.len() as u64;
    Ok((
        framed.freeze(),
        ChunkManifest {
            codec: codec_kind,
            original_size: total_in,
            compressed_size: total_framed,
            crc32c: rolling_crc,
        },
    ))
}

/// `body` を passthrough で集めるだけ。CRC32C も計算する。
pub async fn streaming_passthrough(
    body: StreamingBlob,
) -> Result<(Bytes, ChunkManifest), CodecError> {
    let mut read = blob_to_async_read(body);
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut crc: u32 = 0;
    let mut total: u64 = 0;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = read.read(&mut chunk).await.map_err(CodecError::Io)?;
        if n == 0 {
            break;
        }
        crc = crc32c::crc32c_append(crc, &chunk[..n]);
        total += n as u64;
        buf.extend_from_slice(&chunk[..n]);
    }
    let len = buf.len() as u64;
    Ok((
        Bytes::from(buf),
        ChunkManifest {
            codec: CodecKind::Passthrough,
            original_size: total,
            compressed_size: len,
            crc32c: crc,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use futures::stream;
    use futures::stream::StreamExt;

    /// v0.4 #16: pick_chunk_size threshold table.
    #[test]
    fn pick_chunk_size_thresholds() {
        // None (chunked transfer-encoding) → default 4 MiB
        assert_eq!(pick_chunk_size(None), DEFAULT_S4F2_CHUNK_SIZE);
        // <= 1 MiB → 1 MiB
        assert_eq!(pick_chunk_size(Some(0)), 1024 * 1024);
        assert_eq!(pick_chunk_size(Some(64 * 1024)), 1024 * 1024);
        assert_eq!(pick_chunk_size(Some(1024 * 1024)), 1024 * 1024);
        // 1 MiB ..= 100 MiB → 4 MiB (default)
        assert_eq!(
            pick_chunk_size(Some(1024 * 1024 + 1)),
            DEFAULT_S4F2_CHUNK_SIZE
        );
        assert_eq!(
            pick_chunk_size(Some(50 * 1024 * 1024)),
            DEFAULT_S4F2_CHUNK_SIZE
        );
        assert_eq!(
            pick_chunk_size(Some(100 * 1024 * 1024)),
            DEFAULT_S4F2_CHUNK_SIZE
        );
        // > 100 MiB → 16 MiB
        assert_eq!(
            pick_chunk_size(Some(100 * 1024 * 1024 + 1)),
            16 * 1024 * 1024
        );
        assert_eq!(
            pick_chunk_size(Some(10 * 1024 * 1024 * 1024)),
            16 * 1024 * 1024
        );
    }

    async fn collect(blob: StreamingBlob) -> Bytes {
        let mut buf = BytesMut::new();
        let mut s = blob;
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf.freeze()
    }

    fn make_blob(b: Bytes) -> StreamingBlob {
        let stream = stream::once(async move { Ok::<_, std::io::Error>(b) });
        StreamingBlob::wrap(stream)
    }

    #[tokio::test]
    async fn cpu_zstd_streaming_roundtrip_small() {
        let original = Bytes::from("the quick brown fox jumps over the lazy dog. ".repeat(100));
        let compressed = zstd::stream::encode_all(original.as_ref(), 3).unwrap();
        let blob = make_blob(Bytes::from(compressed));
        let out_blob = cpu_zstd_decompress_stream(blob);
        let out = collect(out_blob).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn cpu_zstd_streaming_handles_chunked_input() {
        let original = Bytes::from(vec![b'x'; 1_000_000]);
        let compressed = zstd::stream::encode_all(original.as_ref(), 3).unwrap();
        // Split compressed into many small chunks to stress the streaming decoder.
        let mut chunks = Vec::new();
        for chunk in compressed.chunks(1024) {
            chunks.push(Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)));
        }
        let in_stream = stream::iter(chunks);
        let blob = StreamingBlob::wrap(in_stream);
        let out_blob = cpu_zstd_decompress_stream(blob);
        let out = collect(out_blob).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn streaming_passes_through_for_passthrough() {
        let original = Bytes::from_static(b"hello");
        let blob = make_blob(original.clone());
        let out_blob = async_read_to_blob(blob_to_async_read(blob));
        let out = collect(out_blob).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn streaming_compress_then_decompress_roundtrip() {
        let original = Bytes::from(vec![b'q'; 200_000]);
        let blob = make_blob(original.clone());
        let (compressed, manifest) = streaming_compress_cpu_zstd(blob, 3).await.unwrap();
        assert!(
            compressed.len() < original.len() / 100,
            "should be highly compressible"
        );
        assert_eq!(manifest.codec, CodecKind::CpuZstd);
        assert_eq!(manifest.original_size, original.len() as u64);
        assert_eq!(manifest.compressed_size, compressed.len() as u64);
        // crc32c は all-in-one と一致する
        assert_eq!(manifest.crc32c, crc32c::crc32c(&original));

        // Decompress 経路で完全に元に戻る
        let decompressed_blob = cpu_zstd_decompress_stream(make_blob(compressed));
        let out = collect(decompressed_blob).await;
        assert_eq!(out, original);
    }

    /// Verifies that `cpu_zstd_decompress_stream` correctly handles
    /// multi-frame zstd streams (multi-call CPU encoder output produces
    /// concatenated valid zstd frames per RFC 8478). `multiple_members(true)`
    /// on the async_compression decoder is what makes this work.
    #[tokio::test]
    async fn concatenated_zstd_frames_are_a_single_valid_stream() {
        let chunk_a = Bytes::from(vec![b'a'; 50_000]);
        let chunk_b = Bytes::from(vec![b'b'; 50_000]);
        let chunk_c = Bytes::from(vec![b'c'; 50_000]);

        let frame_a = zstd::stream::encode_all(chunk_a.as_ref(), 3).unwrap();
        let frame_b = zstd::stream::encode_all(chunk_b.as_ref(), 3).unwrap();
        let frame_c = zstd::stream::encode_all(chunk_c.as_ref(), 3).unwrap();

        let mut concatenated: Vec<u8> = Vec::new();
        concatenated.extend_from_slice(&frame_a);
        concatenated.extend_from_slice(&frame_b);
        concatenated.extend_from_slice(&frame_c);

        let expected: Vec<u8> = chunk_a
            .iter()
            .chain(chunk_b.iter())
            .chain(chunk_c.iter())
            .copied()
            .collect();

        let blob = make_blob(Bytes::from(concatenated));
        let out_blob = cpu_zstd_decompress_stream(blob);
        let out = collect(out_blob).await;
        assert_eq!(out, Bytes::from(expected));
    }

    /// Validates the chunked pipeline shape (chunk size, CRC accumulation,
    /// manifest aggregation, roundtrip via streaming CPU zstd decoder) used
    /// by both `streaming_compress_cpu_zstd` and the GPU codec paths in
    /// `streaming_compress_to_frames`.
    #[tokio::test]
    async fn streaming_chunked_compress_pipeline_roundtrip() {
        // Use cpu zstd as a stand-in for the GPU codec to exercise the same
        // chunking / CRC / output-concat pipeline that the nvcomp path uses.
        // The nvcomp variant differs only in which codec processes each chunk.
        async fn streaming_compress_chunked_cpu_zstd(
            body: StreamingBlob,
            chunk_size: usize,
        ) -> Result<(Bytes, ChunkManifest), CodecError> {
            let mut read = blob_to_async_read(body);
            let mut compressed_buf: Vec<u8> = Vec::with_capacity(chunk_size / 2);
            let mut crc: u32 = 0;
            let mut total_in: u64 = 0;
            let mut chunk_buf = vec![0u8; chunk_size];
            loop {
                let mut filled = 0;
                while filled < chunk_size {
                    let n = read
                        .read(&mut chunk_buf[filled..])
                        .await
                        .map_err(CodecError::Io)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    break;
                }
                crc = crc32c::crc32c_append(crc, &chunk_buf[..filled]);
                total_in += filled as u64;
                let compressed_chunk =
                    zstd::stream::encode_all(&chunk_buf[..filled], 3).map_err(CodecError::Io)?;
                compressed_buf.extend_from_slice(&compressed_chunk);
            }
            let compressed_len = compressed_buf.len() as u64;
            Ok((
                Bytes::from(compressed_buf),
                ChunkManifest {
                    codec: CodecKind::CpuZstd,
                    original_size: total_in,
                    compressed_size: compressed_len,
                    crc32c: crc,
                },
            ))
        }

        // 256 KiB input split into 8 chunks of 32 KiB.
        let original = Bytes::from(
            (0u32..65_536)
                .flat_map(|n| n.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        assert_eq!(original.len(), 262_144);

        let blob = make_blob(original.clone());
        let (compressed, manifest) = streaming_compress_chunked_cpu_zstd(blob, 32 * 1024)
            .await
            .unwrap();

        assert_eq!(manifest.original_size, original.len() as u64);
        assert_eq!(manifest.compressed_size, compressed.len() as u64);
        assert_eq!(manifest.crc32c, crc32c::crc32c(&original));

        // Decompress via the streaming CPU decoder (same path the GET handler uses).
        let decompressed_blob = cpu_zstd_decompress_stream(make_blob(compressed));
        let out = collect(decompressed_blob).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn streaming_passthrough_yields_input_unchanged() {
        let original = Bytes::from_static(b"hello world");
        let (out, manifest) = streaming_passthrough(make_blob(original.clone()))
            .await
            .unwrap();
        assert_eq!(out, original);
        assert_eq!(manifest.codec, CodecKind::Passthrough);
        assert_eq!(manifest.original_size, original.len() as u64);
        assert_eq!(manifest.compressed_size, original.len() as u64);
        assert_eq!(manifest.crc32c, crc32c::crc32c(&original));
    }

    // =================================================================
    // v0.8.4 #73 H-1 + M2 unit coverage.
    // =================================================================

    /// v0.8.4 #73 H-1: a verifier wrapped around a clean stream must
    /// emit exactly the inner bytes and report success at EOF.
    #[tokio::test]
    async fn crc32c_verifying_reader_passes_correct_crc() {
        use tokio::io::AsyncReadExt as _;
        let original = Bytes::from(vec![0xa3u8; 17_000]);
        let crc = crc32c::crc32c(&original);
        let inner = blob_to_async_read(make_blob(original.clone()));
        let mut verifier = Crc32cVerifyingReader::new(inner, crc, original.len() as u64);
        let mut out = Vec::new();
        verifier
            .read_to_end(&mut out)
            .await
            .expect("clean stream must read cleanly");
        assert_eq!(out, original.as_ref());
        // Post-condition: rolling CRC + bytes_read agree with manifest.
        assert_eq!(verifier.rolling_crc(), crc);
        assert_eq!(verifier.bytes_read(), original.len() as u64);
    }

    /// v0.8.4 #73 H-1: a verifier that sees corruption (rolling CRC
    /// differs from the manifest's CRC at EOF) must surface an
    /// `InvalidData` io error to the consumer instead of returning the
    /// bytes silently. This is the streaming-GET integrity guarantee.
    #[tokio::test]
    async fn crc32c_verifying_reader_detects_corruption() {
        use tokio::io::AsyncReadExt as _;
        let original = Bytes::from_static(b"clean payload bytes");
        let real_crc = crc32c::crc32c(&original);
        // Wrap the *same* bytes but tell the verifier to expect a
        // *different* CRC — equivalent to the upstream having tampered
        // with the body (or a back-end corruption that the zstd decoder
        // happened to silently decode into different bytes).
        let bogus_expected_crc = real_crc.wrapping_add(1);
        let inner = blob_to_async_read(make_blob(original.clone()));
        let mut verifier =
            Crc32cVerifyingReader::new(inner, bogus_expected_crc, original.len() as u64);
        let mut out = Vec::new();
        let err = verifier
            .read_to_end(&mut out)
            .await
            .expect_err("CRC mismatch must surface as io::Error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(
            msg.contains("crc32c mismatch"),
            "error must mention CRC mismatch, got `{msg}`"
        );
        // The bytes ARE delivered before the EOF verify (streaming is
        // pass-through); the integrity decision lands at EOF.
        assert_eq!(out, original.as_ref());
    }

    /// v0.8.4 #73 M2: `streaming_compress_to_frames` must reject a body
    /// whose stream produces fewer bytes than `expected_size` (mid-PUT
    /// truncation / client disconnect) with `TruncatedStream`.
    #[tokio::test]
    async fn streaming_compress_truncated_input_returns_truncated_stream_error() {
        use s4_codec::cpu_zstd::CpuZstd;
        let registry = Arc::new(
            CodecRegistry::new(CodecKind::CpuZstd)
                .with(Arc::new(CpuZstd::default())),
        );
        // The synthetic body yields exactly 4 KiB but the caller
        // *advertises* 16 KiB — the same shape as a client that
        // disconnected after 25% of the upload.
        let actual = Bytes::from(vec![b'z'; 4096]);
        let advertised: u64 = 16 * 1024;
        let blob = make_blob(actual.clone());
        let err = streaming_compress_to_frames(
            blob,
            registry,
            CodecKind::CpuZstd,
            1024,
            Some(advertised),
        )
        .await
        .expect_err("truncated stream must error");
        match err {
            CodecError::TruncatedStream { expected, got } => {
                assert_eq!(expected, advertised);
                assert_eq!(got, actual.len() as u64);
            }
            other => panic!("expected TruncatedStream, got {other:?}"),
        }
    }
}
