//! #148: incremental S4F2 frame processing over a streaming body.
//!
//! Multipart objects (and framed-v2 single-PUTs) are `[S4F2 frame]
//! ([S4P1 padding][S4F2 frame])*` sequences. Before this module, every
//! consumer materialized the WHOLE compressed body (`collect_blob`) and
//! then the WHOLE decompressed output (`decompress_multipart`) — one
//! GET of a 2 GiB object OOM-killed a 2 Gi-limit gateway pod (live
//! repro, 2026-07-08 Metered Savings E2E).
//!
//! The GET-side consumer here is [`multipart_decompress_blob`]:
//! decompress a framed body frame-by-frame, emitting each frame's
//! plaintext as soon as it is decoded — O(one frame) memory. With a
//! `range`, frames left of the range are SKIPPED (payload bytes
//! discarded, never decoded), frames right of it terminate the stream
//! early. The Complete-side index scan lives in `service.rs`
//! (`scan_index_via_frame_hops`): O(parts) small ranged reads, because
//! a full-body streaming scan runs while the CLIENT connection is idle
//! and would outlive the #149 idle guard on large objects (found on
//! the 2026-07-10 EKS live verify). It reuses [`parse_header_bytes`].
//!
//! Per-frame integrity is unchanged: each frame's payload goes through
//! `CodecRegistry::decompress` with the header's `ChunkManifest`, so the
//! per-frame crc32c verification the buffered path relied on still runs.

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use s3s::dto::StreamingBlob;
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FRAME_MAGIC, FrameHeader, PADDING_HEADER_BYTES, PADDING_MAGIC, read_frame,
};
use s4_codec::{ChunkManifest, CodecRegistry};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::streaming::blob_to_async_read;

/// Incremental reader over an `AsyncRead` with the three primitives the
/// frame walk needs: exact reads, bulk skips, and clean-EOF detection.
struct FrameReader<R> {
    inner: R,
    /// Absolute offset of the next unread byte (= compressed cursor).
    offset: u64,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, offset: 0 }
    }

    /// Read exactly `n` bytes. `Ok(None)` = the stream ended CLEANLY
    /// before the first byte (frame-boundary EOF); a partial read is an
    /// `UnexpectedEof` error (truncated frame).
    async fn read_exact_or_eof(&mut self, n: usize) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; n];
        let mut filled = 0usize;
        while filled < n {
            let read = self.inner.read(&mut buf[filled..]).await?;
            if read == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("truncated frame: needed {n} bytes, stream ended after {filled}"),
                ));
            }
            filled += read;
        }
        self.offset += n as u64;
        Ok(Some(buf))
    }

    /// Discard exactly `n` bytes (payloads of skipped/padding frames).
    async fn skip(&mut self, n: u64) -> io::Result<()> {
        let mut sink = tokio::io::sink();
        let copied = tokio::io::copy(&mut (&mut self.inner).take(n), &mut sink).await?;
        if copied != n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("truncated frame payload: wanted to skip {n} bytes, got {copied}"),
            ));
        }
        self.offset += n;
        Ok(())
    }
}

/// One step of the frame walk.
enum WalkStep {
    /// Clean EOF at a frame boundary.
    Eof,
    /// A data frame header. The payload has NOT been consumed yet —
    /// the reader's cursor sits at the payload's first byte.
    Frame { header: FrameHeader },
}

/// Advance past any padding frames and parse the next data-frame header
/// (payload left unconsumed). io::Error on malformed / truncated input.
async fn next_data_frame<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
) -> io::Result<WalkStep> {
    loop {
        let frame_offset = reader.offset;
        let Some(magic) = reader.read_exact_or_eof(4).await? else {
            return Ok(WalkStep::Eof);
        };
        if magic.as_slice() == PADDING_MAGIC {
            let len_bytes = reader
                .read_exact_or_eof(PADDING_HEADER_BYTES - 4)
                .await?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "truncated padding header")
                })?;
            let pad_len = u64::from_le_bytes(len_bytes.as_slice().try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "padding header length slice")
            })?);
            reader.skip(pad_len).await?;
            continue;
        }
        if magic.as_slice() != FRAME_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad frame magic at offset {frame_offset}: {magic:02x?}"),
            ));
        }
        let rest = reader
            .read_exact_or_eof(FRAME_HEADER_BYTES - 4)
            .await?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame header")
            })?;
        // Re-assemble the full header and reuse the codec's canonical
        // parser (codec-id validation, 32-bit-size hardening).
        let mut full = Vec::with_capacity(FRAME_HEADER_BYTES);
        full.extend_from_slice(&magic);
        full.extend_from_slice(&rest);
        // `read_frame` also wants the payload present to split it off —
        // feed it a zero-payload view and parse only the header fields.
        let header = parse_header_bytes(&full)?;
        return Ok(WalkStep::Frame { header });
    }
}

