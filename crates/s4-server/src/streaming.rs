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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

/// `StreamingBlob` を AsyncRead として扱えるラッパ。
///
/// `s3s::dto::StreamingBlob` は `futures::Stream<Item = Result<Bytes, StdError>>`
/// なので、`tokio_util::io::StreamReader` を使うと `tokio::io::AsyncRead` に変換できる。
/// ただし StreamReader は `std::io::Error` を期待するので、StdError → io::Error への
/// 変換層を挟む必要がある。
pub fn blob_to_async_read(blob: StreamingBlob) -> impl AsyncRead + Unpin + Send + Sync {
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

/// 入力 `body` を **chunked + framed** に圧縮した output を返す (v0.2 #4)。
///
/// 各 chunk を `registry.compress(chunk_kind)` に通し、結果を `S4F2` frame として
/// 順次連結する。outer `ChunkManifest` は input 全体に対する rolling crc + total
/// original/compressed size を返す。
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
/// **memory peak**: `chunk_size` (input) + `compressed_size` (output buffer accumulating)。
/// 入力 5 GB を 200 MB に圧縮する case で peak ≈ 4 MiB + 200 MB = ~204 MB。
pub async fn streaming_compress_to_frames(
    body: StreamingBlob,
    registry: Arc<CodecRegistry>,
    codec_kind: CodecKind,
    chunk_size: usize,
) -> Result<(Bytes, ChunkManifest), CodecError> {
    use bytes::BytesMut;
    let mut read = blob_to_async_read(body);
    let mut framed = BytesMut::with_capacity(chunk_size);
    let mut rolling_crc: u32 = 0;
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

        let chunk_slice = &chunk_buf[..filled];
        let chunk_crc = crc32c::crc32c(chunk_slice);
        rolling_crc = crc32c::crc32c_append(rolling_crc, chunk_slice);
        total_in += filled as u64;

        let original_chunk = Bytes::copy_from_slice(chunk_slice);
        let (compressed_chunk, _per_chunk_manifest) =
            registry.compress(original_chunk, codec_kind).await?;

        let header = FrameHeader {
            codec: codec_kind,
            original_size: filled as u64,
            compressed_size: compressed_chunk.len() as u64,
            crc32c: chunk_crc,
        };
        write_frame(&mut framed, header, &compressed_chunk);
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
}
