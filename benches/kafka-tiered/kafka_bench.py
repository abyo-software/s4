#!/usr/bin/env python3
"""Kafka tiered-storage (KIP-405) compression benchmark.

For each S3 endpoint — direct (MinIO) and S4 (zstd-3 → MinIO) — a single-node
KRaft Kafka 3.9.1 broker tiers log segments to S3 via the Aiven tiered-storage
plugin (plugin-level compression/encryption OFF, so S4 is the only re-compressor).
Per endpoint, one topic per producer compression.type {none, snappy, lz4, zstd}
is produced, the rolled segments tier to S3, and we measure:
  - tiered bytes per topic (direct vs S4 → saving %)
  - cold remote-fetch latency: restart broker (empty chunk cache), consume from
    earliest (rolled segments are local-deleted, so the read is served from S3).

The honest split (same shape as the Grafana Loki doc): setting the *producer*
compression.type to zstd shrinks segments at the source — usually a bigger,
simpler win than S4 re-compressing. S4's wedge is uncompressed/legacy segments
and the can't-change-producer case.
"""
import json, os, subprocess, time, urllib.request

W = os.path.dirname(os.path.abspath(__file__))
CLUSTER_ID = "zKFnrQZcSPOW9DVGhtfq3w"
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")
NRECORDS = 600000
SEGMENT_BYTES = 8388608
COMPRESSIONS = ["none", "snappy", "lz4", "zstd"]
# (name, s3 endpoint, bucket)
ENDPOINTS = [("direct", "http://localhost:9000", "kafka-direct"),
             ("s4", "http://localhost:8011", "kafka-s4")]

def sh(*a, **k): return subprocess.run(a, env=ENV, capture_output=True, text=True, **k)
def aws(*a): return sh("aws", "--endpoint-url", "http://localhost:9000", *a)
def K(*a): return sh("docker", "exec", "kafkabench", "/opt/kafka/bin/" + a[0], *a[1:])

SERVER_PROPS = """process.roles=broker,controller
node.id=1
controller.quorum.voters=1@localhost:9093
listeners=PLAINTEXT://:9092,CONTROLLER://:9093
advertised.listeners=PLAINTEXT://localhost:9092
inter.broker.listener.name=PLAINTEXT
controller.listener.names=CONTROLLER
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
log.dirs=/var/lib/kafka/data
num.partitions=1
offsets.topic.replication.factor=1
transaction.state.log.replication.factor=1
transaction.state.log.min.isr=1
group.initial.rebalance.delay.ms=0
log.retention.check.interval.ms=10000
remote.log.storage.system.enable=true
remote.log.storage.manager.class.name=io.aiven.kafka.tieredstorage.RemoteStorageManager
remote.log.storage.manager.class.path=/opt/tiered-storage/*
remote.log.storage.manager.impl.prefix=rsm.config.
remote.log.metadata.manager.class.name=org.apache.kafka.server.log.remote.metadata.storage.TopicBasedRemoteLogMetadataManager
remote.log.metadata.manager.listener.name=PLAINTEXT
remote.log.metadata.manager.impl.prefix=rlmm.config.
rlmm.config.remote.log.metadata.topic.replication.factor=1
rlmm.config.remote.log.metadata.topic.num.partitions=5
rsm.config.storage.backend.class=io.aiven.kafka.tieredstorage.storage.s3.S3Storage
rsm.config.storage.s3.bucket.name={bucket}
rsm.config.storage.s3.region=us-east-1
rsm.config.storage.s3.endpoint.url={endpoint}
rsm.config.storage.s3.path.style.access.enabled=true
rsm.config.storage.aws.access.key.id=minioadmin
rsm.config.storage.aws.secret.access.key=minioadmin
rsm.config.chunk.size=4194304
rsm.config.compression.enabled=false
rsm.config.encryption.enabled=false
rsm.config.fetch.chunk.cache.class=io.aiven.kafka.tieredstorage.fetch.cache.MemoryChunkCache
rsm.config.fetch.chunk.cache.size=104857600
rsm.config.fetch.chunk.cache.retention.ms=600000
"""

def write_props(endpoint, bucket):
    os.makedirs(f"{W}/kafka-config", exist_ok=True)
    open(f"{W}/kafka-config/server.properties", "w").write(SERVER_PROPS.format(endpoint=endpoint, bucket=bucket))

def restart_broker(wipe=False):
    sh("docker", "rm", "-f", "kafkabench")
    if wipe:
        sh("rm", "-rf", f"{W}/data")
    os.makedirs(f"{W}/data", exist_ok=True)
    sh("docker", "run", "-d", "--name", "kafkabench", "--network", "host",
       "-e", f"CLUSTER_ID={CLUSTER_ID}",
       "-v", f"{W}/kafka-config/server.properties:/mnt/shared/config/server.properties:ro",
       "-v", f"{W}/kafka-config/payloads.txt:/opt/payloads.txt:ro",
       "-v", f"{W}/plugins/libs:/opt/tiered-storage:ro",
       "-v", f"{W}/data:/var/lib/kafka/data",
       "apache/kafka:3.9.1")
    for _ in range(40):
        if K("kafka-broker-api-versions.sh", "--bootstrap-server", "localhost:9092").returncode == 0:
            return True
        time.sleep(2)
    return False

