use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::task::Waker;
use std::time::Duration;

use mio::{Events, Interest, Poll, Registry, Token, Waker as MioWaker};
use slab::Slab;

use crate::{driver::Driver, fd_inner::InnerRawHandle, op::Op};

struct Registration {
    fd: RawFd,
    waiter: Option<Waker>,
}

struct DriverState {
    registrations: Slab<Registration>,
}

pub struct MioDriver {
    poll: RefCell<Poll>,
    registry: Registry,
    events: RefCell<Events>,
    state: RefCell<DriverState>,
    to_wake: RefCell<Vec<Waker>>,
    waker: RefCell<MioWaker>,
}

impl MioDriver {
    #[inline]
    pub(crate) fn new() -> Result<Self, io::Error> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = MioWaker::new(&registry, Token(usize::MAX))?;

        Ok(Self {
            poll: RefCell::new(poll),
            registry,
            events: RefCell::new(Events::with_capacity(1024)),
            state: RefCell::new(DriverState {
                registrations: Slab::new(),
            }),
            to_wake: RefCell::new(Vec::with_capacity(1024)),
            waker: RefCell::new(waker),
        })
    }

    #[inline]
    fn update_waiter(waiter_slot: &mut Option<Waker>, waker: Waker) {
        if !waiter_slot
            .as_ref()
            .is_some_and(|waiter| waiter.will_wake(&waker))
        {
            *waiter_slot = Some(waker);
        }
    }

    #[inline]
    pub(crate) fn wait_timeout(&self, timeout: Option<Duration>) {
        let mut poll = self.poll.borrow_mut();
        let mut events = self.events.borrow_mut();
        poll.poll(&mut events, timeout)
            .expect("mio poll failed while waiting for I/O events");

        let mut to_wake = Vec::new();
        {
            let mut reusable = self.to_wake.borrow_mut();
            std::mem::swap(&mut to_wake, &mut *reusable);
        }
        to_wake.clear();

        {
            let mut state = self.state.borrow_mut();
            for event in events.iter() {
                // Check if this is an interrupt event
                if event.token().0 == usize::MAX {
                    continue;
                }

                if let Some(registration) = state.registrations.get_mut(event.token().0) {
                    if let Some(task) = registration.waiter.take() {
                        to_wake.push(task);
                    }
                }
            }
        }

        drop(events);
        drop(poll);

        for waker in to_wake.drain(..) {
            waker.wake();
        }

        let mut reusable = self.to_wake.borrow_mut();
        std::mem::swap(&mut to_wake, &mut *reusable);
    }
}

impl Driver for MioDriver {
    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        self.wait_timeout(timeout);
    }

    #[inline]
    fn interrupt(&self) {
        // Write a byte to the interrupt pipe to wake up the poll
        let _ = self.waker.borrow().wake();
    }

    #[inline]
    fn submit<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        let token = op.token();
        let result = op.execute();

        match result {
            Ok(output) => Ok(output),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                let mut state = self.state.borrow_mut();
                let registration = state.registrations.get_mut(token.0).ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::NotFound,
                        format!("I/O token {} is not registered with this driver", token.0),
                    )
                })?;
                Self::update_waiter(&mut registration.waiter, waker);
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        let token = {
            let mut state = self.state.borrow_mut();
            let entry = state.registrations.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Registration {
                fd: handle.handle,
                waiter: None,
            });
            token
        };

        let mut source = mio::unix::SourceFd(&handle.handle);
        if let Err(err) = self.registry.register(&mut source, token, interest) {
            let mut state = self.state.borrow_mut();
            let _ = state.registrations.try_remove(token.0);
            return Err(err);
        }

        Ok(token)
    }

    #[inline]
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let fd = {
            let state = self.state.borrow();
            let registration = state.registrations.get(handle.token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "I/O token {} is not registered with this driver",
                        handle.token.0
                    ),
                )
            })?;
            registration.fd
        };

        let mut source = mio::unix::SourceFd(&fd);
        self.registry
            .reregister(&mut source, handle.token, interest)
    }

    #[inline]
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let fd = {
            let state = self.state.borrow();
            let registration = state.registrations.get(handle.token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "I/O token {} is not registered with this driver",
                        handle.token.0
                    ),
                )
            })?;
            registration.fd
        };

        let mut source = mio::unix::SourceFd(&fd);
        self.registry.deregister(&mut source)?;

        let mut state = self.state.borrow_mut();
        let _ = state.registrations.try_remove(handle.token.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use mio::{Token, Waker};

    use super::{MioDriver, Registration};
    use crate::{driver::Driver, op::Op};

    struct TestWake {
        count: AtomicUsize,
    }

    impl TestWake {
        #[inline]
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }

        #[inline]
        fn wake_count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl std::task::Wake for TestWake {
        #[inline]
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }

        #[inline]
        fn wake_by_ref(self: &Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct SuccessOp {
        token: Token,
        value: usize,
    }

    impl Op for SuccessOp {
        type Output = usize;

        #[inline]
        fn token(&self) -> Token {
            self.token
        }

        #[inline]
        fn execute(&mut self) -> Result<Self::Output, io::Error> {
            Ok(self.value)
        }
    }

    struct PendingOp {
        token: Token,
    }

    impl Op for PendingOp {
        type Output = ();

        #[inline]
        fn token(&self) -> Token {
            self.token
        }

        #[inline]
        fn execute(&mut self) -> Result<Self::Output, io::Error> {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "operation is not ready",
            ))
        }
    }

    #[test]
    fn submit_executes_operation_and_returns_output() {
        let driver = MioDriver::new().expect("mio driver should initialize");
        let wake = Arc::new(TestWake::new());
        let waker = std::task::Waker::from(wake.clone());

        let output = driver
            .submit(
                SuccessOp {
                    token: Token(0),
                    value: 42,
                },
                waker,
            )
            .expect("operation should succeed");

        assert_eq!(output, 42);
        assert_eq!(wake.wake_count(), 0);
    }

    #[test]
    fn submit_would_block_arms_task() {
        let driver = MioDriver::new().expect("mio driver should initialize");
        let wake = Arc::new(TestWake::new());
        let waker = std::task::Waker::from(wake.clone());

        let token = {
            let mut state = driver.state.borrow_mut();
            Token(state.registrations.insert(Registration {
                fd: 0,
                waiter: None,
            }))
        };
        {
            let state = driver.state.borrow();
            assert!(state.registrations.contains(token.0));
        }

        let err = driver
            .submit(PendingOp { token }, waker)
            .expect_err("operation should be pending");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(wake.wake_count(), 0);

        let state = driver.state.borrow();
        let registration = state
            .registrations
            .get(token.0)
            .expect("token should still be registered");
        assert!(registration.waiter.is_some());
    }

    #[test]
    fn wait_wakes_task_for_ready_token() {
        let driver = MioDriver::new().expect("mio driver should initialize");
        let wake = Arc::new(TestWake::new());
        let waker = std::task::Waker::from(wake.clone());

        let token = {
            let mut state = driver.state.borrow_mut();
            Token(state.registrations.insert(Registration {
                fd: 0,
                waiter: Some(waker),
            }))
        };
        {
            let state = driver.state.borrow();
            assert!(state.registrations.contains(token.0));
        }

        let poll = driver.poll.borrow();
        let waker = Waker::new(poll.registry(), token).expect("test waker should register");
        drop(poll);
        waker.wake().expect("test waker should wake poll");

        driver.wait(None);
        assert_eq!(wake.wake_count(), 1);
    }
}
