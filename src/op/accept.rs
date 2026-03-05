use std::io;
use std::mem::{self, MaybeUninit};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::{InnerRawHandle, RawOsHandle};
use crate::op::Op;

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

pub struct AcceptOp<'a> {
    handle: &'a InnerRawHandle,
    completion_token: Option<usize>,
}

impl<'a> AcceptOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            completion_token: None,
        }
    }
}

impl Op for AcceptOp<'_> {
    type Output = (RawOsHandle, SocketAddr);

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let accepted_fd = unsafe {
            libc::accept(
                self.handle.handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if accepted_fd == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        let fd = accepted_fd as RawFd;

        // Ensure close-on-exec for the accepted fd
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

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
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        let peer = unsafe { peer.assume_init() };
        let address = sockaddr_storage_to_socketaddr(&peer)?;
        Poll::Ready(Ok((fd as RawOsHandle, address)))
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
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    // The completion is not ready yet
                    driver.set_completion_waker(completion_token, cx.waker().clone());
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

        let fd = result as RawFd;

        // Ensure close-on-exec for the accepted fd (io_uring may have already set them)
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

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
            return Poll::Ready(Err(io::Error::last_os_error()));
        }

        let peer = unsafe { peer.assume_init() };
        let address = sockaddr_storage_to_socketaddr(&peer)?;
        Poll::Ready(Ok((fd as RawOsHandle, address)))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
