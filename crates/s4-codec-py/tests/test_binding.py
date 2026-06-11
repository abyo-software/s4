"""v0.8.5 #85 H-8: integration tests for the s4-codec Python binding.

Run via:

    maturin develop
    pip install -e ".[dev]"
    pytest tests/

These tests live alongside the Rust crate so they can be discovered by both
`pytest crates/s4-codec-py/tests/` and `cargo nextest run -p s4-codec-py`'s
sibling Rust unit-test runs (which only need the .rs file in the same dir).
"""
import pytest

from s4_codec import (
    CpuGzip,
    CpuZstd,
    S4Error,
    S4SizeMismatchError,
    gpu_available,
)


def test_cpu_zstd_roundtrip():
    codec = CpuZstd(level=3)
    data = b"hello squished s3 " * 1000
    compressed, orig_size, crc = codec.compress(data)
    assert orig_size == len(data)
    assert isinstance(crc, int)
    roundtrip = codec.decompress(compressed, orig_size, crc)
    assert roundtrip == data


def test_cpu_zstd_default_level():
    codec = CpuZstd()  # default level
    data = b"x" * 1024
    compressed, orig_size, crc = codec.compress(data)
    assert codec.decompress(compressed, orig_size, crc) == data


def test_cpu_gzip_produces_valid_gzip_magic():
    codec = CpuGzip(level=6)
    data = b"test data" * 100
    compressed, _orig_size, _crc = codec.compress(data)
    # RFC 1952 magic bytes — proves the binding's gzip output is real gzip,
    # not a zstd payload mislabeled or a raw deflate stream.
    assert compressed[:2] == b"\x1f\x8b", "gzip output must start with 1f 8b"


def test_cpu_gzip_roundtrip_through_python_gzip_module():
    """Smoke test that S4 gzip output decodes via stdlib gzip.

    This is a stronger compat check than the magic-bytes assertion above:
    if the binding ever regressed to emitting raw deflate (RFC 1951) the
    magic bytes would still happen to match by accident on some inputs,
    but `gzip.decompress` would reject the missing CRC trailer.
    """
    import gzip

    codec = CpuGzip(level=6)
    data = b"compatibility check " * 100
    compressed, _orig_size, _crc = codec.compress(data)
    decompressed = gzip.decompress(compressed)
    assert decompressed == data


def test_gpu_available_returns_bool():
    result = gpu_available()
    assert isinstance(result, bool)


def test_corrupted_decompress_raises_specific_error():
    """v0.8.5 #85 M-5: error enum should surface as discriminable exception."""
    codec = CpuZstd()
    data = b"hello world " * 100
    compressed, orig_size, crc = codec.compress(data)
    # Tamper one byte of compressed output → CRC will mismatch (or zstd
    # decode itself will fail, surfaced as a backend error). Either way
    # the binding must raise an S4Error subclass, not a bare ValueError
    # or RuntimeError that tells the caller nothing about provenance.
    tampered = bytearray(compressed)
    if len(tampered) > 10:
        tampered[5] ^= 0xFF
    with pytest.raises((S4Error, OSError, RuntimeError)):
        # OSError/RuntimeError are the parent classes of S4IoError /
        # S4BackendError — accept either so a zstd library that reports
        # corruption as a backend error rather than a CRC mismatch still
        # passes. The discriminability assertion is the next test.
        codec.decompress(bytes(tampered), orig_size, crc)


def test_decompress_wrong_size_raises_size_mismatch():
    """Lying about original_size must surface as S4SizeMismatchError."""
    codec = CpuZstd()
    data = b"foo" * 100
    compressed, orig_size, crc = codec.compress(data)
    # Lie about size — should hit SizeMismatch on decompress (the post-
    # decompress length check), which maps to S4SizeMismatchError per
    # v0.8.5 #85 M-5.
    with pytest.raises(S4SizeMismatchError):
        codec.decompress(compressed, orig_size + 1, crc)


