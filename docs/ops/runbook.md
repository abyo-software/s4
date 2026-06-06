# S4 operations runbook

**Last reviewed:** v0.8.18 (2026-06-07)

Procedures for operating an S4 gateway in production. Each
section is structured as **Symptom → Diagnose → Mitigate →
Recover → Prevent**, so an on-call engineer can paste it into
a triage doc without reformatting.

## Quick reference

| What happened | Section |
|---|---|
| Disk full on the gateway host | [§1](#1-disk-full-on-the-gateway-host) |
| GPU runs out of memory | [§2](#2-gpu-out-of-memory) |
| Backend (S3 / MinIO) returns 5xx | [§3](#3-backend-5xx-storm) |
| SSE-S4 key compromised | [§4](#4-sse-s4-key-rotation--compromise) |
| SSE-KMS KEK file missing | [§5](#5-sse-kms-kek-loss) |
| MFA-Delete secret lost | [§6](#6-mfa-delete-secret-loss) |
| Replication backlog growing | [§7](#7-replication-backlog) |
| TLS certificate near expiry | [§8](#8-tls-cert-rotation) |
| Sidecar orphans on a versioned bucket | [§9](#9-orphan-sidecar-sweep) |
| Pre-v0.8.15 `.s4index` user data | [§10](#10-legacy-reserved-key-migration) |
| `cargo audit` advisory on a release | [§11](#11-audit-advisory-on-release-day) |
| Graceful shutdown | [§12](#12-graceful-shutdown--reload) |

---

## 1. Disk full on the gateway host

**Symptom**

- New PUTs fail with 500 `InternalError`.
- `df` shows the volume backing the access log directory or
  the per-manager state files at 100 %.
- Logs include `IoError: ENOSPC`.

**Diagnose**

```bash
journalctl -u s4-server --since "30 min ago" | grep -E "ENOSPC|disk full|IoError"
du -h --max-depth=2 /var/lib/s4
ls -lh /tmp/s4-* 2>/dev/null
```

S4 itself only buffers per-PUT in memory (size-capped by
`--max-body-bytes`); the main on-disk consumers are:

1. **Access log directory** (`--access-log <DIR>`) if used.
2. **Per-manager state files**, one per `--<x>-state-file` flag
   the operator set at boot. The complete list:
   `--versioning-state-file`, `--object-lock-state-file`,
   `--mfa-delete-state-file`, `--cors-state-file`,
   `--inventory-state-file`, `--notifications-state-file`,
   `--tagging-state-file`, `--replication-state-file`,
   `--lifecycle-state-file`. Each is an atomically-written
   JSON snapshot (see §12 SIGUSR1 below for the write trigger).
3. **`/tmp` overflow** for SDK retry buffers (uncommon).

**Mitigate**

S4 does **not** expose a "stop accepting new connections" knob
that takes effect on `systemctl reload`. SIGHUP only rotates
TLS certificates (see §8). To shed load on a disk-full
condition the operator must either:

- **Front S4 with a load balancer** (HAProxy / nginx / ALB) and
  drain at the LB layer, OR
- **Lower `--max-concurrent-connections`** in the systemd unit
  file and `systemctl restart s4-server` — the value is read
  at boot, so a `reload` won't pick it up.

In flight requests survive a graceful restart (the
`tokio::select!` arm on SIGTERM drains over the 10 s shutdown
window).

**Recover**

```bash
# Rotate / archive access logs (or wire logrotate; see Prevent).
sudo mv /var/lib/s4/access-log/$(date +%Y%m%d).log /backup/

# State-file snapshots are restartable JSON. They're written
# atomically on SIGUSR1 (and on graceful shutdown), and read at
# boot. Backing up to S3 / external storage and truncating in
# place loses nothing as long as the gateway is restarted
# before the next SIGUSR1 / shutdown write.
```

**Prevent**

- Wire `logrotate` (or a sidecar shipper like vector /
  filebeat) on `--access-log <DIR>`. The repo doesn't yet ship
  a `--access-log-max-bytes` knob — manage rotation externally.
- Place each `--<x>-state-file` on a monitored volume; alert at
  70 % full.

---

## 2. GPU out of memory

**Symptom**

- PUTs that should reach nvCOMP path fall back to CPU codec.
- Logs include `s4_gpu_compress_oom_total` metric increment,
  `nvcomp: CUDA out of memory`.

**Diagnose**

```bash
nvidia-smi
# Look for: another process eating VRAM (training jobs co-tenant),
# very high "Memory-Usage" per S4 process, or a leak.

curl -s http://localhost:9091/metrics | grep s4_gpu
# Key: s4_gpu_compress_oom_total, s4_gpu_compress_seconds,
# s4_codec_chosen_total{codec="nvcomp-..."}
```

**Mitigate**

- The codec layer already falls back to the CPU equivalent on
  GPU OOM (per-PUT). No client-visible failure occurs unless
  CPU is also saturated.
- If a noisy-neighbour process is the cause, evict it or move
  S4 to a dedicated GPU host.

**Recover**

```bash
# Drop the sampling dispatcher's GPU promotion threshold so
# small objects stay on CPU and only large ones contend for
# the GPU.
sudo systemctl edit s4-server   # add `--gpu-min-bytes 4194304`
sudo systemctl restart s4-server
```

**Prevent**

- Production hosts should run S4 alone on the GPU; document
  this in the deployment manifest.
- Wire `s4_gpu_compress_oom_total` to a Prometheus alert at
  >5 events / 5 min.

---

## 3. Backend 5xx storm

**Symptom**

- Many PUTs fail with `502 BadGateway` or `503 SlowDown`
  (mapped from backend error).
- `s4_backend_error_total` metric climbs.

**Diagnose**

```bash
# Confirm backend is the source (not S4).
aws --endpoint-url $BACKEND_ENDPOINT s3 ls
# If `aws s3 ls` works direct-to-backend, S4 is the problem.
# If `aws s3 ls` also fails, backend is the problem.

curl -s http://localhost:9091/metrics | grep -E "s4_backend|s4_replication_failed"
```

**Mitigate**

- S4 propagates backend 5xx unchanged. Client SDKs retry; no
  data loss.
- If replication is configured, the dispatcher's bounded
  semaphore (default 1024 in-flight per
  `--replication-max-concurrent`) prevents the backlog from
  unbounded growth.
- Operator-side mitigation is **on the backend**, not on S4.

**Recover**

- Once the backend is healthy again, replication status is
  swept by `replication::status_sweep_for_test`-equivalent
  background task (v0.8.3 #66).
- For lost replication entries, re-run a manual `aws s3 sync
  s3://src s3://dst` from a known-good baseline.

**Prevent**

- Monitor backend latency at p99; alert when p99 > 1 s for >
  5 min.
- Document the backend's own runbook alongside this one.

---

## 4. SSE-S4 key rotation / compromise

**Symptom**

- Suspected key compromise: leaked `--sse-s4-key` file, ex-
  employee, etc.
- Or scheduled rotation cadence triggers.

**Procedure**

The S4 keyring (`v0.5 #29`) supports id-tagged keys:

- **id=1** is the **active** slot (`--sse-s4-key`).
- **id=2..N** are **retired** decrypt-only slots
  (`--sse-s4-key-rotated id=N,key=<path>`).

Rotation moves the old key to a retired slot and installs a
fresh active key:

```bash
# 1. Generate the new key (32 bytes from /dev/urandom).
head -c 32 /dev/urandom > /etc/s4/keys/active-new.key
chmod 0400 /etc/s4/keys/active-new.key

# 2. Move the previous active key to the retired list.
sudo mv /etc/s4/keys/active.key /etc/s4/keys/retired-$(date +%Y%m%d).key
sudo mv /etc/s4/keys/active-new.key /etc/s4/keys/active.key

# 3. Restart with both arguments:
#    --sse-s4-key <active>
#    --sse-s4-key-rotated id=2,key=<retired>   (repeat for older retired slots)
sudo systemctl edit s4-server   # adjust ExecStart
sudo systemctl restart s4-server
```

**Compromise recovery**

If the old key is suspected compromised, plan a re-encrypt:

1. Add the new key as the active slot per the rotation procedure.
2. Run a one-shot scan that GETs every encrypted object (which
   decrypts under the now-retired key) and PUTs it back (which
   encrypts under the new key). The integration test
   `sse_s4_keyring_rotation_e2e` exercises this shape.
3. Once the scan is complete, remove the retired key from the
   `--sse-s4-key-rotated` list and restart.

**Prevent**

- Rotate quarterly on a calendar trigger.
- Store retired keys offline (HSM, vault); don't leave them in
  `/etc/s4/keys` indefinitely.

---

## 5. SSE-KMS KEK loss

**Symptom**

- GETs on SSE-KMS objects fail with `KmsError: KEK not found`.
- The file under `--kms-local-dir` was deleted / overwritten.

**Diagnose**

```bash
ls -l /etc/s4/keks/
journalctl -u s4-server | grep -E "kms|KEK"
```

**Mitigate**

- Restore the KEK file from backup. The KEK is a 32-byte
  random file with the basename `<key-id>.kek`.
- Without the KEK, the wrapped DEKs in `S4E4` frames cannot be
  unwrapped — affected objects are **unrecoverable**.

**Prevent**

- Back up `--kms-local-dir` to encrypted offsite storage
  immediately after each new KEK is added.
- Use `--kms-aws-region` (real AWS KMS) for production-grade
  KEK durability; LocalKms is intended for dev / single-tenant
  on-prem with a backup discipline.

---

## 6. MFA-Delete secret loss

**Symptom**

- Operators cannot delete versioned objects on an
  MFA-Delete-Enabled bucket (TOTP secret lost).

**Recovery**

MFA-Delete state lives in the JSON file the operator passed
to `--mfa-delete-state-file <PATH>` at boot. The file is
written by SIGUSR1 (see §12) and on graceful shutdown; it's
read at boot. To recover:

1. Identify the affected bucket from logs / config.
2. Stop the gateway (SIGTERM) so it doesn't overwrite the file
   from memory.
3. Back up the existing state file, then edit it to remove
   the affected bucket's `Enabled` entry (set to `Disabled`,
   or delete the per-bucket key).
4. Restart S4. The bucket reverts to "no MFA required" on
   delete.

This is a destructive recovery — it removes the MFA-Delete
protection — but it's the documented escape hatch when the
TOTP secret is irrecoverable.

**Prevent**

- Store TOTP secrets in the same vault as the SSE keys.
- For high-value buckets, enable both Object Lock Compliance
  retention AND MFA-Delete so a single secret loss doesn't
  open data to overwrite.

---

## 7. Replication backlog

**Symptom**

- `s4_replication_pending_total` climbs.
- Destination bucket lags behind source.

**Diagnose**

```bash
curl -s http://localhost:9091/metrics | grep -E "s4_replication_(pending|failed|completed)_total"
```

**Mitigate**

- The dispatcher caps in-flight tasks at
  `--replication-max-concurrent` (default 1024). If the cap is
  hit, source-side PUTs proceed but the replica queue grows in
  memory.
- For backlog > 10 000, increase the cap **AND** investigate
  destination throughput (5xx, throttling, etc.).

**Recover**

```bash
sudo systemctl edit s4-server
# Bump --replication-max-concurrent to 4096
sudo systemctl restart s4-server
```

The status sweep (`v0.8.3 #66`) retries failed entries on a
periodic cadence.

**Prevent**

- Monitor source PUT rate × destination's PUT capacity; size
  the cap accordingly.
- Use Prometheus `rate(s4_replication_completed_total[5m])` as
  the steady-state baseline.

---

## 8. TLS cert rotation

**Static cert (`--tls-cert` / `--tls-key`)**

```bash
# Rotate cert + key on disk, then SIGHUP — atomic swap (v0.2).
sudo install -m 0644 new-fullchain.pem /etc/s4/tls/cert.pem
sudo install -m 0600 new-privkey.pem  /etc/s4/tls/key.pem
sudo systemctl reload s4-server   # SIGHUP

# Verify:
echo | openssl s_client -connect s4.example.com:443 -servername s4.example.com 2>/dev/null \
  | openssl x509 -noout -dates
```

If reload fails (bad PEM, mismatched key), the listener keeps
serving with the **previous** config; the failure is logged at
WARN and `s4_tls_cert_reload_failed_total` increments. Fix the
files and retry.

**ACME (`--acme`)**

Renewal is automatic. Backup the `--acme-cache-dir` directory
(default `$HOME/.s4/acme`) so a host loss doesn't restart the
Let's Encrypt rate-limit clock.

---

## 9. Orphan sidecar sweep

See [`docs/orphan-sidecar-recovery.md`](../orphan-sidecar-recovery.md).
Applies only to versioning-Enabled buckets that ran v0.8.15.

---

## 10. Legacy reserved-key migration

**Symptom**

- Upgrade from pre-v0.8.15 leaves user objects ending in
  `.s4index` inaccessible (PUT was always blocked from
  v0.8.15; GET / HEAD / DELETE blocked from v0.8.16).
- Clients see `NoSuchKey` on GET or `InvalidObjectName` on
  DELETE.

**Recovery (v0.8.17+)**

```bash
# 1. Turn on the migration escape hatch (reads only).
sudo systemctl edit s4-server   # add `--allow-legacy-reserved-key-reads`
sudo systemctl restart s4-server

# 2. From a client, list + copy legacy objects to a non-reserved
#    key. The list step uses the backend admin endpoint because
#    S4's listing filter hides `.s4index` entries.
aws s3api --endpoint-url $BACKEND_ENDPOINT \
  list-objects-v2 --bucket $BUCKET \
  --query 'Contents[?ends_with(Key, `.s4index`)].Key' \
  --output text | tr '\t' '\n' > /tmp/legacy.txt

while IFS= read -r K; do
  NEW="${K%.s4index}.migrated.bin"
  aws --endpoint-url $S4_ENDPOINT s3 cp "s3://$BUCKET/$K" "s3://$BUCKET/$NEW"
  aws --endpoint-url $S4_ENDPOINT s3 rm "s3://$BUCKET/$K"
done < /tmp/legacy.txt

# 3. Disable the escape hatch.
sudo systemctl edit s4-server   # remove the flag
sudo systemctl restart s4-server
```

Writes (PUT / Copy / Create-Multipart / tagging-write /
ACL-write) targeting `.s4index` keys stay blocked regardless
of the flag, so an attacker cannot use the migration window to
inject new artifacts.

---

## 11. Audit advisory on release day

**Symptom**

- `cargo audit` flags a new CVE in a production dep tree.
- CI fails on the security-audit job.

**Triage**

```bash
cargo audit
# Identify the advisory ID + crate + version.
cargo tree -p <crate>@<version> -i
# Trace the chain to a direct dep we control.
```

**Mitigate**

1. **Direct dep**: bump the version in `Cargo.toml`, run
   `cargo update`, test, ship.
2. **Indirect via an SDK pin**: check whether the upstream SDK
   has a newer version that resolves the chain. If not,
   evaluate exposure (e.g. is the vulnerable code on the
   network-facing path or only on internal init?) and either
   wait for upstream or add an `audit.toml` ignore with a
   tracked issue and expiry.

**Prevent**

- Subscribe to RustSec advisory feed.
- Run `cargo update` weekly + audit; small-step upgrades are
  easier than emergency big-bang ones.

---

## 12. Graceful shutdown / reload

**SIGTERM** / SIGINT — drains in-flight requests over the
graceful-shutdown window (10 s timeout). Use this for normal
k8s rollouts.

**SIGHUP** — hot-reloads `--tls-cert` / `--tls-key`. Does NOT
re-read other config; for those, use SIGTERM + restart.

**SIGUSR1** — atomically dumps every in-memory state manager
to its `--<x>-state-file` JSON path (v0.8.5 #86). Affects
versioning, object_lock, mfa_delete, cors, inventory,
notifications, tagging, replication, lifecycle. Useful before
a planned host suspend / reboot to capture in-memory changes
that haven't yet reached disk. **It does NOT flush the
access-log buffer** — access logs drain on shutdown via the
graceful `tokio::select!` shutdown_notify path.

```bash
# Drain + reboot
sudo systemctl restart s4-server

# Just rotate TLS without restart (SIGHUP)
sudo systemctl reload s4-server

# Force state-manager snapshot dump (writes every --<x>-state-file)
sudo kill -USR1 $(pidof s4-server)
```

---

## Metric reference

The complete list of `s4_*_total` / `s4_*_seconds` /
`s4_*_bytes` metrics is in
[`docs/observability.md`](../observability.md). All metric
names below are verified against
`crates/s4-server/src/metrics.rs`. Recommended alerts:

| Alert | Threshold | Severity |
|---|---|---|
| `rate(s4_replication_dropped_total[5m]) > 5` | sustained 5 min | critical |
| `s4_replication_dropped_total - s4_replication_replicated_total > 10000` | sustained 10 min | warning |
| `s4_tls_cert_reload_total{result="err"} > 0` | any | warning |
| `s4_policy_denials_total{action=~"s3:Bypass.*"} > 0` | any | high (audit) |
| `rate(s4_gpu_oom_total[5m]) > 1` | over 5 min | warning |
| `rate(s4_rate_limit_throttled_total[1m]) > 100` | over 1 min | info |
| `s4_acme_cert_expiry_seconds < 7 * 24 * 3600` | any | warning |

Metric-naming note: S4 does **not** emit a generic
`s4_backend_error_total` counter today — backend 5xx surfaces
via the per-handler error counters in
`s4_requests_total{status=~"5..."}` and the explicit
replication outcome counters above. If a dedicated
backend-error metric matters for your alerting story, treat
it as a follow-up wire-up against the s3s middleware layer.
