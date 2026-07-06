//! `s4-codec` — standalone offline decoder CLI for S4-written objects.
//!
//! The escape-hatch / no-lock-in story (docs/trust.md §1): objects the S4
//! gateway wrote stay readable **without any gateway running**. Fetch the
//! raw stored bytes straight off the backend bucket (`aws s3api get-object`
//! against the backend endpoint, not S4) and this binary decodes them:
//!
//! - `s4-codec decode <INPUT> [-o OUT]` — parse the `S4F2` frame sequence,
//!   dispatch the per-frame codec, verify each frame's CRC32C of the
//!   original bytes, concatenate.
//! - `s4-codec inspect <INPUT>` — per-frame table (codec / sizes / CRC32C),
//!   no decode. `--json` for machine output.
//! - `s4-codec index <INPUT.s4index>` — decode an `S4IX` sidecar layout.
//!
//! CPU-only by design: covers `passthrough` / `cpu-zstd` / `cpu-gzip` /
//! `cpu-zstd-dict` (with `--dict`) frames. GPU-codec (`nvcomp-*` /
//! `dietgpu-ans`) frames and SSE-encrypted (`S4E*`) bodies fail with a
//! hard, self-explanatory error pointing at the `s4` gateway binary —
//! never silently wrong bytes.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use s4_codec::index::{FrameIndex, decode_index};
use s4_codec::multipart::{FRAME_HEADER_BYTES, FrameHeader, FrameIter};
use s4_codec::{ChunkManifest, CodecKind, cpu_gzip, cpu_zstd, cpu_zstd_dict};

/// SSE-S4 body magics (`"S4E1"`..`"S4E6"`). KEEP IN SYNC with
/// `crates/s4-server/src/sse.rs` (`SSE_MAGIC_V1`..`SSE_MAGIC_V6`). The
/// codec crate deliberately contains no cryptography — the CLI only
/// *recognizes* encrypted bodies so it can fail loudly instead of
/// mis-parsing ciphertext as frames.
const SSE_MAGICS: [&[u8; 4]; 6] = [b"S4E1", b"S4E2", b"S4E3", b"S4E4", b"S4E5", b"S4E6"];

fn sse_variant(body: &[u8]) -> Option<&'static str> {
    const NAMES: [&str; 6] = ["S4E1", "S4E2", "S4E3", "S4E4", "S4E5", "S4E6"];
    let head = body.get(..4)?;
    SSE_MAGICS
        .iter()
        .position(|m| &m[..] == head)
        .map(|i| NAMES[i])
}

#[derive(Debug, Parser)]
#[command(
    name = "s4-codec",
    version,
    about = "Offline decoder for S4-written objects — no gateway required",
    long_about = "Offline decoder for S4-written objects — no gateway required.\n\n\
        Fetch the raw stored bytes straight from your backend bucket (e.g.\n\
        `aws s3api get-object --endpoint-url <backend> ...`, NOT through S4)\n\
        and decode / inspect them here. Covers the CPU codec subset\n\
        (passthrough, cpu-zstd, cpu-gzip, cpu-zstd-dict); GPU-codec\n\
        (nvcomp-*) frames and SSE-encrypted objects fail with a hard error\n\
        pointing at the `s4` gateway binary.\n\n\
        v1 limitation: each command reads the whole input file into memory,\n\
        so objects up to a few GiB are fine; decode larger objects through\n\
        the `s4` gateway."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Decode a framed S4F2 object back to its original bytes
    /// (per-frame CRC32C verified)
    Decode {
        /// Framed S4F2 object file (raw bytes fetched from the backend bucket)
        input: PathBuf,

        /// Output file. Defaults to stdout; writing binary to a terminal
        /// is refused — pipe, redirect, or pass -o
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Raw zstd dictionary bytes for `cpu-zstd-dict` frames (the
        /// gateway stores dictionaries at `.s4dict/<dict-id>` in the bucket)
        #[arg(long)]
        dict: Option<PathBuf>,
    },
    /// Print the per-frame table (codec, sizes, CRC32C) without decoding
    Inspect {
        /// Framed S4F2 object file
        input: PathBuf,

        /// Machine-readable JSON output instead of the table
        #[arg(long)]
        json: bool,
    },
    /// Decode an S4IX `.s4index` sidecar and print its layout
    Index {
        /// Sidecar file (`<key>.s4index` fetched from the backend bucket)
        input: PathBuf,

        /// Machine-readable JSON output instead of the table
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Decode {
            input,
            output,
            dict,
        } => cmd_decode(&input, output.as_deref(), dict.as_deref()),
        Cmd::Inspect { input, json } => cmd_inspect(&input, json),
        Cmd::Index { input, json } => cmd_index(&input, json),
    }
}

