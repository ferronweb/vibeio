//! Metadata helpers.
//!
//! Async versions of `std::fs` metadata operations: [`metadata`] and
//! [`symlink_metadata`].

#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
use std::ffi::CString;

use crate::fs::Metadata;
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
use crate::op::Op;

/// Returns metadata about a file or directory.
///
/// This is the async version of [`std::fs::metadata`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   for better async performance.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
pub async fn metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
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
        let mut op = crate::op::StatxOp::new(libc::AT_FDCWD, path_cstr, 0, libc::STATX_ALL);
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::metadata(path)?))
    }
}

/// Returns metadata about a file or directory.
///
/// This is the async version of [`std::fs::metadata`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux with glibc/musl v1.2.3+, this either offloads
///   to a blocking thread pool or falls back to [`std::fs::metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
pub async fn metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::metadata(path)?))
    }
}

/// Returns metadata about a file or directory without following symlinks.
///
/// This is the async version of [`std::fs::symlink_metadata`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   with `AT_SYMLINK_NOFOLLOW` flag for better async performance.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::symlink_metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
pub async fn symlink_metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
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
        let mut op = crate::op::StatxOp::new(
            libc::AT_FDCWD,
            path_cstr,
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_ALL,
        );
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::symlink_metadata(path)?))
    }
}

/// Returns metadata about a file or directory without following symlinks.
///
/// This is the async version of [`std::fs::symlink_metadata`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux with glibc/musl v1.2.3+, this either offloads
///   to a blocking thread pool or falls back to [`std::fs::symlink_metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
pub async fn symlink_metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::symlink_metadata(path)?))
    }
}
