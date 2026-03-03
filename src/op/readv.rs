use std::io;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::{Interest, Token};

use crate::{
    fd_inner::InnerRawHandle,
    op::{completion_result_to_poll, Op},
};

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

/// Unified vectored-read helper trait implemented on `InnerRawHandle`.
/// Exposes poll-based, completion-based and unified (default) helpers.
pub trait ReadvIo {
    /// Submit the readv operation via the poll/readiness pathway.
    fn poll_readv_poll(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>>;

    /// Submit the readv operation via the completion pathway.
    fn poll_readv_completion(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>>;

    /// High-level readv helper that chooses between completion and poll pathways.
    fn poll_readv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>>;
}

impl ReadvIo for InnerRawHandle {
    #[inline]
    fn poll_readv_poll(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        // Avoid borrowing `bufs` into the op's lifetime parameter (which causes
        // variance/lifetime issues). Instead pass a raw pointer + length into the
        // op; the raw pointer is only dereferenced inside `execute`.
        let ptr = bufs.as_mut_ptr() as *mut IoSliceMut<'static>;
        let len = bufs.len();
        match self.submit(ReadvOp::new(self, ptr, len), cx.waker().clone()) {
            Ok(read) => Poll::Ready(Ok(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[inline]
    fn poll_readv_completion(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let ptr = bufs.as_mut_ptr() as *mut IoSliceMut<'static>;
        let len = bufs.len();
        completion_result_to_poll(
            self.submit_completion(ReadvOp::new(self, ptr, len), cx.waker().clone()),
        )
    }

    #[inline]
    fn poll_readv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        if self.uses_completion() {
            self.poll_readv_completion(cx, bufs)
        } else {
            self.poll_readv_poll(cx, bufs)
        }
    }
}

/// Operation representing a single `readv` call.
///
/// This op avoids storing a direct mutable borrow of the caller's `IoSliceMut`
/// array. Instead it stores a raw pointer + length and reconstructs a temporary
/// slice only inside `execute`. For completion-based submission we additionally
/// allocate owned buffers and a cached `libc::iovec` array so the kernel can
/// write into memory we own; on completion data is copied back into the
/// caller-provided buffers.
pub struct ReadvOp<'a> {
    handle: &'a InnerRawHandle,
    bufs_ptr: *mut IoSliceMut<'static>,
    bufs_len: usize,
    // For completion path: owned buffers and cached iovecs that remain alive
    // until the completion is processed.
    #[cfg(unix)]
    iovecs: Option<Box<[libc::iovec]>>,
    // TODO: support Windows WSABUFs in "iovecs" field.
    owned_buffers: Option<Box<[Box<[u8]>]>>,
}

impl<'a> ReadvOp<'a> {
    #[inline]
    pub fn new(
        handle: &'a InnerRawHandle,
        bufs_ptr: *mut IoSliceMut<'static>,
        bufs_len: usize,
    ) -> Self {
        Self {
            handle,
            bufs_ptr,
            bufs_len,
            #[cfg(unix)]
            iovecs: None,
            // TODO: support Windows WSABUFs in "iovecs" field.
            owned_buffers: None,
        }
    }
}

impl Op for ReadvOp<'_> {
    type Output = usize;

    #[inline]
    fn token(&self) -> Token {
        self.handle.token()
    }

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn execute(&mut self) -> Result<Self::Output, io::Error> {
        // Reconstruct the caller-provided IoSliceMut array from the raw pointer.
        let bufs: &mut [IoSliceMut<'_>] = unsafe {
            // SAFETY: The raw pointer was derived from a `&mut [IoSliceMut<'_>]`
            // provided by the caller immediately before submitting this op.
            // The driver executes the poll-path `submit` synchronously (the
            // same semantic used by other ops here), so the pointer is valid
            // for the duration of this call. We create a temporary mutable
            // slice to iterate and build libc iovec structures.
            std::slice::from_raw_parts_mut(self.bufs_ptr as *mut IoSliceMut, self.bufs_len)
        };

        let iovecs = iovec_to_system(bufs);

        let ret = unsafe { libc::readv(self.handle.handle, iovecs.as_ptr(), iovecs.len() as _) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(ret as usize)
    }

    #[inline]
    fn interest(&self) -> Interest {
        Interest::READABLE
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<(io_uring::squeue::Entry, Option<Box<dyn std::any::Any>>), io::Error> {
        use io_uring::{opcode, types};

        // Reconstruct the caller-provided IoSliceMut array so we can determine
        // how large each owned buffer should be.
        let bufs: &mut [IoSliceMut<'_>] = unsafe {
            std::slice::from_raw_parts_mut(self.bufs_ptr as *mut IoSliceMut, self.bufs_len)
        };

        // Allocate owned buffers with the same lengths as the caller buffers.
        let mut owned_maybeuninit: Box<[MaybeUninit<Box<[u8]>>]> =
            Box::new_uninit_slice(bufs.len());
        for (index, s) in bufs.iter().enumerate() {
            // SAFETY: u8 is a primitive type
            let iovec = unsafe { Box::new_uninit_slice(s.len()).assume_init() };
            owned_maybeuninit[index].write(iovec);
        }
        // SAFETY: All values have been initialized
        let mut owned = unsafe { owned_maybeuninit.assume_init() };

        // Create libc::iovec array pointing to our owned buffers.
        let mut iovecs_maybeuninit: Box<[MaybeUninit<libc::iovec>]> =
            Box::new_uninit_slice(owned.len());
        for (index, s) in owned.iter_mut().enumerate() {
            let iov = libc::iovec {
                iov_base: s.as_mut_ptr().cast::<libc::c_void>(),
                iov_len: s.len(),
            };
            iovecs_maybeuninit[index].write(iov);
        }
        // SAFETY: The boxed slice would have all values initialized after interating over original array
        let iovecs = unsafe { iovecs_maybeuninit.assume_init() };

        // Move the iovec vector and the owned buffers into a single boxed tuple.
        // The driver will take ownership of this boxed value and keep it alive
        // until the completion is processed. We must obtain the pointer to the
        // iovec entries for the SQE from the boxed tuple.
        let boxed: Box<(Box<[libc::iovec]>, Box<[Box<[u8]>]>)> = Box::new((iovecs, owned));
        let iov_ptr = boxed.0.as_ptr();
        let iov_len = boxed.0.len();

        let entry = opcode::Readv::new(types::Fd(self.handle.handle), iov_ptr, iov_len as _)
            .build()
            .user_data(user_data);

        // Return the SQE plus the boxed storage (as Box<dyn Any>) so the driver
        // can retain ownership until completion.
        Ok((entry, Some(boxed)))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn take_completion_storage(&mut self, storage: Option<Box<dyn std::any::Any>>) {
        // The driver will hand back the boxed storage that we previously
        // provided in `build_completion_entry`. Downcast it to the concrete
        // tuple type and restore the owned buffers/iovecs into the op so that
        // `complete` can copy data back into the caller buffers.
        if let Some(boxed_any) = storage {
            if let Ok(boxed_tuple) = boxed_any.downcast::<(Box<[libc::iovec]>, Box<[Box<[u8]>]>)>()
            {
                let (iovecs, owned_buffers) = *boxed_tuple;
                self.iovecs = Some(iovecs);
                self.owned_buffers = Some(owned_buffers);
            }
        }
    }

    // TODO: support Windows
    #[cfg(unix)]
    #[inline]
    fn complete(&mut self, result: i32) -> Result<Self::Output, io::Error> {
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        let mut remaining = result as usize;

        // If we used the completion path, copy data from owned buffers into the
        // caller's IoSliceMut buffers.
        if let Some(owned) = self.owned_buffers.as_ref() {
            // Reconstruct caller buffers
            let dest_bufs: &mut [IoSliceMut<'_>] = unsafe {
                std::slice::from_raw_parts_mut(self.bufs_ptr as *mut IoSliceMut, self.bufs_len)
            };

            let mut src_index = 0usize;
            let mut src_offset = 0usize;

            for dest in dest_bufs.iter_mut() {
                if remaining == 0 {
                    break;
                }
                let src = &owned[src_index];
                let to_copy = dest.len().min(remaining);
                // copy from src starting at src_offset
                dest[..to_copy].copy_from_slice(&src[src_offset..src_offset + to_copy]);
                remaining -= to_copy;
                src_offset += to_copy;
                if src_offset >= src.len() {
                    src_index += 1;
                    src_offset = 0;
                }
            }
        }

        Ok(result as usize)
    }
}
