use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket as StdUdpSocket};
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, IntoRawSocket, RawSocket};
use std::time::Duration;

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
};

#[cfg(windows)]
use crate::driver::RegistrationMode;
use crate::fd_inner::InnerRawHandle;
use crate::io::{IoBuf, IoBufMut};
use crate::op::{ConnectOp, RecvOp, RecvfromOp, SendOp, SendtoOp};

#[cfg(unix)]
#[inline]
fn socket_addr_to_raw(address: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    match address {
        SocketAddr::V4(address) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                ))]
                sin_len: 0,
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in>()
                    .write(sockaddr);
                (
                    storage.assume_init(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(address) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                ))]
                sin6_len: 0,
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in6>()
                    .write(sockaddr);
                (
                    storage.assume_init(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }
}

#[cfg(windows)]
#[inline]
fn socket_addr_to_raw(address: SocketAddr) -> (SOCKADDR_STORAGE, i32) {
    match address {
        SocketAddr::V4(address) => {
            let mut sockaddr = SOCKADDR_IN::default();
            sockaddr.sin_family = AF_INET;
            sockaddr.sin_port = address.port().to_be();
            sockaddr.sin_addr.S_un.S_addr = u32::from_ne_bytes(address.ip().octets());

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN>(),
                );
            }
            (storage, std::mem::size_of::<SOCKADDR_IN>() as i32)
        }
        SocketAddr::V6(address) => {
            let mut sockaddr = SOCKADDR_IN6::default();
            sockaddr.sin6_family = AF_INET6;
            sockaddr.sin6_port = address.port().to_be();
            sockaddr.sin6_flowinfo = address.flowinfo();
            sockaddr.sin6_addr.u.Byte = address.ip().octets();
            sockaddr.Anonymous.sin6_scope_id = address.scope_id() as u32;

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN6 as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                );
            }
            (storage, std::mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

#[cfg(unix)]
#[inline]
async fn connect_one(handle: &InnerRawHandle, address: SocketAddr) -> Result<(), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let raw_addr_ptr = (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>();
    let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
    poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
}

#[cfg(windows)]
#[inline]
async fn connect_one(
    handle: &mut InnerRawHandle,
    socket: &mut StdUdpSocket,
    address: SocketAddr,
) -> Result<(), io::Error> {
    // Since ConnectEx (used by `ConnectOp`) requires the socket to be connection-oriented,
    // we use poll-based I/O instead of completion-based I/O on Windows.
    let old_registration_mode = handle.mode();
    handle.rebind_mode(RegistrationMode::Poll)?;
    socket.set_nonblocking(true)?;
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let raw_addr_ptr = (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>();
    let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
    let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
    drop(op);
    handle.rebind_mode(old_registration_mode)?;
    socket.set_nonblocking(!handle.uses_completion())?;
    result
}

pub struct UdpSocket {
    inner: StdUdpSocket,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl UdpSocket {
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let inner = StdUdpSocket::bind(address)?;
        Self::from_std(inner)
    }

    #[inline]
    pub fn from_std(inner: StdUdpSocket) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
        )?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE | Interest::WRITABLE,
        )?);

        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    #[inline]
    pub fn into_std(self) -> StdUdpSocket {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration
        // handle manually and move out the inner socket.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner)
        }
    }

    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    #[inline]
    pub async fn connect(&mut self, address: impl ToSocketAddrs) -> Result<(), io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            #[cfg(unix)]
            let connect_one_result = connect_one(&self.handle, address).await;
            #[cfg(windows)]
            let connect_one_result = connect_one(&mut self.handle, &mut self.inner, address).await;
            match connect_one_result {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    pub async fn recv_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    pub async fn send<B: IoBuf>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = SendOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    pub async fn send_to<B: IoBuf>(
        &self,
        mut buf: B,
        address: impl ToSocketAddrs,
    ) -> (Result<usize, io::Error>, B) {
        let addresses = match address.to_socket_addrs() {
            Ok(addresses) => addresses,
            Err(err) => return (Err(err), buf),
        };
        let mut last_error = None;

        for address in addresses {
            let handle = &self.handle;
            let mut op = SendtoOp::new(handle, buf, address);
            match poll_fn(|cx| handle.poll_op(cx, &mut op)).await {
                Ok(sent) => return (Ok(sent), op.take_bufs()),
                Err(err) => {
                    buf = op.take_bufs();
                    last_error = Some(err);
                }
            }
        }

        (
            Err(last_error
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses"))),
            buf,
        )
    }

    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    pub async fn peek_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    pub fn try_clone(&self) -> Result<Self, io::Error> {
        Self::from_std(self.inner.try_clone()?)
    }

    #[inline]
    pub fn set_broadcast(&self, broadcast: bool) -> Result<(), io::Error> {
        self.inner.set_broadcast(broadcast)
    }

    #[inline]
    pub fn broadcast(&self) -> Result<bool, io::Error> {
        self.inner.broadcast()
    }

    #[inline]
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.inner.set_ttl(ttl)
    }

    #[inline]
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.inner.ttl()
    }

    #[inline]
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v4(multicast_loop_v4)
    }

    #[inline]
    pub fn multicast_loop_v4(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v4()
    }

    #[inline]
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<(), io::Error> {
        self.inner.set_multicast_ttl_v4(multicast_ttl_v4)
    }

    #[inline]
    pub fn multicast_ttl_v4(&self) -> Result<u32, io::Error> {
        self.inner.multicast_ttl_v4()
    }

    #[inline]
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v6(multicast_loop_v6)
    }

    #[inline]
    pub fn multicast_loop_v6(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v6()
    }

    #[inline]
    pub fn join_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.join_multicast_v4(multiaddr, interface)
    }

    #[inline]
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<(), io::Error> {
        self.inner.join_multicast_v6(multiaddr, interface)
    }

    #[inline]
    pub fn leave_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v4(multiaddr, interface)
    }

    #[inline]
    pub fn leave_multicast_v6(
        &self,
        multiaddr: &Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v6(multiaddr, interface)
    }

    #[inline]
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.inner.take_error()
    }

    #[inline]
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_read_timeout(dur)
    }

    #[inline]
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_write_timeout(dur)
    }

    #[inline]
    pub fn read_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.read_timeout()
    }

    #[inline]
    pub fn write_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.write_timeout()
    }
}

