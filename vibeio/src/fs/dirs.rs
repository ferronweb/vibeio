//! Directory and entry helpers.
//!
//! Async versions of `std::fs` operations for managing directories and entries:
//! [`rename`], [`remove_dir`], [`remove_file`], [`create_dir`], and
//! [`create_dir_all`].

#[cfg(target_os = "linux")]
use std::ffi::CString;

use crate::fs::stat::metadata;
#[cfg(target_os = "linux")]
use crate::op::{MkDirOp, Op, RenameOp, UnlinkOp};

/// Renames a file or directory to a new location.
///
/// This is the async version of [`std::fs::rename`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `renameat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::rename`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `from` does not exist
/// - `to` already exists and is not overwritable
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn rename(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let from_cstr = CString::new(from.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let to_cstr = CString::new(to.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = RenameOp::new(from_cstr, to_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let from = from.to_owned();
        let to = to.to_owned();
        crate::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::rename(from, to)
    }
}

/// Renames a file or directory to a new location.
///
/// This is the async version of [`std::fs::rename`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::rename`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `from` does not exist
/// - `to` already exists and is not overwritable
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn rename(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let from = from.as_ref().to_owned();
        let to = to.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::rename(from, to)
    }
}

/// Removes an empty directory.
///
/// This is the async version of [`std::fs::remove_dir`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `unlinkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::remove_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - `path` is not a directory
/// - The directory is not empty
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn remove_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(path_cstr, true);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_dir(path)
    }
}

/// Removes an empty directory.
///
/// This is the async version of [`std::fs::remove_dir`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::remove_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - `path` is not a directory
/// - The directory is not empty
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn remove_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_dir(path)
    }
}

/// Removes a file.
///
/// This is the async version of [`std::fs::remove_file`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `unlinkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::remove_file`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(path_cstr, false);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_file(path)
    }
}

/// Removes a file.
///
/// This is the async version of [`std::fs::remove_file`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::remove_file`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_file(path)
    }
}

/// Creates a directory.
///
/// This is the async version of [`std::fs::create_dir`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `mkdirat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::create_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions
/// - The directory already exists
#[cfg(target_os = "linux")]
pub async fn create_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        // mode 0o777 is standard for mkdir, umask will be applied
        let mut op = MkDirOp::new(path_cstr, 0o777);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::create_dir(path)
    }
}

/// Creates a directory.
///
/// This is the async version of [`std::fs::create_dir`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::create_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions
/// - The directory already exists
#[cfg(not(target_os = "linux"))]
pub async fn create_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::create_dir(path)
    }
}

/// Creates a new, empty directory and all its parent components if they don't exist.
///
/// This is the async version of [`std::fs::create_dir_all`].
///
/// # Platform-specific behavior
///
/// - This function internally calls [`create_dir`] for each directory component,
///   so it inherits the platform-specific behavior of that function.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path cannot be created
/// - A component in the path is not a directory
/// - The process lacks permissions
pub async fn create_dir_all(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut stack = Vec::new();
    let mut p = path;

    loop {
        // Try to create current path
        match create_dir(p).await {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Exists. Check if dir.
                if let Ok(metadata) = metadata(p).await {
                    if metadata.is_dir() {
                        break;
                    }
                }
                return Err(e);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Parent missing.
                stack.push(p);
                match p.parent() {
                    Some(parent) => p = parent,
                    None => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }

    // Now create directories in stack in reverse order (top to bottom)
    while let Some(p) = stack.pop() {
        match create_dir(p).await {
            Ok(()) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(metadata) = metadata(p).await {
                    if metadata.is_dir() {
                        continue;
                    }
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}
