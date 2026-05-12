//! Frame index — Range GET の partial fetch を可能にするための sidecar object 形式。
//!
//! ## 課題
//!
//! S4-multipart object は `[S4F2 frame]([S4P1 padding][S4F2 frame])*` のシーケンス。
//! Range GET (e.g. `bytes=N-M`) を効率的に処理するには、(a) どの frame が
//! decompressed offset N..M に対応しているか、(b) その frame は object body の
//! どこ (compressed_offset) から始まるか、を知る必要がある。
//!
//! ## 解決策
//!
//! `<key>.s4index` という sidecar object に下記の binary index を書く:
//!
//! ```text
//! ┌──── 32 byte header ────┐
//! │ S4IX magic (4)         │
//! │ version u32 (4)        │
//! │ total_frames u64 (8)   │
//! │ total_original u64 (8) │
//! │ total_padded u64 (8)   │  ← S3 上の object サイズ (padding 含む)
//! └────────────────────────┘
//! 各 frame について 32 byte:
//!   original_offset  u64 LE
//!   original_size    u64 LE
//!   compressed_offset u64 LE  ← S3 object body における frame header の開始位置
//!   compressed_size  u64 LE   ← header (28 byte) + payload の合計
//! ```
//!
//! 1000 frame で 32 KB、10000 frame で 320 KB。10 万 frame でも 3.2 MB に収まる。
//!
//! ## 使い方
//!
//! - PUT: 1 frame の単純 index、PUT 完了後に sidecar 書込
//! - CompleteMultipartUpload: object 全体を一度 fetch + scan して index を構築
//! - Range GET: sidecar fetch → `lookup_range(start, end)` で frame 範囲 + S3 byte 範囲を取得
//!   → backend に partial Range GET → frame parse → decompress → slice

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const INDEX_MAGIC: &[u8; 4] = b"S4IX";
pub const INDEX_VERSION: u32 = 1;
pub const INDEX_HEADER_BYTES: usize = 4 + 4 + 8 + 8 + 4 + 4 + 8; // 40 (with padding)
const HEADER_FIXED: usize = 4 + 4 + 8 + 8 + 8;
pub const ENTRY_BYTES: usize = 8 + 8 + 8 + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameIndexEntry {
    /// この frame が担当する decompressed byte 範囲の開始 (累計、0-based)
    pub original_offset: u64,
    /// 解凍後 byte 数 (frame header の original_size と同じ)
    pub original_size: u64,
    /// S3 object body 内での frame 開始位置 (S4F2 magic の先頭 byte)
    pub compressed_offset: u64,
    /// frame 全体のバイト数 (28 byte header + payload)
    pub compressed_size: u64,
}

