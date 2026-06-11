"""End-to-end for s4fs *writes*: MinIO (docker) + the real ``s4`` binary.

Proves the central write contract — everything s4fs writes directly to the
backend is fully gateway-compatible:

(a) gateway GET decodes the object back to the original bytes;
(b) ``s4 verify-sidecar`` reports OK (version binding intact);
(c) gateway Range GET (sidecar fast-path) returns the right slice;
(d) pandas ``df.to_parquet("s4://…")`` → ``pd.read_parquet`` round-trips;
(e) the gateway-served bytes of the pandas-written object parse with pyarrow.

Gated behind the ``e2e`` marker (excluded by the default ``addopts``)::

    cargo build -p s4-server          # or set S4_BIN
    pytest tests/test_e2e_s4fs_write.py -m e2e

Requires docker and a built ``s4`` binary. A GPU-featured binary needs the
nvCOMP shared libraries on LD_LIBRARY_PATH; the gateway helper below
appends the conventional /tmp/nvcomp extract dir when present.
"""

from __future__ import annotations

import io
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import time

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import datagen  # noqa: E402

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
S4_BIN = os.environ.get("S4_BIN", str(REPO_ROOT / "target" / "debug" / "s4"))
NVCOMP_LIB = "/tmp/nvcomp/nvcomp-linux-x86_64-5.2.0.10_cuda12-archive/lib"

MINIO_PORT = 19210
GW_PORT = 19211
BUCKET = "s4fs-write-e2e"
MINIO_CONTAINER = "s4fs-write-e2e-minio"
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

STORAGE_OPTIONS = {
    "write_enabled": True,
    "target_options": {
        "key": "minioadmin",
        "secret": "minioadmin",
        "endpoint_url": ENDPOINT,
        "skip_instance_cache": True,
    },
    "skip_instance_cache": True,
}


