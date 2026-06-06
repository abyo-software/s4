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
//! ┌──── v1 32 byte header ─┐
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
//!
//! ## v0.8.4 #73 H-2: source object version binding (v2 header)
//!
//! v1 では sidecar に source object の identity が無いため、object overwrite 後に
//! sidecar が stale のままだと Range GET が **間違った frame** を返す危険があった
//! (古い byte offset で新 object を partial GET する hazard)。攻撃者が backend を
//! 直接触れる脅威モデルでは、偽 sidecar を仕込めば任意 frame を露呈させ得る。
//!
//! 対策として v2 header に `source_etag` と `source_compressed_size` を追加。GET
//! 側は HEAD で current etag を取って一致確認 → 不一致なら sidecar を信用せず full
//! GET path に fall back する。
//!
//! ```text
//! ┌──── v2 header (variable) ┐
//! │ S4IX magic (4)           │
//! │ version u32 (4) = 2      │
//! │ total_frames u64 (8)     │
//! │ total_original u64 (8)   │
//! │ total_padded u64 (8)     │
//! │ source_compressed_size u64 (8)  ← v2 で追加
//! │ etag_len u32 (4)                 ← v2 で追加 (UTF-8 byte length, 0 = absent)
//! │ etag bytes (etag_len)            ← v2 で追加 (RFC 7232 entity-tag, quotes 含む)
//! └──────────────────────────┘
//! ```
//!
//! - **back-compat**: v1 sidecar が backend に既存していれば read-only で `decode_index`
//!   が `source_etag = None`, `source_compressed_size = None` で復元する。GET 側は
//!   `None` を見たら "legacy sidecar — verify skip, full GET にも fallback できる"
//!   と扱う (= 既存挙動保持)。
//! - **新規 PUT**: 常に v2 を書く。`source_etag` は backend response の e_tag、
//!   `source_compressed_size` は put body 長 (= `total_padded_size`) が原則。

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const INDEX_MAGIC: &[u8; 4] = b"S4IX";
/// v0.8.4 #73 H-2: bumped 1 → 2. v2 appends `source_compressed_size` (u64) +
/// `etag_len` (u32) + variable-length `etag` bytes to the fixed header. v1
/// readers are kept as a back-compat path (see [`decode_index`]).
pub const INDEX_VERSION: u32 = 2;
/// Legacy v1 fixed header — kept for tests / back-compat readers.
pub const INDEX_VERSION_V1: u32 = 1;
/// v1 fixed header layout (kept for back-compat readers).
pub const HEADER_FIXED_V1: usize = 4 + 4 + 8 + 8 + 8; // 32
/// v2 fixed header layout (`HEADER_FIXED_V1` + `source_compressed_size` u64 +
/// `etag_len` u32). The variable-length `etag` payload follows.
pub const HEADER_FIXED_V2: usize = HEADER_FIXED_V1 + 8 + 4; // 44
/// v0.8.16 F-15: kept for back-compat with external consumers that
/// imported the v0.8.10-era constant. **DEPRECATED** — the value
/// `40` was a typo (it should have been `44` for the v2 fixed
/// header). Use [`HEADER_FIXED_V1`] / [`HEADER_FIXED_V2`] directly.
#[deprecated(
    since = "0.8.16",
    note = "INDEX_HEADER_BYTES was an off-by-4 typo; use HEADER_FIXED_V1 or HEADER_FIXED_V2 instead"
)]
pub const INDEX_HEADER_BYTES: usize = HEADER_FIXED_V2;
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
    /// v0.8.15 H-a: was plain `self.original_offset + self.original_size`,
    /// which panics in `dev` (workspace `overflow_checks = true`) and
    /// wraps in release on an attacker-supplied sidecar entry with
    /// `original_offset = u64::MAX - 10` and `original_size = 100`.
    /// `decode_index` now also pre-validates each entry below, so this
    /// `saturating_add` is defence-in-depth — a corrupted in-memory
    /// `FrameIndexEntry` cannot crash the gateway through `binary_search_by`.
    pub fn original_end(&self) -> u64 {
        self.original_offset.saturating_add(self.original_size)
    }
    pub fn compressed_end(&self) -> u64 {
        self.compressed_offset.saturating_add(self.compressed_size)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameIndex {
    /// S3 上の object 全体サイズ (padding frame 含む)
    pub total_padded_size: u64,
    pub entries: Vec<FrameIndexEntry>,
    /// v0.8.4 #73 H-2: backend-reported ETag of the source object the
    /// sidecar describes. Populated by `s4-server::put_object` from the
    /// backend's PUT response so the matching GET can `head_object` and
    /// confirm it's still talking about the same body. `None` for legacy
    /// (v1) sidecars decoded out of an existing backend, in which case
    /// the GET path treats the partial-fetch as best-effort and falls
    /// back to a full read on any inconsistency signal.
    pub source_etag: Option<String>,
    /// v0.8.4 #73 H-2: backend object's compressed bytes length the sidecar
    /// was computed against. Cross-check signal alongside `source_etag` —
    /// some backends (lifecycle moves, multi-object operations) can change
    /// the bytes without a fresh ETag, so a size mismatch is independently
    /// load-bearing. `None` on legacy v1 sidecars.
    pub source_compressed_size: Option<u64>,
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
    /// v0.8.15 H-a: an entry's `original_offset + original_size` or
    /// `compressed_offset + compressed_size` overflows `u64`. The
    /// downstream `binary_search_by` / `lookup_range` machinery
    /// assumes monotonically-increasing offsets — overflow would let
    /// a forged sidecar drive the range planner into garbage state.
    #[error(
        "frame index entry overflows: original_offset={ooff}, original_size={osize}, \
         compressed_offset={coff}, compressed_size={csize}"
    )]
    EntryOverflow {
        ooff: u64,
        osize: u64,
        coff: u64,
        csize: u64,
    },
    /// v0.8.15 H-c: per-sidecar entry-count cap. Pairs with the v0.8.12
    /// `#124` `Vec::with_capacity` clamp — refuses pathologically-large
    /// `n` at parse time even before the `expected_remaining == input.len()`
    /// guard, so a 32-bit target can't be tricked into running `0..n`
    /// past the buffer.
    #[error("frame index entry count {got} exceeds MAX_FRAMES={max}")]
    TooManyFrames { got: u64, max: u64 },
    /// v0.8.15 H-c: `etag_len` exceeds the maximum addressable size on
    /// this target (32-bit) or the operator-configured cap.
    #[error("sidecar etag_len {got} exceeds MAX_ETAG_BYTES={max}")]
    EtagTooLong { got: u32, max: u32 },
    /// v0.8.16 F-2: consecutive entries are not in non-decreasing
    /// order. `binary_search_by` / `lookup_range` rely on the
    /// invariant that `entries[i+1].original_offset >=
    /// entries[i].original_end()` (and the same for `compressed_*`).
    /// A forged sidecar violating that lets a Range GET drive
    /// `RangePlan.byte_end_exclusive` to a u64-wrapped value.
    #[error(
        "frame index entries out of order: prev_original_end={prev_original_end}, \
         curr_original_offset={curr_original_offset}, prev_compressed_end={prev_compressed_end}, \
         curr_compressed_offset={curr_compressed_offset}"
    )]
    NonMonotonicEntries {
        prev_original_end: u64,
        curr_original_offset: u64,
        prev_compressed_end: u64,
        curr_compressed_offset: u64,
    },
}

