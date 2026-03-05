mod listener;
mod stream;

pub use listener::*;
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::{Shutdown, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{PollTcpStream, TcpListener, TcpStream};

    #[inline]
    fn try_bind_listener(address: SocketAddr) -> Option<TcpListener> {
        match TcpListener::bind(address) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    // TODO: support Windows (IOCP driver)
    #[cfg(unix)]
    #[test]
    fn tcp_listener_and_stream_exchange_data() {
        // TODO: support Windows
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut buffer = [0u8; 4];
                let read = stream.read(&mut buffer).await?;
                assert_eq!(&buffer[..read], b"ping");
                stream.write(b"pong").await?;
                stream.shutdown(Shutdown::Both)?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = TcpStream::connect(server_address)
                .await
                .expect("client should connect");
            client.write(b"ping").await.expect("client should write");
            let mut response = [0u8; 4];
            let read = client
                .read(&mut response)
                .await
                .expect("client should read");
            assert_eq!(&response[..read], b"pong");
            assert_eq!(
                client
                    .peer_addr()
                    .expect("peer address should be available"),
                server_address
            );
            client
                .shutdown(Shutdown::Both)
                .expect("shutdown should succeed");

            server.await.expect("server task should complete");
        });
    }

    // TODO: support Windows (IOCP driver)
    #[cfg(unix)]
    #[test]
    fn tcp_stream_implements_custom_async_io_traits() {
        // TODO: support Windows
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut received = [0u8; 5];
                stream.read_exact(&mut received).await?;
                assert_eq!(&received, b"hello");
                stream.write_all(b"world").await?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = TcpStream::connect(server_address)
                .await
                .expect("client should connect");
            client
                .write_all(b"hello")
                .await
                .expect("custom write_all should succeed");
            let mut received = [0u8; 5];
            client
                .read_exact(&mut received)
                .await
                .expect("custom read_exact should succeed");
            assert_eq!(&received, b"world");

            server.await.expect("server task should complete");
        });
    }

    // TODO: support Windows (IOCP driver)
    #[cfg(unix)]
    #[test]
    fn poll_tcp_stream_uses_readiness_path() {
        // TODO: support Windows
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut received = [0u8; 4];
                let read = stream.read(&mut received).await?;
                assert_eq!(&received[..read], b"mio!");
                stream.write(b"ok").await?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = PollTcpStream::connect(server_address)
                .await
                .expect("client should connect");
            AsyncWriteExt::write_all(&mut client, b"mio!")
                .await
                .expect("tokio write_all should succeed");
            let mut response = [0u8; 2];
            AsyncReadExt::read_exact(&mut client, &mut response)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&response, b"ok");

            server.await.expect("server task should complete");
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn poll_stream_connect_works_with_live_completion_listener_under_uring() {
        let Ok(driver) = AnyDriver::new_uring() else {
            return;
        };
        let runtime = crate::executor::Runtime::new(driver);
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut received = [0u8; 4];
                stream.read_exact(&mut received).await?;
                assert_eq!(&received, b"poll");
                stream.write_all(b"done").await?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = PollTcpStream::connect(server_address)
                .await
                .expect("poll client should connect while completion listener is active");
            AsyncWriteExt::write_all(&mut client, b"poll")
                .await
                .expect("poll write should succeed");
            let mut response = [0u8; 4];
            AsyncReadExt::read_exact(&mut client, &mut response)
                .await
                .expect("poll read should succeed");
            assert_eq!(&response, b"done");

            server.await.expect("server task should complete");
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stream_mode_conversion_works_with_uring_driver() {
        let Ok(driver) = AnyDriver::new_uring() else {
            return;
        };
        let runtime = crate::executor::Runtime::new(driver);
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;

                let mut first = [0u8; 4];
                stream.read_exact(&mut first).await?;
                assert_eq!(&first, b"ping");
                stream.write_all(b"one!").await?;

                let mut second = [0u8; 4];
                stream.read_exact(&mut second).await?;
                assert_eq!(&second, b"pong");
                stream.write_all(b"two!").await?;

                Ok::<(), std_io::Error>(())
            });

            let completion_client = TcpStream::connect(server_address)
                .await
                .expect("client should connect");
            let mut poll_client = completion_client
                .into_poll()
                .expect("completion->poll conversion should succeed");

            AsyncWriteExt::write_all(&mut poll_client, b"ping")
                .await
                .expect("poll write should succeed");
            let mut first_reply = [0u8; 4];
            AsyncReadExt::read_exact(&mut poll_client, &mut first_reply)
                .await
                .expect("poll read should succeed");
            assert_eq!(&first_reply, b"one!");

            let mut completion_client = poll_client
                .into_completion()
                .expect("poll->completion conversion should succeed");

            completion_client
                .write_all(b"pong")
                .await
                .expect("completion write should succeed");
            let mut second_reply = [0u8; 4];
            completion_client
                .read_exact(&mut second_reply)
                .await
                .expect("completion read should succeed");
            assert_eq!(&second_reply, b"two!");

            server.await.expect("server task should complete");
        });
    }
}