/// Read a framed object file, rejecting SSE-encrypted / empty bodies with
/// self-explanatory errors before any frame parsing happens.
fn read_framed_object(path: &Path) -> Result<Bytes> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if let Some(variant) = sse_variant(&raw) {
        bail!(
            "{} is SSE-encrypted (magic {variant}): this CLI has no decryption \
             support by design — the keys live with the gateway operator. GET \
             the object through the `s4` gateway binary (with your --sse-* key \
             flags), then decode the plaintext",
            path.display()
        );
    }
    if raw.is_empty() {
        bail!(
            "{}: file is empty — not an S4F2 framed object",
            path.display()
        );
    }
    Ok(Bytes::from(raw))
}

/// Context string for a frame-parse failure. Frame 0 failures are almost
/// always "this file was never framed", so say so.
fn frame_parse_context(i: usize) -> String {
    if i == 0 {
        "parsing frame 0 — is this a framed S4F2 object? passthrough objects \
         are stored as the raw original bytes (no decode needed); check the \
         object's `x-amz-meta-s4-framed` metadata"
            .to_owned()
    } else {
        format!("parsing frame {i}")
    }
}

/// Decode one frame's payload with the codec its header names, verifying
/// the CRC32C of the original bytes. CPU codecs only — GPU frames fail
/// hard, naming the codec.
fn decode_frame(header: &FrameHeader, payload: &Bytes, dict: Option<&[u8]>) -> Result<Vec<u8>> {
    let manifest = ChunkManifest {
        codec: header.codec,
        original_size: header.original_size,
        compressed_size: header.compressed_size,
        crc32c: header.crc32c,
    };
    match header.codec {
        CodecKind::Passthrough => {
            // Payload IS the original bytes; verify CRC32C + size directly.
            let got = crc32c::crc32c(payload);
            if got != header.crc32c {
                bail!(
                    "crc32c mismatch (chunk corruption?): expected {:#010x}, got {got:#010x}",
                    header.crc32c
                );
            }
            if payload.len() as u64 != header.original_size {
                bail!(
                    "size mismatch: header says {} original bytes, payload is {}",
                    header.original_size,
                    payload.len()
                );
            }
            Ok(payload.to_vec())
        }
        CodecKind::CpuZstd => Ok(cpu_zstd::decompress_blocking(payload, &manifest)?),
        CodecKind::CpuGzip => Ok(cpu_gzip::decompress_blocking(payload, &manifest)?),
        CodecKind::CpuZstdDict => {
            let dict = dict.context(
                "frame uses codec `cpu-zstd-dict` but no --dict was given; pass \
                 `--dict <FILE>` with the raw zstd dictionary bytes (the gateway \
                 stores them at `.s4dict/<dict-id>` in the bucket; the object's \
                 `x-amz-meta-s4-dict-id` metadata names which one)",
            )?;
            Ok(cpu_zstd_dict::decompress_blocking(
                payload, dict, &manifest,
            )?)
        }
        CodecKind::NvcompZstd
        | CodecKind::NvcompBitcomp
        | CodecKind::NvcompGans
        | CodecKind::NvcompGDeflate
        | CodecKind::DietGpuAns => bail!(
            "frame uses GPU codec `{}` — this CPU-only CLI cannot decode it; \
             GET the object through the `s4` gateway binary instead (its GET \
             path decompresses transparently; the open-source `s4-server` \
             crate built with `--features nvcomp-gpu` provides the decoder)",
            header.codec.as_str()
        ),
        // `CodecKind` is #[non_exhaustive]; a frame written by a newer
        // gateway may name a codec this build doesn't know.
        other => bail!(
            "frame uses codec `{}` which this build of s4-codec does not \
             support — upgrade s4-codec, or GET the object through the `s4` \
             gateway binary",
            other.as_str()
        ),
    }
}

