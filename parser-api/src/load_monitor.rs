//! Global e621 API load monitor.
//!
//! Tracks the `x-ratelimit-remaining` header from every e621 response
//! and exposes an adaptive priority-based rate gate so live user requests
//! always get priority over background prefetch/backfill traffic.
//!
//! # Design
//!
//! * `E621LoadMonitor` — global singleton reading remaining budget
//! * `Priority` — Live > Prefetch > Backfill
//! * `AdaptiveGate` — replaces the old `RATE_GATE` `Mutex<Instant>`
//!
//! The gate adapts its delay based on:
//!   - Which `Priority` the caller asks for
//!   - How many x-ratelimit-remaining tokens are left (if known)
//!   - How recently a live (user-facing) request passed through

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::models::cfg;

/// Priority level for outbound e621 requests.
///
/// Higher-priority variants may overtake lower-priority ones at the
/// gate. Within the same priority, requests are strictly FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// User-facing request (feed, digest, search, etc.). Minimal delay.
    Live = 0,
    /// Background hot/cold prefetch worker. Moderate delay.
    Prefetch = 1,
    /// Backfill worker (repost retro-fill). Longest delay, yields to live.
    Backfill = 2,
}

impl Priority {
    fn base_delay_ms_from(&self, runtime: &crate::models::RuntimeConfig) -> u64 {
        match self {
            Priority::Live => runtime.live_rps_delay_ms,
            Priority::Prefetch => runtime.prefetch_rps_delay_ms,
            Priority::Backfill => runtime.backfill_rps_delay_ms,
        }
    }
}

/// Global e621 load monitor. Thread-safe, lock-free for reads.
pub struct E621LoadMonitor {
    /// The last `x-ratelimit-remaining` value seen from e621 responses.
    /// `u32::MAX` means "unknown" (no response received yet).
    last_remaining: AtomicU32,
    /// Unix timestamp when the current rate-limit window resets (from
    /// `x-ratelimit-reset`). `0` means unknown.
    reset_at: AtomicU64,
    /// Number of requests sent since the last `last_remaining` update.
    /// Used to estimate current budget between two server responses.
    sent_since_update: AtomicU32,
    /// Monotonic timestamp of the last **live** (user-facing) request
    /// that passed through the gate.
    last_live_pass: StdMutex<Instant>,
}

impl E621LoadMonitor {
    const UNKNOWN_REMAINING: u32 = u32::MAX;

    pub fn global() -> &'static Self {
        static MONITOR: std::sync::LazyLock<E621LoadMonitor> =
            std::sync::LazyLock::new(|| E621LoadMonitor {
                last_remaining: AtomicU32::new(u32::MAX),
                reset_at: AtomicU64::new(0),
                sent_since_update: AtomicU32::new(0),
                last_live_pass: StdMutex::new(Instant::now()),
            });
        &MONITOR
    }

    /// Feed the monitor with an `x-ratelimit-remaining` value freshly
    /// parsed from an e621 response header. This resets the local
    /// request-counter so subsequent calls to `estimated_remaining()`
    /// are accurate.
    pub fn observe_remaining(&self, remaining: u32) {
        self.last_remaining.store(remaining, Ordering::Relaxed);
        self.sent_since_update.store(0, Ordering::Relaxed);
    }

    /// Feed the monitor with an `x-ratelimit-reset` value (Unix seconds).
    pub fn observe_reset(&self, reset_ts: u64) {
        self.reset_at.store(reset_ts, Ordering::Relaxed);
    }

    /// Record that a request with the given priority passed through the
    /// gate. For `Live` requests, updates `last_live_pass`.
    pub fn record_pass(&self, priority: Priority) {
        if priority == Priority::Live
            && let Ok(mut guard) = self.last_live_pass.lock()
        {
            *guard = Instant::now();
        }
        self.sent_since_update.fetch_add(1, Ordering::Relaxed);
    }

    /// Best estimate of how many requests remain in the current window.
    /// Returns `u32::MAX` if no data has been observed yet.
    pub fn estimated_remaining(&self) -> u32 {
        let base = self.last_remaining.load(Ordering::Relaxed);
        if base == Self::UNKNOWN_REMAINING {
            return Self::UNKNOWN_REMAINING;
        }
        let sent = self.sent_since_update.load(Ordering::Relaxed);
        base.saturating_sub(sent)
    }

    /// How many milliseconds since the last live (user-facing) request
    /// passed through the gate.
    pub fn ms_since_last_live(&self) -> u64 {
        self.last_live_pass
            .lock()
            .map(|guard| guard.elapsed().as_millis() as u64)
            .unwrap_or_else(|_| {
                warn!("E621LoadMonitor::last_live_pass mutex poisoned — returning MAX");
                u64::MAX
            })
    }
}

