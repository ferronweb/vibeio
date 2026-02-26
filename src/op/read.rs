use std::io;

use mio::Token;

use crate::{fd_inner::InnerRawHandle, op::Op};

pub struct ReadOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a mut [u8],
}

impl<'a> ReadOp<'a> {
    pub fn new(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self { handle, buf }
    }
}

impl Op for ReadOp<'_> {
    type Output = usize;

    fn token(&self) -> Token {
        self.handle.token()
    }

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
}
