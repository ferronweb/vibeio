use std::io;
use std::task::{Context, Poll};

use futures_util::future::LocalBoxFuture;
#[cfg(unix)]
use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self, SOCKET, WSABUF, WSA_IO_PENDING},
    Storage::FileSystem::WriteFile,
    System::IO::OVERLAPPED,
};

use crate::blocking::SpawnBlockingError;
use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::{InnerRawHandle, RawOsHandle};
use crate::op::Op;

pub struct WriteOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a [u8],
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<WSABUF>,
    blocking: bool,
    blocking_future: Option<LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>>,
}

impl<'a> WriteOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            blocking: false,
            blocking_future: None,
        }
    }

    #[inline]
    pub fn new_blocking(inner: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self {
            handle: inner,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            blocking: true,
            blocking_future: None,
        }
    }
}

impl Op for WriteOp<'_> {
    type Output = usize;

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let written = if !self.blocking {
            unsafe {
                libc::write(
                    self.handle.handle,
                    self.buf.as_ptr().cast::<libc::c_void>(),
                    self.buf.len(),
                )
            }
        } else {
            use futures_util::FutureExt;
            use std::task::ready;

            let mut blocking_future = if let Some(blocking_future) = self.blocking_future.take() {
                blocking_future
            } else {
                let buf: &[u8] = unsafe { std::mem::transmute::<&[u8], &[u8]>(self.buf) };
                let handle = self.handle.handle;
                Box::pin(crate::spawn_blocking(move || unsafe {
                    libc::write(handle, buf.as_ptr().cast::<libc::c_void>(), buf.len())
                }))
            };
            match ready!(blocking_future.poll_unpin(cx)) {
                Ok(written) => written,
                Err(_) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        io::ErrorKind::Other,
                        "can't spawn blocking task for I/O",
                    )))
                }
            }
        };

        if written == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        Poll::Ready(Ok(written as usize))
    }

    #[cfg(any(unix, windows))]
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
        Poll::Ready(Ok(result as usize))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let write_len = u32::try_from(self.buf.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write buffer is too large for Windows socket I/O",
                    )
                })?;

                let wsabuf = self.socket_buf.get_or_insert(WSABUF {
                    len: 0,
                    buf: std::ptr::null_mut(),
                });
                wsabuf.len = write_len;
                wsabuf.buf = self.buf.as_ptr().cast_mut().cast();

                let send_result = unsafe {
                    WinSock::WSASend(
                        socket as SOCKET,
                        wsabuf as *mut WSABUF,
                        1,
                        std::ptr::null_mut(),
                        0,
                        overlapped,
                        None,
                    )
                };

                if send_result == 0 {
                    return Ok(());
                }

                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(err))
                }
            }
            RawOsHandle::Handle(handle) => {
                let write_len = u32::try_from(self.buf.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write buffer is too large for Windows file I/O",
                    )
                })?;

                let write_result = unsafe {
                    WriteFile(
                        handle as HANDLE,
                        self.buf.as_ptr().cast(),
                        write_len,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if write_result != 0 {
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Write::new(
            types::Fd(self.handle.handle),
            self.buf.as_ptr(),
            self.buf.len() as _,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
