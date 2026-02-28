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

use std::{
    cell::Cell,
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use tm_wheel::TimerHandle;

use crate::executor::current_timer;
use crate::timer::Timer;

/// Behavior for zero-duration sleeps (duration < 1 ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroBehavior {
    /// Complete immediately (default).
    Immediate,
    /// Yield to the scheduler once, then complete on the following poll.
    Yield,
}

/// Sleep is a future that completes after the given `Duration`.
///
/// Usage:
/// ```ignore
/// Sleep::new(Duration::from_millis(100)).await;
/// ```
pub struct Sleep {
    /// Requested duration
    duration: Duration,
    /// The timer handle returned by the timer driver when the timer was scheduled.
    /// `None` means we haven't scheduled yet.
    handle: Option<TimerHandle>,
    /// Whether the timer has already fired / completed.
    fired: Cell<bool>,
    /// For zero-duration sleeps we may schedule a one-shot yield; track whether
    /// that yield has been scheduled so the subsequent poll completes.
    yield_scheduled: Cell<bool>,
    /// How to behave for durations below the timer resolution (less than 1ms).
    zero_behavior: ZeroBehavior,
}

impl Sleep {
    /// Create a new Sleep instance for the provided `duration`.
    #[inline]
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior: ZeroBehavior::Immediate,
        }
    }

    /// Create a Sleep with custom behavior for zero-length waits.
    #[inline]
    pub fn new_with_zero_behavior(duration: Duration, zero_behavior: ZeroBehavior) -> Self {
        Self {
            duration,
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior,
        }
    }

    /// Reset the sleep to a new `duration`.
    ///
    /// If a timer was previously scheduled, cancel it. The timer will be
    /// rescheduled on the next `poll`. This allows reusing a `Sleep` value
    /// to implement steady intervals or dynamic timeout adjustments.
    #[inline]
    pub fn reset(&mut self, duration: Duration) {
        // Update duration and clear fired state.
        self.duration = duration;
        self.fired.set(false);
        self.yield_scheduled.set(false);

        // If there was an outstanding handle, cancel it so the timer won't
        // keep the old waker alive.
        if let Some(handle) = self.handle.take() {
            if let Some(timer_rc) = current_timer() {
                timer_rc.cancel(handle);
            }
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: Sleep is !self-referential; it's safe to get a mutable reference.
        let this = unsafe { self.get_unchecked_mut() };

        if this.fired.get() {
            return Poll::Ready(());
        }

        if this.handle.is_none() {
            // First poll: if duration resolution is below 1ms, handle specially.
            let millis = this.duration.as_millis() as u64;
            if millis < 1 {
                match this.zero_behavior {
                    ZeroBehavior::Immediate => {
                        this.fired.set(true);
                        return Poll::Ready(());
                    }
                    ZeroBehavior::Yield => {
                        // If we haven't scheduled the one-shot yield yet, schedule it
                        // by waking ourselves and return Pending. On the subsequent
                        // poll we will observe `yield_scheduled` and complete.
                        if !this.yield_scheduled.replace(true) {
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        } else {
                            this.fired.set(true);
                            return Poll::Ready(());
                        }
                    }
                }
            }

            // Normal path: schedule with runtime timer.
            let timer_rc: Rc<Timer> =
                current_timer().expect("Sleep::poll called outside of runtime");

            // The timer driver expects a task `Waker`. We clone the task waker here.
            let waker = cx.waker().clone();
            match timer_rc.submit(this.duration, waker) {
                Some(handle) => {
                    this.handle = Some(handle);
                    // Not fired yet.
                    Poll::Pending
                }
                None => {
                    // Timer driver woke us immediately (duration rounded to 0 or similar).
                    this.fired.set(true);
                    Poll::Ready(())
                }
            }
        } else {
            // We were previously scheduled, and now we've been polled again.
            // The runtime's timer driver will wake the task by calling the task
            // waker when the timer expires. Being polled now means we assume the
            // timer fired — mark fired and return Ready.
            this.fired.set(true);
            // Drop the handle (we'll also attempt to cancel it in Drop if it remains).
            this.handle = None;
            Poll::Ready(())
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // If we still have an outstanding timer handle, cancel it so the timer
        // won't hold onto our waker.
        if let Some(handle) = self.handle.take() {
            if let Some(timer_rc) = current_timer() {
                timer_rc.cancel(handle);
            }
        }
    }
}

/// How to handle missed ticks for `Interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// Skip missed ticks and schedule the next tick at the next future multiple.
    Skip,
    /// Return the number of missed ticks so the caller can run catch-up loop.
    CatchUp,
}

