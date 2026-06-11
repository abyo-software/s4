"""Unit tests for S4FileSystem against gateway-captured fixtures.

Every framed fixture body under ``fixtures/`` was written by the real S4
gateway (or ``s4 train-dict``) and captured straight off the MinIO backend
— see ``fixtures/generate_fixtures.py`` for the capture procedure. The
sanctioned exceptions, all asserting *refusal* paths (never decode paths):

- the synthesized nvcomp frame *header* in the GPU-refusal test (building
  a GPU frame requires a GPU; faking only the codec id is acceptable);
- the S4E1..S4E6 SSE envelope magics (building real SSE bodies requires a
  keyring-configured gateway; only the 4 magic bytes matter for refusal);
- v1 / v3 ``.s4index`` re-encodings of the captured v2 sidecar (the
  gateway no longer writes v1, and v3-with-SSE only appears alongside
  encrypted bodies; the entry table is still the captured one).
"""

from __future__ import annotations

import struct

import pytest

import s4_codec
import datagen  # tests/ dir is on sys.path (pytest rootdir insertion)
from conftest import BUCKET, load_fixture, stub_with
from s4fs import S4FileSystem


# -- full-object reads ------------------------------------------------------


def test_cat_zstd_text_roundtrip(s4fs_factory):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    assert fs.cat_file(f"{BUCKET}/text.txt") == load_fixture("text_zstd")["orig"]


def test_cat_gzip_text_roundtrip(s4fs_factory):
    fs, _ = s4fs_factory(("text-gzip.txt", "text_gzip"))
    assert fs.cat_file(f"{BUCKET}/text-gzip.txt") == load_fixture("text_gzip")["orig"]


def test_cat_multi_frame_roundtrip(s4fs_factory):
    fs, _ = s4fs_factory(("multi.bin", "multi_zstd"))
    assert fs.cat_file(f"{BUCKET}/multi.bin") == datagen.multi_frame_body()


def test_cat_dict_compressed_roundtrip(s4fs_factory):
    fs, _ = s4fs_factory(("events/new.json", "dict_event"))
    assert fs.cat_file(f"{BUCKET}/events/new.json") == load_fixture("dict_event")["orig"]


def test_cat_raw_object_passthrough(s4fs_factory):
    """Objects that never went through the gateway come back byte-for-byte."""
    fs, _ = s4fs_factory(("raw.bin", "raw"))
    assert fs.cat_file(f"{BUCKET}/raw.bin") == load_fixture("raw")["orig"]


def test_open_read(s4fs_factory):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    with fs.open(f"{BUCKET}/text.txt", "rb") as f:
        data = f.read()
    assert data == load_fixture("text_zstd")["orig"]


def test_open_seek_read(s4fs_factory):
    fs, _ = s4fs_factory(("multi.bin", "multi_zstd"))
    orig = datagen.multi_frame_body()
    with fs.open(f"{BUCKET}/multi.bin", "rb") as f:
        f.seek(len(orig) - 1000)
        assert f.read(1000) == orig[-1000:]


# -- ls / info: original sizes, hidden internals ------------------------------


def test_info_reports_original_size(s4fs_factory):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    orig = load_fixture("text_zstd")["orig"]
    info = fs.info(f"{BUCKET}/text.txt")
    assert info["size"] == len(orig)
    assert info["s4_size_exact"] is True


def test_info_size_from_metadata_when_no_sidecar(s4fs_factory):
    """Drop the sidecar: size must come from `s4-original-size` metadata."""
    fs, stub = s4fs_factory(("text.txt", "text_zstd"))
    stub.files.pop(f"{BUCKET}/text.txt.s4index", None)
    orig = load_fixture("text_zstd")["orig"]
    info = fs.info(f"{BUCKET}/text.txt")
    assert info["size"] == len(orig)
    assert info["s4_size_exact"] is True


def test_info_inexact_size_flagged(s4fs_factory):
    """No sidecar AND no metadata: compressed size + s4_size_exact=False."""
    fs, stub = s4fs_factory(("text.txt", "text_zstd"))
    stub.files.pop(f"{BUCKET}/text.txt.s4index", None)
    stub.meta.clear()
    info = fs.info(f"{BUCKET}/text.txt")
    assert info["size"] == len(load_fixture("text_zstd")["body"])
    assert info["s4_size_exact"] is False


