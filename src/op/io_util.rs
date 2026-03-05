use std::io;
use std::task::{Context, Poll};

#[cfg(unix)]
use futures_util::future::LocalBoxFuture;
#[cfg(unix)]
use futures_util::FutureExt;
use mio::Interest;

#[cfg(unix)]
use crate::blocking::SpawnBlockingError;
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

#[inline]
#[cfg(unix)]
pub(crate) fn poll_blocking_result<'a, F>(
    blocking_future: &mut Option<LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>>,
    cx: &mut Context<'_>,
    make_future: F,
) -> Poll<io::Result<isize>>
where
    F: FnOnce() -> LocalBoxFuture<'a, Result<isize, SpawnBlockingError>>,
{
    let mut future = if let Some(future) = blocking_future.take() {
        future
    } else {
        make_future()
    };

    match future.poll_unpin(cx) {
        Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
        Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Other,
            "can't spawn blocking task for I/O",
        ))),
        Poll::Pending => {
            *blocking_future = Some(future);
            Poll::Pending
        }
    }
}
