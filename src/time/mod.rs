//! A small `time` utility module for the custom async runtime.
//!
//! This module provides:
//! - `Sleep`: a Future that completes after a duration.
//! - `Interval`: a convenience type with a `tick().await` method to await periodic ticks.
//! - `timeout`: a function / `Timeout` future that races a future against a timeout.
//!
//! Implementation notes:
//! - The runtime's `Timer` driver (in `crate::timer`) is accessed through
//!   `crate::executor::current_timer()` which returns an `Rc<Timer>` when called
//!   from inside a runtime. Calling these time utilities outside a runtime will
//!   panic (matching the library's general behavior for runtime-only APIs).
//! - The `Timer` driver accepts a `Waker` and returns an optional `TimerHandle`.
//!   We store the `TimerHandle` and cancel it if the Sleep is dropped before firing.
//!
//! Caveat:
//! - The runtime's timer driver wakes the task's normal waker when the timer
//!   expires. We do not have a separate firing notification; therefore when a
//!   task is polled after scheduling a timeout/sleep we treat that poll as the
//!   timer having fired. This is the same model used in a number of simple
//!   runtimes and matches how the provided `Timer` driver integrates with the
//!   task waker. It means a poll triggered by something else (rare) could
//!   complete the sleep early; in practice this is uncommon for the intended
//!   use-cases in this project.

mod interval;
mod sleep;
mod timeout;

// Re-export public types and functions
pub use interval::{Interval, MissedTickBehavior};
pub use sleep::{Sleep, ZeroBehavior};
pub use timeout::{timeout, Timeout, TimeoutError};

/// Convenience builder: returns a `Sleep` future.
#[inline]
pub fn sleep(duration: std::time::Duration) -> Sleep {
    Sleep::new(duration)
}

/// Convenience builder allowing zero-behavior control for tiny durations.
#[inline]
pub fn sleep_with_zero_behavior(duration: std::time::Duration, behavior: ZeroBehavior) -> Sleep {
    Sleep::new_with_zero_behavior(duration, behavior)
}

/// Convenience builder: returns an `Interval`.
#[inline]
pub fn interval(period: std::time::Duration) -> Interval {
    Interval::new(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::AnyDriver;
    use crate::executor::new_runtime;
    use std::time::{Duration, Instant};

    #[test]
    fn sleep_completes() {
        let rt = new_runtime(AnyDriver::new_mock());
        rt.block_on(async {
            Sleep::new(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn timeout_expires() {
        let rt = new_runtime(AnyDriver::new_mock());
        let res = rt.block_on(async {
            let never = async {
                futures_util::future::pending::<()>().await;
            };
            timeout(Duration::from_millis(1), never).await
        });
        assert!(res.is_err());
    }

    #[test]
    fn timeout_succeeds_if_future_completes() {
        let rt = new_runtime(AnyDriver::new_mock());
        let res = rt.block_on(async { timeout(Duration::from_secs(1), async { 123usize }).await });
        assert_eq!(res, Ok(123usize));
    }

    #[test]
    fn interval_ticks_skip() {
        let rt = new_runtime(AnyDriver::new_mock());
        rt.block_on(async {
            let mut interval = Interval::new(Duration::from_millis(1));
            // two ticks should complete quickly
            let _ = interval.tick().await;
            let _ = interval.tick().await;
        });
    }

    #[test]
    fn interval_catchup_returns_multiple() {
        let rt = new_runtime(AnyDriver::new_mock());
        rt.block_on(async {
            let mut interval = Interval::new(Duration::from_millis(1));
            interval.set_missed_tick_behavior(MissedTickBehavior::CatchUp);
            // Initialise baseline
            let _ = interval.tick().await;
            // Artificially push next_deadline into the past to simulate missed ticks
            interval.next_deadline = Some(Instant::now() - Duration::from_millis(10));
            let missed = interval.tick().await;
            assert!(missed >= 1);
        });
    }
}
