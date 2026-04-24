// Pedantic lints are noisy in this small atomic-gymnastics module and
// don't flag real issues. Quiet them locally.
#![allow(
    clippy::map_unwrap_or,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::doc_markdown
)]

//! Per-provider circuit breaker.
//!
//! The [`CircuitBreaker`] tracks consecutive 5xx/timeout failures for one
//! provider. Once `failure_threshold` failures accumulate within
//! `window`, the circuit opens and `allow()` returns `false` for
//! `open_duration`. After the quiet period the breaker transitions to
//! `HalfOpen` — the next call is admitted; success closes the circuit,
//! failure re-opens it.
//!
//! Hooks into Elena's cascade-escalation path: when a provider's breaker
//! is open, the router picks the next-tier provider for the session.
//! Integration sits in [`crate::router::ModelRouter`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation; calls proceed.
    Closed,
    /// Opened after the failure threshold; calls short-circuit to error
    /// until `open_until`.
    Open,
    /// One call is being probed to see if the provider recovered.
    HalfOpen,
}

impl From<u8> for CircuitState {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Open,
            2 => Self::HalfOpen,
            _ => Self::Closed,
        }
    }
}

const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// A circuit breaker for one logical provider.
///
/// Cheap to clone — everything is behind an `Arc` + atomics so two loop
/// steps on the same provider share state without contention.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Consecutive failures observed.
    failures: AtomicU32,
    /// State as raw `u8` (see constants above).
    state: AtomicU8,
    /// Monotonic millis at which the circuit re-closes (if `Open`).
    open_until_ms: AtomicI64,
    /// How many consecutive failures trip the breaker.
    failure_threshold: u32,
    /// How long the circuit stays open before moving to HalfOpen.
    open_duration_ms: u64,
}

impl CircuitBreaker {
    /// Build a new breaker.
    ///
    /// `failure_threshold` consecutive failures open the circuit. It stays
    /// open for `open_duration`, then the next call is admitted as
    /// `HalfOpen` — success closes, failure re-opens.
    #[must_use]
    pub fn new(failure_threshold: u32, open_duration: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                failures: AtomicU32::new(0),
                state: AtomicU8::new(CLOSED),
                open_until_ms: AtomicI64::new(0),
                failure_threshold: failure_threshold.max(1),
                open_duration_ms: u64::try_from(open_duration.as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }

    /// Current state. Advances Open → HalfOpen if the open window elapsed.
    pub fn state(&self) -> CircuitState {
        let raw = self.inner.state.load(Ordering::Acquire);
        if raw == OPEN {
            let until = self.inner.open_until_ms.load(Ordering::Acquire);
            if now_ms() >= until {
                // Transition to HalfOpen — one probe call is allowed.
                let _ = self.inner.state.compare_exchange(
                    OPEN,
                    HALF_OPEN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                return CircuitState::HalfOpen;
            }
        }
        CircuitState::from(raw)
    }

    /// True iff a call is allowed right now. Side effect: transitions
    /// `Open` → `HalfOpen` when the window elapses.
    pub fn allow(&self) -> bool {
        !matches!(self.state(), CircuitState::Open)
    }

    /// Record a successful call. Resets the failure count; transitions
    /// `HalfOpen` → `Closed`.
    pub fn record_success(&self) {
        self.inner.failures.store(0, Ordering::Release);
        let _ = self.inner.state.compare_exchange(
            HALF_OPEN,
            CLOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Record a failure. Opens the circuit if the threshold is hit, or
    /// re-opens if it fires during `HalfOpen`.
    pub fn record_failure(&self) {
        let state = CircuitState::from(self.inner.state.load(Ordering::Acquire));
        if state == CircuitState::HalfOpen {
            // Re-open immediately.
            self.open_now();
            return;
        }
        let prev = self.inner.failures.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= self.inner.failure_threshold {
            self.open_now();
        }
    }

    fn open_now(&self) {
        let until =
            now_ms().saturating_add(i64::try_from(self.inner.open_duration_ms).unwrap_or(i64::MAX));
        self.inner.open_until_ms.store(until, Ordering::Release);
        self.inner.state.store(OPEN, Ordering::Release);
        self.inner.failures.store(0, Ordering::Release);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_breaker_allows() {
        let b = CircuitBreaker::new(3, Duration::from_millis(50));
        assert!(b.allow());
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn opens_after_threshold_failures() {
        let b = CircuitBreaker::new(2, Duration::from_millis(50));
        b.record_failure();
        assert!(b.allow(), "still closed after 1 failure");
        b.record_failure();
        assert!(!b.allow(), "open after 2 failures");
        assert_eq!(b.state(), CircuitState::Open);
    }

    #[test]
    fn success_resets_failure_count() {
        let b = CircuitBreaker::new(3, Duration::from_millis(50));
        b.record_failure();
        b.record_failure();
        b.record_success();
        b.record_failure();
        assert!(b.allow(), "one failure after a success should not open the circuit");
    }

    #[test]
    fn transitions_to_half_open_after_window() {
        let b = CircuitBreaker::new(1, Duration::from_millis(20));
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_success_closes_the_circuit() {
        let b = CircuitBreaker::new(1, Duration::from_millis(10));
        b.record_failure();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_success();
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_failure_reopens_the_circuit() {
        let b = CircuitBreaker::new(1, Duration::from_millis(10));
        b.record_failure();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
    }
}