/// Parse the 28 header bytes via the codec crate's `read_frame` (payload
/// checks disabled by declaring the remainder empty is not possible, so
/// parse fields with the same layout/validation semantics). Also used
/// by the Complete-side frame-hop scanner in `service.rs`.
pub(crate) fn parse_header_bytes(full: &[u8]) -> io::Result<FrameHeader> {
    debug_assert_eq!(full.len(), FRAME_HEADER_BYTES);
    // Delegate to `read_frame` with an empty-payload copy so the codec
    // crate stays the single source of truth for the header layout: a
    // zero-length payload only fails the `compressed_size > remaining`
    // check, so temporarily rewrite compressed_size to 0 for the parse,
    // then restore the real value from the raw bytes.
    let mut probe = full.to_vec();
    // compressed_size is the u64 LE at offset 4 (magic) + 4 (codec) + 8
    // (original_size) = 16..24.
    let real_compressed = u64::from_le_bytes(
        probe[16..24]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame header size slice"))?,
    );
    probe[16..24].copy_from_slice(&0u64.to_le_bytes());
    let (mut header, _payload, _rest) = read_frame(Bytes::from(probe)).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame header parse: {e}"),
        )
    })?;
    header.compressed_size = real_compressed;
    Ok(header)
}

/// Frame-by-frame streaming decompression of a framed body.
///
/// - `range: None` — the whole object, in frame-sized chunks.
/// - `range: Some((start, end_exclusive))` — original-byte coordinates;
///   frames wholly before `start` are skipped without decoding, frames
///   at/after `end_exclusive` terminate the stream early, covering
///   frames are decoded and sliced.
/// - `expected_total` — the declared logical size (from the
///   `s4-original-size` stamp / sidecar). A CLEAN frame-boundary EOF
///   short of it is reported as an error instead of ending the body —
///   without this, a backend stream that dies exactly between frames
///   would truncate a full GET silently. `None` = unknown (legacy
///   unstamped objects; the client's only guard is Content-Length,
///   which is also unknown then — documented limitation).
///
/// `per_frame_cap` bounds a single frame's DECLARED sizes (compressed
/// and original) — the memory high-water mark is one frame, so the cap
/// protects against a forged header pinning arbitrary memory, while the
/// aggregate output is unbounded (that is the point of streaming).
///
/// Implementation: the walk runs in a spawned task feeding a BOUNDED
/// (depth 2) channel — the codec registry's decompress future is not
/// `Sync`, so it cannot live inside a `StreamingBlob::wrap`ped stream
/// directly. The bounded channel gives end-to-end backpressure (the
/// task decodes at most 2 frames ahead of the client) and the task
/// exits as soon as the receiving body is dropped (send fails); the
/// receiver additionally aborts the task on drop.
pub(crate) fn multipart_decompress_blob(
    blob: StreamingBlob,
    registry: Arc<CodecRegistry>,
    range: Option<(u64, u64)>,
    expected_total: Option<u64>,
    per_frame_cap: usize,
) -> StreamingBlob {
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(2);
    let task = tokio::spawn(async move {
        let result = walk_and_send(
            blob,
            registry,
            range,
            expected_total,
            per_frame_cap as u64,
            &tx,
        )
        .await;
        if let Err(e) = result {
            // Receiver gone = client disconnected; nothing to report.
            let _ = tx.send(Err(e)).await;
        }
    });
    StreamingBlob::wrap(ChannelStream { rx, task })
}

