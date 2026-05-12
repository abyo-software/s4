//! `s4-codec-wasm` — browser/Node WASM decoder for S4F2-framed S4 objects
//! (v0.4 #24).
//!
//! ## Why this crate exists
//!
//! Frontend apps that read S4-stored objects directly from S3 (presigned
//! URL pattern) need to decompress the S4F2 wire format **without** a
//! round-trip through the S4 server. This crate exposes a tiny
//! `wasm-bindgen` surface — `decompressFramed` and `decompressSingle` —
//! that does exactly that, using the same `CodecKind` IDs and
//! `multipart::FrameIter` parser the server uses on the read path.
//!
//! ## Codec subset
//!
//! Only **CPU** codecs are supported in the browser (no GPU on a
//! `<script>` tag):
//!
//! - `passthrough` (id 0)
//! - `cpu-zstd`    (id 1)
//! - `cpu-gzip`    (id 7)
//!
//! Encountering a GPU-only codec id (`nvcomp-zstd`, `nvcomp-bitcomp`,
//! `nvcomp-gans`, `dietgpu-ans`, `nvcomp-gdeflate`) is a hard error —
//! the caller must route those reads through an S4 server instead.
//!
//! ## Build
//!
//! ```bash
//! # Raw cargo (no JS glue, just .wasm)
//! cargo build --release --target wasm32-unknown-unknown -p s4-codec-wasm
//!
//! # With wasm-pack (recommended — emits package.json-ready bundle)
//! wasm-pack build --release --target web crates/s4-codec-wasm
//! ```
//!
//! ## Note on async
//!
//! `s4-codec`'s `Codec` trait is async (server-side, runs on `tokio`),
//! but the browser has no tokio runtime. This crate calls the
//! `decompress_blocking` helpers in `s4_codec::cpu_zstd` / `cpu_gzip`
//! directly — those are sync wrappers that share the same
//! decompression-bomb cap and crc32c verify logic as the async path.

#![allow(non_snake_case)] // wasm-bindgen exports use camelCase (JS convention)

use bytes::Bytes;
use s4_codec::{
    ChunkManifest, CodecKind, cpu_gzip, cpu_zstd,
    multipart::{FrameIter, PADDING_MAGIC},
};
use wasm_bindgen::prelude::*;

/// Decompress an S4F2-framed object (the wire format S4 writes for both
/// single-PUT and multipart uploads since v0.2 #4). Walks the byte
/// stream as a sequence of `S4F2` frames + `S4P1` padding skips, calls
/// the CPU codec implementations per frame, and concats the per-frame
/// outputs into a single `Uint8Array`.
///
/// JS signature: `decompressFramed(bytes: Uint8Array): Uint8Array`.
///
/// Throws (= JS `Error`) on:
/// - bad / truncated frame
/// - unknown codec id (forward-incompatible writer)
/// - GPU-only codec (route via S4 server instead)
/// - codec mismatch / size mismatch / crc mismatch (= corruption)
#[wasm_bindgen(js_name = decompressFramed)]
pub fn decompress_framed(bytes: js_sys::Uint8Array) -> Result<js_sys::Uint8Array, JsValue> {
    let raw: Vec<u8> = bytes.to_vec();
    let total_in = raw.len();
    let input = Bytes::from(raw);

    let mut out: Vec<u8> = Vec::new();
    let mut frames_seen: usize = 0;
    for frame in FrameIter::new(input) {
        let (header, payload) = frame.map_err(|e| {
            JsValue::from_str(&format!(
                "s4-codec-wasm: S4F2 frame parse failed after {frames_seen} frames: {e}"
            ))
        })?;

        // Reject GPU-only codecs early with a specific error so callers can
        // route those reads through an S4 server (where the GPU path lives).
        match header.codec {
            CodecKind::Passthrough | CodecKind::CpuZstd | CodecKind::CpuGzip => {}
            unsupported => {
                return Err(JsValue::from_str(&format!(
                    "s4-codec-wasm: codec {} is not supported in the browser \
                     (GPU-only); fetch this object via an S4 server instead",
                    unsupported.as_str()
                )));
            }
        }

        let manifest = ChunkManifest {
            codec: header.codec,
            original_size: header.original_size,
            compressed_size: header.compressed_size,
            crc32c: header.crc32c,
        };

        let chunk = decompress_one(&payload, &manifest).map_err(|e| JsValue::from_str(&e))?;
        out.extend_from_slice(&chunk);
        frames_seen += 1;
    }

    if frames_seen == 0 {
        // Diagnostic: empty / non-S4F2 input. We don't fail outright (the
        // caller may legitimately pass an empty buffer) but warn so a
        // misuse surfaces in the dev console.
        web_sys::console::warn_1(&JsValue::from_str(&format!(
            "s4-codec-wasm: decompressFramed read {total_in} bytes but yielded 0 frames \
             (is this really an S4F2 object?)"
        )));
    }

    Ok(js_sys::Uint8Array::from(out.as_slice()))
}

