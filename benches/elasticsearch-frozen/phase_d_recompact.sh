#!/bin/bash
# Phase D: the recommended high-compression path —
# snapshot through S4 at zstd-3 (fast), then `s4 recompact` the cold repo
# bucket to zstd-19 in the background (backend-direct, no connection timeout),
# then prove the snapshot still restores (recompaction is transparent to ES).
set -e
cd "$(dirname "$0")"
S4=../../target/release/s4
ES=http://localhost:9200
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
export AWS_REQUEST_CHECKSUM_CALCULATION=when_required AWS_RESPONSE_CHECKSUM_VALIDATION=when_required
EP=http://localhost:9000
IX=$1; BUCKET=repo-s4z3; REPO=repo_s4z3

bytes() { aws --endpoint-url $EP s3api list-objects-v2 --bucket "$1" --query "sum(Contents[].Size)" --output text 2>/dev/null; }

echo "### recompact path for index: $IX"
curl -s -XDELETE "$ES/_snapshot/$REPO" >/dev/null
aws --endpoint-url $EP s3 rm s3://$BUCKET --recursive >/dev/null 2>&1
curl -s -XPUT "$ES/_snapshot/$REPO" -H 'Content-Type: application/json' \
  -d '{"type":"s3","settings":{"bucket":"'$BUCKET'","client":"s4z3","max_snapshot_bytes_per_sec":"-1","max_restore_bytes_per_sec":"-1"}}' >/dev/null
echo "1) snapshot $IX through S4 zstd-3 ..."
curl -s -XPUT "$ES/_snapshot/$REPO/recomp?wait_for_completion=true" -H 'Content-Type: application/json' \
  -d '{"indices":"'$IX'","include_global_state":false}' | python3 -c "import sys,json;print('   state:',json.load(sys.stdin)['snapshot']['state'])"
B3=$(bytes $BUCKET); echo "   stored @ zstd-3 : $(python3 -c "print(f'{$B3/1e6:.1f} MB')")"

echo "2) s4 recompact -> zstd-19 (dry-run projection) ..."
$S4 recompact $BUCKET --endpoint-url $EP --target-zstd-level 19 2>&1 | grep -iE "would|projected|total|saved|rewritten|skip" | head -8 || true
echo "3) s4 recompact -> zstd-19 (execute) ..."
$S4 recompact $BUCKET --endpoint-url $EP --target-zstd-level 19 --execute 2>&1 | tail -6 || true
B19=$(bytes $BUCKET); echo "   stored @ zstd-19: $(python3 -c "print(f'{$B19/1e6:.1f} MB')")"

echo "4) verify snapshot still restores after recompaction ..."
curl -s -XDELETE "$ES/restore-recomp" >/dev/null
R=$(curl -s -XPOST "$ES/_snapshot/$REPO/recomp/_restore?wait_for_completion=true" -H 'Content-Type: application/json' \
  -d '{"indices":"'$IX'","rename_pattern":"'$IX'","rename_replacement":"restore-recomp","index_settings":{"index.number_of_replicas":0}}')
echo "$R" | python3 -c "import sys,json;d=json.load(sys.stdin);s=d.get('snapshot',{});print('   restore shards:',s.get('shards'))"
CNT=$(curl -s "$ES/restore-recomp/_count" | python3 -c "import sys,json;print(json.load(sys.stdin).get('count'))")
echo "   restored doc count: $CNT (post-zstd-19-recompact, read back through S4)"
curl -s -XDELETE "$ES/restore-recomp" >/dev/null

python3 -c "print(f'SUMMARY $IX: zstd-3={$B3/1e6:.1f}MB  zstd-19={$B19/1e6:.1f}MB  extra_saved={100*(1-$B19/$B3):.1f}% vs zstd-3')"
