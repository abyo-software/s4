# `@abyo-software/s4-codec-wasm`

Browser- and Node-side WASM decoder for [S4](https://github.com/abyo-software/s4)
(Squished S3) objects. Lets a frontend read S4-stored objects directly from
S3 over a presigned URL — no S4 server in the read path.

This crate is the WASM-target sibling of `s4-codec` and shares the same
`CodecKind` IDs and `S4F2` frame format.

## Status

- v0.4 #24 — initial cut.
- API: `decompressFramed`, `decompressSingle`, `supportedCodecs`,
  `supportedFrameMagic`.
- Codec subset (CPU only — no GPU in the browser):
  `passthrough`, `cpu-zstd`, `cpu-gzip`. Encountering a GPU-only codec
  (`nvcomp-zstd`, `nvcomp-bitcomp`, …) returns a hard error — route those
  reads through an S4 server instead.

## Install (when published)

```bash
npm install @abyo-software/s4-codec-wasm
```

`publish` is intentionally **not** wired into CI yet — the package is
shipped manually after each compatible S4 release. PRs welcome.

## Build from source

The crate lives under `crates/s4-codec-wasm/` in the
[abyo-software/s4](https://github.com/abyo-software/s4) repo.

### Plain `cargo` (.wasm only, no JS glue)

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown -p s4-codec-wasm
# → target/wasm32-unknown-unknown/release/s4_codec_wasm.wasm
```

### `wasm-pack` (recommended — emits package.json-ready bundle)

```bash
cargo install wasm-pack            # one-time
cd crates/s4-codec-wasm
wasm-pack build --release --target web --out-dir pkg
# pkg/ now contains s4_codec_wasm.js, s4_codec_wasm.d.ts,
# s4_codec_wasm_bg.wasm — drop into a static site or `npm pack` it.
```

The `package.json` in this directory references the `pkg/` output of the
`wasm-pack` build above.

## Example — decode an S4F2 object fetched via presigned URL

```html
<script type="module">
  import init, { decompressFramed } from "./pkg/s4_codec_wasm.js";
  await init();

  const presignedUrl = "https://my-bucket.s3.amazonaws.com/key?X-Amz-Signature=...";
  const response = await fetch(presignedUrl);
  const compressed = new Uint8Array(await response.arrayBuffer());

  const original = decompressFramed(compressed);
  document.querySelector("pre").textContent =
    new TextDecoder().decode(original);
</script>
```

For legacy single-PUT (non-framed) S4 objects, use `decompressSingle` and
read the codec / original-size / crc32c values out of the S3 object
metadata (`x-amz-meta-s4-codec`, `x-amz-meta-s4-original-size`,
`x-amz-meta-s4-crc32c`).

## Validation status (in this repo)

- Built with `cargo check --target wasm32-unknown-unknown -p s4-codec-wasm`
  and `cargo build --release --target wasm32-unknown-unknown -p s4-codec-wasm`
  (output: ~870 KB `.wasm`).
- Host-target unit tests via `cargo test --manifest-path
  crates/s4-codec-wasm/Cargo.toml --lib` — exercise the CPU-codec dispatch,
  mixed-codec frame walks, GPU-codec rejection, and the diagnostic helpers.
  6 tests pass.
- `wasm-pack` is **not** available on the dev machine; the npm bundle has
  not been emitted in the same change. Run `wasm-pack build` locally to
  produce `pkg/`.

## Design notes

- The crate calls `s4_codec::cpu_zstd::decompress_blocking` and
  `s4_codec::cpu_gzip::decompress_blocking` directly — those are sync
  wrappers around `zstd-rs` / `flate2` that share the same
  decompression-bomb cap and crc32c verify logic as the async server path.
  See `s4-codec/src/cpu_zstd.rs` and `s4-codec/src/cpu_gzip.rs`.
- Frame parsing reuses `s4_codec::multipart::FrameIter` so the WASM and
  server read paths stay byte-for-byte identical.
- For the `s4-codec` crate to compile to `wasm32-unknown-unknown` at all,
  its `tokio` dep is pinned to the slim `["rt"]` feature set instead of
  the workspace `["full"]`. That change lives in
  `crates/s4-codec/Cargo.toml`.

## Demo

`examples/web-demo/index.html` loads a small inline base64 S4F2 sample
and decodes it in-browser. Open it after `wasm-pack build` and the
sample text shows up in the `<pre>`.

## License

Apache-2.0 — same as the rest of S4.
