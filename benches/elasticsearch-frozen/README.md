# Elasticsearch frozen-tier benchmark harness

Reproduces the numbers in
[`docs/use-cases/elasticsearch-frozen-tier.md`](../../docs/use-cases/elasticsearch-frozen-tier.md):
storage cost, snapshot/restore throughput, and **cold frozen-search latency**
for an Elasticsearch frozen tier whose S3 snapshot repository is fronted by S4,
across standard / `best_compression` / LogsDB index modes and zstd levels.

Everything runs locally against MinIO — **no AWS account or billing required**.
Raw outputs from the documented run are in [`results/`](results/).

## Prerequisites

- Docker, `aws` CLI, `python3`
- A built S4 binary (`cargo build --release -p s4-server` → `target/release/s4`)
- ~10 GB free disk, ~8 GB free RAM

> aws-cli v2 sends checksums some S3-compatible servers reject; export
> `AWS_REQUEST_CHECKSUM_CALCULATION=when_required` and
> `AWS_RESPONSE_CHECKSUM_VALIDATION=when_required` (the phase scripts already do).

## 1. Backend + buckets

```bash
docker run -d --name esfrozen-minio -p 9000:9000 -p 9001:9001 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio:latest server /data --console-address ":9001"

export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
for b in repo-direct repo-s4z3 repo-s4z9 repo-s4z19; do
  aws --endpoint-url http://localhost:9000 s3 mb s3://$b
done
```

## 2. S4 gateways (one per zstd level)

```bash
S4=../../target/release/s4
for pair in 8011:3 8012:9 8013:19; do
  port=${pair%:*}; lvl=${pair#*:}
  $S4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=$port \
      --codec=cpu-zstd --dispatcher=always --zstd-level=$lvl &
done
```

## 3. Elasticsearch (single node, frozen tier)

```bash
docker run -d --name esfrozen-es --network host \
  -e ES_JAVA_OPTS="-Xms6g -Xmx6g" \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e "xpack.searchable.snapshot.shared_cache.size=4gb" \
  -e "s3.client.default.endpoint=localhost:9000" -e "s3.client.default.protocol=http" -e "s3.client.default.path_style_access=true" \
  -e "s3.client.s4z3.endpoint=localhost:8011"  -e "s3.client.s4z3.protocol=http"  -e "s3.client.s4z3.path_style_access=true" \
  -e "s3.client.s4z9.endpoint=localhost:8012"  -e "s3.client.s4z9.protocol=http"  -e "s3.client.s4z9.path_style_access=true" \
  -e "s3.client.s4z19.endpoint=localhost:8013" -e "s3.client.s4z19.protocol=http" -e "s3.client.s4z19.path_style_access=true" \
  docker.elastic.co/elasticsearch/elasticsearch:9.4.2

# repo-s3 credentials (keystore) + reload + frozen-tier trial license
for c in default s4z3 s4z9 s4z19; do
  docker exec esfrozen-es sh -c "echo minioadmin | bin/elasticsearch-keystore add -x -f s3.client.$c.access_key"
  docker exec esfrozen-es sh -c "echo minioadmin | bin/elasticsearch-keystore add -x -f s3.client.$c.secret_key"
done
curl -XPOST localhost:9200/_nodes/reload_secure_settings
curl -XPOST "localhost:9200/_license/start_trial?acknowledge=true"

# register one repository per endpoint, and run ES _verify (write/read/delete
# probe through S4 — the first real end-to-end compatibility check)
reg() {
  curl -s -XPUT "localhost:9200/_snapshot/$1" -H 'Content-Type: application/json' \
    -d "{\"type\":\"s3\",\"settings\":{\"bucket\":\"$3\",\"client\":\"$2\",\"max_snapshot_bytes_per_sec\":\"-1\",\"max_restore_bytes_per_sec\":\"-1\"}}" >/dev/null
  echo -n "$1 verify: "; curl -s -XPOST "localhost:9200/_snapshot/$1/_verify" | grep -q '"nodes"' && echo OK || echo FAIL
}
reg repo_direct default repo-direct; reg repo_s4z3 s4z3 repo-s4z3
reg repo_s4z9 s4z9 repo-s4z9; reg repo_s4z19 s4z19 repo-s4z19
```

