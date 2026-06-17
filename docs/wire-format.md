# On-the-wire format

S4 stores data as either:

### Single PUT (framed, `S4F2` magic, since v0.2 #4)
S3 metadata holds the manifest:

```
x-amz-meta-s4-codec:           passthrough | cpu-zstd | nvcomp-zstd | ...
x-amz-meta-s4-original-size:   <decoded bytes>
x-amz-meta-s4-compressed-size: <stored bytes, includes S4F2 framing>
x-amz-meta-s4-crc32c:          <CRC32C of original bytes>
```

Since v0.2 #4 the body is the same `S4F2` framed format multipart uploads
use (one frame per `DEFAULT_S4F2_CHUNK_SIZE` = 4 MiB chunk). Small objects
(< 4 MiB) produce a single S4F2 frame and pay a constant **+28 byte** wire
overhead vs the raw compressed bytes — see footnote [^wire-overhead].

### Multipart (framed, `S4F2` magic, per-part compression)

```
x-amz-meta-s4-multipart: true
x-amz-meta-s4-codec:     <default codec for the object>
```

Object body is a sequence of:

```
┌──────────── 28-byte frame header ────────────┐
│ "S4F2" │ codec_id u32 │ orig u64 │ comp u64  │ crc32c u32 │  payload (comp bytes)
└────────────────────────────────────────────────┘

(optional) ┌──── padding ────┐
           │ "S4P1" │ len u64 │ <len zero bytes>
           └─────────────────┘
```

A sidecar object `<key>.s4index` (binary, `S4IX` magic) maps decompressed
byte ranges to compressed byte offsets — used by Range GET to fetch only the
needed bytes from S3.

[^wire-overhead]: Per v0.4 #18 micro-bench
    (`../crates/s4-server/examples/bench_framed_overhead.rs`, cpu-zstd codec,
    partially-compressible synthetic input, single-frame payloads):

    | size | raw_compressed | framed | overhead_bytes | overhead_pct |
    |---|---:|---:|---:|---:|
    | 1 KiB | 121 B | 149 B | +28 B | 23.14% |
    | 100 KiB | 12 040 B | 12 068 B | +28 B | 0.23% |
    | 1 MiB | 102 811 B | 102 839 B | +28 B | 0.03% |

    Overhead is a flat 28 bytes (= `FRAME_HEADER_BYTES`: `"S4F2"` magic u32 +
    codec_id u32 + original_size u64 + compressed_size u64 + crc32c u32) per
    single-frame object, independent of payload size; the percentage shrinks
    quickly as objects grow. Reproduce with
    `cargo run --release --example bench_framed_overhead -p s4-server`.
