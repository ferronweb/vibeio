use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::FromRawFd;

use mio::Token;

use crate::{fd_inner::InnerRawHandle, op::Op};

pub struct AcceptOp<'a> {
    handle: &'a InnerRawHandle,
}

impl<'a> AcceptOp<'a> {
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self { handle }
    }
}

impl Op for AcceptOp<'_> {
    type Output = (TcpStream, SocketAddr);

    fn token(&self) -> Token {
        self.handle.token()
    }

    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        let accepted_fd = unsafe {
            libc::accept(
                self.handle.handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if accepted_fd == -1 {
            return Err(io::Error::last_os_error());
        }

        let stream = unsafe { TcpStream::from_raw_fd(accepted_fd) };
        stream.set_nonblocking(true)?;
        let address = stream.peer_addr()?;
        Ok((stream, address))
    }
}
