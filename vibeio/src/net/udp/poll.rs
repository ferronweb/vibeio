//! A poll-based UDP socket that always uses readiness-based I/O.

use std::cell::RefCell;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket as StdUdpSocket};
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, IntoRawSocket, RawSocket};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::driver::RegistrationMode;
use crate::fd_inner::InnerRawHandle;
use crate::io::{
    AsInnerRawHandle, AsyncReadPoll, AsyncWritePoll, IoBuf, IoBufMut, IoBufTemporaryPoll,
};
use crate::op::{ReadinessOp, RecvOp, RecvfromOp, SendOp, SendtoOp};

use super::UdpSocket;

/// A poll-based UDP socket that always uses readiness-based I/O.
///
/// This is the poll-only counterpart to [`UdpSocket`], similar to how
/// [`PollTcpStream`](crate::net::PollTcpStream) relates to
/// [`TcpStream`](crate::net::TcpStream).
///
/// All I/O operations on this type use readiness-based (poll) I/O,
/// regardless of whether the runtime supports completion-based I/O.
///
/// # Examples
///
/// ```ignore
/// use vibeio::net::UdpSocket;
///
/// let socket = UdpSocket::bind("127.0.0.1:0")?;
/// let poll_socket = socket.into_poll()?;
/// ```
pub struct PollUdpSocket {
    pub(crate) socket: UdpSocket,
    pub(crate) read_ready: RefCell<bool>,
    pub(crate) write_ready: RefCell<bool>,
}

