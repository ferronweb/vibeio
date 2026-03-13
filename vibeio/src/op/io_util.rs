use std::io;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::fd_inner::InnerRawHandle;

#[inline]
pub(crate) fn poll_result_or_wait(
    result: io::Result<usize>,
    handle: &InnerRawHandle,
    cx: &mut Context<'_>,
    driver: &AnyDriver,
    interest: Interest,
) -> Poll<io::Result<usize>> {
    match result {
        Ok(value) => Poll::Ready(Ok(value)),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Err(submit_err) = driver.submit_poll(handle, cx.waker().clone(), interest) {
                Poll::Ready(Err(submit_err))
            } else {
                Poll::Pending
            }
        }
        Err(err) => Poll::Ready(Err(err)),
    }
}
