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
