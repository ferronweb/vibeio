use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::os::fd::{AsRawFd, RawFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::{
    fd_inner::InnerRawHandle,
    net::TcpStream,
    op::{AcceptOp, CompletionAcceptIo},
};

pub struct TcpListener {
    inner: StdTcpListener,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl TcpListener {
    pub fn bind(address: SocketAddr) -> Result<Self, io::Error> {
        let inner = StdTcpListener::bind(address)?;
        inner.set_nonblocking(true)?;
        let handle = ManuallyDrop::new(InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?);
        Ok(Self { inner, handle })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    pub async fn accept(&mut self) -> Result<(TcpStream, SocketAddr), io::Error> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(TcpStream, SocketAddr), io::Error>> {
        if self.handle.uses_completion() {
            return match self.handle.poll_accept_completion(cx) {
                Poll::Ready(Ok((stream, address))) => match TcpStream::from_std(stream) {
                    Ok(stream) => Poll::Ready(Ok((stream, address))),
                    Err(err) => Poll::Ready(Err(err)),
                },
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            };
        }

        let op = AcceptOp::new(&self.handle);
        match self.handle.submit(op, cx.waker().clone()) {
            Ok((stream, address)) => match TcpStream::from_std(stream) {
                Ok(stream) => Poll::Ready(Ok((stream, address))),
                Err(err) => Poll::Ready(Err(err)),
            },
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