impl PollUdpSocket {
    /// Creates a new `PollUdpSocket` bound to the specified address.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - The address is already in use
    /// - The process lacks permissions to bind to the address
    /// - The runtime is not active
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let inner = StdUdpSocket::bind(address)?;
        Self::from_std(inner)
    }

    /// Creates a new `PollUdpSocket` from a standard library `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUdpSocket) -> Result<Self, io::Error> {
        Ok(Self {
            socket: UdpSocket::from_std_with_mode(inner, RegistrationMode::Poll)?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Converts this poll socket into an adaptive `UdpSocket`.
    #[inline]
    pub fn into_adaptive(self) -> UdpSocket {
        self.socket
    }

    /// Converts this poll socket into a completion-based `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if the runtime does not support completion-based I/O.
    #[inline]
    pub fn into_completion(self) -> Result<UdpSocket, io::Error> {
        let mut socket = self.socket;
        socket.handle.rebind_mode(RegistrationMode::Completion)?;
        socket
            .inner
            .set_nonblocking(!socket.handle.uses_completion())?;
        Ok(socket)
    }

    /// Connects this UDP socket to a remote address.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection fails
    /// - The runtime is not active
    #[inline]
    pub async fn connect(&mut self, address: impl ToSocketAddrs) -> Result<(), io::Error> {
        self.socket.connect(address).await
    }

    /// Returns the local address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not bound.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.socket.local_addr()
    }

    /// Returns the remote address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.socket.peer_addr()
    }

    /// Receives a single datagram message.
    ///
    /// This is the poll-based version of [`UdpSocket::recv`].
    #[inline]
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.recv(buf).await
    }

    /// Receives a single datagram message, returning the sender's address.
    ///
    /// This is the poll-based version of [`UdpSocket::recv_from`].
    #[inline]
    pub async fn recv_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        self.socket.recv_from(buf).await
    }

    /// Sends data on a connected socket.
    ///
    /// This is the poll-based version of [`UdpSocket::send`].
    #[inline]
    pub async fn send<B: IoBuf>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.send(buf).await
    }

    /// Sends data to the specified address.
    ///
    /// This is the poll-based version of [`UdpSocket::send_to`].
    #[inline]
    pub async fn send_to<B: IoBuf>(
        &self,
        buf: B,
        address: impl ToSocketAddrs,
    ) -> (Result<usize, io::Error>, B) {
        self.socket.send_to(buf, address).await
    }

    /// Receives data without removing it from the socket's receive queue.
    ///
    /// This is the poll-based version of [`UdpSocket::peek`].
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.peek(buf).await
    }

    /// Receives data without removing it from the socket's receive queue,
    /// returning the sender's address.
    ///
    /// This is the poll-based version of [`UdpSocket::peek_from`].
    #[inline]
    pub async fn peek_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        self.socket.peek_from(buf).await
    }

    /// Returns a new `PollUdpSocket` that shares the same underlying file descriptor.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be cloned.
    #[inline]
    pub fn try_clone(&self) -> Result<Self, io::Error> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Sets the broadcast flag.
    ///
    /// When set, the socket can send broadcast packets.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_broadcast(&self, broadcast: bool) -> Result<(), io::Error> {
        self.socket.set_broadcast(broadcast)
    }

    /// Returns the current value of the broadcast flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn broadcast(&self) -> Result<bool, io::Error> {
        self.socket.broadcast()
    }

    /// Sets the time-to-live (TTL) value.
    ///
    /// This controls how many hops a packet can traverse before being discarded.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.socket.set_ttl(ttl)
    }

    /// Returns the current TTL value.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.socket.ttl()
    }

    /// Sets the multicast loop flag for IPv4.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<(), io::Error> {
        self.socket.set_multicast_loop_v4(multicast_loop_v4)
    }

    /// Returns the current IPv4 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v4(&self) -> Result<bool, io::Error> {
        self.socket.multicast_loop_v4()
    }

    /// Sets the multicast TTL for IPv4.
    ///
    /// This controls how many hops multicast packets can traverse.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<(), io::Error> {
        self.socket.set_multicast_ttl_v4(multicast_ttl_v4)
    }

    /// Returns the current IPv4 multicast TTL.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_ttl_v4(&self) -> Result<u32, io::Error> {
        self.socket.multicast_ttl_v4()
    }

    /// Sets the multicast loop flag for IPv6.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<(), io::Error> {
        self.socket.set_multicast_loop_v6(multicast_loop_v6)
    }

    /// Returns the current IPv6 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v6(&self) -> Result<bool, io::Error> {
        self.socket.multicast_loop_v6()
    }

    /// Joins a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.socket.join_multicast_v4(multiaddr, interface)
    }

    /// Joins a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<(), io::Error> {
        self.socket.join_multicast_v6(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.socket.leave_multicast_v4(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v6(
        &self,
        multiaddr: &Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        self.socket.leave_multicast_v6(multiaddr, interface)
    }

    /// Takes the pending error from the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.socket.take_error()
    }

    /// Sets the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.socket.set_read_timeout(dur)
    }

    /// Sets the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.socket.set_write_timeout(dur)
    }

    /// Returns the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn read_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.socket.read_timeout()
    }

    /// Returns the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn write_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.socket.write_timeout()
    }

    /// Polls to receive a single datagram message from the socket.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::recv`].
    #[inline]
    pub fn poll_recv(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to receive a single datagram message, returning the sender's address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::recv_from`].
    #[inline]
    pub fn poll_recv_from(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddr), io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvfromOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to send data on a connected socket.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::send`].
    #[inline]
    pub fn poll_send(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = SendOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to send data to the specified address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::send_to`].
    #[inline]
    pub fn poll_send_to(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = SendtoOp::new(handle, buf_temp, target);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to peek at data from the socket without removing it.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::peek`].
    #[inline]
    pub fn poll_peek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new_peek(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to peek at data from the socket without removing it,
    /// returning the sender's address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::peek_from`].
    #[inline]
    pub fn poll_peek_from(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddr), io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvfromOp::new_peek(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
    #[inline]
    pub fn try_io_readable<Io, IoR>(&self, io: Io) -> io::Result<IoR>
    where
        Io: FnOnce() -> io::Result<IoR>,
    {
        if *self.read_ready.borrow() {
            let result = io();
            if result.is_err() {
                *self.read_ready.borrow_mut() = false;
            }
            result
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "read not ready"))
        }
    }

    /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
    #[inline]
    pub fn try_io_writable<Io, IoR>(&self, io: Io) -> io::Result<IoR>
    where
        Io: FnOnce() -> io::Result<IoR>,
    {
        if *self.write_ready.borrow() {
            let result = io();
            if result.is_err() {
                *self.write_ready.borrow_mut() = false;
            }
            result
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "write not ready"))
        }
    }
}

impl AsyncReadPoll for PollUdpSocket {
    #[inline]
    fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if *self.read_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .socket
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_readable(&self.socket.handle))?;
        *self.read_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl AsyncWritePoll for PollUdpSocket {
    #[inline]
    fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if *self.write_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .socket
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_writable(&self.socket.handle))?;
        *self.write_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl<'a> AsInnerRawHandle<'a> for PollUdpSocket {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.socket.as_inner_raw_handle()
    }
}

#[cfg(unix)]
impl AsRawFd for PollUdpSocket {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.socket.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for PollUdpSocket {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.socket.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for PollUdpSocket {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.socket.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for PollUdpSocket {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        self.socket.into_std().into_raw_socket()
    }
}
