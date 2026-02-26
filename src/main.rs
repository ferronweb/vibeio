mod driver;
mod executor;
mod fd_inner;
mod net;
mod op;
mod task;

use std::net::ToSocketAddrs;

use executor::{new_runtime, yield_now};

use crate::{driver::AnyDriver, executor::spawn};

fn main() {
    let runtime =
        new_runtime(AnyDriver::new_mio().expect("failed to initialize mio-backed runtime driver"));

    // A basic TCP echo server example
    runtime.block_on(async {
        let mut listener =
            net::TcpListener::bind("127.0.0.1:5555".to_socket_addrs().unwrap().next().unwrap())
                .unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(stream);
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
}