def test_info_raw_object_exact(s4fs_factory):
    fs, _ = s4fs_factory(("raw.bin", "raw"))
    info = fs.info(f"{BUCKET}/raw.bin")
    assert info["size"] == len(load_fixture("raw")["orig"])
    assert info["s4_size_exact"] is True


def test_ls_hides_sidecars_and_dicts_and_reports_original_sizes(s4fs_factory):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"), ("raw.bin", "raw"))
    names = fs.ls(BUCKET)
    assert f"{BUCKET}/text.txt" in names
    assert f"{BUCKET}/raw.bin" in names
    assert not any(n.endswith(".s4index") for n in names)
    assert not any(".s4dict" in n for n in names)
    detail = {e["name"]: e for e in fs.ls(BUCKET, detail=True)}
    assert detail[f"{BUCKET}/text.txt"]["size"] == len(load_fixture("text_zstd")["orig"])


def test_hidden_paths_unreachable(s4fs_factory):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    assert not fs.exists(f"{BUCKET}/text.txt.s4index")
    with pytest.raises(FileNotFoundError):
        fs.info(f"{BUCKET}/text.txt.s4index")


# -- range reads ---------------------------------------------------------------


def test_range_read_matches_full_read(s4fs_factory):
    fs, _ = s4fs_factory(("multi.bin", "multi_zstd"))
    orig = datagen.multi_frame_body()
    path = f"{BUCKET}/multi.bin"
    for start, end in [
        (0, 100),
        (1000, 70000),
        (len(orig) - 500, len(orig)),
        (4 * 1024 * 1024 - 100, 4 * 1024 * 1024 + 100),  # crosses the frame boundary
        (None, 100),
        (-500, None),
    ]:
        assert fs.cat_file(path, start=start, end=end) == orig[slice(start, end)], (start, end)


def test_range_read_fetches_less_than_full_object(s4fs_factory):
    """Sidecar-driven partial fetch must transfer fewer backend bytes than
    a full-object read (the lock-in-free Range GET promise)."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    body_len = len(load_fixture("multi_zstd")["body"])
    path = f"{BUCKET}/multi.bin"
    stub.bytes_fetched = 0
    out = fs.cat_file(path, start=10, end=1000)
    assert out == datagen.multi_frame_body()[10:1000]
    assert stub.bytes_fetched < body_len, (
        f"range read transferred {stub.bytes_fetched}B >= full body {body_len}B"
    )


def test_range_read_on_raw_object_delegates(s4fs_factory):
    fs, _ = s4fs_factory(("raw.bin", "raw"))
    orig = load_fixture("raw")["orig"]
    assert fs.cat_file(f"{BUCKET}/raw.bin", start=5, end=25) == orig[5:25]


def test_range_read_without_sidecar_falls_back_to_full(s4fs_factory):
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    stub.files.pop(f"{BUCKET}/multi.bin.s4index")
    orig = datagen.multi_frame_body()
    with pytest.warns(UserWarning, match="without a usable"):
        out = fs.cat_file(f"{BUCKET}/multi.bin", start=100, end=200)
    assert out == orig[100:200]


def test_stale_sidecar_falls_back_to_full_read(s4fs_factory):
    """A sidecar whose source ETag disagrees with the live object must not
    drive a partial fetch (overwrite-after-index hazard)."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    idx = s4_codec.decode_index(stub.files[path + ".s4index"])
    assert idx["source_etag"], "fixture sidecar should carry an etag binding"
    stub.etags[path] = '"0123456789abcdef0123456789abcdef"'  # != sidecar etag
    orig = datagen.multi_frame_body()
    with pytest.warns(UserWarning, match="without a usable"):
        out = fs.cat_file(path, start=0, end=100)
    assert out == orig[0:100]
    full_calls = [c for c in stub.calls if c[0] == path and c[1] is None and c[2] is None]
    assert full_calls, "stale sidecar must force a full-object read"


# -- parquet via pyarrow ---------------------------------------------------------