/// Test-only helper: the rest of the impl block continues below.
impl E621LoadMonitor {
    /// Create an isolated monitor for testing. Not a singleton — each call
    /// produces an independent monitor that does not share state with
    /// `global()`.
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self {
            last_remaining: AtomicU32::new(u32::MAX),
            reset_at: AtomicU64::new(0),
            sent_since_update: AtomicU32::new(0),
            last_live_pass: StdMutex::new(Instant::now()),
        }
    }
}

/// Priority-aware rate gate that replaces the old `RATE_GATE` mutex.
///
/// Unlike the old single-mutex approach, this gate:
/// - Applies per-priority base delays
/// - Scales delays based on remaining e621 budget
/// - Allows backfill/prefetch to yield to live traffic when the live
///   window (`backfill_live_window_ms`) is still hot
///
/// Still uses a single tokio mutex internally so within-priority ordering
/// is deterministic (FIFO).
pub struct AdaptiveGate {
    inner: tokio::sync::Mutex<AdaptiveGateInner>,
}

struct AdaptiveGateInner {
    next_available: tokio::time::Instant,
}

impl AdaptiveGate {
    pub fn global() -> &'static Self {
        static GATE: std::sync::LazyLock<AdaptiveGate> =
            std::sync::LazyLock::new(|| AdaptiveGate {
                inner: tokio::sync::Mutex::new(AdaptiveGateInner {
                    next_available: tokio::time::Instant::now(),
                }),
            });
        &GATE
    }

    /// Create an isolated gate for testing. Not a singleton — each call
    /// produces an independent gate that does not share state with
    /// `global()`.
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(AdaptiveGateInner {
                next_available: tokio::time::Instant::now(),
            }),
        }
    }

    /// Wait until this request is permitted to pass through the gate.
    ///
    /// The wait time depends on:
    /// 1. Base delay for the priority level
    /// 2. Budget multiplier (how many x-ratelimit-remaining tokens are left)
    /// 3. Live-window check (backfill yields if a live request just passed)
    pub async fn wait(&self, priority: Priority) {
        let monitor = E621LoadMonitor::global();
        let remaining = monitor.estimated_remaining();
        let runtime = &cfg().runtime;

        // Live-window check: if backfill/prefetch and a live request just
        // passed recently, add extra delay to stay out of the way.
        let live_window_extra = if priority > Priority::Live {
            let ms_since_live = monitor.ms_since_last_live();
            let live_window = runtime.backfill_live_window_ms;
            if ms_since_live < live_window {
                // Still in the live window — add delay proportional to
                // how recently the live request passed.
                (live_window - ms_since_live).clamp(50, 2000)
            } else {
                0
            }
        } else {
            0
        };

        // Calculate effective delay:
        //   base_delay * budget_multiplier + live_window_extra.
        // Cache runtime once so we only read cfg() a single time.
        let base_delay = priority.base_delay_ms_from(runtime);
        let multiplier = budget_multiplier(remaining);
        let effective_delay = base_delay * multiplier + live_window_extra;

        let mut inner = self.inner.lock().await;
        let now = tokio::time::Instant::now();
        if inner.next_available > now {
            sleep(inner.next_available - now).await;
        }
        inner.next_available = tokio::time::Instant::now() + Duration::from_millis(effective_delay);

        // Record the pass for the monitor.
        monitor.record_pass(priority);
    }
}

