use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

use crate::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
    op::{CompletionConnectIo, CompletionReadIo, CompletionWriteIo, ConnectOp, ReadOp, WriteOp},
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
                // sin_len is required on *BSD and macOS
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd"
                ))]
                sin_len: 0,
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
                // sin6_len is required on *BSD and macOS
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd"
                ))]
                sin6_len: 0,
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

fn new_nonblocking_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, libc::sockaddr_storage, libc::socklen_t), io::Error> {
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket_fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let socket_fd = unsafe { OwnedFd::from_raw_fd(socket_fd) };
    set_nonblocking(socket_fd.as_raw_fd())?;

    let stream = unsafe { std::net::TcpStream::from_raw_fd(socket_fd.into_raw_fd()) };
    stream.set_nonblocking(true)?;
    Ok((stream, raw_addr, raw_addr_len))
}

fn start_nonblocking_connect(
    fd: RawFd,
    raw_addr: &libc::sockaddr_storage,
    raw_addr_len: libc::socklen_t,
) -> Result<(), io::Error> {
    let connect_result = unsafe {
        libc::connect(
            fd,
            (raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            raw_addr_len,
        )
    };

    if connect_result == -1 {
        let err = io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EWOULDBLOCK) | Some(libc::EALREADY)
        ) {
            return Err(err);
        }
    }

    Ok(())
}

pub struct TcpStream {
    inner: std::net::TcpStream,
    handle: ManuallyDrop<InnerRawHandle>,
}

/// A poll-only variant that always uses readiness-based operations.
pub struct PollTcpStream {
    stream: TcpStream,
}

impl TcpStream {
    pub async fn connect(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let mut addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        while let Some(address) = addresses.next() {
            match Self::connect_one(address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    async fn connect_one(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_nonblocking_socket(address)?;
        let mut stream = Self::from_std(inner)?;

        if stream.handle.uses_completion() {
            let raw_addr = Box::new(raw_addr);
            poll_fn(|cx| {
                let raw_addr_ptr =
                    (&*raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>();
                stream.poll_connect_completion_io(cx, raw_addr_ptr, raw_addr_len)
            })
            .await?;
        } else {
            start_nonblocking_connect(stream.inner.as_raw_fd(), &raw_addr, raw_addr_len)?;
            poll_fn(|cx| stream.poll_connect_poll_io(cx)).await?;
        }

        Ok(stream)
    }

    #[inline]
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.poll_read_io(cx, buf)).await
    }

    #[inline]
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.poll_write_io(cx, buf)).await
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.inner.shutdown(how)
    }

    #[inline]
    pub(crate) fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: std::net::TcpStream,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        inner.set_nonblocking(true)?;
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE.add(Interest::WRITABLE),
            mode,
        )?);
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn into_poll(self) -> Result<PollTcpStream, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        Ok(PollTcpStream { stream })
    }

    #[inline]
    fn poll_connect_poll_io(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let op = ConnectOp::new(&self.handle);
        match self.handle.submit(op, cx.waker().clone()) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_connect_completion_io(
        &mut self,
        cx: &mut Context<'_>,
        raw_addr: *const libc::sockaddr,
        raw_addr_len: libc::socklen_t,
    ) -> Poll<Result<(), io::Error>> {
        self.handle
            .poll_connect_completion(cx, raw_addr, raw_addr_len)
    }

    #[inline]
    fn poll_read_poll_io(
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

    #[inline]
    fn poll_read_completion_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.handle.poll_read_completion(cx, buf)
    }

    #[inline]
    fn poll_write_poll_io(
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

    #[inline]
    fn poll_write_completion_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.handle.poll_write_completion(cx, buf)
    }

    #[inline]
    fn poll_read_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.handle.uses_completion() {
            self.poll_read_completion_io(cx, buf)
        } else {
            self.poll_read_poll_io(cx, buf)
        }
    }

    #[inline]
    fn poll_write_io(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.handle.uses_completion() {
            self.poll_write_completion_io(cx, buf)
        } else {
            self.poll_write_poll_io(cx, buf)
        }
    }
}

impl PollTcpStream {
    #[inline]
    pub async fn connect(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_nonblocking_socket(address)?;
        let mut stream = TcpStream::from_std_with_mode(inner, RegistrationMode::Poll)?;
        start_nonblocking_connect(stream.inner.as_raw_fd(), &raw_addr, raw_addr_len)?;
        poll_fn(|cx| stream.poll_connect_poll_io(cx)).await?;
        Ok(Self { stream })
    }

    #[inline]
    pub(crate) fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Ok(Self {
            stream: TcpStream::from_std_with_mode(inner, RegistrationMode::Poll)?,
        })
    }

    #[inline]
    pub fn into_adaptive(self) -> TcpStream {
        self.stream
    }

    #[inline]
    pub fn into_completion(self) -> Result<TcpStream, io::Error> {
        let mut stream = self.stream;
        stream.handle.rebind_mode(RegistrationMode::Completion)?;
        Ok(stream)
    }

    #[inline]
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.stream.poll_read_poll_io(cx, buf)).await
    }

    #[inline]
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        poll_fn(|cx| self.stream.poll_write_poll_io(cx, buf)).await
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.local_addr()
    }

    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.peer_addr()
    }

    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.stream.shutdown(how)
    }
}

impl AsRawFd for TcpStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsRawFd for PollTcpStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.stream.inner.as_raw_fd()
    }
}

impl IntoRawFd for TcpStream {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its fd ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_fd()
        }
    }
}

impl IntoRawFd for PollTcpStream {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl AsyncRead for TcpStream {
    #[inline]
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        TcpStream::read(self, buf).await
    }
}

impl TokioAsyncRead for PollTcpStream {
    #[inline]
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
        match this.stream.poll_read_poll_io(cx, unfilled) {
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
    #[inline]
    async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        TcpStream::write(self, buf).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl TokioAsyncWrite for PollTcpStream {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.get_mut().stream.poll_write_poll_io(cx, buf)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.get_mut().shutdown(Shutdown::Write))
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        if let Some(non_empty) = bufs.iter().find(|buf| !buf.is_empty()) {
            this.stream.poll_write_poll_io(cx, non_empty)
        } else {
            Poll::Ready(Ok(0))
        }
    }
}

impl Drop for TcpStream {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
