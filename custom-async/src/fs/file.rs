use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::mem::ManuallyDrop;
use std::path::Path;

use mio::Interest;

#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, IntoRawHandle, RawHandle};

use crate::fs::Metadata;
use crate::io::{IoBuf, IoBufMut, IoBufWithCursor};
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
            FileIo::Completion(handle) => Some(handle),
            FileIo::Blocking => None,
        }
    }

    #[inline]
    pub async fn read_at<B: IoBufMut>(&self, mut buf: B, offset: u64) -> (io::Result<usize>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = ReadAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if current_driver().is_some() {
            read_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) };
            (read_at_blocking(&self.inner, slice, offset), buf)
        }
    }

    #[inline]
    pub async fn read_exact_at<B: IoBufMut>(&self, buf: B, mut offset: u64) -> (io::Result<()>, B) {
        let mut buf = IoBufWithCursor::new(buf);
        while buf.buf_len() > 0 {
            let (read, mut buf_returned) = self.read_at(buf, offset).await;
            let read = match read {
                Ok(read) => read,
                Err(err) => {
                    return (Err(err), buf_returned.into_inner());
                }
            };
            if read == 0 {
                return (
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    )),
                    buf_returned.into_inner(),
                );
            }

            offset = offset.saturating_add(read as u64);
            buf_returned.advance(read);
            buf = buf_returned
        }

        (Ok(()), buf.into_inner())
    }

    #[inline]
    pub async fn write_at<B: IoBuf>(&self, buf: B, offset: u64) -> (io::Result<usize>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = WriteAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if current_driver().is_some() {
            write_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
            (write_at_blocking(&self.inner, slice, offset), buf)
        }
    }

    #[inline]
    pub async fn write_exact_at<B: IoBuf>(&self, buf: B, mut offset: u64) -> (io::Result<()>, B) {
        let mut buf = IoBufWithCursor::new(buf);
        while buf.buf_len() > 0 {
            let (written, mut buf_returned) = self.write_at(buf, offset).await;
            let written = match written {
                Ok(written) => written,
                Err(err) => {
                    return (Err(err), buf_returned.into_inner());
                }
            };
            if written == 0 {
                return (
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to write whole buffer",
                    )),
                    buf_returned.into_inner(),
                );
            }

            offset = offset.saturating_add(written as u64);
            buf_returned.advance(written);
            buf = buf_returned;
        }

        (Ok(()), buf.into_inner())
    }

    #[inline]
    pub async fn sync_all(&self) -> io::Result<()> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(target_os = "linux")]
            {
                let mut op = crate::op::FsyncOp::new(handle, false);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_all_in_blocking_pool(&self.inner).await
            }
        } else if current_driver().is_some() {
            sync_all_in_blocking_pool(&self.inner).await
        } else {
            sync_all_blocking(&self.inner)
        }
    }

    #[inline]
    pub async fn sync_data(&self) -> io::Result<()> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(target_os = "linux")]
            {
                let mut op = crate::op::FsyncOp::new(handle, true);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_data_in_blocking_pool(&self.inner).await
            }
        } else if current_driver().is_some() {
            sync_data_in_blocking_pool(&self.inner).await
        } else {
            sync_data_blocking(&self.inner)
        }
    }

    #[inline]
    pub async fn metadata(&self) -> io::Result<Metadata> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            {
                use std::ffi::CString;

                let mut op = crate::op::StatxOp::new(
                    handle.handle,
                    CString::new(b"").expect("invalid path"),
                    libc::AT_EMPTY_PATH,
                    libc::STATX_ALL,
                );
                let statx = poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;
                Ok(Metadata::from_statx(statx))
            }
            #[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
            {
                let _ = handle;
                metadata_in_blocking_pool(&self.inner).await
            }
        } else if current_driver().is_some() {
            metadata_in_blocking_pool(&self.inner).await
        } else {
            metadata_blocking(&self.inner)
        }
    }
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
fn sync_all_blocking(file: &std::fs::File) -> io::Result<()> {
    file.sync_all()
}

#[inline]
fn sync_data_blocking(file: &std::fs::File) -> io::Result<()> {
    file.sync_data()
}

#[inline]
fn metadata_blocking(file: &std::fs::File) -> io::Result<Metadata> {
    Ok(Metadata::from_std(file.metadata()?))
}

#[inline]
pub(crate) fn blocking_pool_io_error() -> io::Error {
    io::Error::other("can't spawn blocking task for file I/O")
}

#[inline]
async fn read_at_in_blocking_pool<B: IoBufMut>(
    file: &std::fs::File,
    mut buf: B,
    offset: u64,
) -> (io::Result<usize>, B) {
    let file = match file.try_clone() {
        Ok(file) => file,
        Err(e) => return (Err(e), buf),
    };
    let temp_slice: &'static mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) };
    (
        crate::spawn_blocking(move || read_at_blocking(&file, temp_slice, offset))
            .await
            .unwrap_or_else(|_| Err(blocking_pool_io_error())),
        buf,
    )
}

#[inline]
async fn write_at_in_blocking_pool<B: IoBuf>(
    file: &std::fs::File,
    buf: B,
    offset: u64,
) -> (io::Result<usize>, B) {
    let file = match file.try_clone() {
        Ok(file) => file,
        Err(e) => return (Err(e), buf),
    };
    let temp_slice: &'static [u8] =
        unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
    (
        crate::spawn_blocking(move || write_at_blocking(&file, temp_slice, offset))
            .await
            .unwrap_or_else(|_| Err(blocking_pool_io_error())),
        buf,
    )
}

#[inline]
async fn sync_all_in_blocking_pool(file: &std::fs::File) -> io::Result<()> {
    let file = file.try_clone()?;
    crate::spawn_blocking(move || sync_all_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn sync_data_in_blocking_pool(file: &std::fs::File) -> io::Result<()> {
    let file = file.try_clone()?;
    crate::spawn_blocking(move || sync_data_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn metadata_in_blocking_pool(file: &std::fs::File) -> io::Result<Metadata> {
    let file = file.try_clone()?;
    crate::spawn_blocking(move || metadata_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

impl AsyncRead for File {
    // TODO: use Readv and Read ops
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let (read, buf) = self.read_at(buf, self.cursor).await;
        if let Ok(read) = read {
            self.cursor = self.cursor.saturating_add(read as u64);
        }
        (read, buf)
    }
}

impl AsyncWrite for File {
    // TODO: use Writev and Write ops
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let (written, buf) = self.write_at(buf, self.cursor).await;
        if let Ok(written) = written {
            self.cursor = self.cursor.saturating_add(written as u64);
        }
        (written, buf)
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
