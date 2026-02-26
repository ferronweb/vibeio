use std::io;

use mio::Token;

use crate::{fd_inner::InnerRawHandle, op::Op};

pub struct WriteOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a [u8],
}

impl<'a> WriteOp<'a> {
    pub fn new(handle: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self { handle, buf }
    }
}

impl Op for WriteOp<'_> {
    type Output = usize;

    fn token(&self) -> Token {
        self.handle.token()
    }

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
}
