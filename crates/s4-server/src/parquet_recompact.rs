//! Parquet-aware recompaction (`parquet-recompact` feature).
//!
//! Cold data-lake Parquet is overwhelmingly written with the **snappy** column
//! codec (Spark / pandas / Arrow's long-time default). This module reads such an
//! object and re-encodes its column chunks with **zstd**, producing a *native*
//! Parquet — still readable by pyarrow / Spark / Trino / DuckDB with **no S4 in
//! the read path**. It is an offline recompaction (like `s4 recompact`), not the
//! transparent gateway: the output is a normal Parquet, just smaller.
//!
//! Fidelity: the re-encode goes through Arrow and rewrites column chunks, but it
//! **preserves the input row-group boundaries** (so predicate-pushdown
//! granularity is unchanged) and carries the original Parquet **key-value file
//! metadata** (Spark/pandas schema) into the output. It does **not** promise to
//! preserve every low-level detail (column encodings, page sizes, statistics
//! shape, `created_by`, bloom filters); the output is **decoded-value +
//! key-value-metadata compatible, not byte/footer identical**, so exotic schemas
//! (deeply nested / dictionary columns) should be validated on a staging copy.
//! Each object is value-verified (per row group, bounded memory) before it is
//! written, and the verifier never overwrites with unverified data. Verify
//! outcomes split three ways: a **structural** mismatch (Arrow/physical schema
//! drift, row/row-group count, key-value metadata — an exotic input we decline to
//! rewrite) is a **conservative skip** (exit 0); a **decoded-value** mismatch
//! after a successful encode is by default a **hard failure** (nonzero exit) so a
//! writer regression can't hide behind a quiet no-op run, downgradable to a
//! counted skip with `--tolerate-value-mismatch` for the rare benign
//! representation-drift case; and a **corrupt/unparseable footer** is always a
//! **hard failure** (unambiguous data-integrity issue, no false-positive risk).
//! In every case the object is never overwritten and the batch continues.
//!
//! Scope: this targets **non-zstd** cold Parquet (snappy / none / gzip). Files
//! whose columns are already entirely zstd are skipped regardless of level — it
//! does not re-tune existing zstd levels. Objects are **skipped** (not silently
//! rewritten) when they are server-side-encrypted (SSE-S3/KMS/C), under
//! Object-Lock retention/legal-hold, carry an `Expires` header, or carry
//! sort-order / bloom-filter footer metadata. Page/column indexes and any S3
//! managed-checksum attribute are **regenerated** by the rewrite over the
//! identical data (functionally equivalent for the new bytes, not carried
//! verbatim — the same algorithm class is not guaranteed). Object **ACLs are not
//! carried over** by the rewrite (the only recoverable-but-dropped attribute;
//! everything else above is skipped or regenerated instead).
//!
//! Concurrency: the in-place overwrite is **conditional** (`If-Match` on the
//! source ETag, plus a pre-PUT re-HEAD of ETag + `Last-Modified`), so a
//! concurrent *content* rewrite is detected and skipped, never clobbered. It is
//! **best-effort for cold/quiescent prefixes**, by design: an S3 ETag does not
//! change on a tag-only or metadata-only update, and `Last-Modified` has
//! whole-second granularity, so a same-second tag/metadata-only change on an
//! **unversioned** bucket cannot be fully CAS-protected — this command is meant
//! to run on cold data (see `--older-than`) where such races don't occur. On a
//! versioned bucket the rewrite lands as a new version, so the prior version is
//! never lost regardless.

// `Bytes` is only used by the in-memory re-encode helpers, which are
// `#[cfg(test)]` (the backend path streams through temp files instead).
#[cfg(test)]
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::ParquetMetaData;
use parquet::file::properties::WriterProperties;

/// Outcome of re-encoding one Parquet object.
#[derive(Debug, Clone)]
pub struct ReencodeStats {
    pub input_len: usize,
    pub output_len: usize,
    pub rows: usize,
    pub columns: usize,
    pub row_groups: usize,
}

impl ReencodeStats {
    /// Saving as a fraction; positive = smaller. Negative = the re-encode grew
    /// the object (e.g. already-dense input).
    pub fn saved_fraction(&self) -> f64 {
        if self.input_len == 0 {
            return 0.0;
        }
        (self.input_len as f64 - self.output_len as f64) / self.input_len as f64
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParquetRecompactError {
    #[error("not a Parquet object (missing PAR1 magic)")]
    NotParquet,
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    /// Structural / conservative verify failure (Arrow or physical schema drift,
    /// row/row-group count, key-value metadata, batch over the memory cap). These
    /// are EXPECTED for exotic inputs we decline to rewrite — the caller treats
    /// them as a safe skip, not a hard failure.
    #[error("round-trip verification failed: {0}")]
    VerifyFailed(String),
    /// Decoded column VALUES differ (bitwise `ArrayData`) after a successful
    /// encode. The caller treats this as a **distinct, loudly-reported skip**
    /// (never an overwrite), separate from structural drift — but NOT a hard
    /// failure, because the bitwise compare can also trip on benign
    /// representation differences (e.g. an explicitly Dictionary-typed schema's
    /// id ordering). Surfaced in the report's `value-mismatch` counter so an
    /// operator can investigate; a real writer regression shows up across objects.
    #[error("round-trip VALUE mismatch: {0}")]
    VerifyValueMismatch(String),
    #[error("writer memory exceeded the cap mid-encode ({0} bytes)")]
    WriterMemoryExceeded(u64),
    #[error("re-encoded output exceeded the cap mid-write ({0} bytes)")]
    OutputTooLarge(u64),
}

/// Cheap check: does the blob start and end with the Parquet `PAR1` magic?
pub fn looks_like_parquet(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[..4] == b"PAR1" && &bytes[bytes.len() - 4..] == b"PAR1"
}

/// Upper bound on the Thrift footer we'll let the Parquet reader parse. The
/// footer (schema + per-row-group/column metadata) is allocated by `try_new`
/// before any row-group cap applies, so a corrupt/hostile object declaring a
/// gigantic footer could OOM there. Real footers are KB–low-MB even for huge
/// files; 128 MiB is an absurd ceiling that only a pathological object trips.
const MAX_FOOTER_BYTES: u64 = 128 << 20;

/// Streaming equivalent of [`looks_like_parquet`] for a spooled-to-disk object:
/// reads only the framing/footer-length trailer without loading the body.
/// Returns `(looks_like_parquet, declared_footer_len)`. The Parquet trailer is
/// `[4-byte footer length LE][PAR1]`, so the footer length sits at `len-8..len-4`
/// and the magic at `len-4..len`. `declared_footer_len` is `None` when the file
/// is too short to be Parquet.
fn parquet_trailer(f: &mut std::fs::File) -> std::io::Result<(bool, Option<u64>)> {
    use std::io::{Read, Seek, SeekFrom};
    let len = f.seek(SeekFrom::End(0))?;
    if len < 8 {
        return Ok((false, None));
    }
    let mut head = [0u8; 4];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut head)?;
    let mut trailer = [0u8; 8];
    f.seek(SeekFrom::End(-8))?;
    f.read_exact(&mut trailer)?;
    let footer_len = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) as u64;
    let ok = &head == b"PAR1" && &trailer[4..] == b"PAR1";
    Ok((ok, Some(footer_len)))
}

