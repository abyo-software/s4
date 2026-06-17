# Durability, corruption recovery, and the repair tool

### Write protocol
A PUT goes through three S3 calls behind one client-visible request:

1. **PUT `<key>`** — the compressed S4F2-framed body (atomic single-PUT
   for objects under the multipart threshold; otherwise an S3 multipart
   upload with per-part frames).
2. **PUT `<key>.s4index`** — the S4IX sidecar with per-frame offset +
   original-size + crc32c entries.
3. (multipart only) **CompleteMultipartUpload** — finalises the main
   object atomically; the sidecar is written after this completes.

The main object PUT is the **commit point**; the sidecar exists to
optimise Range GET and is treated as recoverable / rebuildable from the
main object (next section).

### Failure modes and what each one looks like

| Failure | Visible symptom | Recovery |
|---|---|---|
| Client disconnects mid-PUT | Backend returns `IncompleteBody` or 5xx, S4 maps to `TruncatedStream` (v0.8.4 #73). Main object NOT created; sidecar NOT created. No partial state. | None needed — retry the PUT |
| Main object PUT succeeds, sidecar PUT fails | GETs work (full object decode, no range optimisation); Range GETs fall back to "read whole object, decode, slice". | `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>` rebuilds the sidecar by re-scanning frames in the main object |
| Multipart UploadPart succeeds, CompleteMultipartUpload fails | Backend cleans up uncommitted parts on lifecycle-driven `AbortIncompleteMultipartUpload` (S3 default 7 days, or operator policy). | Retry the upload; orphan parts charged but auto-deleted |
| S3 returns a corrupted object body (rare, but happens on hardware faults) | Per-frame `crc32c` mismatch on decode → `CodecError::CrcMismatch` → S4 returns 500 to client with diagnostic. | None within S4 — fix at the backend storage layer; S4 won't return corrupted bytes |
| Sidecar diverges from main object (manual `aws-cli` edit, etc.) | First Range GET that hits the diverged region returns 500 with `IndexFrameMismatch`. | `s4 verify-sidecar <bucket>/<key> --endpoint-url <BACKEND>` flags it; `s4 repair-sidecar` rebuilds |
| Backend object exists, sidecar missing entirely | GETs work; Range GETs degrade to fallback path. | `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>` |
| Bucket has accumulated orphan `.s4index` from the v0.8.15 H-g window | Storage bill grows but reads still work (orphans never reach the GET path). | `s4 sweep-orphan-sidecars <bucket> --endpoint-url <BACKEND> --delete` (run without `--delete` first to inspect). See `../orphan-sidecar-recovery.md`. |

### CRC scope

`crc32c` is computed over the **decompressed original payload** of each
frame and stored in both the frame header and the sidecar entry. This
catches:
- Mid-flight corruption at the backend storage layer
- Codec backend bugs that decode to subtly wrong bytes
- Forged manifest attacks where the attacker replaces the compressed body

It does **not** catch:
- A correctly-encoded malicious payload from a tampered backend (the
  CRC verifies the bytes match what was encoded, not that what was
  encoded was the originally-PUT bytes) — that's what S4's SigV4 auth
  on the PUT side covers
- Lost frames from a truncated multipart that nonetheless committed
  (the per-part Complete API itself is the integrity check there)

### Repair tool status

v0.9 #106 shipped three sidecar-maintenance subcommands on the `s4`
binary. All three point at the **backend** (not the S4 gateway) — the
gateway hides `.s4index` from listings and decompresses bodies on GET,
both of which break this tooling:

```bash
# Read-only check. Exits 0 on Ok / LegacyV1 / MissingHarmless
# (single-frame object, no sidecar by design) / MissingUnknown (body
# exceeds the deep-scan cap, can't classify); exits 1 on
# MissingDivergent / StaleEtag / StaleSize / DecodeError /
# EncryptedSidecarUnsupported (SSE-S4 chunked, see follow-up below).
s4 verify-sidecar bucket/key --endpoint-url https://s3.example.com

# Re-scan the main object and overwrite the sidecar. Default body cap
# is 5 GiB (matches --max-body-bytes); pass --max-body-bytes to raise.
# Does NOT yet support SSE-S4 chunked encrypted objects from the CLI
# (operator needs the SSE keyring; v0.10 roadmap is to plumb
# `--sse-s4-key <path>` through). Until then, re-PUT the object via
# the v0.9+ gateway to regenerate the v3 sidecar.
s4 repair-sidecar bucket/key --endpoint-url https://s3.example.com

# Find dangling `.s4index` whose pair is missing or stale. Dry-run by
# default; --delete actually removes them. The default --delete only
# removes pair-bound orphans (PairedMissing / PairedEtagMismatch /
# PairedSizeMismatch); SidecarUndecodable entries stay until you
# escalate with --delete-undecodable (guards against deleting legacy
# reserved-name user data under --allow-legacy-reserved-key-reads).
s4 sweep-orphan-sidecars bucket --endpoint-url https://s3.example.com [--delete] [--delete-undecodable]
```

The manual fallback (DELETE the sidecar — Range GET drops to the
full-read path) still works for one-offs without the CLI handy. See
`../orphan-sidecar-recovery.md` for the v0.8.15 H-g cleanup recipe
using `s4 sweep-orphan-sidecars`.
