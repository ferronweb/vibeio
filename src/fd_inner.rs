// TODO: implement Windows support

use std::io;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::task::Waker;

use mio::{Interest, Token};

use crate::{
    driver::{AnyDriver, RegistrationMode},
    executor::current_driver,
    op::Op,
};

pub struct InnerRawHandle {
    pub(crate) handle: RawFd,
    pub(crate) token: Token,
    interest: Interest,
    mode: RegistrationMode,
    driver: Rc<AnyDriver>,
}

impl InnerRawHandle {
    #[inline]
    pub(crate) fn new(handle: RawFd, interest: Interest) -> Result<Self, io::Error> {
        let default_mode = if current_driver()
            .as_ref()
            .is_some_and(|driver| driver.supports_completion())
        {
            RegistrationMode::Completion
        } else {
            RegistrationMode::Poll
        };

        Self::new_with_mode(handle, interest, default_mode)
    }

    #[inline]
    pub(crate) fn new_with_mode(
        handle: RawFd,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        let driver = current_driver().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "can't register I/O handle outside runtime",
            )
        })?;
        let mode = if matches!(mode, RegistrationMode::Completion) && !driver.supports_completion()
        {
            RegistrationMode::Poll
        } else {
            mode
        };
        let mut inner = InnerRawHandle {
            handle,
            token: Token(0),
            interest,
            mode,
            driver: driver.clone(),
        };

        inner.token = driver.register_handle_with_mode(&inner, interest, mode)?;
        Ok(inner)
    }

    #[inline]
    pub(crate) fn token(&self) -> Token {
        self.token
    }

    #[inline]
    pub(crate) fn reregister(&self, interest: Interest) -> Result<(), io::Error> {
        self.driver.reregister_handle(self, interest)
    }

    #[inline]
    pub(crate) fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.driver.submit(op, waker)
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        self.driver.supports_completion()
    }

    #[inline]
    pub(crate) fn uses_completion(&self) -> bool {
        self.supports_completion() && matches!(self.mode, RegistrationMode::Completion)
    }

    #[inline]
    pub(crate) fn mode(&self) -> RegistrationMode {
        self.mode
    }

    #[inline]
    pub(crate) fn rebind_mode(
        &mut self,
        requested_mode: RegistrationMode,
    ) -> Result<(), io::Error> {
        let mode = if matches!(requested_mode, RegistrationMode::Completion)
            && !self.driver.supports_completion()
        {
            RegistrationMode::Poll
        } else {
            requested_mode
        };

        if self.mode == mode {
            return Ok(());
        }

        self.driver.deregister_handle(self)?;
        self.token = self
            .driver
            .register_handle_with_mode(self, self.interest, mode)?;
        self.mode = mode;
        Ok(())
    }

    #[inline]
    pub(crate) fn submit_completion<O, R>(&self, op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.driver.submit_completion(op, waker)
    }
}

impl Drop for InnerRawHandle {
    #[inline]
    fn drop(&mut self) {
        let _ = self.driver.deregister_handle(self);
    }
}