def bucket_bytes(bucket, prefix=None):
    args = ["s3api", "list-objects-v2", "--bucket", bucket,
            "--query", "[sum(Contents[].Size), length(Contents)]", "--output", "json"]
    if prefix:
        args += ["--prefix", prefix]
    r = aws(*args)
    try:
        v = json.loads(r.stdout or "[null,0]"); return int(v[0] or 0), int(v[1] or 0)
    except Exception:
        return 0, 0

def wait_tiering_settle(bucket, max_s=180):
    """Poll until the bucket stops growing (all rolled segments copied)."""
    last, stable = (-1, -1), 0
    for _ in range(int(max_s / 3)):
        cur = bucket_bytes(bucket)
        if cur == last and cur[1] > 0:
            stable += 1
            if stable >= 3:
                return True
        else:
            stable, last = 0, cur
        time.sleep(3)
    return False

def local_log_count(topic):
    r = sh("docker", "exec", "kafkabench", "sh", "-c",
           f"ls /var/lib/kafka/data/{topic}-0/*.log 2>/dev/null | wc -l")
    try:
        return int((r.stdout or "0").strip())
    except Exception:
        return -1

def wait_local_deleted(topics, max_s=120):
    """Wait until each topic has <=1 local .log (only the active segment), so a
    consume-from-earliest is forced to read rolled segments from remote."""
    for _ in range(int(max_s / 3)):
        if all(0 <= local_log_count(t) <= 1 for t in topics):
            return True
        time.sleep(3)
    return False

def presign(endpoint, bucket, key):
    r = sh("aws", "--endpoint-url", endpoint, "s3", "presign", f"s3://{bucket}/{key}", "--expires-in", "300")
    return r.stdout.strip()

def fairness_check(bucket, endpoint, topic="tnone"):
    """Roll-point-independent check: GET one tiered `none` segment back through S4
    (logical size) vs its stored compressed size — confirms the saving is the
    per-segment zstd ratio, not a counting artifact."""
    r = aws("s3api", "list-objects-v2", "--bucket", bucket, "--prefix", f"{topic}-",
            "--query", "Contents[?ends_with(Key, `.log`)].Key | [0]", "--output", "text")
    key = (r.stdout or "").strip()
    if not key or key == "None":
        return None
    stored = aws("s3api", "head-object", "--bucket", bucket, "--key", key,
                 "--query", "ContentLength", "--output", "text").stdout.strip()
    # binary-safe length: capture raw bytes (text=False), not decoded chars
    body = subprocess.run(["curl", "-s", presign(endpoint, bucket, key)],
                          env=ENV, capture_output=True).stdout
    logical = len(body)
    stored = int(stored)
    return {"key": key, "stored_compressed_bytes": stored, "logical_bytes_via_s4": logical,
            "per_segment_saving_pct": round((logical - stored) / logical * 100, 2) if logical else None}

def produce(topic, comp, n):
    r = K("kafka-producer-perf-test.sh", "--topic", topic, "--num-records", str(n),
          "--throughput", "-1", "--payload-file", "/opt/payloads.txt",
          "--producer-props", "bootstrap.servers=localhost:9092", f"compression.type={comp}",
          "batch.size=262144", "linger.ms=50")
    ok = f"{n} records sent" in (r.stdout or "")
    return ok, (r.stdout or "").strip().splitlines()[-1] if r.stdout else (r.stderr or "")[-200:]

def create_topic(topic):
    K("kafka-topics.sh", "--bootstrap-server", "localhost:9092", "--create", "--topic", topic,
      "--partitions", "1", "--replication-factor", "1",
      "--config", "remote.storage.enable=true", "--config", f"segment.bytes={SEGMENT_BYTES}",
      "--config", "local.retention.ms=1000", "--config", "retention.ms=-1",
      "--config", "max.message.bytes=4194304")

def consume_timed(topic, expected):
    """Cold consume-from-earliest via consumer-perf-test; returns (fetch_ms, count).
    Parses the CSV row: ...,data.consumed.in.nMsg,...,fetch.time.ms,... ."""
    r = K("kafka-consumer-perf-test.sh", "--bootstrap-server", "localhost:9092",
          "--topic", topic, "--messages", str(expected), "--group", f"g-{topic}-cold",
          "--timeout", "120000")
    fetch_ms, nmsg = None, 0
    for ln in (r.stdout or "").splitlines():
        cols = [c.strip() for c in ln.split(",")]
        # summary row has 'data.consumed.in.nMsg' as a number in col index 4
        if len(cols) >= 8 and cols[0] and cols[0][0].isdigit():
            try:
                nmsg = int(float(cols[4])); fetch_ms = float(cols[7])
            except (ValueError, IndexError):
                continue
    return fetch_ms, nmsg


