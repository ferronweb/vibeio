use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::task::Waker;

use io_uring::{opcode, squeue, types, IoUring};
use mio::{Interest, Token};
use parking_lot::Mutex;

use crate::{
    driver::{Driver, RegistrationMode},
    fd_inner::InnerRawHandle,
    op::{CompletionKind, Op},
};

const KEY_KIND_BITS: u64 = 8;
const KEY_KIND_MASK: u64 = (1u64 << KEY_KIND_BITS) - 1;
const POLL_KEY_KIND: u8 = 0;

struct PendingRegistration {
    waiters: Vec<Waker>,
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

struct DriverState {
    registrations: HashMap<Token, HandleRegistration>,
    next_token: usize,
    pending: HashMap<u64, PendingRegistration>,
    completed: HashMap<u64, i32>,
}

pub struct UringDriver {
    ring: Mutex<IoUring>,
    state: Mutex<DriverState>,
}

impl UringDriver {
    pub(crate) fn new(entries: u32) -> Result<Self, io::Error> {
        Ok(Self {
            ring: Mutex::new(IoUring::new(entries)?),
            state: Mutex::new(DriverState {
                registrations: HashMap::new(),
                next_token: 0,
                pending: HashMap::new(),
                completed: HashMap::new(),
            }),
        })
    }

    fn encode_completion_key(token: Token, kind: CompletionKind) -> u64 {
        ((token.0 as u64) << KEY_KIND_BITS) | (kind as u8 as u64)
    }

    fn encode_poll_key(token: Token) -> u64 {
        ((token.0 as u64) << KEY_KIND_BITS) | POLL_KEY_KIND as u64
    }

    fn decode_token(key: u64) -> Token {
        Token((key >> KEY_KIND_BITS) as usize)
    }

    fn decode_key_kind(key: u64) -> u8 {
        (key & KEY_KIND_MASK) as u8
    }

    fn io_uring_would_block_error(kind: CompletionKind) -> io::Error {
        io::Error::new(
            ErrorKind::WouldBlock,
            format!("{kind} completion is pending"),
        )
    }

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

    fn operation_poll_mask(kind: Option<CompletionKind>, fallback_mask: u32) -> u32 {
        match kind {
            Some(CompletionKind::Accept) | Some(CompletionKind::Read) => libc::POLLIN as u32,
            Some(CompletionKind::Connect) | Some(CompletionKind::Write) => libc::POLLOUT as u32,
            None => fallback_mask,
        }
    }