#[cfg(unix)]
impl AsRawFd for UdpSocket {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for UdpSocket {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for UdpSocket {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for UdpSocket {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        self.into_std().into_raw_socket()
    }
}

impl Drop for UdpSocket {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::SocketAddr;

    use crate::driver::AnyDriver;

    use super::UdpSocket;

    #[inline]
    fn try_bind_udp(address: SocketAddr) -> Option<UdpSocket> {
        match UdpSocket::bind(address) {
            Ok(socket) => Some(socket),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("udp socket should bind: {err}"),
        }
    }

    #[test]
    fn udp_send_recv_and_peek_variants_work() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");

            let Some(mut server) = try_bind_udp(address) else {
                return;
            };
            let Some(mut client) = try_bind_udp(address) else {
                return;
            };

            let server_addr = server.local_addr().expect("server local_addr should work");
            let client_addr = client.local_addr().expect("client local_addr should work");

            let sent = client
                .send_to(b"ping".to_vec(), server_addr)
                .await
                .0
                .expect("send_to should succeed");
            assert_eq!(sent, 4);

            let peek_from_buf = vec![0u8; 16];
            let (data, peek_from_buf) = server.peek_from(peek_from_buf).await;
            let (peeked, from_peek) = data.expect("peek_from should succeed");
            assert_eq!(&peek_from_buf[..peeked], b"ping");
            assert_eq!(from_peek, client_addr);

            let recv_from_buf = vec![0u8; 16];
            let (data, recv_from_buf) = server.recv_from(recv_from_buf).await;
            let (read, from_read) = data.expect("recv_from should succeed");
            assert_eq!(&recv_from_buf[..read], b"ping");
            assert_eq!(from_read, client_addr);

            server
                .connect(client_addr)
                .await
                .expect("server connect should work");
            client
                .connect(server_addr)
                .await
                .expect("client connect should work");

            let sent = client
                .send(b"echo".to_vec())
                .await
                .0
                .expect("send should succeed");
            assert_eq!(sent, 4);

            let peek_buf = vec![0u8; 16];
            let (peeked, peek_buf) = server.peek(peek_buf).await;
            let peeked = peeked.expect("peek should succeed");
            assert_eq!(&peek_buf[..peeked], b"echo");

            let recv_buf = vec![0u8; 16];
            let (read, recv_buf) = server.recv(recv_buf).await;
            let read = read.expect("recv should succeed");
            assert_eq!(&recv_buf[..read], b"echo");
        });
    }
}
