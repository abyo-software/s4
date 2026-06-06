//! v0.8.18 P7: chaos / fault-injection test scaffold.
//!
//! Today this file is a placeholder establishing the test target
//! so CI runs it. Full chaos infrastructure — backend-method-level
//! fault injection across the 70+ `s3s::S3` trait methods, plus
//! tokio-test time / spawn-priority hooks for race scenarios —
//! lands in v0.8.19+ as a separate effort.
//!
//! The shape these tests will take, captured here so future
//! contributors don't reinvent the wheel:
//!
//! 1. Backend returns 5xx mid-stream
//! 2. Backend latency injection (slow HEAD / GET / PUT)
//! 3. SIGKILL mid-multipart (simulated via state-store drop)
//! 4. SSE-S4 keyring rotation during an in-flight PUT
//! 5. Concurrent overwrite of the same key (idempotency)
//!
//! The goal is **not** to assert specific timings or numbers —
//! it's to confirm the gateway never **silently corrupts data**
//! or panics when the backend / clock / disk behaves badly.

use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

/// v0.8.18 P7 smoke. Ensures the chaos test target compiles and
/// runs in CI; real scenarios populate the file as the
/// infrastructure matures.
///
/// v0.8.19 D-10: holds a concrete assertion (rather than the
/// silent no-op the v0.8.18 placeholder shipped) so a future
/// refactor that breaks `Bytes::from_static` or `AtomicU32`'s
/// type signature can't accidentally leave the placeholder
/// compiling-but-useless. The assert verifies the
/// `Ordering::Relaxed` load semantics on a freshly-constructed
/// counter — trivially true, but it's an actual signal that
/// `cargo test` exercises.
#[test]
fn chaos_scaffold_smoke() {
    use std::sync::atomic::Ordering;
    let bytes = Bytes::from_static(b"placeholder");
    let counter = Arc::new(AtomicU32::new(0));
    assert_eq!(bytes.len(), 11);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}
