use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use crate::executor::current_driver;
use crate::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut};

#[derive(Debug, Default, Clone, Copy)]
pub struct Stdin {
    _private: (),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stdout {
    _private: (),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stderr {
    _private: (),
}

#[inline]
pub fn stdin() -> Stdin {
    Stdin { _private: () }
}

#[inline]
pub fn stdout() -> Stdout {
    Stdout { _private: () }
}

#[inline]
pub fn stderr() -> Stderr {
    Stderr { _private: () }
}

#[inline]
fn read_stdin_blocking(buf: &mut [u8]) -> io::Result<usize> {
    let mut stdin = std::io::stdin();
    stdin.read(buf)
}

#[inline]
fn write_stdout_blocking(buf: &[u8]) -> io::Result<usize> {
    let mut stdout = std::io::stdout();
    stdout.write(buf)
}

#[inline]
fn write_stderr_blocking(buf: &[u8]) -> io::Result<usize> {
    let mut stderr = std::io::stderr();
    stderr.write(buf)
}

#[inline]
fn flush_stdout_blocking() -> io::Result<()> {
    let mut stdout = std::io::stdout();
    stdout.flush()
}

#[inline]
fn flush_stderr_blocking() -> io::Result<()> {
    let mut stderr = std::io::stderr();
    stderr.flush()
}

#[inline]
fn blocking_pool_io_error() -> io::Error {
    io::Error::other("can't spawn blocking task for stdio I/O")
}

#[inline]
async fn read_in_blocking_pool<B: IoBufMut>(buf: B) -> (io::Result<usize>, B) {
    let buf = Arc::new(Mutex::new(RefCell::new(Some(buf))));
    let buf_clone = buf.clone();
    crate::spawn_blocking(move || {
        let mut buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let temp_slice: &'static mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) };
        let result = read_stdin_blocking(temp_slice);
        (result, buf)
    })
    .await
    .unwrap_or_else(|_| {
        (
            Err(blocking_pool_io_error()),
            buf.try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("buf is none"),
        )
    })
}

#[inline]
async fn write_stdout_in_blocking_pool<B: IoBuf>(buf: B) -> (io::Result<usize>, B) {
    let buf = Arc::new(Mutex::new(RefCell::new(Some(buf))));
    let buf_clone = buf.clone();
    crate::spawn_blocking(move || {
        let buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let temp_slice: &'static [u8] =
            unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
        let result = write_stdout_blocking(temp_slice);
        (result, buf)
    })
    .await
    .unwrap_or_else(|_| {
        (
            Err(blocking_pool_io_error()),
            buf.try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("buf is none"),
        )
    })
}

#[inline]
async fn write_stderr_in_blocking_pool<B: IoBuf>(buf: B) -> (io::Result<usize>, B) {
    let buf = Arc::new(Mutex::new(RefCell::new(Some(buf))));
    let buf_clone = buf.clone();
    crate::spawn_blocking(move || {
        let buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let temp_slice: &'static [u8] =
            unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
        let result = write_stderr_blocking(temp_slice);
        (result, buf)
    })
    .await
    .unwrap_or_else(|_| {
        (
            Err(blocking_pool_io_error()),
            buf.try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("buf is none"),
        )
    })
}

#[inline]
async fn flush_stdout_in_blocking_pool() -> io::Result<()> {
    crate::spawn_blocking(move || flush_stdout_blocking())
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn flush_stderr_in_blocking_pool() -> io::Result<()> {
    crate::spawn_blocking(move || flush_stderr_blocking())
        .await
        .map_err(|_| blocking_pool_io_error())?
}

impl AsyncRead for Stdin {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            read_in_blocking_pool(buf).await
        } else {
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) };
            (read_stdin_blocking(slice), buf)
        }
    }
}

impl AsyncWrite for Stdout {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            write_stdout_in_blocking_pool(buf).await
        } else {
            let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
            (write_stdout_blocking(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            flush_stdout_in_blocking_pool().await
        } else {
            flush_stdout_blocking()
        }
    }
}

impl AsyncWrite for Stderr {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            write_stderr_in_blocking_pool(buf).await
        } else {
            let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
            (write_stderr_blocking(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            flush_stderr_in_blocking_pool().await
        } else {
            flush_stderr_blocking()
        }
    }
}
