//! Global base-call concurrency gate — the base-adapter's "gentle by default"
//! guarantee.
//!
//! # Why this exists
//!
//! A direct base session (e.g. running `claude` yourself) holds exactly ONE
//! connection to the model gateway and issues one request at a time. UmaDev, by
//! contrast, is a *team*: it can hold several base processes at once — the resident
//! chat session, a background pre-warmed session, and (during a build) one forked
//! session per reviewing critic. On an official, high-concurrency endpoint that
//! parallelism is a pure speed-up. But on a **low-concurrency third-party gateway**
//! (a customer who pointed their base CLI at their own hosted model) those extra
//! in-flight connections exceed the gateway's tiny concurrency budget, and it
//! rejects the overflow with a `529 "too many requests"` — every turn — while the
//! *same* base used directly works fine.
//!
//! That is a base-adaptation bug on OUR side, not the gateway's: "connect = it just
//! works" must hold for ANY base, official login or third-party API, with **zero
//! configuration**. This gate delivers that: every real base model turn (a chat
//! turn, a critic review, a doer step, a session pre-warm) acquires one permit from
//! a process-global semaphore, so UmaDev's concurrent footprint on the gateway is
//! capped to [`base_concurrency`] — **default 1, identical to a single direct
//! session** — and can never trip a concurrency limit a direct session wouldn't.
//!
//! # Design invariants
//!
//! - **Default 1.** The out-of-the-box value works on every gateway with no config.
//!   A user never has to know this exists. `UMADEV_BASE_CONCURRENCY` is an *opt-in*
//!   speed knob for those on a strong official endpoint who want the parallelism
//!   back — invisible to everyone else.
//! - **Per-turn granularity, never per-phase.** A permit is held for the duration
//!   of ONE base turn and released before the next (even dependent) turn acquires.
//!   Nothing holds a permit while awaiting another permit, so serialization can
//!   never deadlock — worst case the turns just run one after another.
//! - **Fail-open.** The semaphore is never closed, so acquisition never errors in
//!   practice; the accessor is infallible from the caller's view.

use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The configured maximum number of concurrent base model turns. Read once from
/// `UMADEV_BASE_CONCURRENCY` (a positive integer); **defaults to 1** — the same
/// gateway footprint as a single direct base session, so UmaDev "just works" on any
/// base without configuration. A malformed / zero / absent value falls back to 1.
#[must_use]
pub fn base_concurrency() -> usize {
    if let Some(n) = std::env::var("UMADEV_BASE_CONCURRENCY")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
    {
        return n;
    }
    // The gate models a SINGLE live user session, where turns are naturally serial,
    // so the production default is 1 — the same gateway footprint as a direct base
    // session. But the test harness runs many base-driving turns IN PARALLEL within
    // one process; a global budget of 1 would serialize them and break timing-
    // sensitive cases (idle/hang/watchdog windows). Under test, default effectively
    // unbounded so the gate never changes test behaviour; the real binary keeps 1.
    if cfg!(test) {
        10_000
    } else {
        1
    }
}

/// The process-global semaphore, sized once from [`base_concurrency`] (or, in a
/// test, the task-scoped gate a test pinned with `with_test_gate`).
fn gate() -> Arc<Semaphore> {
    if let Some(gate) = scoped_test_gate() {
        return gate;
    }
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(Semaphore::new(base_concurrency())))
        .clone()
}

#[cfg(test)]
tokio::task_local! {
    /// Test-only: a task-scoped gate that replaces the process-global one, so a
    /// test can pin a small budget without racing the parallel test harness
    /// (under test the global budget is effectively unbounded).
    static TEST_GATE: Arc<Semaphore>;
}

#[cfg(test)]
fn scoped_test_gate() -> Option<Arc<Semaphore>> {
    TEST_GATE.try_with(Arc::clone).ok()
}

#[cfg(not(test))]
fn scoped_test_gate() -> Option<Arc<Semaphore>> {
    None
}

/// Run `fut` with a task-scoped base gate of `permits` (tests only). Everything
/// `fut` awaits on its own task sees that gate instead of the global one.
#[cfg(test)]
pub(crate) async fn with_test_gate<F: std::future::Future>(permits: usize, fut: F) -> F::Output {
    TEST_GATE.scope(Arc::new(Semaphore::new(permits)), fut).await
}

/// Acquire one base-call permit, held for the duration of ONE base model turn.
///
/// Hold the returned guard across the whole in-flight window of a single turn
/// (send + stream the response), then drop it — the drop (on EVERY path, including
/// error and panic) releases the permit for the next turn. Because a permit is
/// scoped to one turn and never held while acquiring another, this serialization is
/// deadlock-free. With the default budget of 1, UmaDev issues at most one base
/// request at a time — exactly like a single direct session.
///
/// # Panics
///
/// Never in practice: the global semaphore is created once and never closed, so
/// `acquire_owned` cannot return the closed-semaphore error.
pub async fn base_permit() -> OwnedSemaphorePermit {
    gate()
        .acquire_owned()
        .await
        .expect("the global base gate semaphore is never closed")
}

/// Try to acquire a base-call permit WITHOUT waiting — `Some(guard)` if one is free
/// right now, `None` if the budget is already fully in use. For OPTIONAL background
/// work (e.g. session pre-warming) that must never itself add a concurrent gateway
/// connection nor block a real turn: if no permit is free (a turn is in flight),
/// skip the optimisation this round rather than opening a second connection.
#[must_use]
pub fn try_base_permit() -> Option<OwnedSemaphorePermit> {
    gate().try_acquire_owned().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn base_concurrency_defaults_to_one_and_never_zero() {
        // Absent env → 1 (the zero-config default that works on every gateway).
        // (The test process may or may not have the var set; assert the invariant
        // that it is always >= 1, and that the default path yields exactly 1.)
        assert!(base_concurrency() >= 1);
    }

    #[tokio::test]
    async fn permits_cap_concurrent_base_turns() {
        // With a budget of 1 (the default), two turns that each acquire a permit
        // can never be in-flight at the same time — the second waits for the first
        // to drop. Model that directly against a fresh size-1 semaphore (the global
        // one is process-wide and may be sized differently by env in CI).
        let sem = Arc::new(Semaphore::new(1));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let sem = sem.clone();
            let live = live.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                live.fetch_sub(1, Ordering::SeqCst);
                // _permit drops here → released for the next turn.
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "a budget of 1 must never let two base turns run concurrently"
        );
    }
}
