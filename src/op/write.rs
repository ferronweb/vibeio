use std::io;
use std::task::{Context, Poll};

use mio::Token;

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, CompletionKind, Op},
};

pub trait CompletionWriteIo {
    fn poll_write_completion(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>>;
}

impl CompletionWriteIo for InnerRawHandle {
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
}

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
    fn completion_kind(&self) -> Option<CompletionKind> {
        Some(CompletionKind::Write)
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        Ok(opcode::Send::new(
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
