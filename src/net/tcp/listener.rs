use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use mio::Interest;

use crate::op::AcceptOp;
use crate::{fd_inner::InnerRawHandle, net::TcpStream};

pub struct TcpListener {
    inner: StdTcpListener,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl TcpListener {
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let inner = StdTcpListener::bind(address)?;
        Self::from_std(inner)
    }

    #[inline]
    pub fn from_std(inner: std::net::TcpListener) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    #[inline]
    pub async fn accept(&self) -> Result<(TcpStream, SocketAddr), io::Error> {
        let mut op = AcceptOp::new(&self.handle);
        let (raw, address) = poll_fn(move |cx| self.handle.poll_op(cx, &mut op)).await?;
        // Recreate a std TcpStream from the raw fd and convert it into our async TcpStream.
        // If conversion fails, the std TcpStream will be dropped and the fd closed.
        #[cfg(unix)]
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(raw) };
        #[cfg(windows)]
        let crate::fd_inner::RawOsHandle::Socket(raw) = raw
        else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid raw handle",
            )));
        };
        #[cfg(windows)]
        let std_stream = unsafe { std::net::TcpStream::from_raw_socket(raw) };
        match TcpStream::from_std(std_stream) {
            Ok(stream) => Ok((stream, address)),
            Err(err) => Err(err),
        }
    }
}

#[cfg(unix)]
impl AsRawFd for TcpListener {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for TcpListener {
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

#[cfg(windows)]
impl AsRawSocket for TcpListener {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for TcpListener {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its socket ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_socket()
        }
    }
}

impl Drop for TcpListener {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
