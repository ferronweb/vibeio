//! A compatibility layer for using [`hyper`] with the `vibeio` async runtime.
//!
//! This crate provides the necessary adapters to run `hyper`-based HTTP servers
//! and clients on top of `vibeio` instead of `tokio`. It implements the traits
//! required by `hyper`'s runtime abstraction:
//!
//! - [`VibeioExecutor`]: An executor that spawns tasks on the `vibeio` runtime
//!   using `vibeio::spawn`.
//! - [`VibeioTimer`]: A timer that uses `vibeio::time::sleep` for delay operations.
//! - [`VibeioIo`]: A wrapper type that adapts `vibeio`'s I/O types to implement
//!   `hyper`'s `Read` and `Write` traits, and also implements `tokio`'s
//!   `AsyncRead` and `AsyncWrite` traits for compatibility.
//!
//! # Overview
//!
//! This crate enables `hyper` to work with `vibeio` by implementing `hyper`'s
//! runtime traits (`Executor`, `Timer`, `Read`, `Write`, `Sleep`) in terms of
//! `vibeio` primitives.
//!
//! ## Executor
//!
//! The [`VibeioExecutor`] type implements `hyper::rt::Executor` by spawning
//! futures onto the `vibeio` runtime via `vibeio::spawn`.
//!
//! ## Timer
//!
//! The [`VibeioTimer`] type implements `hyper::rt::Timer` by converting sleep
//! requests into `vibeio::time::sleep` futures wrapped in a compatible type.
//!
//! ## I/O Adapters
//!
//! The [`VibeioIo<T>`] wrapper adapts any type that implements `tokio::io::AsyncRead`
//! and `tokio::io::AsyncWrite` to work with `hyper`'s I/O traits. It also
//! implements the reverse conversion, allowing `hyper`'s I/O types to be used
//! with `tokio`-style async functions.
//!
//! # Implementation notes
//!
//! - The `VibeioIo` wrapper uses `Pin<Box<T>>` internally to support the
//!   trait implementations required by `hyper` and `tokio`.
//! - The `VibeioSleep` type (internal) implements both `hyper::rt::Sleep`
//!   and `std::future::Future` to bridge the two runtimes' sleep abstractions.
//! - Timer handles are properly cancelled when `VibeioSleep` is dropped to
//!   avoid resource leaks.

use std::{
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll},
};

use hyper::rt::{Executor, Sleep, Timer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// An executor that spawns tasks onto the `vibeio` runtime.
///
/// This type implements `hyper::rt::Executor` and uses `vibeio::spawn` to
/// execute futures on the `vibeio` runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct VibeioExecutor;

impl<Fut> Executor<Fut> for VibeioExecutor
where
    Fut: std::future::Future + 'static,
    Fut::Output: 'static,
{
    #[inline]
    fn execute(&self, fut: Fut) {
        vibeio::spawn(fut);
    }
}

/// A timer that uses `vibeio`'s time utilities for sleep operations.
///
/// This type implements `hyper::rt::Timer` and uses `vibeio::time::sleep`
/// to implement delay operations.
#[derive(Debug, Default, Clone, Copy)]
pub struct VibeioTimer;

impl Timer for VibeioTimer {
    #[inline]
    fn sleep(&self, duration: std::time::Duration) -> Pin<Box<dyn Sleep>> {
        Box::pin(VibeioSleep {
            inner: Box::pin(vibeio::time::sleep(duration)),
        })
    }

    #[inline]
    fn sleep_until(&self, deadline: std::time::Instant) -> Pin<Box<dyn Sleep>> {
        Box::pin(VibeioSleep {
            inner: Box::pin(vibeio::time::sleep_until(deadline)),
        })
    }

    #[inline]
    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: std::time::Instant) {
        if let Some(mut sleep) = sleep.as_mut().downcast_mut_pin::<VibeioSleep>() {
            sleep.reset(new_deadline);
        }
    }
}

/// A sleep future that wraps `vibeio::time::Sleep` and implements `hyper::rt::Sleep`.
struct VibeioSleep {
    inner: Pin<Box<vibeio::time::Sleep>>,
}

impl std::future::Future for VibeioSleep {
    type Output = ();

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

impl Sleep for VibeioSleep {}

unsafe impl Send for VibeioSleep {}
unsafe impl Sync for VibeioSleep {}

impl VibeioSleep {
    #[inline]
    fn reset(&mut self, new_deadline: std::time::Instant) {
        self.inner.reset(new_deadline);
    }
}

/// A wrapper type that adapts I/O types for use with `hyper` and `tokio`.
///
/// `VibeioIo<T>` wraps any type `T` that implements `tokio::io::AsyncRead`
/// and `tokio::io::AsyncWrite` and provides implementations for:
///
/// - `hyper::rt::Read` and `hyper::rt::Write`
/// - `tokio::io::AsyncRead` and `tokio::io::AsyncWrite`
///
/// This allows seamless interoperability between `hyper`'s I/O traits and
/// `tokio`'s async I/O traits when using the `vibeio` runtime.
#[derive(Debug)]
pub struct VibeioIo<T> {
    inner: Pin<Box<T>>,
}

impl<T> VibeioIo<T> {
    /// Creates a new `VibeioIo` wrapper around the given I/O type.
    #[inline]
    pub fn new(inner: T) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<T> Deref for VibeioIo<T> {
    type Target = Pin<Box<T>>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for VibeioIo<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// Implement hyper::rt::Read for types that implement tokio::io::AsyncRead
impl<T> hyper::rt::Read for VibeioIo<T>
where
    T: AsyncRead,
{
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = {
            let mut tbuf = unsafe { ReadBuf::uninit(buf.as_mut()) };
            match self.inner.as_mut().poll_read(cx, &mut tbuf) {
                Poll::Ready(Ok(_)) => tbuf.filled().len(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };

        unsafe { buf.advance(n) };
        Poll::Ready(Ok(()))
    }
}

// Implement hyper::rt::Write for types that implement tokio::io::AsyncWrite
impl<T> hyper::rt::Write for VibeioIo<T>
where
    T: AsyncWrite,
{
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write_vectored(cx, bufs)
    }
}

// Implement tokio::io::AsyncRead for types that implement hyper::rt::Read
impl<T> AsyncRead for VibeioIo<T>
where
    T: hyper::rt::Read,
{
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        tbuf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled = tbuf.filled().len();
        let sub_filled = {
            let mut buf = unsafe { hyper::rt::ReadBuf::uninit(tbuf.unfilled_mut()) };
            match self.inner.as_mut().poll_read(cx, buf.unfilled()) {
                Poll::Ready(Ok(_)) => buf.filled().len(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };

        unsafe {
            tbuf.assume_init(sub_filled);
            tbuf.set_filled(filled + sub_filled);
        };
        Poll::Ready(Ok(()))
    }
}

// Implement tokio::io::AsyncWrite for types that implement hyper::rt::Write
impl<T> AsyncWrite for VibeioIo<T>
where
    T: hyper::rt::Write,
{
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write_vectored(cx, bufs)
    }
}
