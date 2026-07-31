//! Path-based file helpers.
//!
//! Async versions of `std::fs` path helpers: [`canonicalize`], [`read`],
//! [`read_to_string`], and [`write`].

use std::path::PathBuf;

use crate::fs::file::File;
use crate::fs::open_options::OpenOptions;
use crate::io::{AsyncRead, AsyncWrite, IoBuf};

/// Returns the canonical form of a path with all components normalized.
///
/// This is the async version of [`std::fs::canonicalize`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::canonicalize`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions to access components of the path
pub async fn canonicalize<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    if crate::executor::offload_fs() {
        crate::spawn_blocking(move || path.canonicalize())
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        path.canonicalize()
    }
}

/// Reads the entire contents of a file into a vector of bytes.
///
/// This is the async version of [`std::fs::read`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::read`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to read the file
pub async fn read(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    let mut file: File = OpenOptions::new().read(true).open(path).await?;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        let (read, returned_buf) = file.read(buf).await;
        let read = read?;
        buf = returned_buf;

        if read == 0 {
            break;
        }

        let slice =
            unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len().min(read)) };
        bytes.extend_from_slice(slice);
    }

    Ok(bytes)
}

/// Reads the entire contents of a file into a string.
///
/// This is the async version of [`std::fs::read_to_string`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::read_to_string`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - [`read`] fails
/// - The file contents are not valid UTF-8
pub async fn read_to_string(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.utf8_error()))
}

/// Writes a byte slice to a file, creating it if necessary.
///
/// This is the async version of [`std::fs::write`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::write`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - The file cannot be opened for writing
/// - The write operation fails
pub async fn write(
    path: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;

    let mut slice = contents.as_ref();
    while !slice.is_empty() {
        let (w, _) = file.write(slice.to_vec()).await;
        let w = w?;
        if w == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole buffer",
            ));
        }
        slice = &slice[w..];
    }
    file.flush().await
}
