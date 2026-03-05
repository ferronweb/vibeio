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

#[cfg(windows)]
#[inline]
fn winsock_last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe {
        windows_sys::Win32::Networking::WinSock::WSAGetLastError()
    })
}

#[cfg(windows)]
#[inline]
pub(crate) fn socket_read(
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
    buf: &mut [u8],
) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = 0;

    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        return Err(winsock_last_error());
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
#[inline]
pub(crate) fn socket_recv(
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
    buf: &mut [u8],
    peek: bool,
) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKET_ERROR, WSABUF,
    };

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = if peek { MSG_PEEK } else { 0 };

    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        return Err(winsock_last_error());
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
#[inline]
pub(crate) fn socket_write(
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
    buf: &[u8],
) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_ptr().cast_mut().cast(),
    };
    let mut bytes: u32 = 0;

    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        return Err(winsock_last_error());
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
#[inline]
pub(crate) fn socket_read_vectored(
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
    bufs: &mut [std::io::IoSliceMut<'_>],
) -> io::Result<usize> {
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
        return Err(winsock_last_error());
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
#[inline]
pub(crate) fn socket_write_vectored(
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
    bufs: &[std::io::IoSlice<'_>],
) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let mut wsabufs = Vec::with_capacity(bufs.len());
    for buf in bufs.iter() {
        let len = u32::try_from(buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "writev buffer is too large for Windows socket I/O",
            )
        })?;
        wsabufs.push(WSABUF {
            len,
            buf: buf.as_ptr().cast_mut().cast(),
        });
    }

    let mut bytes: u32 = 0;
    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            wsabufs.as_mut_ptr(),
            wsabufs.len() as u32,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        return Err(winsock_last_error());
    }

    Ok(bytes as usize)
}
