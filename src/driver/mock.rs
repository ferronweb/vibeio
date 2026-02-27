use crate::driver::Driver;
use mio::{Interest, Token};

pub struct MockDriver {}

impl MockDriver {
    #[inline]
    pub(crate) fn new() -> Self {
        MockDriver {}
    }
}

impl Driver for MockDriver {
    #[inline]
    fn wait(&self) {
        panic!("runtime stalled: main task is pending but no tasks are ready");
    }

    #[inline]
    fn submit<O, R>(&self, _op: O, _waker: std::task::Waker) -> Result<R, std::io::Error>
    where
        O: crate::op::Op<Output = R>,
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O operation submission",
        ))
    }

    #[inline]
    fn register_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
        _interest: Interest,
    ) -> Result<Token, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle registration",
        ))
    }

    #[inline]
    fn reregister_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
        _interest: Interest,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle re-registration",
        ))
    }

    #[inline]
    fn deregister_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle deregistration",
        ))
    }
}