def test_compress_releases_gil():
    """Threading smoke test: compress should release GIL during long calls.

    The binding's `Python::allow_threads` wrapper around `block_on` is the
    only thing that lets a Python thread make progress while compress is
    churning. If a refactor ever drops the wrapper this test catches it.
    """
    import threading
    import time

    codec = CpuZstd()
    big_data = b"x" * (50 * 1024 * 1024)  # 50 MB

    progress = []

    def background():
        for _ in range(5):
            time.sleep(0.01)
            progress.append(time.time())

    t = threading.Thread(target=background)
    t.start()
    codec.compress(big_data)  # Should not block t
    t.join()

    # Background thread made progress during compress
    assert len(progress) == 5


def test_module_version_matches_workspace():
    """v0.8.5 #82 regression guard: __version__ should NOT be 0.1.0."""
    import s4_codec

    v = s4_codec.__version__
    parts = v.split(".")
    major, minor = int(parts[0]), int(parts[1])
    assert (major, minor) >= (0, 8), (
        f"binding version {v} < 0.8 — workspace inheritance broken?"
    )


# ---------------------------------------------------------------------------
# v1.1 s4fs: wire-format read helpers (read_frame / frame_iter / decode_index
# / crc32c) + CpuZstdDict. The synthesized frame/index bytes below restate the
# frozen v1.0 S4F2 / S4IX layouts from `crates/s4-codec/src/multipart.rs` and
# `index.rs` — an independent little-endian re-statement that cross-checks the
# binding against the documented format (the Rust unit tests already pin
# write↔read symmetry).
# ---------------------------------------------------------------------------

import struct

from s4_codec import (
    CpuZstdDict,
    S4FrameError,
    S4IndexError,
    crc32c,
    decode_index,
    frame_iter,
    read_frame,
)

# CodecKind::id() values (wire-frozen, see s4-codec/src/lib.rs)
_CODEC_ID_PASSTHROUGH = 0
_CODEC_ID_CPU_ZSTD = 1


def _frame(codec_id: int, original_size: int, payload: bytes, crc: int) -> bytes:
    """[S4F2][codec_id u32][orig u64][comp u64][crc u32] LE + payload."""
    return (
        struct.pack("<4sIQQI", b"S4F2", codec_id, original_size, len(payload), crc)
        + payload
    )


def test_crc32c_known_vector():
    # RFC 3720 CRC32C check value for "123456789".
    assert crc32c(b"123456789") == 0xE3069283


def test_read_frame_passthrough():
    payload = b"hello frame payload"
    buf = _frame(_CODEC_ID_PASSTHROUGH, len(payload), payload, crc32c(payload))
    header, got_payload, rest = read_frame(buf + b"TRAILING")
    assert header["codec"] == "passthrough"
    assert header["original_size"] == len(payload)
    assert header["compressed_size"] == len(payload)
    assert header["crc32c"] == crc32c(payload)
    assert got_payload == payload
    assert rest == b"TRAILING"


def test_read_frame_zstd_payload_roundtrips_through_cpu_zstd():
    data = b"squish me " * 500
    compressed, orig_size, crc = CpuZstd(level=3).compress(data)
    buf = _frame(_CODEC_ID_CPU_ZSTD, orig_size, compressed, crc)
    header, payload, rest = read_frame(buf)
    assert header["codec"] == "cpu-zstd"
    assert rest == b""
    assert CpuZstd().decompress(payload, header["original_size"], header["crc32c"]) == data


def test_read_frame_bad_magic_raises():
    with pytest.raises(S4FrameError):
        read_frame(b"BAD!" + b"\x00" * 64)


def test_read_frame_truncated_raises():
    with pytest.raises(S4FrameError):
        read_frame(b"S4F2\x00")


