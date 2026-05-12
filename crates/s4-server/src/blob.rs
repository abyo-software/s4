//! `s3s::dto::StreamingBlob` と `bytes::Bytes` の相互変換ヘルパ。
//!
//! Phase 1 の方針: PUT body 全体を一旦 memory に集めてから圧縮する。streaming-aware な
//! chunk 圧縮 (Phase 2 で取り組む) に比べると memory cost が高いが、roundtrip 検証と
//! manifest 生成の単純さを優先。max_bytes で受け取れる最大サイズを上限化する。

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use s3s::dto::StreamingBlob;
use s3s::stream::ByteStream;

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
pub fn bytes_to_blob(bytes: Bytes) -> StreamingBlob {
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(bytes) });
    StreamingBlob::wrap(stream)
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("body exceeded configured limit ({limit} bytes); saw at least {seen_at_least}")]
    Oversized { limit: usize, seen_at_least: usize },
    #[error("error reading streaming body: {0}")]
    Read(String),
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