impl FrameIndexEntry {
    pub fn original_end(&self) -> u64 {
        self.original_offset + self.original_size
    }
    pub fn compressed_end(&self) -> u64 {
        self.compressed_offset + self.compressed_size
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameIndex {
    /// S3 上の object 全体サイズ (padding frame 含む)
    pub total_padded_size: u64,
    pub entries: Vec<FrameIndexEntry>,
}

impl FrameIndex {
    pub fn total_original_size(&self) -> u64 {
        self.entries.last().map(|e| e.original_end()).unwrap_or(0)
    }

    /// Range request `[start, end_exclusive)` を解決して必要 frame の (start_idx, end_idx_exclusive)
    /// と S3 上の partial-fetch byte range `[byte_start, byte_end_exclusive)` を返す。
    ///
    /// 1 frame でもオーバーラップしていればその frame の **全 byte** を fetch する
    /// (= 部分 frame は decompress 単位)。
    pub fn lookup_range(&self, start: u64, end_exclusive: u64) -> Option<RangePlan> {
        if self.entries.is_empty() || start >= end_exclusive {
            return None;
        }
        let total = self.total_original_size();
        if start >= total {
            return None;
        }
        let clamped_end = end_exclusive.min(total);

        // start を含む frame を二分探索 (entries は original_offset 昇順)
        let first_idx = match self.entries.binary_search_by(|e| {
            if e.original_end() <= start {
                std::cmp::Ordering::Less
            } else if e.original_offset > start {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
            Ok(i) => i,
            Err(_) => return None,
        };
        // end を含む frame (end-1 を含むもの)
        let last_inclusive = clamped_end - 1;
        let last_idx = match self.entries.binary_search_by(|e| {
            if e.original_end() <= last_inclusive {
                std::cmp::Ordering::Less
            } else if e.original_offset > last_inclusive {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
            Ok(i) => i,
            Err(_) => return None,
        };

        let byte_start = self.entries[first_idx].compressed_offset;
        let byte_end_exclusive = self.entries[last_idx].compressed_end();
        Some(RangePlan {
            first_frame_idx: first_idx,
            last_frame_idx_inclusive: last_idx,
            byte_start,
            byte_end_exclusive,
            // slice 開始 / 終了の original 内 offset
            slice_start_in_combined: start - self.entries[first_idx].original_offset,
            slice_end_in_combined: clamped_end - self.entries[first_idx].original_offset,
        })
    }
}

/// `lookup_range` の結果。`byte_start..byte_end_exclusive` を S3 から fetch、
/// 該当 frames を decompress し、結果バイト列を `[slice_start_in_combined,
/// slice_end_in_combined)` で slice すれば最終結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangePlan {
    pub first_frame_idx: usize,
    pub last_frame_idx_inclusive: usize,
    pub byte_start: u64,
    pub byte_end_exclusive: u64,
    pub slice_start_in_combined: u64,
    pub slice_end_in_combined: u64,
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("index too short: {0} bytes")]
    TooShort(usize),
    #[error("bad index magic: {got:?}")]
    BadMagic { got: [u8; 4] },
    #[error("unsupported index version {0} (this build supports {INDEX_VERSION})")]
    UnsupportedVersion(u32),
    #[error("entry count {claimed} doesn't match buffer remaining {remaining}")]
    EntryCountMismatch { claimed: u64, remaining: usize },
}

pub fn encode_index(idx: &FrameIndex) -> Bytes {
    let mut buf = BytesMut::with_capacity(HEADER_FIXED + idx.entries.len() * ENTRY_BYTES);
    buf.put_slice(INDEX_MAGIC);
    buf.put_u32_le(INDEX_VERSION);
    buf.put_u64_le(idx.entries.len() as u64);
    buf.put_u64_le(idx.total_original_size());
    buf.put_u64_le(idx.total_padded_size);
    for e in &idx.entries {
        buf.put_u64_le(e.original_offset);
        buf.put_u64_le(e.original_size);
        buf.put_u64_le(e.compressed_offset);
        buf.put_u64_le(e.compressed_size);
    }
    buf.freeze()
}

pub fn decode_index(mut input: Bytes) -> Result<FrameIndex, IndexError> {
    if input.len() < HEADER_FIXED {
        return Err(IndexError::TooShort(input.len()));
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&input[..4]);
    if &magic != INDEX_MAGIC {
        return Err(IndexError::BadMagic { got: magic });
    }
    input.advance(4);
    let version = input.get_u32_le();
    if version != INDEX_VERSION {
        return Err(IndexError::UnsupportedVersion(version));
    }
    let n = input.get_u64_le();
    let _total_original = input.get_u64_le();
    let total_padded_size = input.get_u64_le();
    let expected_remaining = (n as usize).saturating_mul(ENTRY_BYTES);
    if input.len() != expected_remaining {
        return Err(IndexError::EntryCountMismatch {
            claimed: n,
            remaining: input.len(),
        });
    }
    let mut entries = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let original_offset = input.get_u64_le();
        let original_size = input.get_u64_le();
        let compressed_offset = input.get_u64_le();
        let compressed_size = input.get_u64_le();
        entries.push(FrameIndexEntry {
            original_offset,
            original_size,
            compressed_offset,
            compressed_size,
        });
    }
    Ok(FrameIndex {
        total_padded_size,
        entries,
    })
}

/// Object body の bytes 全体を scan して FrameIndex を構築する。
/// `multipart_e2e.rs` 等で full-scan path として使用。
pub fn build_index_from_body(body: &Bytes) -> Result<FrameIndex, crate::multipart::FrameError> {
    let mut entries = Vec::new();
    let mut original_off: u64 = 0;
    // FrameIter は padding を skip してしまうので、自前で位置追跡しながら parse する
    let mut cursor = 0usize;
    let mut iter_buf = body.clone();
    while cursor < body.len() {
        // padding magic を skip
        if cursor + 4 <= body.len() && &body[cursor..cursor + 4] == crate::multipart::PADDING_MAGIC
        {
            // PADDING_HEADER_BYTES = 4 magic + 8 length
            if cursor + crate::multipart::PADDING_HEADER_BYTES > body.len() {
                break;
            }
            let pad_len = u64::from_le_bytes(body[cursor + 4..cursor + 12].try_into().unwrap());
            cursor += crate::multipart::PADDING_HEADER_BYTES + pad_len as usize;
            iter_buf = body.slice(cursor..);
            continue;
        }
        // data frame
        if cursor + crate::multipart::FRAME_HEADER_BYTES > body.len() {
            break;
        }
        let (header, _payload, rest) = crate::multipart::read_frame(iter_buf.clone())?;
        let frame_total = crate::multipart::FRAME_HEADER_BYTES + header.compressed_size as usize;
        entries.push(FrameIndexEntry {
            original_offset: original_off,
            original_size: header.original_size,
            compressed_offset: cursor as u64,
            compressed_size: frame_total as u64,
        });
        original_off += header.original_size;
        cursor += frame_total;
        iter_buf = rest;
    }
    Ok(FrameIndex {
        total_padded_size: body.len() as u64,
        entries,
    })
}

/// `<key>` から sidecar key を生成。
pub fn sidecar_key(object_key: &str) -> String {
    format!("{object_key}.s4index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodecKind;
    use crate::multipart::{FrameHeader, pad_to_minimum, write_frame};

    fn sample_index() -> FrameIndex {
        FrameIndex {
            total_padded_size: 200,
            entries: vec![
                FrameIndexEntry {
                    original_offset: 0,
                    original_size: 100,
                    compressed_offset: 0,
                    compressed_size: 50,
                },
                FrameIndexEntry {
                    original_offset: 100,
                    original_size: 80,
                    compressed_offset: 60, // gap of 10 = padding
                    compressed_size: 40,
                },
                FrameIndexEntry {
                    original_offset: 180,
                    original_size: 50,
                    compressed_offset: 100,
                    compressed_size: 30,
                },
            ],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let idx = sample_index();
        let bytes = encode_index(&idx);
        let decoded = decode_index(bytes).unwrap();
        assert_eq!(decoded, idx);
    }

    #[test]
    fn lookup_range_within_single_frame() {
        let idx = sample_index();
        // 元 byte [10, 50) は frame 0 (original 0..100) の中
        let plan = idx.lookup_range(10, 50).unwrap();
        assert_eq!(plan.first_frame_idx, 0);
        assert_eq!(plan.last_frame_idx_inclusive, 0);
        assert_eq!(plan.byte_start, 0);
        assert_eq!(plan.byte_end_exclusive, 50); // frame 0 全体
        assert_eq!(plan.slice_start_in_combined, 10);
        assert_eq!(plan.slice_end_in_combined, 50);
    }

    #[test]
    fn lookup_range_spans_frames() {
        let idx = sample_index();
        // [50, 150) は frame 0 後半 + frame 1 前半
        let plan = idx.lookup_range(50, 150).unwrap();
        assert_eq!(plan.first_frame_idx, 0);
        assert_eq!(plan.last_frame_idx_inclusive, 1);
        assert_eq!(plan.byte_start, 0);
        assert_eq!(plan.byte_end_exclusive, 100); // frame 0 (0..50) + frame 1 (60..100)
        assert_eq!(plan.slice_start_in_combined, 50);
        assert_eq!(plan.slice_end_in_combined, 150);
    }

    #[test]
    fn lookup_range_at_end_clamps() {
        let idx = sample_index();
        // total original = 100 + 80 + 50 = 230、要求 200..1000 → 200..230 にクランプ
        let plan = idx.lookup_range(200, 1000).unwrap();
        assert_eq!(plan.first_frame_idx, 2);
        assert_eq!(plan.last_frame_idx_inclusive, 2);
        // frame 2 全体 (compressed_offset=100, size=30 → byte 100..130)
        assert_eq!(plan.byte_start, 100);
        assert_eq!(plan.byte_end_exclusive, 130);
    }

    #[test]
    fn lookup_range_out_of_bounds_returns_none() {
        let idx = sample_index();
        assert!(idx.lookup_range(500, 600).is_none());
    }

    #[test]
    fn build_index_from_real_body_skips_padding() {
        // 2 frame + 中間 padding を組んで、index が正しく構築されることを確認
        let mut buf = BytesMut::new();
        let p1 = Bytes::from_static(b"AAAA");
        write_frame(
            &mut buf,
            FrameHeader {
                codec: CodecKind::Passthrough,
                original_size: 100,
                compressed_size: p1.len() as u64,
                crc32c: 0,
            },
            &p1,
        );
        let frame1_end = buf.len();
        // pad to 5000 bytes
        pad_to_minimum(&mut buf, 5000);
        let pad_end = buf.len();
        let p2 = Bytes::from_static(b"BBBB");
        write_frame(
            &mut buf,
            FrameHeader {
                codec: CodecKind::Passthrough,
                original_size: 80,
                compressed_size: p2.len() as u64,
                crc32c: 0,
            },
            &p2,
        );

        let idx = build_index_from_body(&buf.freeze()).unwrap();
        assert_eq!(idx.entries.len(), 2);
        assert_eq!(idx.entries[0].original_offset, 0);
        assert_eq!(idx.entries[0].compressed_offset, 0);
        assert_eq!(idx.entries[0].original_size, 100);
        assert_eq!(idx.entries[0].compressed_size, frame1_end as u64);
        assert_eq!(idx.entries[1].original_offset, 100);
        assert_eq!(idx.entries[1].compressed_offset, pad_end as u64);
        assert_eq!(idx.entries[1].original_size, 80);
        assert_eq!(idx.total_original_size(), 180);
    }
}