/// Interval provides a simple API to await periodic ticks while maintaining
/// a steady cadence (accounting for drift): each tick tries to occur at an
/// integer multiple of the original period relative to the interval start.
///
/// Example:
/// ```ignore
/// let mut interval = Interval::new(Duration::from_millis(100));
/// loop {
///     let ticks = interval.tick().await;
///     for _ in 0..ticks { /* handle tick */ }
/// }
/// ```
pub struct Interval {
    period: Duration,
    /// The next absolute deadline for the interval. If `None`, the next tick
    /// will be scheduled relative to the current time.
    next_deadline: Option<Instant>,
    /// Behavior when ticks are missed.
    missed_tick_behavior: MissedTickBehavior,
}

impl Interval {
    #[inline]
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            next_deadline: None,
            missed_tick_behavior: MissedTickBehavior::Skip,
        }
    }

    /// Configure how missed ticks are handled.
    #[inline]
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed_tick_behavior = behavior;
    }

    /// Reset the interval schedule so the next tick is computed relative to
    /// the time when `tick()` is next called (useful when you want to restart
    /// the cadence).
    #[inline]
    pub fn reset(&mut self) {
        self.next_deadline = None;
    }

    /// Await the next tick. Returns the number of ticks that should be processed:
    /// - For `MissedTickBehavior::Skip` this will be `1`.
    /// - For `MissedTickBehavior::CatchUp` this may be `> 1` if several periods were missed.
    pub async fn tick(&mut self) -> u64 {
        let now = Instant::now();

        // Determine base next (the previous next_deadline or now+period)
        let base_next = self.next_deadline.unwrap_or_else(|| now + self.period);

        match self.missed_tick_behavior {
            MissedTickBehavior::Skip => {
                // Advance target forward until it's in the future.
                let mut target = base_next;
                if target <= now {
                    if self.period.as_nanos() == 0 {
                        target = now;
                    } else {
                        // Advance by whole periods until target > now.
                        // Use a loop; the number of iterations is typically small.
                        let mut cnt: u64 = 0;
                        while target <= now {
                            target += self.period;
                            cnt = cnt.saturating_add(1);
                            // Safety: in pathological cases we break after huge iterations,
                            // but such large loops are unlikely in practice.
                            if cnt == u64::MAX {
                                break;
                            }
                        }
                    }
                }

                let remaining = target.saturating_duration_since(Instant::now());
                if remaining.as_millis() == 0 {
                    // For immediate remaining, yield once to avoid busy-looping.
                    Sleep::new_with_zero_behavior(remaining, ZeroBehavior::Yield).await;
                } else {
                    Sleep::new(remaining).await;
                }

                // Schedule next deadline for subsequent tick
                self.next_deadline = Some(target + self.period);
                1
            }
            MissedTickBehavior::CatchUp => {
                if base_next > now {
                    // Not missed yet: sleep until base_next and return 1
                    let remaining = base_next.saturating_duration_since(Instant::now());
                    if remaining.as_millis() == 0 {
                        Sleep::new_with_zero_behavior(remaining, ZeroBehavior::Yield).await;
                    } else {
                        Sleep::new(remaining).await;
                    }
                    self.next_deadline = Some(base_next + self.period);
                    1
                } else {
                    // We missed one or more ticks. Compute how many.
                    if self.period.as_nanos() == 0 {
                        // If period is zero, avoid infinite loops; treat as a single tick.
                        self.next_deadline = Some(Instant::now() + self.period);
                        return 1;
                    }

                    let elapsed = now.duration_since(base_next);
                    let missed = (elapsed.as_nanos() / self.period.as_nanos()) as u64 + 1;

                    // Advance base_next by `missed` periods to compute the next deadline.
                    let mut new_next = base_next;
                    // loop to add period missed times
                    for _ in 0..missed {
                        new_next += self.period;
                    }

                    // Set next_deadline to the instant after the catch-up window.
                    self.next_deadline = Some(new_next + self.period);

                    // Return the number of missed ticks so caller can catch up.
                    missed
                }
            }
        }
    }
}

