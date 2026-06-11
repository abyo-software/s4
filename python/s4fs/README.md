# s4fs — fsspec filesystem for S4 objects (no gateway required)

`s4fs` lets pandas / pyarrow / DuckDB / Polars (anything fsspec-aware) read
[S4](https://github.com/abyo-software/s4) gateway-written objects **directly
from the S3 backend**. Objects are transparently decompressed on read,
`ls`/`info` report the original (decompressed) sizes, and range reads use the
`<key>.s4index` sidecar to fetch + decode only the frames that overlap the
requested range. Objects that never went through the gateway pass through
byte-for-byte. This is the lock-in escape hatch: if you stop running the
gateway, your data stays readable.

Writes are supported too (opt-in, `write_enabled=True`): s4fs encodes the
body in the exact format the gateway's single-PUT path produces — S4F2
frames with the gateway's chunk-size policy, the five manifest metadata
keys, and an ETag-bound `.s4index` sidecar for multi-frame bodies — so
gateway GET / Range GET, `s4 verify-sidecar` and s4fs itself all read the
result back.

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

## Write (opt-in)

```python
import pandas as pd
opts = {
    "write_enabled": True,   # writes are refused without this
    "target_options": {"endpoint_url": "http://backend:9000"},
}
df.to_parquet("s4://bucket/data.parquet", storage_options=opts)
df2 = pd.read_parquet("s4://bucket/data.parquet", storage_options=opts)
```

```python
import fsspec
fs = fsspec.filesystem(
    "s4", write_enabled=True, target_options={"endpoint_url": "http://backend:9000"}
)
fs.pipe_file("bucket/key.bin", b"payload")          # or fs.open(..., "wb")
fs.put_file("local.csv", "bucket/data.csv")
```

What a write does (mirrors the gateway single-PUT path, byte-compatible):

1. compress into S4F2 frames (`cpu-zstd` level 3 by default; chunk size
   follows the gateway policy — 1 MiB for bodies ≤ 1 MiB, 4 MiB up to
   100 MiB, 16 MiB above) or store raw with `write_codec="passthrough"`;
2. PUT the body **with** the gateway's manifest metadata (`s4-codec`,
   `s4-original-size`, `s4-compressed-size`, `s4-crc32c`, `s4-framed`);
3. for multi-frame bodies, PUT a `<key>.s4index` sidecar bound to the
   body's backend ETag + size (the binding gateway Range GET and
   `s4 verify-sidecar` check).

Write constraints:

- **Opt-in.** Without `write_enabled=True` every write API raises
  `NotImplementedError` (the pre-1.2 read-only contract).
- **Metadata-capable underlying fs required.** The manifest metadata stamp
  is what makes the gateway decode the object; a framed body without it
  would be served as raw compressed bytes. s3fs works out of the box;
  other filesystems are refused with `S4MetadataUnsupportedError` unless
  they declare a `s4fs_metadata_pipe_kwarg` attribute naming the
  `pipe_file()` keyword that accepts a `{str: str}` metadata dict.
- **Create / overwrite only.** Append (`mode="ab"`) raises
  `NotImplementedError`. `open(path, "wb")` buffers the whole object in
  memory and uploads on close.
- **Codecs: `cpu-zstd` (default) and `passthrough`.** SSE encryption,
  zstd-dictionary compression (`cpu-zstd-dict`), `cpu-gzip` and GPU codecs
  raise `NotImplementedError` — write through the gateway for those.
- **No gateway versioning.** A direct backend overwrite does not advance a
  gateway-side version chain; if you rely on `--versioning`, write through
  the gateway.

## Decoded formats

- S4F2-framed objects (single-PUT and multipart), S4P1 padding skipped
- codecs: `passthrough`, `cpu-zstd`, `cpu-gzip`, `cpu-zstd-dict`
  (dictionaries are fetched from `.s4dict/<id>` and fingerprint-verified)
- unframed gateway objects carrying a metadata manifest (`cpu-gzip`,
  legacy raw zstd, `passthrough`)
- `.s4index` sidecars v1/v2/v3 with ETag staleness checks (a stale sidecar
  falls back to a full-object read)

## Limitations

- **Writes are opt-in and scoped.** Default is read-only; see
  [Write (opt-in)](#write-opt-in) above for what `write_enabled=True`
  supports and refuses. Copy / move / delete stay unsupported (the
  gateway owns reserved-metadata propagation and sidecar/version
  cleanup).
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
pytest python/s4fs/tests/test_e2e_minio.py -m e2e        # read e2e: docker + MinIO + real gateway
pytest python/s4fs/tests/test_e2e_s4fs_write.py -m e2e   # write e2e: gateway GET / verify-sidecar / Range GET
```
