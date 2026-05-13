# S4 Observability

S4 exposes a Prometheus text-format scrape endpoint at `GET /metrics` on
the same listener that serves the S3-compatible API (port-shared). The
exporter is installed once at startup from `s4_server::metrics::install()`
and the route is wired in `s4_server::routing::HealthRouter`. No
separate admin port is opened — this makes `/metrics` reachable behind
the same TLS / SigV4 gate the rest of the API uses.

## Metric reference

### Request-level (CPU + GPU pipelines, always on)

| name | type | labels | bumped on |
|------|------|--------|-----------|
| `s4_requests_total` | counter | `op` (`put`/`get`), `codec`, `result` (`ok`/`err`) | every PUT / GET |
| `s4_bytes_in_total` | counter | `op`, `codec` | bytes received from the client (PUT) / from the backend before decompress (GET) |
| `s4_bytes_out_total` | counter | `op`, `codec` | bytes forwarded to the backend (PUT) / returned to the client (GET) |
| `s4_request_latency_seconds` | histogram | `op`, `codec` | wall-clock per-request latency |

Compression ratio is derived in PromQL as
`s4_bytes_out_total / s4_bytes_in_total{op="put"}` — no separate ratio
metric is exported (cardinality-vs-value tradeoff already lost by other
S3 gateways that ship one).

### v0.8 #55 — GPU pipeline (only fires on nvCOMP / DietGPU paths)

| name | type | labels | bumped on |
|------|------|--------|-----------|
| `s4_gpu_compress_seconds` | histogram | `codec` | every GPU compress op (host→device + kernel + device→host) |
| `s4_gpu_decompress_seconds` | histogram | `codec` | every GPU decompress op |
| `s4_gpu_throughput_bytes_per_sec` | gauge | `codec`, `op` (`compress`/`decompress`) | last-sample throughput; compress = `bytes_in/secs`, decompress = `bytes_out/secs` (nvCOMP convention) |
| `s4_gpu_in_flight` | gauge | `codec` | inc'd when the GPU op is dispatched, dec'd on completion (success or failure) |
| `s4_gpu_oom_total` | counter | `codec` | bumped when the codec returns an out-of-memory-classified error (substring match against `cudaErrorMemoryAllocation` / `out of memory`) |

The GPU metrics are stamped from the s4-server side via the
`CodecRegistry::compress_with_telemetry` callback shape — `s4-codec`
itself stays free of a `metrics` dep so the codec crate's dep tree
remains slim (also lets the wasm / py bindings ship without the
prometheus exporter).

### Operational counters (subsystem-specific)

| name | type | labels | bumped on |
|------|------|--------|-----------|
| `s4_policy_denials_total` | counter | `action`, `bucket` | bucket policy `Deny` |
| `s4_tls_cert_reload_total` | counter | `result` | SIGHUP-driven cert reload |
| `s4_acme_renewal_total` | counter | `result` | ACME renewal cycle (success or failure) |
| `s4_acme_cert_expiry_seconds` | gauge | — | seconds until active ACME cert expires |
| `s4_rate_limit_throttled_total` | counter | `principal`, `bucket` | rate limiter rejection |
| `s4_compliance_mode_active` | gauge | `mode` | strict-mode marker (1.0 when on) |
| `s4_notifications_dropped_total` | counter | `dest` | event dropped after retry budget |
| `s4_lifecycle_actions_total` | counter | `bucket`, `action` | lifecycle expire / transition / noncurrent_expire |
| `s4_replication_dropped_total` | counter | `bucket` | cross-bucket replication PUT failed after retries |
| `s4_replication_replicated_total` | counter | `bucket`, `dest` | cross-bucket replication PUT succeeded |
| `s4_mfa_delete_denials_total` | counter | `bucket` | MFA-Delete gate refusal |

## Recommended Grafana layout (4-panel GPU dashboard)

The four panels below cover the v0.8 #55 GPU pipeline and read
naturally left-to-right as a single row (12-column grid, 6×3 each):

1. **GPU throughput (gauge → time-series, 6×3)**
   PromQL: `s4_gpu_throughput_bytes_per_sec`, plotted per `(codec, op)`.
   Y-axis unit: `bytes/sec`. Operators see at a glance whether the
   nvCOMP zstd path is hitting its expected ~4 GB/s on H100 / ~2 GB/s
   on RTX 4070 Ti.

2. **GPU compress p99 latency (histogram → percentile, 6×3)**
   PromQL:
   ```
   histogram_quantile(0.99, sum by (codec, le) (
     rate(s4_gpu_compress_seconds_bucket[5m])
   ))
   ```
   Y-axis unit: `seconds`. Pairs with throughput — a sudden p99 spike
   without a throughput drop usually indicates a single oversized
   payload, not a fleet-wide regression.

3. **GPU in-flight ops (gauge, 6×3)**
   PromQL: `s4_gpu_in_flight`, plotted per `codec`.
   Alert when this stays pinned at the configured concurrency cap
   (`--gpu-inflight`, default 4) for >5m — that's GPU saturation /
   queue head-of-line blocking. Use the threshold rule:
   ```
   max_over_time(s4_gpu_in_flight[5m]) >= <inflight_cap>
   ```

4. **GPU OOM rate (counter → rate, 6×3)**
   PromQL: `rate(s4_gpu_oom_total[5m])`, plotted per `codec`.
   Single-stat with a red-orange-green threshold (>0.01/s = page).
   Pair with the `s4_requests_total{result="err"}` counter on the
   request-level dashboard to attribute error spikes to GPU OOM
   versus generic backend / network failures.

A drop-in dashboard JSON is intentionally not shipped — the panel
PromQL above is verbatim what we use internally and is short enough
that operators can paste it into a fresh dashboard without us
maintaining a Grafana JSON in the s4 repo.

## Limitations (v0.8 #55 follow-ups)

- **DietGPU**: the `dietgpu.rs` codec wrapper is a Phase 2 stub (no
  FFI yet). Its `CodecKind::DietGpuAns` is recognised by the GPU /
  CPU classifier in `CodecRegistry`, so the moment the FFI lands
  the same `compress_with_telemetry` path will start stamping
  `s4_gpu_*` metrics for it without any further service.rs edit.
- **Per-chunk streaming GPU compress**: `streaming_compress_to_frames`
  in `s4-server::streaming` calls the non-telemetry
  `CodecRegistry::compress` per chunk. The per-chunk GPU stamp is a
  follow-up; the per-request stamp on the buffered path
  (NvcompBitcomp / NvcompGDeflate / non-streaming NvcompZstd) is in
  this PR.
- **Compressed-bytes-out split metric**: `bytes_out` is currently
  threaded through `CompressTelemetry` and consumed only by the
  throughput gauge math. A dedicated `s4_gpu_bytes_out_total{codec}`
  counter is reserved for follow-up (the call sites already pass the
  value, so the future PR is purely a `record_*` addition).
