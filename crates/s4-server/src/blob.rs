//! `s3s::dto::StreamingBlob` と `bytes::Bytes` の相互変換ヘルパ。
//!
//! Phase 1 の方針: PUT body 全体を一旦 memory に集めてから圧縮する。streaming-aware な
//! chunk 圧縮 (Phase 2 で取り組む) に比べると memory cost が高いが、roundtrip 検証と
//! manifest 生成の単純さを優先。max_bytes で受け取れる最大サイズを上限化する。

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use s3s::StdError;
use s3s::dto::StreamingBlob;
use s3s::stream::{ByteStream, RemainingLength};

/// `StreamingBlob` を Bytes に collect。`max_bytes` を超えたら早期に Err。
pub async fn collect_blob(blob: StreamingBlob, max_bytes: usize) -> Result<Bytes, BlobError> {
    let hint = blob.remaining_length().exact().unwrap_or(0).min(max_bytes);
    let mut buf = BytesMut::with_capacity(hint);
    let mut stream = blob;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BlobError::Read(format!("{e}")))?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BlobError::Oversized {
                limit: max_bytes,
                seen_at_least: buf.len() + chunk.len(),
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// `Bytes` を 1 chunk の `StreamingBlob` に包む。
///
/// content-length を **既知** として返す ByteStream impl にすることが重要。
/// `StreamingBlob::wrap` (futures::Stream 越し) だと remaining_length が unknown
/// になり、aws-sdk-s3 が `AwsChunkedContentEncodingInterceptor` で
/// `UnsizedRequestBody` エラーを返す。
pub fn bytes_to_blob(bytes: Bytes) -> StreamingBlob {
    StreamingBlob::new(SingleChunkBlob(Some(bytes)))
}

/// 単一の `Bytes` を 1 度だけ yield して終わる `ByteStream`。
/// `remaining_length` を正確な byte 数として返すので、aws-sdk-s3 の chunked
/// signing path がそのまま動く。
struct SingleChunkBlob(Option<Bytes>);

impl Stream for SingleChunkBlob {
    type Item = Result<Bytes, StdError>;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.take().map(Ok))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            Some(_) => (1, Some(1)),
            None => (0, Some(0)),
        }
    }
}

impl ByteStream for SingleChunkBlob {
    fn remaining_length(&self) -> RemainingLength {
        match &self.0 {
            Some(b) => RemainingLength::new_exact(b.len()),
            None => RemainingLength::new_exact(0),
        }
    }
}

/// v1.0 stability: `#[non_exhaustive]` — new streaming-body failure
/// modes may be added in minor releases. Downstream callers must
/// include a `_ =>` arm when matching on this enum.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlobError {
    #[error("body exceeded configured limit ({limit} bytes); saw at least {seen_at_least}")]
    Oversized { limit: usize, seen_at_least: usize },
    #[error("error reading streaming body: {0}")]
    Read(String),
}

/// `blob` の先頭から最大 `sample_bytes` を読み出して `(sample, rest_stream)` に分ける。
/// `sample` は collected Bytes、`rest_stream` は残りの未消費 chunk ストリーム。
/// stream 全体が `sample_bytes` 未満ならば `rest_stream` は空。
pub async fn peek_sample(
    mut blob: StreamingBlob,
    sample_bytes: usize,
) -> Result<(Bytes, StreamingBlob), BlobError> {
    let mut sample = BytesMut::with_capacity(sample_bytes);
    let mut leftover: Option<Bytes> = None;
    while sample.len() < sample_bytes {
        match blob.next().await {
            Some(Ok(chunk)) => {
                let remaining = sample_bytes.saturating_sub(sample.len());
                if chunk.len() <= remaining {
                    sample.extend_from_slice(&chunk);
                } else {
                    sample.extend_from_slice(&chunk[..remaining]);
                    leftover = Some(chunk.slice(remaining..));
                    break;
                }
            }
            Some(Err(e)) => return Err(BlobError::Read(format!("{e}"))),
            None => break,
        }
    }
    let sample_bytes = sample.freeze();
    let rest = chain_leftover_with_blob(leftover, blob);
    Ok((sample_bytes, rest))
}

/// `peek_sample` で取り出した sample を rest stream の先頭に再 prepend して
/// 1 本のストリームに戻す。
pub fn chain_sample_with_rest(sample: Bytes, rest: StreamingBlob) -> StreamingBlob {
    let head = futures::stream::once(async move { Ok::<_, std::io::Error>(sample) });
    let tail = rest.map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    StreamingBlob::wrap(head.chain(tail))
}

fn chain_leftover_with_blob(leftover: Option<Bytes>, rest: StreamingBlob) -> StreamingBlob {
    match leftover {
        Some(b) => chain_sample_with_rest(b, rest),
        None => rest,
    }
}

/// `peek_sample` の結果を再度結合した上で全体を Bytes に collect。GPU codec 経路用。
pub async fn collect_with_sample(
    sample: Bytes,
    rest: StreamingBlob,
    max_bytes: usize,
) -> Result<Bytes, BlobError> {
    if sample.len() > max_bytes {
        return Err(BlobError::Oversized {
            limit: max_bytes,
            seen_at_least: sample.len(),
        });
    }
    let mut buf = BytesMut::with_capacity(sample.len() + 4096);
    buf.extend_from_slice(&sample);
    let mut stream = rest;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BlobError::Read(format!("{e}")))?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BlobError::Oversized {
                limit: max_bytes,
                seen_at_least: buf.len() + chunk.len(),
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collect_roundtrip() {
        let original = Bytes::from_static(b"hello squished s3");
        let blob = bytes_to_blob(original.clone());
        let collected = collect_blob(blob, 1024).await.unwrap();
        assert_eq!(collected, original);
    }

    #[tokio::test]
    async fn collect_rejects_oversized() {
        let big = Bytes::from(vec![0u8; 2048]);
        let blob = bytes_to_blob(big);
        let err = collect_blob(blob, 1024).await.unwrap_err();
        assert!(matches!(err, BlobError::Oversized { .. }));
    }
}
