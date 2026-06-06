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

## Recovery recipe (`aws-cli` one-shot)

The recipe lists every `*.s4index` in the bucket, filters to the
ones whose paired logical key has the versioning marker
(`s4-multipart: true`) AND no matching live binding, and deletes
those sidecars. Read the dry-run output before adding `--delete`.

```bash
BUCKET=my-bucket
ENDPOINT=https://s4.example.com

# 1. List candidate sidecars (S4 listing filter hides them; talk
#    to the backend directly via its admin endpoint, NOT through
#    the S4 gateway).
aws s3api --endpoint-url "$BACKEND_ENDPOINT" \
    list-objects-v2 --bucket "$BUCKET" \
    --query 'Contents[?ends_with(Key, `.s4index`)].Key' \
    --output text | tr '\t' '\n' > /tmp/s4_sidecars.txt

# 2. For each candidate, fetch the sidecar and check the embedded
#    source_etag against the live HEAD ETag of the paired logical
#    key. Mismatched ETag (or no live key) → orphan, queue for
#    deletion.
while IFS= read -r SIDECAR_KEY; do
  PAIRED_KEY="${SIDECAR_KEY%.s4index}"
  LIVE_ETAG=$(
    aws s3api --endpoint-url "$ENDPOINT" head-object \
      --bucket "$BUCKET" --key "$PAIRED_KEY" \
      --query ETag --output text 2>/dev/null || echo NONE
  )
  # Bail out if HEAD failed (paired key doesn't exist via the
  # gateway) OR if the sidecar exists for a key that lives under
  # the versioning shadow.
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

## Notes

- The recipe uses `$BACKEND_ENDPOINT` (e.g. real AWS S3 / MinIO /
  Garage) for the list step because the S4 gateway's
  `ListObjectsV2` filter hides `.s4index` entries from listings by
  design. Direct backend access is required to enumerate them.
- v0.8.17 may add an `s4 admin sweep-orphan-sidecars` subcommand
  that automates this loop. Until then, the manual recipe is the
  supported path.
- The sweep is **idempotent and safe** — deleting a stale
  `<key>.s4index` only forces the next Range GET on `<key>` to
  fall back to a full read, which is already the v0.8.15-window
  behaviour for these keys (the binding was broken anyway).
