use std::io;
use std::io::IoSliceMut;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use futures_util::future::LocalBoxFuture;
use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self as WinSock, SOCKET, WSABUF, WSA_IO_PENDING},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::blocking::SpawnBlockingError;
use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::{InnerRawHandle, RawOsHandle};
#[cfg(unix)]
use crate::op::io_util::poll_blocking_result;
use crate::op::io_util::poll_result_or_wait;
use crate::op::Op;

/// Converts a slice of `IoSlice` to a system iovec buffer.
#[cfg(unix)]
#[inline]
fn iovec_to_system(bufs: &mut [IoSliceMut<'_>]) -> Box<[libc::iovec]> {
    let mut iovecs_maybeuninit: Box<[MaybeUninit<libc::iovec>]> = Box::new_uninit_slice(bufs.len());
    for (index, s) in bufs.iter_mut().enumerate() {
        let iov = libc::iovec {
            iov_base: s.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: s.len(),
        };
        iovecs_maybeuninit[index].write(iov);
    }
    // SAFETY: The boxed slice would have all values initialized after interating over original array
    unsafe { iovecs_maybeuninit.assume_init() }
}

#[cfg(windows)]
#[inline]
fn socket_read_vectored(socket: SOCKET, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let mut wsabufs = Vec::with_capacity(bufs.len());
    for buf in bufs.iter_mut() {
        let len = u32::try_from(buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "readv buffer is too large for Windows socket I/O",
            )
        })?;
        wsabufs.push(WSABUF {
            len,
            buf: buf.as_mut_ptr().cast(),
        });
    }

    let mut bytes: u32 = 0;
    let mut flags: u32 = 0;
    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            wsabufs.as_mut_ptr(),
            wsabufs.len() as u32,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct ReadvOp<'a, 'b> {
    handle: &'a InnerRawHandle,
    bufs: &'a mut [IoSliceMut<'b>],
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_wsabufs: Option<Box<[WSABUF]>>,
    #[cfg(windows)]
    completion_staging: Option<Vec<u8>>,
    #[cfg(target_os = "linux")]
    completion_system_iovecs: Option<Box<[libc::iovec]>>,
    blocking: bool,
    #[cfg(unix)]
    blocking_future: Option<LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>>,
}

impl<'a, 'b> ReadvOp<'a, 'b> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, bufs: &'a mut [IoSliceMut<'b>]) -> Self {
        Self {
            handle,
            bufs,
            completion_token: None,
            #[cfg(windows)]
            completion_wsabufs: None,
            #[cfg(windows)]
            completion_staging: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
            blocking: false,
            #[cfg(unix)]
            blocking_future: None,
        }
    }

    #[inline]
    pub fn new_blocking(inner: &'a InnerRawHandle, bufs: &'a mut [IoSliceMut<'b>]) -> Self {
        Self {
            handle: inner,
            bufs,
            completion_token: None,
            #[cfg(windows)]
            completion_wsabufs: None,
            #[cfg(windows)]
            completion_staging: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
            blocking: true,
            #[cfg(unix)]
            blocking_future: None,
        }
    }
}

