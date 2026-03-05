use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{self as WinSock, SOCKET, WSABUF, WSA_IO_PENDING},
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::op::io_util::poll_result_or_wait;
use crate::op::Op;

#[cfg(windows)]
#[inline]
fn socket_send(socket: SOCKET, buf: &[u8]) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_ptr().cast_mut().cast(),
    };
    let mut bytes: u32 = 0;

    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct SendOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a [u8],
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<WSABUF>,
}

impl<'a> SendOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
        }
    }
}

impl Op for SendOp<'_> {
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
            let written = unsafe {
                libc::send(
                    self.handle.handle,
                    self.buf.as_ptr().cast::<libc::c_void>(),
                    self.buf.len(),
                    0,
                )
            };
            if written == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_send(socket as SOCKET, self.buf),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based send currently supports sockets only on Windows",
            )),
        };

        poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE)
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
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
                "WSASend can be used only with sockets",
            ));
        };

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

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Send::new(
            types::Fd(self.handle.handle),
            self.buf.as_ptr(),
            self.buf.len() as _,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
