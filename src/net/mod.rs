mod tcp_listener;
mod tcp_stream;

pub use tcp_listener::TcpListener;
pub use tcp_stream::TcpStream;

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::{Shutdown, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::{
        driver::AnyDriver,
        executor::{new_runtime, spawn},
    };

    use super::{TcpListener, TcpStream};

    fn try_bind_listener(address: SocketAddr) -> Option<TcpListener> {
        match TcpListener::bind(address) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    #[test]
    fn tcp_listener_and_stream_exchange_data() {
        let runtime = new_runtime(AnyDriver::new_mio().expect("mio driver should initialize"));
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(mut listener) = try_bind_listener(address) else {
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
                Ok::<(), io::Error>(())
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

    #[test]
    fn tcp_stream_implements_tokio_async_io_traits() {
        let runtime = new_runtime(AnyDriver::new_mio().expect("mio driver should initialize"));
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(mut listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut received = [0u8; 5];
                tokio::io::AsyncReadExt::read_exact(&mut stream, &mut received).await?;
                assert_eq!(&received, b"hello");
                tokio::io::AsyncWriteExt::write_all(&mut stream, b"world").await?;
                Ok::<(), io::Error>(())
            });

            let mut client = TcpStream::connect(server_address)
                .await
                .expect("client should connect");
            AsyncWriteExt::write_all(&mut client, b"hello")
                .await
                .expect("tokio write_all should succeed");
            let mut received = [0u8; 5];
            AsyncReadExt::read_exact(&mut client, &mut received)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&received, b"world");

            server.await.expect("server task should complete");
        });
    }
}