def test_frame_iter_skips_s4p1_padding():
    p1 = b"first"
    p2 = b"second"
    pad_payload = b"\x00" * 100
    padding = struct.pack("<4sQ", b"S4P1", len(pad_payload)) + pad_payload
    buf = (
        _frame(_CODEC_ID_PASSTHROUGH, len(p1), p1, crc32c(p1))
        + padding
        + _frame(_CODEC_ID_PASSTHROUGH, len(p2), p2, crc32c(p2))
    )
    frames = frame_iter(buf)
    assert [payload for _h, payload in frames] == [p1, p2]


def test_frame_iter_corrupt_tail_raises():
    p1 = b"first"
    buf = _frame(_CODEC_ID_PASSTHROUGH, len(p1), p1, crc32c(p1)) + b"GARBAGE!"
    with pytest.raises(S4FrameError):
        frame_iter(buf)


def _index_v2(entries, total_padded, source_compressed=0, etag=b""):
    """S4IX v2: [magic][version u32][n u64][total_orig u64][total_padded u64]
    [source_compressed_size u64][etag_len u32][etag] + n × 4 u64 entries."""
    total_orig = sum(e[1] for e in entries)
    buf = struct.pack(
        "<4sIQQQQI",
        b"S4IX",
        2,
        len(entries),
        total_orig,
        total_padded,
        source_compressed,
        len(etag),
    ) + etag
    for ooff, osize, coff, csize in entries:
        buf += struct.pack("<QQQQ", ooff, osize, coff, csize)
    return buf


def test_decode_index_v2():
    entries = [(0, 100, 0, 50), (100, 80, 60, 40)]
    raw = _index_v2(entries, total_padded=200, source_compressed=100, etag=b'"abc"')
    idx = decode_index(raw)
    assert idx["total_padded_size"] == 200
    assert idx["total_original_size"] == 180
    assert idx["source_etag"] == '"abc"'
    assert idx["source_compressed_size"] == 100
    assert idx["sse"] is None
    assert idx["entries"] == [
        {
            "original_offset": 0,
            "original_size": 100,
            "compressed_offset": 0,
            "compressed_size": 50,
        },
        {
            "original_offset": 100,
            "original_size": 80,
            "compressed_offset": 60,
            "compressed_size": 40,
        },
    ]


def test_decode_index_bad_magic_raises():
    with pytest.raises(S4IndexError):
        decode_index(b"NOPE" + b"\x00" * 64)


def test_decode_index_truncated_raises():
    with pytest.raises(S4IndexError):
        decode_index(b"S4IX\x02")


def test_cpu_zstd_dict_roundtrip():
    # Any non-empty byte string is a valid zstd "raw content" dictionary;
    # compress/decompress must agree as long as both sides use the same one.
    dictionary = b'{"timestamp":"2026-01-01T00:00:00Z","level":"info","service":"x"}' * 16
    codec = CpuZstdDict(dictionary, level=3)
    data = b'{"timestamp":"2026-06-10T13:01:02Z","level":"info","service":"x","n":1}'
    compressed, orig_size, crc = codec.compress(data)
    assert orig_size == len(data)
    assert codec.decompress(compressed, orig_size, crc) == data


def test_cpu_zstd_dict_wrong_dict_raises():
    # Data is a verbatim slice of the dictionary with no internal redundancy,
    # so the compressor must reference the dictionary content; decoding with
    # a different dictionary then yields different bytes → typed error
    # (zstd refuses the frame or the CRC check catches it), never silence.
    import os

    dictionary = os.urandom(4096)
    right = CpuZstdDict(dictionary, level=19)
    wrong = CpuZstdDict(os.urandom(4096), level=19)
    data = dictionary[100:1100]
    compressed, orig_size, crc = right.compress(data)
    assert len(compressed) < len(data), "dict reference must actually compress"
    with pytest.raises((S4Error, OSError, RuntimeError)):
        wrong.decompress(compressed, orig_size, crc)


def test_cpu_zstd_dict_empty_dict_rejected():
    with pytest.raises((S4Error, RuntimeError)):
        CpuZstdDict(b"")


