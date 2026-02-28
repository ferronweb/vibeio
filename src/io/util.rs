use std::io;

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

#[cfg(test)]
mod tests {
    use std::io;

    use crate::{
        driver::AnyDriver,
        executor::new_runtime,
        io::{copy, AsyncRead, AsyncWrite},
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
    fn copy_copies_all_bytes_and_flushes_writer() {
        let runtime = new_runtime(AnyDriver::new_mock());
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
}
