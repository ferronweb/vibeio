mod file;
mod open_options;

pub use file::File;
pub use open_options::OpenOptions;

use crate::io::{AsyncRead, AsyncWrite};

pub async fn read(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    let mut file: File = OpenOptions::new()
        .read(true)
        .open(path)
        .await?;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        let read = file.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
    }

    Ok(bytes)
}

pub async fn read_to_string(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.utf8_error()))
}

pub async fn write(path: impl AsRef<std::path::Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;
    file.write_all(contents.as_ref()).await?;
    file.flush().await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{
        driver::AnyDriver,
        executor::Runtime,
        fs::{read, read_to_string, write, File, OpenOptions},
    };

    fn unique_path(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("custom_async_{name}_{now}.tmp"))
    }

    #[test]
    fn fs_read_write_helpers_work() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("helpers");
            write(&path, b"hello world")
                .await
                .expect("write helper should succeed");

            let bytes = read(&path).await.expect("read helper should succeed");
            assert_eq!(bytes, b"hello world");

            let string = read_to_string(&path)
                .await
                .expect("read_to_string helper should succeed");
            assert_eq!(string, "hello world");

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn file_read_at_and_write_exact_at_work() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("offset");
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open for write should succeed");
            file.write_exact_at(b"abcdef", 0)
                .await
                .expect("write_exact_at should succeed");

            let file = File::open(&path)
                .await
                .expect("open for read should succeed");
            let mut out = [0u8; 4];
            file.read_exact_at(&mut out, 2)
                .await
                .expect("read_exact_at should succeed");
            assert_eq!(&out, b"cdef");

            let _ = std::fs::remove_file(path);
        });
    }
}
