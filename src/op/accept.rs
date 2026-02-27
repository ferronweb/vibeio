use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::task::{Context, Poll};

use mio::Token;

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, CompletionKind, Op},
};

pub trait CompletionAcceptIo {
    fn poll_accept_completion(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(TcpStream, SocketAddr), io::Error>>;
}

impl CompletionAcceptIo for InnerRawHandle {
    #[inline]
    fn poll_accept_completion(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(TcpStream, SocketAddr), io::Error>> {
        completion_result_to_poll(self.submit_completion(AcceptOp::new(self), cx.waker().clone()))
    }
}

pub struct AcceptOp<'a> {
    handle: &'a InnerRawHandle,
}

impl<'a> AcceptOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self { handle }
    }
}

impl Op for AcceptOp<'_> {
    type Output = (TcpStream, SocketAddr);

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    #[inline]
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

    #[inline]
    fn completion_kind(&self) -> Option<CompletionKind> {
        Some(CompletionKind::Accept)
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        Ok(opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data))
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }

        let stream = unsafe { TcpStream::from_raw_fd(result) };
        stream.set_nonblocking(true)?;
        let address = stream.peer_addr()?;
        Ok((stream, address))
    }
}
