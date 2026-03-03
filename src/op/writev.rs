/// writev.rs
use std::io;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::{Interest, Token};
use std::io::IoSlice;

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, Op},
};

/// Converts a slice of `IoSlice` to a system iovec buffer.
#[cfg(unix)]
#[inline]
fn iovec_to_system(bufs: &[IoSlice<'_>]) -> Box<[libc::iovec]> {
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

/// Unified vectored-write helper trait implemented on `InnerRawHandle`.
/// Exposes poll-based, completion-based and unified (default) helpers.
pub trait WritevIo {
    /// Submit the vectored write operation via the poll/readiness pathway.
    fn poll_writev_poll(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>>;

    /// Submit the vectored write operation via the completion pathway.
    fn poll_writev_completion(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>>;

    /// High-level vectored-write helper that chooses between completion and poll pathways.
    fn poll_writev(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>>;
}

impl WritevIo for InnerRawHandle {
    #[inline]
    fn poll_writev_poll(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        match self.submit(WritevOp::new(self, bufs), cx.waker().clone()) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_writev_completion(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        completion_result_to_poll(
            self.submit_completion(WritevOp::new(self, bufs), cx.waker().clone()),
        )
    }

    #[inline]
    fn poll_writev(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        if self.uses_completion() {
            self.poll_writev_completion(cx, bufs)
        } else {
            self.poll_writev_poll(cx, bufs)
        }
    }
}

pub struct WritevOp<'a> {
    handle: &'a InnerRawHandle,
    bufs: &'a [IoSlice<'a>],
    // Optional cached iovec list for completion path lifetime reasons.
    #[cfg(unix)]
    iovecs: Option<Box<[libc::iovec]>>,
    // TODO: support Windows WSABUFs in "iovecs" field.
}

impl<'a> WritevOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, bufs: &'a [IoSlice<'a>]) -> Self {
        Self {
            handle,
            bufs,
            iovecs: None,
        }
    }
}

impl Op for WritevOp<'_> {
    type Output = usize;

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        // Build a temporary iovec array for the syscall.
        let iovecs = iovec_to_system(self.bufs);

        let written = unsafe {
            libc::writev(
                self.handle.handle,
                iovecs.as_ptr(),
                iovecs.len() as libc::c_int,
            )
        };

        if written == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(written as usize)
    }

    #[inline]
    fn interest(&self) -> Interest {
        Interest::WRITABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn std::any::Any>>), io::Error> {
        use io_uring::{opcode, types};

        // Build libc::iovec array referencing caller buffers. We must transfer
        // ownership of the iovec vector to the driver so it remains valid for
        // the duration of the submission. Create the Vec, take a raw pointer
        // to its buffer, then move the Vec into a Box<dyn Any> which the driver
        // will store.
        let iovecs = iovec_to_system(self.bufs);

        let iov_ptr = iovecs.as_ptr();
        let iov_len = iovecs.len();

        // Move ownership of the Vec into boxed storage so the driver can own it.
        let storage: Option<Box<dyn std::any::Any>> = Some(Box::new(iovecs));

        let entry = opcode::Writev::new(types::Fd(self.handle.handle), iov_ptr, iov_len as _)
            .build()
            .user_data(user_data);

        Ok((entry, storage))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn take_completion_storage(&mut self, storage: Option<Box<dyn std::any::Any>>) {
        if let Some(boxed) = storage {
            // Attempt to downcast to the type we stored in build_completion_entry.
            if let Ok(vec) = boxed.downcast::<Box<[libc::iovec]>>() {
                self.iovecs = Some(*vec);
            }
        }
    }

    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(result as usize)
    }
}