def test_pyarrow_parquet_reads_through_s4fs(s4fs_factory):
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    fs, _ = s4fs_factory(("data.parquet", "parquet_zstd"))
    table = pq.read_table(f"{BUCKET}/data.parquet", filesystem=fs)
    import io

    expected = pq.read_table(io.BytesIO(load_fixture("parquet_zstd")["orig"]))
    assert table.equals(expected)
    assert table.num_rows == 5000


def test_pyarrow_parquet_needs_exact_size(s4fs_factory):
    """pyarrow seeks to (size - footer) — a compressed-size `info` would
    point the footer probe at garbage. Assert the exact original size is
    what info() returns for the parquet fixture."""
    fs, _ = s4fs_factory(("data.parquet", "parquet_zstd"))
    assert fs.info(f"{BUCKET}/data.parquet")["size"] == len(
        load_fixture("parquet_zstd")["orig"]
    )


# -- sidecar trust: stale + unbound (legacy v1) -------------------------------


def _reencode_sidecar(raw: bytes, version: int, sse_block: bytes = b"") -> bytes:
    """Re-encode a captured (v2) sidecar under another header version.

    Layouts transcribed from ``crates/s4-codec/src/index.rs``: v1 is the
    32-byte fixed header straight into the entry table; v3 is the v2
    header plus a 30-byte SSE chunk-geometry block before the entries.
    """
    idx = s4_codec.decode_index(raw)
    buf = b"S4IX" + struct.pack(
        "<IQQQ",
        version,
        len(idx["entries"]),
        idx["total_original_size"],
        idx["total_padded_size"],
    )
    if version >= 2:
        etag = (idx["source_etag"] or "").encode()
        buf += struct.pack("<QI", idx["source_compressed_size"] or 0, len(etag)) + etag
    if version == 3:
        assert len(sse_block) == 30, "v3 SSE block is 30 bytes"
        buf += sse_block
    for e in idx["entries"]:
        buf += struct.pack(
            "<QQQQ",
            e["original_offset"],
            e["original_size"],
            e["compressed_offset"],
            e["compressed_size"],
        )
    return buf


def test_stale_sidecar_size_falls_back_to_metadata(s4fs_factory):
    """info() must not trust a sidecar whose ETag binding disagrees with
    the live object — size resolution falls back to object metadata."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    stub.etags[path] = '"0123456789abcdef0123456789abcdef"'  # != sidecar etag
    # Distinguishable metadata value proves the sidecar total was not used.
    stub.meta[path]["s4-original-size"] = "12345"
    info = fs.info(path)
    assert info["size"] == 12345
    assert info["s4_size_exact"] is True


def test_unbound_v1_sidecar_not_trusted_for_range(s4fs_factory):
    """A legacy v1 sidecar (no source ETag / compressed-size binding) must
    not drive partial fetches — range reads fall back to a full read."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    raw_v1 = _reencode_sidecar(stub.files[path + ".s4index"], version=1)
    idx = s4_codec.decode_index(raw_v1)
    assert idx["source_etag"] is None and idx["source_compressed_size"] is None
    assert idx["entries"], "v1 re-encoding kept the captured entry table"
    stub.files[path + ".s4index"] = raw_v1
    orig = datagen.multi_frame_body()
    with pytest.warns(UserWarning, match="without a usable"):
        out = fs.cat_file(path, start=100, end=200)
    assert out == orig[100:200]
    full_calls = [c for c in stub.calls if c[0] == path and c[1] is None and c[2] is None]
    assert full_calls, "unbound v1 sidecar must force a full-object read"


