"""Deterministic payload generators shared by the fixture-capture script
(`fixtures/generate_fixtures.py`), the unit tests, and the MinIO e2e test.

Keeping the generators here means the multi-megabyte *original* bodies never
need to be committed — only the (much smaller) gateway-compressed fixtures
are stored under ``fixtures/``.
"""

from __future__ import annotations

import io


def text_body() -> bytes:
    """~64 KiB of compressible log-like text (single S4F2 frame)."""
    return b"".join(
        b"%06d [info] checkout-api order_created amount_cents=%d currency=USD\n" % (i, 100 + i)
        for i in range(900)
    )


def multi_frame_body() -> bytes:
    """~5.1 MiB of position-unique but compressible text.

    Bodies in 1 MiB..100 MiB compress in 4 MiB chunks
    (``s4-server/src/streaming.rs::pick_chunk_size``), so this body spans
    **two** S4F2 frames — enough to exercise sidecar-driven partial reads.
    """
    pad = b"x" * 110
    return b"".join(b"%08d " % i + pad + b"\n" for i in range(42000))


def json_event(i: int) -> bytes:
    """One small JSON event, shaped like the dict-training corpus."""
    return (
        b'{"timestamp":"2026-06-10T12:%02d:%02dZ","level":"info",'
        b'"service":"checkout-api","event":"order_created",'
        b'"order_id":"ord_%08d","customer_id":"cus_%08d",'
        b'"amount_cents":%d,"currency":"USD","items":%d}'
        % (i % 60, (i * 7) % 60, i, i * 31, 100 + i * 13, i % 9)
    )


def parquet_body() -> bytes:
    """A small but multi-row-group parquet file (~tens of KiB)."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    table = pa.table(
        {
            "id": list(range(5000)),
            "name": [f"row-{i:05d}" for i in range(5000)],
            "value": [float(i) * 0.5 for i in range(5000)],
        }
    )
    buf = io.BytesIO()
    pq.write_table(table, buf, row_group_size=1000, compression="none")
    return buf.getvalue()
