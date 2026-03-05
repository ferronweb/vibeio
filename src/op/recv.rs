use std::io;
use std::task::{Context, Poll};

use futures_util::future::LocalBoxFuture;
use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self, MSG_PEEK, SOCKET, WSABUF, WSA_IO_PENDING},
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::{InnerRawHandle, RawOsHandle};
use crate::op::io_util::poll_result_or_wait;
#[cfg(windows)]
use crate::op::io_util::socket_read;
use crate::op::Op;

pub struct RecvOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a mut [u8],
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<WSABUF>,
    peek: bool,
}

impl<'a> RecvOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            peek: false,
        }
    }

    pub fn new_peek(handle: &'a InnerRawHandle, buf: &'a mut [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
            peek: true,
        }
    }
}

impl Op for RecvOp<'_> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(unix)]
        let result = {
            let read = unsafe {
                libc::recv(
                    self.handle.handle,
                    self.buf.as_mut_ptr().cast::<libc::c_void>(),
                    self.buf.len(),
                    if self.peek { libc::MSG_PEEK } else { 0 },
                )
            };
            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(read as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_read(socket as SOCKET, self.buf),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based recv currently supports sockets only on Windows",
            )),
        };

        poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE)
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
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSARecv can be used only with listening sockets",
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

        let mut flags: u32 = if self.peek { MSG_PEEK as u32 } else { 0 };
        let recv_result = unsafe {
            WinSock::WSARecv(
                socket as SOCKET,
                wsabuf as *mut WSABUF,
                1,
                std::ptr::null_mut(),
                &mut flags,
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

        let entry = opcode::Recv::new(
            types::Fd(self.handle.handle),
            self.buf.as_mut_ptr(),
            self.buf.len() as _,
        )
        .flags(if self.peek { libc::MSG_PEEK } else { 0 })
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
