#!/bin/bash
# Reindex the identical doc set into best_compression + logsdb, then
# force-merge all three to 1 segment (the realistic pre-frozen state).
set -e
ES=http://localhost:9200
cd "$(dirname "$0")"

reindex() {
  local dest="$1"
  echo "reindex -> $dest ..."
  curl -s -XPOST "$ES/_reindex?wait_for_completion=true&slices=auto&refresh=false" \
    -H 'Content-Type: application/json' \
    -d "{\"source\":{\"index\":\"bench-standard-default\",\"size\":5000},\"dest\":{\"index\":\"$dest\",\"op_type\":\"create\"}}" \
    | python3 -c "import sys,json;d=json.load(sys.stdin);print('  reindexed', d.get('total'), 'took', d.get('took'),'ms', 'failures:', len(d.get('failures',[])))"
}

reindex bench-standard-bestcomp
reindex bench-logsdb

echo "=== refresh + force-merge to 1 segment (this is the slow part) ==="
for ix in bench-standard-default bench-standard-bestcomp bench-logsdb; do
  curl -s -XPOST "$ES/$ix/_refresh" >/dev/null
  echo "force-merge $ix ..."
  t0=$(date +%s)
  curl -s -XPOST "$ES/$ix/_forcemerge?max_num_segments=1" >/dev/null
  curl -s -XPOST "$ES/$ix/_refresh" >/dev/null
  echo "  $ix merged in $(( $(date +%s) - t0 ))s"
done

echo "=== on-disk primary sizes ==="
curl -s "$ES/_cat/indices/bench-*?h=index,docs.count,pri.store.size,store.size&bytes=b&s=index"
