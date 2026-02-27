mod accept;
mod connect;
mod read;
mod write;

use std::io;
use std::task::Poll;

use mio::Token;

pub use accept::{AcceptOp, CompletionAcceptIo};
pub use connect::{CompletionConnectIo, ConnectOp};
pub use read::{CompletionReadIo, ReadOp};
pub use write::{CompletionWriteIo, WriteOp};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CompletionKind {
    Connect = 1,
    Accept = 2,
    Read = 3,
    Write = 4,
}

pub(crate) fn completion_result_to_poll<T>(
    result: Result<T, io::Error>,
) -> Poll<Result<T, io::Error>> {
    match result {
        Ok(output) => Poll::Ready(Ok(output)),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
        Err(err) => Poll::Ready(Err(err)),
    }
}

pub trait Op {
    /// I/O operation return type
    type Output;

    /// Returns the token of the I/O source this operation targets.
    fn token(&self) -> Token;

    /// Executes the I/O operation (poll/readiness path).
    fn execute(&mut self) -> Result<Self::Output, io::Error>;

    /// Completion operation kind for drivers that support completion I/O.
    #[inline]
    fn completion_kind(&self) -> Option<CompletionKind> {
        None
    }

    /// Builds an io_uring submission entry for this operation.
    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        _user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }

    /// Converts a completion result into the operation output.
    #[inline]
    fn complete(&mut self, _result: i32) -> Result<Self::Output, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }
}
