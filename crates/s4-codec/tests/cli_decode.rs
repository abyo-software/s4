//! Integration tests for the `s4-codec` escape-hatch CLI
//! (`src/bin/s4-codec.rs`). Spawns the built binary via
//! `env!("CARGO_BIN_EXE_s4-codec")` — the standard cargo integration-test
//! mechanism, no extra dev-deps.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::BytesMut;
use s4_codec::index::{FrameIndex, FrameIndexEntry, encode_index};
use s4_codec::multipart::{FRAME_HEADER_BYTES, FrameHeader, pad_to_minimum, write_frame};
use s4_codec::{CodecKind, cpu_zstd, cpu_zstd_dict};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_s4-codec"))
}

/// Per-test scratch directory under the OS tempdir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "s4-codec-cli-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Build a gateway-shaped framed body: one `cpu-zstd` S4F2 frame per
/// chunk, with an S4P1 padding frame in the middle (mirrors what the
/// multipart path writes for sub-5 MiB parts). Returns `(body, original)`.
fn framed_zstd_object(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
    let mut buf = BytesMut::new();
    let mut original = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let (compressed, manifest) =
            cpu_zstd::compress_blocking(chunk, cpu_zstd::CpuZstd::DEFAULT_LEVEL).expect("compress");
        write_frame(
            &mut buf,
            FrameHeader {
                codec: CodecKind::CpuZstd,
                original_size: manifest.original_size,
                compressed_size: compressed.len() as u64,
                crc32c: manifest.crc32c,
            },
            &compressed,
        );
        original.extend_from_slice(chunk);
        if i == 0 && chunks.len() > 1 {
            // Insert padding after the first frame so the CLI's
            // padding-skip path is exercised on every multi-chunk test.
            let target = buf.len() + 256;
            pad_to_minimum(&mut buf, target);
        }
    }
    (buf.to_vec(), original)
}

#[test]
fn decode_roundtrips_to_output_file() {
    let dir = TempDir::new("roundtrip");
    let chunk_a = b"hello S4 escape hatch, this line compresses fine ".repeat(64);
    let chunk_b = b"second frame with different bytes 0123456789 ".repeat(32);
    let (body, original) = framed_zstd_object(&[&chunk_a, &chunk_b]);
    let input = dir.file("obj.s4f2", &body);
    let output = dir.path("obj.decoded");

    let out = bin()
        .args(["decode"])
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .output()
        .expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(
        std::fs::read(&output).expect("read decoded"),
        original,
        "decoded bytes must equal the pre-compression original"
    );
    assert!(
        stderr_of(&out).contains("decoded 2 frame(s)"),
        "summary should count data frames (padding skipped): {}",
        stderr_of(&out)
    );
}

#[test]
fn decode_writes_to_piped_stdout() {
    let dir = TempDir::new("stdout");
    let chunk = b"stdout roundtrip payload ".repeat(100);
    let (body, original) = framed_zstd_object(&[&chunk]);
    let input = dir.file("obj.s4f2", &body);

    // `.output()` pipes stdout, so `IsTerminal` is false and the CLI
    // must stream the raw decoded bytes.
    let out = bin().args(["decode"]).arg(&input).output().expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, original);
}

#[test]
fn inspect_prints_codec_and_sizes() {
    let dir = TempDir::new("inspect");
    let chunk = b"inspect me ".repeat(50);
    let (body, original) = framed_zstd_object(&[&chunk]);
    let input = dir.file("obj.s4f2", &body);

    let out = bin().args(["inspect"]).arg(&input).output().expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(stdout.contains("cpu-zstd"), "codec name missing: {stdout}");
    assert!(
        stdout.contains(&original.len().to_string()),
        "original size missing: {stdout}"
    );
    assert!(stdout.contains("CRC32C"), "crc column missing: {stdout}");
}

#[test]
fn inspect_json_is_machine_readable() {
    let dir = TempDir::new("inspect-json");
    let chunk = b"json output ".repeat(40);
    let (body, original) = framed_zstd_object(&[&chunk]);
    let input = dir.file("obj.s4f2", &body);

    let out = bin()
        .args(["inspect", "--json"])
        .arg(&input)
        .output()
        .expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("\"codec\":\"cpu-zstd\""),
        "per-frame codec missing: {stdout}"
    );
    assert!(
        stdout.contains("\"total_frames\": 1"),
        "totals missing: {stdout}"
    );
    assert!(
        stdout.contains(&format!("\"total_original_size\": {}", original.len())),
        "original total missing: {stdout}"
    );
    assert!(
        stdout.contains(&format!("\"file_size\": {}", body.len())),
        "file size missing: {stdout}"
    );
}

#[test]
fn corrupted_crc_fails_loudly() {
    let dir = TempDir::new("badcrc");
    let chunk = b"corrupt my checksum ".repeat(30);
    let (mut body, _original) = framed_zstd_object(&[&chunk]);
    // Frame header layout: magic(4) + codec_id(4) + orig(8) + comp(8) + crc(4).
    // Flip a bit in the CRC32C field of frame 0.
    let crc_off = FRAME_HEADER_BYTES - 4;
    body[crc_off] ^= 0xFF;
    let input = dir.file("obj.s4f2", &body);

    let out = bin().args(["decode"]).arg(&input).output().expect("spawn");
    assert!(!out.status.success(), "corrupted CRC must not exit 0");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("crc32c mismatch"),
        "error must name the CRC failure: {stderr}"
    );
}