def test_unbound_v1_sidecar_not_trusted_for_size(s4fs_factory):
    """A legacy v1 sidecar must not feed exact sizes either: with metadata
    gone, info() reports the compressed size flagged inexact."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    stub.files[path + ".s4index"] = _reencode_sidecar(
        stub.files[path + ".s4index"], version=1
    )
    stub.meta.clear()
    info = fs.info(path)
    assert info["size"] == len(load_fixture("multi_zstd")["body"])
    assert info["s4_size_exact"] is False


# -- SSE refusal: never return ciphertext -------------------------------------

# Transcribed from crates/s4-server/src/sse.rs SSE_MAGIC_V1..V6.
SSE_MAGICS = [b"S4E1", b"S4E2", b"S4E3", b"S4E4", b"S4E5", b"S4E6"]


@pytest.mark.parametrize("magic", SSE_MAGICS, ids=[m.decode() for m in SSE_MAGICS])
def test_sse_envelope_magic_refused_without_metadata(magic):
    """Defense in depth: even when object metadata is unreachable, the
    S4E* envelope magic alone must trigger the refusal — ciphertext must
    never come back as if it were data."""
    stub = stub_with()
    stub.files[f"{BUCKET}/secret.bin"] = magic + b"\x00" * 64
    fs = S4FileSystem(fs=stub)
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(f"{BUCKET}/secret.bin")


def test_sse_metadata_flag_refused_full_and_range():
    """The gateway stamps `s4-encrypted: aes-256-gcm` on every encrypted
    PUT (service.rs); that flag alone must refuse full and range reads."""
    stub = stub_with()
    path = f"{BUCKET}/secret.bin"
    stub.files[path] = b"S4E2" + b"\x00" * 64
    stub.meta[path] = {"s4-encrypted": "aes-256-gcm"}
    fs = S4FileSystem(fs=stub)
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(path)
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(path, start=0, end=10)


def test_sse_range_read_refused_by_magic_sniff():
    """Range read on an SSE body with no metadata and no sidecar: the
    4-byte head sniff must refuse before delegating the range."""
    stub = stub_with()
    path = f"{BUCKET}/secret.bin"
    stub.files[path] = b"S4E6" + b"\x00" * 64
    fs = S4FileSystem(fs=stub)
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(path, start=5, end=20)


def test_sse_v3_sidecar_binding_refuses_range_read(s4fs_factory):
    """A v3 sidecar carrying an SSE chunk binding (idx["sse"] non-None)
    must refuse the sidecar-driven range path: its offsets describe the
    pre-encrypt body, and the stored bytes are ciphertext."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    sse_block = struct.pack(
        "<IIH8sQI",
        65536,  # enc_chunk_size (>0 → binding present)
        2,  # enc_chunk_count
        1,  # enc_key_id
        b"\x00" * 8,  # enc_salt
        len(stub.files[path]),  # enc_plaintext_len
        24,  # enc_header_bytes (S4E6_HEADER_BYTES)
    )
    stub.files[path + ".s4index"] = _reencode_sidecar(
        stub.files[path + ".s4index"], version=3, sse_block=sse_block
    )
    idx = s4_codec.decode_index(stub.files[path + ".s4index"])
    assert idx["sse"] is not None, "v3 re-encoding must carry the SSE binding"
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(path, start=10, end=100)


# -- versioning shadow keys are hidden ----------------------------------------


def test_version_shadow_keys_hidden_everywhere(s4fs_factory):
    """Shadow keys are an *infix* (`<key>.__s4ver__/<vid>`, service.rs
    version_shadow_key) — they must vanish from ls / find / glob / info
    even when the user key lives under a sub-prefix."""
    fs, stub = s4fs_factory(("docs/report.parquet", "parquet_zstd"))
    shadow = f"{BUCKET}/docs/report.parquet.__s4ver__/v0123456789abcdef"
    stub.files[shadow] = load_fixture("raw")["orig"]

    assert fs.ls(f"{BUCKET}/docs") == [f"{BUCKET}/docs/report.parquet"]
    assert all(".__s4ver__" not in n for n in fs.find(BUCKET))
    assert fs.glob(f"{BUCKET}/docs/*") == [f"{BUCKET}/docs/report.parquet"]
    assert not fs.exists(shadow)
    with pytest.raises(FileNotFoundError):
        fs.info(shadow)


# -- open() refuses inexact sizes ----------------------------------------------


def test_open_refuses_inexact_size(s4fs_factory):
    """Framed object, no sidecar, no metadata: AbstractBufferedFile would
    clamp reads to the *compressed* size — open() must refuse instead of
    silently truncating the stream."""
    fs, stub = s4fs_factory(("text.txt", "text_zstd"))
    stub.files.pop(f"{BUCKET}/text.txt.s4index", None)
    stub.meta.clear()
    with pytest.raises(ValueError, match="cat_file"):
        fs.open(f"{BUCKET}/text.txt", "rb")
    # cat_file is unaffected: full decode does not depend on info() size.
    assert fs.cat_file(f"{BUCKET}/text.txt") == load_fixture("text_zstd")["orig"]


