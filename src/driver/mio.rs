use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::sync::Mutex;
use std::task::Waker;

use mio::{Events, Interest, Poll, Token};

use crate::{driver::Driver, fd_inner::InnerRawHandle, op::Op};

struct Registration {
    waiter: Option<Waker>,
}

struct DriverState {
    registrations: HashMap<Token, Registration>,
    next_token: usize,
}

pub struct MioDriver {
    poll: Mutex<Poll>,
    events: Mutex<Events>,
    state: Mutex<DriverState>,
}

impl MioDriver {
    pub(crate) fn new() -> Result<Self, io::Error> {
        let poll = Poll::new()?;

        Ok(Self {
            poll: Mutex::new(poll),
            events: Mutex::new(Events::with_capacity(1024)),
            state: Mutex::new(DriverState {
                registrations: HashMap::new(),
                next_token: 0,
            }),
        })
    }

    fn allocate_token(state: &mut DriverState) -> Result<Token, io::Error> {
        for _ in 0..usize::MAX {
            let token = Token(state.next_token);
            state.next_token = state.next_token.wrapping_add(1);

            if !state.registrations.contains_key(&token) {
                return Ok(token);
            }
        }

        Err(io::Error::new(
            ErrorKind::Other,
            "mio token space exhausted for I/O registrations",
        ))
    }
}

impl Driver for MioDriver {
    fn wait(&self) {
        let mut poll = self.poll.lock().expect("mio poll mutex is poisoned");
        let mut events = self.events.lock().expect("mio events mutex is poisoned");

        poll.poll(&mut events, None)
            .expect("mio poll failed while waiting for I/O events");

        let mut to_wake = Vec::new();
        {
            let mut state = self.state.lock().expect("mio state mutex is poisoned");
            for event in events.iter() {
                if let Some(registration) = state.registrations.get_mut(&event.token()) {
                    if let Some(task) = registration.waiter.take() {
                        to_wake.push(task);
                    }
                }
            }
        }
        events.clear();

        for waker in to_wake {
            waker.wake();
        }
    }

    fn submit<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        let token = op.token();
        let result = op.execute();

        match result {
            Ok(output) => Ok(output),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                let mut state = self.state.lock().expect("mio state mutex is poisoned");
                let registration = state.registrations.get_mut(&token).ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::NotFound,
                        format!("I/O token {} is not registered with this driver", token.0),
                    )
                })?;
                registration.waiter = Some(waker);
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        let token = {
            let mut state = self.state.lock().expect("mio state mutex is poisoned");
            let token = Self::allocate_token(&mut state)?;
            state
                .registrations
                .insert(token, Registration { waiter: None });
            token
        };

        let poll = self.poll.lock().expect("mio poll mutex is poisoned");
        let mut source = mio::unix::SourceFd(&handle.handle);
        if let Err(err) = poll.registry().register(&mut source, token, interest) {
            let mut state = self.state.lock().expect("mio state mutex is poisoned");
            state.registrations.remove(&token);
            return Err(err);
        }

        Ok(token)
    }

    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        {
            let state = self.state.lock().expect("mio state mutex is poisoned");
            if !state.registrations.contains_key(&handle.token) {
                return Err(io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "I/O token {} is not registered with this driver",
                        handle.token.0
                    ),
                ));
            }
        }

        let poll = self.poll.lock().expect("mio poll mutex is poisoned");
        let mut source = mio::unix::SourceFd(&handle.handle);
        poll.registry()
            .reregister(&mut source, handle.token, interest)
    }

    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let poll = self.poll.lock().expect("mio poll mutex is poisoned");
        let mut source = mio::unix::SourceFd(&handle.handle);
        poll.registry().deregister(&mut source)?;

        let mut state = self.state.lock().expect("mio state mutex is poisoned");
        state.registrations.remove(&handle.token);
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
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }

        fn wake_count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl std::task::Wake for TestWake {
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }

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

        fn token(&self) -> Token {
            self.token
        }

        fn execute(&mut self) -> Result<Self::Output, io::Error> {
            Ok(self.value)
        }
    }

    struct PendingOp {
        token: Token,
    }

    impl Op for PendingOp {
        type Output = ();

        fn token(&self) -> Token {
            self.token
        }

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

        let token = Token(7);
        {
            let mut state = driver.state.lock().expect("mio state mutex is poisoned");
            state
                .registrations
                .insert(token, Registration { waiter: None });
        }

        let err = driver
            .submit(PendingOp { token }, waker)
            .expect_err("operation should be pending");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(wake.wake_count(), 0);

        let state = driver.state.lock().expect("mio state mutex is poisoned");
        let registration = state
            .registrations
            .get(&token)
            .expect("token should still be registered");
        assert!(registration.waiter.is_some());
    }

    #[test]
    fn wait_wakes_task_for_ready_token() {
        let driver = MioDriver::new().expect("mio driver should initialize");
        let wake = Arc::new(TestWake::new());
        let waker = std::task::Waker::from(wake.clone());

        let token = Token(9);
        {
            let mut state = driver.state.lock().expect("mio state mutex is poisoned");
            state.registrations.insert(
                token,
                Registration {
                    waiter: Some(waker),
                },
            );
        }

        let poll = driver.poll.lock().expect("mio poll mutex is poisoned");
        let waker = Waker::new(poll.registry(), token).expect("test waker should register");
        drop(poll);
        waker.wake().expect("test waker should wake poll");

        driver.wait();
        assert_eq!(wake.wake_count(), 1);
    }
}
