use std::io::{self, ErrorKind};
use std::mem;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::{Interest, Token};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::SOCKADDR;

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, Op},
};

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
    #[cfg(unix)]
    completion_addr: Option<(*const libc::sockaddr, libc::socklen_t)>,
    #[cfg(windows)]
    completion_addr: Option<(*const SOCKADDR, i32)>,
}

impl<'a> ConnectOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            completion_addr: None,
        }
    }

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[inline]
    pub fn new_completion(handle: &'a InnerRawHandle, addr: *const SOCKADDR, addrlen: i32) -> Self {
        Self {
            handle,
            completion_addr: Some((addr, addrlen)),
        }
    }
}

/// Unified connect helper trait implemented on `InnerRawHandle`.
///
/// Exposes:
/// - `poll_connect_poll`  -> poll/readiness submission path
/// - `poll_connect_completion` -> completion submission path
/// - `poll_connect` -> high-level helper that picks the appropriate path based on mode
pub trait ConnectIo {
    /// Poll/readiness submission path (ignores address parameters).
    fn poll_connect_poll(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>>;

    /// Completion submission path (requires address pointer and length).
    #[cfg(unix)]
    fn poll_connect_completion(
        &self,
        cx: &mut Context<'_>,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>>;

    /// Completion submission path (requires address pointer and length).
    #[cfg(windows)]
    fn poll_connect_completion(
        &self,
        cx: &mut Context<'_>,
        addr: *const SOCKADDR,
        addrlen: i32,
    ) -> Poll<Result<(), io::Error>>;

    /// High-level connect that chooses completion vs poll based on registration mode.
    #[cfg(unix)]
    fn poll_connect(
        &self,
        cx: &mut Context<'_>,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>>;

    /// High-level connect that chooses completion vs poll based on registration mode.
    #[cfg(windows)]
    fn poll_connect(
        &self,
        cx: &mut Context<'_>,
        addr: *const SOCKADDR,
        addrlen: i32,
    ) -> Poll<Result<(), io::Error>>;
}

impl ConnectIo for InnerRawHandle {
    #[inline]
    fn poll_connect_poll(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.submit(ConnectOp::new(self), cx.waker().clone()) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[inline]
    fn poll_connect_completion(
        &self,
        cx: &mut Context<'_>,
        addr: *const SOCKADDR,
        addrlen: i32,
    ) -> Poll<Result<(), io::Error>> {
        completion_result_to_poll(self.submit_completion(
            ConnectOp::new_completion(self, addr, addrlen),
            cx.waker().clone(),
        ))
    }

    #[cfg(unix)]
    #[inline]
    fn poll_connect(
        &self,
        cx: &mut Context<'_>,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>> {
        if self.uses_completion() {
            self.poll_connect_completion(cx, addr, addrlen)
        } else {
            self.poll_connect_poll(cx)
        }
    }

    #[cfg(windows)]
    #[inline]
    fn poll_connect(
        &self,
        cx: &mut Context<'_>,
        addr: *const SOCKADDR,
        addrlen: i32,
    ) -> Poll<Result<(), io::Error>> {
        if self.uses_completion() {
            self.poll_connect_completion(cx, addr, addrlen)
        } else {
            self.poll_connect_poll(cx)
        }
    }
}

impl Op for ConnectOp<'_> {
    type Output = ();

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    // TODO: support Windows
    #[cfg(unix)]
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
    fn interest(&self) -> Interest {
        Interest::WRITABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn std::any::Any>>), io::Error> {
        use io_uring::{opcode, types};

        let (addr, addrlen) = self.completion_addr.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing socket address for completion-based connect",
            )
        })?;

        let entry = opcode::Connect::new(types::Fd(self.handle.handle), addr, addrlen)
            .build()
            .user_data(user_data);

        Ok((entry, None))
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(())
    }
}
