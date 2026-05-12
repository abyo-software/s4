use super::{Error, Result};

/// Compression algorithm.
///
/// First-class members are [`Algo::Snappy`], [`Algo::Lz4`], [`Algo::Zstd`].
/// Phase 0 nvCOMP measurements (4070 Ti SUPER, 2026-05-09) elevated Snappy to
/// dark-horse status — 11.26 GB/s compress + **146.35 GB/s decompress** on the
/// json_logs synthetic dataset, beating LZ4 on both axes.
///
/// [`Algo::Deflate`] is kept for legacy compatibility (gzip/zlib pipelines)
/// and is *not* a recommended default for new code.
///
/// [`Algo::GDeflate`] is GPU-only (nvCOMP); the CPU backend rejects it
/// explicitly so callers don't silently fall back to a different format.
///
/// [`Algo::Bitcomp`] is a typed-numeric GPU codec (NVIDIA proprietary,
/// distributed inside nvCOMP). Phase 0 measured **ratio 3.59 / comp 419
/// GB/s / decomp 366 GB/s** on `postings.bin` with the `Uint32` hint —
/// the strongest of every tested algo on numeric columns. With the
/// `Char` hint the same dataset degrades to ratio 1.19, so `Bitcomp` is
/// not a generic-purpose codec and is intentionally **excluded** from
/// [`Tier::default_algo`]. Use it explicitly for posting lists,
/// numeric doc-values, and similar typed columns. CPU has no Bitcomp
/// implementation; the CPU backend rejects it like `GDeflate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Algo {
    Snappy,
    Lz4,
    Zstd,
    Deflate,
    GDeflate,
    /// nvCOMP Bitcomp — typed numeric column codec. The `data_type` hint
    /// is critical: Phase 0 saw ratio 3.59× with `Uint32` vs 1.19× with
    /// `Char` on the same posting-list dataset.
    Bitcomp {
        data_type: BitcompDataType,
    },
}

/// Type hint passed through to nvCOMP's `nvcompBatchedBitcompFormatOpts`.
///
/// Bitcomp uses the hint to pick its internal bit-packing / delta layout.
/// For posting lists (sorted u32 doc-id deltas) `Uint32` is the right
/// answer and reproduces the Phase 0 3.59× ratio.
///
/// `Char` is the safe-but-unimpressive default — it makes Bitcomp behave
/// as a generic byte codec, achieving only ~1.2× ratio on numeric data.
/// We surface it so callers can explicitly fall back when they don't
/// know the column shape, but the default helper
/// [`BitcompDataType::for_postings_uint32`] picks `Uint32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BitcompDataType {
    /// `NVCOMP_TYPE_CHAR` — generic 8-bit. Use when the column shape is
    /// unknown; Phase 0 ratio 1.19 on postings (= bad).
    Char,
    /// `NVCOMP_TYPE_UCHAR` — unsigned 8-bit.
    Uint8,
    /// `NVCOMP_TYPE_USHORT` — unsigned 16-bit.
    Uint16,
    /// `NVCOMP_TYPE_UINT` — unsigned 32-bit. Phase 0 best-for-postings:
    /// 3.59× ratio + 366 GB/s decomp on `postings.bin`.
    Uint32,
    /// `NVCOMP_TYPE_ULONGLONG` — unsigned 64-bit.
    Uint64,
    /// `NVCOMP_TYPE_CHAR` (signed). Same numeric value as `Char`; kept
    /// distinct in the surface API for clarity at call sites.
    Int8,
    /// `NVCOMP_TYPE_SHORT` — signed 16-bit.
    Int16,
    /// `NVCOMP_TYPE_INT` — signed 32-bit.
    Int32,
    /// `NVCOMP_TYPE_LONGLONG` — signed 64-bit.
    Int64,
    /// `NVCOMP_TYPE_FLOAT` — IEEE-754 binary32.
    Float32,
    /// `NVCOMP_TYPE_DOUBLE` — IEEE-754 binary64.
    Float64,
    /// `NVCOMP_TYPE_BFLOAT16` — Brain-float 16-bit.
    BFloat16,
}

impl BitcompDataType {
    /// The default "best for posting lists" hint. Phase 0 reproduced
    /// 3.59× ratio on sorted u32 delta columns at this setting.
    pub const fn for_postings_uint32() -> Self {
        BitcompDataType::Uint32
    }

    /// Stable string label used by [`Algo::name`] and [`Algo::parse`].
    pub const fn name(self) -> &'static str {
        match self {
            BitcompDataType::Char => "char",
            BitcompDataType::Uint8 => "uint8",
            BitcompDataType::Uint16 => "uint16",
            BitcompDataType::Uint32 => "uint32",
            BitcompDataType::Uint64 => "uint64",
            BitcompDataType::Int8 => "int8",
            BitcompDataType::Int16 => "int16",
            BitcompDataType::Int32 => "int32",
            BitcompDataType::Int64 => "int64",
            BitcompDataType::Float32 => "float32",
            BitcompDataType::Float64 => "float64",
            BitcompDataType::BFloat16 => "bfloat16",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "char" => Ok(BitcompDataType::Char),
            "uint8" => Ok(BitcompDataType::Uint8),
            "uint16" => Ok(BitcompDataType::Uint16),
            "uint32" => Ok(BitcompDataType::Uint32),
            "uint64" => Ok(BitcompDataType::Uint64),
            "int8" => Ok(BitcompDataType::Int8),
            "int16" => Ok(BitcompDataType::Int16),
            "int32" => Ok(BitcompDataType::Int32),
            "int64" => Ok(BitcompDataType::Int64),
            "float32" => Ok(BitcompDataType::Float32),
            "float64" => Ok(BitcompDataType::Float64),
            "bfloat16" => Ok(BitcompDataType::BFloat16),
            other => Err(Error::UnknownAlgo(format!("bitcomp:{other}"))),
        }
    }
}