/// Decompress a single (non-framed, legacy single-PUT) S4 object.
///
/// Pre-S4F2 single PUTs stored only the raw codec output in S3 with the
/// codec / original_size / crc32c carried in object metadata. This entry
/// point is for browser code that reads those legacy objects (rare in
/// new deployments — S4 has emitted S4F2 by default since v0.2).
///
/// JS signature:
/// `decompressSingle(bytes: Uint8Array, codec: string,
///   originalSize: bigint | number, crc32c: number): Uint8Array`.
///
/// `codec` strings: `"passthrough"`, `"cpu-zstd"`, `"cpu-gzip"`.
#[wasm_bindgen(js_name = decompressSingle)]
pub fn decompress_single(
    bytes: js_sys::Uint8Array,
    codec: &str,
    original_size: u64,
    crc32c: u32,
) -> Result<js_sys::Uint8Array, JsValue> {
    let kind: CodecKind = codec
        .parse()
        .map_err(|e| JsValue::from_str(&format!("s4-codec-wasm: unknown codec {codec:?}: {e}")))?;

    let payload: Vec<u8> = bytes.to_vec();
    let manifest = ChunkManifest {
        codec: kind,
        original_size,
        compressed_size: payload.len() as u64,
        crc32c,
    };

    let out = decompress_one(&payload, &manifest).map_err(|e| JsValue::from_str(&e))?;
    Ok(js_sys::Uint8Array::from(out.as_slice()))
}

/// Internal dispatch — exactly the three CPU codecs the browser supports.
///
/// Returns `String` on error (rather than `JsValue`) so the host-target
/// `cargo test` can exercise this path without `wasm-bindgen`'s
/// "function not implemented on non-wasm32 targets" panic.
fn decompress_one(payload: &[u8], manifest: &ChunkManifest) -> Result<Vec<u8>, String> {
    match manifest.codec {
        CodecKind::Passthrough => {
            // Passthrough = identity + crc32c verify + size match. No
            // helper in `s4_codec::passthrough` (the trait impl does it
            // inline against `Bytes`) so we recreate the same checks
            // here — keeps `passthrough.rs` untouched.
            if payload.len() as u64 != manifest.compressed_size {
                return Err(format!(
                    "s4-codec-wasm: passthrough size mismatch: manifest {} vs payload {}",
                    manifest.compressed_size,
                    payload.len()
                ));
            }
            let crc = crc32c::crc32c(payload);
            if crc != manifest.crc32c {
                return Err(format!(
                    "s4-codec-wasm: passthrough crc32c mismatch: expected {:#010x}, got {crc:#010x}",
                    manifest.crc32c
                ));
            }
            Ok(payload.to_vec())
        }
        CodecKind::CpuZstd => cpu_zstd::decompress_blocking(payload, manifest)
            .map_err(|e| format!("s4-codec-wasm: cpu-zstd: {e}")),
        CodecKind::CpuGzip => cpu_gzip::decompress_blocking(payload, manifest)
            .map_err(|e| format!("s4-codec-wasm: cpu-gzip: {e}")),
        unsupported => Err(format!(
            "s4-codec-wasm: codec {} is not supported in the browser",
            unsupported.as_str()
        )),
    }
}

/// Build-time visibility helper — pulled into a `wasm-bindgen` getter so
/// JS consumers can introspect what this exact .wasm bundle supports
/// without parsing semver. Returns a comma-separated string.
#[wasm_bindgen(js_name = supportedCodecs)]
pub fn supported_codecs() -> String {
    "passthrough,cpu-zstd,cpu-gzip".to_string()
}

