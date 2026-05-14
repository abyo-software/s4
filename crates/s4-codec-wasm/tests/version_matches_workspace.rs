//! Regression guard for v0.8.5 #82 C-2 (npm semver chaos).
//!
//! Same shape as the sibling test in `s4-codec-py`: the WASM binding
//! used to hard-code `version = "0.1.0"` while the workspace was on
//! v0.8.x, so `wasm-pack build` would have emitted a `package.json`
//! claiming the bundle was a pre-release. The fix is workspace
//! inheritance via `version.workspace = true`; this test asserts the
//! inherited value is in the v0.8+ series so a future revert is caught
//! before the .wasm reaches npm.

use semver::Version;

#[test]
fn binding_version_is_workspace_inherited_semver() {
    let raw = env!("CARGO_PKG_VERSION");
    let v = Version::parse(raw)
        .unwrap_or_else(|e| panic!("CARGO_PKG_VERSION {raw:?} is not valid semver: {e}"));

    let floor = Version::new(0, 8, 0);
    assert!(
        v >= floor,
        "s4-codec-wasm CARGO_PKG_VERSION = {v}, expected >= {floor}. \
         Most likely cause: `[package].version` reverted from \
         `version.workspace = true` back to a hard-coded literal \
         (v0.8.5 #82 C-2 regression). Re-instate workspace inheritance."
    );

    assert_ne!(
        raw, "0.1.0",
        "s4-codec-wasm is hard-coded to the legacy 0.1.0 literal again \
         (v0.8.5 #82 C-2 regression)."
    );
}
