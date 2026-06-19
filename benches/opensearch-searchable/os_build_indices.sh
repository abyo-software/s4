#!/bin/bash
# Reindex the identical doc set into best_compression / zstd / zstd_no_dict,
# then force-merge all four to 1 segment.
set -e
ES=http://localhost:9200
reindex() {
  echo "reindex -> $1 ..."
  curl -s -XPOST "$ES/_reindex?wait_for_completion=true&slices=auto&refresh=false" -H 'Content-Type: application/json' \
    -d "{\"source\":{\"index\":\"os-default\",\"size\":5000},\"dest\":{\"index\":\"$1\",\"op_type\":\"create\"}}" \
    | python3 -c "import sys,json;d=json.load(sys.stdin);print('  reindexed',d.get('total'),'failures:',len(d.get('failures',[])))"
}
reindex os-bestcomp
reindex os-zstd
reindex os-zstdnodict
echo "=== force-merge to 1 segment ==="
for ix in os-default os-bestcomp os-zstd os-zstdnodict; do
  curl -s -XPOST "$ES/$ix/_refresh" >/dev/null
  t0=$(date +%s); curl -s -XPOST "$ES/$ix/_forcemerge?max_num_segments=1" >/dev/null
  curl -s -XPOST "$ES/$ix/_refresh" >/dev/null
  echo "  $ix merged in $(( $(date +%s) - t0 ))s"
done
echo "=== on-disk primary sizes (OpenSearch index codecs) ==="
curl -s "$ES/_cat/indices/os-*?h=index,docs.count,pri.store.size&bytes=b&s=index"
