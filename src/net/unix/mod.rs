mod listener;
mod stream;

pub use listener::*;
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self as std_io};
    use std::io::{IoSlice, IoSliceMut};
    use std::net::Shutdown;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{UnixListener, UnixStream};

    fn unique_socket_path(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("custom-async-{name}-{nanos}-{id}.sock"))
    }

    fn cleanup_socket(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std_io::ErrorKind::NotFound => {}
            Err(err) => panic!("failed to cleanup socket {}: {err}", path.display()),
        }
    }

    #[inline]
    fn try_bind_listener(path: &Path) -> Option<UnixListener> {
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    #[test]
    fn unix_listener_and_stream_exchange_data() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let socket_path = unique_socket_path("exchange");
            cleanup_socket(&socket_path);

            let Some(listener) = try_bind_listener(&socket_path) else {
                return;
            };
            assert_eq!(
                listener
                    .local_addr()
                    .expect("listener local addr should be available")
                    .as_pathname(),
                Some(socket_path.as_path())
            );

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut buffer = [0u8; 4];
                let read = stream.read(&mut buffer).await?;
                assert_eq!(&buffer[..read], b"ping");
                stream.write(b"pong").await?;
                stream.shutdown(Shutdown::Both)?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = UnixStream::connect(&socket_path)
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
                    .expect("peer address should be available")
                    .as_pathname(),
                Some(socket_path.as_path())
            );
            client
                .shutdown(Shutdown::Both)
                .expect("shutdown should succeed");

            server.await.expect("server task should complete");
            cleanup_socket(&socket_path);
        });
    }

    #[test]
    fn unix_stream_implements_custom_async_io_traits() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let socket_path = unique_socket_path("traits");
            cleanup_socket(&socket_path);

            let Some(listener) = try_bind_listener(&socket_path) else {
                return;
            };
            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut received = [0u8; 5];
                stream.read_exact(&mut received).await?;
                assert_eq!(&received, b"hello");
                stream.write_all(b"world").await?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = UnixStream::connect(&socket_path)
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
            cleanup_socket(&socket_path);
        });
    }

    #[test]
    fn unix_stream_vectored_io_works() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let socket_path = unique_socket_path("vectored");
            cleanup_socket(&socket_path);

            let Some(listener) = try_bind_listener(&socket_path) else {
                return;
            };
            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut a = [0u8; 3];
                let mut b = [0u8; 5];
                let mut read_bufs = [IoSliceMut::new(&mut a), IoSliceMut::new(&mut b)];
                let read = stream.read_vectored(&mut read_bufs).await?;
                let mut received = Vec::new();
                received.extend_from_slice(&a);
                if read > a.len() {
                    let rem = read - a.len();
                    received.extend_from_slice(&b[..rem]);
                }
                assert_eq!(&received, b"vectored");

                let write_bufs = [IoSlice::new(b"vec"), IoSlice::new(b"tor-ok")];
                let written = stream.write_vectored(&write_bufs).await?;
                assert_eq!(written, 9);
                Ok::<(), std_io::Error>(())
            });

            let mut client = UnixStream::connect(&socket_path)
                .await
                .expect("client should connect");
            let write_bufs = [IoSlice::new(b"vec"), IoSlice::new(b"tored")];
            let written = client
                .write_vectored(&write_bufs)
                .await
                .expect("vectored write should succeed");
            assert_eq!(written, 8);

            let mut a = [0u8; 4];
            let mut b = [0u8; 5];
            let mut read_bufs = [IoSliceMut::new(&mut a), IoSliceMut::new(&mut b)];
            let read = client
                .read_vectored(&mut read_bufs)
                .await
                .expect("vectored read should succeed");
            assert_eq!(read, 9);
            let mut response = Vec::new();
            response.extend_from_slice(&a);
            if read > a.len() {
                let rem = read - a.len();
                response.extend_from_slice(&b[..rem]);
            }
            assert_eq!(&response, b"vector-ok");

            server.await.expect("server task should complete");
            cleanup_socket(&socket_path);
        });
    }
}
