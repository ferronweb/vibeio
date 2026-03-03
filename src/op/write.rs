use std::io;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

pub struct WriteOp<'a> {
    handle: &'a InnerRawHandle,
    buf: &'a [u8],
    completion_token: Option<usize>,
}

impl<'a> WriteOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: &'a [u8]) -> Self {
        Self {
            handle,
            buf,
            completion_token: None,
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
        let written = unsafe {
            libc::write(
                self.handle.handle,
                self.buf.as_ptr().cast::<libc::c_void>(),
                self.buf.len(),
            )
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

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            // Get the completion result
            match driver.get_completion_result(completion_token) {
                Some(result) => result,
                None => {
                    // The completion is not ready yet
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