fn cmd_decode(input: &Path, output: Option<&Path>, dict: Option<&Path>) -> Result<()> {
    let body = read_framed_object(input)?;
    let dict_bytes = dict
        .map(|p| std::fs::read(p).with_context(|| format!("reading --dict {}", p.display())))
        .transpose()?;

    let mut decoded: Vec<u8> = Vec::new();
    let mut compressed_total: u64 = 0;
    let mut frames = 0usize;
    for (i, item) in FrameIter::new(body).enumerate() {
        let (header, payload) = item.with_context(|| frame_parse_context(i))?;
        let bytes = decode_frame(&header, &payload, dict_bytes.as_deref())
            .with_context(|| format!("decoding frame {i} (codec `{}`)", header.codec.as_str()))?;
        compressed_total += header.compressed_size;
        decoded.extend_from_slice(&bytes);
        frames += 1;
    }

    match output {
        Some(path) => {
            std::fs::write(path, &decoded).with_context(|| format!("writing {}", path.display()))?
        }
        None => {
            let mut stdout = std::io::stdout();
            if stdout.is_terminal() {
                bail!(
                    "refusing to write binary output to a terminal — pass \
                     `-o <FILE>` or pipe/redirect stdout"
                );
            }
            stdout.write_all(&decoded).context("writing to stdout")?;
            stdout.flush().context("flushing stdout")?;
        }
    }
    // Summary goes to stderr so piped stdout stays byte-pure.
    eprintln!(
        "s4-codec: decoded {frames} frame(s), {compressed_total} compressed bytes -> {} original bytes (per-frame crc32c verified)",
        decoded.len()
    );
    Ok(())
}

fn cmd_inspect(input: &Path, json: bool) -> Result<()> {
    let body = read_framed_object(input)?;
    let file_size = body.len() as u64;

    let mut headers: Vec<FrameHeader> = Vec::new();
    for (i, item) in FrameIter::new(body).enumerate() {
        let (header, _payload) = item.with_context(|| frame_parse_context(i))?;
        headers.push(header);
    }
    let total_original: u64 = headers.iter().map(|h| h.original_size).sum();
    let total_compressed: u64 = headers.iter().map(|h| h.compressed_size).sum();
    // Bytes not accounted for by `header + payload` are S4P1 padding
    // (headers included), present when multipart parts were padded up to
    // the S3 5 MiB minimum.
    let framed_bytes: u64 = headers
        .iter()
        .map(|h| FRAME_HEADER_BYTES as u64 + h.compressed_size)
        .sum();
    let padding = file_size.saturating_sub(framed_bytes);

    if json {
        use std::fmt::Write as _;
        let mut out = String::from("{\n  \"frames\": [\n");
        for (i, h) in headers.iter().enumerate() {
            let comma = if i + 1 == headers.len() { "" } else { "," };
            let _ = writeln!(
                out,
                "    {{\"codec\":\"{}\",\"original_size\":{},\"compressed_size\":{},\"crc32c\":{}}}{comma}",
                h.codec.as_str(),
                h.original_size,
                h.compressed_size,
                h.crc32c
            );
        }
        let _ = write!(
            out,
            "  ],\n  \"total_frames\": {},\n  \"total_original_size\": {total_original},\n  \
             \"total_compressed_size\": {total_compressed},\n  \"file_size\": {file_size},\n  \
             \"padding_bytes\": {padding}\n}}",
            headers.len()
        );
        println!("{out}");
    } else {
        println!(
            "{:<6} {:<15} {:>14} {:>14}   CRC32C",
            "FRAME", "CODEC", "ORIGINAL", "COMPRESSED"
        );
        for (i, h) in headers.iter().enumerate() {
            println!(
                "{i:<6} {:<15} {:>14} {:>14}   {:#010x}",
                h.codec.as_str(),
                h.original_size,
                h.compressed_size,
                h.crc32c
            );
        }
        println!(
            "total: {} frame(s), {total_original} original bytes, {total_compressed} compressed payload bytes, {file_size} file bytes ({padding} padding)",
            headers.len()
        );
    }
    Ok(())
}

