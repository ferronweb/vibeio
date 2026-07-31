//! Sleep future and zero-duration behavior configuration.

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use crate::executor::current_timer;
use crate::timer::{Timer, TimerHandle};

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
    /// The absolute deadline when this sleep should complete.
    deadline: Instant,
    /// The timer used to schedule the sleep.
    timer: Option<Rc<Timer>>,
}

impl Sleep {
    /// Create a new Sleep instance for the provided `duration`.
    #[inline]
    pub fn new(duration: Duration) -> Self {
        Self {
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior: ZeroBehavior::Immediate,
            deadline: Instant::now() + duration,
            timer: None,
        }
    }

    /// Create a Sleep with custom behavior for zero-length waits.
    #[inline]
    pub fn new_with_zero_behavior(duration: Duration, zero_behavior: ZeroBehavior) -> Self {
        Self {
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior,
            deadline: Instant::now() + duration,
            timer: None,
        }
    }

    /// Create a Sleep that completes at the specified absolute `deadline`.
    ///
    /// This is a convenience constructor that computes the relative duration
    /// from `Instant::now()` to the provided `deadline`.
    #[inline]
    pub fn sleep_until(deadline: Instant) -> Self {
        Self {
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior: ZeroBehavior::Immediate,
            deadline,
            timer: None,
        }
    }

    /// Reset the sleep to a new absolute `deadline` (`Instant`).
    ///
    /// If a timer was previously scheduled, cancel it. The timer will be
    /// rescheduled on the next `poll`. This allows reusing a `Sleep` value
    /// to implement steady intervals or dynamic timeout adjustments using
    /// absolute deadlines instead of relative durations.
    #[inline]
    pub fn reset(&mut self, deadline: Instant) {
        // Compute relative duration until deadline and update state.
        self.deadline = deadline;
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

    /// Handles a submission that was woken immediately by the timer driver
    /// (duration rounded to zero or similar).
    fn handle_immediate_wakeup(&self, cx: &mut Context<'_>) -> Poll<()> {
        match self.zero_behavior {
            ZeroBehavior::Immediate => {
                self.fired.set(true);
                Poll::Ready(())
            }
            ZeroBehavior::Yield => {
                // If we haven't scheduled the one-shot yield yet, schedule it
                // by waking ourselves and return Pending. On the subsequent
                // poll we will observe `yield_scheduled` and complete.
                if !self.yield_scheduled.replace(true) {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    self.fired.set(true);
                    Poll::Ready(())
                }
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

        let timer_rc = this
            .timer
            .get_or_insert_with(|| current_timer().expect("Sleep::poll called outside of runtime"));

        if this.handle.is_none() {
            // First poll: schedule the timer. The timer driver expects a task
            // `Waker`; we clone the task waker here.
            let waker = cx.waker().clone();
            return match timer_rc.submit(this.deadline, waker) {
                Some(handle) => {
                    this.handle = Some(handle);
                    Poll::Pending
                }
                None => this.handle_immediate_wakeup(cx),
            };
        }

        // We were previously scheduled, and now we've been polled again.
        // The runtime's timer driver will wake the task by calling the task
        // waker when the timer expires.

        // Check if the deadline has actually been reached.
        if Instant::now() >= this.deadline {
            this.fired.set(true);
            // Drop the handle (we'll also attempt to cancel it in Drop if it remains).
            this.handle = None;
            return Poll::Ready(());
        }

        // Spurious wakeup: the timer might have woken us up early, or another
        // waker woke the task. Re-register the waker to ensure we get woken up
        // again.
        let handle = match this.handle.take() {
            Some(handle) => handle,
            None => return Poll::Pending,
        };
        let Some(timer_rc) = current_timer() else {
            return Poll::Pending;
        };
        timer_rc.cancel(handle);
        let waker = cx.waker().clone();
        match timer_rc.submit(this.deadline, waker) {
            Some(handle) => {
                this.handle = Some(handle);
                Poll::Pending
            }
            None => {
                // Timer driver woke us immediately (duration rounded to 0 or similar).
                this.fired.set(true);
                Poll::Ready(())
            }
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // If we still have an outstanding timer handle, cancel it so the timer
        // won't hold onto our waker.
        if let Some(handle) = self.handle.take() {
            if let Some(timer_rc) = self.timer.take() {
                timer_rc.cancel(handle);
            }
        }
    }
}
