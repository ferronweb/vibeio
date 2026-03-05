use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use mio::{Interest, Token};
use slab::Slab;
use windows_sys::Win32::Foundation::{
    ERROR_ABANDONED_WAIT_0, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus, OVERLAPPED,
};

use crate::driver::{CompletionIoResult, Interruptor};
use crate::{
    driver::{Driver, RegistrationMode},
    fd_inner::{InnerRawHandle, RawOsHandle},
    op::Op,
};

const INTERRUPT_KEY: usize = usize::MAX;

pub struct IocpInterruptor {
    port: std::sync::Weak<OwnedHandle>,
}

impl Interruptor for IocpInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(port) = self.port.upgrade() {
            let _ = unsafe {
                PostQueuedCompletionStatus(
                    port.as_raw_handle() as HANDLE,
                    0,
                    INTERRUPT_KEY,
                    ptr::null_mut(),
                )
            };
        }
    }
}

struct Completion {
    waiter: Option<Waker>,
    completed: Option<i32>,
    overlapped: Option<Box<OverlappedCtx>>,
}

#[repr(C)]
struct OverlappedCtx {
    overlapped: OVERLAPPED,
    token: usize,
}

struct DriverState {
    registrations: Slab<RegistrationMode>,
    completions: Slab<Completion>,
}

pub struct IocpDriver {
    port: Arc<OwnedHandle>,
    state: RefCell<DriverState>,
}

impl IocpDriver {
    #[inline]
    pub(crate) fn new() -> Result<Self, io::Error> {
        let port = unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE as HANDLE, ptr::null_mut(), 0, 0)
        };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            port: Arc::new(unsafe { OwnedHandle::from_raw_handle(port as RawHandle) }),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(1024),
                completions: Slab::with_capacity(1024),
            }),
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
    fn iocp_handle(&self) -> HANDLE {
        self.port.as_raw_handle() as HANDLE
    }

    #[inline]
    fn raw_os_handle_to_windows_handle(handle: RawOsHandle) -> HANDLE {
        match handle {
            RawOsHandle::Socket(socket) => socket as HANDLE,
            RawOsHandle::Handle(handle) => handle as HANDLE,
        }
    }

    #[inline]
    fn duration_to_timeout_ms(timeout: Option<Duration>) -> u32 {
        match timeout {
            Some(timeout) => timeout.as_millis().min(u32::MAX as u128) as u32,
            None => u32::MAX,
        }
    }

    #[inline]

    fn process_one(&self, timeout_ms: u32) -> Result<bool, io::Error> {
        let mut transferred: u32 = 0;
        let mut completion_key: usize = 0;
        let mut overlapped: *mut OVERLAPPED = ptr::null_mut();

        let success = unsafe {
            GetQueuedCompletionStatus(
                self.iocp_handle(),
                &mut transferred,
                &mut completion_key,
                &mut overlapped,
                timeout_ms,
            )
        } != 0;

        if overlapped.is_null() {
            if success {
                // Posted interrupt packet.
                return Ok(true);
            }

            let err = io::Error::last_os_error();
            if matches!(
                err.raw_os_error(),
                Some(code) if code == WAIT_TIMEOUT as i32 || code == ERROR_ABANDONED_WAIT_0 as i32
            ) {
                return Ok(false);
            }
            return Err(err);
        }

        if completion_key == INTERRUPT_KEY {
            return Ok(true);
        }

        let completion_result = if success {
            transferred as i32
        } else {
            let err_code = io::Error::last_os_error().raw_os_error().unwrap_or(1);
            -err_code
        };

        let mut state = self.state.borrow_mut();
        // SAFETY: every OVERLAPPED pointer submitted by this driver points to the first field
        // of OverlappedCtx (repr(C), first field), and stays alive in Completion::overlapped
        // until consumed here.
        let completion_token = unsafe { (*overlapped.cast::<OverlappedCtx>()).token };
        if let Some(completion) = state.completions.get_mut(completion_token) {
            completion.completed = Some(completion_result);
            completion.overlapped = None;
            if let Some(waiter) = completion.waiter.take() {
                waiter.wake();
            }
        }

        Ok(true)
    }

    #[inline]
    fn process_ready_completions(&self) -> Result<(), io::Error> {
        while self.process_one(0)? {}
        Ok(())
    }
}

