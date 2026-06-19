# Revision notes — Elasticsearch frozen-tier use case (2026-06-19)

Follow-up measurements + doc revision on top of the 2026-06-18 baseline run.
All measured locally against MinIO (no AWS billing). Host: AMD Ryzen 9 9950X,
ES 9.4.2, MinIO RELEASE.2025-02-28 (an isolated `esrev-` stack on ports
9305/9100/8021-8030, separate from the original 9200/9000 run), S4 v1.2.2,
4,000,000 ECS-style docs across standard / best_compression / LogsDB.

## What was measured (real numbers)

| ID | What | Result file | Status |
|----|------|-------------|--------|
| B1 | Cold latency under injected backend RTT (toxiproxy, 0/5/20/50 ms one-way, 4 query types, direct vs S4 zstd-3, cache cleared each run) | `rtt-injection.json` | **MEASURED** |
| B2 | `.s4index` sidecar cold-path overhead (backend GET ops per cold query, S4 zstd-3 vs passthrough baseline, counted from S4's structured op log) | `sidecar-overhead.json` | **MEASURED** |
| B3 | Break-even model — parameterised host $/mo × instances, HA(2) & non-HA(1), 500 TB & 1 PB net savings | `breakeven.json` (via `breakeven.py`) | **COMPUTED** (arithmetic on the measured `saved_ratio` + explicit host price) |
| B4 | HA failover smoke — 2 stateless S4 instances behind an nginx LB, kill one, check cold/warm query + snapshot PUT | `ha-failover.json` | **MEASURED — all 7 steps PASS** |
| B5 | recompact concurrency vs ES snapshot/_cleanup | `recompact-concurrency.json` | **documented-not-tested** (deliberate; see below) |

## Headline findings

- **B1 (RTT).** The cheap analytics queries (count / agg / full-text) are
  RTT-invariant: 2-4 ms cold and S4 within ±1 ms of direct at *every* injected
  RTT (they are answered without re-fetching from the repository). The heavy
  cold `top-N + sort` is **not** RTT-invariant: S4's relative overhead is +7.1%
  at 0 ms but grows to +9.2% / +32.9% / +69.8% at 5 / 20 / 50 ms one-way. This
  is an honest negative-for-the-hypothesis result and is reflected in the doc:
  the heavy raw-document fetch issues several `sidecar-partial` GETs, each of
  which costs a backend round-trip, so its penalty scales with backend latency.
  (toxiproxy latency injection independently confirmed: a HEAD took ~111 ms
  longer through the 50 ms-each-way proxy than direct.)
- **B2 (sidecar).** S4 issues the **same** number of backend GETs as the
  no-sidecar passthrough baseline on the cold top-N fetch (8 vs 8), and **zero**
  separate `.s4index`-keyed GETs (`cold_separate_s4index_gets_median = 0`). The
  sidecar is folded into each data GET as a `path="sidecar-partial"` covering
  range (5 of the 8 GETs), i.e. it makes each GET fetch *fewer* compressed bytes
  rather than adding a round-trip. The cheap analytics queries issue 0 backend
  GETs (ES answers them without a repo fetch); warm queries issue 0 in both
  arms. So S4 adds no extra cold-path backend round-trip per query here.
- **B3 (break-even).** At $70/mo/host and $23/TB-mo S3 Standard, break-even is
  ~11 TB (standard-default, 1 instance) / ~23 TB (HA 2 instances); ~14/27 TB for
  LogsDB; ~9/18 TB after recompact→zstd-19. At 500 TB / 1 PB every codec is
  comfortably net-positive even with 2 HA instances (e.g. standard-default HA:
  +$2,965/mo at 500 TB, +$6,070/mo at 1 PB). Re-run with your own
  `--s4-host-usd-month` / `--instances`.
- **B4 (HA).** With 2 stateless S4 instances behind nginx, killing one leaves
  cold search (232 ms, 79,925 hits via the survivor), warm search (3 ms from
  cache) and snapshot PUT (SUCCESS) all working. Confirms the A2 mitigation: a
  single gateway is a read-path SPOF, fixed by ≥2 stateless instances behind a
  load balancer / multi-value DNS (sidecars live in S3, so instances are
  interchangeable).
- **B5 (recompact concurrency).** Not run on purpose: running `s4 recompact`
  concurrently with an ES snapshot / `_cleanup` on the same repo is unsafe by
  the tool's own admission (HEAD→PUT TOCTOU, no S3 compare-and-swap — see
  `phase_d_recompact-evidence.txt`). A "test" would be a coin-flip or risk
  corrupting a repo, so it is documented from first principles with the
  exclusive-quiet-window recommendation instead.

## Honesty carried forward (not diluted)

The +6.5–9.5% cold-sort overhead, the `best_compression` double-compression
caveat, the zstd-19 slowloris `PARTIAL`-snapshot failure, and the
4,000,000-doc post-recompact round-trip verification all remain in the doc. B1
*adds* the further caveat that the cold-sort penalty grows under real backend
RTT.

## TODO / not covered this round

- `TODO(measure)`: B1 only swept the standard index; best_compression / LogsDB
  RTT curves not run (LogsDB's 7.2 s cold fetch makes the absolute numbers move
  but the relative-growth trend is expected to be the same shape).
- `TODO(measure)`: real-S3 (non-local) latency + egress $ are still modeled, not
  billed — the brief forbids AWS billing. The break-even model's host price is a
  parameter, not a measurement.
- The isolated `esrev-` reproduction's LogsDB on-disk size came out larger than
  the 2026-06-18 baseline (879 vs 741 MiB pre-snapshot) due to LogsDB
  codec/merge nondeterminism; the **authoritative storage/latency numbers in the
  doc remain the 2026-06-18 `phase_a/b/c/d` results**, which this round did not
  re-measure or overwrite.
