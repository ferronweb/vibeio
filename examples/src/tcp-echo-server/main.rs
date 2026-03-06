use custom_async::{
    io::{AsyncRead, AsyncWrite},
    RuntimeBuilder,
};

fn main() -> Result<(), std::io::Error> {
    let runtime = RuntimeBuilder::new().build()?;

    // A basic TCP echo server example
    runtime.block_on(async {
        let listener = custom_async::net::TcpListener::bind("127.0.0.1:5555")?;
        while let Ok((stream, _)) = listener.accept().await {
            custom_async::spawn(async move {
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
