use std::future::poll_fn;
use std::io::{self, IoSlice, Read, Write};
use std::mem::{self, MaybeUninit};
use std::net::{Shutdown, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{
    fd_inner::InnerRawHandle,
    op::{ConnectOp, ReadOp, WriteOp},
};

fn socket_addr_to_raw(
    address: SocketAddr,
) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
    match address {
        SocketAddr::V4(address) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in>()
                    .write(sockaddr);
                (
                    libc::AF_INET,
                    storage.assume_init(),
                    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(address) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in6>()
                    .write(sockaddr);
                (
                    libc::AF_INET6,
                    storage.assume_init(),
                    mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) -> Result<(), io::Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }

    if flags & libc::O_NONBLOCK == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

fn connect_nonblocking(address: SocketAddr) -> Result<std::net::TcpStream, io::Error> {
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket_fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let socket_fd = unsafe { OwnedFd::from_raw_fd(socket_fd) };
    set_nonblocking(socket_fd.as_raw_fd())?;

    let connect_result = unsafe {
        libc::connect(
            socket_fd.as_raw_fd(),
            (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            raw_addr_len,
        )
    };

    if connect_result == -1 {
        let err = io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EWOULDBLOCK)
        ) {
            return Err(err);
        }
    }

    let stream = unsafe { std::net::TcpStream::from_raw_fd(socket_fd.into_raw_fd()) };
    stream.set_nonblocking(true)?;
    Ok(stream)
}

pub struct TcpStream {
    inner: std::net::TcpStream,
    handle: InnerRawHandle,
}

impl TcpStream {
    pub async fn connect(address: SocketAddr) -> Result<Self, io::Error> {
        let inner = connect_nonblocking(address)?;
        let mut stream = Self::from_std(inner)?;
        poll_fn(|cx| stream.poll_connect(cx)).await?;
        Ok(stream)
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.poll_read_io(cx, buf)).await
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.poll_write_io(cx, buf)).await
    }

    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.inner.shutdown(how)
    }

    pub(crate) fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        inner.set_nonblocking(true)?;
        let handle = InnerRawHandle::new(
            inner.as_raw_fd(),
            Interest::READABLE.add(Interest::WRITABLE),
        )?;
        Ok(Self { inner, handle })
    }

    pub(crate) fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let op = ConnectOp::new(&self.handle);
        match self.handle.submit(op, cx.waker().clone()) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn poll_read_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let op = ReadOp::new(&self.handle, buf);
        match self.handle.submit(op, cx.waker().clone()) {
            Ok(read) => Poll::Ready(Ok(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn poll_write_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let op = WriteOp::new(&self.handle, buf);
        match self.handle.submit(op, cx.waker().clone()) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        self.inner.read(buf)
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.inner.flush()
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize, io::Error> {
        self.inner.write_vectored(bufs)
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();
        let unfilled = buf.initialize_unfilled();
        match this.poll_read_io(cx, unfilled) {
            Poll::Ready(Ok(read)) => {
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.get_mut().poll_write_io(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.get_mut().shutdown(Shutdown::Write))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        if let Some(non_empty) = bufs.iter().find(|buf| !buf.is_empty()) {
            this.poll_write_io(cx, non_empty)
        } else {
            Poll::Ready(Ok(0))
        }
    }
}
