use vibeio::RuntimeBuilder;

fn main() -> Result<(), std::io::Error> {
    let runtime = RuntimeBuilder::new().build()?;

    // A basic single-threaded TCP echo server example
    // In production, you would typically use a multi-threaded runtime and offload blocking I/O.
    runtime.block_on(async {
        let listener = vibeio::net::TcpListener::bind("127.0.0.1:5555")?;

        while let Ok((stream, _)) = listener.accept().await {
            vibeio::spawn(async move {
                let (mut reader, mut writer) = vibeio::io::split(stream);
                let _ = vibeio::io::copy(&mut reader, &mut writer).await;
            });
        }

        Ok(())
    })
}
