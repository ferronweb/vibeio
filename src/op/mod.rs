mod accept;
mod connect;
mod read;
mod readv;
mod write;
mod writev;

use std::any::Any;
use std::io;
use std::task::Poll;

use mio::{Interest, Token};

pub use accept::AcceptIo;
pub use connect::ConnectIo;
pub use read::ReadIo;
pub use readv::ReadvIo;
pub use write::WriteIo;
pub use writev::WritevIo;

#[inline]
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

    /// Builds an io_uring submission entry for this operation. Returns the
    /// constructed SQE and an optional boxed storage value which the driver
    /// will hold onto for the duration of the completion.
    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        _user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn Any>>), io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }

    /// Called by the driver to transfer previously-stored completion storage
    /// back into the operation prior to calling `complete`.
    #[cfg(target_os = "linux")]
    #[inline]
    fn take_completion_storage(&mut self, _storage: Option<Box<dyn Any>>) {}

    /// Converts a completion result into the operation output.
    #[inline]
    fn complete(&mut self, _result: i32) -> Result<Self::Output, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod vectored_uring_tests {
    use std::future::poll_fn;
    use std::io::{IoSlice, IoSliceMut};

    use mio::Interest;

    use crate::op::{ReadvIo, WritevIo};
    use crate::{driver::AnyDriver, fd_inner::InnerRawHandle};

    #[test]
    fn io_uring_vectored_read_write_pipe() {
        // Create a runtime with an io_uring driver and run the test inside it so
        // current_driver() is available for InnerRawHandle::new.
        let driver = AnyDriver::new_uring().expect("failed to create uring driver");
        let runtime = crate::executor::Runtime::new(driver);

        runtime.block_on(async {
            // create a pipe (pair of fds)
            let mut fds: [libc::c_int; 2] = [0, 0];
            let res = unsafe { libc::pipe(fds.as_mut_ptr() as *mut libc::c_int) };
            assert_eq!(res, 0, "pipe() failed");

            let rfd = fds[0];
            let wfd = fds[1];

            // Register both ends with the runtime. Since the current driver supports
            // completion and the InnerRawHandle default chooses completion mode, these
            // handles will use the completion path (io_uring) where available.
            let rhandle = InnerRawHandle::new(rfd, Interest::READABLE)
                .expect("failed to create reader InnerRawHandle");
            let whandle = InnerRawHandle::new(wfd, Interest::WRITABLE)
                .expect("failed to create writer InnerRawHandle");

            // Prepare vectored write buffers
            let a = b"hello ";
            let b = b"world!";
            let bufs = [IoSlice::new(a), IoSlice::new(b)];
            let total_len = a.len() + b.len();

            // Submit vectored write. poll_writev will choose completion-path (io_uring)
            // for this handle because the driver supports completions.
            let write_res = poll_fn(|cx| whandle.poll_writev(cx, &bufs)).await;
            let written = write_res.expect("writev failed");
            assert!(
                written > 0,
                "expected writev to write at least one byte (wrote 0)"
            );

            // Prepare vectored read buffers (split sizes arbitrarily)
            let mut dst1 = vec![0u8; 3]; // will receive "hel"
            let mut dst2 = vec![0u8; total_len - 3]; // rest
            let mut rd_bufs = [IoSliceMut::new(&mut dst1), IoSliceMut::new(&mut dst2)];

            // Read using vectored read. poll_readv will choose completion-path when available.
            let read_res = poll_fn(|cx| rhandle.poll_readv(cx, &mut rd_bufs)).await;
            let read = read_res.expect("readv failed");

            // We expect to read at least as many bytes as were written (pipe semantics
            // on local write -> read without closing may give the bytes).
            // It's possible for writev to be partial; just check we read some data and
            // that the bytes we did read match the written prefix.
            assert!(read > 0, "expected to read at least one byte");

            // Reconstruct the received bytes and compare with written prefix.
            let mut received = Vec::with_capacity(read);
            received.extend_from_slice(&dst1[..dst1.len().min(read)]);
            if read > dst1.len() {
                let rem = read - dst1.len();
                received.extend_from_slice(&dst2[..rem]);
            }

            // The bytes we received should be a prefix of the concatenation of a+b.
            let expected = [a.as_slice(), b.as_slice()].concat();
            assert_eq!(
                received,
                expected[..received.len()],
                "received bytes don't match expected prefix"
            );

            // Close fds to avoid leaking
            unsafe {
                let _ = libc::close(rfd);
                let _ = libc::close(wfd);
            }
        });
    }
}