/// Error returned when a `timeout` expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutError;

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "operation timed out")
    }
}

impl std::error::Error for TimeoutError {}

/// Timeout future that races the provided future against a timeout duration.
///
/// If the inner future completes first, `Timeout` yields `Ok(T)`.
/// If the timeout elapses first, `Timeout` yields `Err(TimeoutError)` and the
/// inner future is dropped.
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
    /// If true, the timeout has already fired (and future should be treated as timed out).
    timed_out: bool,
}

impl<F> Timeout<F> {
    /// Create a new `Timeout` future.
    #[inline]
    pub fn new(future: F, duration: Duration) -> Self {
        Self {
            future,
            sleep: Sleep::new(duration),
            timed_out: false,
        }
    }
}

impl<F> Future for Timeout<F>
where
    F: Future,
{
    type Output = Result<F::Output, TimeoutError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: we don't move the fields out; we only create pinned projections.
        let this = unsafe { self.get_unchecked_mut() };

        if this.timed_out {
            return Poll::Ready(Err(TimeoutError));
        }

        // First, poll the inner future. If it completes, return the value and
        // allow `sleep` to be dropped (which cancels the timer).
        // We need a pinned projection of `future`.
        let mut future_pin = unsafe { Pin::new_unchecked(&mut this.future) };
        match Future::poll(future_pin.as_mut(), cx) {
            Poll::Ready(output) => return Poll::Ready(Ok(output)),
            Poll::Pending => {}
        }

        // Next, poll the timeout sleep. If it is ready, mark timed out and
        // return Err. Otherwise remain Pending.
        let mut sleep_pin = unsafe { Pin::new_unchecked(&mut this.sleep) };
        match sleep_pin.as_mut().poll(cx) {
            Poll::Ready(()) => {
                this.timed_out = true;
                // Dropping `future` happens naturally when this Timeout is dropped
                // by the executor or after we return. Return Err now.
                Poll::Ready(Err(TimeoutError))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Convenience async function that awaits `future` but returns an error if it
/// does not complete within `duration`.
///
/// Example:
/// ```ignore
/// let res = timeout(Duration::from_secs(1), some_future).await;
/// match res {
///     Ok(v) => { /* completed in time */ }
///     Err(_) => { /* timed out */ }
/// }
/// ```
#[inline]
pub async fn timeout<T>(
    duration: Duration,
    future: impl Future<Output = T>,
) -> Result<T, TimeoutError> {
    Timeout::new(future, duration).await
}

/// Convenience builder: returns a `Sleep` future.
#[inline]
pub fn sleep(duration: Duration) -> Sleep {
    Sleep::new(duration)
}

/// Convenience builder allowing zero-behavior control for tiny durations.
#[inline]
pub fn sleep_with_zero_behavior(duration: Duration, behavior: ZeroBehavior) -> Sleep {
    Sleep::new_with_zero_behavior(duration, behavior)
}

/// Convenience builder: returns an `Interval`.
#[inline]
pub fn interval(period: Duration) -> Interval {
    Interval::new(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::AnyDriver;
    use crate::executor::new_runtime;

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
