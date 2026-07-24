//! Sendfile operations for Linux and FreeBSD.

use std::mem::ManuallyDrop;
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

#[cfg(target_os = "linux")]
use super::splice::splice;
use mio::Interest;

use crate::{fd_inner::InnerRawHandle, io::AsInnerRawHandle};

#[cfg(target_os = "linux")]
async fn sendfile_exact_completion<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    // splice() requires at least one of the file descriptors to be a pipe.
    // Therefore, we need to create a pipe and use it as the destination.
    let mut fds: [RawFd; 2] = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        // pipe2 failed, can't continue
        return Err(std::io::Error::last_os_error());
    }
    let pipe_reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let pipe_writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // We only need to poll the pipe writer for writability.
    let pipe_writer_handle = WriteOwnedFd::new(pipe_writer)?;

    let mut total_from_file = 0;
    let mut total_to_socket = 0;
    let mut file_eof = false;
    while (total_from_file < len && !file_eof) || total_to_socket < total_from_file {
        let splice_from_file_len = (len - total_from_file).min(usize::MAX as u64) as usize;
        if !file_eof && splice_from_file_len > 0 {
            let n = splice(from, &pipe_writer_handle, splice_from_file_len).await?;
            if n == 0 {
                file_eof = true;
            } else {
                total_from_file += n as u64;
            }
        }

        let splice_to_socket_len =
            (total_from_file - total_to_socket).min(usize::MAX as u64) as usize;
        if splice_to_socket_len > 0 {
            let n = splice(&pipe_reader, to, splice_to_socket_len).await?;
            if n == 0 {
                break;
            }
            total_to_socket += n as u64;
        }
    }

    drop(pipe_reader);
    drop(pipe_writer_handle);

    Ok(total_to_socket)
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
async fn sendfile_exact_poll<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    let mut total = 0;
    while total < len {
        let len_to_copy = (len - total).min(usize::MAX as u64) as usize;
        let n = {
            let from_handle = unsafe { BorrowedFd::borrow_raw(from.as_raw_fd()) };
            let to_handle = to.as_inner_raw_handle();

            let mut op = crate::op::SendfileOp::new(from_handle, to_handle, len_to_copy);
            std::future::poll_fn(move |cx| to_handle.poll_op(cx, &mut op)).await?
        };
        if n == 0 {
            break;
        }
        total += n as u64;
    }

    Ok(total)
}

/// Transfer data from a file to a socket using `sendfile` semantics.
///
/// - **Linux with `io_uring`** - uses `splice` with an intermediate pipe to transfer data.
/// - **Linux with `epoll`, FreeBSD** - uses `sendfile` syscall.
///
/// This function isn't supported on Windows, due to concurrency limits on client versions
/// of Windows regarding `TransmitFile`.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub async fn sendfile_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> std::io::Result<u64> {
    #[cfg(target_os = "linux")]
    if to.as_inner_raw_handle().uses_completion() {
        return sendfile_exact_completion(from, to, len).await;
    }

    sendfile_exact_poll(from, to, len).await
}

#[cfg(target_os = "linux")]
struct WriteOwnedFd {
    _writer: OwnedFd,
    handle: ManuallyDrop<InnerRawHandle>,
}

#[cfg(target_os = "linux")]
impl WriteOwnedFd {
    fn new(writer: OwnedFd) -> std::io::Result<Self> {
        let handle =
            ManuallyDrop::new(InnerRawHandle::new(writer.as_raw_fd(), Interest::WRITABLE)?);
        if !handle.uses_completion() {
            // Set the pipe write side to non-blocking mode.
            let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
            if flags != -1 {
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
            }
        }
        Ok(Self {
            _writer: writer,
            handle,
        })
    }
}

#[cfg(target_os = "linux")]
impl<'a> AsInnerRawHandle<'a> for WriteOwnedFd {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

#[cfg(target_os = "linux")]
impl Drop for WriteOwnedFd {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
