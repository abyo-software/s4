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


def test_range_read_after_info_still_uses_sidecar(s4fs_factory):
    """Regression (v1.2): info() rewrites size to the *original* size; the
    live-info snapshot it seeds for the sidecar binding check must keep
    the *backend* size, or every post-info() range read silently loses
    the partial-fetch fast-path (full-read fallback + warning)."""
    fs, stub = s4fs_factory(("multi.bin", "multi_zstd"))
    path = f"{BUCKET}/multi.bin"
    orig = datagen.multi_frame_body()
    assert fs.info(path)["size"] == len(orig)  # poisons the cache pre-fix
    stub.bytes_fetched = 0
    import warnings

    with warnings.catch_warnings():
        warnings.simplefilter("error")  # the fallback path warns — fail on it
        assert fs.cat_file(path, start=10, end=1000) == orig[10:1000]
    assert stub.bytes_fetched < len(load_fixture("multi_zstd")["body"])


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


# -- read-only enforcement (write_enabled defaults to False) -------------------


@pytest.mark.parametrize(
    "op",
    [
        lambda fs: fs.pipe_file("b/x", b"data"),
        lambda fs: fs.put_file("/etc/hostname", "b/x"),
        lambda fs: fs.mkdir("b/x"),
        lambda fs: fs.makedirs("b/x"),
        lambda fs: fs.touch("b/x"),
        lambda fs: fs.open("b/x", "wb"),
    ],
)
def test_write_apis_raise_read_only_by_default(s4fs_factory, op):
    fs, _ = s4fs_factory(("text.txt", "text_zstd"))
    with pytest.raises(NotImplementedError, match="read-only"):
        op(fs)


@pytest.mark.parametrize(
    "op",
    [
        lambda fs: fs.rm("b/x"),
        lambda fs: fs.rm_file("b/x"),
        lambda fs: fs.mv("b/x", "b/y"),
        lambda fs: fs.cp_file("b/x", "b/y"),
        lambda fs: fs.rmdir("b/x"),
    ],
)
def test_mutation_apis_stay_unsupported_even_with_writes_enabled(op):
    stub = stub_with(("text.txt", "text_zstd"))
    fs = S4FileSystem(fs=stub, write_enabled=True)
    with pytest.raises(NotImplementedError, match="does not implement"):
        op(fs)


# -- fsspec registration ---------------------------------------------------------


def test_protocol_registered():
    import fsspec

    stub = stub_with(("text.txt", "text_zstd"))
    fs = fsspec.filesystem("s4", fs=stub)
    assert isinstance(fs, S4FileSystem)


# -- write support (v1.2, opt-in) ----------------------------------------------


def _writable(**kwargs):
    """Fresh writable S4FileSystem over an empty stub backend.

    The stub starts with one pre-existing raw object so the bucket
    "exists" for ls()-style probes."""
    stub = stub_with(("raw.bin", "raw"))
    fs = S4FileSystem(fs=stub, write_enabled=True, **kwargs)
    return fs, stub


def test_write_single_frame_stamps_gateway_metadata():
    fs, stub = _writable()
    data = datagen.text_body()
    path = f"{BUCKET}/written.txt"
    fs.pipe_file(path, data)

    stored = stub.files[path]
    assert stored[:4] == b"S4F2", "body must be S4F2-framed"
    assert len(stored) < len(data), "zstd frame should compress the text"
    # Metadata restates the gateway PUT-path stamp (service.rs
    # write_manifest + META_FRAMED).
    meta = stub.meta[path]
    assert meta["s4-codec"] == "cpu-zstd"
    assert meta["s4-original-size"] == str(len(data))
    assert meta["s4-compressed-size"] == str(len(stored))
    assert meta["s4-crc32c"] == str(s4_codec.crc32c(data))
    assert meta["s4-framed"] == "true"
    # Single frame => no sidecar (gateway policy).
    assert path + ".s4index" not in stub.files

    # Read back through a *fresh* instance (no warm caches).
    fresh = S4FileSystem(fs=stub)
    assert fresh.cat_file(path) == data
    assert fresh.info(path)["size"] == len(data)
    assert fresh.info(path)["s4_size_exact"] is True


def test_write_multi_frame_emits_bound_sidecar_and_partial_reads():
    fs, stub = _writable(write_zstd_level=1)
    data = datagen.multi_frame_body()  # ~5.1 MiB -> two 4 MiB-chunk frames
    path = f"{BUCKET}/multi-written.bin"
    fs.pipe_file(path, data)

    body = stub.files[path]
    sidecar = stub.files[path + ".s4index"]
    idx = s4_codec.decode_index(sidecar)
    assert len(idx["entries"]) == 2
    assert idx["total_original_size"] == len(data)
    assert idx["total_padded_size"] == len(body)
    # Version binding == what the backend reported for the body PUT
    # (gateway write_sidecar contract; quote-stripped ETag form).
    assert idx["source_compressed_size"] == len(body)
    assert idx["source_etag"] == stub.etags[path].strip('"')

    fresh = S4FileSystem(fs=stub)
    assert fresh.cat_file(path) == data
    # Sidecar-driven range read fetches less than the compressed body.
    stub.bytes_fetched = 0
    assert fresh.cat_file(path, start=10, end=900) == data[10:900]
    assert stub.bytes_fetched < len(body)
    # Frame-boundary crossing decodes correctly.
    lo, hi = 4 * 1024 * 1024 - 64, 4 * 1024 * 1024 + 64
    assert fresh.cat_file(path, start=lo, end=hi) == data[lo:hi]


