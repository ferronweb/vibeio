use custom_async::RuntimeBuilder;

fn main() -> Result<(), std::io::Error> {
    let runtime = RuntimeBuilder::new().build()?;

    // A basic TCP echo server example
    runtime.block_on(async {
        let listener = custom_async::net::TcpListener::bind("127.0.0.1:5555")?;

        while let Ok((stream, _)) = listener.accept().await {
            custom_async::spawn(async move {
                let (mut reader, mut writer) = custom_async::io::split(stream);
                let _ = custom_async::io::copy(&mut reader, &mut writer);
            });
        }

        Ok(())
    })
}
