use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::sync::Arc as StdArc;
use std::task::Waker;
use std::time::Duration;

use io_uring::types::{SubmitArgs, Timespec};
use io_uring::{opcode, squeue, types, IoUring};
use mio::{Interest, Token};
use slab::Slab;

use crate::driver::{CompletionIoResult, Interruptor};
use crate::{
    driver::{Driver, RegistrationMode},
    fd_inner::InnerRawHandle,
};

const KEY_KIND_BITS: u64 = 1;
const KEY_KIND_MASK: u64 = (1u64 << KEY_KIND_BITS) - 1;
const POLL_KEY_KIND: u8 = 0;
const COMPLETION_KEY_KIND: u8 = 1;

pub struct UringInterruptor {
    eventfd: std::sync::Weak<RawFd>,
}

impl Interruptor for UringInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(eventfd) = self.eventfd.upgrade() {
            // Write to the eventfd to wake up the driver
            let value: u64 = 1;
            let _ = unsafe {
                libc::write(
                    *eventfd,
                    &value as *const u64 as *const std::ffi::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
        }
    }
}

struct PollRegistration {
    fd: RawFd,
    poll_mask: u32,
    waiter: Option<Waker>,
    poll_armed: bool,
}

enum HandleRegistration {
    Completion,
    Poll(PollRegistration),
}

struct Completion {
    waiter: Option<Waker>,
    completed: Option<i32>,
    ignored_data: Option<Box<dyn std::any::Any>>,
}

struct DriverState {
    registrations: Slab<HandleRegistration>,
    completions: Slab<Completion>,
}

pub struct UringDriver {
    ring: RefCell<IoUring>,
    state: RefCell<DriverState>,
    interrupt_eventfd: Option<StdArc<RawFd>>,
    interrupt_buffer: RefCell<[u8; 8]>,
    ext_arg: bool,
    timespec: RefCell<Option<Timespec>>,
}

impl Drop for UringDriver {
    fn drop(&mut self) {
        if let Some(eventfd) = self.interrupt_eventfd.take() {
            // Close eventfd
            unsafe { libc::close(*eventfd) };
        }
    }
}

