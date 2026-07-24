use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::fd_inner::InnerRawHandle;
use crate::op::io_util::poll_result_or_wait;
use crate::op::Op;

pub struct SendfileOp<'a> {
    fd_in: BorrowedFd<'a>,
    fd_out: &'a InnerRawHandle,
    len: usize,
    completion_token: Option<usize>,
}

impl<'a> SendfileOp<'a> {
    #[inline]
    pub fn new(fd_in: BorrowedFd<'a>, fd_out: &'a InnerRawHandle, len: usize) -> Self {
        Self {
            fd_in,
            fd_out,
            len,
            completion_token: None,
        }
    }
}

impl Op for SendfileOp<'_> {
    type Output = usize;

    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = {
            #[cfg(target_os = "linux")]
            let returned = unsafe {
                libc::sendfile(
                    self.fd_in.as_raw_fd(),
                    self.fd_out.handle,
                    std::ptr::null_mut(),
                    self.len,
                )
            };
            #[cfg(target_os = "freebsd")]
            let returned = {
                let mut sbytes = 0;
                // On FreeBSD, sendfile returns 0 on success, and bytes sent inside `sbytes`,
                // unlike on Linux, where it returns the number of bytes sent directly.
                let result = unsafe {
                    libc::sendfile(
                        self.fd_in.as_raw_fd(),
                        self.fd_out.handle,
                        0,
                        self.len,
                        std::ptr::null_mut(),
                        &mut sbytes,
                        0,
                    )
                };
                if result < 0 {
                    result as i64
                } else {
                    sbytes
                }
            };

            if returned == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(returned as usize)
            }
        };

        poll_result_or_wait(result, self.fd_out, cx, driver, Interest::WRITABLE)
    }
}

impl Drop for SendfileOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