/// Returns the magic bytes the parser recognises (S4F2 frames, S4P1
/// padding) — useful for "what version of the S4 wire format does this
/// bundle understand" diagnostics.
#[wasm_bindgen(js_name = supportedFrameMagic)]
pub fn supported_frame_magic() -> String {
    format!(
        "S4F2,{}",
        std::str::from_utf8(PADDING_MAGIC).unwrap_or("S4P1")
    )
}

// ---------------------------------------------------------------------------
// Tests run on the host target (cargo test --manifest-path
// crates/s4-codec-wasm/Cargo.toml). For wasm-target tests use
// `wasm-pack test --node` if/when wasm-pack is available locally.
// ---------------------------------------------------------------------------
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use s4_codec::multipart::{FRAME_HEADER_BYTES, FrameHeader, write_frame};

    /// Build an S4F2 byte stream from a vec of (codec, original_bytes)
    /// using `s4_codec`'s sync compress helpers + frame writer. Bypasses
    /// the wasm-bindgen JS bridge so we can exercise the dispatch logic
    /// on the host target.
    fn build_framed(parts: &[(CodecKind, &[u8])]) -> Vec<u8> {
        let mut buf = BytesMut::new();
        for (codec, data) in parts {
            let (compressed, manifest) = match codec {
                CodecKind::Passthrough => (
                    data.to_vec(),
                    ChunkManifest {
                        codec: CodecKind::Passthrough,
                        original_size: data.len() as u64,
                        compressed_size: data.len() as u64,
                        crc32c: crc32c::crc32c(data),
                    },
                ),
                CodecKind::CpuZstd => cpu_zstd::compress_blocking(data, 3).unwrap(),
                CodecKind::CpuGzip => cpu_gzip::compress_blocking(data, 6).unwrap(),
                _ => unreachable!("test-only codecs"),
            };
            let header = FrameHeader {
                codec: manifest.codec,
                original_size: manifest.original_size,
                compressed_size: manifest.compressed_size,
                crc32c: manifest.crc32c,
            };
            write_frame(&mut buf, header, &compressed);
        }
        buf.to_vec()
    }

    /// Replicates the `decompressFramed` dispatch loop without the
    /// `wasm-bindgen` Uint8Array marshalling — the actual JS bridge is
    /// trivially `Vec<u8> ↔ Uint8Array`, and exercising the logic here
    /// keeps host `cargo test` meaningful.
    fn decompress_framed_native(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for frame in FrameIter::new(Bytes::from(bytes)) {
            let (header, payload) = frame.map_err(|e| e.to_string())?;
            let manifest = ChunkManifest {
                codec: header.codec,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            let chunk = decompress_one(&payload, &manifest)?;
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    #[test]
    fn roundtrip_single_zstd_frame() {
        let plain = b"hello squished s3 hello squished s3 hello squished s3";
        let framed = build_framed(&[(CodecKind::CpuZstd, plain)]);
        let out = decompress_framed_native(framed).unwrap();
        assert_eq!(out, plain);
    }

    #[test]
    fn roundtrip_mixed_codecs() {
        let part_a = b"alpha alpha alpha";
        let part_b = vec![b'x'; 4096];
        let part_c = b"gzip me too";
        let framed = build_framed(&[
            (CodecKind::Passthrough, part_a.as_ref()),
            (CodecKind::CpuZstd, &part_b),
            (CodecKind::CpuGzip, part_c.as_ref()),
        ]);
        let out = decompress_framed_native(framed).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(part_a);
        expected.extend_from_slice(&part_b);
        expected.extend_from_slice(part_c);
        assert_eq!(out, expected);
    }

    #[test]
    fn rejects_gpu_codec_with_helpful_error() {
        // Hand-craft an S4F2 header claiming nvcomp-zstd (id 2) with a
        // zero-byte payload. The dispatch must refuse rather than try.
        let mut buf = BytesMut::new();
        let header = FrameHeader {
            codec: CodecKind::NvcompZstd,
            original_size: 0,
            compressed_size: 0,
            crc32c: 0,
        };
        write_frame(&mut buf, header, &[]);
        // FrameIter parses the frame fine; dispatch is what should reject.
        let frame = FrameIter::new(buf.freeze().clone())
            .next()
            .unwrap()
            .unwrap();
        let manifest = ChunkManifest {
            codec: frame.0.codec,
            original_size: frame.0.original_size,
            compressed_size: frame.0.compressed_size,
            crc32c: frame.0.crc32c,
        };
        let msg = decompress_one(&frame.1, &manifest).unwrap_err();
        assert!(
            msg.contains("nvcomp-zstd"),
            "error must name the offending codec; got: {msg}"
        );
    }

    #[test]
    fn truncated_frame_yields_error() {
        // Header magic only, no length / payload. FrameIter must report,
        // and decompressFramed propagates as JsValue.
        let mut buf = BytesMut::new();
        let truncated_header_bytes = FRAME_HEADER_BYTES - 4;
        buf.extend_from_slice(b"S4F2");
        buf.extend_from_slice(&vec![0u8; truncated_header_bytes - 4]);
        // Don't write payload — header says compressed_size = 0 so this
        // is technically a valid 0-byte frame; force truncation by
        // dropping a byte.
        let mut bytes = buf.to_vec();
        bytes.pop();
        let result = decompress_framed_native(bytes);
        assert!(result.is_err(), "expected truncated input to error");
    }

    #[test]
    fn supported_codecs_string_is_stable() {
        assert_eq!(supported_codecs(), "passthrough,cpu-zstd,cpu-gzip");
    }

    #[test]
    fn supported_frame_magic_includes_padding() {
        let s = supported_frame_magic();
        assert!(s.contains("S4F2"));
        assert!(s.contains("S4P1"));
    }

    /// `examples/web-demo/index.html` ships an inline base64-encoded
    /// S4F2 sample so the demo works offline. This test decodes the
    /// same bytes via `decompressFramed`'s native dispatch path so any
    /// drift between the demo and the codec lights up CI before users
    /// see a broken page.
    #[test]
    fn demo_sample_decodes_to_expected_text() {
        // Same base64 string as `SAMPLE_BASE64` in
        // `examples/web-demo/index.html`. Keep these two in sync (or
        // teach the build to inject the test sample into the HTML).
        const SAMPLE_BASE64: &str = concat!(
            "UzRGMgEAAAC7AAAAAAAAAKsAAAAAAAAALgYiryi1L/0AWBUFAIIKJSqQp+n///9/",
            "rN/J6U9Pn2zZ30Jys/tbLinuqm7Hzlp4VtqC0Q3Ee+HbpQSIsgUa/aDZqIXOf9P/",
            "WqcfdSd9uMG8KBrvXgogynTGML7tkZqM2PQ3nbjvXlKSQszNTfA2yxPh0wvfAJII",
            "cBwEbvDA8mupfCwrvXw9dncK3TlZOHZ/CyuDRVhhgaHBh6VhHEsqI2uMBwgDAGIQ",
            "p6Y0FS98Bg=="
        );

        // Tiny standalone base64 decoder (we don't want to add the
        // `base64` crate dep just for one test).
        fn b64_decode(s: &str) -> Vec<u8> {
            let table: [i8; 256] = {
                let mut t = [-1i8; 256];
                for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
                    .iter()
                    .enumerate()
                {
                    t[*c as usize] = i as i8;
                }
                t
            };
            let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
            for chunk in bytes.chunks(4) {
                let mut buf = [0u8; 4];
                let mut pad = 0;
                for (i, &b) in chunk.iter().enumerate() {
                    if b == b'=' {
                        pad += 1;
                        buf[i] = 0;
                    } else {
                        buf[i] = table[b as usize] as u8;
                    }
                }
                let n = (buf[0] as u32) << 18
                    | (buf[1] as u32) << 12
                    | (buf[2] as u32) << 6
                    | (buf[3] as u32);
                out.push((n >> 16) as u8);
                if pad < 2 {
                    out.push((n >> 8) as u8);
                }
                if pad < 1 {
                    out.push(n as u8);
                }
            }
            out
        }

        let bytes = b64_decode(SAMPLE_BASE64);
        let decoded = decompress_framed_native(bytes).expect("demo sample must decode");
        let text = std::str::from_utf8(&decoded).expect("demo sample must be UTF-8");
        assert!(
            text.starts_with("Hello from S4F2!"),
            "demo sample drifted; got: {text:?}"
        );
    }
}
