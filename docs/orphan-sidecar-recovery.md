# Orphan `.s4index` sidecar recovery (v0.8.15 H-g aftermath)

Versioning-Enabled buckets running v0.8.15 picked up a bug where
multipart **Complete** wrote an `<key>.s4index` sidecar bound to
the to-be-deleted parent key while the actual object body landed
under the versioning shadow path `<key>.__s4ver__/<version-id>`.
The sidecar's `source_etag` therefore never matched the live
shadow's ETag, the Range-GET fast-path silently fell through to a
full read every time, and the orphan sidecar accumulated on the
backend without ever being reaped by lifecycle rules (lifecycle
operates on logical user keys, not on `.s4index` artifacts).

v0.8.16 #151 F-7 stopped emitting new orphans by skipping the
sidecar block entirely on versioning-Enabled multipart Complete.
This document covers the **one-time recovery** of orphans created
during the v0.8.15 window.

## When this applies

Run the recovery if **all** of the following hold:

1. The bucket has Versioning = Enabled.
2. Multipart uploads (`CompleteMultipartUpload`) ran against the
   bucket while it was on v0.8.15.
3. The backend reports `<key>.s4index` objects that don't pair to
   any active Range-GET path.

Buckets that stayed on v0.8.14 or earlier, that aren't versioning-
Enabled, or that only saw single-PUT writes during the v0.8.15
window have no orphans to recover.

## Recovery recipe (v0.9+ `s4 sweep-orphan-sidecars`)

```bash
BUCKET=my-bucket
BACKEND_ENDPOINT=https://s3.example.com  # real backend (AWS S3 /
                                         # MinIO / Garage / Ceph
                                         # RGW). NOT the S4 gateway.

# 1. Dry-run: report every orphan without touching anything.
s4 sweep-orphan-sidecars "$BUCKET" \
    --endpoint-url "$BACKEND_ENDPOINT"
# Output (one line per orphan):
#   N orphan(s) found in my-bucket (M sidecars scanned):
#     foo.bin.s4index  (paired key MISSING)
#     bar.bin.s4index  (ETag mismatch (sidecar=..., live=...))
# Exit code 1 when any orphan is found — wire that into your CI / cron
# branch as "needs operator action".

# 2. Inspect the output. When satisfied, re-run with --delete to
#    remove the three "pair-bound" categories (PairedMissing,
#    PairedEtagMismatch, PairedSizeMismatch). SidecarUndecodable
#    entries are SKIPPED — they could be legitimate user data on a
#    bucket using the v0.8.17 `--allow-legacy-reserved-key-reads`
#    migration hatch (where users PUT objects whose key happened to
#    end in `.s4index` pre-v0.8.15).
s4 sweep-orphan-sidecars "$BUCKET" \
    --endpoint-url "$BACKEND_ENDPOINT" \
    --delete

# 3. If the dry-run also reported SidecarUndecodable entries AND you
#    are CERTAIN none of them are legacy reserved-name user data,
#    escalate:
s4 sweep-orphan-sidecars "$BUCKET" \
    --endpoint-url "$BACKEND_ENDPOINT" \
    --delete --delete-undecodable
```

The endpoint MUST point at the backend (not the S4 gateway) because
the gateway's `ListObjectsV2` filter hides `.s4index` entries from
listings by design.

The sweep is **idempotent and safe** — deleting a stale `<key>.s4index`
only forces the next Range GET on `<key>` to fall back to a full read,
which is already the v0.8.15-window behaviour for these keys (the
binding was broken anyway).

The same CLI ships two companion commands for single-object
maintenance:

- `s4 verify-sidecar <bucket>/<key> --endpoint-url <BACKEND>` —
  read-only: reports `Ok` / `Missing` / `StaleEtag` / `StaleSize` /
  `LegacyV1` / `DecodeError`.
- `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>` —
  re-scans the main object frames and overwrites the sidecar. Capped
  at 5 GiB main-object body by default (`--max-body-bytes` raises it).

## Pre-v0.9 manual fallback (`aws-cli`)

The `s4 sweep-orphan-sidecars` subcommand landed in v0.9. Operators
still on v0.8.x can run the same logic with `aws-cli`:

```bash
BUCKET=my-bucket
ENDPOINT=https://s4.example.com          # the S4 gateway endpoint
BACKEND_ENDPOINT=https://s3.example.com  # real backend; see note above

# 1. List candidate sidecars (must hit the backend directly).
aws s3api --endpoint-url "$BACKEND_ENDPOINT" \
    list-objects-v2 --bucket "$BUCKET" \
    --query 'Contents[?ends_with(Key, `.s4index`)].Key' \
    --output text | tr '\t' '\n' > /tmp/s4_sidecars.txt

# 2. For each candidate, HEAD the paired logical key. Missing key →
#    orphan (queue for delete).
while IFS= read -r SIDECAR_KEY; do
  PAIRED_KEY="${SIDECAR_KEY%.s4index}"
  LIVE_ETAG=$(
    aws s3api --endpoint-url "$ENDPOINT" head-object \
      --bucket "$BUCKET" --key "$PAIRED_KEY" \
      --query ETag --output text 2>/dev/null || echo NONE
  )
  if [ "$LIVE_ETAG" = "NONE" ]; then
    echo "$SIDECAR_KEY  (orphan: paired key missing)"
  fi
done < /tmp/s4_sidecars.txt > /tmp/s4_orphans.txt

# 3. Inspect /tmp/s4_orphans.txt manually, then delete:
#    awk '{print $1}' /tmp/s4_orphans.txt | while IFS= read -r K; do
#      aws s3api --endpoint-url "$BACKEND_ENDPOINT" delete-object \
#        --bucket "$BUCKET" --key "$K"
#    done
```

The manual recipe only catches `PairedMissing`; the v0.9 CLI also
flags `PairedEtagMismatch` / `PairedSizeMismatch` (sidecar stale
against a still-present paired key) and `SidecarUndecodable` (corrupt
bytes). Upgrade when you can.
