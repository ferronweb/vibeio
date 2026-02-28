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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationMode {
    Poll,
    Completion,
}

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

    /// Registers an I/O source with the requested mode.
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        _mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        self.register_handle(handle, interest)
    }

    /// Updates the interest set for a registered I/O source.
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error>;

    /// Removes an I/O source from the poller.
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error>;

    /// Returns whether the driver supports completion-based I/O operations.
    #[inline]
    fn supports_completion(&self) -> bool {
        false
    }

    /// Submits a completion-based I/O operation.
    #[inline]
    fn submit_completion<O, R>(&self, _op: O, _waker: Waker) -> Result<R, io::Error>
    where
        O: Op<Output = R>,
    {
        Err(unsupported_completion_error())
    }

    /// Interrupts a waiting I/O operation.
    #[inline]
    fn interrupt(&self) {
        // Default implementation does nothing
    }
}

pub enum AnyDriver {
    Mio(MioDriver),
    Mock(MockDriver),
    #[cfg(target_os = "linux")]
    IoUring(UringDriver),
}

impl AnyDriver {
    #[inline]
    pub(crate) fn new_mio() -> Result<Self, std::io::Error> {
        Ok(AnyDriver::Mio(MioDriver::new()?))
    }

    #[inline]
    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring(entries: u32) -> Result<Self, io::Error> {
        Ok(AnyDriver::IoUring(UringDriver::new(entries)?))
    }

    #[inline]
    pub(crate) fn new_best() -> Result<Self, io::Error> {
        #[cfg(target_os = "linux")]
        if let Ok(driver) = UringDriver::new(1024) {
            return Ok(AnyDriver::IoUring(driver));
        }

        Self::new_mio()
    }

    #[inline]
    pub(crate) fn wait(&self) {
        match self {
            AnyDriver::Mio(driver) => driver.wait(),
            AnyDriver::Mock(driver) => driver.wait(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.wait(),
        }
    }

    #[inline]
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

    #[inline]
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

    #[inline]
    pub(crate) fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.register_handle_with_mode(handle, interest, mode),
            AnyDriver::Mock(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle_with_mode(handle, interest, mode),
        }
    }

    #[inline]
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

    #[inline]
    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.deregister_handle(handle),
        }
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        match self {
            AnyDriver::Mio(driver) => driver.supports_completion(),
            AnyDriver::Mock(driver) => driver.supports_completion(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.supports_completion(),
        }
    }

    #[inline]
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

    #[inline]
    pub(crate) fn interrupt(&self) {
        match self {
            AnyDriver::Mio(driver) => driver.interrupt(),
            AnyDriver::Mock(driver) => driver.interrupt(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.interrupt(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AnyDriver;
    use std::{
        future::poll_fn,
        task::{Poll, Waker},
    };

    #[test]
    fn test_mio_driver_interrupt_basic() {
        let driver = AnyDriver::new_mio().expect("Failed to create MioDriver");

        // Test that interrupt doesn't panic and can be called multiple times
        driver.interrupt();
        driver.interrupt();
        driver.interrupt();
    }

    #[test]
    fn test_uring_driver_interrupt_basic() {
        #[cfg(target_os = "linux")]
        {
            let driver = AnyDriver::new_uring(1024).expect("Failed to create UringDriver");

            // Test that interrupt doesn't panic and can be called multiple times
            driver.interrupt();
            driver.interrupt();
            driver.interrupt();
        }
    }

    #[test]
    fn test_mock_driver_interrupt_basic() {
        let driver = AnyDriver::new_mock();

        // Test that interrupt doesn't panic and can be called multiple times
        driver.interrupt();
        driver.interrupt();
        driver.interrupt();
    }

    #[test]
    fn test_interrupt_mio() {
        let runtime =
            crate::executor::new_runtime(AnyDriver::new_mio().expect("Failed to create MioDriver"));

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[test]
    fn test_interruptor_uring() {
        #[cfg(target_os = "linux")]
        {
            let runtime = crate::executor::new_runtime(
                AnyDriver::new_uring(1024).expect("Failed to create UringDriver"),
            );

            let (tx, rx) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                let waker: Waker = rx.recv().unwrap();
                drop(rx); // Drop the receiver before waking the task
                waker.wake();
            });

            runtime.block_on(poll_fn(move |cx| {
                if tx.send(cx.waker().clone()).is_ok() {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }));
        }
    }
}
