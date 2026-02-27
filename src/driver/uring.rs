use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind};
use std::sync::Mutex;
use std::task::Waker;

use io_uring::{squeue, IoUring};
use mio::{Interest, Token};

use crate::{
    driver::Driver,
    fd_inner::InnerRawHandle,
    op::{CompletionKind, Op},
};

const KEY_KIND_BITS: u64 = 8;

struct PendingRegistration {
    waiters: Vec<Waker>,
}

struct DriverState {
    registrations: HashSet<Token>,
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
                registrations: HashSet::new(),
                next_token: 0,
                pending: HashMap::new(),
                completed: HashMap::new(),
            }),
        })
    }

    fn encode_key(token: Token, kind: CompletionKind) -> u64 {
        ((token.0 as u64) << KEY_KIND_BITS) | (kind as u8 as u64)
    }

    fn decode_token(key: u64) -> Token {
        Token((key >> KEY_KIND_BITS) as usize)
    }

    fn io_uring_would_block_error(kind: CompletionKind) -> io::Error {
        io::Error::new(
            ErrorKind::WouldBlock,
            format!("{kind} completion is pending"),
        )
    }

    fn submitter_call_result(result: Result<usize, io::Error>) -> Result<(), io::Error> {
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn push_entry(&self, entry: squeue::Entry) -> Result<(), io::Error> {
        let mut ring = self.ring.lock().expect("io_uring ring mutex is poisoned");

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
        let key = Self::encode_key(token, kind);

        {
            let mut state = self.state.lock().expect("io_uring state mutex is poisoned");
            if !state.registrations.contains(&token) {
                return Err(io::Error::new(
                    ErrorKind::NotFound,
                    format!("I/O token {} is not registered with this driver", token.0),
                ));
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
            let mut state = self.state.lock().expect("io_uring state mutex is poisoned");
            state.pending.remove(&key);
            return Err(err);
        }

        Err(Self::io_uring_would_block_error(kind))
    }

    fn clear_handle_operations(state: &mut DriverState, token: Token) {
        state
            .pending
            .retain(|key, _| Self::decode_token(*key) != token);
        state
            .completed
            .retain(|key, _| Self::decode_token(*key) != token);
    }
}

impl Driver for UringDriver {
    fn wait(&self) {
        {
            let ring = self.ring.lock().expect("io_uring ring mutex is poisoned");
            if let Err(err) = Self::submitter_call_result(ring.submit_and_wait(1)) {
                panic!("io_uring submit_and_wait failed while waiting for I/O: {err}");
            }
        }

        let completions = {
            let mut ring = self.ring.lock().expect("io_uring ring mutex is poisoned");
            let mut cq = ring.completion();
            cq.sync();
            cq.map(|cqe| (cqe.user_data(), cqe.result()))
                .collect::<Vec<_>>()
        };

        if completions.is_empty() {
            return;
        }

        let mut to_wake = Vec::new();
        {
            let mut state = self.state.lock().expect("io_uring state mutex is poisoned");
            for (key, result) in completions {
                let token = Self::decode_token(key);
                if !state.registrations.contains(&token) {
                    continue;
                }

                state.completed.insert(key, result);
                if let Some(pending) = state.pending.remove(&key) {
                    to_wake.extend(pending.waiters);
                }
            }
        }

        for waker in to_wake {
            waker.wake();
        }
    }

    fn submit<O, R>(&self, _op: O, _waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "poll-based operations are not supported by io_uring driver; use completion APIs",
        ))
    }

    fn register_handle(
        &self,
        _handle: &InnerRawHandle,
        _interest: Interest,
    ) -> Result<Token, io::Error> {
        let mut state = self.state.lock().expect("io_uring state mutex is poisoned");

        for _ in 0..usize::MAX {
            let token = Token(state.next_token);
            state.next_token = state.next_token.wrapping_add(1);
            if state.registrations.insert(token) {
                return Ok(token);
            }
        }

        Err(io::Error::new(
            ErrorKind::Other,
            "io_uring token space exhausted for I/O registrations",
        ))
    }

    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        _interest: Interest,
    ) -> Result<(), io::Error> {
        let state = self.state.lock().expect("io_uring state mutex is poisoned");
        if state.registrations.contains(&handle.token) {
            Ok(())
        } else {
            Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            ))
        }
    }

    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let mut state = self.state.lock().expect("io_uring state mutex is poisoned");
        state.registrations.remove(&handle.token);
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
