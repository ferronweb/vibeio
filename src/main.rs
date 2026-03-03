pub mod blocking;
mod builder;
mod driver;
mod executor;
mod fd_inner;
pub mod io;
pub mod net;
mod op;
mod task;

#[cfg(feature = "time")]
pub mod time;
#[cfg(feature = "time")]
mod timer;

pub use crate::builder::*;
pub use crate::executor::*;

use crate::io::{AsyncRead, AsyncWrite};

fn main() -> Result<(), std::io::Error> {
    let runtime = RuntimeBuilder::new().build()?;

    // A basic TCP echo server example
    runtime.block_on(async {
        let listener = net::TcpListener::bind("127.0.0.1:5555")?;
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

        Ok(())
    })
}