impl Driver for IocpDriver {
    type Interruptor = IocpInterruptor;

    #[inline]
    fn flush(&self) {
        match self.process_ready_completions() {
            Ok(_) => {}
            Err(err) => panic!("iocp flush failed while processing completions: {err}"),
        }
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        let timeout_ms = Self::duration_to_timeout_ms(timeout);
        match self.process_one(timeout_ms) {
            Ok(true) => {
                if let Err(err) = self.process_ready_completions() {
                    panic!("iocp drain failed while waiting for I/O: {err}");
                }
            }
            Ok(false) => {}
            Err(err) => panic!("iocp wait failed while waiting for I/O: {err}"),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        _interest: Interest,
    ) -> Result<Token, io::Error> {
        self.register_handle_with_mode(handle, _interest, RegistrationMode::Completion)
    }

    #[inline]
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        _interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        if !matches!(mode, RegistrationMode::Completion) {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "IOCP driver currently supports completion mode only",
            ));
        }

        let token = {
            let mut state = self.state.borrow_mut();
            let entry = state.registrations.vacant_entry();
            let token = Token(entry.key());
            entry.insert(mode);
            token
        };

        let source_handle = Self::raw_os_handle_to_windows_handle(handle.handle);
        let completion_port =
            unsafe { CreateIoCompletionPort(source_handle, self.iocp_handle(), token.0, 0) };
        if completion_port.is_null() {
            let mut state = self.state.borrow_mut();
            let _ = state.registrations.try_remove(token.0);
            return Err(io::Error::last_os_error());
        }

        Ok(token)
    }

    #[inline]
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        _interest: Interest,
    ) -> Result<(), io::Error> {
        let state = self.state.borrow();
        if state.registrations.get(handle.token.0).is_none() {
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
    fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        let (completion_token, overlapped_ptr) = {
            let mut state = self.state.borrow_mut();
            let vacant_completion = state.completions.vacant_entry();
            let completion_token = vacant_completion.key();

            let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OverlappedCtx>() });
            overlapped.token = completion_token;
            let overlapped_ptr: *mut OVERLAPPED = &mut overlapped.overlapped;

            vacant_completion.insert(Completion {
                waiter: Some(waker),
                completed: None,
                overlapped: Some(overlapped),
            });

            (completion_token, overlapped_ptr)
        };

        if let Err(err) = op.submit_windows(overlapped_ptr) {
            let mut state = self.state.borrow_mut();
            let _ = state.completions.try_remove(completion_token);
            return CompletionIoResult::SubmitErr(err);
        }

        CompletionIoResult::Retry(completion_token)
    }

    #[inline]
    fn submit_poll(
        &self,
        _handle: &InnerRawHandle,
        _waker: Waker,
        _interest: Interest,
    ) -> Result<(), io::Error> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "IOCP driver currently supports completion mode only",
        ))
    }

    #[inline]
    fn get_completion_result(&self, token: usize) -> Option<i32> {
        let mut state = self.state.borrow_mut();
        let completed = state
            .completions
            .get(token)
            .and_then(|completion| completion.completed);
        if completed.is_some() {
            state.completions.remove(token);
        }
        completed
    }

    #[inline]
    fn set_completion_waker(&self, token: usize, waker: Waker) {
        let mut state = self.state.borrow_mut();
        if let Some(completion) = state.completions.get_mut(token) {
            Self::update_waiter(&mut completion.waiter, waker);
        }
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        IocpInterruptor {
            port: Arc::downgrade(&self.port),
        }
    }
}