/// The Parquet key-value file metadata we carry/compare, excluding `ARROW:schema`
/// — the Arrow writer regenerates that automatically from the schema (which we
/// verify separately), so carrying the input's verbatim would duplicate it.
fn carried_kv(
    kv: Option<&Vec<parquet::file::metadata::KeyValue>>,
) -> Vec<parquet::file::metadata::KeyValue> {
    kv.map(|v| {
        v.iter()
            .filter(|k| k.key != "ARROW:schema")
            .cloned()
            .collect()
    })
    .unwrap_or_default()
}

/// Per-row-group decode batch size targeting a fixed *byte* budget rather than a
/// fixed row count, so a very wide schema or large variable-width values can't
/// materialize a giant batch before the post-decode memory guard runs. Derived
/// from the footer's uncompressed `total_byte_size / num_rows`; clamped to
/// `[1, MAX_BATCH_ROWS]`. A single row wider than the budget yields 1 row/batch
/// (and the post-decode guard then skips it if even that exceeds the cap).
fn batch_rows_for(total_byte_size: i64, num_rows: i64, max_mem: u64) -> usize {
    const MAX_BATCH_ROWS: usize = 8_192;
    const TARGET_BATCH_BYTES: u64 = 16 << 20; // 16 MiB decoded target per batch
    if num_rows <= 0 {
        return MAX_BATCH_ROWS;
    }
    let bytes_per_row = (total_byte_size.max(0) as u64 / num_rows as u64).max(1);
    // shrink the target if the caller's memory cap is tighter than 16 MiB
    let target = TARGET_BATCH_BYTES.min(max_mem.max(1));
    ((target / bytes_per_row) as usize).clamp(1, MAX_BATCH_ROWS)
}

/// True if every column chunk in every row group is already zstd-compressed —
/// re-encoding such a file to zstd would be a no-op, so it is skipped. Reads the
/// actual footer (unspoofable), not object metadata. A file with **no column
/// chunks** (zero columns / zero row groups) has nothing to recompress, so it is
/// vacuously "already done" and returns true — that keeps the skip idempotent
/// (such a file is never re-evaluated endlessly run after run).
pub fn all_columns_already_zstd(meta: &ParquetMetaData) -> bool {
    for rg in meta.row_groups() {
        for col in rg.columns() {
            if !matches!(col.compression(), Compression::ZSTD(_)) {
                return false;
            }
        }
    }
    true
}

/// A `Write` that counts bytes and aborts once the running total exceeds `cap`,
/// flipping `overflowed` so the caller can distinguish "hit the cap" from a real
/// I/O error after the write fails. Bounds the scratch the output spool can
/// consume during the encode (the output normally shrinks, but a pathological
/// expansion shouldn't be able to fill the disk before the post-encode check).
struct CappedSink<W> {
    inner: W,
    written: u64,
    cap: u64,
    overflowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<W: std::io::Write> std::io::Write for CappedSink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Check the projected total BEFORE writing, and count only the bytes the
        // inner writer actually accepts — so a partial write or an inner error
        // can't overcount and false-trip the cap.
        if self.written.saturating_add(buf.len() as u64) > self.cap {
            self.overflowed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(std::io::Error::other("output cap exceeded"));
        }
        let n = self.inner.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Core re-encode: read the input Parquet (via `open_input`, a reader factory),
/// write a zstd Parquet to `sink`, preserving the input row-group boundaries and
/// (non-ARROW) key-value file metadata. Returns (rows, columns, row_groups); the
/// caller supplies the byte sizes. The output is streamed to `sink` (a `Vec` for
/// callers that want bytes, or a `File` so it never sits in RAM); the input is
/// read through `open_input`, which yields a fresh `ChunkReader` each call (a
/// cheap `Bytes` clone for the in-memory path, or a reopened `File` for the
/// backend path — so a multi-GB object is ranged-read off disk, never buffered).
/// `max_writer_mem` bounds live Arrow heap; `max_output_bytes` bounds the bytes
/// written to `sink` (output disk scratch).
fn recompress_to_sink<RIn, FIn, W>(
    open_input: FIn,
    zstd_level: i32,
    sink: W,
    max_writer_mem: u64,
    max_output_bytes: u64,
) -> Result<(usize, usize, usize), ParquetRecompactError>
where
    FIn: FnMut() -> Result<RIn, ParquetRecompactError>,
    RIn: parquet::file::reader::ChunkReader + 'static,
    W: std::io::Write + Send,
{
    let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sink = CappedSink {
        inner: sink,
        written: 0,
        cap: max_output_bytes,
        overflowed: overflowed.clone(),
    };
    // Run the encode; if anything errors AND the output-cap tripped, report it as
    // OutputTooLarge (a skip) rather than a generic I/O failure.
    match recompress_to_sink_inner(open_input, zstd_level, sink, max_writer_mem) {
        Err(_) if overflowed.load(std::sync::atomic::Ordering::Relaxed) => {
            Err(ParquetRecompactError::OutputTooLarge(max_output_bytes))
        }
        other => other,
    }
}

fn recompress_to_sink_inner<RIn, FIn, W>(
    mut open_input: FIn,
    zstd_level: i32,
    sink: W,
    max_writer_mem: u64,
) -> Result<(usize, usize, usize), ParquetRecompactError>
where
    FIn: FnMut() -> Result<RIn, ParquetRecompactError>,
    RIn: parquet::file::reader::ChunkReader + 'static,
    W: std::io::Write + Send,
{
    // Parse the footer ONCE and reuse it for every per-row-group reader below
    // (via `new_with_metadata`), so a file with many row groups isn't an
    // O(row_groups × footer_parse) reparse storm.
    let arm = parquet::arrow::arrow_reader::ArrowReaderMetadata::load(
        &open_input()?,
        parquet::arrow::arrow_reader::ArrowReaderOptions::default(),
    )?;
    let schema = arm.schema().clone();
    let columns = schema.fields().len();
    let meta = arm.metadata().clone();
    let num_row_groups = meta.num_row_groups();
    // carry the original Parquet-level key-value metadata (Spark/pandas schema),
    // minus ARROW:schema which the writer regenerates
    let kv = carried_kv(meta.file_metadata().key_value_metadata());

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(zstd_level)?))
        .set_key_value_metadata(if kv.is_empty() { None } else { Some(kv) })
        // very large cap so a flush() (not the size cap) decides boundaries
        .set_max_row_group_size(usize::MAX)
        .build();
    let mut writer = ArrowWriter::try_new(sink, schema, Some(props))?;
    let mut rows = 0usize;
    // Write one output row group per input row group: read RG-by-RG and flush
    // after each so the output preserves the input's row-group boundaries. Read in
    // byte-budgeted batches (not a fixed row count) so a wide/large-value schema
    // can't materialize a giant batch before the live-memory guard runs.
    for rg_idx in 0..num_row_groups {
        let rg = meta.row_group(rg_idx);
        let batch_rows = batch_rows_for(rg.total_byte_size(), rg.num_rows(), max_writer_mem);
        let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(open_input()?, arm.clone())
            .with_row_groups(vec![rg_idx])
            .with_batch_size(batch_rows)
            .build()?;
        for batch in reader {
            let batch = batch?;
            rows += batch.num_rows();
            // Pre-write guard: bound the decoded batch's own Arrow heap BEFORE
            // handing it to the writer, so a single very wide / variable-width
            // batch can't balloon memory between post-write checks. Measures the
            // actual decoded `RecordBatch`, not the footer estimate.
            let batch_mem = batch.get_array_memory_size() as u64;
            if batch_mem > max_writer_mem {
                return Err(ParquetRecompactError::WriterMemoryExceeded(batch_mem));
            }
            writer.write(&batch)?;
            // Post-write guard: the footer's uncompressed-size estimate can
            // undercount Arrow's live encoder heap (dictionary/list/string-heavy
            // columns), so also bound the *actual* in-flight writer memory. Abort
            // -> the caller skips the object.
            let live = writer.memory_size() as u64;
            if live > max_writer_mem {
                return Err(ParquetRecompactError::WriterMemoryExceeded(live));
            }
        }
        writer.flush()?;
    }
    writer.close()?;
    Ok((rows, columns, num_row_groups))
}