#[test]
fn nvcomp_frame_fails_naming_the_codec() {
    let dir = TempDir::new("nvcomp");
    // Hand-craft a frame that claims a GPU codec; payload bytes are
    // irrelevant because the CLI must refuse before decoding.
    let payload = vec![0xAAu8; 64];
    let mut buf = BytesMut::new();
    write_frame(
        &mut buf,
        FrameHeader {
            codec: CodecKind::NvcompZstd,
            original_size: 1024,
            compressed_size: payload.len() as u64,
            crc32c: 0,
        },
        &payload,
    );
    let input = dir.file("obj.s4f2", &buf);

    let out = bin().args(["decode"]).arg(&input).output().expect("spawn");
    assert!(!out.status.success(), "GPU frame must not exit 0");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("nvcomp-zstd"),
        "error must name the codec: {stderr}"
    );
    assert!(
        stderr.contains("`s4` gateway"),
        "error must point at the gateway binary: {stderr}"
    );

    // `inspect` never decodes, so the same object must still be
    // introspectable (that's how an operator finds out it's GPU-coded).
    let out = bin().args(["inspect"]).arg(&input).output().expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).contains("nvcomp-zstd"));
}

#[test]
fn sse_encrypted_object_fails_loudly() {
    let dir = TempDir::new("sse");
    // S4E2 body: magic + opaque ciphertext. The CLI must refuse on the
    // magic alone (it has no decryption support by design).
    let mut body = b"S4E2".to_vec();
    body.extend_from_slice(&[0u8; 64]);
    let input = dir.file("obj.enc", &body);

    for cmd in ["decode", "inspect"] {
        let out = bin().args([cmd]).arg(&input).output().expect("spawn");
        assert!(!out.status.success(), "{cmd}: SSE body must not exit 0");
        let stderr = stderr_of(&out);
        assert!(
            stderr.contains("SSE-encrypted") && stderr.contains("S4E2"),
            "{cmd}: error must explain the SSE refusal: {stderr}"
        );
        assert!(
            stderr.contains("`s4` gateway"),
            "{cmd}: error must point at the gateway binary: {stderr}"
        );
    }
}

#[test]
fn dict_frames_decode_with_dict_and_fail_without() {
    let dir = TempDir::new("dict");
    // Train a real dictionary the same way the gateway's `train-dict`
    // path does (zstd::dict::from_samples under the hood).
    let corpus: Vec<Vec<u8>> = (0..200u32)
        .map(|i| {
            format!(
                "{{\"level\":\"info\",\"service\":\"checkout-api\",\"order_id\":\"ord_{i:08}\",\"amount_cents\":{}}}",
                100 + i * 13
            )
            .into_bytes()
        })
        .collect();
    let dict = cpu_zstd_dict::train_from_samples(&corpus, 16 * 1024).expect("train dict");
    let dict_file = dir.file("shared.dict", &dict);

    let original =
        b"{\"level\":\"info\",\"service\":\"checkout-api\",\"order_id\":\"ord_99999999\",\"amount_cents\":4200}";
    let (compressed, manifest) = cpu_zstd_dict::compress_blocking(
        original,
        &dict,
        cpu_zstd_dict::CpuZstdDict::DEFAULT_LEVEL,
    )
    .expect("compress");
    let mut buf = BytesMut::new();
    write_frame(
        &mut buf,
        FrameHeader {
            codec: CodecKind::CpuZstdDict,
            original_size: manifest.original_size,
            compressed_size: compressed.len() as u64,
            crc32c: manifest.crc32c,
        },
        &compressed,
    );
    let input = dir.file("obj.s4f2", &buf);

    // With --dict: roundtrip.
    let out = bin()
        .args(["decode"])
        .arg(&input)
        .arg("--dict")
        .arg(&dict_file)
        .output()
        .expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, original);

    // Without --dict: hard error telling the operator what to pass.
    let out = bin().args(["decode"]).arg(&input).output().expect("spawn");
    assert!(!out.status.success(), "dict frame without --dict must fail");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("cpu-zstd-dict") && stderr.contains("--dict"),
        "error must name the codec and the missing flag: {stderr}"
    );
}

#[test]
fn index_decodes_a_real_sidecar() {
    let dir = TempDir::new("index");
    let idx = FrameIndex {
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
                compressed_offset: 60,
                compressed_size: 40,
            },
        ],
        source_etag: Some("\"abc123\"".to_owned()),
        source_compressed_size: Some(987),
        sse_v3: None,
    };
    let sidecar = encode_index(&idx);
    let input = dir.file("obj.s4index", &sidecar);

    let out = bin().args(["index"]).arg(&input).output().expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(stdout.contains("abc123"), "etag missing: {stdout}");
    assert!(
        stdout.contains("entries:                 2"),
        "entry count missing: {stdout}"
    );
    assert!(
        stdout.contains("180"),
        "total original (100+80) missing: {stdout}"
    );

    let out = bin()
        .args(["index", "--json"])
        .arg(&input)
        .output()
        .expect("spawn");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("\"total_original_size\": 180"),
        "json totals missing: {stdout}"
    );
    // The stored etag contains literal quotes — they must be escaped.
    assert!(
        stdout.contains("\"source_etag\": \"\\\"abc123\\\"\""),
        "json etag escaping wrong: {stdout}"
    );
    assert!(
        stdout.contains("\"original_offset\":100"),
        "json entries missing: {stdout}"
    );

    // Feeding a non-sidecar file must fail with a typed decode error.
    let bogus = dir.file("not-an-index", b"definitely not S4IX");
    let out = bin().args(["index"]).arg(&bogus).output().expect("spawn");
    assert!(!out.status.success(), "bogus sidecar must not exit 0");
    assert!(
        stderr_of(&out).contains("S4IX"),
        "error should mention the expected format: {}",
        stderr_of(&out)
    );
}
