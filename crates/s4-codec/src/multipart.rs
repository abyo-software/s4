//! Multipart upload で使う on-the-wire フレーム形式。
//!
//! ## 課題
//!
//! AWS S3 multipart upload は各 part を独立にアップロードし、CompleteMultipartUpload
//! で順番に concat した bytes が最終 object になる。S4 が per-part で圧縮すると、
//! 最終 object は **N 個の圧縮済 chunk の concat**。GET 時に「どこからどこまでが
//! 1 chunk か」を知るためのメタが必要だが、object metadata には全 chunk の境界を
//! 入れる容量がない (S3 metadata 上限 2 KB、1000 parts × 8 byte = 8 KB で溢れる)。
//!
//! ## 解決策: in-band frame header
//!
//! 各 part bytes の先頭に固定 24 byte のフレームヘッダを置き、続く `compressed_size`
//! バイトが圧縮済 payload。GET は object 全体を読み込み、先頭から frame を順に
//! parse し各 chunk を解凍 → 連結する。
//!
//! ```text
//! ┌──────────────────────────── 24 bytes ────────────────────────────┐
//! │ magic    │ orig_size │ compressed_size │ crc32c │   ── then payload ──
//! │ "S4F1"   │  u64 LE   │     u64 LE      │ u32 LE │ [compressed_size bytes]
//! └──────────┴───────────┴─────────────────┴────────┘
//! ```
//!
//! - codec は object metadata の `s4-codec` で **全 part 共通** (CreateMultipartUpload
//!   で固定)。Phase 2 で per-frame codec 化を検討可。
//! - object metadata に `s4-multipart=true` を立てておき、GET 側はそれを見て frame
//!   parse を有効化する。
//!
//! ## 制限事項 (Phase 1)
//!
//! - **Range GET 非対応**: chunk 境界と byte offset の対応を計算しないので、
//!   client が Range を指定しても無視 (もしくは下流の Range を尊重して invalid
//!   解凍になる) — 実装上は Range を S4 で reject する方が安全。Phase 2 で対応。
//! - **per-part 別 codec 非対応**: 上記 frame format に codec ID を入れるか、
//!   object metadata を per-part に拡張するかの判断は Phase 2 で。

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

/// Frame magic = ASCII "S4F1" (S4 Frame, version 1).
pub const FRAME_MAGIC: &[u8; 4] = b"S4F1";
/// Padding frame magic = ASCII "S4P1" (S4 Padding, version 1)。
///
/// AWS S3 は multipart の non-final part に min 5 MB 制約を課すが、S4 が圧縮すると
/// part が 5 MB を下回ることが多発する (圧縮率 10-100x で 5 MB が 50 KB-500 KB)。
/// その場合 `write_padded_frame` が compressed payload の後ろに `[S4P1][len:u64]
/// [len bytes of zeros]` を書き込んで全体を S3 の最小サイズまで膨らませる。
/// `FrameIter` は padding を skip するので decode 側は意識不要。
pub const PADDING_MAGIC: &[u8; 4] = b"S4P1";
pub const FRAME_HEADER_BYTES: usize = 4 + 8 + 8 + 4; // = 24
pub const PADDING_HEADER_BYTES: usize = 4 + 8; // = 12

/// AWS S3 の non-final multipart part の最小サイズ (5 MiB)。
pub const S3_MULTIPART_MIN_PART_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub original_size: u64,
    pub compressed_size: u64,
    pub crc32c: u32,
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame too short: need at least {FRAME_HEADER_BYTES} bytes, have {0}")]
    TooShort(usize),
    #[error("bad frame magic: expected {expected:?}, got {got:?}")]
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    #[error("frame compressed_size {compressed_size} exceeds remaining buffer {remaining}")]
    PayloadTruncated {
        compressed_size: u64,
        remaining: usize,
    },
}

