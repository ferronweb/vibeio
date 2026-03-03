use std::io;
use std::task::{Context, Poll};

use mio::{Interest, Token};

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, Op},
};

/// Unified read helper trait implemented on `InnerRawHandle`.
/// Exposes poll-based, completion-based and unified (default) helpers.
pub trait ReadIo {
    /// Submit the read operation via the poll/readiness pathway.
    fn poll_read_poll(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>>;

    /// Submit the read operation via the completion pathway.
    fn poll_read_completion(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>>;

    /// High-level read helper that chooses between completion and poll pathways.
    fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, io::Error>>;
}

impl ReadIo for InnerRawHandle {
    #[inline]
    fn poll_read_poll(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.submit(ReadOp::new(self, buf), cx.waker().clone()) {
            Ok(read) => Poll::Ready(Ok(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_read_completion(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        completion_result_to_poll(
            self.submit_completion(ReadOp::new(self, buf), cx.waker().clone()),
        )
    }

    #[inline]
    fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, io::Error>> {
        if self.uses_completion() {
            self.poll_read_completion(cx, buf)
        } else {
            self.poll_read_poll(cx, buf)
        }
    }
}

pub struct ReadOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a mut [u8],
}

impl<'a> ReadOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self { handle, buf }
    }
}

impl Op for ReadOp<'_> {
    type Output = usize;

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        let read = unsafe {
            libc::read(
                self.handle.handle,
                self.buf.as_mut_ptr().cast::<libc::c_void>(),
                self.buf.len(),
            )
        };

        if read == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(read as usize)
    }

    #[inline]
    fn interest(&self) -> Interest {
        Interest::READABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn std::any::Any>>), io::Error> {
        use io_uring::{opcode, types};

        // Build the read SQE as before. This operation does not require any
        // auxiliary driver-owned storage, so return the SQE together with None.
        let entry = opcode::Read::new(
            types::Fd(self.handle.handle),
            self.buf.as_mut_ptr(),
            self.buf.len() as _,
        )
        .build()
        .user_data(user_data);

        Ok((entry, None))
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(result as usize)
    }
}