/// v0.8.15 H-c: hard upper bound on the number of entries
/// [`decode_index`] will accept. 16 M × 32 B = 512 MiB sidecar
/// body — orders of magnitude over any real workload (a typical
/// 5 GiB object hits ~1280 frames at the 4 MiB default chunk).
/// Above this we'd be parsing an attacker payload, not a legitimate
/// sidecar.
pub const MAX_FRAMES: u64 = 16 * 1024 * 1024;
/// v0.8.15 H-c: hard upper bound on the etag-length field. AWS S3
/// ETags are ≤ 64 bytes including quotes; MinIO / Garage match. The
/// 4 KiB cap leaves room for non-canonical multipart ETags
/// (`<hex>-<n>`) without admitting attacker-controlled payloads.
pub const MAX_ETAG_BYTES: u32 = 4096;

/// v0.8.4 #73 H-2: emit the **v2** layout (with `source_etag` /
/// `source_compressed_size`). Pre-v0.8.4 deployments that PUT under v1 are
/// still readable (decode_index dispatches on the version field) — only the
/// writer path is bumped here.
pub fn encode_index(idx: &FrameIndex) -> Bytes {
    let etag_bytes = idx.source_etag.as_deref().unwrap_or("").as_bytes();
    let mut buf = BytesMut::with_capacity(
        HEADER_FIXED_V2 + etag_bytes.len() + idx.entries.len() * ENTRY_BYTES,
    );
    buf.put_slice(INDEX_MAGIC);
    buf.put_u32_le(INDEX_VERSION);
    buf.put_u64_le(idx.entries.len() as u64);
    buf.put_u64_le(idx.total_original_size());
    buf.put_u64_le(idx.total_padded_size);
    // v2 additions
    buf.put_u64_le(idx.source_compressed_size.unwrap_or(0));
    buf.put_u32_le(etag_bytes.len() as u32);
    buf.put_slice(etag_bytes);
    for e in &idx.entries {
        buf.put_u64_le(e.original_offset);
        buf.put_u64_le(e.original_size);
        buf.put_u64_le(e.compressed_offset);
        buf.put_u64_le(e.compressed_size);
    }
    buf.freeze()
}

