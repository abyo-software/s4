//! v0.8.4 #77 (audit H-8): graceful recovery from `RwLock` / `Mutex` poisoning
//! across every state-manager module.
//!
//! ## Why
//!
//! A panic inside a write-guarded section poisons the lock. The default Rust
//! behaviour (`.expect()` / `.unwrap()`) is to propagate the poison via a
//! second panic on the **next** access, which will turn a benign in-memory
//! state-manager hiccup (one map insertion that panicked) into a process
//! death the next time anyone calls `to_json` (e.g. the SIGUSR1 dump-back
//! hook in `main.rs`) — and the signal handler taking the gateway down with
//! it loses every other in-memory snapshot the operator was hoping to dump.
//!
//! For our state managers (versioning, object_lock, tagging, replication,
//! notifications, lifecycle, inventory, mfa, cors, multipart_state) the
//! poisoned data is virtually always still **usable** — the panic just left
//! it in an intermediate but well-typed state (the inserts are
//! `HashMap<_, _>::insert`, the values are owned `Clone` types, so a
//! mid-write panic at most leaves one entry under-written, never produces
//! a corrupted graph). Recovering + logging is therefore strictly safer
//! than re-panicking.
//!
//! ## What
//!
//! - [`recover_read`] / [`recover_write`] for `RwLock<T>`
//! - [`recover_mutex`] for `Mutex<T>`
//!
//! Each:
//! 1. Tries the normal `.read()` / `.write()` / `.lock()`.
//! 2. On `Err(PoisonError)`, emits a `tracing::warn!` line tagged with the
//!    caller-supplied `name` (= `"<manager>.<field>"`) so operators can
//!    grep for poisoned-lock recoveries in production logs.
//! 3. Bumps the `s4_lock_poison_recovery_total{lock,kind}` Prometheus
//!    counter (one tick per recovery, so the rate dashboard shows poisoned-
//!    lock pressure).
//! 4. Returns the inner guard via [`std::sync::PoisonError::into_inner`] —
//!    same lock, same data, just acknowledged-poison.
//!
//! ## Limitations
//!
//! - This module does **not** unpoison the lock (Rust's stdlib has no
//!   stable API for that). Subsequent acquisitions will hit the
//!   `PoisonError` path again and bump the counter — that's intentional;
//!   operators want to see the post-poison rate, not have it silently
//!   reset on first recovery.
//! - The wrappers don't catch panics that originate **outside** the
//!   guarded section. If the panic happens before `.write()` returns,
//!   nothing is poisoned (lock state is consistent). If it happens
//!   inside the guarded mutation, the lock is poisoned and the next
//!   guard hand-off goes through this recovery path.
//! - For `Mutex<T>` specifically, attempting to re-`lock()` from the
//!   **same thread** that originally panicked still propagates the
//!   poison (we forward via `into_inner`); the production scenario is
//!   always cross-thread (a worker thread panicked, the SIGUSR1 dump
//!   thread observes the poison), which this helper handles cleanly.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Acquire a read guard on `lock`, recovering from poison.
///
/// `name` is a static label of the form `"<manager>.<field>"` (e.g.
/// `"versioning.index"`) used for the WARN log + Prometheus label.
pub fn recover_read<'a, T: ?Sized>(
    lock: &'a RwLock<T>,
    name: &'static str,
) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(
                lock = name,
                "RwLock poisoned; reading recovered data (v0.8.4 #77)"
            );
            crate::metrics::record_lock_poison_recovery(name, "read");
            poisoned.into_inner()
        }
    }
}

/// Acquire a write guard on `lock`, recovering from poison.
///
/// `name` is a static label of the form `"<manager>.<field>"` (e.g.
/// `"replication.statuses"`) used for the WARN log + Prometheus label.
pub fn recover_write<'a, T: ?Sized>(
    lock: &'a RwLock<T>,
    name: &'static str,
) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(
                lock = name,
                "RwLock poisoned; recovering inner state (v0.8.4 #77)"
            );
            crate::metrics::record_lock_poison_recovery(name, "write");
            poisoned.into_inner()
        }
    }
}

/// Acquire a `Mutex` guard, recovering from poison. Used by the few
/// manager-internal `Mutex<T>` sites (the dedup `HashSet` in
/// `replication::warn_lock_propagation_skipped` + the in-test capture
/// vectors).
pub fn recover_mutex<'a, T: ?Sized>(lock: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    match lock.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(
                lock = name,
                "Mutex poisoned; recovering inner state (v0.8.4 #77)"
            );
            crate::metrics::record_lock_poison_recovery(name, "mutex");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recover_read_after_write_panic_returns_inner_data() {
        let lock = Arc::new(RwLock::new(vec![1u32, 2, 3]));
        let lock_cl = Arc::clone(&lock);
        // Poison the lock by panicking inside a write guard.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = lock_cl.write().expect("clean lock");
            g.push(4);
            panic!("force-poison");
        }));
        assert!(lock.is_poisoned(), "panic inside write must poison the lock");

        // recover_read must NOT panic + must surface the post-mutation state.
        let g = recover_read(&lock, "test.recover_read");
        assert_eq!(*g, vec![1, 2, 3, 4]);
    }

    #[test]
    fn recover_write_after_write_panic_allows_continued_writes() {
        let lock = Arc::new(RwLock::new(0u32));
        let lock_cl = Arc::clone(&lock);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = lock_cl.write().expect("clean lock");
            *g = 42;
            panic!("force-poison");
        }));
        assert!(lock.is_poisoned());

        {
            let mut g = recover_write(&lock, "test.recover_write");
            *g = 100;
        }
        let g = recover_read(&lock, "test.recover_read_post");
        assert_eq!(*g, 100);
    }

    #[test]
    fn recover_mutex_after_lock_panic_returns_inner() {
        let lock = Arc::new(Mutex::new(String::from("alpha")));
        let lock_cl = Arc::clone(&lock);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = lock_cl.lock().expect("clean lock");
            g.push_str("-beta");
            panic!("force-poison");
        }));
        assert!(lock.is_poisoned());

        let g = recover_mutex(&lock, "test.recover_mutex");
        assert_eq!(&*g, "alpha-beta");
    }
}
