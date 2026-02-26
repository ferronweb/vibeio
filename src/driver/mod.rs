mod mio;
mod mock;

use std::task::Waker;

use ::mio::{Interest, Token};

use crate::{
    driver::{mio::MioDriver, mock::MockDriver},
    fd_inner::InnerRawHandle,
    op::Op,
};

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
}

pub enum AnyDriver {
    Mio(MioDriver),
    Mock(MockDriver),
}

impl AnyDriver {
    pub(crate) fn new_mio() -> Result<Self, std::io::Error> {
        Ok(AnyDriver::Mio(MioDriver::new()?))
    }

    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    pub(crate) fn wait(&self) {
        match self {
            AnyDriver::Mio(driver) => driver.wait(),
            AnyDriver::Mock(driver) => driver.wait(),
        }
    }

    pub(crate) fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, std::io::Error>
    where
        O: Op<Output = R>,
    {
        match self {
            AnyDriver::Mio(driver) => driver.submit(op, waker),
            AnyDriver::Mock(driver) => driver.submit(op, waker),
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
        }
    }

    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
        }
    }
}