/// v0.8.4 #73 H-2: legacy v1 encoder retained for the back-compat unit test
/// (`sidecar_header_back_compat_old_format_no_source_etag`) which has to
/// synthesize a v1 buffer to prove decode_index still parses it. Production
/// callers should always go through [`encode_index`] which emits v2.
#[doc(hidden)]
pub fn encode_index_v1_for_test(idx: &FrameIndex) -> Bytes {
    let mut buf = BytesMut::with_capacity(HEADER_FIXED_V1 + idx.entries.len() * ENTRY_BYTES);
    buf.put_slice(INDEX_MAGIC);
    buf.put_u32_le(INDEX_VERSION_V1);
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
    if input.len() < HEADER_FIXED_V1 {
        return Err(IndexError::TooShort(input.len()));
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&input[..4]);
    if &magic != INDEX_MAGIC {
        return Err(IndexError::BadMagic { got: magic });
    }
    input.advance(4);
    let version = input.get_u32_le();
    let n = input.get_u64_le();
    let _total_original = input.get_u64_le();
    let total_padded_size = input.get_u64_le();
    // v0.8.15 H-c: hard cap on `n` *before* any size arithmetic. The
    // existing `expected_remaining == input.len()` check is a
    // necessary condition but not sufficient — on a 32-bit target,
    // `n as usize` truncates a 33-bit value and the buffer check
    // would silently pass with the wrong loop count. Reject early.
    if n > MAX_FRAMES {
        return Err(IndexError::TooManyFrames {
            got: n,
            max: MAX_FRAMES,
        });
    }
    // Dispatch on version. v1 jumps straight to the entry table; v2 reads
    // the additional fixed fields + variable-length etag before the entries.
    let (source_compressed_size, source_etag) = match version {
        v if v == INDEX_VERSION_V1 => (None, None),
        v if v == INDEX_VERSION => {
            // v2 fixed-header tail: source_compressed_size (u64) + etag_len (u32).
            if input.len() < 8 + 4 {
                return Err(IndexError::TooShort(input.len()));
            }
            let scs = input.get_u64_le();
            let etag_len_u32 = input.get_u32_le();
            // v0.8.15 H-c: bound `etag_len` *before* the `as usize`
            // cast so the buffer check on a 32-bit WASM target can't
            // be tricked into a usize-truncated value.
            if etag_len_u32 > MAX_ETAG_BYTES {
                return Err(IndexError::EtagTooLong {
                    got: etag_len_u32,
                    max: MAX_ETAG_BYTES,
                });
            }
            let etag_len = etag_len_u32 as usize;
            if input.len() < etag_len {
                return Err(IndexError::TooShort(input.len()));
            }
            // Slice off the etag bytes; treat decode failure as "no etag" so
            // a corrupted etag field still leaves a usable index (the GET
            // path will fall back to full read on the missing binding).
            let etag_bytes = input.split_to(etag_len);
            let etag = if etag_len == 0 {
                None
            } else {
                std::str::from_utf8(&etag_bytes).ok().map(str::to_owned)
            };
            (if scs == 0 { None } else { Some(scs) }, etag)
        }
        other => return Err(IndexError::UnsupportedVersion(other)),
    };
    // v0.8.15 H-c: `n * ENTRY_BYTES` cannot overflow `usize` here
    // because `n <= MAX_FRAMES = 16M` and `ENTRY_BYTES = 32`, and on
    // 32-bit targets the resulting value fits in `usize` (≤ 512
    // MiB). The `as usize` cast on `n` is now bounded by the same
    // ceiling.
    let expected_remaining = (n as usize).saturating_mul(ENTRY_BYTES);
    if input.len() != expected_remaining {
        return Err(IndexError::EntryCountMismatch {
            claimed: n,
            remaining: input.len(),
        });
    }
    // v0.8.12 HIGH-14 fix: clamp the initial allocation the way the
    // CpuZstd / CpuGzip decompress path does (see
    // `DECOMPRESS_BOOTSTRAP_CAPACITY` in `lib.rs`, landed in #89).
    // A forged sidecar with `n = 100_000_000` paired with a 3.2 GiB
    // body (the only way the `expected_remaining` check above passes
    // for that `n`) would otherwise commit ~3.2 GiB of `FrameIndexEntry`
    // slots up front, on top of the 3.2 GiB body bytes already in
    // RAM. The honest cap is 4096 entries (128 KiB at
    // `ENTRY_BYTES = 32`) — large enough that single-PUT framed and
    // typical multipart objects don't pay any growth cost, small
    // enough that an adversarial sidecar can't drive multi-GiB
    // pre-allocations behind the bounded `expected_remaining`
    // check. The `push` loop below grows the vector naturally and
    // is itself bounded by `expected_remaining == input.len()`.
    const BOOTSTRAP_ENTRIES: usize = 4096;
    let initial_cap = (n as usize).min(BOOTSTRAP_ENTRIES);
    let mut entries = Vec::with_capacity(initial_cap);
    for _ in 0..n {
        let original_offset = input.get_u64_le();
        let original_size = input.get_u64_le();
        let compressed_offset = input.get_u64_le();
        let compressed_size = input.get_u64_le();
        // v0.8.15 H-a: refuse entries whose `offset + size` overflows
        // `u64`. The downstream `binary_search_by` / `lookup_range`
        // machinery relies on monotone offsets — a wrapped value
        // would let a forged sidecar drive `RangePlan.byte_end_exclusive`
        // to garbage.
        if original_offset.checked_add(original_size).is_none()
            || compressed_offset.checked_add(compressed_size).is_none()
        {
            return Err(IndexError::EntryOverflow {
                ooff: original_offset,
                osize: original_size,
                coff: compressed_offset,
                csize: compressed_size,
            });
        }
        entries.push(FrameIndexEntry {
            original_offset,
            original_size,
            compressed_offset,
            compressed_size,
        });
    }
    // v0.8.16 F-2: inter-entry monotonicity. v0.8.15 H-a closed the
    // per-entry `offset + size` overflow but did NOT verify that
    // entries are in non-decreasing order. The downstream
    // `binary_search_by` in `lookup_range` assumes sorted entries
    // — feed it a sidecar with `[ooff=100,...],[ooff=0,...]` and the
    // partition point logic returns garbage, then `start - entries[
    // first_idx].original_offset` underflows `u64` (wraps in
    // release, panics in dev) and the resulting `RangePlan` drives
    // an arbitrary backend GET range. Reject out-of-order entries
    // here with a dedicated typed error.
    for win in entries.windows(2) {
        let prev = &win[0];
        let curr = &win[1];
        if curr.original_offset < prev.original_end()
            || curr.compressed_offset < prev.compressed_end()
        {
            return Err(IndexError::NonMonotonicEntries {
                prev_original_end: prev.original_end(),
                curr_original_offset: curr.original_offset,
                prev_compressed_end: prev.compressed_end(),
                curr_compressed_offset: curr.compressed_offset,
            });
        }
    }
    Ok(FrameIndex {
        total_padded_size,
        entries,
        source_etag,
        source_compressed_size,
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
            // v0.8.16 F-3: was `pad_len as usize`, silently
            // truncating on 32-bit. A forged `S4P1 || u64::MAX`
            // padding header advanced the cursor by `0xFFFF_FFFF`
            // on 64-bit (skipping past `body.len()` into the next
            // iteration's break) and by `0xFFFF_FFFF` truncated
            // on 32-bit (different behaviour by target). Use
            // try_from + checked_add so a malformed body fails
            // closed with a typed `FrameError` instead of either
            // wandering off the end of the buffer or silently
            // skipping the bad frame.
            let pad_len_usize = usize::try_from(pad_len)
                .map_err(|_| crate::multipart::FrameError::PayloadTooLarge(pad_len))?;
            let next_cursor = cursor
                .checked_add(crate::multipart::PADDING_HEADER_BYTES)
                .and_then(|n| n.checked_add(pad_len_usize))
                .ok_or(crate::multipart::FrameError::PayloadTooLarge(pad_len))?;
            cursor = next_cursor;
            if cursor > body.len() {
                break;
            }
            iter_buf = body.slice(cursor..);
            continue;
        }
        // data frame
        if cursor + crate::multipart::FRAME_HEADER_BYTES > body.len() {
            break;
        }
        let (header, _payload, rest) = crate::multipart::read_frame(iter_buf.clone())?;
        // v0.8.16 F-3: `header.compressed_size as usize` had the
        // same 32-bit-truncation hazard as the padding cursor
        // arithmetic above. Use try_from so a forged 4 GiB+ frame
        // surfaces as `PayloadTooLarge` instead of wandering off.
        let compressed_size_usize = usize::try_from(header.compressed_size)
            .map_err(|_| crate::multipart::FrameError::PayloadTooLarge(header.compressed_size))?;
        let frame_total = crate::multipart::FRAME_HEADER_BYTES
            .checked_add(compressed_size_usize)
            .ok_or(crate::multipart::FrameError::PayloadTooLarge(
                header.compressed_size,
            ))?;
        entries.push(FrameIndexEntry {
            original_offset: original_off,
            original_size: header.original_size,
            compressed_offset: cursor as u64,
            compressed_size: frame_total as u64,
        });
        // v0.8.16 F-3: `original_off +=` was a plain `+`, panicking
        // in dev / wrapping in release on a forged body whose
        // cumulative original sizes overflow u64. Use checked_add
        // → typed error.
        original_off = original_off.checked_add(header.original_size).ok_or(
            crate::multipart::FrameError::PayloadTooLarge(header.original_size),
        )?;
        cursor = cursor.checked_add(frame_total).ok_or(
            crate::multipart::FrameError::PayloadTooLarge(header.compressed_size),
        )?;
        iter_buf = rest;
    }
    Ok(FrameIndex {
        total_padded_size: body.len() as u64,
        entries,
        // The caller (s4-server `put_object`) stamps the version-binding
        // fields after the backend PUT returns the authoritative ETag —
        // build_index_from_body itself only sees the post-compress bytes
        // and cannot fabricate a server-blessed ETag.
        source_etag: None,
        source_compressed_size: None,
    })
}