def main():
    # payload file lives next to server.properties so the container can read it
    if not os.path.exists(f"{W}/kafka-config/payloads.txt"):
        os.makedirs(f"{W}/kafka-config", exist_ok=True)
        with open(f"{W}/kafka-config/payloads.txt", "w") as f:
            subprocess.run(["python3", f"{W}/gen_payloads.py", "50000"], stdout=f, check=True)
    payload_lines = sum(1 for _ in open(f"{W}/kafka-config/payloads.txt"))
    print(f"payloads: {payload_lines} unique lines", flush=True)

    meta = {
        "date": "2026-06-19",
        "host": "AMD Ryzen 9 9950X (16C/32T), Linux",
        "kafka": "apache/kafka:3.9.1 (KRaft single-node, KIP-405 tiered storage)",
        "plugin": "Aiven tiered-storage-for-apache-kafka v1.1.1 (S3 backend; plugin compression+encryption OFF)",
        "object_store": "minio/minio:latest (local)",
        "s4": "v1.2.2, --codec cpu-zstd --dispatcher always --zstd-level 3 --logical-etag",
        "records_per_topic": NRECORDS,
        "segment_bytes": SEGMENT_BYTES,
        "stored_bytes": "tiered S3 bytes per topic prefix (rolled segments only; active local segment excluded — same for every config). Includes S4 sidecars for the s4 endpoint.",
        "remote_fetch_ms": "consumer-perf-test fetch.time.ms for a single cold consume-from-earliest after a broker restart (empty chunk cache); rolled segments are local-deleted so the read is served from S3. Single noisy sample; no-RTT local values; transferable signal is the direct-vs-S4 delta.",
        "not_exercised": "multi-broker replication; plugin-level compression; the --logical-etag negative is captured separately.",
    }
    results = {"meta": meta, "storage": [], "latency": []}
    tiered = {}   # endpoint -> {comp: (bytes, objs)}
    fetch = {}    # endpoint -> {comp: (elapsed, count)}

    for name, endpoint, bucket in ENDPOINTS:
        print(f"\n=== endpoint {name} ({endpoint} -> {bucket}) ===", flush=True)
        aws("s3", "rm", f"s3://{bucket}", "--recursive")
        write_props(endpoint, bucket)
        assert restart_broker(wipe=True), f"{name}: broker not ready"
        topics = [f"t{c}" for c in COMPRESSIONS]
        for c, t in zip(COMPRESSIONS, topics):
            create_topic(t)
            ok, line = produce(t, c, NRECORDS)
            if not ok:
                raise SystemExit(f"{name}/{c}: produce incomplete -> {line}")
            print(f"  produced {c:6s} -> {t}: {line}", flush=True)
        assert wait_tiering_settle(bucket), f"{name}: tiering did not settle"
        assert wait_local_deleted(topics), f"{name}: local segments not deleted (cold fetch would read local)"
        results.setdefault("local_log_counts_after_tiering", {})[name] = {t: local_log_count(t) for t in topics}
        tiered[name] = {}
        for c, t in zip(COMPRESSIONS, topics):
            b, o = bucket_bytes(bucket, prefix=f"{t}-")
            tiered[name][c] = (b, o)
            print(f"  tiered {c:6s}: {b/1e6:7.1f}MB ({o} objs)", flush=True)
        if name == "s4":
            results["fairness_check"] = fairness_check(bucket, endpoint)
            print(f"  fairness (tnone segment): {results['fairness_check']}", flush=True)
        # cold remote-fetch: restart (empty cache), consume each topic from earliest
        assert restart_broker(wipe=False), f"{name}: broker not ready after restart"
        time.sleep(3)
        fetch[name] = {}
        for c, t in zip(COMPRESSIONS, topics):
            el, cnt = consume_timed(t, NRECORDS)
            fetch[name][c] = (el, cnt)
            print(f"  fetch  {c:6s}: {el}s ({cnt} records)", flush=True)

    for c in COMPRESSIONS:
        db, do = tiered["direct"][c]; sb, so = tiered["s4"][c]
        results["storage"].append({
            "compression": c, "direct_bytes": db, "direct_objs": do,
            "s4_bytes": sb, "s4_objs": so,
            "saving_pct": round((db - sb) / db * 100, 2) if db else None,
        })
        results["latency"].append({
            "compression": c,
            "direct_fetch_ms": fetch["direct"][c][0], "direct_count": fetch["direct"][c][1],
            "s4_fetch_ms": fetch["s4"][c][0], "s4_count": fetch["s4"][c][1],
        })

    json.dump(results, open(f"{W}/results/kafka.json", "w"), indent=2)
    print("\nwrote results/kafka.json", flush=True)
    for s in results["storage"]:
        print(f"  {s['compression']:6s} direct={s['direct_bytes']/1e6:6.1f}MB "
              f"s4={s['s4_bytes']/1e6:6.1f}MB saving={s['saving_pct']}%", flush=True)


if __name__ == "__main__":
    os.makedirs(f"{W}/results", exist_ok=True)
    main()