def test_open_inexact_opt_in_restores_clamped_reads():
    """allow_inexact_open=True restores the pre-1.0.1 behavior: reads are
    clamped (documented truncation) instead of raising."""
    stub = stub_with(("text.txt", "text_zstd"))
    stub.files.pop(f"{BUCKET}/text.txt.s4index", None)
    stub.meta.clear()
    fs = S4FileSystem(fs=stub, allow_inexact_open=True)
    with fs.open(f"{BUCKET}/text.txt", "rb") as f:
        data = f.read()
    orig = load_fixture("text_zstd")["orig"]
    body_len = len(load_fixture("text_zstd")["body"])
    assert data == orig[:body_len]  # clamped at the compressed size


def test_open_exact_size_unaffected_by_guard(s4fs_factory):
    """Objects with an exact size (metadata or sidecar) open as before."""
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    with fs.open(f"{BUCKET}/text.txt", "rb") as f:
        assert f.read() == load_fixture("text_zstd")["orig"]


# -- codec edge cases -------------------------------------------------------------


def test_nvcomp_frame_raises_not_implemented(s4fs_factory):
    """GPU frames can't be fixture-captured without a GPU; synthesizing a
    single frame header with a GPU codec id (2 = nvcomp-zstd) is the
    explicitly-sanctioned exception to the no-hand-rolled-bytes rule."""
    payload = b"\x00" * 32
    frame = struct.pack("<4sIQQI", b"S4F2", 2, 64, len(payload), 0) + payload
    stub = stub_with()
    stub.files[f"{BUCKET}/gpu.bin"] = frame
    fs = S4FileSystem(fs=stub)
    with pytest.raises(NotImplementedError, match="gateway or the\n?\\s*s4-codec CLI"):
        fs.cat_file(f"{BUCKET}/gpu.bin")


def test_dict_fingerprint_mismatch_raises(s4fs_factory):
    fs, stub = s4fs_factory(("events/new.json", "dict_event"))
    dict_keys = [k for k in stub.files if "/.s4dict/" in k]
    assert dict_keys, "dict fixture missing"
    stub.files[dict_keys[0]] = stub.files[dict_keys[0]] + b"tampered"
    with pytest.raises(ValueError, match="fingerprint mismatch"):
        fs.cat_file(f"{BUCKET}/events/new.json")


def test_corrupted_frame_payload_raises(s4fs_factory):
    fs, stub = s4fs_factory(("text.txt", "text_zstd"))
    path = f"{BUCKET}/text.txt"
    body = bytearray(stub.files[path])
    body[40] ^= 0xFF  # flip a payload byte past the 28-byte header
    stub.files[path] = bytes(body)
    with pytest.raises((s4_codec.S4Error, OSError, RuntimeError)):
        fs.cat_file(path)


# -- read-only enforcement ---------------------------------------------------------


@pytest.mark.parametrize(
    "op",
    [
        lambda fs: fs.pipe_file("b/x", b"data"),
        lambda fs: fs.rm("b/x"),
        lambda fs: fs.rm_file("b/x"),
        lambda fs: fs.mkdir("b/x"),
        lambda fs: fs.makedirs("b/x"),
        lambda fs: fs.touch("b/x"),
        lambda fs: fs.mv("b/x", "b/y"),
        lambda fs: fs.cp_file("b/x", "b/y"),
        lambda fs: fs.open("b/x", "wb"),
    ],
)
def test_write_apis_raise_read_only(s4fs_factory, op):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    with pytest.raises(NotImplementedError, match="read-only"):
        op(fs)


# -- fsspec registration ---------------------------------------------------------


def test_protocol_registered():
    import fsspec

    stub = stub_with(("text.txt", "text_zstd"))
    fs = fsspec.filesystem("s4", fs=stub)
    assert isinstance(fs, S4FileSystem)
