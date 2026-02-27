mod mio;
mod mock;
#[cfg(target_os = "linux")]
mod uring;

use std::io;
use std::task::Waker;

use ::mio::{Interest, Token};

#[cfg(target_os = "linux")]
use crate::driver::uring::UringDriver;
use crate::{
    driver::{mio::MioDriver, mock::MockDriver},
    fd_inner::InnerRawHandle,
    op::Op,
};

fn unsupported_completion_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "driver does not support completion-based I/O submission",
    )
}

pub trait Driver {
    /// Waits for the I/O.
    fn wait(&self);

    /// Submits an I/O operation.
    fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, std::io::Error>
    where
        O: Op<Output = R>;

    /// Registers an I/O source and returns its token.
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error>;

    /// Updates the interest set for a registered I/O source.
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error>;

    /// Removes an I/O source from the poller.
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error>;

    /// Returns whether the driver supports completion-based I/O operations.
    fn supports_completion(&self) -> bool {
        false
    }

    /// Submits a completion-based I/O operation.
    fn submit_completion<O, R>(&self, _op: O, _waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        Err(unsupported_completion_error())
    }
}

pub enum AnyDriver {
    Mio(MioDriver),
    Mock(MockDriver),
    #[cfg(target_os = "linux")]
    IoUring(UringDriver),
}

impl AnyDriver {
    pub(crate) fn new_mio() -> Result<Self, std::io::Error> {
        Ok(AnyDriver::Mio(MioDriver::new()?))
    }

    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new_uring(entries: u32) -> Result<Self, io::Error> {
        Ok(AnyDriver::IoUring(UringDriver::new(entries)?))
    }

    pub(crate) fn new_best() -> Result<Self, io::Error> {
        #[cfg(target_os = "linux")]
        if let Ok(driver) = UringDriver::new(256) {
            return Ok(AnyDriver::IoUring(driver));
        }

        Self::new_mio()
    }

    pub(crate) fn wait(&self) {
        match self {
            AnyDriver::Mio(driver) => driver.wait(),
            AnyDriver::Mock(driver) => driver.wait(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.wait(),
        }
    }

    pub(crate) fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, std::io::Error>
    where
        O: Op<Output = R>,
    {
        match self {
            AnyDriver::Mio(driver) => driver.submit(op, waker),
            AnyDriver::Mock(driver) => driver.submit(op, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit(op, waker),
        }
    }

    pub(crate) fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.register_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.register_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle(handle, interest),
        }
    }

    pub(crate) fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.reregister_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.reregister_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.reregister_handle(handle, interest),
        }
    }

    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.deregister_handle(handle),
        }
    }

    pub(crate) fn supports_completion(&self) -> bool {
        match self {
            AnyDriver::Mio(driver) => driver.supports_completion(),
            AnyDriver::Mock(driver) => driver.supports_completion(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.supports_completion(),
        }
    }

    pub(crate) fn submit_completion<O, R>(&self, op: O, waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        match self {
            AnyDriver::Mio(driver) => driver.submit_completion(op, waker),
            AnyDriver::Mock(driver) => driver.submit_completion(op, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_completion(op, waker),
        }
    }
}