/// `<key>` から sidecar key を生成。
pub fn sidecar_key(object_key: &str) -> String {
    format!("{object_key}{SIDECAR_SUFFIX}")
}

/// v0.8.15 M-1: the per-object sidecar key suffix. Exposed publicly so
/// the listener-side reserved-name guard
/// (`s4-server::routing::is_reserved_object_key`) and the list-filter
/// `ends_with(".s4index")` calls share one source of truth.
pub const SIDECAR_SUFFIX: &str = ".s4index";

/// v0.8.15 M-1: classify a candidate user-PUT object key as a
/// reserved sidecar name. The S4 gateway uses `<key>.s4index` for
/// its internal Range-GET fast-path; a user PUT under that name
/// would either be hidden from `ListObjectsV2` (the filter strips
/// `.s4index` suffixes) or get collected by the sidecar-cleanup
/// path on `DeleteObject`. Returning a reserved-key error at the
/// listener edge stops both before the user can be surprised.
pub fn is_reserved_sidecar_key(object_key: &str) -> bool {
    object_key.ends_with(SIDECAR_SUFFIX)
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
            // Default-constructed in the v0.8.4 #73 H-2 sample so this fixture
            // still drives the lookup_range / encode_decode / build_from_body
            // paths that don't care about the version-binding fields.
            source_etag: None,
            source_compressed_size: None,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let idx = sample_index();
        let bytes = encode_index(&idx);
        let decoded = decode_index(bytes).unwrap();
        assert_eq!(decoded, idx);
    }

    /// v0.8.4 #73 H-2: v2 round-trip with the new `source_etag` /
    /// `source_compressed_size` fields populated.
    #[test]
    fn encode_decode_roundtrip_v2_with_source_binding() {
        let mut idx = sample_index();
        idx.source_etag = Some("\"deadbeefcafe\"".into());
        idx.source_compressed_size = Some(987_654);
        let bytes = encode_index(&idx);
        // First 4 bytes magic + next 4 bytes LE = INDEX_VERSION (2).
        assert_eq!(&bytes[..4], INDEX_MAGIC);
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(version, INDEX_VERSION, "writer must always emit v2");
        let decoded = decode_index(bytes).unwrap();
        assert_eq!(decoded, idx);
    }

    /// v0.8.4 #73 H-2: a sidecar produced by a pre-v0.8.4 deployment
    /// (= raw v1 bytes) must still decode cleanly under the v2 reader
    /// with `source_etag = None` / `source_compressed_size = None`. The
    /// GET path treats the `None` shape as "legacy — verify skip" so
    /// existing on-disk sidecars keep serving partial fetches without a
    /// flag day. This locks in the `decode_index` dispatch on the
    /// `version` field that makes the back-compat path real.
    #[test]
    fn sidecar_header_back_compat_old_format_no_source_etag() {
        let v2_idx = {
            let mut idx = sample_index();
            idx.source_etag = Some("\"unused\"".into());
            idx.source_compressed_size = Some(42);
            idx
        };
        // Round-trip through the v1 encoder — i.e. simulate decoding a
        // sidecar that was written by a pre-v0.8.4 server. The version-
        // binding fields are dropped on the way through (v1 has no slot
        // for them) and must come back as `None`.
        let v1_bytes = encode_index_v1_for_test(&v2_idx);
        // Sanity: the on-wire version field is v1.
        let version = u32::from_le_bytes(v1_bytes[4..8].try_into().unwrap());
        assert_eq!(version, INDEX_VERSION_V1);
        let decoded = decode_index(v1_bytes).expect("v1 sidecar must still decode");
        // Frame entries + total_padded_size survive (the partial-fetch
        // logic still works), but the new v2-only fields surface as None
        // so the GET path knows it cannot do an etag-bind verify and
        // applies the legacy "best-effort + fallback to full GET" rule.
        assert_eq!(decoded.entries, v2_idx.entries);
        assert_eq!(decoded.total_padded_size, v2_idx.total_padded_size);
        assert_eq!(decoded.source_etag, None);
        assert_eq!(decoded.source_compressed_size, None);
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
