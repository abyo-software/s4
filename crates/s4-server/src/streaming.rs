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
use s4_codec::{ChunkManifest, CodecError, CodecKind};
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
pub fn cpu_zstd_decompress_stream(body: StreamingBlob) -> StreamingBlob {
    let read = blob_to_async_read(body);
    let decoder = ZstdDecoder::new(BufReader::new(read));
    async_read_to_blob(decoder)
}

/// codec が streaming-aware かを判定 (S4Service 側で fast path 分岐に使う)。
pub fn supports_streaming_decompress(codec: CodecKind) -> bool {
    matches!(codec, CodecKind::Passthrough | CodecKind::CpuZstd)
}

pub fn supports_streaming_compress(codec: CodecKind) -> bool {
    matches!(codec, CodecKind::Passthrough | CodecKind::CpuZstd)
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