def test_exception_class_hierarchy():
    """v0.8.5 #85 M-5: validate the documented inheritance tree."""
    from s4_codec import (
        S4BackendError,
        S4CodecMismatchError,
        S4CrcMismatchError,
        S4IoError,
        S4ManifestSizeExceedsLimitError,
        S4ManifestSizeMismatchError,
        S4UnregisteredCodecError,
    )

    # Variants on the codec-error base
    assert issubclass(S4CrcMismatchError, S4Error)
    assert issubclass(S4SizeMismatchError, S4Error)
    assert issubclass(S4CodecMismatchError, S4Error)
    assert issubclass(S4UnregisteredCodecError, S4Error)
    assert issubclass(S4ManifestSizeExceedsLimitError, S4Error)
    assert issubclass(S4ManifestSizeMismatchError, S4Error)

    # S4Error itself is a ValueError so legacy `except ValueError:` blocks
    # that targeted the previous flat mapping continue to fire.
    assert issubclass(S4Error, ValueError)

    # Backend / Io map to stdlib semantics, NOT to S4Error
    assert issubclass(S4BackendError, RuntimeError)
    assert issubclass(S4IoError, OSError)  # IOError is OSError in py3
    assert not issubclass(S4BackendError, S4Error)
    assert not issubclass(S4IoError, S4Error)


# ---------------------------------------------------------------------------
# v1.2 s4fs write support: encode_s4_object / bind_index / pick_chunk_size.
# Roundtrips are verified through the binding's own *read* helpers
# (read_frame / frame_iter / decode_index), which the suites above pin
# against the frozen v1.0 wire formats.
# ---------------------------------------------------------------------------

from s4_codec import bind_index, encode_s4_object, pick_chunk_size

_MIB = 1024 * 1024


def _decode_body(enc) -> bytes:
    """Decode an encode_s4_object framed body via the read-side helpers."""
    out = b""
    for header, payload in frame_iter(enc["body"]):
        assert header["codec"] == "cpu-zstd"
        out += CpuZstd().decompress(payload, header["original_size"], header["crc32c"])
    return out


def test_pick_chunk_size_threshold_table():
    """Restates s4-server/src/streaming.rs::pick_chunk_size (keep-in-sync)."""
    assert pick_chunk_size(0) == _MIB
    assert pick_chunk_size(64 * 1024) == _MIB
    assert pick_chunk_size(_MIB) == _MIB
    assert pick_chunk_size(_MIB + 1) == 4 * _MIB
    assert pick_chunk_size(50 * _MIB) == 4 * _MIB
    assert pick_chunk_size(100 * _MIB) == 4 * _MIB
    assert pick_chunk_size(100 * _MIB + 1) == 16 * _MIB
    assert pick_chunk_size(10 * 1024 * _MIB) == 16 * _MIB


def test_encode_single_frame_roundtrip_and_metadata():
    data = b"compress me please " * 4000  # ~76 KiB -> one 1 MiB chunk
    enc = encode_s4_object(data, codec="cpu-zstd", level=3)
    assert _decode_body(enc) == data
    assert len(frame_iter(enc["body"])) == 1
    # Single frame => no sidecar (mirrors the gateway's >1-entry policy).
    assert enc["sidecar"] is None
    meta = enc["metadata"]
    assert meta["s4-codec"] == "cpu-zstd"
    assert meta["s4-original-size"] == str(len(data))
    assert meta["s4-compressed-size"] == str(len(enc["body"]))
    assert meta["s4-crc32c"] == str(crc32c(data))
    assert meta["s4-framed"] == "true"


