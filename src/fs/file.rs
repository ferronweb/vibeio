use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::mem::ManuallyDrop;
use std::path::Path;

use mio::Interest;

#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, IntoRawHandle, RawHandle};

use crate::{
    driver::RegistrationMode,
    executor::current_driver,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
    op::{ReadAtOp, WriteAtOp},
};

#[cfg(windows)]
use crate::fd_inner::RawOsHandle;

use crate::fs::open_options::OpenOptions;

enum FileIo {
    Completion(ManuallyDrop<InnerRawHandle>),
    Blocking,
}

pub struct File {
    inner: std::fs::File,
    io: FileIo,
    cursor: u64,
}

impl File {
    #[inline]
    pub async fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path).await
    }

    #[inline]
    pub async fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    #[inline]
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    #[inline]
    pub fn from_std(inner: std::fs::File) -> io::Result<Self> {
        Self::from_std_with_cursor(inner, 0)
    }

    #[inline]
    pub(crate) fn from_std_with_cursor(inner: std::fs::File, cursor: u64) -> io::Result<Self> {
        let io = if let Some(driver) = current_driver() {
            if driver.supports_completion() {
                #[cfg(unix)]
                let raw_handle = inner.as_raw_fd();
                #[cfg(windows)]
                let raw_handle = RawOsHandle::Handle(inner.as_raw_handle());

                match InnerRawHandle::new_with_driver_and_mode(
                    &driver,
                    raw_handle,
                    Interest::READABLE | Interest::WRITABLE,
                    RegistrationMode::Completion,
                ) {
                    Ok(handle) => FileIo::Completion(ManuallyDrop::new(handle)),
                    Err(_) => FileIo::Blocking,
                }
            } else {
                FileIo::Blocking
            }
        } else {
            FileIo::Blocking
        };

        Ok(Self { inner, io, cursor })
    }

    #[inline]
    pub fn into_std(self) -> std::fs::File {
        let mut this = ManuallyDrop::new(self);
        unsafe {
            if let FileIo::Completion(handle) = &mut this.io {
                ManuallyDrop::drop(handle);
            }
            std::ptr::read(&this.inner)
        }
    }

    #[inline]
    fn completion_handle(&self) -> Option<&InnerRawHandle> {
        match &self.io {
            FileIo::Completion(handle) => Some(&*handle),
            FileIo::Blocking => None,
        }
    }

    #[inline]
    pub async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = ReadAtOp::new(handle, buf, offset);
            poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
        } else if current_driver().is_some() {
            read_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            read_at_blocking(&self.inner, buf, offset)
        }
    }

    #[inline]
    pub async fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            let read = self.read_at(buf, offset).await?;
            if read == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }

            offset = offset.saturating_add(read as u64);
            let (_, rest) = buf.split_at_mut(read);
            buf = rest;
        }

        Ok(())
    }

    #[inline]
    pub async fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = WriteAtOp::new(handle, buf, offset);
            poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
        } else if current_driver().is_some() {
            write_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            write_at_blocking(&self.inner, buf, offset)
        }
    }

    #[inline]
    pub async fn write_exact_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            let written = self.write_at(buf, offset).await?;
            if written == 0 {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }

            offset = offset.saturating_add(written as u64);
            buf = &buf[written..];
        }

        Ok(())
    }

    // TODO: metadata-related methods (`metadata`, `set_permissions`, timestamps, etc.).
}

#[cfg(unix)]
#[inline]
fn read_at_blocking(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
#[inline]
fn read_at_blocking(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
#[inline]
fn write_at_blocking(file: &std::fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
#[inline]
fn write_at_blocking(file: &std::fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[inline]
pub(crate) fn blocking_pool_io_error() -> io::Error {
    io::Error::new(ErrorKind::Other, "can't spawn blocking task for file I/O")
}

async fn read_at_in_blocking_pool(
    file: &std::fs::File,
    buf: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let file = file.try_clone()?;
    let len = buf.len();
    let (read, tmp) = crate::spawn_blocking(move || {
        let mut tmp = vec![0u8; len];
        let read = read_at_blocking(&file, &mut tmp, offset)?;
        Ok::<(usize, Vec<u8>), io::Error>((read, tmp))
    })
    .await
    .map_err(|_| blocking_pool_io_error())??;

    buf[..read].copy_from_slice(&tmp[..read]);
    Ok(read)
}

async fn write_at_in_blocking_pool(
    file: &std::fs::File,
    buf: &[u8],
    offset: u64,
) -> io::Result<usize> {
    let file = file.try_clone()?;
    let tmp = buf.to_vec();
    crate::spawn_blocking(move || write_at_blocking(&file, &tmp, offset))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

impl AsyncRead for File {
    #[inline]
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let read = self.read_at(buf, self.cursor).await?;
        self.cursor = self.cursor.saturating_add(read as u64);
        Ok(read)
    }
}

impl AsyncWrite for File {
    #[inline]
    async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        let written = self.write_at(buf, self.cursor).await?;
        self.cursor = self.cursor.saturating_add(written as u64);
        Ok(written)
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl Drop for File {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            if let FileIo::Completion(handle) = &mut self.io {
                ManuallyDrop::drop(handle);
            }
        }
    }
}

#[cfg(unix)]
impl AsRawFd for File {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for File {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawHandle for File {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.as_raw_handle()
    }
}

#[cfg(windows)]
impl IntoRawHandle for File {
    #[inline]
    fn into_raw_handle(self) -> RawHandle {
        self.into_std().into_raw_handle()
    }
}
