//! Regression guard for v0.8.5 #82 C-2 (PyPI semver chaos).
//!
//! Before v0.8.5 the `s4-codec-py` crate's `[package].version` was
//! hard-coded to `"0.1.0"` while the rest of the workspace was on
//! v0.8.x. A `maturin build` would then have published a wheel labelled
//! `s4_codec-0.1.0-…whl` to PyPI — i.e. tagged as a pre-release of code
//! that is in fact a stable v0.8.x line. The fix was to switch to
//! `version.workspace = true` so the binding inherits the workspace
//! semver.
//!
//! `CARGO_PKG_VERSION` alone can't tell us "did inheritance work?" (it
//! evaluates to whatever string is in the manifest, hard-coded or not),
//! so the assertion below is "the version is at least 0.8.0". If a
//! future hand-edit reverts to the literal "0.1.0" — or anything else
//! pre-0.8 — this test fails loudly before the wheel reaches PyPI.

use semver::Version;

#[test]
fn binding_version_is_workspace_inherited_semver() {
    let raw = env!("CARGO_PKG_VERSION");
    let v = Version::parse(raw)
        .unwrap_or_else(|e| panic!("CARGO_PKG_VERSION {raw:?} is not valid semver: {e}"));

    // Regression guard: the historical hard-coded value (v0.8.5 #82 C-2).
    // If you ever legitimately need to ship a 0.1.x line of the binding
    // (you don't — npm/PyPI semver should track the workspace), update
    // the floor below in lock-step and document why.
    let floor = Version::new(0, 8, 0);
    assert!(
        v >= floor,
        "s4-codec-py CARGO_PKG_VERSION = {v}, expected >= {floor}. \
         The most likely cause is the `[package].version` field reverting \
         from `version.workspace = true` back to a hard-coded literal \
         (the v0.8.5 #82 C-2 bug). Re-instate workspace inheritance."
    );

    // Belt-and-braces: the original buggy literal explicitly.
    assert_ne!(
        raw, "0.1.0",
        "s4-codec-py is hard-coded to the legacy 0.1.0 literal again \
         (v0.8.5 #82 C-2 regression)."
    );
}
