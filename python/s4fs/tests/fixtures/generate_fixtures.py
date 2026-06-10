#!/usr/bin/env python
"""Capture real gateway-written backend bytes as unit-test fixtures.

The s4fs unit tests must decode **exactly what the S4 gateway writes** —
hand-assembled wire bytes would risk testing a private re-implementation of
the format instead of the real thing. This script:

1. starts MinIO in docker (port 19100) and creates a bucket;
2. runs the real ``s4`` binary (gateway mode / ``train-dict``) against it;
3. PUTs deterministic payloads (see ``tests/datagen.py``) *through the
   gateway* (and one raw object directly to the backend);
4. captures, for each object, the raw backend bytes (``<name>.body``), the
   ``<key>.s4index`` sidecar when present (``<name>.s4index``), the user
   metadata (``<name>.meta.json``), and — for small payloads — the original
   plaintext (``<name>.orig``; the larger originals are regenerated from
   ``datagen.py`` instead of being committed);
5. tears everything down.

Usage (requires docker + a built ``s4`` binary):

    cargo build -p s4-server               # or --release
    /path/to/venv/bin/python generate_fixtures.py [--s4-bin ../target/debug/s4]

Captured objects:

- ``text_zstd``    — ~64 KiB text, ``--codec cpu-zstd`` (single frame)
- ``multi_zstd``   — ~5.1 MiB text, ``--codec cpu-zstd`` (two frames)
- ``parquet_zstd`` — small parquet file, ``--codec cpu-zstd``
- ``text_gzip``    — ~64 KiB text, ``--codec cpu-gzip``
- ``dict_event``   — small JSON event compressed with ``cpu-zstd-dict``
  (+ ``zstd_dict.bin`` = the ``.s4dict/<id>`` dictionary object)
- ``raw``          — PUT directly to the backend, never framed
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import time

import boto3
from botocore.config import Config

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import datagen  # noqa: E402

FIXTURE_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = FIXTURE_DIR.parents[3]

MINIO_PORT = 19100
GW_PORT = 19101
BUCKET = "s4fs-fixtures"
CREDS = dict(
    aws_access_key_id="minioadmin",
    aws_secret_access_key="minioadmin",
    region_name="us-east-1",
)
MINIO_CONTAINER = "s4fs-fixture-minio"


def client(port: int):
    return boto3.client(
        "s3",
        endpoint_url=f"http://127.0.0.1:{port}",
        config=Config(s3={"addressing_style": "path"}),
        **CREDS,
    )


def wait_port(port: int, timeout: float = 30.0) -> None:
    import socket

    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket() as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.3)
    raise RuntimeError(f"port {port} did not open within {timeout}s")


def start_gateway(s4_bin: str, extra_args: list[str]) -> subprocess.Popen:
    env = os.environ | {
        "AWS_ACCESS_KEY_ID": "minioadmin",
        "AWS_SECRET_ACCESS_KEY": "minioadmin",
        "AWS_REGION": "us-east-1",
    }
    proc = subprocess.Popen(
        [
            s4_bin,
            "--host",
            "127.0.0.1",
            "--port",
            str(GW_PORT),
            "--endpoint-url",
            f"http://127.0.0.1:{MINIO_PORT}",
            *extra_args,
        ],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    wait_port(GW_PORT)
    time.sleep(0.5)
    return proc


def capture(backend, key: str, name: str, orig: bytes | None) -> None:
    body = backend.get_object(Bucket=BUCKET, Key=key)["Body"].read()
    (FIXTURE_DIR / f"{name}.body").write_bytes(body)
    meta = backend.head_object(Bucket=BUCKET, Key=key).get("Metadata", {})
    (FIXTURE_DIR / f"{name}.meta.json").write_text(json.dumps(meta, indent=2, sort_keys=True))
    try:
        sidecar = backend.get_object(Bucket=BUCKET, Key=key + ".s4index")["Body"].read()
        (FIXTURE_DIR / f"{name}.s4index").write_bytes(sidecar)
    except backend.exceptions.NoSuchKey:
        pass
    if orig is not None:
        (FIXTURE_DIR / f"{name}.orig").write_bytes(orig)
    print(f"captured {name}: body={len(body)}B meta={meta}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--s4-bin", default=str(REPO_ROOT / "target" / "debug" / "s4"))
    args = ap.parse_args()

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
    )
    try:
        wait_port(MINIO_PORT)
        time.sleep(1.0)
        backend = client(MINIO_PORT)
        backend.create_bucket(Bucket=BUCKET)

        # -- raw object straight to the backend (never framed) ------------
        raw = b"plain bytes, never touched by the gateway\n" * 8
        backend.put_object(Bucket=BUCKET, Key="raw.bin", Body=raw)

        # -- dict-training corpus straight to the backend ------------------
        for i in range(200):
            backend.put_object(
                Bucket=BUCKET, Key=f"events/sample-{i:04d}.json", Body=datagen.json_event(i)
            )

        # -- cpu-zstd gateway ----------------------------------------------
        gw = start_gateway(args.s4_bin, ["--codec", "cpu-zstd"])
        try:
            gwc = client(GW_PORT)
            gwc.put_object(Bucket=BUCKET, Key="text.txt", Body=datagen.text_body())
            gwc.put_object(Bucket=BUCKET, Key="multi.bin", Body=datagen.multi_frame_body())
            gwc.put_object(Bucket=BUCKET, Key="data.parquet", Body=datagen.parquet_body())
        finally:
            gw.terminate()
            gw.wait()

        # -- cpu-gzip gateway ----------------------------------------------
        gw = start_gateway(args.s4_bin, ["--codec", "cpu-gzip"])
        try:
            gwc = client(GW_PORT)
            gwc.put_object(Bucket=BUCKET, Key="text-gzip.txt", Body=datagen.text_body())
        finally:
            gw.terminate()
            gw.wait()

        # -- train a dictionary, then a --zstd-dict gateway ------------------
        env = os.environ | {
            "AWS_ACCESS_KEY_ID": "minioadmin",
            "AWS_SECRET_ACCESS_KEY": "minioadmin",
            "AWS_REGION": "us-east-1",
        }
        out = subprocess.run(
            [
                args.s4_bin,
                "train-dict",
                f"{BUCKET}/events/",
                "--endpoint-url",
                f"http://127.0.0.1:{MINIO_PORT}",
            ],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        # stdout names `.s4dict/<id>`; extract the 16-hex id.
        import re

        m = re.search(r"\.s4dict/([0-9a-f]{16})", out.stdout + out.stderr)
        if not m:
            raise RuntimeError(f"train-dict output had no dict id:\n{out.stdout}\n{out.stderr}")
        dict_id = m.group(1)
        print(f"trained dict id: {dict_id}")

        gw = start_gateway(
            args.s4_bin,
            ["--codec", "cpu-zstd", "--zstd-dict", f"{BUCKET}/events/={dict_id}"],
        )
        try:
            gwc = client(GW_PORT)
            gwc.put_object(Bucket=BUCKET, Key="events/new.json", Body=datagen.json_event(7777))
        finally:
            gw.terminate()
            gw.wait()

        # -- capture ----------------------------------------------------------
        capture(backend, "text.txt", "text_zstd", datagen.text_body())
        capture(backend, "multi.bin", "multi_zstd", None)  # orig regenerated
        capture(backend, "data.parquet", "parquet_zstd", datagen.parquet_body())
        capture(backend, "text-gzip.txt", "text_gzip", datagen.text_body())
        capture(backend, "events/new.json", "dict_event", datagen.json_event(7777))
        capture(backend, "raw.bin", "raw", raw)
        dict_bytes = backend.get_object(Bucket=BUCKET, Key=f".s4dict/{dict_id}")["Body"].read()
        (FIXTURE_DIR / "zstd_dict.bin").write_bytes(dict_bytes)
        print(f"captured zstd_dict.bin ({len(dict_bytes)}B, id={dict_id})")
    finally:
        subprocess.run(["docker", "rm", "-f", MINIO_CONTAINER], capture_output=True)


if __name__ == "__main__":
    main()
