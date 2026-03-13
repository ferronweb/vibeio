use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

use crate::io::{
    AsInnerRawHandle, IoBuf, IoBufMut, IoBufTemporaryPoll, IoVectoredBuf, IoVectoredBufMut,
    IoVectoredBufTemporaryPoll,
};
use crate::op::{ReadOp, ReadvOp, WriteOp, WritevOp};
use crate::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

fn pipe_inner() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: Box<[MaybeUninit<RawFd>]> = Box::new_uninit_slice(2);
    let fds = unsafe {
        libc::pipe(fds.as_mut_ptr().cast::<RawFd>());
        fds.assume_init()
    };
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

pub fn pipe() -> std::io::Result<(Pipe, Pipe)> {
    let (read, write) = pipe_inner()?;
    Ok((
        Pipe::from_std_with_mode(read, RegistrationMode::Completion)?,
        Pipe::from_std_with_mode(write, RegistrationMode::Completion)?,
    ))
}

pub struct Pipe {
    inner: OwnedFd,
    handle: ManuallyDrop<InnerRawHandle>,
}

/// A poll-only variant that always uses readiness-based operations.
pub struct PollPipe {
    stream: Pipe,
}

impl Pipe {
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: OwnedFd,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        let flags = unsafe { libc::fcntl(inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn into_poll(self) -> Result<PollPipe, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        let flags = unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if stream.handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
        Ok(PollPipe { stream })
    }
}

impl PollPipe {
    #[inline]
    pub fn into_adaptive(self) -> Pipe {
        self.stream
    }

    #[inline]
    pub fn into_completion(self) -> Result<Pipe, io::Error> {
        let mut stream = self.stream;
        stream.handle.rebind_mode(RegistrationMode::Completion)?;
        let flags = unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if stream.handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
        Ok(stream)
    }
}

impl AsRawFd for Pipe {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsRawFd for PollPipe {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.stream.inner.as_raw_fd()
    }
}

impl IntoRawFd for Pipe {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its fd ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_fd()
        }
    }
}

