# OpenSearch searchable-snapshot benchmark harness

Reproduces the numbers in
[`docs/use-cases/opensearch-searchable-snapshots.md`](../../docs/use-cases/opensearch-searchable-snapshots.md):
storage cost and searchable-snapshot cold-search latency for an OpenSearch
searchable snapshot whose S3 repository is fronted by S4, across the four
`index.codec` variants (default / best_compression / zstd / zstd_no_dict).

Runs locally against MinIO — **no AWS account required**. Raw output from the
documented run is in [`results/os_full.json`](results/).

## Prerequisites
- Docker, `aws` CLI, `python3`
- A built S4 binary (`cargo build --release -p s4-server` → `target/release/s4`)
- aws-cli v2: export `AWS_REQUEST_CHECKSUM_CALCULATION=when_required` and
  `AWS_RESPONSE_CHECKSUM_VALIDATION=when_required` (the phase scripts do this).

## 1. Backend + buckets
```bash
docker run -d --name osbench-minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio:latest server /data
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
for b in os-repo-direct os-repo-s4z3 os-repo-s4z9; do
  aws --endpoint-url http://localhost:9000 s3 mb s3://$b; done
```

## 2. S4 gateways — **`--logical-etag` is required** for OpenSearch
Without it, OpenSearch's `repository-s3` (AWS SDK v2) rejects every blob with
`Data read has a different checksum than expected` (it validates uploads
against `MD5(original)`; S4 otherwise returns the compressed-bytes ETag).
```bash
S4=../../target/release/s4
$S4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=8011 \
    --codec=cpu-zstd --dispatcher=always --zstd-level=3 --logical-etag &
$S4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=8012 \
    --codec=cpu-zstd --dispatcher=always --zstd-level=9 --logical-etag &
```

## 3. OpenSearch (single node, searchable snapshots)
`repository-s3` is **not bundled** with OpenSearch — install it, and set a
`region` (its AWS SDK v2 requires one; the ES plugin does not).
```bash
docker run -d --name osbench-os --network host \
  -e OPENSEARCH_JAVA_OPTS="-Xms6g -Xmx6g" -e DISABLE_SECURITY_PLUGIN=true \
  -e "discovery.type=single-node" -e bootstrap.memory_lock=false \
  -e "node.roles=[cluster_manager,data,ingest,search,remote_cluster_client]" \
  -e "node.search.cache.size=4gb" \
  opensearchproject/opensearch:2
docker exec osbench-os bin/opensearch-plugin install --batch repository-s3
# add s3 client settings (endpoint/protocol/path_style_access/region) to
# config/opensearch.yml for clients `default` (:9000), `s4z3` (:8011),
# `s4z9` (:8012), then restart.
docker restart osbench-os

# repo-s3 credentials (keystore) + reload, then register repos:
for c in default s4z3 s4z9; do
  docker exec osbench-os sh -c "echo minioadmin | bin/opensearch-keystore add -x -f s3.client.$c.access_key"
  docker exec osbench-os sh -c "echo minioadmin | bin/opensearch-keystore add -x -f s3.client.$c.secret_key"
done
curl -XPOST localhost:9200/_nodes/reload_secure_settings
# register osr_direct/osr_s4z3/osr_s4z9 against the matching client+bucket.
```

## 4. Data + measurements
```bash
bash os_create_indices.sh                          # 4 index.codec variants
python3 gen_and_index.py os-default 4000000        # 4M docs into os-default
bash os_build_indices.sh                           # reindex into the other 3 + force-merge
python3 os_phase_full.py                           # cost + searchable cold search -> results/os_full.json
```

## Cleanup
```bash
docker rm -f osbench-os osbench-minio
pkill -f 'target/release/s4 .*--port=801'
```

## Notes / gotchas
- **`--logical-etag` is mandatory** (see §2) — this use case is what surfaced
  the S4 fix (shipped in `s4-server` via `--logical-etag`).
- OpenSearch's `index.codec` (zstd / best_compression) compresses only stored
  fields, so S4 still finds ~17% on a native-zstd index (doc-values, postings,
  term dicts).
- `node.search.cache.size` requires the `search` role; searchable snapshots
  mount via `_restore` with `storage_type: remote_snapshot`.
- Use `bench-`/`os-` index names — the built-in `logs-*-*` template forces data
  streams.
