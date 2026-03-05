use std::future::poll_fn;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
use std::path::Path;

use mio::Interest;

use crate::op::{ConnectOp, ReadOp, ReadvOp, WriteOp, WritevOp};
use crate::{
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

#[inline]
fn socket_addr_to_raw(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), io::Error> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty socket path",
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path contains interior NUL byte",
        ));
    }

    let mut sockaddr = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
    sockaddr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let max_path_len = sockaddr.sun_path.len();
    if bytes.len() >= max_path_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }

    for (index, byte) in bytes.iter().copied().enumerate() {
        sockaddr.sun_path[index] = byte as libc::c_char;
    }

    let addr_len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "haiku",
        target_os = "aix",
    ))]
    {
        sockaddr.sun_len = addr_len as libc::sa_family_t;
    }

    Ok((sockaddr, addr_len))
}

#[inline]
fn new_socket(
    path: &Path,
) -> Result<(StdUnixStream, libc::sockaddr_un, libc::socklen_t), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(path)?;
    let socket_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { StdUnixStream::from_raw_fd(socket_fd) };
    Ok((stream, raw_addr, raw_addr_len))
}

pub struct UnixStream {
    inner: StdUnixStream,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl UnixStream {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(path.as_ref())?;
        let stream = Self::from_std(inner)?;

        let raw_addr_ptr = (&raw_addr as *const libc::sockaddr_un).cast::<libc::sockaddr>();
        let handle = &stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
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
    pub fn from_std(inner: StdUnixStream) -> Result<Self, io::Error> {
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }
}

impl AsRawFd for UnixStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for UnixStream {
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

impl AsyncRead for UnixStream {
    #[inline]
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let handle = &self.handle;
        let mut op = ReadOp::new(handle, buf);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
    }

    #[inline]
    async fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> Result<usize, io::Error> {
        if bufs.is_empty() {
            return Ok(0);
        }
        let handle = &self.handle;
        let mut op = ReadvOp::new(handle, bufs);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
    }
}

impl AsyncWrite for UnixStream {
    #[inline]
    async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        let handle = &self.handle;
        let mut op = WriteOp::new(handle, buf);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    #[inline]
    async fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize, io::Error> {
        if bufs.is_empty() {
            return Ok(0);
        }
        let handle = &self.handle;
        let mut op = WritevOp::new(handle, bufs);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
    }
}

impl Drop for UnixStream {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
