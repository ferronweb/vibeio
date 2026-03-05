use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{
    SocketAddr, UnixListener as StdUnixListener, UnixStream as StdUnixStream,
};
use std::path::Path;

use mio::Interest;

use crate::fd_inner::InnerRawHandle;
use crate::net::UnixStream;
use crate::op::AcceptUnixOp;

pub struct UnixListener {
    inner: StdUnixListener,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl UnixListener {
    #[inline]
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let inner = StdUnixListener::bind(path)?;
        Self::from_std(inner)
    }

    #[inline]
    pub fn from_std(inner: StdUnixListener) -> Result<Self, io::Error> {
        let handle = ManuallyDrop::new(InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    #[inline]
    pub async fn accept(&self) -> Result<(UnixStream, SocketAddr), io::Error> {
        let mut op = AcceptUnixOp::new(&self.handle);
        let raw = poll_fn(move |cx| self.handle.poll_op(cx, &mut op)).await?;
        let std_stream = unsafe { StdUnixStream::from_raw_fd(raw) };
        let address = std_stream.peer_addr()?;
        let stream = UnixStream::from_std(std_stream)?;
        Ok((stream, address))
    }
}

impl AsRawFd for UnixListener {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for UnixListener {
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

impl Drop for UnixListener {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
