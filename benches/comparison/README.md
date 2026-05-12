# Compression-ratio comparison bench (v0.3 #14)

Reproducible head-to-head between S4 and the obvious S3-compatible
alternatives. Output: a single CSV per run that the README
"How it Compares" section cites.

## What it measures

For each (workload, system) cell:

- **`stored_bytes`**: bytes the backend actually persists — this is what
  shows up on the AWS bill. Captured via `aws s3api head-object` on the
  underlying storage layer (so S4 is measured at MinIO, after S4 has
  squished the data on its way through).
- **`ratio`**: `original_bytes / stored_bytes`.
- **`put_secs` / `get_secs`**: end-to-end wall-clock for the upload /
  download round-trip via `aws s3 cp`.

## Systems covered

| System | Codec | Notes |
|---|---|---|
| `garage` | zstd level 6 | Garage v1.0.1, single-replica |
| `minio` | server-side text compression | MinIO 2024-09-13 with `MINIO_COMPRESSION_ENABLE=on` (text/json/log only) |
| `s4-cpu` | `cpu-zstd-3` | S4 Dockerfile, points at MinIO backend |
| `s4-gpu` | `nvcomp-zstd` | S4 Dockerfile.gpu, requires `--gpus all` on host |

## Workloads

| ID | Description | Default size |
|---|---|---|
| `nginx-log` | Realistic nginx access log lines (text-heavy, highly compressible) | 64 MiB |
| `parquet-like` | Mixed numeric column + text metadata (~2× compressible) | 64 MiB |
| `random-bytes` | Pseudo-random binary (incompressible — sanity floor) | 16 MiB |

Override sizes via env: `SIZE_TEXT=$((1024*1024*1024))` for 1 GiB
text/parquet workloads; `SIZE_RAND=$((128*1024*1024))` for bigger random.

## Running

```bash
docker compose -f benches/comparison/docker-compose.yml up -d
./benches/comparison/run.sh
docker compose -f benches/comparison/docker-compose.yml down -v
```

The script auto-skips backends that aren't reachable (e.g. `s4-gpu`
without `--gpus all` on the docker host). Output:
`benches/comparison/bench-result.csv` (override path with first arg).

## Out of scope (deferred)

- **Silesia / Calgary corpus**: a follow-up issue should download these
  via build.rs, but for the v0.3 release the synthetic workloads are
  enough to make the relative comparison.
- **Real Parquet files**: the synthetic `parquet-like` workload mimics
  the byte-level patterns of a column-store but isn't a real file. A
  follow-up could fetch a sample TPCH or NYC taxi parquet.
- **`peak_rss_mb`**: column reserved in the CSV; populating it requires
  per-process `getrusage` plumbing (out of scope for shell driver).

See [issue #14](https://github.com/abyo-software/s4/issues/14) for the
full ambition; this directory is the v0.3 MVP delivery.
