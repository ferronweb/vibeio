// TODO: implement Windows support

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::task::Waker;

use mio::{Interest, Token};

use crate::{driver::AnyDriver, executor::current_driver, op::Op};

pub struct InnerRawHandle {
    pub(crate) handle: RawFd,
    pub(crate) token: Token,
    driver: Arc<AnyDriver>,
}

impl InnerRawHandle {
    pub(crate) fn new(handle: RawFd, interest: Interest) -> Result<Self, io::Error> {
        let driver = current_driver().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "can't register I/O handle outside runtime",
            )
        })?;
        let mut inner = InnerRawHandle {
            handle,
            token: Token(0),
            driver: driver.clone(),
        };

        inner.token = driver.register_handle(&inner, interest)?;
        Ok(inner)
    }

    pub(crate) fn token(&self) -> Token {
        self.token
    }

    pub(crate) fn reregister(&self, interest: Interest) -> Result<(), io::Error> {
        self.driver.reregister_handle(self, interest)
    }

    pub(crate) fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.driver.submit(op, waker)
    }

    pub(crate) fn supports_completion(&self) -> bool {
        self.driver.supports_completion()
    }

    pub(crate) fn submit_completion<O, R>(&self, op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        self.driver.submit_completion(op, waker)
    }
}

impl Drop for InnerRawHandle {
    fn drop(&mut self) {
        let _ = self.driver.deregister_handle(self);
    }
}
