use std::io;
use std::mem;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
    #[cfg(unix)]
    addr: (*const libc::sockaddr, libc::socklen_t),
    #[cfg(windows)]
    addr: (*const SOCKADDR, i32),
    completion_token: Option<usize>,
}

impl<'a> ConnectOp<'a> {
    #[cfg(unix)]
    #[inline]
    pub fn new(
        handle: &'a InnerRawHandle,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Self {
        Self {
            handle,
            addr: (addr, addrlen),
            completion_token: None,
        }
    }

    #[cfg(windows)]
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, addr: *const SOCKADDR, addrlen: i32) -> Self {
        Self {
            handle,
            addr: (addr, addrlen),
            completion_token: None,
        }
    }
}

impl Op for ConnectOp<'_> {
    type Output = ();

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
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
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        if socket_error != 0 {
            if matches!(
                socket_error,
                libc::EINPROGRESS | libc::EALREADY | libc::EWOULDBLOCK
            ) {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(socket_error)));
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
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }

            return Poll::Ready(Err(err));
        }

        Poll::Ready(Ok(()))
    }

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            // Get the completion result
            match driver.get_completion_result(completion_token) {
                Some(result) => result,
                None => {
                    // The completion is not ready yet
                    return Poll::Pending;
                }
            }
        } else {
            // Submit the op
            match driver.submit_completion(self, cx.waker().clone()) {
                CompletionIoResult::Ok(result) => result,
                CompletionIoResult::Retry(token) => {
                    self.completion_token = Some(token);
                    return Poll::Pending;
                }
                CompletionIoResult::SubmitErr(err) => return Poll::Ready(Err(err)),
            }
        };
        if result < 0 {
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }
        Poll::Ready(Ok(()))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let (addr, addrlen) = self.addr;

        let entry = opcode::Connect::new(types::Fd(self.handle.handle), addr, addrlen)
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}
