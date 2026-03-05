use std::io;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
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

pub struct ReadvOp<'a, 'b> {
    handle: &'a InnerRawHandle,
    bufs: &'a mut [IoSliceMut<'b>],
    completion_token: Option<usize>,
    #[cfg(target_os = "linux")]
    completion_system_iovecs: Option<Box<[libc::iovec]>>,
    blocking: bool,
    blocking_future: Option<LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>>,
}

impl<'a, 'b> ReadvOp<'a, 'b> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, bufs: &'a mut [IoSliceMut<'b>]) -> Self {
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
    pub fn new_blocking(inner: &'a InnerRawHandle, bufs: &'a mut [IoSliceMut<'b>]) -> Self {
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

impl Op for ReadvOp<'_, '_> {
    type Output = usize;

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let read = if !self.blocking {
            let mut iovecs = iovec_to_system(self.bufs);
            unsafe { libc::readv(self.handle.handle, iovecs.as_mut_ptr(), iovecs.len() as _) }
        } else {
            use futures_util::FutureExt;
            use std::task::ready;

            let mut blocking_future = if let Some(blocking_future) = self.blocking_future.take() {
                blocking_future
            } else {
                let bufs: &mut [IoSliceMut] = unsafe {
                    std::mem::transmute::<&mut [IoSliceMut], &mut [IoSliceMut]>(self.bufs)
                };
                let handle = self.handle.handle;
                Box::pin(crate::spawn_blocking(move || {
                    let mut iovecs = iovec_to_system(bufs);
                    unsafe { libc::readv(handle, iovecs.as_mut_ptr(), iovecs.len() as _) }
                }))
            };
            match ready!(blocking_future.poll_unpin(cx)) {
                Ok(read) => read,
                Err(_) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        io::ErrorKind::Other,
                        "can't spawn blocking task for I/O",
                    )))
                }
            }
        };

        if read == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        Poll::Ready(Ok(read as usize))
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