## 4. Data + measurements

```bash
bash create_indices.sh                              # 3 index modes
python3 gen_and_index.py bench-standard-default 4000000   # 4M docs
bash build_indices.sh                               # reindex into the other 2 + force-merge

python3 phase_a_snapshots.py    # storage cost + snapshot throughput  -> results/phase_a.json
python3 phase_c_restore.py      # restore throughput                  -> results/phase_c.json
python3 phase_b_frozen.py       # cold frozen-search latency          -> results/phase_b.json
bash phase_d_recompact.sh bench-standard-default   # snapshot zstd-3 -> s4 recompact zstd-19 -> verify restore
```

## 5. Revision phases (2026-06-19) — RTT / sidecar / HA / break-even

These are **non-destructive additions** layered on top of A–D; they don't change
the A–D outputs. All are env-parameterised (defaults reproduce the canonical
`localhost:9200`/`:9000` stack; override `ES_URL` / `MINIO_URL` etc. for an
isolated stack). Raw outputs + a one-page summary land in `results/`.

```bash
# B3 break-even model (pure arithmetic on the measured saved_ratio; no infra)
python3 breakeven.py --s4-host-usd-month 70 --instances 2 --out results/breakeven.json

# B1 cold latency under injected backend RTT (needs a toxiproxy in front of the
# object store + a dedicated S4 instance upstream of it + ES clients tdirect/ts4z3;
# the .sh prints the exact prereqs). Per-connection latency proxy is used on
# purpose — NOT a global `tc netem` (that would perturb co-tenant processes).
bash phase_b1_rtt.sh                         # -> results/rtt-injection.json

# B2 .s4index sidecar cold-path overhead (backend GETs per cold query, S4 vs a
# passthrough-codec baseline; counts ops from S4's structured op log — run the
# S4 instances with structured logging on stdout).
python3 phase_b2_sidecar.py                  # -> results/sidecar-overhead.json

# B4 HA failover smoke (starts 2 stateless S4 instances + an nginx round-robin
# upstream, registers a repo through the LB, kills one instance).
bash phase_b4_ha.sh                          # -> results/ha-failover.json

# B5 recompact concurrency: documented-not-tested (running recompact concurrently
# with ES snapshot/_cleanup on the same repo is unsafe by the tool's own TOCTOU
# admission — see results/recompact-concurrency.json; no script, by design).
```

> **nginx + SigV4:** the B4 LB must preserve the client Host header
> (`proxy_set_header Host $http_host`) — AWS SigV4 signs Host, so rewriting it to
> the upstream name returns 403 SignatureDoesNotMatch. `phase_b4_ha.sh` does this.

See `results/REVISION-NOTES.md` for what each phase found and what's still TODO.

## Cleanup

```bash
docker rm -f esfrozen-es esfrozen-minio esfrozen-toxiproxy esfrozen-nginx
pkill -f 'target/release/s4 .*--port=80'
```

## Notes / gotchas (encountered building this)

- The built-in `logs-*-*` index template forces data streams; the bench uses
  `bench-*` index names to avoid it.
- Frozen tier (`storage: shared_cache`) needs an **Enterprise/trial license**
  and a non-zero `xpack.searchable.snapshot.shared_cache.size` (defaults to 0
  on a mixed-role node).
- Two *different* throttles dominate wall-clock unless lifted:
  **snapshot** is capped by the repo setting `max_snapshot_bytes_per_sec`
  (default `40mb`); **restore** is capped by the cluster/node recovery setting
  `indices.recovery.max_bytes_per_sec` (default `40mb`) — *not* by the repo's
  `max_restore_bytes_per_sec`, which defaults to unlimited. To see S4's real
  throughput, set the repo `max_snapshot_bytes_per_sec: -1` **and**
  `PUT _cluster/settings {"transient":{"indices.recovery.max_bytes_per_sec":"-1"}}`
  (the latter is what `phase_c_restore.py` does).
- Driving a snapshot through an S4 gateway pinned to `zstd-19` trips S4's 30s
  `--read-timeout-seconds` slowloris guard on large parts → `PARTIAL` snapshot.
  Use `s4 recompact` for high levels (phase D), or raise the timeout.
