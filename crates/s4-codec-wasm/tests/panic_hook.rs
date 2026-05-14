//! v0.8.5 #82 C-3 — smoke test for the WASM panic-hook install path.
//!
//! Why this test exists: before v0.8.5 a panic inside the WASM codec
//! (e.g. a tripped decompression-bomb cap or a malformed-frame
//! `unwrap`) would silently poison the WASM linear memory and kill the
//! JS context with no diagnostic in `console`. The fix is to install
//! `console_error_panic_hook` on module load via
//! `#[wasm_bindgen(start)] s4_codec_wasm_init()`.
//!
//! What we *can* assert from a host-target `cargo test`:
//!
//! 1. The hook setter exists and is reachable as a public symbol
//!    (`set_once()` from the dep is callable — i.e. the dep is wired
//!    into the bindings crate, the most common source of "I added the
//!    Cargo.toml dep but forgot the `use`" regressions).
//!
//! 2. `set_once()` is idempotent — calling it twice in quick succession
//!    does not panic. This protects against accidentally turning the
//!    init into a one-shot that breaks if `wasm-bindgen-test` (which
//!    invokes it per-test) or a future caller re-enters.
//!
//! What we *cannot* assert here: the actual browser-side
//! `console.error` redirection on a panic — that needs a real WASM JS
//! host, exercised via `wasm-pack test --node` (not run as part of the
//! workspace `cargo test` because it needs `wasm-pack` + a node
//! runtime). The smoke test below catches the regressions we care
//! about most: "the dep got removed", "the symbol stopped being
//! `pub`", "set_once is no longer idempotent". A real
//! browser-environment panic message is exercised by the demo at
//! `examples/web-demo/`, which deliberately triggers a malformed-input
//! decompress and observes `console.error`.

#[test]
fn panic_hook_set_once_is_idempotent_and_dep_is_wired() {
    // Hitting the dep at all proves the Cargo.toml line is honoured;
    // calling twice proves `set_once()` is the real once-cell-backed
    // flavour and not something that traps on re-entry.
    console_error_panic_hook::set_once();
    console_error_panic_hook::set_once();
}

#[test]
fn s4_codec_wasm_init_symbol_is_public_and_callable() {
    // Catches: someone makes `s4_codec_wasm_init` private during a
    // refactor, or removes it entirely. Either change would silently
    // disable the panic hook in the browser (the `#[wasm_bindgen(start)]`
    // attribute only auto-emits the JS init shim when the function is
    // a wasm-bindgen-exportable item).
    s4_codec_wasm::s4_codec_wasm_init();
    // And calling it again must remain a no-op (set_once contract).
    s4_codec_wasm::s4_codec_wasm_init();
}
