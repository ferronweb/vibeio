use std::{
    cell::RefCell,
    task::Waker,
    time::{Duration, Instant},
};

use tm_wheel::TimerHandle;

pub struct Timer {
    // The timer wheel will overflow after ~139 years of waiting,
    // but running for that long is very unlikely.
    wheel: RefCell<tm_wheel::TimerDriver<Waker, 7, 64>>,
    instant: RefCell<Instant>,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            wheel: RefCell::new(tm_wheel::TimerDriver::new(slab::Slab::new())),
            instant: RefCell::new(Instant::now()),
        }
    }

    pub fn submit(&self, duration: Duration, waker: Waker) -> Option<TimerHandle> {
        let millis = duration.as_millis() as u64;
        if millis < 1 {
            // If the duration is less than 1 millisecond, wake the task immediately
            // One tick is equivalent to 1 millisecond.
            waker.wake();
            return None;
        }
        let mut wheel = self.wheel.borrow_mut();
        let now = wheel.now();
        Some(wheel.insert(waker, now + millis))
    }

    pub fn cancel(&self, handle: TimerHandle) {
        let mut wheel = self.wheel.borrow_mut();
        wheel.remove(handle);
    }

    pub fn spin_and_get_deadline(&self) -> (Option<Duration>, bool) {
        let mut instant = self.instant.borrow_mut();
        let mut wheel = self.wheel.borrow_mut();
        let mut woken_up = false;
        if !wheel.is_empty() {
            // Advance the timer wheel, but only if not empty
            for waker in wheel.advance(instant.elapsed().as_millis() as u64) {
                // Wake the pending tasks
                waker.wake();
                woken_up = true;
            }
        }

        *instant = Instant::now();

        // From the source code of tm-wheel, it seems like the deadline is absolute:
        //   return NonZeroU64::new(now + bucket_offset * Self::time_units(level));
        // So we need to subtract the current time to get the relative deadline
        let now = wheel.now();
        (
            wheel
                .nearest_wakeup()
                .map(|deadline| Duration::from_millis(deadline.get() - now)),
            woken_up,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::{RawWaker, RawWakerVTable, Waker};
    use std::time::Duration;

    fn mock_waker(counter: Arc<Mutex<u32>>) -> Waker {
        fn clone(data: *const ()) -> RawWaker {
            RawWaker::new(data, &VTABLE)
        }
        fn wake(data: *const ()) {
            let counter = unsafe { &*(data as *const Mutex<u32>) };
            let mut lock = counter.lock().unwrap();
            *lock += 1;
        }
        fn wake_by_ref(data: *const ()) {
            wake(data)
        }
        fn drop(_: *const ()) {}

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        let ptr = Arc::into_raw(counter) as *const ();
        unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
    }

    #[test]
    fn test_timer_submit_and_deadline() {
        let timer = Timer::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        // Submit a timer for 50 ms
        let _handle = timer.submit(Duration::from_millis(50), waker);

        // The deadline should be Some and close to 50 ms
        let (deadline, _woken_up) = timer.spin_and_get_deadline();
        assert!(deadline.is_some());
        let ms = deadline.unwrap().as_millis();
        assert!(ms <= 50 && ms > 0, "Deadline should be <= 50ms, got {}", ms);
    }

    #[test]
    fn test_timer_cancel() {
        let timer = Timer::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        // Submit a timer and then cancel it
        let handle = timer
            .submit(Duration::from_millis(100), waker)
            .expect("Failed to submit timer");
        timer.cancel(handle);

        // After cancel, deadline should be None
        let (deadline, _woken_up) = timer.spin_and_get_deadline();
        assert!(deadline.is_none());
    }

    #[test]
    fn test_timer_wake() {
        let timer = Timer::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        // Submit a timer for 1 ms
        let _handle = timer.submit(Duration::from_millis(1), waker);

        // Wait a bit and spin the wheel
        std::thread::sleep(Duration::from_millis(5));
        timer.spin_and_get_deadline();

        // The waker should have been called
        let count = counter.lock().unwrap();
        assert!(
            *count >= 1,
            "Waker should have been called or at least not panic"
        );
    }
}
