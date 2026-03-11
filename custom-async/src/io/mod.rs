#![allow(async_fn_in_trait)]

mod buf;
#[cfg(all(unix, feature = "pipe"))]
mod pipe;
#[cfg(all(target_os = "linux", feature = "splice"))]
mod splice;
mod util;

use crate::fd_inner::InnerRawHandle;

pub use self::buf::*;
#[cfg(all(unix, feature = "pipe"))]
pub use self::pipe::*;
#[cfg(all(target_os = "linux", feature = "splice"))]
pub use self::splice::*;
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

pub trait AsInnerRawHandle<'a> {
    #[allow(private_interfaces)]
    fn as_inner_raw_handle(&'a self) -> &'a crate::fd_inner::InnerRawHandle;
}

impl<'a> AsInnerRawHandle<'a> for InnerRawHandle {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self
    }
}
