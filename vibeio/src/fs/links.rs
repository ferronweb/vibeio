//! Link helpers.
//!
//! Async versions of `std::fs` link operations: [`hard_link`], [`symlink_dir`],
//! [`symlink_file`], and [`symlink`], plus Windows-specific symlink helpers.

#[cfg(target_os = "linux")]
use std::ffi::CString;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

#[cfg(target_os = "linux")]
use crate::op::{HardLinkOp, Op, SymlinkOp};

/// Creates a symbolic link to a directory on Windows.
///
/// This is a Windows-specific helper function that uses the `CreateSymbolicLinkW` API.
/// For cross-platform symlink creation, use [`symlink_dir`] instead.
///
/// # Platform-specific behavior
///
/// - This function is only available on Windows.
/// - It creates a symbolic link to a directory using the Windows API.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub fn windows_symlink_dir(path: String, target: String) -> std::io::Result<()> {
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let res = CreateSymbolicLinkW(
            path_w.as_ptr(),
            target_w.as_ptr(),
            1, // SYMBOLIC_LINK_FLAG_DIRECTORY
        );
        if !res {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Creates a symbolic link to a file on Windows.
///
/// This is a Windows-specific helper function that uses the `CreateSymbolicLinkW` API.
/// For cross-platform symlink creation, use [`symlink_file`] instead.
///
/// # Platform-specific behavior
///
/// - This function is only available on Windows.
/// - It creates a symbolic link to a file using the Windows API.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub fn windows_symlink_file(path: String, target: String) -> std::io::Result<()> {
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let res = CreateSymbolicLinkW(
            path_w.as_ptr(),
            target_w.as_ptr(),
            0, // SYMBOLIC_LINK_FLAG_FILE
        );
        if !res {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
/// Creates a hard link at the destination path pointing to the source.
///
/// This is the async version of [`std::fs::hard_link`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `linkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::hard_link`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist
/// - `dst` already exists
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;

        let driver = driver.expect("invalid driver state");
        let mut op = HardLinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a hard link at the destination path pointing to the source.
///
/// This is the async version of [`std::fs::hard_link`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::hard_link`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist
/// - `dst` already exists
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_dir`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses the [`windows_symlink_dir`] helper.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();
        crate::spawn_blocking(move || windows_symlink_dir(src_str, dst_str))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();

        windows_symlink_dir(src_str, dst_str)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(target_os = "linux")]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        // On Linux with io_uring, use SymlinkOp
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = SymlinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On other Unix platforms (not Linux or Windows), this either offloads to a
///   blocking thread pool or falls back to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_file`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses the [`windows_symlink_file`] helper.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();
        crate::spawn_blocking(move || windows_symlink_file(src_str, dst_str))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();

        windows_symlink_file(src_str, dst_str)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(target_os = "linux")]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        // On Linux with io_uring, use SymlinkOp
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = SymlinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On other Unix platforms (not Linux or Windows), this either offloads to a
///   blocking thread pool or falls back to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link.
///
/// This is a convenience function that calls [`symlink_file`]. Use this when you
/// don't know or don't care whether the source is a file or directory.
///
/// For explicit symlink creation, use [`symlink_file`] or [`symlink_dir`] instead.
///
/// # Platform-specific behavior
///
/// See [`symlink_file`] for platform-specific behavior details.
///
/// # Errors
///
/// See [`symlink_file`] for error conditions.
pub async fn symlink(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    symlink_file(src, dst).await
}