/// The frame walk driving [`multipart_decompress_blob`]'s channel.
/// Returns `Ok(())` on clean EOF (or early range completion / receiver
/// drop); `Err` on malformed / truncated input or decode failure.
async fn walk_and_send(
    blob: StreamingBlob,
    registry: Arc<CodecRegistry>,
    range: Option<(u64, u64)>,
    expected_total: Option<u64>,
    per_frame_cap: u64,
    tx: &tokio::sync::mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let mut reader = FrameReader::new(blob_to_async_read(blob));
    let mut original_cursor: u64 = 0;
    loop {
        let step = next_data_frame(&mut reader).await?;
        let WalkStep::Frame { header } = step else {
            // Clean EOF. Running out of frames before the range end (or
            // the declared total) means the body is shorter than what
            // the request was resolved against — treat as corruption
            // rather than ending the body early.
            if let Some((_, end)) = range
                && original_cursor < end
            {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "framed body ended at original offset {original_cursor} \
                         before range end {end}"
                    ),
                ));
            }
            if range.is_none()
                && let Some(total) = expected_total
                && original_cursor != total
            {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "framed body decoded to {original_cursor} bytes but the \
                         object declares {total} (truncated or stale stamp)"
                    ),
                ));
            }
            return Ok(());
        };
        if header.compressed_size > per_frame_cap || header.original_size > per_frame_cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame exceeds per-frame cap {per_frame_cap}: compressed {}, original {}",
                    header.compressed_size, header.original_size
                ),
            ));
        }
        let frame_start = original_cursor;
        let frame_end = frame_start.saturating_add(header.original_size);
        if let Some((start, end)) = range {
            if frame_end <= start {
                // Wholly before the range: skip without decoding.
                reader.skip(header.compressed_size).await?;
                original_cursor = frame_end;
                continue;
            }
            if frame_start >= end {
                // Past the range: stop reading (drop the rest).
                return Ok(());
            }
        }
        let payload = reader
            .read_exact_or_eof(header.compressed_size as usize)
            .await?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame payload")
            })?;
        let manifest = ChunkManifest {
            codec: header.codec,
            original_size: header.original_size,
            compressed_size: header.compressed_size,
            crc32c: header.crc32c,
        };
        let decoded = registry
            .decompress(Bytes::from(payload), &manifest)
            .await
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("multipart frame decompress: {e}"),
                )
            })?;
        original_cursor = frame_start.saturating_add(decoded.len() as u64);
        let (out, finished) = match range {
            Some((start, end)) => {
                let lo = start.saturating_sub(frame_start).min(decoded.len() as u64);
                let hi = (end - frame_start).min(decoded.len() as u64);
                (
                    decoded.slice(lo as usize..hi as usize),
                    original_cursor >= end,
                )
            }
            None => (decoded, false),
        };
        // Skip zero-length slices (empty frames) rather than emitting
        // empty chunks.
        if !out.is_empty() && tx.send(Ok(out)).await.is_err() {
            // Client dropped the response body — stop reading.
            return Ok(());
        }
        if finished {
            return Ok(());
        }
    }
}

/// `Stream` over the walker task's channel. `Receiver::poll_recv` keeps
/// the type `Sync` (no non-`Sync` decode future is held across polls),
/// which `StreamingBlob::wrap` requires. Dropping the stream aborts the
/// walker so an abandoned response body cannot leave a task reading the
/// backend indefinitely.
struct ChannelStream {
    rx: tokio::sync::mpsc::Receiver<io::Result<Bytes>>,
    task: tokio::task::JoinHandle<()>,
}

impl futures::Stream for ChannelStream {
    type Item = io::Result<Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for ChannelStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use bytes::BytesMut;
    use s4_codec::CodecKind;
    use s4_codec::cpu_zstd::CpuZstd;
    use s4_codec::multipart::{pad_to_minimum, write_frame};
    use s4_codec::passthrough::Passthrough;

    fn registry() -> Arc<CodecRegistry> {
        Arc::new(
            CodecRegistry::new(CodecKind::CpuZstd)
                .with(Arc::new(Passthrough))
                .with(Arc::new(CpuZstd::default())),
        )
    }

    /// Build a framed body of `parts` (each padded to `pad_to`), returning
    /// (framed body, concatenated original bytes).
    async fn framed_body(parts: &[Vec<u8>], pad_to: usize) -> (Bytes, Vec<u8>) {
        let reg = registry();
        let mut body = BytesMut::new();
        let mut original = Vec::new();
        for part in parts {
            let (compressed, manifest) = reg
                .compress(Bytes::from(part.clone()), CodecKind::CpuZstd)
                .await
                .unwrap();
            let mut framed = BytesMut::new();
            write_frame(
                &mut framed,
                FrameHeader {
                    codec: CodecKind::CpuZstd,
                    original_size: part.len() as u64,
                    compressed_size: compressed.len() as u64,
                    crc32c: manifest.crc32c,
                },
                &compressed,
            );
            if pad_to > 0 {
                pad_to_minimum(&mut framed, pad_to);
            }
            body.extend_from_slice(&framed);
            original.extend_from_slice(part);
        }
        (body.freeze(), original)
    }

