use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::task::Poll;

use mio::Interest;

use crate::{fd_inner::InnerRawHandle, net::TcpStream, op::AcceptIo};

pub struct TcpListener {
    inner: StdTcpListener,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl TcpListener {
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let inner = StdTcpListener::bind(address)?;
        let handle = ManuallyDrop::new(InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    #[inline]
    pub async fn accept(&mut self) -> Result<(TcpStream, SocketAddr), io::Error> {
        poll_fn(|cx| match self.handle.poll_accept(cx) {
            Poll::Ready(Ok((raw, address))) => {
                // Recreate a std TcpStream from the raw fd and convert it into our async TcpStream.
                // If conversion fails, the std TcpStream will be dropped and the fd closed.
                let std_stream = unsafe { std::net::TcpStream::from_raw_fd(raw) };
                match TcpStream::from_std(std_stream) {
                    Ok(stream) => Poll::Ready(Ok((stream, address))),
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        })
        .await
    }
}

impl AsRawFd for TcpListener {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

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

impl Drop for TcpListener {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