impl IntoRawFd for PollPipe {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl<'a> AsInnerRawHandle<'a> for Pipe {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

impl<'a> AsInnerRawHandle<'a> for PollPipe {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.stream.as_inner_raw_handle()
    }
}

impl AsyncRead for Pipe {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = ReadOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn read_vectored<B: IoVectoredBufMut>(
        &mut self,
        bufs: B,
    ) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = ReadvOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl TokioAsyncRead for PollPipe {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();
        // Equivalent to .assume_init_mut() in Rust 1.93.0+
        let unfilled = unsafe { &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
        let buf_temp = unsafe { IoBufTemporaryPoll::new(unfilled.as_mut_ptr(), unfilled.len()) };
        let mut op = ReadOp::new(&this.stream.handle, buf_temp);
        match this.stream.handle.poll_op_poll(cx, &mut op) {
            Poll::Ready(Ok(read)) => {
                unsafe {
                    buf.assume_init(read);
                }
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Pipe {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = WriteOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    #[inline]
    async fn write_vectored<B: IoVectoredBuf>(&mut self, bufs: B) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = WritevOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl TokioAsyncWrite for PollPipe {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let buf = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = WriteOp::new(&this.stream.handle, buf);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        let bufs = unsafe { IoVectoredBufTemporaryPoll::new(bufs) };
        let mut op = WritevOp::new(&this.stream.handle, bufs);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl Drop for Pipe {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{pipe, Pipe, PollPipe};

    fn make_runtime() -> crate::executor::Runtime {
        crate::executor::Runtime::new(AnyDriver::new_mio().expect("mio driver should initialize"))
    }

    // ── Pipe (completion / adaptive path) ────────────────────────────────────

    #[test]
    fn pipe_exchanges_data() {
        make_runtime().block_on(async {
            let (mut reader, mut writer) = pipe().expect("pipe should be created");

            let writer_task = spawn(async move {
                writer
                    .write(b"hello".to_vec())
                    .await
                    .0
                    .expect("writer should write");
            });

            let buf = [0u8; 5];
            let (read, buf) = reader.read(buf).await;
            let read = read.expect("reader should read");
            assert_eq!(&buf[..read], b"hello");

            writer_task.await;
        });
    }

    #[test]
    fn pipe_write_then_read_multiple_messages() {
        make_runtime().block_on(async {
            let (mut reader, mut writer) = pipe().expect("pipe should be created");

            let writer_task = spawn(async move {
                writer
                    .write(b"ping".to_vec())
                    .await
                    .0
                    .expect("first write should succeed");
                writer
                    .write(b"pong".to_vec())
                    .await
                    .0
                    .expect("second write should succeed");
            });

            let buf = [0u8; 4];
            let (read, buf) = reader.read(buf).await;
            assert_eq!(&buf[..read.expect("first read should succeed")], b"ping");

            let buf = [0u8; 4];
            let (read, buf) = reader.read(buf).await;
            assert_eq!(&buf[..read.expect("second read should succeed")], b"pong");

            writer_task.await;
        });
    }

    #[test]
    fn pipe_read_vectored() {
        make_runtime().block_on(async {
            let (mut reader, mut writer) = pipe().expect("pipe should be created");

            let writer_task = spawn(async move {
                writer
                    .write(b"abcdef".to_vec())
                    .await
                    .0
                    .expect("writer should write");
            });

            // Two separate buffers of 3 bytes each.
            let bufs: Vec<libc::iovec> = vec![
                libc::iovec {
                    iov_base: Box::into_raw(vec![0u8; 3].into_boxed_slice()) as *mut libc::c_void,
                    iov_len: 3,
                },
                libc::iovec {
                    iov_base: Box::into_raw(vec![0u8; 3].into_boxed_slice()) as *mut libc::c_void,
                    iov_len: 3,
                },
            ];

            let (read, bufs) = reader.read_vectored(bufs).await;
            let read = read.expect("vectored read should succeed");
            assert_eq!(read, 6);

            // Reconstruct slices to check contents.
            let first = unsafe { std::slice::from_raw_parts(bufs[0].iov_base as *const u8, 3) };
            let second = unsafe { std::slice::from_raw_parts(bufs[1].iov_base as *const u8, 3) };
            assert_eq!(first, b"abc");
            assert_eq!(second, b"def");

            // Free the iovec memory.
            for iov in &bufs {
                unsafe {
                    drop(Box::from_raw(std::slice::from_raw_parts_mut(
                        iov.iov_base as *mut u8,
                        iov.iov_len,
                    )));
                }
            }

            writer_task.await;
        });
    }

    #[test]
    fn pipe_write_vectored() {
        make_runtime().block_on(async {
            let (mut reader, mut writer) = pipe().expect("pipe should be created");

            let writer_task = spawn(async move {
                let bufs: Vec<libc::iovec> = vec![
                    libc::iovec {
                        iov_base: b"foo" as *const [u8; 3] as *mut libc::c_void,
                        iov_len: 3,
                    },
                    libc::iovec {
                        iov_base: b"bar" as *const [u8; 3] as *mut libc::c_void,
                        iov_len: 3,
                    },
                ];
                let (written, _) = writer.write_vectored(bufs).await;
                written.expect("vectored write should succeed");
            });

            let buf = [0u8; 6];
            let (read, buf) = reader.read(buf).await;
            assert_eq!(&buf[..read.expect("read should succeed")], b"foobar");

            writer_task.await;
        });
    }

    #[test]
    fn pipe_flush_is_noop() {
        make_runtime().block_on(async {
            let (_, mut writer) = pipe().expect("pipe should be created");
            writer.flush().await.expect("flush should succeed");
        });
    }

    // ── into_poll / into_completion round-trips ───────────────────────────────

    #[test]
    fn pipe_into_poll_and_back_exchanges_data() {
        make_runtime().block_on(async {
            let (mut reader, writer) = pipe().expect("pipe should be created");

            // Convert the write end to PollPipe and back to completion Pipe.
            let poll_writer = writer.into_poll().expect("into_poll should succeed");
            let mut writer = poll_writer
                .into_completion()
                .expect("into_completion should succeed");

            let writer_task = spawn(async move {
                writer
                    .write(b"roundtrip".to_vec())
                    .await
                    .0
                    .expect("write should succeed");
            });

            let buf = [0u8; 9];
            let (read, buf) = reader.read(buf).await;
            assert_eq!(&buf[..read.expect("read should succeed")], b"roundtrip");

            writer_task.await;
        });
    }

    #[test]
    fn pipe_into_poll_into_adaptive_exchanges_data() {
        make_runtime().block_on(async {
            let (mut reader, writer) = pipe().expect("pipe should be created");

            let poll_writer = writer.into_poll().expect("into_poll should succeed");
            let mut writer: Pipe = poll_writer.into_adaptive();

            let writer_task = spawn(async move {
                writer
                    .write(b"adaptive".to_vec())
                    .await
                    .0
                    .expect("write should succeed");
            });

            let buf = [0u8; 8];
            let (read, buf) = reader.read(buf).await;
            assert_eq!(&buf[..read.expect("read should succeed")], b"adaptive");

            writer_task.await;
        });
    }

    // ── PollPipe (readiness / tokio path) ─────────────────────────────────────

    #[test]
    fn poll_pipe_uses_readiness_path() {
        make_runtime().block_on(async {
            let (reader, writer) = pipe().expect("pipe should be created");
            let mut poll_reader: PollPipe =
                reader.into_poll().expect("reader into_poll should succeed");
            let mut poll_writer: PollPipe =
                writer.into_poll().expect("writer into_poll should succeed");

            let writer_task = spawn(async move {
                AsyncWriteExt::write_all(&mut poll_writer, b"mio!")
                    .await
                    .expect("tokio write_all should succeed");
            });

            let mut buf = [0u8; 4];
            AsyncReadExt::read_exact(&mut poll_reader, &mut buf)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&buf, b"mio!");

            writer_task.await;
        });
    }

    #[test]
    fn poll_pipe_write_vectored_via_tokio() {
        make_runtime().block_on(async {
            let (reader, writer) = pipe().expect("pipe should be created");
            let mut poll_reader: PollPipe =
                reader.into_poll().expect("reader into_poll should succeed");
            let mut poll_writer: PollPipe =
                writer.into_poll().expect("writer into_poll should succeed");

            let writer_task = spawn(async move {
                use std::io::IoSlice;
                let slices = [IoSlice::new(b"vec"), IoSlice::new(b"tor")];
                AsyncWriteExt::write_vectored(&mut poll_writer, &slices)
                    .await
                    .expect("tokio write_vectored should succeed");
            });

            let mut buf = [0u8; 6];
            AsyncReadExt::read_exact(&mut poll_reader, &mut buf)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&buf, b"vector");

            writer_task.await;
        });
    }

    #[test]
    fn poll_pipe_is_write_vectored() {
        make_runtime().block_on(async {
            let (reader, writer) = pipe().expect("pipe should be created");
            let poll_reader = reader.into_poll().expect("reader into_poll should succeed");
            let poll_writer = writer.into_poll().expect("writer into_poll should succeed");
            // Both ends should report is_write_vectored = true.
            use tokio::io::AsyncWrite;
            assert!(std::pin::Pin::new(&poll_reader).is_write_vectored());
            assert!(std::pin::Pin::new(&poll_writer).is_write_vectored());
        });
    }

    // ── fd / ownership ────────────────────────────────────────────────────────

    #[test]
    fn pipe_as_raw_fd_is_valid() {
        make_runtime().block_on(async {
            use std::os::fd::AsRawFd;
            let (reader, writer) = pipe().expect("pipe should be created");
            assert!(reader.as_raw_fd() >= 0);
            assert!(writer.as_raw_fd() >= 0);
            assert_ne!(reader.as_raw_fd(), writer.as_raw_fd());
        });
    }

    #[test]
    fn poll_pipe_as_raw_fd_matches_inner() {
        make_runtime().block_on(async {
            use std::os::fd::AsRawFd;
            let (reader, _writer) = pipe().expect("pipe should be created");
            let fd = reader.as_raw_fd();
            let poll_reader = reader.into_poll().expect("into_poll should succeed");
            assert_eq!(poll_reader.as_raw_fd(), fd);
        });
    }

    #[test]
    fn pipe_into_raw_fd_transfers_ownership() {
        make_runtime().block_on(async {
            use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
            let (reader, _writer) = pipe().expect("pipe should be created");
            let fd = reader.as_raw_fd();
            let raw = reader.into_raw_fd();
            assert_eq!(raw, fd);
            // Re-wrap so the fd is properly closed and doesn't leak.
            let _ = unsafe { OwnedFd::from_raw_fd(raw) };
        });
    }

    #[test]
    fn poll_pipe_into_raw_fd_transfers_ownership() {
        make_runtime().block_on(async {
            use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
            let (reader, _writer) = pipe().expect("pipe should be created");
            let fd = reader.as_raw_fd();
            let poll_reader = reader.into_poll().expect("into_poll should succeed");
            let raw = poll_reader.into_raw_fd();
            assert_eq!(raw, fd);
            let _ = unsafe { OwnedFd::from_raw_fd(raw) };
        });
    }

    // ── edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn pipe_read_vectored_empty_bufs_returns_zero() {
        make_runtime().block_on(async {
            let (mut reader, _writer) = pipe().expect("pipe should be created");
            let empty: Vec<libc::iovec> = vec![];
            let (result, _) = reader.read_vectored(empty).await;
            assert_eq!(result.expect("empty vectored read should succeed"), 0);
        });
    }

    #[test]
    fn pipe_write_vectored_empty_bufs_returns_zero() {
        make_runtime().block_on(async {
            let (_reader, mut writer) = pipe().expect("pipe should be created");
            let empty: Vec<libc::iovec> = vec![];
            let (result, _) = writer.write_vectored(empty).await;
            assert_eq!(result.expect("empty vectored write should succeed"), 0);
        });
    }

    #[test]
    fn poll_pipe_flush_and_shutdown_are_noop() {
        make_runtime().block_on(async {
            let (_reader, writer) = pipe().expect("pipe should be created");
            let mut poll_writer = writer.into_poll().expect("into_poll should succeed");
            AsyncWriteExt::flush(&mut poll_writer)
                .await
                .expect("flush should succeed");
            AsyncWriteExt::shutdown(&mut poll_writer)
                .await
                .expect("shutdown should succeed");
        });
    }
}