def test_overwrite_single_frame_removes_stale_sidecar():
    fs, stub = _writable(write_zstd_level=1)
    path = f"{BUCKET}/shrinking.bin"
    fs.pipe_file(path, datagen.multi_frame_body())
    assert path + ".s4index" in stub.files
    fs.pipe_file(path, b"now tiny")
    assert path + ".s4index" not in stub.files, "stale sidecar must be cleaned up"
    fresh = S4FileSystem(fs=stub)
    assert fresh.cat_file(path) == b"now tiny"


def test_write_passthrough_codec_stores_raw_bytes():
    fs, stub = _writable()
    fs.write_codec = "passthrough"
    data = b"already compressed \x00\x01\x02" * 100
    path = f"{BUCKET}/raw-written.bin"
    fs.pipe_file(path, data)
    assert stub.files[path] == data, "passthrough body is byte-identical"
    meta = stub.meta[path]
    assert meta["s4-codec"] == "passthrough"
    assert "s4-framed" not in meta, "gateway never frames passthrough"
    assert S4FileSystem(fs=stub).cat_file(path) == data


def test_write_empty_object():
    fs, stub = _writable()
    path = f"{BUCKET}/empty.bin"
    fs.pipe_file(path, b"")
    assert stub.files[path] == b""
    assert stub.meta[path]["s4-original-size"] == "0"
    assert S4FileSystem(fs=stub).cat_file(path) == b""


def test_open_wb_buffers_and_writes_on_close():
    fs, stub = _writable()
    data = datagen.text_body()
    path = f"{BUCKET}/streamed.txt"
    with fs.open(path, "wb") as f:
        f.write(data[: len(data) // 2])
        f.write(data[len(data) // 2 :])
    assert stub.files[path][:4] == b"S4F2"
    assert stub.meta[path]["s4-original-size"] == str(len(data))
    assert S4FileSystem(fs=stub).cat_file(path) == data


def test_put_file_roundtrip(tmp_path):
    fs, stub = _writable()
    local = tmp_path / "local.bin"
    data = b"local file payload " * 500
    local.write_bytes(data)
    fs.put_file(str(local), f"{BUCKET}/uploaded.bin")
    assert S4FileSystem(fs=stub).cat_file(f"{BUCKET}/uploaded.bin") == data


def test_write_refused_on_metadata_less_fs():
    """A backend that drops metadata would produce 'unstamped framed'
    objects the gateway serves raw — refuse with the typed error."""
    from conftest import NoMetadataStubFileSystem

    from s4fs import S4MetadataUnsupportedError

    stub = NoMetadataStubFileSystem({})
    fs = S4FileSystem(fs=stub, write_enabled=True)
    with pytest.raises(S4MetadataUnsupportedError, match="metadata"):
        fs.pipe_file(f"{BUCKET}/x.bin", b"data")
    with pytest.raises(S4MetadataUnsupportedError, match="metadata"):
        fs.open(f"{BUCKET}/x.bin", "wb")
    assert f"{BUCKET}/x.bin" not in stub.files, "no body may land without the stamp"


def test_write_to_reserved_keys_rejected():
    fs, _ = _writable()
    with pytest.raises(ValueError, match="reserved"):
        fs.pipe_file(f"{BUCKET}/x.s4index", b"data")
    with pytest.raises(ValueError, match="reserved"):
        fs.pipe_file(f"{BUCKET}/.s4dict/0123456789abcdef", b"data")


def test_append_mode_rejected():
    fs, _ = _writable()
    with pytest.raises(NotImplementedError, match="'rb' and 'wb'"):
        fs.open(f"{BUCKET}/x.bin", "ab")


def test_write_unsupported_codec_points_at_gateway():
    fs, _ = _writable(write_codec="cpu-gzip")
    with pytest.raises(NotImplementedError, match="gateway"):
        fs.pipe_file(f"{BUCKET}/x.bin", b"data")


def test_storage_options_enable_writes_via_fsspec():
    import fsspec

    stub = stub_with(("raw.bin", "raw"))
    fs = fsspec.filesystem("s4", fs=stub, write_enabled=True, skip_instance_cache=True)
    data = b"via storage_options " * 64
    with fsspec.open(f"s4://{BUCKET}/opt.bin", "wb", fs=stub, write_enabled=True) as f:
        f.write(data)
    assert fs.cat_file(f"{BUCKET}/opt.bin") == data