impl Algo {
    /// "Best for posting lists" Bitcomp constructor — 3.59× ratio + 366
    /// GB/s decomp in Phase 0 on sorted u32 delta columns. Bitcomp is
    /// **not** a Tier default because it requires a typed column to
    /// shine; this helper documents the right call-site idiom.
    pub const fn for_postings_uint32() -> Self {
        Algo::Bitcomp {
            data_type: BitcompDataType::Uint32,
        }
    }

    pub fn name(self) -> &'static str {
        // For Bitcomp we return the data-type-less label so existing
        // callers that just want the family can still match. The full
        // name (with data-type suffix) is available via
        // [`Algo::display_full`] / [`Algo::display_short`] equivalents
        // on the `Display` impl.
        match self {
            Algo::Snappy => "snappy",
            Algo::Lz4 => "lz4",
            Algo::Zstd => "zstd",
            Algo::Deflate => "deflate",
            Algo::GDeflate => "gdeflate",
            Algo::Bitcomp { .. } => "bitcomp",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "snappy" => Ok(Algo::Snappy),
            "lz4" => Ok(Algo::Lz4),
            "zstd" => Ok(Algo::Zstd),
            "deflate" => Ok(Algo::Deflate),
            "gdeflate" => Ok(Algo::GDeflate),
            // Bitcomp accepts:
            //   - bare `bitcomp` → defaults to Uint32 (best-for-postings)
            //   - `bitcomp:<type>` (colon)  e.g. `bitcomp:uint32`
            //   - `bitcomp-<type>` (hyphen) e.g. `bitcomp-uint32` (CLI-friendly)
            "bitcomp" => Ok(Algo::for_postings_uint32()),
            other if other.starts_with("bitcomp:") || other.starts_with("bitcomp-") => {
                let suffix = &other[8..];
                let dt = BitcompDataType::parse(suffix)?;
                Ok(Algo::Bitcomp { data_type: dt })
            }
            other => Err(Error::UnknownAlgo(other.to_string())),
        }
    }

    /// Whether this algorithm is recommended for new code.
    /// Returns `false` for `Deflate` (legacy), `GDeflate` (GPU-only), and
    /// `Bitcomp` (typed-column-only — not a generic default).
    pub fn is_first_class(self) -> bool {
        matches!(self, Algo::Snappy | Algo::Lz4 | Algo::Zstd)
    }
}

impl std::fmt::Display for Algo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display includes the data-type suffix for Bitcomp so logs and
        // CSV output are unambiguous. Use [`Algo::name`] to get the
        // family-only label.
        match self {
            Algo::Bitcomp { data_type } => write!(f, "bitcomp-{}", data_type.name()),
            other => f.write_str(other.name()),
        }
    }
}

/// Storage tier — determines the recommended codec.
///
/// Default mappings come straight out of the Phase 0 ベンチ:
///
/// - **Hot / CHT**: [`Algo::Snappy`]. Decompression is the dominant cost on
///   the read path; Snappy's 146 GB/s GPU decompress (synthetic json_logs)
///   beat zstd's 45 GB/s by **3.2×** while keeping comparable ratio (3.07×
///   vs 5.31×).
/// - **Warm / Cold**: [`Algo::Zstd`]. Storage cost dominates over latency
///   here, so zstd's higher ratio (5.31× vs 3.07× on the same data) saves
///   ~40% in S3 / disk footprint relative to Snappy.
/// - **Wire / RPC**: [`Algo::Lz4`]. Matches the existing transport-layer
///   choice in `ferro-transport` (LZ4 frame format) and Tantivy's
///   `lz4-compression` feature; staying on LZ4 avoids a wire-format break.
///
/// Bitcomp is intentionally **not** a Tier default — its win is conditional
/// on the input being a typed numeric column. Use [`Algo::for_postings_uint32`]
/// at call sites that know the column shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Tier {
    /// Hot / Compressed Hot Tier — read-latency dominated.
    Hot,
    /// Warm — local NVMe, balance of latency and storage cost.
    Warm,
    /// Cold — object store (S3), storage cost dominated.
    Cold,
    /// Frozen — long-term archive, storage cost the only thing that matters.
    Frozen,
    /// Wire / RPC payload (transport-layer compression).
    Wire,
}

impl Tier {
    /// Recommended codec for this tier. See [`Tier`] docs for the rationale.
    pub fn default_algo(self) -> Algo {
        match self {
            Tier::Hot => Algo::Snappy,
            Tier::Warm | Tier::Cold | Tier::Frozen => Algo::Zstd,
            Tier::Wire => Algo::Lz4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cold => "cold",
            Tier::Frozen => "frozen",
            Tier::Wire => "wire",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
