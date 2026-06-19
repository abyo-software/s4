#!/bin/bash
set -e
cd "$(dirname "$0")"
ES=http://localhost:9200
MAP=$(cat mapping.json)
common='"number_of_shards":1,"number_of_replicas":0,"refresh_interval":"-1"'

mk() {
  local name="$1" extra="$2"
  curl -s -XDELETE "$ES/$name" >/dev/null
  local settings="{$common$extra}"
  local body="{\"settings\":$settings,\"mappings\":$MAP}"
  local r=$(curl -s -XPUT "$ES/$name" -H 'Content-Type: application/json' -d "$body")
  echo "$r" | python3 -c "import sys,json;d=json.load(sys.stdin);print('$name:', 'OK' if d.get('acknowledged') else d.get('error',{}).get('reason','ERR'))"
}

mk bench-standard-default  ""
mk bench-standard-bestcomp ',"index.codec":"best_compression"'
mk bench-logsdb            ',"index.mode":"logsdb"'
