mod driver;
mod executor;
mod fd_inner;
mod io;
mod net;
mod op;
mod task;

#[cfg(feature = "time")]
mod time;
#[cfg(feature = "time")]
mod timer;

use crate::{
    driver::AnyDriver,
    executor::{new_runtime, spawn},
    io::{AsyncRead, AsyncWrite},
};

fn main() {
    let runtime =
        new_runtime(AnyDriver::new_best().expect("failed to initialize runtime I/O driver"));

    // A basic TCP echo server example
    runtime.block_on(async {
        let mut listener = net::TcpListener::bind("127.0.0.1:5555").unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            spawn(async move {
                let mut stream = stream;
                let mut buffer = [0u8; 2048];

                loop {
                    let read = match stream.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(_) => break,
                    };

                    if stream.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
}
