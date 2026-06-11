"""End-to-end: MinIO (docker) + the real ``s4`` gateway + s4fs over s3fs.

Gated behind the ``e2e`` marker (excluded by the default ``addopts`` in
``pyproject.toml``). Run explicitly with::

    cargo build -p s4-server          # or set S4_BIN
    pytest tests/test_e2e_minio.py -m e2e

Requires docker and a built ``s4`` binary (``S4_BIN`` env var or
``<repo>/target/debug/s4``).
"""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from typing import Any, Optional

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import datagen  # noqa: E402

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
S4_BIN = os.environ.get("S4_BIN", str(REPO_ROOT / "target" / "debug" / "s4"))

MINIO_PORT = 19200
GW_PORT = 19201
BUCKET = "s4fs-e2e"
MINIO_CONTAINER = "s4fs-e2e-minio"
ENDPOINT = f"http://127.0.0.1:{MINIO_PORT}"
CREDS_ENV = {
    "AWS_ACCESS_KEY_ID": "minioadmin",
    "AWS_SECRET_ACCESS_KEY": "minioadmin",
    "AWS_REGION": "us-east-1",
}

pytestmark = [
    pytest.mark.e2e,
    pytest.mark.skipif(shutil.which("docker") is None, reason="docker not available"),
    pytest.mark.skipif(not pathlib.Path(S4_BIN).exists(), reason=f"s4 binary not found at {S4_BIN}"),
]


def _wait_port(port: int, timeout: float = 60.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket() as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.3)
    raise RuntimeError(f"port {port} did not open within {timeout}s")


def _boto(port: int):
    import boto3
    from botocore.config import Config

    return boto3.client(
        "s3",
        endpoint_url=f"http://127.0.0.1:{port}",
        aws_access_key_id="minioadmin",
        aws_secret_access_key="minioadmin",
        region_name="us-east-1",
        config=Config(s3={"addressing_style": "path"}),
    )


def _gateway(*extra: str) -> subprocess.Popen:
    proc = subprocess.Popen(
        [
            S4_BIN,
            "--host",
            "127.0.0.1",
            "--port",
            str(GW_PORT),
            "--endpoint-url",
            ENDPOINT,
            # Pin the dispatcher: with the default `sampling` dispatcher a
            # GPU-featured binary on a CUDA host would compress >=1MiB
            # bodies with nvcomp-* — frames s4fs (correctly) refuses. This
            # suite exercises the CPU codecs, so force the --codec choice.
            "--dispatcher",
            "always",
            *extra,
        ],
        env=os.environ | CREDS_ENV,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _wait_port(GW_PORT)
    time.sleep(0.5)
    return proc


@pytest.fixture(scope="module")
def backend():
    """MinIO + objects written through real gateway runs (zstd / gzip /
    zstd-dict) and one raw direct PUT."""
    subprocess.run(["docker", "rm", "-f", MINIO_CONTAINER], capture_output=True)
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            MINIO_CONTAINER,
            "-p",
            f"{MINIO_PORT}:9000",
            "minio/minio:latest",
            "server",
            "/data",
        ],
        check=True,
        capture_output=True,
    )
    try:
        _wait_port(MINIO_PORT)
        time.sleep(1.0)
        be = _boto(MINIO_PORT)
        be.create_bucket(Bucket=BUCKET)

        be.put_object(Bucket=BUCKET, Key="raw.bin", Body=b"never compressed\n" * 20)
        for i in range(200):
            be.put_object(
                Bucket=BUCKET, Key=f"events/sample-{i:04d}.json", Body=datagen.json_event(i)
            )

        gw = _gateway("--codec", "cpu-zstd")
        try:
            gwc = _boto(GW_PORT)
            gwc.put_object(Bucket=BUCKET, Key="text.txt", Body=datagen.text_body())
            gwc.put_object(Bucket=BUCKET, Key="multi.bin", Body=datagen.multi_frame_body())
            gwc.put_object(Bucket=BUCKET, Key="data.parquet", Body=datagen.parquet_body())
        finally:
            gw.terminate()
            gw.wait()

        gw = _gateway("--codec", "cpu-gzip")
        try:
            _boto(GW_PORT).put_object(Bucket=BUCKET, Key="text-gzip.txt", Body=datagen.text_body())
        finally:
            gw.terminate()
            gw.wait()

        out = subprocess.run(
            [S4_BIN, "train-dict", f"{BUCKET}/events/", "--endpoint-url", ENDPOINT],
            env=os.environ | CREDS_ENV,
            capture_output=True,
            text=True,
            check=True,
        )
        m = re.search(r"\.s4dict/([0-9a-f]{16})", out.stdout + out.stderr)
        assert m, f"train-dict printed no dict id:\n{out.stdout}\n{out.stderr}"
        dict_id = m.group(1)

        gw = _gateway("--codec", "cpu-zstd", "--zstd-dict", f"{BUCKET}/events/={dict_id}")
        try:
            _boto(GW_PORT).put_object(
                Bucket=BUCKET, Key="events/new.json", Body=datagen.json_event(7777)
            )
        finally:
            gw.terminate()
            gw.wait()

        # SSE-S4 keyring run: the stored body is an S4E6 envelope (chunked
        # AES-256-GCM, default --sse-chunk-size) that s4fs must refuse to
        # serve — the keyring lives gateway-side only.
        with tempfile.TemporaryDirectory() as td:
            keyfile = pathlib.Path(td) / "sse.key"
            keyfile.write_bytes(os.urandom(32))
            gw = _gateway("--codec", "cpu-zstd", "--sse-s4-key", str(keyfile))
            try:
                _boto(GW_PORT).put_object(
                    Bucket=BUCKET, Key="secret.bin", Body=b"top secret payload\n" * 2048
                )
            finally:
                gw.terminate()
                gw.wait()

        yield be
    finally:
        subprocess.run(["docker", "rm", "-f", MINIO_CONTAINER], capture_output=True)


