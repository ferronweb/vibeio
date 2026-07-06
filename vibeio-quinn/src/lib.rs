//! A compatibility layer for using [`quinn`] with the `vibeio` async runtime.
//!
//! This crate provides the necessary runtime adapters to run QUIC-based
//! applications built on `quinn` on top of `vibeio`. It implements the
//! traits required by `quinn`'s runtime abstraction:
//!
//! - [`VibeioRuntime`]: A runtime that implements [`quinn::Runtime`] using
//!   `vibeio`'s task spawning, timer, and I/O primitives.
//!
//! # Overview
//!
//! [`VibeioRuntime`] satisfies all three abstract interfaces that `quinn`
//! requires from a runtime:
//!
//! | `quinn` trait | Implementation |
//! |---|---|
//! | [`quinn::Runtime`] | [`VibeioRuntime`] struct |
//! | [`quinn::AsyncTimer`] | Internal wrapper around `vibeio::time::Sleep` |
//! | [`quinn::AsyncUdpSocket`] | Internal wrapper combining `PollUdpSocket` + `quinn-udp` batching |
//! | [`quinn::UdpPoller`] | Internal type backed by `AsyncWritePoll` readiness |
//!
//! # Usage
//!
//! ```rust,ignore
//! use quinn::{Endpoint, EndpointConfig, ServerConfig};
//! use std::sync::Arc;
//! use vibeio_quinn::VibeioRuntime;
//!
//! let runtime = Arc::new(VibeioRuntime);
//!
//! let mut endpoint = Endpoint::new(
//!     EndpointConfig::default(),
//!     None,
//!     runtime,
//! )?;
//! ```
//!
//! # Architecture notes
//!
//! `vibeio` is a **single-threaded** runtime. All spawned futures and I/O
//! operations execute on the thread that owns the runtime. The internal I/O
//! handle types use `Rc<AnyDriver>` internally and are therefore `!Send` and
//! `!Sync`. To satisfy `quinn`'s `Send + Sync + 'static` bounds on its
//! runtime traits, the adapters in this crate use `unsafe impl Send + Sync`
//! under the justification that the runtime's single-threaded architecture
//! prevents data races.
//!
//! Each `quinn` [`Endpoint`](quinn::Endpoint) should be driven from a single
//! `vibeio` task (e.g. via `endpoint.run_until_stopped()` or manual polling).
//! The UDP socket, timer, and task spawn implementations all assume they are
//! used from the thread associated with the current `vibeio` runtime context.

use std::{
    fmt,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Instant,
};

use quinn::udp::{self, RecvMeta, Transmit};
use quinn::{AsyncTimer, AsyncUdpSocket, Runtime, UdpPoller};
use vibeio::io::{AsyncReadPoll, AsyncWritePoll};
use vibeio::net::PollUdpSocket;

/// A [`quinn::Runtime`] implementation for the `vibeio` async runtime.
///
/// This type implements all the traits required by Quinn to operate on top of
/// the `vibeio` runtime: task spawning, timers, and UDP socket I/O.
///
/// `vibeio` is a single-threaded runtime. [`VibeioRuntime`] is safe to
/// construct and use from any thread, but all spawned tasks and I/O operations
/// run on the thread associated with the current `vibeio` runtime instance.
#[derive(Debug, Clone, Copy)]
pub struct VibeioRuntime;

impl Runtime for VibeioRuntime {
    #[inline]
    fn new_timer(&self, i: Instant) -> Pin<Box<dyn AsyncTimer>> {
        Box::pin(VibeioTimer {
            inner: Box::pin(vibeio::time::sleep_until(i)),
        })
    }

    #[inline]
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        let future: Pin<Box<dyn Future<Output = ()>>> = unsafe { std::mem::transmute(future) };
        vibeio::spawn(future);
    }

    #[inline]
    fn wrap_udp_socket(&self, sock: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let io = sock.try_clone()?;
        io.set_nonblocking(true)?;
        let socket = PollUdpSocket::from_std(sock)?;
        let state = udp::UdpSocketState::new((&io).into())?;
        Ok(Arc::new(VibeioUdpSocket {
            io: socket,
            state,
            socket: io,
        }))
    }

    #[inline]
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// ---- Timer ----

struct VibeioTimer {
    inner: Pin<Box<vibeio::time::Sleep>>,
}

impl fmt::Debug for VibeioTimer {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VibeioTimer").finish_non_exhaustive()
    }
}

unsafe impl Send for VibeioTimer {}
unsafe impl Sync for VibeioTimer {}

impl AsyncTimer for VibeioTimer {
    #[inline]
    fn reset(self: Pin<&mut Self>, i: Instant) {
        self.get_mut().inner.as_mut().get_mut().reset(i);
    }

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        self.get_mut().inner.as_mut().poll(cx)
    }
}

// ---- UDP Socket ----

struct VibeioUdpSocket {
    io: PollUdpSocket,
    state: udp::UdpSocketState,
    socket: std::net::UdpSocket,
}

impl fmt::Debug for VibeioUdpSocket {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VibeioUdpSocket")
            .field("local_addr", &self.local_addr())
            .finish_non_exhaustive()
    }
}

unsafe impl Send for VibeioUdpSocket {}
unsafe impl Sync for VibeioUdpSocket {}

impl AsyncUdpSocket for VibeioUdpSocket {
    #[inline]
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(VibeioUdpPoller { socket: self })
    }

    #[inline]
    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.io
            .try_io_writable(|| self.state.send((&self.socket).into(), transmit))
    }

    #[inline]
    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_readable(cx)?);
            match self
                .io
                .try_io_readable(|| self.state.recv((&self.socket).into(), bufs, meta))
            {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    #[inline]
    fn max_transmit_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    #[inline]
    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }

    #[inline]
    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}

// ---- UDP Poller ----

struct VibeioUdpPoller {
    socket: Arc<VibeioUdpSocket>,
}

impl fmt::Debug for VibeioUdpPoller {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VibeioUdpPoller").finish_non_exhaustive()
    }
}

unsafe impl Send for VibeioUdpPoller {}
unsafe impl Sync for VibeioUdpPoller {}

impl UdpPoller for VibeioUdpPoller {
    #[inline]
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.socket.io.poll_writable(cx)
    }
}