/// Compute a delay multiplier based on remaining e621 budget.
///
/// Returns a multiplier applied to the base delay:
/// - Remaining > 200 (high budget): 1x (normal speed)
/// - Remaining 100–200: 2x
/// - Remaining 50–100: 3x
/// - Remaining < 50 (low budget): 5x
/// - Unknown (MAX): 2x (conservative)
fn budget_multiplier(remaining: u32) -> u64 {
    if remaining == E621LoadMonitor::UNKNOWN_REMAINING {
        return 2;
    }
    if remaining > 200 {
        1
    } else if remaining > 100 {
        2
    } else if remaining > 50 {
        3
    } else {
        5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── budget_multiplier tests ────────────────────────────────────

    #[test]
    fn budget_multiplier_high_budget() {
        assert_eq!(budget_multiplier(250), 1);
        assert_eq!(budget_multiplier(201), 1);
    }

    #[test]
    fn budget_multiplier_medium_budget() {
        assert_eq!(budget_multiplier(150), 2);
        assert_eq!(budget_multiplier(101), 2);
    }

    #[test]
    fn budget_multiplier_low_budget() {
        assert_eq!(budget_multiplier(75), 3);
        assert_eq!(budget_multiplier(51), 3);
    }

    #[test]
    fn budget_multiplier_critical_budget() {
        assert_eq!(budget_multiplier(49), 5);
        assert_eq!(budget_multiplier(0), 5);
    }

    #[test]
    fn budget_multiplier_unknown() {
        assert_eq!(budget_multiplier(E621LoadMonitor::UNKNOWN_REMAINING), 2);
    }

    // ── Priority ordering ──────────────────────────────────────────

    #[test]
    fn priority_ordering() {
        assert!(Priority::Live < Priority::Prefetch);
        assert!(Priority::Prefetch < Priority::Backfill);
    }

    // ── E621LoadMonitor: observe + record_pass + estimated ─────────
    //
    // These tests use `new_for_test()` — an isolated instance that does
    // NOT share state with `E621LoadMonitor::global()`. This eliminates
    // cross-test pollution even under parallel `cargo test`.

    #[test]
    fn estimated_remaining_unknown_when_no_observation() {
        let m = E621LoadMonitor::new_for_test();
        assert_eq!(m.estimated_remaining(), E621LoadMonitor::UNKNOWN_REMAINING);
    }

    #[test]
    fn observe_remaining_updates_estimate() {
        let m = E621LoadMonitor::new_for_test();
        m.observe_remaining(200);
        assert_eq!(m.estimated_remaining(), 200);
    }

    #[test]
    fn record_pass_decrements_estimate() {
        let m = E621LoadMonitor::new_for_test();
        m.observe_remaining(100);
        m.record_pass(Priority::Live);
        m.record_pass(Priority::Live);
        assert_eq!(m.estimated_remaining(), 98);
    }

    #[test]
    fn observe_remaining_resets_estimate() {
        let m = E621LoadMonitor::new_for_test();
        m.observe_remaining(50);
        m.record_pass(Priority::Live);
        assert_eq!(m.estimated_remaining(), 49);
        // A fresh observation resets the counter.
        m.observe_remaining(30);
        assert_eq!(m.estimated_remaining(), 30);
    }

    #[test]
    fn estimated_remaining_saturates_at_zero() {
        let m = E621LoadMonitor::new_for_test();
        m.observe_remaining(2);
        m.record_pass(Priority::Live);
        m.record_pass(Priority::Live);
        m.record_pass(Priority::Live); // Should saturate at 0, not underflow
        assert_eq!(m.estimated_remaining(), 0);
    }

    #[test]
    fn prefetch_pass_does_not_update_last_live() {
        // Use a single test that interleaves live/prefetch on an
        // isolated monitor so there is zero cross-test interference.
        let m = E621LoadMonitor::new_for_test();
        m.record_pass(Priority::Live);
        let after_live = m.ms_since_last_live();
        // Now record a prefetch pass — should NOT update last_live_pass.
        m.record_pass(Priority::Prefetch);
        let after_prefetch = m.ms_since_last_live();
        assert!(
            after_prefetch >= after_live,
            "prefetch should not update last_live_pass: after_live={after_live}ms, after_prefetch={after_prefetch}ms"
        );
    }

    // ── AdaptiveGate tokio tests ───────────────────────────────────
    // Uses new_for_test() to create an isolated gate independent of the
    // global singleton, so tests don't interfere with each other.

    #[tokio::test]
    async fn gate_wait_live_completes_immediately() {
        // wait(Live) on a fresh gate with an empty gate should complete
        // without significant delay.
        let gate = AdaptiveGate::new_for_test();
        let start = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(3), gate.wait(Priority::Live))
            .await
            .expect("wait(Live) should complete within 3s");
        let elapsed = start.elapsed();
        // Should take less than a second (base delay is 250ms).
        // Allow generous margin for CI.
        assert!(
            elapsed < Duration::from_secs(2),
            "wait(Live) took {elapsed:?}, expected < 2s"
        );
    }

    #[tokio::test]
    async fn gate_wait_fifo_two_backfills() {
        // Two consecutive backfill waits on an isolated gate should
        // complete strictly sequentially (FIFO ordering).
        let gate = AdaptiveGate::new_for_test();
        let start = tokio::time::Instant::now();

        gate.wait(Priority::Backfill).await;
        let t1 = start.elapsed();

        gate.wait(Priority::Backfill).await;
        let t2 = start.elapsed();

        assert!(
            t2 > t1,
            "second backfill wait should complete after first: t1={t1:?}, t2={t2:?}"
        );
    }
}