def _s4_env() -> dict:
    env = dict(os.environ | CREDS_ENV)
    if os.path.isdir(NVCOMP_LIB):
        prev = env.get("LD_LIBRARY_PATH")
        env["LD_LIBRARY_PATH"] = f"{NVCOMP_LIB}:{prev}" if prev else NVCOMP_LIB
    return env


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
    """Real gateway against MinIO. ``--dispatcher always`` is pinned for the
    same reason as the read e2e: a GPU-featured binary must not promote PUTs
    to nvcomp frames (this suite only PUTs through s4fs anyway, but keep the
    GET-side behavior deterministic)."""
    proc = subprocess.Popen(
        [
            S4_BIN,
            "--host",
            "127.0.0.1",
            "--port",
            str(GW_PORT),
            "--endpoint-url",
            ENDPOINT,
            "--dispatcher",
            "always",
            "--codec",
            "cpu-zstd",
            *extra,
        ],
        env=_s4_env(),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _wait_port(GW_PORT)
    time.sleep(0.5)
    return proc


def _verify_sidecar(key: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [S4_BIN, "verify-sidecar", f"{BUCKET}/{key}", "--endpoint-url", ENDPOINT],
        env=_s4_env(),
        capture_output=True,
        text=True,
    )


@pytest.fixture(scope="module")
def backend():
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
        yield be
    finally:
        subprocess.run(["docker", "rm", "-f", MINIO_CONTAINER], capture_output=True)


def _s4fs(**kw):
    import s3fs

    from s4fs import S4FileSystem

    inner = s3fs.S3FileSystem(
        key="minioadmin",
        secret="minioadmin",
        endpoint_url=ENDPOINT,
        skip_instance_cache=True,
    )
    return S4FileSystem(fs=inner, write_enabled=True, **kw)


@pytest.fixture(scope="module")
def written(backend):
    """Write the suite's objects through s4fs once, then assert against the
    same backend from every test."""
    fs = _s4fs()
    text = datagen.text_body()  # ~64 KiB -> single frame, no sidecar
    multi = datagen.multi_frame_body()  # ~5.1 MiB -> 2 frames + sidecar
    fs.pipe_file(f"{BUCKET}/text.txt", text)
    fs.pipe_file(f"{BUCKET}/multi.bin", multi)
    return {"text.txt": text, "multi.bin": multi}


def test_backend_objects_are_s4_format(backend, written):
    """The stored bytes are framed + metadata-stamped, the multi-frame
    object grew a bound sidecar — i.e. s4fs really wrote S4 format, the
    rest of this suite isn't accidentally testing passthrough."""
    head = backend.head_object(Bucket=BUCKET, Key="multi.bin")
    meta = head["Metadata"]
    assert meta["s4-codec"] == "cpu-zstd"
    assert meta["s4-framed"] == "true"
    assert meta["s4-original-size"] == str(len(written["multi.bin"]))
    assert int(meta["s4-compressed-size"]) == head["ContentLength"]
    body = backend.get_object(Bucket=BUCKET, Key="multi.bin")["Body"].read()
    assert body[:4] == b"S4F2"
    assert len(body) < len(written["multi.bin"])
    backend.head_object(Bucket=BUCKET, Key="multi.bin.s4index")  # sidecar exists
    # Single-frame object: framed + stamped, but no sidecar (gateway policy).
    backend.head_object(Bucket=BUCKET, Key="text.txt")
    with pytest.raises(Exception, match="404|Not Found"):
        backend.head_object(Bucket=BUCKET, Key="text.txt.s4index")


def test_gateway_get_decodes_s4fs_written_objects(backend, written):
    """(a) the real gateway serves the original bytes back."""
    gw = _gateway()
    try:
        gwc = _boto(GW_PORT)
        for key, orig in written.items():
            got = gwc.get_object(Bucket=BUCKET, Key=key)["Body"].read()
            assert got == orig, f"gateway GET mismatch for {key}"
    finally:
        gw.terminate()
        gw.wait()


def test_verify_sidecar_reports_ok(backend, written):
    """(b) ``s4 verify-sidecar``: multi-frame object has an intact version
    binding; single-frame object is the by-design sidecar-less case."""
    out = _verify_sidecar("multi.bin")
    assert out.returncode == 0, out.stdout + out.stderr
    assert out.stdout.startswith("OK"), out.stdout
    assert "version binding intact" in out.stdout, out.stdout

    out = _verify_sidecar("text.txt")
    assert out.returncode == 0, out.stdout + out.stderr
    assert out.stdout.startswith("OK"), out.stdout


def test_gateway_range_get_uses_sidecar_fast_path(backend, written):
    """(c) gateway Range GETs (the sidecar fast-path for multi-frame
    objects) return the correct slices, including across the 4 MiB frame
    boundary."""
    orig = written["multi.bin"]
    gw = _gateway()
    try:
        gwc = _boto(GW_PORT)
        for lo, hi in [
            (0, 99),
            (100, 4096),
            (4 * 1024 * 1024 - 50, 4 * 1024 * 1024 + 50),
            (len(orig) - 1234, len(orig) - 1),
        ]:
            got = gwc.get_object(
                Bucket=BUCKET, Key="multi.bin", Range=f"bytes={lo}-{hi}"
            )["Body"].read()
            assert got == orig[lo : hi + 1], (lo, hi)
    finally:
        gw.terminate()
        gw.wait()


def test_s4fs_reads_back_its_own_writes(backend, written):
    """(d-prep) a fresh s4fs instance reads back full bodies and ranges."""
    fs = _s4fs()
    for key, orig in written.items():
        assert fs.cat_file(f"{BUCKET}/{key}") == orig
        assert fs.info(f"{BUCKET}/{key}")["size"] == len(orig)
    multi = written["multi.bin"]
    lo, hi = 4 * 1024 * 1024 - 50, 4 * 1024 * 1024 + 50
    assert fs.cat_file(f"{BUCKET}/multi.bin", start=lo, end=hi) == multi[lo:hi]


def test_pandas_to_parquet_roundtrip_via_url(backend):
    """(d) the headline workflow: ``df.to_parquet("s4://…")`` straight to
    the backend, read back with ``pd.read_parquet`` over the same URL."""
    pd = pytest.importorskip("pandas")
    df = pd.DataFrame(
        {
            "id": list(range(5000)),
            "name": [f"row-{i:05d}" for i in range(5000)],
            "value": [float(i) * 0.5 for i in range(5000)],
        }
    )
    url = f"s4://{BUCKET}/frames/written.parquet"
    df.to_parquet(url, storage_options=STORAGE_OPTIONS)
    got = pd.read_parquet(url, storage_options=STORAGE_OPTIONS)
    pd.testing.assert_frame_equal(got, df)


def test_gateway_serves_pandas_written_parquet_to_pyarrow(backend):
    """(e) the pandas-written object is a normal S4 object: the gateway
    decodes it and the served bytes parse as parquet with pyarrow."""
    pd = pytest.importorskip("pandas")
    pq = pytest.importorskip("pyarrow.parquet")
    key = "frames/written.parquet"
    # (Re)write deterministically so this test stands alone too.
    df = pd.DataFrame({"a": list(range(1000)), "b": [i * 2 for i in range(1000)]})
    df.to_parquet(f"s4://{BUCKET}/{key}", storage_options=STORAGE_OPTIONS)
    gw = _gateway()
    try:
        body = _boto(GW_PORT).get_object(Bucket=BUCKET, Key=key)["Body"].read()
    finally:
        gw.terminate()
        gw.wait()
    table = pq.read_table(io.BytesIO(body))
    pd.testing.assert_frame_equal(table.to_pandas(), df)


def test_overwrite_through_s4fs_keeps_gateway_consistent(backend):
    """Overwrite multi-frame -> single-frame: the stale sidecar is removed
    and the gateway serves the new body."""
    fs = _s4fs()
    key = "shrink.bin"
    fs.pipe_file(f"{BUCKET}/{key}", datagen.multi_frame_body())
    backend.head_object(Bucket=BUCKET, Key=f"{key}.s4index")
    fs.pipe_file(f"{BUCKET}/{key}", b"tiny now")
    with pytest.raises(Exception, match="404|Not Found"):
        backend.head_object(Bucket=BUCKET, Key=f"{key}.s4index")
    gw = _gateway()
    try:
        got = _boto(GW_PORT).get_object(Bucket=BUCKET, Key=key)["Body"].read()
        assert got == b"tiny now"
    finally:
        gw.terminate()
        gw.wait()