/// In-memory re-encode (tests only — unbounded `Vec`/`u64::MAX` caps). The
/// backend orchestration uses the spooled, bounded temp-file path instead (see
/// `recompact_one_object`); this stays `#[cfg(test)]` so it can't be a library
/// OOM footgun.
#[cfg(test)]
fn recompress_parquet(
    input: Bytes,
    zstd_level: i32,
) -> Result<(Vec<u8>, ReencodeStats), ParquetRecompactError> {
    if !looks_like_parquet(&input) {
        return Err(ParquetRecompactError::NotParquet);
    }
    let input_len = input.len();
    let mut out: Vec<u8> = Vec::with_capacity(input_len);
    // In-memory path (tests / callers wanting bytes): each reader is a cheap
    // refcount clone of the same `Bytes`; no live-memory or output cap — the
    // backend orchestration is the one that bounds writer memory + output disk.
    let (rows, columns, row_groups) = recompress_to_sink(
        || Ok(input.clone()),
        zstd_level,
        &mut out,
        u64::MAX,
        u64::MAX,
    )?;
    let stats = ReencodeStats {
        input_len,
        output_len: out.len(),
        rows,
        columns,
        row_groups,
    };
    Ok((out, stats))
}

/// Verify the re-encoded output reads back **value-for-value identical** to the
/// input — full Arrow schema (incl. metadata), the **Parquet physical schema
/// descriptor** leaf-for-leaf (so an Arrow round-trip that silently changes the
/// on-disk physical type — INT96 timestamps, decimal representation, field IDs —
/// is caught even though Arrow values still compare equal), total rows,
/// row-group count, carried key-value metadata, and every column's data (per row
/// group, bounded memory).
/// Generic over both the input and output sources via reader factories
/// (`open_input` / `open_output`, each an in-memory `Bytes` or a reopened
/// `File`), so the in-memory and temp-file paths share one verifier and neither
/// side is ever fully buffered in RAM. The verifier is conservative: any
/// mismatch returns `VerifyFailed`.
fn verify_equal<FI, RI, FO, RO>(
    mut open_input: FI,
    expected_row_groups: usize,
    mut open_output: FO,
    max_batch_mem: u64,
) -> Result<(), ParquetRecompactError>
where
    FI: FnMut() -> Result<RI, ParquetRecompactError>,
    RI: parquet::file::reader::ChunkReader + 'static,
    FO: FnMut() -> Result<RO, ParquetRecompactError>,
    RO: parquet::file::reader::ChunkReader + 'static,
{
    use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
    // Parse each side's footer ONCE and reuse it for every per-row-group reader
    // below, so verify isn't an O(row_groups × footer_parse) reparse storm.
    let arm_in = ArrowReaderMetadata::load(&open_input()?, ArrowReaderOptions::default())?;
    let arm_out = ArrowReaderMetadata::load(&open_output()?, ArrowReaderOptions::default())?;
    {
        let ib = &arm_in;
        let ob = &arm_out;
        // full Arrow schema, including top-level schema metadata
        if ib.schema() != ob.schema() {
            return Err(ParquetRecompactError::VerifyFailed(
                "output Arrow schema (incl. metadata) differs from input".to_string(),
            ));
        }
        // The Parquet *physical* schema tree must match. The Arrow schema check
        // above is necessary but NOT sufficient: the Arrow reader normalizes
        // physical encodings (INT96 timestamps -> Timestamp, decimal physical
        // representation, logical/converted annotations, field IDs, LIST/MAP
        // group annotations, 2-level vs 3-level list layout) to the same Arrow
        // type on both the input and the re-encoded output, so value+Arrow-schema
        // equality can pass while the on-disk Parquet type a downstream engine
        // (legacy Spark/Hive INT96, Iceberg field IDs, decimal-as-fixed, nested
        // LIST/MAP shape) reads has silently changed. Compare the full Parquet
        // schema `Type` tree, leaf AND group nodes, by comparing the root's
        // child fields (the root message itself is intentionally excluded — the
        // Arrow writer always renames the root, e.g. `arrow_schema`, which is
        // cosmetic). `Type: PartialEq` walks the whole subtree: physical/logical
        // type, precision/scale, field id, repetition, names and nesting. Any
        // drift fails the object (a skip — never an overwrite).
        let in_root = ib.metadata().file_metadata().schema_descr().root_schema();
        let out_root = ob.metadata().file_metadata().schema_descr().root_schema();
        if in_root.get_fields() != out_root.get_fields() {
            return Err(ParquetRecompactError::VerifyFailed(
                "Parquet physical schema tree drifted (physical/logical type, precision, \
                 field-id, or nested LIST/MAP shape changed in the Arrow round-trip)"
                    .to_string(),
            ));
        }
        let in_meta = ib.metadata().file_metadata();
        let out_meta = ob.metadata().file_metadata();
        if in_meta.num_rows() != out_meta.num_rows() {
            return Err(ParquetRecompactError::VerifyFailed(format!(
                "total row count changed: {} -> {}",
                in_meta.num_rows(),
                out_meta.num_rows()
            )));
        }
        if ib.metadata().num_row_groups() != ob.metadata().num_row_groups() {
            return Err(ParquetRecompactError::VerifyFailed(
                "row-group count changed".to_string(),
            ));
        }
        // the Parquet key-value file metadata (Spark/pandas schema) must survive
        // (excluding ARROW:schema, which the writer regenerates)
        if carried_kv(in_meta.key_value_metadata()) != carried_kv(out_meta.key_value_metadata()) {
            return Err(ParquetRecompactError::VerifyFailed(
                "key-value file metadata not preserved".to_string(),
            ));
        }
    }
    // Boundaries are preserved (input RG i == output RG i), so we can compare
    // batch-by-batch in lockstep at a fixed batch size: both readers yield
    // identically-sized batches, and we only ever hold ~one batch from each in
    // memory (NOT a whole decoded row group) — bounded regardless of row-group
    // size, and not trusting the footer's size estimate.
    for rg in 0..expected_row_groups {
        // Byte-budgeted batch size (same bound as the encode side) so verify
        // can't materialize a giant batch for wide/large-value row groups.
        let rgm = arm_in.metadata().row_group(rg);
        let batch_rows = batch_rows_for(rgm.total_byte_size(), rgm.num_rows(), max_batch_mem);
        let mut in_rdr =
            ParquetRecordBatchReaderBuilder::new_with_metadata(open_input()?, arm_in.clone())
                .with_row_groups(vec![rg])
                .with_batch_size(batch_rows)
                .build()?;
        let mut out_rdr =
            ParquetRecordBatchReaderBuilder::new_with_metadata(open_output()?, arm_out.clone())
                .with_row_groups(vec![rg])
                .with_batch_size(batch_rows)
                .build()?;
        loop {
            match (in_rdr.next(), out_rdr.next()) {
                (Some(a), Some(b)) => {
                    let (a, b) = (a?, b?);
                    // Same decoded-batch memory bound as the encode side, so a
                    // very wide / variable-width verify batch can't balloon RAM.
                    // A batch over the cap means we can't verify within budget —
                    // skip the object (VerifyFailed), never overwrite unverified.
                    let batch_mem = a.get_array_memory_size().max(b.get_array_memory_size()) as u64;
                    if batch_mem > max_batch_mem {
                        return Err(ParquetRecompactError::VerifyFailed(format!(
                            "verify batch exceeds memory cap ({batch_mem} bytes) in row group {rg}"
                        )));
                    }
                    if a.num_rows() != b.num_rows() {
                        return Err(ParquetRecompactError::VerifyFailed(format!(
                            "batch row mismatch in row group {rg}"
                        )));
                    }
                    for (i, (ca, cb)) in a.columns().iter().zip(b.columns()).enumerate() {
                        // ArrayData PartialEq compares value buffers bitwise (so
                        // NaN bit-patterns match — what we want for fidelity).
                        // A decoded-value difference is a genuine regression (see
                        // VerifyValueMismatch), not exotic-schema drift.
                        if ca.to_data() != cb.to_data() {
                            return Err(ParquetRecompactError::VerifyValueMismatch(format!(
                                "column {i} differs in row group {rg}"
                            )));
                        }
                    }
                }
                (None, None) => break,
                _ => {
                    return Err(ParquetRecompactError::VerifyFailed(format!(
                        "batch count mismatch in row group {rg}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// In-memory re-encode + verify (tests only). Returns the new bytes + stats
/// only if the round-trip is value-for-value identical. `#[cfg(test)]` for the
/// same reason as [`recompress_parquet`] — unbounded in-memory path.
#[cfg(test)]
fn recompress_parquet_verified(
    input: Bytes,
    zstd_level: i32,
) -> Result<(Bytes, ReencodeStats), ParquetRecompactError> {
    let (out, stats) = recompress_parquet(input.clone(), zstd_level)?;
    let out_bytes = Bytes::from(out);
    verify_equal(
        || Ok(input.clone()),
        stats.row_groups,
        || Ok(out_bytes.clone()),
        u64::MAX,
    )?;
    Ok((out_bytes, stats))
}

// ---------------------------------------------------------------------------
// Backend orchestration: list a bucket/prefix, re-encode each cold Parquet
// object in place, and (with --execute) write it back. Dry-run by default.
// ---------------------------------------------------------------------------

use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use std::time::Duration;

/// Default per-object body cap for this command. Both the input and the
/// re-encoded output are spooled to temp files (never fully buffered in RAM), so
/// this is primarily a **disk** cap — it bounds the scratch space one object can
/// consume. Peak RAM is independent of object size: ≈ one decoded Arrow batch +
/// the writer's in-progress row group (bounded by `--max-row-group-bytes`).
/// Raise `--max-body-bytes` (with scratch headroom; see `--tmp-dir`) to process
/// larger objects.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 512 << 20; // 512 MiB

/// Default uncompressed row-group cap (the `--max-row-group-bytes` flag). Used
/// two ways: (1) a **footer preflight** that skips an object whose row-group
/// `total_byte_size` exceeds it, and (2) the **live writer-memory cap** during
/// the re-encode (both the decoded-batch and in-flight-writer guards abort above
/// it). The re-encode and verify both stream batch-by-batch, so neither holds a
/// whole row group; this is the safety bound on the worst-case live Arrow heap.
/// The footer figure is an **estimate** — Arrow's decoded heap (offset/validity
/// buffers, decoded dictionaries) can exceed `total_byte_size`, which is exactly
/// why the live guards exist; keep host headroom above this.
pub const DEFAULT_MAX_ROW_GROUP_BYTES: u64 = 256 << 20; // 256 MiB

/// AWS S3's single-`PutObject` ceiling — outputs above this would fail late, so
/// they're skipped before the PUT. S3 documents the limit as 5 GB; we use the
/// decimal value (the smaller of 5 GB vs 5 GiB) so objects in the 5e9..5GiB
/// band skip cleanly rather than hard-failing at PUT time on backends that
/// enforce decimal GB.
const SINGLE_PUT_LIMIT: u64 = 5_000_000_000; // 5 GB (decimal, AWS-documented)

#[derive(Debug, Clone)]
pub struct ParquetRecompactParams {
    pub execute: bool,
    pub target_zstd_level: i32,
    /// Skip the rewrite unless it shrinks the object by at least this percent.
    pub min_gain_percent: f64,
    /// Only consider keys ending with this suffix (default ".parquet").
    pub suffix: String,
    pub max_objects: Option<usize>,
    /// Skip objects larger than this. Input and output are both spooled to temp
    /// files (not held in RAM), so this is primarily a per-object **disk** cap;
    /// live Arrow memory is bounded separately by `max_uncompressed_row_group_bytes`.
    pub max_body_bytes: u64,
    /// Live Arrow-memory bound: footer preflight skip when a row group's
    /// uncompressed `total_byte_size` exceeds this, AND the decoded-batch /
    /// in-flight-writer guards during the (batch-streamed) re-encode and verify.
    pub max_uncompressed_row_group_bytes: u64,
    /// Only recompact objects older than this (cold data); `None` = no age gate.
    pub older_than: Option<Duration>,
    /// Skip the `GetObjectTagging` read and rewrite WITHOUT carrying tags over.
    pub no_tags: bool,
    /// Directory to spool the rewritten Parquet into (the temp file). `None` =
    /// the OS default temp dir. Point this at a volume with headroom when
    /// raising `--max-body-bytes`, so a large rewrite can't fill `/tmp`.
    pub tmp_dir: Option<std::path::PathBuf>,
    /// Downgrade a decoded-VALUE verify mismatch from a hard failure (the
    /// default — nonzero exit so a writer regression can't hide) to a conservative
    /// skip (`value-mismatch` counter). Opt-in for operators who've confirmed the
    /// mismatch is benign representation drift on exotic (e.g. explicit-dictionary)
    /// schemas. Either way the object is never overwritten.
    pub tolerate_value_mismatch: bool,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ParquetRecompactReport {
    pub scanned: usize,
    pub recompacted: usize,
    pub skipped_suffix: usize,
    pub skipped_not_parquet: usize,
    pub skipped_low_gain: usize,
    pub skipped_already_zstd: usize,
    pub skipped_too_large: usize,
    pub skipped_too_new: usize,
    pub skipped_etag_raced: usize,
    pub skipped_tags_unreadable: usize,
    pub skipped_unknown_age: usize,
    pub skipped_unsupported_footer: usize,
    pub skipped_verify_failed: usize,
    pub skipped_value_mismatch: usize,
    pub skipped_encrypted: usize,
    pub skipped_etag_unavailable: usize,
    pub skipped_locked: usize,
    pub skipped_has_expires: usize,
    /// objects in an archive storage class (GLACIER / DEEP_ARCHIVE) that a plain
    /// GET can't read without a restore — pre-skipped from the listing
    pub skipped_archived: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Echoes `--no-tags`: when true, rewritten objects did NOT carry their
    /// existing object tags over. Surfaced so automation/operators see the drop.
    pub no_tags: bool,
    /// Per-object hard failures: (key, cause). Counted in `failed`.
    pub failures: Vec<(String, String)>,
}

enum Outcome {
    Recompacted {
        before: u64,
        after: u64,
    },
    NotParquet,
    LowGain,
    AlreadyZstd,
    TooLarge,
    EtagRaced,
    TagsUnreadable,
    /// the GET's authoritative `Last-Modified` is newer than the `--older-than`
    /// cutoff (the object was rewritten between listing and GET) — skip; the
    /// listing-time pre-skip can't see a post-list overwrite
    TooNew,
    /// the GET returned no `Last-Modified` while an age gate is set — can't prove
    /// the object is cold, so skip rather than risk rewriting hot data
    UnknownAge,
    /// footer carries metadata this tool can't preserve (sort columns / bloom
    /// filters) — skip rather than silently drop query-planning metadata
    UnsupportedFooter,
    /// structural verify mismatch (Arrow/physical schema drift, row/row-group
    /// count, key-value metadata) — an exotic input we decline to rewrite; skip,
    /// never overwrite
    VerifyFailed,
    /// decoded column VALUES differed (bitwise) after a successful encode, and
    /// `--tolerate-value-mismatch` downgraded it from a hard failure to this
    /// counted skip (never an overwrite). Without that flag a value mismatch is a
    /// hard failure instead — see [`ParquetRecompactError::VerifyValueMismatch`]
    ValueMismatch,
    /// the object is server-side encrypted at the backend (SSE-S3/KMS/C) — a
    /// re-PUT could drop the encryption / key / context, so skip it
    Encrypted,
    /// the backend returned no ETag, so the conflict guard / conditional PUT
    /// can't protect the in-place overwrite — skip rather than overwrite blind
    EtagUnavailable,
    /// the object carries Object-Lock retention / legal hold — a re-PUT would
    /// change compliance semantics (or be blocked), so skip it
    Locked,
    /// the object carries an `Expires` header the rewrite can't cleanly
    /// round-trip — skip rather than silently change its cache semantics
    HasExpires,
}

/// One pass over `s3://bucket/prefix`: re-encode each cold Parquet object's
/// columns to zstd in place. Dry-run unless `params.execute`.
pub async fn run_parquet_recompact(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    params: &ParquetRecompactParams,
) -> Result<ParquetRecompactReport, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let mut report = ParquetRecompactReport {
        no_tags: params.no_tags,
        ..Default::default()
    };
    let cutoff_secs: Option<i64> = params.older_than.map(|d| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        // saturating so an extreme --older-than can't overflow (a far-past
        // cutoff just makes more objects count as "too new", which is safe)
        now.saturating_sub(i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    });
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(c) = continuation.as_ref() {
            req = req.continuation_token(c);
        }
        let resp = req.send().await?;
        for obj in resp.contents() {
            let Some(key) = obj.key() else { continue };
            if crate::migrate::is_internal_key(key) {
                continue;
            }
            if !key.ends_with(&params.suffix) {
                report.skipped_suffix += 1;
                continue;
            }
            // bound the scan (not just the rewrites) so a skip-heavy prefix
            // can't run far past --max-objects
            if let Some(max) = params.max_objects
                && report.scanned >= max
            {
                return Ok(report);
            }
            report.scanned += 1;
            // cheap pre-skips from the listing (no GET)
            // Archive storage classes (GLACIER / DEEP_ARCHIVE) can't be read by a
            // plain GET without a restore — pre-skip them from the listing so a
            // cold-tier object in a data-lake prefix doesn't become a hard
            // per-object failure. (GLACIER_IR is instant-retrieval, so allowed.)
            if obj
                .storage_class()
                .is_some_and(|c| matches!(c.as_str(), "GLACIER" | "DEEP_ARCHIVE"))
            {
                report.skipped_archived += 1;
                continue;
            }
            let size = obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);
            if size > params.max_body_bytes {
                report.skipped_too_large += 1;
                continue;
            }
            if let Some(cut) = cutoff_secs {
                // Conservative age gate: skip if newer than the cutoff, AND skip
                // if the timestamp is missing (don't risk rewriting hot data).
                match obj.last_modified() {
                    Some(lm) if lm.secs() > cut => {
                        report.skipped_too_new += 1;
                        continue;
                    }
                    None => {
                        report.skipped_unknown_age += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            match recompact_one_object(client, bucket, key, params, cutoff_secs).await {
                Ok(Outcome::Recompacted { before, after }) => {
                    report.recompacted += 1;
                    report.bytes_before += before;
                    report.bytes_after += after;
                }
                Ok(Outcome::NotParquet) => report.skipped_not_parquet += 1,
                Ok(Outcome::LowGain) => report.skipped_low_gain += 1,
                Ok(Outcome::AlreadyZstd) => report.skipped_already_zstd += 1,
                Ok(Outcome::TooLarge) => report.skipped_too_large += 1,
                Ok(Outcome::EtagRaced) => report.skipped_etag_raced += 1,
                Ok(Outcome::TagsUnreadable) => report.skipped_tags_unreadable += 1,
                Ok(Outcome::UnsupportedFooter) => report.skipped_unsupported_footer += 1,
                Ok(Outcome::VerifyFailed) => report.skipped_verify_failed += 1,
                Ok(Outcome::ValueMismatch) => report.skipped_value_mismatch += 1,
                Ok(Outcome::Encrypted) => report.skipped_encrypted += 1,
                Ok(Outcome::EtagUnavailable) => report.skipped_etag_unavailable += 1,
                Ok(Outcome::Locked) => report.skipped_locked += 1,
                Ok(Outcome::HasExpires) => report.skipped_has_expires += 1,
                Ok(Outcome::TooNew) => report.skipped_too_new += 1,
                Ok(Outcome::UnknownAge) => report.skipped_unknown_age += 1,
                Err(e) => {
                    report.failed += 1;
                    report.failures.push((key.to_string(), e.to_string()));
                    tracing::warn!(key = %key, error = %e, "parquet-recompact: object failed");
                }
            }
        }
        match resp.next_continuation_token() {
            Some(t) => continuation = Some(t.to_string()),
            None => break,
        }
    }
    Ok(report)
}

async fn recompact_one_object(
    client: &Client,
    bucket: &str,
    key: &str,
    params: &ParquetRecompactParams,
    cutoff_secs: Option<i64>,
) -> Result<Outcome, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let resp = client.get_object().bucket(bucket).key(key).send().await?;
    // Keep the RAW (quoted) ETag for the conditional `If-Match` PUT; the
    // normalized form is only for our own comparisons.
    let raw_etag = resp.e_tag().map(str::to_string);
    let source_etag = raw_etag.as_deref().map(crate::migrate::normalize_etag);
    // Capture LastModified too: an S3 ETag does not change on a metadata-only /
    // tag-only update, so a concurrent such change with unchanged bytes would
    // slip past an ETag-only guard and get clobbered by the stale metadata we
    // captured at GET. We re-check both ETag and LastModified before the PUT.
    let source_last_modified = resp.last_modified().cloned();
    // On a versioned bucket the GET/HEAD carry an exact version-id; capturing it
    // lets the pre-PUT guard catch metadata-only changes that mint a new version
    // (which neither the ETag nor a same-second Last-Modified would reveal).
    let source_version_id = resp.version_id().map(str::to_string);
    // Re-apply the cold-data cutoff to the GET's *authoritative* Last-Modified.
    // The listing-time pre-skip uses the ListObjectsV2 timestamp, which can be
    // stale if the key was overwritten between listing and this GET — re-check
    // here (conservatively: a missing timestamp under an age gate is a skip) so a
    // freshly-hot object can never be fetched, verified, and rewritten.
    if let Some(cut) = cutoff_secs {
        match resp.last_modified() {
            Some(lm) if lm.secs() > cut => return Ok(Outcome::TooNew),
            None => return Ok(Outcome::UnknownAge),
            _ => {}
        }
    }
    // Backend server-side encryption: a GET transparently decrypts SSE-S3/KMS, so
    // re-PUTting without the original SSE/key/context would silently drop the
    // object's encryption. Skip rather than risk that.
    if resp.server_side_encryption().is_some() || resp.sse_customer_algorithm().is_some() {
        return Ok(Outcome::Encrypted);
    }
    // Object Lock retention / legal hold: a re-PUT creates a new version with
    // different compliance semantics (or is blocked) — skip rather than touch it.
    if resp.object_lock_mode().is_some()
        || resp.object_lock_retain_until_date().is_some()
        || resp.object_lock_legal_hold_status().is_some()
    {
        return Ok(Outcome::Locked);
    }
    // An `Expires` header carries cache/lifecycle semantics the rewrite can't
    // cleanly round-trip (PutObject only re-derives it from a deprecated typed
    // field). Consistent with the SSE / Object-Lock / sort-column skips above,
    // skip such objects rather than silently dropping the header.
    if resp.expires_string().is_some() {
        return Ok(Outcome::HasExpires);
    }
    // Without a source ETag the conflict guard / conditional PUT can't protect
    // an overwrite — skip early. Enforced in dry-run too (not just execute) so a
    // dry-run is an honest preflight: it reports the same EtagUnavailable skips
    // execute would make, instead of claiming the object would be recompacted.
    if source_etag.is_none() {
        return Ok(Outcome::EtagUnavailable);
    }
    // Pre-collect guard: refuse to buffer a body whose declared length already
    // exceeds the cap (a raced grow / oversized object), before reading it.
    if let Some(cl) = resp.content_length()
        && u64::try_from(cl).is_ok_and(|n| n > params.max_body_bytes)
    {
        return Ok(Outcome::TooLarge);
    }
    // Preserved object headers + user metadata are captured later from the
    // pre-PUT HEAD (not this GET) so they reflect the freshest state and the
    // metadata-replay race window is just HEAD->PUT, not GET->PUT.
    let make_tmp = || -> std::io::Result<tempfile::NamedTempFile> {
        match params.tmp_dir.as_deref() {
            Some(dir) => tempfile::NamedTempFile::new_in(dir),
            None => tempfile::NamedTempFile::new(),
        }
    };
    // Spool the GET body straight to a capped temp file instead of buffering it
    // in RAM. Every Parquet pass below (magic check, footer preflight, re-encode,
    // verify) range-reads off this file via ChunkReader, so peak memory is one
    // decoded batch + the writer's in-progress row group — independent of the
    // object's size. Capped: abort as soon as the body crosses the cap, so a
    // missing/wrong Content-Length on a nonconforming backend can't fill disk.
    let input_tmp = make_tmp()?;
    {
        let mut in_writer = input_tmp.as_file().try_clone()?;
        let mut stream = resp.body;
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            written = written.saturating_add(chunk.len() as u64);
            if written > params.max_body_bytes {
                return Ok(Outcome::TooLarge);
            }
            std::io::Write::write_all(&mut in_writer, &chunk)?;
        }
        std::io::Write::flush(&mut in_writer)?;
    }
    let before = input_tmp.as_file().metadata()?.len();
    let (is_parquet, footer_len) = parquet_trailer(&mut input_tmp.reopen()?)?;
    if !is_parquet {
        return Ok(Outcome::NotParquet);
    }
    // Guard the footer allocation BEFORE try_new parses it: a hostile/corrupt
    // object can declare a gigantic Thrift footer and OOM the reader before any
    // row-group cap applies. Skip such objects rather than risk the allocation.
    if footer_len.is_some_and(|n| n > MAX_FOOTER_BYTES) {
        return Ok(Outcome::TooLarge);
    }
    // Footer preflight (reads the footer, unspoofable): idempotency skip, an
    // uncompressed-row-group OOM guard, and an unsupported-feature guard so we
    // never silently drop query-planning metadata we can't reproduce. A
    // `.parquet`-suffixed object whose footer is PAR1-framed but UNPARSEABLE is a
    // corrupt object the operator should know about — surface it as a hard
    // failure (nonzero exit) rather than a quiet skip, while still not writing
    // anything and letting the rest of the batch continue.
    match ParquetRecordBatchReaderBuilder::try_new(input_tmp.reopen()?) {
        Ok(b) => {
            let meta = b.metadata();
            if all_columns_already_zstd(meta) {
                return Ok(Outcome::AlreadyZstd);
            }
            for rg in meta.row_groups() {
                if rg.total_byte_size().max(0) as u64 > params.max_uncompressed_row_group_bytes {
                    return Ok(Outcome::TooLarge);
                }
                // sort-order and bloom-filter metadata are query-planning hints
                // the Arrow re-encode does not carry — skip rather than drop them
                if rg.sorting_columns().is_some_and(|s| !s.is_empty()) {
                    return Ok(Outcome::UnsupportedFooter);
                }
                if rg
                    .columns()
                    .iter()
                    .any(|c| c.bloom_filter_offset().is_some())
                {
                    return Ok(Outcome::UnsupportedFooter);
                }
            }
        }
        Err(e) => {
            tracing::warn!(key = %key, error = %e, "parquet-recompact: footer unparseable (corrupt object)");
            return Err(format!("corrupt/unparseable Parquet footer: {e}").into());
        }
    }

    // Spool the re-encode to its own temp file so the rewritten Parquet never
    // sits in RAM either. Honor --tmp-dir for both spools.
    let tmp = make_tmp()?;
    let write_handle = tmp.as_file().try_clone()?;
    let (_rows, _cols, row_groups) = match recompress_to_sink(
        || {
            input_tmp
                .reopen()
                .map_err(|e| ParquetRecompactError::VerifyFailed(format!("reopen temp input: {e}")))
        },
        params.target_zstd_level,
        write_handle,
        params.max_uncompressed_row_group_bytes,
        params.max_body_bytes,
    ) {
        Ok(v) => v,
        // live encoder memory blew past the row-group cap, or the output grew
        // past the body cap — treat both as too-large (a skip), not a hard
        // failure, so the batch keeps going
        Err(ParquetRecompactError::WriterMemoryExceeded(bytes)) => {
            tracing::warn!(key = %key, live_bytes = bytes, "parquet-recompact: writer memory exceeded cap, skipping");
            return Ok(Outcome::TooLarge);
        }
        Err(ParquetRecompactError::OutputTooLarge(bytes)) => {
            tracing::warn!(key = %key, cap_bytes = bytes, "parquet-recompact: re-encoded output exceeded cap, skipping");
            return Ok(Outcome::TooLarge);
        }
        Err(e) => return Err(Box::new(e)),
    };
    let after = tmp.as_file().metadata()?.len();
    // Verify FIRST (before the gain gate), so a corrupt/changed output is always
    // surfaced as `verify-failed` rather than being masked as `low-gain`. The
    // verify is value-for-value, streamed batch-by-batch (bounded memory), and
    // runs even in dry-run. A mismatch (incl. an exotic representation the
    // verifier can't match) is a SKIP, never a hard failure — we don't overwrite
    // and one odd file doesn't fail the whole batch.
    match verify_equal(
        || {
            input_tmp
                .reopen()
                .map_err(|e| ParquetRecompactError::VerifyFailed(format!("reopen temp input: {e}")))
        },
        row_groups,
        || {
            tmp.reopen().map_err(|e| {
                ParquetRecompactError::VerifyFailed(format!("reopen temp output: {e}"))
            })
        },
        params.max_uncompressed_row_group_bytes,
    ) {
        Ok(()) => {}
        // Decoded VALUE mismatch after a successful encode — never overwrite, and
        // surface it as its own loud counter so an operator can investigate. NOT a
        // hard failure: the bitwise ArrayData compare can also trip on benign
        // representation drift (e.g. an explicitly Dictionary-typed schema's id
        // ordering), so failing the run on one such file would be wrong; a true
        // writer regression instead shows up as this count across many objects.
        Err(e @ ParquetRecompactError::VerifyValueMismatch(_)) => {
            if params.tolerate_value_mismatch {
                tracing::warn!(key = %key, error = %e, "parquet-recompact: VALUE mismatch tolerated — not overwriting (investigate)");
                return Ok(Outcome::ValueMismatch);
            }
            tracing::error!(key = %key, error = %e, "parquet-recompact: VALUE mismatch — not overwriting, failing object (use --tolerate-value-mismatch to downgrade to a skip)");
            return Err(Box::new(e));
        }
        // Structural / conservative verify failure (exotic schema we decline to
        // rewrite) — a safe skip, never an overwrite, batch continues.
        Err(e) => {
            tracing::warn!(key = %key, error = %e, "parquet-recompact: verify failed (conservative skip)");
            return Ok(Outcome::VerifyFailed);
        }
    }
    let saved = if before > 0 {
        (before as f64 - after as f64) / before as f64
    } else {
        0.0
    };
    if saved * 100.0 < params.min_gain_percent {
        return Ok(Outcome::LowGain);
    }
    // S3 single-PUT ceiling — skip rather than fail late at PutObject time
    if after > SINGLE_PUT_LIMIT {
        return Ok(Outcome::TooLarge);
    }
    if !params.execute {
        // Dry-run is an honest preview of execute: surface a tags-unreadable
        // object here too (execute would skip it), so the dry-run report matches
        // what execute would actually do rather than over-counting recompactions.
        if !params.no_tags
            && let Err(e) = crate::migrate::fetch_tags(client, bucket, key).await
        {
            if e.unreadable {
                return Ok(Outcome::TagsUnreadable);
            }
            return Err(e.cause.into());
        }
        return Ok(Outcome::Recompacted { before, after });
    }

    // conflict guard: re-check the ETag right before overwriting AND make the PUT
    // itself conditional on the source ETag via If-Match where the backend
    // supports it — a precondition failure (concurrent writer) is reported as a
    // race, not a hard failure.
    let head = client.head_object().bucket(bucket).key(key).send().await?;
    if head.e_tag().map(crate::migrate::normalize_etag) != source_etag {
        return Ok(Outcome::EtagRaced);
    }
    // Defense in depth on top of If-Match: also skip if Last-Modified moved
    // between GET and this HEAD (a concurrent CONTENT rewrite that somehow kept
    // the same ETag, or a backend that re-stamps Last-Modified). NOTE this is
    // best-effort and does NOT fully close tag-only / metadata-only races:
    // object tagging is an S3 subresource that need not bump either the ETag or
    // Last-Modified, and Last-Modified has whole-second granularity. That
    // residual race is unclosable on an unversioned bucket — hence this command
    // targets cold/quiescent prefixes (see `--older-than`), and on a versioned
    // bucket the prior version is retained regardless.
    if head.last_modified() != source_last_modified.as_ref() {
        return Ok(Outcome::EtagRaced);
    }
    // On a versioned bucket, a new version-id since the GET means a concurrent
    // change (including a metadata-only one that wouldn't move ETag/Last-Modified)
    // — closes that race where version-ids are available (unversioned buckets
    // expose none, so the best-effort caveat above still applies there).
    if source_version_id.is_some() && head.version_id().map(str::to_string) != source_version_id {
        return Ok(Outcome::EtagRaced);
    }
    // Re-apply the safety skips against the pre-PUT HEAD: SSE / Object-Lock /
    // Expires can be added to an object independently of its bytes (so the ETag
    // and Last-Modified wouldn't move), and we must not overwrite an object that
    // became encrypted / locked / expiry-bearing after the initial GET.
    if head.server_side_encryption().is_some() || head.sse_customer_algorithm().is_some() {
        return Ok(Outcome::Encrypted);
    }
    if head.object_lock_mode().is_some()
        || head.object_lock_retain_until_date().is_some()
        || head.object_lock_legal_hold_status().is_some()
    {
        return Ok(Outcome::Locked);
    }
    if head.expires_string().is_some() {
        return Ok(Outcome::HasExpires);
    }
    // Source the preserved headers + user metadata from THIS pre-PUT HEAD (not
    // the earlier GET), so a metadata-only change during the download/encode/
    // verify window isn't replayed stale — the replay window is just HEAD->PUT.
    // (Still best-effort: a metadata-only change in that final window can't be
    // CAS-protected on an unversioned bucket; see the note above.)
    let content_type = head.content_type().map(str::to_string);
    let cache_control = head.cache_control().map(str::to_string);
    let content_disposition = head.content_disposition().map(str::to_string);
    let content_encoding = head.content_encoding().map(str::to_string);
    let content_language = head.content_language().map(str::to_string);
    let website_redirect_location = head.website_redirect_location().map(str::to_string);
    // omit a default STANDARD class on PUT (some backends are strict about it)
    let storage_class = head
        .storage_class()
        .cloned()
        .filter(|s| s.as_str() != "STANDARD");
    let mut metadata = head.metadata().cloned().unwrap_or_default();
    // Carry tags over (unless --no-tags), fetched AFTER the conflict HEAD so the
    // tag-read→PUT window is as narrow as possible (the tag subresource isn't
    // CAS-protectable; this only narrows the residual race). An unreadable
    // tagging API skips the object rather than silently stripping its tags.
    let tags = if params.no_tags {
        Vec::new()
    } else {
        match crate::migrate::fetch_tags(client, bucket, key).await {
            Ok(t) => t,
            Err(e) if e.unreadable => return Ok(Outcome::TagsUnreadable),
            Err(e) => return Err(e.cause.into()),
        }
    };

    // strip any stale S4 gateway control metadata (e.g. s4-codec / s4-framed) so
    // the native Parquet isn't later mis-served as an S4 frame, then add our
    // level stamp. Case-insensitive: S3 user-metadata keys are case-insensitive,
    // so match the canonical `strip_reserved_client_metadata` helper rather than
    // a case-sensitive prefix that a casing-preserving backend could slip past.
    // The level stamp is purely INFORMATIONAL (records the level this rewrite
    // used) — it is NOT the idempotency mechanism: re-runs skip already-zstd
    // files by reading the Parquet footer (`all_columns_already_zstd`,
    // unspoofable), never by trusting this metadata.
    metadata.retain(|k, _| !k.to_ascii_lowercase().starts_with("s4-"));
    metadata.insert(
        "s4-parquet-zstd-level".to_string(),
        params.target_zstd_level.to_string(),
    );
    // Per-operation nonce: stamped on the PUT and matched on ambiguous-PUT
    // recovery (below) so a *pre-existing* stamp from an earlier run can never be
    // mistaken for this rewrite having committed. Fresh per object per run.
    let op_id = uuid::Uuid::new_v4().to_string();
    metadata.insert("s4-parquet-op-id".to_string(), op_id.clone());
    let body_stream = ByteStream::from_path(tmp.path()).await?;
    let put = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body_stream)
        .set_if_match(raw_etag.clone())
        .set_metadata(Some(metadata))
        .set_content_type(content_type)
        .set_cache_control(cache_control)
        .set_content_disposition(content_disposition)
        .set_content_encoding(content_encoding)
        .set_content_language(content_language)
        .set_website_redirect_location(website_redirect_location)
        .set_storage_class(storage_class)
        .set_tagging(if tags.is_empty() {
            None
        } else {
            Some(crate::migrate::encode_tagging(&tags))
        })
        .send()
        .await;
    match put {
        Ok(_) => Ok(Outcome::Recompacted { before, after }),
        Err(e) => {
            // 412 Precondition Failed / 409 Conflict = lost the If-Match race
            let status = e.raw_response().map(|r| r.status().as_u16());
            if status == Some(412) || status == Some(409) {
                Ok(Outcome::EtagRaced)
            } else {
                // Ambiguous PUT: a transport error after the server may have
                // already committed the overwrite (the classic "committed but
                // the client lost the response"). HEAD-probe for THIS operation's
                // unique nonce — only our just-issued PUT could have written it,
                // so a pre-existing stamp from an earlier run can't false-positive.
                // If present, the rewrite landed; treat as recompacted rather
                // than falsely reporting a hard failure. Otherwise it genuinely
                // failed.
                match client.head_object().bucket(bucket).key(key).send().await {
                    Ok(h)
                        if h.metadata()
                            .and_then(|m| m.get("s4-parquet-op-id"))
                            .is_some_and(|v| v == &op_id) =>
                    {
                        tracing::warn!(key = %key, error = %e, "parquet-recompact: PUT errored but object carries this op's nonce — treating committed-but-lost-response as recompacted");
                        Ok(Outcome::Recompacted { before, after })
                    }
                    _ => Err(Box::new(e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    fn write_parquet(batch: &RecordBatch, codec: Compression, rg_size: usize) -> Bytes {
        let props = WriterProperties::builder()
            .set_compression(codec)
            .set_max_row_group_size(rg_size)
            .build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props)).unwrap();
        w.write(batch).unwrap();
        w.close().unwrap();
        Bytes::from(buf)
    }

    fn log_batch(rows: usize) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("level", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
        ]));
        let ids: Int64Array = (0..rows as i64).collect();
        let levels: StringArray = (0..rows)
            .map(|i| {
                if i % 7 == 0 {
                    None
                } else {
                    Some(["INFO", "WARN", "ERROR"][i % 3])
                }
            })
            .collect();
        // include NaN + nulls to exercise the buffer-wise verify
        let scores: Float64Array = (0..rows)
            .map(|i| match i % 5 {
                0 => None,
                1 => Some(f64::NAN),
                _ => Some(i as f64 * 0.5),
            })
            .collect();
        RecordBatch::try_new(
            schema,
            vec![Arc::new(ids), Arc::new(levels), Arc::new(scores)],
        )
        .unwrap()
    }

    #[test]
    fn rejects_non_parquet() {
        assert!(!looks_like_parquet(b"not parquet"));
        assert!(matches!(
            recompress_parquet(Bytes::from_static(b"not parquet at all"), 3),
            Err(ParquetRecompactError::NotParquet)
        ));
    }

    #[test]
    fn snappy_to_zstd_shrinks_preserves_data_rowgroups_and_nulls_nan() {
        let batch = log_batch(60_000);
        // two row groups in, two row groups out (boundaries preserved)
        let input = write_parquet(&batch, Compression::SNAPPY, 30_000);
        let input_len = input.len();
        let (out, stats) = recompress_parquet_verified(input, 3).unwrap();
        assert!(looks_like_parquet(&out));
        assert_eq!(stats.rows, 60_000);
        assert_eq!(stats.columns, 3);
        assert_eq!(
            stats.row_groups, 2,
            "input row-group count must be preserved"
        );
        let out_meta = ParquetRecordBatchReaderBuilder::try_new(out.clone())
            .unwrap()
            .metadata()
            .clone();
        assert_eq!(out_meta.num_row_groups(), 2);
        assert!(
            all_columns_already_zstd(&out_meta),
            "output columns must be zstd"
        );
        assert!(stats.output_len < input_len, "zstd should beat snappy here");
    }

    #[test]
    fn empty_file_zero_rows_verifies() {
        // a schema'd Parquet with 0 rows must round-trip + verify cleanly
        let batch = log_batch(0);
        let input = write_parquet(&batch, Compression::SNAPPY, 10_000);
        let (out, stats) = recompress_parquet_verified(input, 3).unwrap();
        assert!(looks_like_parquet(&out));
        assert_eq!(stats.rows, 0);
    }

    #[test]
    fn already_zstd_is_detected() {
        let batch = log_batch(10_000);
        let zin = write_parquet(
            &batch,
            Compression::ZSTD(ZstdLevel::try_new(3).unwrap()),
            10_000,
        );
        let meta = ParquetRecordBatchReaderBuilder::try_new(zin)
            .unwrap()
            .metadata()
            .clone();
        assert!(all_columns_already_zstd(&meta));
        // and a snappy file is NOT flagged as already-zstd
        let sin = write_parquet(&batch, Compression::SNAPPY, 10_000);
        let smeta = ParquetRecordBatchReaderBuilder::try_new(sin)
            .unwrap()
            .metadata()
            .clone();
        assert!(!all_columns_already_zstd(&smeta));
    }

    #[test]
    fn int96_physical_drift_is_caught() {
        use parquet::data_type::{Int96, Int96Type};
        use parquet::file::writer::SerializedFileWriter;
        use parquet::schema::parser::parse_message_type;

        // Legacy-Spark style: a timestamp stored as the Parquet INT96 physical
        // type. The Arrow reader normalizes INT96 -> Timestamp(ns) and ArrowWriter
        // writes it back as INT64, so the Arrow schema + decoded values still
        // compare equal — only the on-disk *physical* type changed. The
        // schema-descriptor diff must catch this and SKIP (never overwrite).
        let schema = Arc::new(parse_message_type("message schema { REQUIRED INT96 ts; }").unwrap());
        let props = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .build(),
        );
        let mut buf = Vec::new();
        {
            let mut w = SerializedFileWriter::new(&mut buf, schema, props).unwrap();
            let mut rg = w.next_row_group().unwrap();
            let mut col = rg.next_column().unwrap().unwrap();
            let (mut a, mut b) = (Int96::new(), Int96::new());
            a.set_data(0, 0, 2_451_545);
            b.set_data(1, 0, 2_451_546);
            col.typed::<Int96Type>()
                .write_batch(&[a, b], None, None)
                .unwrap();
            col.close().unwrap();
            rg.close().unwrap();
            w.close().unwrap();
        }
        match recompress_parquet_verified(Bytes::from(buf), 3) {
            Err(ParquetRecompactError::VerifyFailed(msg)) => {
                assert!(
                    msg.contains("physical schema"),
                    "expected physical-schema-drift message, got: {msg}"
                );
            }
            other => panic!("INT96 drift must be rejected as VerifyFailed, got {other:?}"),
        }
    }

    #[test]
    fn preserves_key_value_metadata() {
        let batch = log_batch(5_000);
        // write with snappy + a parquet KV pair, then recompact and check it survives
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
                "pandas".to_string(),
                "{\"index_columns\": [\"id\"]}".to_string(),
            )]))
            .build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let (out, _) = recompress_parquet_verified(Bytes::from(buf), 3).unwrap();
        let meta = ParquetRecordBatchReaderBuilder::try_new(out)
            .unwrap()
            .metadata()
            .clone();
        let kv = meta
            .file_metadata()
            .key_value_metadata()
            .expect("KV preserved");
        assert!(
            kv.iter().any(|k| k.key == "pandas"),
            "pandas KV metadata must survive"
        );
    }
}
