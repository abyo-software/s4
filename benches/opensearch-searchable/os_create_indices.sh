#!/bin/bash
# Create the four OpenSearch index-codec variants used by the benchmark.
set -e
cd "$(dirname "$0")"
ES=http://localhost:9200
MAP=$(cat os_mapping.json)
common='"number_of_shards":1,"number_of_replicas":0,"refresh_interval":"-1"'

mk() {
  local name="$1" extra="$2"
  curl -s -XDELETE "$ES/$name" >/dev/null
  local r=$(curl -s -XPUT "$ES/$name" -H 'Content-Type: application/json' \
    -d "{\"settings\":{$common$extra},\"mappings\":$MAP}")
  echo "$r" | python3 -c "import sys,json;d=json.load(sys.stdin);print('$name:', 'OK' if d.get('acknowledged') else d.get('error',{}).get('reason','ERR')[:120])"
}

mk os-default    ''
mk os-bestcomp   ',"index.codec":"best_compression"'
mk os-zstd       ',"index.codec":"zstd","index.codec.compression_level":3'
mk os-zstdnodict ',"index.codec":"zstd_no_dict","index.codec.compression_level":3'
