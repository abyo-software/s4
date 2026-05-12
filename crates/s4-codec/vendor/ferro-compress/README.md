# vendor/ferro-compress

Vendored subset of `ferro-compress` for S4. **Do not** include this directory
in the s4 workspace `members =` list yet — it is a footprint, not a wired-up
dependency. The Phase 1 implementor decides whether to:

1. Promote it to a real workspace member and wire `s4-codec/src/nvcomp.rs` to it,
2. Keep it as documentation of which upstream files are needed and import via
   `path = "../../../ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress"`
   instead, OR
3. Extract a third independent repo (`gpu-compress-rs`) and rewrite both Ferro
   and S4 to depend on that.

See `NOTICE` for license + provenance, `src/lib.rs` for the trimmed module
graph, and `S4_PROPOSAL.md` (workspace root) for the strategic context.

## Re-syncing from upstream

Upstream lives at
`/home/y1/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress/`.
To pull a fresh copy of the verbatim files (everything except `src/lib.rs`,
`Cargo.toml`, `README.md`, `NOTICE`):

```bash
SRC=/home/y1/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress
DST=/home/y1/git/s4/crates/s4-codec/vendor/ferro-compress
for f in algo.rs error.rs nvcomp.rs nvcomp_hlif.rs bitcomp_device.rs slab_alloc.rs; do
    cp "$SRC/src/$f" "$DST/src/$f"
done
cp "$SRC/src/nvcomp_sys/"*.rs "$DST/src/nvcomp_sys/"
cp "$SRC/src/cuda_kernels/nvcomp_hlif_shim.cpp" "$DST/src/cuda_kernels/"
cp "$SRC/build.rs" "$DST/build.rs"
```

Then re-check `src/lib.rs` for any new public items upstream added that S4
should re-export.