    fn submitter_call_result(result: Result<usize, io::Error>) -> Result<(), io::Error> {
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn push_entry(&self, entry: squeue::Entry) -> Result<(), io::Error> {
        let mut ring = self.ring.lock();

        let pushed = {
            let mut sq = ring.submission();
            sq.sync();
            let pushed = unsafe { sq.push(&entry).is_ok() };
            sq.sync();
            pushed
        };

        if !pushed {
            Self::submitter_call_result(ring.submit())?;

            let mut sq = ring.submission();
            sq.sync();
            unsafe {
                sq.push(&entry).map_err(|_| {
                    io::Error::new(ErrorKind::Other, "io_uring submission queue is full")
                })?;
            }
            sq.sync();
        }

        Self::submitter_call_result(ring.submit())
    }

    fn push_poll_add(&self, token: Token, fd: RawFd, poll_mask: u32) -> Result<(), io::Error> {
        let entry = opcode::PollAdd::new(types::Fd(fd), poll_mask)
            .build()
            .user_data(Self::encode_poll_key(token));
        self.push_entry(entry)
    }

    fn submit_completion_entry<O>(&self, op: &mut O, waker: Waker) -> Result<O::Output, io::Error>
    where
        O: Op,
    {
        let kind = op.completion_kind().ok_or_else(|| {
            io::Error::new(
                ErrorKind::Unsupported,
                "operation does not support completion-based submission",
            )
        })?;
        let token = op.token();
        let key = Self::encode_completion_key(token, kind);

        {
            let mut state = self.state.lock();
            match state.registrations.get(&token) {
                Some(HandleRegistration::Completion) => {}
                Some(HandleRegistration::Poll(_)) => {
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!(
                            "I/O token {} is registered for poll mode, not completion mode",
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

            if let Some(result) = state.completed.remove(&key) {
                return op.complete(result);
            }

            if let Some(pending) = state.pending.get_mut(&key) {
                pending.waiters.push(waker);
                return Err(Self::io_uring_would_block_error(kind));
            }

            state.pending.insert(
                key,
                PendingRegistration {
                    waiters: vec![waker],
                },
            );
        }

        let entry = op.build_completion_entry(key)?;
        if let Err(err) = self.push_entry(entry) {
            let mut state = self.state.lock();
            state.pending.remove(&key);
            return Err(err);
        }

        Err(Self::io_uring_would_block_error(kind))
    }

    fn collect_completions(&self, wait_for_one: bool) -> Result<Vec<Waker>, io::Error> {
        {
            let ring = self.ring.lock();
            let submit_result = if wait_for_one {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            };
            Self::submitter_call_result(submit_result)?;
        }

        let completions = {
            let mut ring = self.ring.lock();
            let mut cq = ring.completion();
            cq.sync();
            cq.map(|cqe| (cqe.user_data(), cqe.result()))
                .collect::<Vec<_>>()
        };

        if completions.is_empty() {
            return Ok(Vec::new());
        }

        let mut to_wake = Vec::new();
        let mut state = self.state.lock();
        for (key, result) in completions {
            let token = Self::decode_token(key);
            let key_kind = Self::decode_key_kind(key);

            if key_kind == POLL_KEY_KIND {
                if let Some(HandleRegistration::Poll(registration)) =
                    state.registrations.get_mut(&token)
                {
                    registration.poll_armed = false;
                    if let Some(waiter) = registration.waiter.take() {
                        to_wake.push(waiter);
                    }
                }
                let _ = result;
                continue;
            }

            if !matches!(
                state.registrations.get(&token),
                Some(HandleRegistration::Completion)
            ) {
                continue;
            }

            state.completed.insert(key, result);
            if let Some(pending) = state.pending.remove(&key) {
                to_wake.extend(pending.waiters);
            }
        }

        Ok(to_wake)
    }

    fn clear_handle_operations(state: &mut DriverState, token: Token) {
        state
            .pending
            .retain(|key, _| Self::decode_token(*key) != token);
        state
            .completed
            .retain(|key, _| Self::decode_token(*key) != token);
    }

    fn has_inflight_io(state: &DriverState) -> bool {
        if !state.pending.is_empty() {
            return true;
        }

        state.registrations.values().any(|registration| {
            matches!(
                registration,
                HandleRegistration::Poll(PollRegistration {
                    poll_armed: true,
                    ..
                })
            )
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
            "io_uring token space exhausted for I/O registrations",
        ))
    }
}

impl Driver for UringDriver {
    fn wait(&self) {
        let mut woke_any = false;
        match self.collect_completions(false) {
            Ok(wakers) => {
                if !wakers.is_empty() {
                    woke_any = true;
                }
                for waker in wakers {
                    waker.wake();
                }
            }
            Err(err) => panic!("io_uring submit failed while processing I/O completions: {err}"),
        }

        if woke_any {
            return;
        }

        {
            let state = self.state.lock();
            if !Self::has_inflight_io(&state) {
                return;
            }
        }

        match self.collect_completions(true) {
            Ok(wakers) => {
                for waker in wakers {
                    waker.wake();
                }
            }
            Err(err) => panic!("io_uring submit_and_wait failed while waiting for I/O: {err}"),
        }
    }

    fn submit<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        let token = op.token();
        {
            let state = self.state.lock();
            match state.registrations.get(&token) {
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

        match op.execute() {
            Ok(output) => Ok(output),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                let desired_kind = op.completion_kind();
                let poll_spec = {
                    let mut state = self.state.lock();
                    let registration = state.registrations.get_mut(&token).ok_or_else(|| {
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

                    registration.waiter = Some(waker);
                    let desired_mask =
                        Self::operation_poll_mask(desired_kind, registration.poll_mask);
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
                        let mut state = self.state.lock();
                        if let Some(HandleRegistration::Poll(registration)) =
                            state.registrations.get_mut(&token)
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

    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        self.register_handle_with_mode(handle, interest, RegistrationMode::Completion)
    }

    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        let mut state = self.state.lock();
        let token = Self::allocate_token(&mut state)?;

        match mode {
            RegistrationMode::Completion => {
                state
                    .registrations
                    .insert(token, HandleRegistration::Completion);
            }
            RegistrationMode::Poll => {
                state.registrations.insert(
                    token,
                    HandleRegistration::Poll(PollRegistration {
                        fd: handle.handle,
                        poll_mask: Self::interest_to_poll_mask(interest),
                        waiter: None,
                        poll_armed: false,
                    }),
                );
            }
        }

        Ok(token)
    }

    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let mut state = self.state.lock();
        match state.registrations.get_mut(&handle.token) {
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

    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let mut state = self.state.lock();
        if state.registrations.remove(&handle.token).is_none() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            ));
        }

        Self::clear_handle_operations(&mut state, handle.token);
        Ok(())
    }

    fn supports_completion(&self) -> bool {
        true
    }

    fn submit_completion<O, R>(&self, mut op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.submit_completion_entry(&mut op, waker)
    }
}
