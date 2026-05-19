codex
[HIGH] crates/s4-codec/src/nvcomp.rs:108,198,284 - nvCOMP still does `Vec::with_capacity(expected_orig_size)` after validation, so a manifest with `original_size = u32::MAX` and matching compressed length can still force a multi-GiB allocation under the 5 GiB ceiling; apply the same bootstrap-cap pattern if the backend supports growth, or use a fallible/streamed allocation strategy with a lower operational cap.

[LOW] crates/s4-codec/src/cpu_zstd.rs:326 - Issue #89 regression coverage only exercises async `Codec::decompress`, leaving the exported `decompress_blocking` path untested for over-limit and sub-limit huge manifests; add blocking variants of both new regression tests.

[LOW] crates/s4-codec/src/cpu_gzip.rs:320 - Issue #89 regression coverage only exercises async `Codec::decompress`, leaving the exported `decompress_blocking` path untested for over-limit and sub-limit huge manifests; add blocking variants of both new regression tests.
tokens used
26,548
