use std::io;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, AF_INET, AF_INET6, MSG_PEEK, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKADDR_STORAGE, SOCKET, WSABUF, WSA_IO_PENDING,
    },
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::op::Op;

#[cfg(unix)]
#[inline]
fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family as libc::c_int;

    if family == libc::AF_INET {
        let addr_in: &libc::sockaddr_in =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
        let port = u16::from_be(addr_in.sin_port);
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

#[cfg(windows)]
#[inline]
fn sockaddr_storage_to_socketaddr(storage: &SOCKADDR_STORAGE) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family;

    if family == AF_INET {
        let addr_in: &SOCKADDR_IN = unsafe { &*(storage as *const _ as *const SOCKADDR_IN) };
        let port = u16::from_be(addr_in.sin_port);
        let ip_u32 = u32::from_be(unsafe { addr_in.sin_addr.S_un.S_addr });
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == AF_INET6 {
        let addr_in6: &SOCKADDR_IN6 = unsafe { &*(storage as *const _ as *const SOCKADDR_IN6) };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(unsafe { addr_in6.sin6_addr.u.Byte });
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            unsafe { addr_in6.Anonymous.sin6_scope_id },
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
#[inline]
fn socket_recvfrom(socket: SOCKET, buf: &mut [u8], peek: bool) -> io::Result<(usize, SocketAddr)> {
    use windows_sys::Win32::Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKADDR, SOCKADDR_STORAGE, SOCKET_ERROR, WSABUF,
    };

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = if peek { MSG_PEEK as u32 } else { 0 };
    let mut addr = SOCKADDR_STORAGE::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;

    let recv_result = unsafe {
        WinSock::WSARecvFrom(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            (&mut addr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            &mut addr_len,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    let address = sockaddr_storage_to_socketaddr(&addr)?;
    Ok((bytes as usize, address))
}

pub struct RecvfromOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a mut [u8],
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<WSABUF>,
    #[cfg(windows)]
    completion_addr: Option<SOCKADDR_STORAGE>,
    #[cfg(windows)]
    completion_addr_len: i32,
    #[cfg(windows)]
    completion_flags: u32,
    #[cfg(target_os = "linux")]
    completion_addr: Option<libc::sockaddr_storage>,
    #[cfg(target_os = "linux")]
    completion_iovec: Option<libc::iovec>,
    #[cfg(target_os = "linux")]
    completion_msghdr: Option<libc::msghdr>,
    peek: bool,
}

impl<'a> RecvfromOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            #[cfg(windows)]
            completion_addr: None,
            #[cfg(windows)]
            completion_addr_len: 0,
            #[cfg(windows)]
            completion_flags: 0,
            #[cfg(target_os = "linux")]
            completion_addr: None,
            #[cfg(target_os = "linux")]
            completion_iovec: None,
            #[cfg(target_os = "linux")]
            completion_msghdr: None,
            peek: false,
        }
    }

    #[inline]
    pub fn new_peek(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            #[cfg(windows)]
            completion_addr: None,
            #[cfg(windows)]
            completion_addr_len: 0,
            #[cfg(windows)]
            completion_flags: 0,
            #[cfg(target_os = "linux")]
            completion_addr: None,
            #[cfg(target_os = "linux")]
            completion_iovec: None,
            #[cfg(target_os = "linux")]
            completion_msghdr: None,
            peek: true,
        }
    }
}

impl Op for RecvfromOp<'_> {
    type Output = (usize, SocketAddr);

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(unix)]
        let result = {
            let mut addr = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let read = unsafe {
                libc::recvfrom(
                    self.handle.handle,
                    self.buf.as_mut_ptr().cast::<libc::c_void>(),
                    self.buf.len(),
                    if self.peek { libc::MSG_PEEK } else { 0 },
                    addr.as_mut_ptr().cast::<libc::sockaddr>(),
                    &mut addr_len,
                )
            };

            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                let address = sockaddr_storage_to_socketaddr(unsafe { &addr.assume_init() })?;
                Ok((read as usize, address))
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_recvfrom(socket as SOCKET, self.buf, self.peek),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based recvfrom currently supports sockets only on Windows",
            )),
        };

        match result {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                match driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE) {
                    Ok(_) => Poll::Pending,
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
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
            #[cfg(windows)]
            {
                self.completion_addr = None;
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        #[cfg(target_os = "linux")]
        {
            let address = self
                .completion_addr
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        "recvfrom completion missing source address",
                    )
                })
                .and_then(sockaddr_storage_to_socketaddr);
            return Poll::Ready(address.map(|address| (result as usize, address)));
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "completion-based recvfrom is unsupported on this Unix platform",
            )));
        }

        #[cfg(windows)]
        {
            let address = self
                .completion_addr
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        "recvfrom completion missing source address",
                    )
                })
                .and_then(sockaddr_storage_to_socketaddr);
            self.completion_addr = None;
            return Poll::Ready(address.map(|address| (result as usize, address)));
        }
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSARecvFrom can be used only with sockets",
            ));
        };

        let read_len = u32::try_from(self.buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read buffer is too large for Windows socket I/O",
            )
        })?;

        let wsabuf = self.socket_buf.get_or_insert(WSABUF {
            len: 0,
            buf: std::ptr::null_mut(),
        });
        wsabuf.len = read_len;
        wsabuf.buf = self.buf.as_mut_ptr().cast();

        if self.completion_addr.is_none() {
            self.completion_addr = Some(SOCKADDR_STORAGE::default());
        }
        self.completion_addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
        self.completion_flags = if self.peek { MSG_PEEK as u32 } else { 0 };

        let addr_ptr = self
            .completion_addr
            .as_mut()
            .expect("completion_addr should be initialized");
        let recv_result = unsafe {
            WinSock::WSARecvFrom(
                socket as SOCKET,
                wsabuf as *mut WSABUF,
                1,
                std::ptr::null_mut(),
                &mut self.completion_flags,
                (addr_ptr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                &mut self.completion_addr_len,
                overlapped,
                None,
            )
        };

        if recv_result == 0 {
            return Ok(());
        }

        let err = unsafe { WinSock::WSAGetLastError() };
        if err == WSA_IO_PENDING {
            Ok(())
        } else {
            self.completion_addr = None;
            Err(io::Error::from_raw_os_error(err))
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        self.completion_addr = Some(unsafe { std::mem::zeroed() });
        self.completion_iovec = Some(libc::iovec {
            iov_base: self.buf.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: self.buf.len(),
        });
        self.completion_msghdr = Some(libc::msghdr {
            msg_name: self
                .completion_addr
                .as_mut()
                .expect("completion_addr should be initialized")
                as *mut libc::sockaddr_storage as *mut libc::c_void,
            msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            msg_iov: self
                .completion_iovec
                .as_mut()
                .expect("completion_iovec should be initialized")
                as *mut libc::iovec,
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        });

        let entry = opcode::RecvMsg::new(
            types::Fd(self.handle.handle),
            self.completion_msghdr
                .as_mut()
                .expect("completion_msghdr should be initialized") as *mut libc::msghdr,
        )
        .flags(if self.peek { libc::MSG_PEEK as u32 } else { 0 })
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