    /// Wrap bytes as a StreamingBlob delivered in `chunk` -sized pieces —
    /// exercises every header/payload split across chunk boundaries.
    fn chunked_blob(bytes: Bytes, chunk: usize) -> StreamingBlob {
        let chunks: Vec<Result<Bytes, io::Error>> = bytes
            .chunks(chunk)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        StreamingBlob::wrap(futures::stream::iter(chunks))
    }

    async fn collect_stream(blob: StreamingBlob) -> io::Result<Vec<u8>> {
        use futures::TryStreamExt as _;
        let mut out = Vec::new();
        let mut s = std::pin::pin!(blob);
        while let Some(chunk) = s.try_next().await.map_err(io::Error::other)? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn decompress_stream_roundtrips_full_body() {
        let parts = vec![vec![b'a'; 300_000], vec![b'b'; 200_000], vec![b'c'; 123]];
        let (body, original) = framed_body(&parts, 128 * 1024).await;
        for chunk in [3, 1000, body.len()] {
            let blob = multipart_decompress_blob(
                chunked_blob(body.clone(), chunk),
                registry(),
                None,
                Some(original.len() as u64),
                8 * 1024 * 1024,
            );
            let got = collect_stream(blob).await.unwrap();
            assert_eq!(got, original, "chunk={chunk}");
        }
    }

    #[tokio::test]
    async fn decompress_stream_range_skips_and_slices() {
        let parts = vec![
            vec![b'a'; 300_000],
            vec![b'b'; 200_000],
            vec![b'c'; 100_000],
        ];
        let (body, original) = framed_body(&parts, 64 * 1024).await;
        // Range spanning the a/b boundary, plus a b-only range, plus a
        // tail range ending exactly at EOF.
        let cases = [
            (299_990u64, 300_010u64),
            (300_000, 500_000),
            (550_000, 600_000),
            (0, 1),
        ];
        for (start, end) in cases {
            let blob = multipart_decompress_blob(
                chunked_blob(body.clone(), 4096),
                registry(),
                Some((start, end)),
                None,
                8 * 1024 * 1024,
            );
            let got = collect_stream(blob).await.unwrap();
            assert_eq!(
                got,
                &original[start as usize..end as usize],
                "range {start}..{end}"
            );
        }
    }

    #[tokio::test]
    async fn decompress_stream_errors_on_truncated_body() {
        let (body, _original) = framed_body(&[vec![b'a'; 100_000]], 0).await;
        let truncated = body.slice(..body.len() - 10);
        let blob = multipart_decompress_blob(
            chunked_blob(truncated, 4096),
            registry(),
            None,
            None,
            1 << 20,
        );
        let err = collect_stream(blob).await.expect_err("must error");
        assert_eq!(err.kind(), io::ErrorKind::Other, "wrapped io error: {err}");
    }

    /// A backend stream that dies exactly at a frame boundary is a
    /// CLEAN EOF to the walker — the declared-total check is what turns
    /// it into a loud error instead of a silently short body.
    #[tokio::test]
    async fn decompress_stream_errors_on_frame_boundary_truncation() {
        let parts = vec![vec![b'a'; 100_000], vec![b'b'; 50_000]];
        let (body, original) = framed_body(&parts, 0).await;
        // Cut the body exactly at the frame-1/frame-2 boundary.
        let reference = s4_codec::index::build_index_from_body(&body).unwrap();
        let boundary = reference.entries[1].compressed_offset as usize;
        let cut = body.slice(..boundary);
        let blob = multipart_decompress_blob(
            chunked_blob(cut, 4096),
            registry(),
            None,
            Some(original.len() as u64),
            1 << 24,
        );
        let err = collect_stream(blob)
            .await
            .expect_err("frame-boundary truncation must error when the total is declared");
        assert!(err.to_string().contains("declares"), "{err}");
    }

    #[tokio::test]
    async fn decompress_stream_enforces_per_frame_cap() {
        let (body, _original) = framed_body(&[vec![b'a'; 100_000]], 0).await;
        let blob =
            multipart_decompress_blob(chunked_blob(body, 4096), registry(), None, None, 1024);
        let err = collect_stream(blob)
            .await
            .expect_err("oversized frame must be rejected");
        assert!(err.to_string().contains("per-frame cap"), "{err}");
    }
}
