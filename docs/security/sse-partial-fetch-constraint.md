# SSE partial-fetch constraint — why only SSE-S4 chunked supports
# Range GET fast-path

**Last reviewed:** v0.9.0 (2026-06-07)
**Audience:** operators sizing Range-GET-heavy workloads against an
SSE-enabled S4 deployment, and reviewers evaluating why SSE-KMS /
SSE-C / SSE-S4 buffered / multipart-with-SSE keep the buffered
fallback path.
**Companion:** [`threat-model.md`](threat-model.md) §2 row
*"Range GET on encrypted object slices ciphertext at pre-encrypt
offsets"*.

## TL;DR

| SSE mode | Wire magic | Range GET fast-path? | Why |
|---|---|---|---|
| SSE-S4 chunked (`--sse-chunk-size > 0`) | `S4E6` | ✅ Yes — partial-fetch + per-chunk decrypt via v3 sidecar (v0.9 #106) | Each chunk carries its own 16-byte AES-GCM tag → chunk-aligned decrypt is well-defined |
| SSE-S4 chunked legacy | `S4E5` | ⚠️ Decrypt-only (no v3 sidecar emitted) | Pre-v0.8.1 #57 chunked variant; kept for back-compat reads only |
| SSE-S4 buffered (`--sse-chunk-size 0`) | `S4E2` | ❌ No — buffered fallback | One AES-GCM tag covers the whole body; partial decrypt impossible without full-body verify |
| SSE-S4 legacy single-key | `S4E1` | ❌ No — buffered fallback | Same whole-body tag shape as S4E2 |
| SSE-C (customer-provided key) | `S4E3` | ❌ No — buffered fallback | Single whole-body AES-GCM tag (per-request key, no chunked variant defined yet) |
| SSE-KMS (envelope) | `S4E4` | ❌ No — buffered fallback | Single whole-body AES-GCM tag under the per-object DEK (no chunked variant defined yet) |
| Multipart with any SSE | per-part `S4Ex` | ❌ No — sidecar omission (v0.8.16 #151 / #106) | Per-part SSE crosses the part boundary; v3 sidecar isn't yet plumbed for the multipart wire shape |

## Why this is not "implementation laziness"

AES-256-GCM is an **authenticated encryption with associated data**
(AEAD) construction. A single GCM encrypt operation produces:

- a ciphertext the same length as the plaintext,
- a 12-byte nonce, and
- **one 16-byte authentication tag** that covers the *entire*
  ciphertext + AAD.

Decrypt is defined only over a `(nonce, ciphertext, tag, AAD)`
quadruple. If a caller hands GCM a contiguous **subset** of the
ciphertext — say bytes 1000..2000 of a 5 GiB body — the decryption
result is mathematically undefined and the implementation MUST reject
it. There is no "trust the prefix and skip the tag" mode in the
AEAD contract; that would be a chosen-ciphertext oracle.

So for the **non-chunked envelopes** (`S4E1` / `S4E2` / `S4E3` /
`S4E4`), the on-disk layout is one envelope = one nonce + one tag
covering the whole encrypted body. The only way to expose plaintext
byte `N..M` to the GET path is:

1. fetch the **entire** ciphertext from the backend,
2. AES-GCM-decrypt + tag-verify the whole body,
3. then frame-parse the plaintext, slice the requested range, and
   discard the rest.

That's exactly what the v0.8.12 #120 buffered fallback does, and it's
the **correct** answer for whole-body AEAD — not an optimization
deferment.

## Why the chunked variant (S4E6) is different

The `S4E6` wire shape introduced in v0.8 #52 (refined to S4E6 in
v0.8.1 #57) explicitly slices the plaintext into fixed-size chunks
**before** AES-GCM is invoked, then encrypts each chunk under a
**distinct nonce derived from a per-PUT salt + chunk index**, with its
own 16-byte tag:

```text
[S4E6 header (24 B): magic + algo + key_id + chunk_size + chunk_count
                     + per-PUT salt]
[chunk 0 ciphertext (chunk_size bytes)] [chunk 0 tag (16 B)]
[chunk 1 ciphertext (chunk_size bytes)] [chunk 1 tag (16 B)]
...
[chunk N-1 ciphertext (≤ chunk_size bytes)] [chunk N-1 tag (16 B)]
```

The AEAD contract is satisfied **per chunk**, not over the whole
body. That changes the picture:

- Partial-fetching just the enclosing chunks (e.g. chunks 7..9 for a
  Range GET that lands inside the plaintext bytes covered by those
  chunks) still yields a well-formed `(nonce, ciphertext, tag, AAD)`
  quadruple for each chunk, so each one can be authenticated and
  decrypted independently.
- The per-PUT salt + chunk index in the AAD prevents a backend
  attacker from rearranging chunks (chunk 7's tag won't verify if
  the gateway hands it to AES-GCM as if it were chunk 8).
- The v3 sidecar (`INDEX_VERSION = 3` in
  [`crates/s4-codec/src/index.rs`](../../crates/s4-codec/src/index.rs))
  carries the `enc_chunk_size`, `enc_chunk_count`, `enc_key_id`,
  `enc_salt`, `enc_plaintext_len`, and `enc_header_bytes` fields so
  the GET path can compute encrypted byte ranges **without re-reading
  the body's S4E6 header**.

That's what unlocks the v0.9 #106 fast-path. The gateway maps a
client-visible Range `bytes=N-M`:

1. plaintext-frame index lookup (existing S4IX path) → which framed
   chunks (`compressed_offset`, `compressed_size` in **pre-encrypt**
   byte space) cover bytes `N..M`,
2. for each pre-encrypt byte range, compute the enclosing
   `S4E6` chunk indices using `enc_chunk_size`,
3. partial GET against the backend for only those chunks
   (`S4E6` header bytes + `chunks_in_range * (enc_chunk_size + 16)`),
4. per-chunk AES-GCM decrypt + tag verify,
5. frame-parse the decrypted plaintext, slice at the requested
   boundaries.

## What would it take to extend the fast-path to other SSE modes?

This is **not** "wire-up some plumbing" work. It requires designing
and shipping a new chunked AEAD envelope for each mode, then version-
bumping the sidecar (or introducing per-mode sidecar variants) to
carry the per-chunk salt and key material. Concretely:

- **SSE-KMS chunked** (provisional `S4E7`): needs the DEK fetched
  once at PUT, then per-chunk nonce derivation under that DEK with
  the wrapped-DEK + key_id placed in the AAD of each chunk (so a
  rewrapped-DEK attacker still fails verify). The wrapped DEK is
  per-object, not per-chunk, so the header layout matches today's
  `S4E4` plus the chunked tail.
- **SSE-C chunked** (provisional `S4E8`): same chunked tail, but the
  per-request customer key arrives in every PUT/GET header — the
  per-chunk nonce derivation still works, but the v3-style sidecar
  cannot store the chunk_size + count without leaking workload-size
  information to anyone who can `HEAD .s4index` against the bucket
  (`HEAD` doesn't include the customer-key check); operators using
  SSE-C generally care about exactly this leak, so the sidecar
  shape for SSE-C may end up encrypted or omitted.
- **Multipart per-part SSE**: each part already carries its own
  `S4Ex` envelope today. The sidecar omission (v0.8.16 #151) is
  driven by the per-part boundary not matching the per-frame
  boundary; a fast-path here would need either part-aligned framing
  (operator-visible chunking constraint) or per-part v3 sidecar
  entries with a part-boundary map.

These are roadmap candidates for v0.11+ (the v0.10 cycle is focused
on Docker image distribution + the remaining v3 sidecar plumbing for
already-chunked SSE-S4 edge cases). They are **not** "we forgot to
ship them" — each requires its own wire-format proposal, a fuzz
target, and a key-material lifecycle review.

## Operator guidance — when does the fast-path matter?

Range GET fast-path matters when **all four** of:

1. clients hit the same object with many small `Range:` requests
   (parquet / ORC footer reads, video segment seeks, log-line slice
   reads),
2. those objects are **large** (> ~16 MiB; below that the
   buffered-decrypt cost is a one-page-cache event and the
   fast-path savings are noise),
3. SSE is required by your compliance posture (otherwise just turn
   SSE off and you get the v0.8.4 #73 sidecar fast-path
   automatically), and
4. you can scope your data to **SSE-S4** rather than SSE-KMS or
   SSE-C (the latter two have downstream key-management value the
   chunked envelope doesn't yet capture).

If all four hold, the recommended configuration is:

```bash
s4-server \
  --sse-s4-key /etc/s4/sse.key \
  --sse-chunk-size 1048576 \
  ...
```

(`1048576` = 1 MiB chunks. Smaller chunks = finer-grained partial
fetch + higher tag overhead; larger chunks = fewer GCM-tag bytes on
disk but more wasted decrypt work per Range GET. 1 MiB is the
default since v0.8 #52 and matches the typical parquet row-group
read pattern.)

If your workload is **Range-GET-heavy and you need SSE-KMS or
SSE-C**: the buffered fallback fetches the whole encrypted body on
every Range GET. For a 5 GiB object that's 5 GiB of backend egress
per Range request. Either:

- accept the buffered cost (acceptable when Range GETs are rare or
  objects are small), or
- restructure the data into smaller objects (e.g. one object per
  parquet row group) so the buffered fetch is bounded, or
- wait for v0.11+ chunked-KMS / chunked-C envelopes (candidate, not
  committed).

If your workload is **multipart-with-SSE + Range-GET-heavy**: the
sidecar is omitted entirely on multipart-SSE PUT (v0.8.16 #151), so
every Range GET reads the full object back. Same mitigation list.

## References

- Wire-format definitions:
  [`crates/s4-server/src/sse.rs`](../../crates/s4-server/src/sse.rs)
  (S4E1..S4E6 envelopes).
- Sidecar v3 layout:
  [`crates/s4-codec/src/index.rs`](../../crates/s4-codec/src/index.rs)
  (`INDEX_VERSION = 3`).
- Threat-model row:
  [`threat-model.md`](threat-model.md) §2 (compressed payload at
  rest).
- Range GET fast-path entry:
  [README §"Streaming I/O" → "Encryption-aware Range GET fast-path"](../../README.md#streaming-io).
- AES-GCM construction:
  [NIST SP 800-38D](https://csrc.nist.gov/publications/detail/sp/800-38d/final)
  (the AEAD contract — see §7.2 for the "any modification to the
  ciphertext or AAD changes the tag" property that makes partial
  decrypt impossible).