/// 1 フレーム分を直列化: header + payload を `dst` に追記。
pub fn write_frame(dst: &mut BytesMut, header: FrameHeader, payload: &[u8]) {
    debug_assert_eq!(payload.len() as u64, header.compressed_size);
    dst.reserve(FRAME_HEADER_BYTES + payload.len());
    dst.put_slice(FRAME_MAGIC);
    dst.put_u64_le(header.original_size);
    dst.put_u64_le(header.compressed_size);
    dst.put_u32_le(header.crc32c);
    dst.put_slice(payload);
}

/// `dst` の現在サイズが `min_total` byte を下回っていれば、padding frame を追記して
/// `min_total` byte を超えさせる。最終 `dst.len()` は `min_total + ε` (ε は
/// padding header 12 byte 分) を保証。
///
/// padding 自体の中身は zero bytes (compress も decompress も無し)。
pub fn pad_to_minimum(dst: &mut BytesMut, min_total: usize) {
    if dst.len() >= min_total {
        return;
    }
    // 残り = min_total - 現在 ですが、padding 自体に PADDING_HEADER_BYTES 必要。
    let need = min_total - dst.len();
    let payload_len = need.saturating_sub(PADDING_HEADER_BYTES);
    dst.reserve(PADDING_HEADER_BYTES + payload_len);
    dst.put_slice(PADDING_MAGIC);
    dst.put_u64_le(payload_len as u64);
    // zero-fill。`put_bytes` で 1 回 syscall。
    dst.put_bytes(0, payload_len);
}

/// `input` の先頭から 1 フレーム読み出し、`(header, payload, remainder)` を返す。
pub fn read_frame(mut input: Bytes) -> Result<(FrameHeader, Bytes, Bytes), FrameError> {
    if input.len() < FRAME_HEADER_BYTES {
        return Err(FrameError::TooShort(input.len()));
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&input[..4]);
    if &magic != FRAME_MAGIC {
        return Err(FrameError::BadMagic {
            expected: *FRAME_MAGIC,
            got: magic,
        });
    }
    input.advance(4);
    let original_size = input.get_u64_le();
    let compressed_size = input.get_u64_le();
    let crc32c = input.get_u32_le();
    if (compressed_size as usize) > input.len() {
        return Err(FrameError::PayloadTruncated {
            compressed_size,
            remaining: input.len(),
        });
    }
    let payload = input.split_to(compressed_size as usize);
    Ok((
        FrameHeader {
            original_size,
            compressed_size,
            crc32c,
        },
        payload,
        input,
    ))
}

/// `input` 全体を frame の sequence として parse、各 frame を yield する iterator。
///
/// `S4P1` (padding) を見つけたら header の length 分だけ skip して次に進む
/// (= caller には見せない)。
pub struct FrameIter {
    rest: Bytes,
}

impl FrameIter {
    pub fn new(input: Bytes) -> Self {
        Self { rest: input }
    }
}

