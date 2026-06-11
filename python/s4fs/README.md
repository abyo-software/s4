# s4fs — fsspec filesystem for S4 objects (no gateway required)

`s4fs` lets pandas / pyarrow / DuckDB / Polars (anything fsspec-aware) read
[S4](https://github.com/abyo-software/s4) gateway-written objects **directly
from the S3 backend**. Objects are transparently decompressed on read,
`ls`/`info` report the original (decompressed) sizes, and range reads use the
`<key>.s4index` sidecar to fetch + decode only the frames that overlap the
requested range. Objects that never went through the gateway pass through
byte-for-byte. This is the lock-in escape hatch: if you stop running the
gateway, your data stays readable.

## Install

```bash
pip install -e python/s4fs[s3]    # from a source checkout
# requires the s4-codec wheel: cd crates/s4-codec-py && maturin build --release
```

## Use

```python
import pandas as pd
opts = {"target_options": {"endpoint_url": "http://backend:9000"}}
df = pd.read_parquet("s4://bucket/data.parquet", storage_options=opts)
```

```python
import fsspec, pyarrow.parquet as pq
fs = fsspec.filesystem("s4", target_options={"endpoint_url": "http://backend:9000"})
table = pq.read_table("bucket/data.parquet", filesystem=fs)
```

```python
import duckdb
con = duckdb.connect(); con.register_filesystem(fs)
con.sql("SELECT count(*) FROM read_parquet('s4://bucket/data.parquet')")
```

Any underlying fsspec filesystem can be injected instead of s3fs:
`S4FileSystem(fs=my_fs)` (used by the unit tests with an in-memory stub).

## Decoded formats

- S4F2-framed objects (single-PUT and multipart), S4P1 padding skipped
- codecs: `passthrough`, `cpu-zstd`, `cpu-gzip`, `cpu-zstd-dict`
  (dictionaries are fetched from `.s4dict/<id>` and fingerprint-verified)
- unframed gateway objects carrying a metadata manifest (`cpu-gzip`,
  legacy raw zstd, `passthrough`)
- `.s4index` sidecars v1/v2/v3 with ETag staleness checks (a stale sidecar
  falls back to a full-object read)

## Limitations

- **Read-only.** All write APIs raise `NotImplementedError` — write through
  the S4 gateway, which owns the framing / sidecar / metadata contract.
- **GPU frames are refused loudly.** `nvcomp-*` / `dietgpu-ans` frames raise
  `NotImplementedError` (decode them through the gateway); s4fs never
  returns silently-wrong bytes.
- **SSE-encrypted objects are refused loudly.** Reads raise
  `NotImplementedError` (the keyring / KMS / SSE-C key lives in the
  gateway — read encrypted objects through the gateway). Detection is
  threefold: the `s4-encrypted` object metadata stamp, the sidecar's v3
  SSE binding, and the `S4E1`–`S4E6` envelope magic in the body; s4fs
  never returns ciphertext as if it were data.
- Exact-size resolution in `ls`/`info` may cost one extra backend request
  per object (sidecar GET or metadata HEAD); results are cached per
  filesystem instance.
- Range reads on framed objects without a usable sidecar fall back to a
  full-object read (with a warning when the object is multi-frame).
  Legacy v1 sidecars (no source ETag/size binding) are treated as
  unusable — they cannot be tied to the live object.
- `open()` refuses framed objects whose original size is inexact (no
  usable sidecar, no `s4-original-size` metadata) instead of silently
  truncating buffered reads at the compressed size; opt back in with
  `S4FileSystem(allow_inexact_open=True)`. `cat_file()` is unaffected.

## Tests

```bash
pytest python/s4fs/tests                      # unit (gateway-captured fixtures)
pytest python/s4fs/tests/test_e2e_minio.py -m e2e   # docker + MinIO + real gateway
```