impl UringDriver {
    #[inline]
    pub(crate) fn new(entries: u32, builder: io_uring::Builder) -> Result<Self, io::Error> {
        // Create eventfd for interruption
        let eventfd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if eventfd < 0 {
            return Err(io::Error::last_os_error());
        }

        let ring = builder.build(entries)?;
        let ext_arg = ring.params().is_feature_ext_arg();
        let driver = Self {
            ring: RefCell::new(ring),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(entries as usize),
                completions: Slab::with_capacity(entries as usize),
            }),
            interrupt_eventfd: Some(StdArc::new(eventfd)),
            interrupt_buffer: RefCell::new([0; 8]),
            ext_arg,
            timespec: RefCell::new(None),
        };

        driver.submit_interrupt();

        Ok(driver)
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
    fn encode_completion_key(token: usize) -> u64 {
        ((token as u64) << KEY_KIND_BITS) | COMPLETION_KEY_KIND as u64
    }

    #[inline]
    fn encode_poll_key(token: Token) -> u64 {
        ((token.0 as u64) << KEY_KIND_BITS) | POLL_KEY_KIND as u64
    }

    #[inline]
    fn decode_token(key: u64) -> Token {
        Token((key >> KEY_KIND_BITS) as usize)
    }

    #[inline]
    fn decode_key_kind(key: u64) -> u8 {
        (key & KEY_KIND_MASK) as u8
    }

    #[inline]
    fn interest_to_poll_mask(interest: Interest) -> u32 {
        let mut mask = 0;
        if interest.is_readable() {
            mask |= libc::POLLIN as u32;
        }
        if interest.is_writable() {
            mask |= libc::POLLOUT as u32;
        }
        mask
    }

    #[inline]
    fn submitter_call_result(result: Result<usize, io::Error>) -> Result<(), io::Error> {
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::ETIME) => Ok(()), // io_uring Timeout
            Err(err) => Err(err),
        }
    }

    #[inline]
    fn push_entry(&self, entry: squeue::Entry) -> Result<(), io::Error> {
        let mut ring = self.ring.borrow_mut();

        if ring.submission().is_full() {
            Self::submitter_call_result(ring.submit())?;
        }

        let mut sq = ring.submission();
        unsafe {
            sq.push(&entry)
                .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
        }

        Ok(())
    }

    #[inline]
    fn push_poll_add(&self, token: Token, fd: RawFd, poll_mask: u32) -> Result<(), io::Error> {
        let entry = opcode::PollAdd::new(types::Fd(fd), poll_mask)
            .build()
            .user_data(Self::encode_poll_key(token));
        self.push_entry(entry)
    }

    #[inline]
    fn collect_completions(
        &self,
        wait_for_one: bool,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        {
            let mut ring = self.ring.borrow_mut();
            let should_submit = if wait_for_one {
                true
            } else {
                !ring.submission().is_empty()
            };

            if should_submit {
                let submit_result = if wait_for_one {
                    if let Some(timeout) = timeout {
                        if self.ext_arg {
                            // Linux 5.11+
                            ring.submitter()
                                .submit_with_args(1, &SubmitArgs::new().timespec(&timeout.into()))
                        } else {
                            // Linux 5.4+
                            let timespec = timeout.into();
                            let timespec_ptr = &timespec as *const Timespec;
                            self.timespec.borrow_mut().replace(timespec);
                            let _ = self.push_entry(
                                opcode::Timeout::new(timespec_ptr)
                                    .build()
                                    .user_data(u64::MAX - 1),
                            );

                            ring.submit_and_wait(1)
                        }
                    } else {
                        ring.submit_and_wait(1)
                    }
                } else {
                    ring.submit()
                };
                Self::submitter_call_result(submit_result)?;
            }
        }

        let mut interrupt = false;

        {
            let mut ring = self.ring.borrow_mut();
            let mut state = self.state.borrow_mut();
            let cq = ring.completion();

            for cqe in cq {
                let key = cqe.user_data();
                let result = cqe.result();

                if key == u64::MAX {
                    // Task interrupted
                    interrupt = true;
                    continue;
                } else if key == u64::MAX - 1 {
                    // Timeout (Linux <5.10)
                    continue;
                }

                let token = Self::decode_token(key);
                let key_kind = Self::decode_key_kind(key);

                if key_kind == POLL_KEY_KIND {
                    if let Some(HandleRegistration::Poll(registration)) =
                        state.registrations.get_mut(token.0)
                    {
                        registration.poll_armed = false;
                        if let Some(waiter) = registration.waiter.take() {
                            waiter.wake();
                        }
                    }
                    continue;
                }

                if let Some(completion) = state.completions.get_mut(token.0) {
                    completion.completed = Some(result);
                    if let Some(waiter) = completion.waiter.take() {
                        waiter.wake();
                    }
                    if completion.ignored_data.is_some() {
                        state.completions.remove(token.0);
                    }
                }
            }
        }

        if interrupt {
            self.submit_interrupt();
        }

        Ok(())
    }

    #[inline]
    fn submit_interrupt(&self) {
        use io_uring::{opcode, types};
        // Submit a read operation to the eventfd to wake up the driver
        let mut buffer = self.interrupt_buffer.borrow_mut();
        let _ = self.push_entry(
            opcode::Read::new(
                types::Fd(
                    *self
                        .interrupt_eventfd
                        .as_ref()
                        .expect("interrupt_eventfd is not initialized")
                        .as_ref(),
                ),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
            .build()
            .user_data(u64::MAX),
        );
    }
}

impl Driver for UringDriver {
    type Interruptor = UringInterruptor;

    #[inline]
    fn flush(&self) {
        match self.collect_completions(false, None) {
            Ok(_) => {}
            Err(err) => panic!("io_uring submit failed while processing I/O completions: {err}"),
        }
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        match self.collect_completions(true, timeout) {
            Ok(_) => {}
            Err(err) => panic!("io_uring submit_and_wait failed while waiting for I/O: {err}"),
        }
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        UringInterruptor {
            eventfd: StdArc::downgrade(
                self.interrupt_eventfd
                    .as_ref()
                    .expect("interrupt_eventfd is not initialized"),
            ),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        self.register_handle_with_mode(handle, interest, RegistrationMode::Completion)
    }

    #[inline]
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        let mut state = self.state.borrow_mut();
        let entry = state.registrations.vacant_entry();
        let token = Token(entry.key());

        match mode {
            RegistrationMode::Completion => {
                entry.insert(HandleRegistration::Completion);
            }
            RegistrationMode::Poll => {
                entry.insert(HandleRegistration::Poll(PollRegistration {
                    fd: handle.handle,
                    poll_mask: Self::interest_to_poll_mask(interest),
                    waiter: None,
                    poll_armed: false,
                }));
            }
        }

        Ok(token)
    }

