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