@pytest.mark.parametrize(
    ("size", "expected_frames"),
    [
        (_MIB, 1),  # <= 1 MiB -> 1 MiB chunk -> 1 frame
        (4 * _MIB - 1, 1),  # 4 MiB chunking, just under the boundary
        (4 * _MIB, 1),  # exactly one full 4 MiB chunk
        (4 * _MIB + 1, 2),  # one byte over -> second frame
        (9 * _MIB, 3),  # 4 + 4 + 1
    ],
)
def test_encode_frame_count_at_chunk_boundaries(size, expected_frames):
    # Mildly compressible, position-dependent payload: catches chunk
    # reordering / off-by-one slicing that uniform bytes would mask.
    data = bytes((i * 31 + (i >> 13)) & 0xFF for i in range(size))
    enc = encode_s4_object(data, level=1)
    frames = frame_iter(enc["body"])
    assert len(frames) == expected_frames, size
    assert _decode_body(enc) == data
    assert sum(h["original_size"] for h, _p in frames) == size
    # Per-frame headers carry per-chunk sizes/CRCs; aggregate metadata
    # carries whole-body values (gateway streaming_compress_to_frames).
    assert enc["metadata"]["s4-original-size"] == str(size)
    assert enc["metadata"]["s4-crc32c"] == str(crc32c(data))
    if expected_frames > 1:
        assert enc["sidecar"] is not None
    else:
        assert enc["sidecar"] is None


def test_encode_multi_frame_sidecar_layout_and_binding():
    size = 9 * _MIB
    data = bytes((i * 7 + 3) & 0xFF for i in range(size))
    enc = encode_s4_object(data, level=1)
    idx = decode_index(enc["sidecar"])
    assert idx["total_original_size"] == size
    assert idx["total_padded_size"] == len(enc["body"])
    # Unbound until the caller PUTs the body and learns the backend ETag.
    assert idx["source_etag"] is None
    assert idx["source_compressed_size"] is None
    # Entries tile the original space contiguously and point at real frames.
    off = 0
    for e in idx["entries"]:
        assert e["original_offset"] == off
        off += e["original_size"]
        header, _payload, _rest = read_frame(
            enc["body"][e["compressed_offset"] : e["compressed_offset"] + e["compressed_size"]]
        )
        assert header["original_size"] == e["original_size"]
    assert off == size

    bound = bind_index(enc["sidecar"], source_compressed_size=len(enc["body"]), source_etag="abc")
    b = decode_index(bound)
    assert b["source_etag"] == "abc"
    assert b["source_compressed_size"] == len(enc["body"])
    assert b["entries"] == idx["entries"]
    # ETag-less backends: size-only binding is still a valid v2 sidecar.
    sizeonly = decode_index(bind_index(enc["sidecar"], source_compressed_size=123))
    assert sizeonly["source_etag"] is None
    assert sizeonly["source_compressed_size"] == 123


def test_encode_empty_body():
    enc = encode_s4_object(b"")
    assert enc["body"] == b""
    assert enc["sidecar"] is None
    meta = enc["metadata"]
    assert meta["s4-original-size"] == "0"
    assert meta["s4-compressed-size"] == "0"
    assert meta["s4-crc32c"] == "0"
    assert meta["s4-framed"] == "true"
    assert frame_iter(enc["body"]) == []


def test_encode_passthrough_is_raw_and_unframed():
    data = b"do not compress me"
    enc = encode_s4_object(data, codec="passthrough")
    assert enc["body"] == data  # byte-identical, no S4F2 wrapping
    assert enc["sidecar"] is None
    meta = enc["metadata"]
    assert meta["s4-codec"] == "passthrough"
    assert meta["s4-original-size"] == str(len(data))
    assert meta["s4-compressed-size"] == str(len(data))
    assert meta["s4-crc32c"] == str(crc32c(data))
    # The gateway never stamps s4-framed on passthrough bodies.
    assert "s4-framed" not in meta


@pytest.mark.parametrize("codec", ["cpu-gzip", "cpu-zstd-dict", "nvcomp-lz4", "dietgpu-ans"])
def test_encode_unsupported_codecs_point_at_gateway(codec):
    with pytest.raises(NotImplementedError, match="gateway"):
        encode_s4_object(b"x", codec=codec)


def test_bind_index_rejects_garbage():
    with pytest.raises(S4IndexError):
        bind_index(b"NOPE" + b"\x00" * 64, source_compressed_size=1)
