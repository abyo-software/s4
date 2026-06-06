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
#[test]
fn chaos_scaffold_smoke() {
    // Touch the building blocks future scenarios will use, so
    // the placeholder isn't `cfg(unused)` and doesn't get
    // stripped on the first refactor.
    let _bytes = Bytes::from_static(b"placeholder");
    let _counter = Arc::new(AtomicU32::new(0));
}
