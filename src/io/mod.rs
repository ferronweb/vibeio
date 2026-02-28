#![allow(async_fn_in_trait)]

mod util;

use self::util::*;

use std::io::{self, ErrorKind};

pub trait AsyncRead {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error>;

    #[inline]
    async fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<(), io::Error> {
        while !buf.is_empty() {
            let read = self.read(buf).await?;
            if read == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            let (_, rest) = buf.split_at_mut(read);
            buf = rest;
        }

        Ok(())
    }
}

pub trait AsyncWrite {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error>;

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    #[inline]
    async fn write_all(&mut self, mut buf: &[u8]) -> Result<(), io::Error> {
        while !buf.is_empty() {
            let written = self.write(buf).await?;
            if written == 0 {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            buf = &buf[written..];
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::{
        driver::AnyDriver,
        executor::new_runtime,
        io::{AsyncRead, AsyncWrite},
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

    #[test]
    fn read_exact_and_write_all_work_for_partial_io() {
        let runtime = new_runtime(AnyDriver::new_mock());
        runtime.block_on(async {
            let mut reader = SliceReader::new(b"abcdef", 2);
            let mut out = [0u8; 6];
            reader
                .read_exact(&mut out)
                .await
                .expect("read_exact should read full buffer");
            assert_eq!(&out, b"abcdef");

            let mut writer = VecWriter::new(2);
            writer
                .write_all(b"xyz123")
                .await
                .expect("write_all should write full buffer");
            assert_eq!(writer.data, b"xyz123");
        });
    }
}
