mod accept;
mod connect;
mod read;
mod write;

use std::io;
use std::task::Poll;

use mio::{Interest, Token};

pub use accept::AcceptIo;
pub use connect::ConnectIo;
pub use read::ReadIo;
pub use write::WriteIo;

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

    /// Returns a poll interest for the I/O source this operation targets.
    #[inline]
    fn interest(&self) -> Interest {
        Interest::READABLE | Interest::WRITABLE
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
