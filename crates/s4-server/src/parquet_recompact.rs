//! Parquet-aware recompaction (`parquet-recompact` feature).
//!
//! Cold data-lake Parquet is overwhelmingly written with the **snappy** column
//! codec (fast, but weak). This module reads a Parquet object and re-encodes its
//! column data with **zstd**, producing a *native* Parquet file — still readable
//! by pyarrow / Spark / Trino / DuckDB with **no S4 in the read path**. It is an
//! offline recompaction (like `s4 recompact`), not the transparent gateway: the
//! output is a normal Parquet, just smaller.
//!
//! The re-encode goes through Arrow (`ParquetRecordBatchReader` → `ArrowWriter`
//! with `Compression::ZSTD`), which decodes every column and writes fresh column
//! chunks, so the writer keeps the footer / offsets / statistics internally
//! consistent. Logical data is preserved (verified here, and cross-checked with
//! pyarrow in the bench harness).

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::datatypes::SchemaRef;
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

/// Outcome of re-encoding one Parquet object.
#[derive(Debug, Clone)]
pub struct ReencodeStats {
    pub input_len: usize,
    pub output_len: usize,
    pub rows: usize,
    pub columns: usize,
}

impl ReencodeStats {
    /// Saving as a fraction in [−∞, 1]; positive = smaller. Negative means the
    /// re-encode grew the object (e.g. already-zstd / incompressible input).
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
    #[error("round-trip verification failed: {0}")]
    VerifyFailed(String),
}

/// Cheap check: does the blob start and end with the Parquet `PAR1` magic?
/// (Parquet files are framed `PAR1 ... <footer> PAR1`.)
pub fn looks_like_parquet(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[..4] == b"PAR1" && &bytes[bytes.len() - 4..] == b"PAR1"
}

/// Read every row group of a Parquet blob into a single concatenated
/// `RecordBatch` (used for round-trip equality checks).
fn read_all(bytes: Bytes) -> Result<(SchemaRef, RecordBatch), ParquetRecompactError> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let schema = builder.schema().clone();
    let reader = builder.build()?;
    let mut batches = Vec::new();
    for b in reader {
        batches.push(b?);
    }
    let merged = concat_batches(&schema, &batches)?;
    Ok((schema, merged))
}

/// Re-encode a Parquet blob's columns with zstd at `zstd_level`, keeping the
/// output a native Parquet file. Returns the new bytes and stats.
pub fn recompress_parquet(
    input: Bytes,
    zstd_level: i32,
) -> Result<(Vec<u8>, ReencodeStats), ParquetRecompactError> {
    if !looks_like_parquet(&input) {
        return Err(ParquetRecompactError::NotParquet);
    }
    let input_len = input.len();
    let builder = ParquetRecordBatchReaderBuilder::try_new(input.clone())?;
    let schema = builder.schema().clone();
    let columns = schema.fields().len();
    let reader = builder.build()?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(zstd_level)?))
        .build();
    let mut out: Vec<u8> = Vec::with_capacity(input_len);
    let mut writer = ArrowWriter::try_new(&mut out, schema, Some(props))?;
    let mut rows = 0usize;
    for batch in reader {
        let batch = batch?;
        rows += batch.num_rows();
        writer.write(&batch)?;
    }
    writer.close()?;

    let stats = ReencodeStats {
        input_len,
        output_len: out.len(),
        rows,
        columns,
    };
    Ok((out, stats))
}

/// Re-encode and verify the result reads back **logically identical** to the
/// input (schema + every column value), before any write. Returns the new bytes
/// and stats only if the round-trip is byte-for-byte equal at the value level.
pub fn recompress_parquet_verified(
    input: Bytes,
    zstd_level: i32,
) -> Result<(Vec<u8>, ReencodeStats), ParquetRecompactError> {
    let (out, stats) = recompress_parquet(input.clone(), zstd_level)?;
    let (orig_schema, orig) = read_all(input)?;
    let (new_schema, new) = read_all(Bytes::from(out.clone()))?;
    // Compare column data (not schema KV metadata, which the writer may rewrite).
    if orig_schema.fields() != new_schema.fields() {
        return Err(ParquetRecompactError::VerifyFailed(format!(
            "schema fields changed: {} cols -> {} cols",
            orig_schema.fields().len(),
            new_schema.fields().len()
        )));
    }
    if orig.num_rows() != new.num_rows() {
        return Err(ParquetRecompactError::VerifyFailed(format!(
            "row count changed: {} -> {}",
            orig.num_rows(),
            new.num_rows()
        )));
    }
    for (i, (a, b)) in orig.columns().iter().zip(new.columns()).enumerate() {
        // Compare the logical column values (ArrayData impls PartialEq; the
        // dynamic Array trait object does not).
        if a.to_data() != b.to_data() {
            return Err(ParquetRecompactError::VerifyFailed(format!(
                "column {i} ({}) differs after re-encode",
                orig_schema.field(i).name()
            )));
        }
    }
    Ok((out, stats))
}

// ---------------------------------------------------------------------------
// Backend orchestration: list a bucket/prefix, re-encode each Parquet object in
// place, and (with --execute) write it back. Dry-run by default.
// ---------------------------------------------------------------------------

use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;

const STAMP_KEY: &str = "s4-parquet-zstd-level";

