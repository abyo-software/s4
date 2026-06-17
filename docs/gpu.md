# GPU compression (NVIDIA nvCOMP)

### Try with GPU compression (NVIDIA nvCOMP)

```bash
# Requires NVIDIA Container Toolkit + a CUDA-capable GPU
docker compose -f docker-compose.gpu.yml up -d
aws --endpoint-url http://localhost:8014 s3 cp parquet-file.parq s3://demo/
```

See [docker-compose.gpu.yml](../docker-compose.gpu.yml) for details.

### GPU small-PUT batching (`--gpu-batch-small-puts`, opt-in)

Per-object GPU compression below `--gpu-min-bytes` (default 1 MiB) loses to
CPU because each call pays a fixed kernel-launch + PCIe round-trip.
`--gpu-batch-small-puts` (v1.1, off by default; requires the `nvcomp-gpu`
build + a CUDA GPU at boot, refuses to start otherwise) coalesces
**concurrent** small PUTs into a single `nvcompBatchedZstd` kernel launch:

```bash
s4 --endpoint-url ... --gpu-batch-small-puts \
   --gpu-batch-max-items 32 \      # flush at 32 pending bodies (default)
   --gpu-batch-window-ms 4 \       # ...or after 4 ms, whichever first (default)
   --gpu-batch-floor-bytes 4096    # bodies below 4 KiB stay on cpu-zstd (default)
```

Eligibility: dispatcher picked `cpu-zstd`, no `--zstd-dict` match, declared
`Content-Length` in `[--gpu-batch-floor-bytes, --gpu-min-bytes)`. Stored
objects are **standard `nvcomp-zstd` bodies — wire-format identical to the
per-object GPU path**; the GET path has zero batch awareness. Any decline
(queue full, GPU error, batched output not smaller than the input) falls
back to the unchanged cpu-zstd path; watch the split via the
`s4_gpu_batch_total{result="batched"|"fallback"}` counter.

Trade-offs, measured on 1000 × 8 KiB log-like objects (RTX 4070 Ti SUPER +
Ryzen 9 9950X, nvCOMP 5.2.0.10, `cargo bench -p s4-codec --features
nvcomp-gpu --bench gpu_small_batch`, 2026-06-11):

| Path | Wall time | Objects/s | Total compressed |
|---|---:|---:|---:|
| cpu-zstd-3, sequential | 15.7–19.5 ms | ~52–64k | 735,396 B (11.14×) |
| nvcomp-zstd per-object | 702–707 ms | ~1.4k | 665,375 B (12.31×) |
| nvcomp-zstd **batched (32/launch)** | 29.7–29.9 ms | ~33.5k | 665,375 B (12.31×) |

Honest read: batching makes small-object GPU compression **~24× faster
than per-object GPU** and yields ~10% smaller output than cpu-zstd-3, but
a single CPU core still finishes this 8 KiB workload ~1.5–1.9× sooner in
wall time on this hardware. Enable the flag when (a) ingest CPU is the
bottleneck and you want to offload small-object compression to an
otherwise-idle GPU, or (b) the extra compression ratio matters at fleet
scale; skip it if raw single-node PUT latency is what you optimise — each
batched PUT also waits up to `--gpu-batch-window-ms` for its batch to fill.
