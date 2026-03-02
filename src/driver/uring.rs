use std::any::Any;
use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::sync::Arc as StdArc;
use std::task::Waker;
use std::time::Duration;

use io_uring::types::{SubmitArgs, Timespec};
use io_uring::{opcode, squeue, types, IoUring};
use libc;
use mio::{Interest, Token};
use slab::Slab;

use crate::driver::Interruptor;
use crate::{
    driver::{Driver, RegistrationMode},
    fd_inner::InnerRawHandle,
    op::Op,
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

struct CompletionRegistration {
    inflight: bool,
    waiters: Vec<Waker>,
    completed: Option<i32>,
    /// Optional auxiliary storage provided by the operation for the duration
    /// of an io_uring submission. The driver keeps ownership while the
    /// operation is inflight and transfers it back to the Op before calling
    /// `complete`.
    aux: Option<Box<dyn Any>>,
}

impl Default for CompletionRegistration {
    fn default() -> Self {
        Self {
            inflight: false,
            waiters: Vec::new(),
            completed: None,
            aux: None,
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
    Completion(CompletionRegistration),
    Poll(PollRegistration),
}

struct DriverState {
    registrations: Slab<HandleRegistration>,
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
    pub(crate) fn new(entries: u32) -> Result<Self, io::Error> {
        // Create eventfd for interruption
        let eventfd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if eventfd < 0 {
            return Err(io::Error::last_os_error());
        }

        let ring = IoUring::new(entries)?;
        let ext_arg = ring.params().is_feature_ext_arg();
        let driver = Self {
            ring: RefCell::new(ring),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(entries as usize),
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
    fn push_waiter(waiters: &mut Vec<Waker>, waker: Waker) {
        if !waiters
            .last()
            .is_some_and(|waiter| waiter.will_wake(&waker))
        {
            waiters.push(waker);
        }
    }

    #[inline]
    fn encode_completion_key(token: Token) -> u64 {
        ((token.0 as u64) << KEY_KIND_BITS) | COMPLETION_KEY_KIND as u64
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
            sq.push(&entry).map_err(|_| {
                io::Error::new(ErrorKind::Other, "io_uring submission queue is full")
            })?;
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
    fn submit_completion_entry<O>(&self, op: &mut O, waker: Waker) -> Result<O::Output, io::Error>
    where
        O: Op,
    {
        let token = op.token();
        // No fixed upper bound on completion kind codes anymore — registration stores
        // completion state dynamically per-kind.
        let key = Self::encode_completion_key(token);

        // First, check whether a completion result is already available or if an
        // operation is inflight; update registration state/waiters accordingly.
        {
            let mut state = self.state.borrow_mut();
            let registration = state.registrations.get_mut(token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("I/O token {} is not registered with this driver", token.0),
                )
            })?;

            let HandleRegistration::Completion(registration) = registration else {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "I/O token {} is registered for poll mode, not completion mode",
                        token.0
                    ),
                ));
            };

            // If a completion has already been collected for this registration,
            // transfer any stored auxiliary data back into the operation and
            // return the completion result synchronously.
            if let Some(result) = registration.completed.take() {
                // Take the stored auxiliary data (if any) and give it back to the op.
                let aux = registration.aux.take();
                op.take_completion_storage(aux);
                return op.complete(result);
            }

            if registration.inflight {
                Self::push_waiter(&mut registration.waiters, waker);
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "An I/O completion is pending",
                ));
            }

            // Mark inflight and register the caller's waker.
            registration.inflight = true;
            Self::push_waiter(&mut registration.waiters, waker);
        }

        // Build the SQE and optionally receive auxiliary storage the operation
        // wants the driver to own while the I/O is inflight.
        let (entry, storage) = op.build_completion_entry(key).map_err(|e| e)?;

        // Push the SQE into the submission queue. If this fails, undo the inflight
        // flag and clear waiters on the registration.
        if let Err(err) = self.push_entry(entry) {
            let mut state = self.state.borrow_mut();
            if let Some(HandleRegistration::Completion(registration)) =
                state.registrations.get_mut(token.0)
            {
                registration.inflight = false;
                registration.waiters.clear();
                // Discard any provided storage since the submission failed.
                registration.aux = None;
            }
            return Err(err);
        }

        // Store auxiliary storage (if any) into the registration so it remains
        // alive for the duration of the inflight operation.
        if storage.is_some() {
            let mut state = self.state.borrow_mut();
            if let Some(HandleRegistration::Completion(registration)) =
                state.registrations.get_mut(token.0)
            {
                registration.aux = storage;
            } else {
                // Shouldn't happen, but if it does, drop the storage.
            }
        }

        Err(io::Error::new(
            ErrorKind::WouldBlock,
            "An I/O completion is pending",
        ))
    }

    #[inline]
    fn collect_completions(
        &self,
        wait_for_one: bool,
        timeout: Option<Duration>,
    ) -> Result<bool, io::Error> {
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
                            let entry = opcode::Timeout::new(timespec_ptr)
                                .build()
                                .user_data(u64::MAX - 1);

                            let mut sq = ring.submission();
                            // Safety: timespec would be saved into the I/O driver itself
                            let _ = unsafe { sq.push(&entry) };
                            drop(sq);

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

        let mut woke_any = false;
        let mut interrupt = false;
        let mut timeout = false;

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
                    timeout = true;
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

                if let Some(HandleRegistration::Completion(registration)) =
                    state.registrations.get_mut(token.0)
                {
                    registration.completed = Some(result);
                    registration.inflight = false;
                    if !registration.waiters.is_empty() {
                        woke_any = true;
                    }
                    for waiter in registration.waiters.drain(..) {
                        waiter.wake();
                    }
                }
            }
        }

        if interrupt {
            self.submit_interrupt();
        }

        if interrupt || timeout {
            woke_any = true;
        }

        Ok(woke_any)
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
    fn submit<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        let token = op.token();
        {
            let state = self.state.borrow();
            match state.registrations.get(token.0) {
                Some(HandleRegistration::Poll(_)) => (),
                Some(HandleRegistration::Completion(_)) => {
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

        match op.execute() {
            Ok(output) => Ok(output),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
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
                    let desired_mask = Self::interest_to_poll_mask(op.interest());
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
                entry.insert(HandleRegistration::Completion(
                    CompletionRegistration::default(),
                ));
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
            Some(HandleRegistration::Completion(_)) => Ok(()),
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
    fn submit_completion<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.submit_completion_entry(&mut op, waker)
    }
}