class CountingFS:
    """Delegating wrapper around an fsspec filesystem that counts the bytes
    each ``cat_file`` call returned (a proxy for backend transfer volume)."""

    def __init__(self, inner: Any):
        self._inner = inner
        self.bytes_fetched = 0

    def cat_file(self, path: str, start: Optional[int] = None, end: Optional[int] = None, **kw):
        out = self._inner.cat_file(path, start=start, end=end, **kw)
        self.bytes_fetched += len(out)
        return out

    def __getattr__(self, name: str):
        return getattr(self._inner, name)


def _s3(**kw):
    import s3fs

    return s3fs.S3FileSystem(
        key="minioadmin",
        secret="minioadmin",
        endpoint_url=ENDPOINT,
        **kw,
    )


@pytest.fixture()
def s4(backend):
    from s4fs import S4FileSystem

    inner = CountingFS(_s3(skip_instance_cache=True))
    return S4FileSystem(fs=inner), inner


def test_cat_matches_original_all_codecs(s4):
    fs, _ = s4
    assert fs.cat_file(f"{BUCKET}/text.txt") == datagen.text_body()
    assert fs.cat_file(f"{BUCKET}/multi.bin") == datagen.multi_frame_body()
    assert fs.cat_file(f"{BUCKET}/text-gzip.txt") == datagen.text_body()
    assert fs.cat_file(f"{BUCKET}/events/new.json") == datagen.json_event(7777)
    assert fs.cat_file(f"{BUCKET}/raw.bin") == b"never compressed\n" * 20


def test_ls_reports_original_sizes_and_hides_internals(s4):
    fs, _ = s4
    detail = {e["name"]: e for e in fs.ls(BUCKET, detail=True)}
    assert detail[f"{BUCKET}/text.txt"]["size"] == len(datagen.text_body())
    assert detail[f"{BUCKET}/multi.bin"]["size"] == len(datagen.multi_frame_body())
    assert detail[f"{BUCKET}/text-gzip.txt"]["size"] == len(datagen.text_body())
    assert detail[f"{BUCKET}/data.parquet"]["size"] == len(datagen.parquet_body())
    names = list(detail)
    assert not any(n.endswith(".s4index") for n in names)
    assert not any(".s4dict" in n for n in names)


def test_pyarrow_read_table(s4):
    import io

    import pyarrow.parquet as pq

    fs, _ = s4
    table = pq.read_table(f"{BUCKET}/data.parquet", filesystem=fs)
    expected = pq.read_table(io.BytesIO(datagen.parquet_body()))
    assert table.equals(expected)


def test_pandas_read_parquet_via_url(backend):
    pd = pytest.importorskip("pandas")
    df = pd.read_parquet(
        f"s4://{BUCKET}/data.parquet",
        storage_options={
            "target_options": {
                "key": "minioadmin",
                "secret": "minioadmin",
                "endpoint_url": ENDPOINT,
                "skip_instance_cache": True,
            },
            "skip_instance_cache": True,
        },
    )
    assert len(df) == 5000
    assert list(df.columns) == ["id", "name", "value"]


def test_duckdb_read_parquet(s4):
    duckdb = pytest.importorskip("duckdb")
    fs, _ = s4
    con = duckdb.connect()
    con.register_filesystem(fs)
    n = con.execute(f"SELECT count(*) FROM read_parquet('s4://{BUCKET}/data.parquet')").fetchone()
    assert n[0] == 5000


def test_range_read_matches_and_transfers_less(s4, backend):
    fs, counting = s4
    orig = datagen.multi_frame_body()
    path = f"{BUCKET}/multi.bin"
    body_len = backend.head_object(Bucket=BUCKET, Key="multi.bin")["ContentLength"]

    full = fs.cat_file(path)
    assert full == orig

    counting.bytes_fetched = 0
    sliced = fs.cat_file(path, start=100, end=4096)
    assert sliced == orig[100:4096] == full[100:4096]
    assert counting.bytes_fetched < body_len, (
        f"range read transferred {counting.bytes_fetched}B >= compressed body {body_len}B"
    )

    # Crossing the 4 MiB frame boundary still decodes correctly.
    lo, hi = 4 * 1024 * 1024 - 50, 4 * 1024 * 1024 + 50
    assert fs.cat_file(path, start=lo, end=hi) == orig[lo:hi]


def test_open_seek_tail(s4):
    fs, _ = s4
    orig = datagen.multi_frame_body()
    with fs.open(f"{BUCKET}/multi.bin", "rb") as f:
        f.seek(-1234, 2)
        assert f.read() == orig[-1234:]


def test_write_through_s4fs_refused(s4):
    fs, _ = s4
    with pytest.raises(NotImplementedError, match="read-only"):
        fs.pipe_file(f"{BUCKET}/nope.bin", b"x")


def test_sse_object_refused(s4, backend):
    """An object PUT through an SSE-S4 (keyring) gateway is stored as an
    S4E* ciphertext envelope — s4fs must refuse it loudly, never return
    ciphertext, for both full and range reads."""
    fs, _ = s4
    body = backend.get_object(Bucket=BUCKET, Key="secret.bin")["Body"].read()
    assert body[:4] in (b"S4E2", b"S4E5", b"S4E6"), "fixture should be SSE-encrypted on disk"
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(f"{BUCKET}/secret.bin")
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        fs.cat_file(f"{BUCKET}/secret.bin", start=10, end=200)
    with pytest.raises(NotImplementedError, match="does not decrypt SSE"):
        with fs.open(f"{BUCKET}/secret.bin", "rb") as f:
            f.read()
