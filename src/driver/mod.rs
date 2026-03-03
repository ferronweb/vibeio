#[cfg(unix)]
mod mio;
mod mock;
#[cfg(target_os = "linux")]
mod uring;

use std::task::Waker;
use std::{io, time::Duration};

use ::mio::{Interest, Token};

use crate::driver::mio::MioInterruptor;
use crate::driver::mock::MockInterruptor;
#[cfg(target_os = "linux")]
use crate::driver::uring::{UringDriver, UringInterruptor};
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

pub trait Interruptor {
    /// Interrupts a waiting I/O operation.
    fn interrupt(&self);
}

pub enum AnyInterruptor {
    Mock(MockInterruptor),
    #[cfg(unix)]
    Mio(MioInterruptor),
    #[cfg(target_os = "linux")]
    IoUring(UringInterruptor),
}

impl AnyInterruptor {
    pub(crate) fn interrupt(&self) {
        match self {
            AnyInterruptor::Mio(interruptor) => interruptor.interrupt(),
            #[cfg(unix)]
            AnyInterruptor::Mock(interruptor) => interruptor.interrupt(),
            #[cfg(target_os = "linux")]
            AnyInterruptor::IoUring(interruptor) => interruptor.interrupt(),
        }
    }
}

pub trait Driver {
    type Interruptor: Interruptor;

    /// Flushes the driver's I/O.
    #[inline]
    fn flush(&self) {}

    /// Waits for the I/O.
    fn wait(&self, timeout: Option<Duration>);

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
    fn get_interruptor(&self) -> Self::Interruptor;
}

pub enum AnyDriver {
    Mock(MockDriver),
    #[cfg(unix)]
    Mio(MioDriver),
    #[cfg(target_os = "linux")]
    IoUring(UringDriver),
}

impl AnyDriver {
    #[cfg(unix)]
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
    pub(crate) fn new_uring_custom(builder: io_uring::Builder) -> Result<Self, io::Error> {
        Ok(AnyDriver::IoUring(UringDriver::new(1024, builder)?))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring() -> Result<Self, io::Error> {
        Self::new_uring_custom(io_uring::IoUring::builder())
    }

    #[inline]
    pub(crate) fn new_best() -> Result<Self, io::Error> {
        #[cfg(target_os = "linux")]
        if let Ok(driver) = Self::new_uring() {
            return Ok(driver);
        }

        #[cfg(unix)]
        let driver = Self::new_mio()?;
        #[cfg(windows)]
        let driver = Self::new_mock(); // TODO: add real IOCP driver for Windows

        Ok(driver)
    }

    #[inline]
    pub(crate) fn flush(&self) {
        match self {
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.flush(),
            AnyDriver::Mock(driver) => driver.flush(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.flush(),
        }
    }

    #[inline]
    pub(crate) fn wait(&self, timeout: Option<Duration>) {
        match self {
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.wait(timeout),
            AnyDriver::Mock(driver) => driver.wait(timeout),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.wait(timeout),
        }
    }

    #[inline]
    pub(crate) fn submit<O, R>(&self, op: O, waker: Waker) -> Result<R, std::io::Error>
    where
        O: Op<Output = R>,
    {
        match self {
            #[cfg(unix)]
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
            #[cfg(unix)]
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
            #[cfg(unix)]
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
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.reregister_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.reregister_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.reregister_handle(handle, interest),
        }
    }

    #[inline]
    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.deregister_handle(handle),
        }
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        match self {
            #[cfg(unix)]
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
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.submit_completion(op, waker),
            AnyDriver::Mock(driver) => driver.submit_completion(op, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_completion(op, waker),
        }
    }

    #[inline]
    pub(crate) fn get_interruptor(&self) -> AnyInterruptor {
        match self {
            #[cfg(unix)]
            AnyDriver::Mio(driver) => AnyInterruptor::Mio(driver.get_interruptor()),
            AnyDriver::Mock(driver) => AnyInterruptor::Mock(driver.get_interruptor()),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => AnyInterruptor::IoUring(driver.get_interruptor()),
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

    #[cfg(unix)]
    #[test]
    fn test_mio_driver_interrupt_basic() {
        let driver = AnyDriver::new_mio().expect("Failed to create MioDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_uring_driver_interrupt_basic() {
        let driver = AnyDriver::new_uring().expect("Failed to create UringDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[test]
    fn test_mock_driver_interrupt_basic() {
        let driver = AnyDriver::new_mock();
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(unix)]
    #[test]
    fn test_interrupt_mio() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("Failed to create MioDriver"),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn test_interruptor_uring() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_uring().expect("Failed to create UringDriver"),
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
