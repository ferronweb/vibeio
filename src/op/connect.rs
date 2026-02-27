use std::io::{self, ErrorKind};
use std::mem;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::Token;

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, CompletionKind, Op},
};

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
    completion_addr: Option<(*const libc::sockaddr, libc::socklen_t)>,
}

impl<'a> ConnectOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            completion_addr: None,
        }
    }

    #[inline]
    pub fn new_completion(
        handle: &'a InnerRawHandle,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Self {
        Self {
            handle,
            completion_addr: Some((addr, addrlen)),
        }
    }
}

pub trait CompletionConnectIo {
    fn poll_connect_completion(
        &self,
        cx: &mut Context<'_>,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>>;
}

impl CompletionConnectIo for InnerRawHandle {
    #[inline]
    fn poll_connect_completion(
        &self,
        cx: &mut Context<'_>,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>> {
        completion_result_to_poll(self.submit_completion(
            ConnectOp::new_completion(self, addr, addrlen),
            cx.waker().clone(),
        ))
    }
}

impl Op for ConnectOp<'_> {
    type Output = ();

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    #[inline]
    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        let mut socket_error: libc::c_int = 0;
        let mut socket_error_len = mem::size_of::<libc::c_int>() as libc::socklen_t;
        let getsockopt_result = unsafe {
            libc::getsockopt(
                self.handle.handle,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut socket_error_len,
            )
        };
        if getsockopt_result == -1 {
            return Err(io::Error::last_os_error());
        }

        if socket_error != 0 {
            if matches!(
                socket_error,
                libc::EINPROGRESS | libc::EALREADY | libc::EWOULDBLOCK
            ) {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "connect is in progress",
                ));
            }
            return Err(io::Error::from_raw_os_error(socket_error));
        }

        let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let getpeername_result = unsafe {
            libc::getpeername(
                self.handle.handle,
                peer.as_mut_ptr().cast::<libc::sockaddr>(),
                &mut peer_len,
            )
        };

        if getpeername_result == -1 {
            let err = io::Error::last_os_error();
            if matches!(
                err.raw_os_error(),
                Some(libc::EINPROGRESS)
                    | Some(libc::EALREADY)
                    | Some(libc::EWOULDBLOCK)
                    | Some(libc::ENOTCONN)
            ) {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "connect is in progress",
                ));
            }

            return Err(err);
        }

        Ok(())
    }

    #[inline]
    fn completion_kind(&self) -> Option<CompletionKind> {
        Some(CompletionKind::Connect)
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let (addr, addrlen) = self.completion_addr.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing socket address for completion-based connect",
            )
        })?;

        Ok(
            opcode::Connect::new(types::Fd(self.handle.handle), addr, addrlen)
                .build()
                .user_data(user_data),
        )
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(())
    }
}
