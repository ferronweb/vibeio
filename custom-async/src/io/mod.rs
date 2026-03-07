#![allow(async_fn_in_trait)]

mod buf;
mod util;

pub use self::buf::*;
pub use self::util::*;

use std::io::{self, ErrorKind};

pub trait AsyncRead {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B);

    #[inline]
    async fn read_vectored<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
    ) -> (Result<usize, io::Error>, V) {
        (
            Err(io::Error::new(
                ErrorKind::Unsupported,
                "readv not implemented",
            )),
            bufs,
        )
    }
}

pub trait AsyncWrite {
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B);

    #[inline]
    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> (Result<usize, io::Error>, V) {
        (
            Err(io::Error::new(
                ErrorKind::Unsupported,
                "readv not implemented",
            )),
            bufs,
        )
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::io::{AsyncRead, AsyncWrite};

    use crate::io::{IoBuf, IoBufMut};

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
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (Result<usize, io::Error>, B) {
            if self.offset >= self.data.len() {
                return (Ok(0), buf);
            }

            let remaining = self.data.len() - self.offset;
            let cap = buf.buf_capacity();
            let read_len = remaining.min(cap).min(self.chunk_size.max(1));

            unsafe {
                let ptr = buf.as_buf_mut_ptr().add(buf.buf_len());
                std::ptr::copy_nonoverlapping(self.data[self.offset..].as_ptr(), ptr, read_len);
                buf.set_buf_init(buf.buf_len() + read_len);
            }

            self.offset += read_len;
            (Ok(read_len), buf)
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
        async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
            let len = buf.buf_len();
            if len == 0 {
                return (Ok(0), buf);
            }
            let write_len = len.min(self.chunk_size.max(1));

            let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), write_len) };
            self.data.extend_from_slice(slice);

            (Ok(write_len), buf)
        }

        #[inline]
        async fn flush(&mut self) -> Result<(), io::Error> {
            self.flushed = true;
            Ok(())
        }
    }
}