#[derive(Debug, Clone)]
pub struct ParquetRecompactParams {
    pub execute: bool,
    pub target_zstd_level: i32,
    /// Skip the rewrite unless it shrinks the object by at least this percent.
    pub min_gain_percent: f64,
    /// Only consider keys ending with this suffix (default ".parquet").
    pub suffix: String,
    pub max_objects: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct ParquetRecompactReport {
    pub scanned: usize,
    pub recompacted: usize,
    pub skipped_suffix: usize,
    pub skipped_not_parquet: usize,
    pub skipped_low_gain: usize,
    pub skipped_already: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// One pass over `s3://bucket/prefix`: re-encode each Parquet object's columns
/// to zstd in place. Returns a report; with `params.execute == false` nothing is
/// written (dry-run).
pub async fn run_parquet_recompact(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    params: &ParquetRecompactParams,
) -> Result<ParquetRecompactReport, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let mut report = ParquetRecompactReport::default();
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
            if !key.ends_with(&params.suffix) {
                report.skipped_suffix += 1;
                continue;
            }
            report.scanned += 1;
            if let Some(max) = params.max_objects {
                if report.recompacted >= max {
                    return Ok(report);
                }
            }
            match recompact_one_object(client, bucket, key, params).await {
                Ok(Outcome::Recompacted { before, after }) => {
                    report.recompacted += 1;
                    report.bytes_before += before;
                    report.bytes_after += after;
                }
                Ok(Outcome::NotParquet) => report.skipped_not_parquet += 1,
                Ok(Outcome::LowGain) => report.skipped_low_gain += 1,
                Ok(Outcome::AlreadyDone) => report.skipped_already += 1,
                Err(e) => {
                    report.failed += 1;
                    eprintln!("parquet-recompact: {key}: {e}");
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

enum Outcome {
    Recompacted { before: u64, after: u64 },
    NotParquet,
    LowGain,
    AlreadyDone,
}

async fn recompact_one_object(
    client: &Client,
    bucket: &str,
    key: &str,
    params: &ParquetRecompactParams,
) -> Result<Outcome, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let resp = client.get_object().bucket(bucket).key(key).send().await?;
    // idempotency: skip if already recompacted to >= target level
    if let Some(meta) = resp.metadata() {
        if let Some(lvl) = meta.get(STAMP_KEY).and_then(|v| v.parse::<i32>().ok()) {
            if lvl >= params.target_zstd_level {
                return Ok(Outcome::AlreadyDone);
            }
        }
    }
    let content_type = resp.content_type().map(str::to_string);
    let mut metadata = resp.metadata().cloned().unwrap_or_default();
    let body = resp.body.collect().await?.into_bytes();
    let before = body.len() as u64;

    let (out, stats) = match recompress_parquet_verified(body, params.target_zstd_level) {
        Ok(v) => v,
        Err(ParquetRecompactError::NotParquet) => return Ok(Outcome::NotParquet),
        Err(e) => return Err(Box::new(e)),
    };
    let after = stats.output_len as u64;
    if stats.saved_fraction() * 100.0 < params.min_gain_percent {
        return Ok(Outcome::LowGain);
    }
    if !params.execute {
        return Ok(Outcome::Recompacted { before, after });
    }
    metadata.insert(STAMP_KEY.to_string(), params.target_zstd_level.to_string());
    let mut put = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(out))
        .set_metadata(Some(metadata));
    if let Some(ct) = content_type {
        put = put.content_type(ct);
    }
    put.send().await?;
    Ok(Outcome::Recompacted { before, after })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    /// Build a small, compressible snappy Parquet in memory.
    fn snappy_fixture(rows: usize) -> Bytes {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("level", DataType::Utf8, false),
            Field::new("msg", DataType::Utf8, false),
        ]));
        let ids: Int64Array = (0..rows as i64).collect();
        // low-cardinality, highly compressible columns
        let levels: StringArray = (0..rows)
            .map(|i| Some(["INFO", "WARN", "ERROR"][i % 3]))
            .collect();
        let msgs: StringArray = (0..rows)
            .map(|i| {
                format!(
                    "request completed path=/api/v1/items status=200 idx={}",
                    i % 100
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(Some)
            .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(ids), Arc::new(levels), Arc::new(msgs)],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        Bytes::from(buf)
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
    fn snappy_to_zstd_shrinks_and_preserves_data() {
        let input = snappy_fixture(50_000);
        let input_len = input.len();
        let (out, stats) = recompress_parquet_verified(input, 3).unwrap();
        assert!(looks_like_parquet(&out), "output must be native Parquet");
        assert_eq!(stats.rows, 50_000);
        assert_eq!(stats.columns, 3);
        assert_eq!(stats.input_len, input_len);
        assert_eq!(stats.output_len, out.len());
        // compressible fixture: zstd should beat snappy
        assert!(
            stats.output_len < input_len,
            "zstd ({}) should be smaller than snappy ({})",
            stats.output_len,
            input_len
        );
        assert!(stats.saved_fraction() > 0.0);
    }

    #[test]
    fn verified_roundtrip_detects_identity() {
        // A clean re-encode must pass verification (no VerifyFailed).
        let input = snappy_fixture(10_000);
        let res = recompress_parquet_verified(input, 9);
        assert!(res.is_ok(), "verified re-encode should succeed: {res:?}");
    }
}
