use std::io::{self, IoSlice};
use std::task::{Context, Poll};

use futures_util::future::LocalBoxFuture;
use mio::Interest;

use crate::blocking::SpawnBlockingError;
use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

/// Converts a slice of `IoSlice` to a system iovec buffer.
#[cfg(unix)]
#[inline]
fn iovec_to_system(bufs: &[IoSlice<'_>]) -> Box<[libc::iovec]> {
    use std::mem::MaybeUninit;

    let mut iovecs_maybeuninit: Box<[MaybeUninit<libc::iovec>]> = Box::new_uninit_slice(bufs.len());
    for (index, s) in bufs.iter().enumerate() {
        let iov = libc::iovec {
            iov_base: s.as_ptr() as *mut libc::c_void,
            iov_len: s.len(),
        };
        iovecs_maybeuninit[index].write(iov);
    }
    // SAFETY: The boxed slice would have all values initialized after interating over original array
    unsafe { iovecs_maybeuninit.assume_init() }
}

pub struct WritevOp<'a, 'b> {
    handle: &'a InnerRawHandle,
    bufs: &'a [IoSlice<'b>],
    completion_token: Option<usize>,
    #[cfg(target_os = "linux")]
    completion_system_iovecs: Option<Box<[libc::iovec]>>,
    blocking: bool,
    blocking_future: Option<LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>>,
}

impl<'a, 'b> WritevOp<'a, 'b> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, bufs: &'a [IoSlice<'b>]) -> Self {
        Self {
            handle,
            bufs,
            completion_token: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
            blocking: false,
            blocking_future: None,
        }
    }

    #[inline]
    pub fn new_blocking(inner: &'a InnerRawHandle, bufs: &'a [IoSlice<'b>]) -> Self {
        Self {
            handle: inner,
            bufs,
            completion_token: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
            blocking: true,
            blocking_future: None,
        }
    }
}

impl Op for WritevOp<'_, '_> {
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
            // Build a temporary iovec array for the syscall.
            let iovecs = iovec_to_system(self.bufs);

            unsafe {
                libc::writev(
                    self.handle.handle,
                    iovecs.as_ptr(),
                    iovecs.len() as libc::c_int,
                )
            }
        } else {
            use futures_util::FutureExt;
            use std::task::ready;

            let mut blocking_future = if let Some(blocking_future) = self.blocking_future.take() {
                blocking_future
            } else {
                let bufs: &[IoSlice] =
                    unsafe { std::mem::transmute::<&[IoSlice], &[IoSlice]>(self.bufs) };
                let handle = self.handle.handle;
                Box::pin(crate::spawn_blocking(move || {
                    // Build a temporary iovec array for the syscall.
                    let iovecs = iovec_to_system(bufs);

                    unsafe { libc::writev(handle, iovecs.as_ptr(), iovecs.len() as libc::c_int) }
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
                Some(result) => {
                    self.completion_token = None;
                    result
                }
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

        // Build a temporary iovec array for the syscall.
        let iovecs = if let Some(iovecs) = self.completion_system_iovecs.take() {
            iovecs
        } else {
            iovec_to_system(self.bufs)
        };

        let entry = opcode::Writev::new(
            types::Fd(self.handle.handle),
            iovecs.as_ptr(),
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
