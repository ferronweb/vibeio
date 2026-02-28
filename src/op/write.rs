use std::io;
use std::task::{Context, Poll};

use mio::{Interest, Token};

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, Op},
};

/// Unified write helper trait implemented on `InnerRawHandle`.
/// Exposes poll-based, completion-based and unified (default) helpers.
pub trait WriteIo {
    /// Submit the write operation via the poll/readiness pathway.
    fn poll_write_poll(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>>;

    /// Submit the write operation via the completion pathway.
    fn poll_write_completion(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>>;

    /// High-level write helper that chooses between completion and poll pathways.
    fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>>;
}

// Compatibility traits removed — use `WriteIo` directly.

impl WriteIo for InnerRawHandle {
    #[inline]
    fn poll_write_poll(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.submit(WriteOp::new(self, buf), cx.waker().clone()) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_write_completion(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        completion_result_to_poll(
            self.submit_completion(WriteOp::new(self, buf), cx.waker().clone()),
        )
    }

    #[inline]
    fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        if self.uses_completion() {
            self.poll_write_completion(cx, buf)
        } else {
            self.poll_write_poll(cx, buf)
        }
    }
}

// Compatibility forwards removed — consumers should call methods on `WriteIo`.

pub struct WriteOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a [u8],
}

impl<'a> WriteOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self { handle, buf }
    }
}

impl Op for WriteOp<'_> {
    type Output = usize;

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    #[inline]
    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        let written = unsafe {
            libc::write(
                self.handle.handle,
                self.buf.as_ptr().cast::<libc::c_void>(),
                self.buf.len(),
            )
        };

        if written == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(written as usize)
    }

    #[inline]
    fn interest(&self) -> Interest {
        Interest::WRITABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        Ok(opcode::Write::new(
            types::Fd(self.handle.handle),
            self.buf.as_ptr(),
            self.buf.len() as _,
        )
        .build()
        .user_data(user_data))
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(result as usize)
    }
}
