use std::io;
use std::sync::Arc;

use futures_util::lock::Mutex as AsyncMutex;

use super::{AsyncRead, AsyncWrite};

pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> Result<u64, io::Error>
where
    R: AsyncRead + ?Sized,
    W: AsyncWrite + ?Sized,
{
    let mut buffer = [0u8; 8192];
    let mut copied = 0u64;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        writer.write_all(&buffer[..read]).await?;
        copied = copied.saturating_add(read as u64);
    }

    writer.flush().await?;
    Ok(copied)
}

/// Owned read half for a split I/O object.
///
/// The halves share ownership of the inner object via an `Arc<AsyncMutex<T>>`.
pub struct ReadHalf<T> {
    inner: Arc<AsyncMutex<T>>,
}

/// Owned write half for a split I/O object.
pub struct WriteHalf<T> {
    inner: Arc<AsyncMutex<T>>,
}

/// Split an object implementing both `AsyncRead` and `AsyncWrite` into two
/// independently usable halves. The halves share ownership of the original
/// object via an `Arc<AsyncMutex<T>>` so they may be used concurrently in
/// async contexts.
///
/// Note: this is a simple, owned split helper — it clones an `Arc` around
/// a mutex protecting the whole I/O object. It does not provide lock-free
/// simultaneous read/write on the underlying object; callers still need to
/// tolerate possible contention on the mutex.
pub fn split<T>(io: T) -> (ReadHalf<T>, WriteHalf<T>)
where
    T: AsyncRead + AsyncWrite + 'static,
{
    let inner = Arc::new(AsyncMutex::new(io));
    (
        ReadHalf {
            inner: inner.clone(),
        },
        WriteHalf { inner },
    )
}

impl<T> ReadHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    /// Consume the half and return the shared inner `Arc<AsyncMutex<T>>`.
    pub fn into_inner(self) -> Arc<AsyncMutex<T>> {
        self.inner
    }
}

impl<T> WriteHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    /// Consume the half and return the shared inner `Arc<AsyncMutex<T>>`.
    pub fn into_inner(self) -> Arc<AsyncMutex<T>> {
        self.inner
    }
}

impl<T> AsyncRead for ReadHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let mut guard = self.inner.lock().await;
        // Forward the call to the underlying object.
        (&mut *guard).read(buf).await
    }
}

impl<T> AsyncWrite for WriteHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        let mut guard = self.inner.lock().await;
        (&mut *guard).write(buf).await
    }

    async fn flush(&mut self) -> Result<(), io::Error> {
        let mut guard = self.inner.lock().await;
        (&mut *guard).flush().await
    }
}

