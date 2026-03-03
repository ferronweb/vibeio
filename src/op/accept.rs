use std::io;
use std::mem::{self, MaybeUninit};
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::task::{Context, Poll};

use mio::{Interest, Token};

use crate::{
    fd_inner::{InnerRawHandle, RawOsHandle},
    op::{completion_result_to_poll, Op},
};

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> Result<(), io::Error> {
    // set FD_CLOEXEC on file descriptor flags
    let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fdflags == -1 {
        return Err(io::Error::last_os_error());
    }
    if fdflags & libc::FD_CLOEXEC == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

#[cfg(unix)]
fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
) -> Result<SocketAddr, io::Error> {
    // Determine family. ss_family field is platform-dependent type; cast to c_uchar then to c_int for comparison.
    let family = storage.ss_family as libc::c_int;

    if family == libc::AF_INET {
        let addr_in: &libc::sockaddr_in =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
        let port = u16::from_be(addr_in.sin_port);
        // s_addr is in network byte order
        let ip_u32 = u32::from_be(addr_in.sin_addr.s_addr);
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == libc::AF_INET6 {
        let addr_in6: &libc::sockaddr_in6 =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            addr_in6.sin6_scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

/// Poll-based accept helper trait. Submits the operation via the poll/readiness pathway.
/// Unified accept helper trait implemented on `InnerRawHandle`.
///
/// This single trait exposes:
/// - `poll_accept` (high-level, picks completion vs poll based on registration mode)
/// - `poll_accept_poll` (poll/readiness pathway)
/// - `poll_accept_completion` (completion/pathway)
pub trait AcceptIo {
    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>>;

    fn poll_accept_poll(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>>;

    fn poll_accept_completion(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>>;
}

impl AcceptIo for InnerRawHandle {
    #[inline]
    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>> {
        if self.uses_completion() {
            self.poll_accept_completion(cx)
        } else {
            self.poll_accept_poll(cx)
        }
    }

    #[inline]
    fn poll_accept_poll(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>> {
        match self.submit(AcceptOp::new(self), cx.waker().clone()) {
            Ok((raw, address)) => Poll::Ready(Ok((raw, address))),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_accept_completion(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(RawOsHandle, SocketAddr), io::Error>> {
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
    type Output = (RawOsHandle, SocketAddr);

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    // TODO: support Windows
    #[cfg(unix)]
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

        let fd = accepted_fd as RawFd;

        // Ensure close-on-exec for the accepted fd
        set_cloexec(fd)?;

        // Obtain peer address via getpeername into a sockaddr_storage
        let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let getpeername_result = unsafe {
            libc::getpeername(
                fd,
                peer.as_mut_ptr().cast::<libc::sockaddr>(),
                &mut peer_len,
            )
        };

        if getpeername_result == -1 {
            return Err(io::Error::last_os_error());
        }

        let peer = unsafe { peer.assume_init() };
        let address = sockaddr_storage_to_socketaddr(&peer)?;
        Ok((fd as RawOsHandle, address))
    }

    #[inline]
    fn interest(&self) -> Interest {
        Interest::READABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn std::any::Any>>), io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data);

        Ok((entry, None))
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }

        let fd = result as RawFd;

        // TODO: support Windows

        // Ensure close-on-exec for the accepted fd (io_uring may have already set them)
        set_cloexec(fd)?;

        // Get peer address via getpeername
        let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let getpeername_result = unsafe {
            libc::getpeername(
                fd,
                peer.as_mut_ptr().cast::<libc::sockaddr>(),
                &mut peer_len,
            )
        };

        if getpeername_result == -1 {
            return Err(io::Error::last_os_error());
        }

        let peer = unsafe { peer.assume_init() };
        let address = sockaddr_storage_to_socketaddr(&peer)?;
        Ok((fd as RawOsHandle, address))
    }
}
