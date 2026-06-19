#!/bin/bash
# Phase B1 runner: cold frozen-search latency under injected backend RTT.
#
# Stands up a toxiproxy in front of the object store and a dedicated S4 zstd-3
# instance whose upstream is the toxiproxy, then sweeps a one-way delay and runs
# the 4 cold query types direct-vs-S4 (both traversing the same RTT). It shows
# S4's *relative* overhead stays ~RTT-invariant -> results/rtt-injection.json.
#
# Per-connection latency proxy (toxiproxy) is used ON PURPOSE instead of a global
# `tc qdisc/netem`: netem on loopback/the docker bridge would perturb every other
# process on the host and usually needs privileges. toxiproxy delays only this
# proxy's connections.
#
# Override the canonical defaults for an isolated stack, e.g.:
#   ES_URL=http://localhost:9305 MINIO_URL=http://localhost:9100 \
#   TOXI_URL=http://localhost:8474 ./phase_b1_rtt.sh
set -e
cd "$(dirname "$0")"

: "${ES_URL:=http://localhost:9200}"
: "${MINIO_URL:=http://localhost:9100}"
: "${TOXI_URL:=http://localhost:8474}"
: "${TOXI_PROXY:=minio}"
: "${RTT_MS:=0,5,20,50}"

echo "B1: ES=$ES_URL MINIO=$MINIO_URL TOXI=$TOXI_URL proxy=$TOXI_PROXY rtt=$RTT_MS"
echo "Prereqs (the orchestrator wires these for the esrev stack):"
echo "  - toxiproxy running, admin at $TOXI_URL, proxy '$TOXI_PROXY' listen->upstream=MinIO"
echo "  - an S4 zstd-3 instance whose --endpoint-url is the toxiproxy data port"
echo "  - ES s3 clients: tdirect (-> toxiproxy data port), ts4z3 (-> that S4 instance)"

ES_URL="$ES_URL" MINIO_URL="$MINIO_URL" TOXI_URL="$TOXI_URL" TOXI_PROXY="$TOXI_PROXY" \
  RTT_MS="$RTT_MS" python3 phase_b1_rtt.py