/// Copy data in both directions between two I/O objects that implement both
/// `AsyncRead` and `AsyncWrite`.
///
/// This function takes ownership of both objects, splits them into read/write
/// halves (so the copies may proceed concurrently), and runs two `copy`
/// operations in parallel:
/// - bytes read from `a` are written to `b`
/// - bytes read from `b` are written to `a`
///
/// Returns a tuple `(a_to_b, b_to_a)` with the number of bytes copied in each
/// direction. The function returns an error if either direction returns an
/// error.
pub async fn copy_bidirectional<A, B>(a: A, b: B) -> Result<(u64, u64), io::Error>
where
    A: AsyncRead + AsyncWrite + 'static,
    B: AsyncRead + AsyncWrite + 'static,
{
    // Split both objects into independent read/write halves.
    let (mut a_r, mut a_w) = split(a);
    let (mut b_r, mut b_w) = split(b);

    // Create the two copy futures. They borrow disjoint halves, so creating
    // both futures is allowed.
    let f1 = copy(&mut a_r, &mut b_w);
    let f2 = copy(&mut b_r, &mut a_w);

    // Run both copies concurrently and await their results.
    let (res1, res2) = futures_util::future::join(f1, f2).await;

    // Propagate any errors; on success return the number of bytes copied for
    // each direction.
    let n1 = res1?;
    let n2 = res2?;
    Ok((n1, n2))
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::{
        executor::new_runtime,
        io::{split, util::copy, AsyncRead, AsyncWrite},
    };

    struct SliceReader {
        data: Vec<u8>,
        offset: usize,
        chunk_size: usize,
    }

    impl SliceReader {
        #[inline]
        fn new(data: &[u8], chunk_size: usize) -> Self {
            Self {
                data: data.to_vec(),
                offset: 0,
                chunk_size,
            }
        }
    }

    impl AsyncRead for SliceReader {
        #[inline]
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
            if self.offset >= self.data.len() {
                return Ok(0);
            }

            let remaining = self.data.len() - self.offset;
            let read_len = remaining.min(buf.len()).min(self.chunk_size.max(1));
            buf[..read_len].copy_from_slice(&self.data[self.offset..self.offset + read_len]);
            self.offset += read_len;
            Ok(read_len)
        }
    }

    struct VecWriter {
        data: Vec<u8>,
        chunk_size: usize,
        flushed: bool,
    }

    impl VecWriter {
        #[inline]
        fn new(chunk_size: usize) -> Self {
            Self {
                data: Vec::new(),
                chunk_size,
                flushed: false,
            }
        }
    }

    impl AsyncWrite for VecWriter {
        #[inline]
        async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
            if buf.is_empty() {
                return Ok(0);
            }
            let write_len = buf.len().min(self.chunk_size.max(1));
            self.data.extend_from_slice(&buf[..write_len]);
            Ok(write_len)
        }

        #[inline]
        async fn flush(&mut self) -> Result<(), io::Error> {
            self.flushed = true;
            Ok(())
        }
    }

    struct ReadWriteVec {
        read_data: Vec<u8>,
        read_off: usize,
        write_data: Vec<u8>,
    }

    impl ReadWriteVec {
        fn new(read: &[u8]) -> Self {
            Self {
                read_data: read.to_vec(),
                read_off: 0,
                write_data: Vec::new(),
            }
        }
    }

    impl AsyncRead for ReadWriteVec {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
            if self.read_off >= self.read_data.len() {
                return Ok(0);
            }
            let remaining = self.read_data.len() - self.read_off;
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.read_data[self.read_off..self.read_off + n]);
            self.read_off += n;
            Ok(n)
        }
    }

    impl AsyncWrite for ReadWriteVec {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
            self.write_data.extend_from_slice(buf);
            Ok(buf.len())
        }

        async fn flush(&mut self) -> Result<(), io::Error> {
            Ok(())
        }
    }

    #[test]
    fn copy_copies_all_bytes_and_flushes_writer() {
        let runtime = new_runtime(crate::driver::AnyDriver::new_mock(), false);
        runtime.block_on(async {
            let mut reader = SliceReader::new(b"hello world", 3);
            let mut writer = VecWriter::new(2);

            let copied = copy(&mut reader, &mut writer)
                .await
                .expect("copy should succeed");
            assert_eq!(copied, 11);
            assert_eq!(writer.data, b"hello world");
            assert!(writer.flushed);
        });
    }

    #[test]
    fn split_allows_separate_read_and_write_halves() {
        let runtime = new_runtime(crate::driver::AnyDriver::new_mock(), false);
        runtime.block_on(async {
            let rw = ReadWriteVec::new(b"abc");
            let (mut r, mut w) = split(rw);

            let mut out = [0u8; 3];
            r.read(&mut out).await.expect("read should succeed");
            assert_eq!(&out, b"abc");

            w.write_all(b"xyz").await.expect("write_all should succeed");

            // Acquire the inner mutex to inspect the written data.
            let inner = r.into_inner();
            let guard = inner.lock().await;
            assert_eq!(guard.write_data, b"xyz");
        });
    }

    #[test]
    fn copy_bidirectional_works() {
        let runtime = new_runtime(crate::driver::AnyDriver::new_mock(), false);
        runtime.block_on(async {
            let a = ReadWriteVec::new(b"hello");
            let b = ReadWriteVec::new(b"world");

            let (a_to_b, b_to_a) = super::copy_bidirectional(a, b)
                .await
                .expect("copy_bidirectional should succeed");

            assert_eq!(a_to_b, 5);
            assert_eq!(b_to_a, 5);
        });
    }
}
