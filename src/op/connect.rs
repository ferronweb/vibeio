use std::io::{self, ErrorKind};
use std::mem;
use std::mem::MaybeUninit;

use mio::Token;

use crate::{fd_inner::InnerRawHandle, op::Op};

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
}

impl<'a> ConnectOp<'a> {
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self { handle }
    }
}

impl Op for ConnectOp<'_> {
    type Output = ();

    fn token(&self) -> Token {
        self.handle.token()
    }

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
}
