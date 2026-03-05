use std::io;
use std::task::{Context, Poll};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::op::Op;

pub struct ReadAtOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a mut [u8],
    offset: u64,
    completion_token: Option<usize>,
}

impl<'a> ReadAtOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a mut [u8], offset: u64) -> Self {
        Self {
            handle,
            buf,
            offset,
            completion_token: None,
        }
    }
}

impl Op for ReadAtOp<'_> {
    type Output = usize;

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
        let RawOsHandle::Handle(handle) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ReadAtOp expects a file handle, not a socket",
            ));
        };

        let read_len = u32::try_from(self.buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read buffer is too large for Windows file I/O",
            )
        })?;

        unsafe {
            (*overlapped).Anonymous.Anonymous.Offset = self.offset as u32;
            (*overlapped).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        let read_result = unsafe {
            ReadFile(
                handle as HANDLE,
                self.buf.as_mut_ptr().cast(),
                read_len,
                std::ptr::null_mut(),
                overlapped,
            )
        };

        if read_result != 0 {
            return Ok(());
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            Ok(())
        } else {
            Err(err)
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let read_len = u32::try_from(self.buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read buffer is too large for io_uring",
            )
        })?;

        let entry = opcode::Read::new(
            types::Fd(self.handle.handle),
            self.buf.as_mut_ptr(),
            read_len,
        )
        .offset(self.offset)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}