    #[inline]
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let mut state = self.state.borrow_mut();
        match state.registrations.get_mut(handle.token.0) {
            Some(HandleRegistration::Completion) => Ok(()),
            Some(HandleRegistration::Poll(registration)) => {
                registration.poll_mask = Self::interest_to_poll_mask(interest);
                Ok(())
            }
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            )),
        }
    }

    #[inline]
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let mut state = self.state.borrow_mut();
        if state.registrations.try_remove(handle.token.0).is_none() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            ));
        }

        Ok(())
    }

    #[inline]
    fn supports_completion(&self) -> bool {
        true
    }

    #[inline]
    fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let token = handle.token();
        {
            let state = self.state.borrow();
            match state.registrations.get(token.0) {
                Some(HandleRegistration::Poll(_)) => (),
                Some(HandleRegistration::Completion) => {
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!(
                            "I/O token {} is registered for completion mode, not poll mode",
                            token.0
                        ),
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!("I/O token {} is not registered with this driver", token.0),
                    ));
                }
            }
        }

        let poll_spec = {
            let mut state = self.state.borrow_mut();
            let registration = state.registrations.get_mut(token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("I/O token {} is not registered with this driver", token.0),
                )
            })?;

            let HandleRegistration::Poll(registration) = registration else {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "I/O token {} is registered for completion mode, not poll mode",
                        token.0
                    ),
                ));
            };

            Self::update_waiter(&mut registration.waiter, waker);
            let desired_mask = Self::interest_to_poll_mask(interest);
            registration.poll_mask = desired_mask;

            if registration.poll_armed {
                None
            } else {
                registration.poll_armed = true;
                Some((registration.fd, desired_mask))
            }
        };

        if let Some((fd, poll_mask)) = poll_spec {
            if let Err(submit_err) = self.push_poll_add(token, fd, poll_mask) {
                let mut state = self.state.borrow_mut();
                if let Some(HandleRegistration::Poll(registration)) =
                    state.registrations.get_mut(token.0)
                {
                    registration.poll_armed = false;
                    registration.waiter = None;
                }
                return Err(submit_err);
            }
        }

        Ok(())
    }

    #[inline]
    fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> super::CompletionIoResult
    where
        O: crate::op::Op,
    {
        let mut state = self.state.borrow_mut();
        let vacant_completion = state.completions.vacant_entry();
        let token = vacant_completion.key();

        // Build the SQE. If this fails, return the error.
        let entry = match op.build_completion_entry(Self::encode_completion_key(token)) {
            Ok(entry) => entry,
            Err(err) => return CompletionIoResult::SubmitErr(err),
        };

        // Push the SQE into the submission queue. If this fails, undo the inflight
        // flag and clear waiters on the registration.
        if let Err(err) = self.push_entry(entry) {
            return CompletionIoResult::SubmitErr(err);
        }

        // Store the operation in the completions slab.
        vacant_completion.insert(Completion {
            waiter: Some(waker),
            completed: None,
            ignored_data: None,
        });

        CompletionIoResult::Retry(token)
    }

    #[inline]
    fn get_completion_result(&self, token: usize) -> Option<i32> {
        let mut state = self.state.borrow_mut();
        let completed = state.completions.get(token).and_then(|c| c.completed);
        if completed.is_some() {
            state.completions.remove(token);
        }
        completed
    }

    #[inline]
    fn set_completion_waker(&self, token: usize, waker: Waker) {
        let mut state = self.state.borrow_mut();
        if let Some(c) = state.completions.get_mut(token) {
            c.waiter = Some(waker);
        }
    }

    #[inline]
    fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        let mut state = self.state.borrow_mut();
        if let Some(c) = state.completions.get_mut(token) {
            c.ignored_data = Some(data);
        }
    }
}
