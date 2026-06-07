# Streaming PUT checksum verify — coverage matrix and constraints

**v0.10 wave-2 #A3 follow-up.** Companion to [`docs/security/sse-partial-fetch-constraint.md`](sse-partial-fetch-constraint.md). Same pattern as A2-doc: surface a fundamental implementation constraint explicitly so operators reading [`README.md`](../../README.md) §"Streaming I/O" don't mistake "buffered" for "deferred / lazy implementation".

## TL;DR

| PUT shape × codec branch | client checksum verify mode | reason |
|---|---|---|
| Single-PUT, `cpu-zstd` / `nvcomp-zstd` (`supports_streaming_compress = true`) | **streaming, in-stream tee** (v0.9 #streaming-checksum) | encoder consumes the body as a `Stream<Bytes>`; same chunks tee through the hasher set; memory peak ≈ compressed-size + per-chunk window |
| Single-PUT, `passthrough` | **buffered** (`verify_client_body_checksums` on collected bytes) | no encode step but the framing wrapper still needs the body in memory to compute the per-frame CRC32C the GET path verifies |
| Single-PUT, non-streaming GPU codec (`nvcomp-bitcomp`, `nvcomp-gdeflate`, …) | **buffered** | `Codec::compress_with_telemetry(bytes, codec_kind)` takes `bytes: Bytes` by value — the codec needs the entire body in one buffer to copy to GPU HBM; teeing through a hasher first wouldn't change the memory peak |
| Multipart `upload_part` (any codec) | **buffered, per-part** (`verify_client_body_checksums` on collected part body) | the part body is already in memory because (a) the dispatcher needs a sample for codec selection, (b) the codec needs the full body for encode (same constraint as above, multiplied across N parts in flight), (c) `pad_to_minimum` needs the framed length to decide whether to skip padding |

In short: **the streaming-tee path is available only when the codec itself is streaming-compatible AND the body is single-PUT.** Every other path has an unavoidable in-memory buffer step *before* the hasher would be useful, so streaming the checksum doesn't reduce memory pressure — it would just add a tee layer on top of the same buffered work.

This is intentionally documented as a **codec-API constraint**, not as "TODO: streaming verify for multipart". Closing that gap requires re-architecting the codec trait to accept a `Stream<Bytes>` input (and the multipart framing pipeline to compute padding from a running length instead of a final length), neither of which is a v0.10 scope.

## Where each path lives in `s4-server`

- Single-PUT streaming branch: [`crates/s4-server/src/service.rs::put_object`](../../crates/s4-server/src/service.rs) "Streaming-framed branch" block (search for `tee_into_hashers_with_handle`). Streaming wrapper itself: [`crates/s4-server/src/streaming_checksum.rs`](../../crates/s4-server/src/streaming_checksum.rs).
- Single-PUT buffered branch: same file, "buffered branch" block (search for `verify_client_body_checksums` followed by `compute_digests`). v0.9 audit-R2 #P2-INT-2 added the trailer-verify call here too so `x-amz-trailer` checksums aren't silently dropped on the buffered path.
- Multipart `upload_part`: [`crates/s4-server/src/service.rs::upload_part`](../../crates/s4-server/src/service.rs) (search for `S4 upload_part: framed compressed payload`). The `verify_client_body_checksums` call is between `collect_blob` and the `dispatcher.pick_with_size_hint` codec selection.

## What "streaming win" actually requires

For streaming verify to reduce memory peak beyond the existing buffered path, **all three** of these must hold simultaneously:

1. **The codec is streaming** — implements an `encode_stream(Stream<Bytes>) -> Stream<Bytes>` shape (or equivalent), so input chunks can be encoded and forwarded without ever holding the whole body. Today only `cpu-zstd` and `nvcomp-zstd` qualify (`supports_streaming_compress() == true`).
2. **The downstream consumer accepts a stream** — the AWS SDK's `PutObjectInput.body` is `Option<StreamingBlob>` which is fine; the wire request is also chunked. ✅ already true.
3. **The pre-encode and post-encode steps don't require the full body** — no framing step that needs the final compressed length, no padding step that depends on the encoded byte count, no checksum-of-the-compressed-output step. ✅ true for single-PUT (one `S4F2` frame wraps the compressed body, header carries the length but is written *after* the encode stream completes); ❌ false for multipart `upload_part` (needs `pad_to_minimum` decision on the framed length, which means the framed body must be fully materialised first).

`upload_part` could in principle be re-architected to compute padding from a running length and patch the frame header retroactively — but that's a new design (`S4F3` frame format? streaming padding marker?) that doesn't fit v0.10's "encryption-aware completion + Docker publish" theme. Tracked as a v0.11+ candidate but not a v0.10 deliverable.

## What still works on the buffered paths

Both buffered paths (single-PUT non-streaming codec, multipart `upload_part`) run **all six** AWS-spec checksum algorithms (`Content-MD5`, `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`) via [`verify_client_body_checksums`](../../crates/s4-server/src/service.rs) once the body is in memory. Mismatch → `400 BadDigest`. v0.9 audit-R2 #P2-INT-2 added the matching `verify_client_trailer_checksums` call on the single-PUT buffered branch so SigV4-streaming trailer checksums are no longer silently dropped on that path. The functional contract is identical to the streaming path — the only difference is when the verify runs (after `collect_blob` vs in-stream tee) and what memory peak the gateway observes (compressed + framing buffer vs encode-window).

For very large multipart objects, the operator can tune memory peak by:

- **Reducing part size at the client.** The streaming win is across parts: in-flight parts share the same in-memory budget per concurrent dispatcher slot, so a 100 MiB part size + 10 in-flight parts ≈ 1 GiB working set, vs 5 MiB × 10 ≈ 50 MiB. AWS SDK defaults to 8 MiB which is already a good baseline.
- **`--max-body-bytes` cap (per request).** Refuses oversized parts up-front before any allocation. Default 5 GiB matches AWS S3 single-PUT max; lower for memory-constrained deployments.

The codec choice doesn't affect multipart memory peak in a useful way — every codec collects the part body first regardless of `supports_streaming_compress()`, because the framing + padding constraints above are the same across codecs.

## Roadmap candidates (not v0.10)

| Candidate | What it would require | Status |
|---|---|---|
| `S4F3` streaming frame format | New magic + length-at-end semantics so the frame header can be patched after the encode stream completes; padding marker compatible with a running length | v0.11+ design |
| Streaming `nvcomp-bitcomp` / `nvcomp-gdeflate` | Re-architect the nvCOMP wrappers to chunk the GPU transfer (currently a single HBM upload), expose a `Stream<Bytes>` encode API | v0.12+ design (nvCOMP upstream API constraint) |
| Multipart streaming `upload_part` | Combine the above two — needs `S4F3` AND every codec to support streaming, since `upload_part` doesn't know the codec until after the sample | v0.12+ |

These are tracked here, not in [`README.md`](../../README.md), to keep the README's "Streaming I/O" section focused on what operators get today vs what's available behind opt-in flags.

## References

- v0.9 #streaming-checksum (commit `e59b115`): single-PUT streaming verify implementation
- v0.9 audit-R2 #P2-INT-2 (commit `714018b`): buffered-branch trailer verify (closes the gap on the buffered single-PUT path)
- [`docs/security/sse-partial-fetch-constraint.md`](sse-partial-fetch-constraint.md): companion doc on the SSE-S4 chunked vs other-SSE AEAD constraint (same "fundamental contract, not deferred plumbing" framing)
- [`crates/s4-server/src/streaming_checksum.rs`](../../crates/s4-server/src/streaming_checksum.rs): public API + `tee_into_hashers_with_handle` / `ComputedDigests` / `compute_digests` documentation
- [`crates/s4-codec/src/`](../../crates/s4-codec/src/): codec trait surface (look for `supports_streaming_compress()` on `Codec` impls)