/// Minimal JSON string escaping (quotes, backslashes, control chars) for
/// the one free-form string the sidecar carries (the source ETag). The
/// crate has no serde_json dependency and the rest of the output is
/// numeric / fixed-token, so this is all the escaping `--json` needs.
fn json_escape(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn cmd_index(input: &Path, json: bool) -> Result<()> {
    let raw = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let idx: FrameIndex = decode_index(Bytes::from(raw)).with_context(|| {
        format!(
            "decoding {} as an S4IX sidecar (expected a `<key>.s4index` object)",
            input.display()
        )
    })?;

    if json {
        use std::fmt::Write as _;
        let etag = match &idx.source_etag {
            Some(e) => format!("\"{}\"", json_escape(e)),
            None => "null".to_owned(),
        };
        let scs = match idx.source_compressed_size {
            Some(n) => n.to_string(),
            None => "null".to_owned(),
        };
        let sse = match &idx.sse_v3 {
            Some(s) => format!(
                "{{\"enc_chunk_size\":{},\"enc_chunk_count\":{},\"enc_key_id\":{},\
                 \"enc_salt\":\"{}\",\"enc_plaintext_len\":{},\"enc_header_bytes\":{}}}",
                s.enc_chunk_size,
                s.enc_chunk_count,
                s.enc_key_id,
                hex(&s.enc_salt),
                s.enc_plaintext_len,
                s.enc_header_bytes
            ),
            None => "null".to_owned(),
        };
        let mut out = format!(
            "{{\n  \"total_frames\": {},\n  \"total_original_size\": {},\n  \
             \"total_padded_size\": {},\n  \"source_etag\": {etag},\n  \
             \"source_compressed_size\": {scs},\n  \"sse\": {sse},\n  \"entries\": [\n",
            idx.entries.len(),
            idx.total_original_size(),
            idx.total_padded_size
        );
        for (i, e) in idx.entries.iter().enumerate() {
            let comma = if i + 1 == idx.entries.len() { "" } else { "," };
            let _ = writeln!(
                out,
                "    {{\"original_offset\":{},\"original_size\":{},\"compressed_offset\":{},\"compressed_size\":{}}}{comma}",
                e.original_offset, e.original_size, e.compressed_offset, e.compressed_size
            );
        }
        out.push_str("  ]\n}");
        println!("{out}");
    } else {
        println!("S4IX sidecar: {}", input.display());
        println!("  entries:                 {}", idx.entries.len());
        println!("  total_original_size:     {}", idx.total_original_size());
        println!(
            "  total_padded_size:       {}   (backend object size incl. padding)",
            idx.total_padded_size
        );
        println!(
            "  source_etag:             {}",
            idx.source_etag
                .as_deref()
                .unwrap_or("(none — legacy v1 or unbound sidecar)")
        );
        println!(
            "  source_compressed_size:  {}",
            idx.source_compressed_size
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(none)".to_owned())
        );
        match &idx.sse_v3 {
            Some(s) => println!(
                "  sse (v3 S4E6 geometry):  chunk_size={} chunk_count={} key_id={} plaintext_len={} header_bytes={} salt={}",
                s.enc_chunk_size,
                s.enc_chunk_count,
                s.enc_key_id,
                s.enc_plaintext_len,
                s.enc_header_bytes,
                hex(&s.enc_salt)
            ),
            None => println!("  sse (v3 S4E6 geometry):  none"),
        }
        println!();
        println!(
            "{:<6} {:>14} {:>12} {:>14} {:>12}",
            "IDX", "ORIG_OFFSET", "ORIG_SIZE", "COMP_OFFSET", "COMP_SIZE"
        );
        for (i, e) in idx.entries.iter().enumerate() {
            println!(
                "{i:<6} {:>14} {:>12} {:>14} {:>12}",
                e.original_offset, e.original_size, e.compressed_offset, e.compressed_size
            );
        }
    }
    Ok(())
}