impl Op for ReadvOp<'_, '_> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        if self.blocking {
            #[cfg(unix)]
            {
                let read = match poll_blocking_result(&mut self.blocking_future, cx, || {
                    let bufs: &mut [IoSliceMut] = unsafe {
                        std::mem::transmute::<&mut [IoSliceMut], &mut [IoSliceMut]>(self.bufs)
                    };
                    let handle = self.handle.handle;
                    Box::pin(crate::spawn_blocking(move || {
                        let mut iovecs = iovec_to_system(bufs);
                        unsafe { libc::readv(handle, iovecs.as_mut_ptr(), iovecs.len() as _) }
                    }))
                }) {
                    Poll::Ready(Ok(read)) => read,
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                };

                let result = if read == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(read as usize)
                };
                return poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE);
            }

            #[cfg(windows)]
            {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "blocking poll-based readv is unsupported on Windows",
                )));
            }
        }

        #[cfg(unix)]
        let result = {
            let mut iovecs = iovec_to_system(self.bufs);
            let read =
                unsafe { libc::readv(self.handle.handle, iovecs.as_mut_ptr(), iovecs.len() as _) };
            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(read as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_read_vectored(socket as SOCKET, self.bufs),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based readv currently supports sockets only on Windows",
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
            #[cfg(windows)]
            {
                self.completion_wsabufs = None;
                self.completion_staging = None;
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        #[cfg(windows)]
        {
            if let Some(staging) = self.completion_staging.take() {
                let mut src_offset = 0usize;
                let mut remaining = result as usize;
                for dst in self.bufs.iter_mut() {
                    if remaining == 0 {
                        break;
                    }
                    let chunk = remaining.min(dst.len());
                    if chunk == 0 {
                        continue;
                    }

                    let dst_slice =
                        unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr(), dst.len()) };
                    dst_slice[..chunk].copy_from_slice(&staging[src_offset..src_offset + chunk]);
                    src_offset += chunk;
                    remaining -= chunk;
                }
            }
            self.completion_wsabufs = None;
        }

        Poll::Ready(Ok(result as usize))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let mut wsabufs = Vec::with_capacity(self.bufs.len());
                for buf in self.bufs.iter_mut() {
                    let len = u32::try_from(buf.len()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "readv buffer is too large for Windows socket I/O",
                        )
                    })?;
                    wsabufs.push(WSABUF {
                        len,
                        buf: buf.as_mut_ptr().cast(),
                    });
                }

                let mut wsabufs = wsabufs.into_boxed_slice();
                let mut flags = 0u32;
                let recv_result = unsafe {
                    WinSock::WSARecv(
                        socket as SOCKET,
                        wsabufs.as_mut_ptr(),
                        wsabufs.len() as u32,
                        std::ptr::null_mut(),
                        &mut flags,
                        overlapped,
                        None,
                    )
                };

                if recv_result == 0 {
                    self.completion_wsabufs = Some(wsabufs);
                    self.completion_staging = None;
                    return Ok(());
                }

                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    self.completion_wsabufs = Some(wsabufs);
                    self.completion_staging = None;
                    Ok(())
                } else {
                    self.completion_wsabufs = None;
                    self.completion_staging = None;
                    Err(io::Error::from_raw_os_error(err))
                }
            }
            RawOsHandle::Handle(handle) => {
                let total_len = self.bufs.iter().try_fold(0usize, |acc, buf| {
                    acc.checked_add(buf.len()).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "readv buffer length overflow")
                    })
                })?;
                let total_len_u32 = u32::try_from(total_len).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "readv total length is too large for Windows file I/O",
                    )
                })?;

                let mut staging = vec![0u8; total_len];
                let read_result = unsafe {
                    ReadFile(
                        handle as HANDLE,
                        staging.as_mut_ptr().cast(),
                        total_len_u32,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if read_result != 0 {
                    self.completion_wsabufs = None;
                    self.completion_staging = Some(staging);
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    self.completion_wsabufs = None;
                    self.completion_staging = Some(staging);
                    Ok(())
                } else {
                    self.completion_wsabufs = None;
                    self.completion_staging = None;
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

        // Build a temporary iovec array for the syscall.
        let mut iovecs = if let Some(iovecs) = self.completion_system_iovecs.take() {
            iovecs
        } else {
            iovec_to_system(self.bufs)
        };

        let entry = opcode::Readv::new(
            types::Fd(self.handle.handle),
            iovecs.as_mut_ptr(),
            iovecs.len() as _,
        )
        .build()
        .user_data(user_data);

        // Store the iovec array for the completion, because it needs to be kept alive until the
        // completion is ready.
        self.completion_system_iovecs = Some(iovecs);

        Ok(entry)
    }
}