impl Iterator for FrameIter {
    type Item = Result<(FrameHeader, Bytes), FrameError>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            if self.rest.len() < 4 {
                return Some(Err(FrameError::TooShort(self.rest.len())));
            }
            let mut magic = [0u8; 4];
            magic.copy_from_slice(&self.rest[..4]);
            if &magic == PADDING_MAGIC {
                // skip padding frame: 4 magic + 8 len + len bytes
                if self.rest.len() < PADDING_HEADER_BYTES {
                    return Some(Err(FrameError::TooShort(self.rest.len())));
                }
                self.rest.advance(4);
                let pad_len = self.rest.get_u64_le();
                if (pad_len as usize) > self.rest.len() {
                    return Some(Err(FrameError::PayloadTruncated {
                        compressed_size: pad_len,
                        remaining: self.rest.len(),
                    }));
                }
                self.rest.advance(pad_len as usize);
                continue;
            }
            // それ以外は data frame として parse
            return match read_frame(std::mem::take(&mut self.rest)) {
                Ok((hdr, payload, remainder)) => {
                    self.rest = remainder;
                    Some(Ok((hdr, payload)))
                }
                Err(e) => Some(Err(e)),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_single() {
        let payload = Bytes::from_static(b"hello frame payload");
        let header = FrameHeader {
            original_size: 999,
            compressed_size: payload.len() as u64,
            crc32c: 0xdead_beef,
        };
        let mut buf = BytesMut::new();
        write_frame(&mut buf, header, &payload);
        assert_eq!(buf.len(), FRAME_HEADER_BYTES + payload.len());
        let bytes = buf.freeze();
        let (got_header, got_payload, rest) = read_frame(bytes).unwrap();
        assert_eq!(got_header, header);
        assert_eq!(got_payload, payload);
        assert!(rest.is_empty());
    }

    #[test]
    fn frame_iter_walks_all_frames() {
        let mut buf = BytesMut::new();
        for i in 0..5u32 {
            let payload = vec![i as u8; (i as usize + 1) * 4];
            let h = FrameHeader {
                original_size: 100 + i as u64,
                compressed_size: payload.len() as u64,
                crc32c: i,
            };
            write_frame(&mut buf, h, &payload);
        }
        let total = FrameIter::new(buf.freeze())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(total.len(), 5);
        for (i, (h, payload)) in total.iter().enumerate() {
            assert_eq!(h.original_size, 100 + i as u64);
            assert_eq!(h.crc32c, i as u32);
            assert_eq!(payload.len(), (i + 1) * 4);
        }
    }

    #[test]
    fn frame_bad_magic_rejected() {
        let mut buf = BytesMut::with_capacity(FRAME_HEADER_BYTES);
        buf.put_slice(b"BAD!");
        buf.put_u64_le(0);
        buf.put_u64_le(0);
        buf.put_u32_le(0);
        let err = read_frame(buf.freeze()).unwrap_err();
        assert!(matches!(err, FrameError::BadMagic { .. }));
    }

    #[test]
    fn frame_truncated_rejected() {
        // header says 100 bytes payload, but we provide 0
        let mut buf = BytesMut::with_capacity(FRAME_HEADER_BYTES);
        buf.put_slice(FRAME_MAGIC);
        buf.put_u64_le(100);
        buf.put_u64_le(100);
        buf.put_u32_le(0);
        let err = read_frame(buf.freeze()).unwrap_err();
        assert!(matches!(err, FrameError::PayloadTruncated { .. }));
    }

    #[test]
    fn frame_too_short_for_header_rejected() {
        let buf = Bytes::from_static(b"shortdata");
        let err = read_frame(buf).unwrap_err();
        assert!(matches!(err, FrameError::TooShort(_)));
    }

    #[test]
    fn padding_skipped_by_iter() {
        let mut buf = BytesMut::new();
        // frame 1: small data
        let p1 = Bytes::from_static(b"first frame");
        write_frame(
            &mut buf,
            FrameHeader {
                original_size: 11,
                compressed_size: p1.len() as u64,
                crc32c: 0,
            },
            &p1,
        );
        // pad to 1024 bytes (well above min)
        pad_to_minimum(&mut buf, 1024);
        assert!(buf.len() >= 1024);
        // frame 2: another small data
        let p2 = Bytes::from_static(b"second frame");
        write_frame(
            &mut buf,
            FrameHeader {
                original_size: 12,
                compressed_size: p2.len() as u64,
                crc32c: 0,
            },
            &p2,
        );

        let frames: Vec<_> = FrameIter::new(buf.freeze())
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            frames.len(),
            2,
            "padding must be skipped, only data yielded"
        );
        assert_eq!(frames[0].1, p1);
        assert_eq!(frames[1].1, p2);
    }

    #[test]
    fn pad_to_minimum_is_noop_when_already_above() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0u8; 1024]);
        pad_to_minimum(&mut buf, 100);
        assert_eq!(buf.len(), 1024);
    }

    #[test]
    fn pad_to_minimum_grows_to_target() {
        let mut buf = BytesMut::new();
        write_frame(
            &mut buf,
            FrameHeader {
                original_size: 0,
                compressed_size: 0,
                crc32c: 0,
            },
            &[],
        );
        let before = buf.len();
        pad_to_minimum(&mut buf, 5_000_000);
        assert!(buf.len() >= 5_000_000);
        assert!(buf.len() < 5_000_000 + 64, "no excessive overshoot");
        assert!(buf.len() > before);
    }
}
