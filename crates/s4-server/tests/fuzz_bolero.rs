//! v0.8.18 P4: server-side fuzz coverage with bolero. Mirrors the
//! `crates/s4-codec/tests/fuzz_bolero.rs` shape (`cargo bolero
//! test --engine libfuzzer <target> -- -max_total_time=86400`)
//! and covers two parsers that ingest fully attacker-controlled
//! strings on the listener edge:
//!
//! - **`sigv4a_auth_header_bolero`** — the SigV4a Authorization
//!   header parser (`sigv4a::parse_authorization_header`).
//! - **`policy_json_bolero`** — the IAM bucket-policy JSON
//!   parser (`policy::Policy::from_json_str`).
//!
//! Both targets share the same correctness contract: **any input
//! must produce a `Result` without panicking, without running
//! for > 10 000 inner iterations, and without allocating more
//! than the caller-bounded cap**. The corpora live under
//! `crates/s4-server/tests/__fuzz__/<target>/corpus/` and are
//! seeded by the nightly fuzz farm
//! (`.github/workflows/fuzz-nightly.yml`).
//!
//! Two more targets are roadmap (v0.8.19+):
//!
//! - SigV4 canonical query / path encoders
//!   (`routing::canonical_query_string` /
//!   `routing::canonical_uri_path`). Needs a test-only
//!   re-export so the `tests/` directory can call into the
//!   currently-private helpers.
//! - SSE chunked-frame decrypt against random keys / bodies
//!   (via the public `sse::decrypt_chunked_buffered_default`
//!   surface).
//!
//! Local invocation:
//!
//!     cargo test --test fuzz_bolero
//!     # or, for the coverage-guided engine:
//!     cargo install cargo-bolero
//!     cargo bolero test --engine libfuzzer sigv4a_auth_header_bolero \
//!         -- -max_total_time=600

use s4_server::policy::Policy;
use s4_server::sigv4a::parse_authorization_header;

/// SigV4a Authorization header parser. Untrusted input arrives
/// from the listener as a raw header value; the parser is the
/// auth boundary so robustness here is load-bearing.
///
/// Property: parses to a `Result`, never panics, never loops
/// (parser is straight-line over a fixed `split(',')`).
#[test]
fn sigv4a_auth_header_bolero() {
    bolero::check!()
        .with_type::<String>()
        .for_each(|input: &String| {
            let _ = parse_authorization_header(input);
        });
}

/// IAM bucket-policy JSON parser. The v0.8.11 CRIT-5 fix added
/// `#[serde(deny_unknown_fields)]` so unsupported keywords like
/// `NotAction` fail closed; this fuzz target covers the broader
/// "any UTF-8 string is either rejected with a typed error OR
/// produces a valid `Policy`" contract.
///
/// Property: parses to a `Result`, never panics on the decoder
/// path. We deliberately accept that proptest-quality "valid
/// JSON" inputs may produce parse errors — what we don't accept
/// is a `SIGSEGV` / unwind / unbounded loop.
#[test]
fn policy_json_bolero() {
    bolero::check!()
        .with_type::<String>()
        .for_each(|input: &String| {
            let _ = Policy::from_json_str(input);
        });
}
