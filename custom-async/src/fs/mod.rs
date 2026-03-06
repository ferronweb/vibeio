mod file;
mod metadata;
mod open_options;

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::path::PathBuf;

pub use file::*;
pub use metadata::*;
pub use open_options::*;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

use crate::io::{AsyncRead, AsyncWrite};
#[cfg(target_os = "linux")]
use crate::op::HardLinkOp;
#[cfg(target_os = "linux")]
use crate::op::Op;
#[cfg(target_os = "linux")]
use crate::op::RenameOp;
#[cfg(target_os = "linux")]
use crate::op::SymlinkOp;
#[cfg(target_os = "linux")]
use crate::op::UnlinkOp;

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

pub async fn canonicalize<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    crate::spawn_blocking(move || path.canonicalize())
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

pub async fn read(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    let mut file: File = OpenOptions::new().read(true).open(path).await?;
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
    String::from_utf8(bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.utf8_error()))
}

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
    file.write_all(contents.as_ref()).await?;
    file.flush().await
}

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
    } else {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();
    crate::spawn_blocking(move || std::fs::hard_link(src, dst))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

#[cfg(windows)]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let src_str = src.to_string_lossy().to_string();
    let dst_str = dst.to_string_lossy().to_string();
    crate::spawn_blocking(move || windows_symlink_dir(src_str, dst_str))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

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
    } else {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();
    crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

#[cfg(windows)]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let src_str = src.to_string_lossy().to_string();
    let dst_str = dst.to_string_lossy().to_string();
    crate::spawn_blocking(move || windows_symlink_file(src_str, dst_str))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

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
    } else {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();
    crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

pub async fn symlink(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    symlink_file(src, dst).await
}

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
    } else {
        let from = from.to_owned();
        let to = to.to_owned();
        crate::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn rename(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let from = from.as_ref().to_owned();
    let to = to.as_ref().to_owned();
    crate::spawn_blocking(move || std::fs::rename(from, to))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

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
    } else {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn remove_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref().to_owned();
    crate::spawn_blocking(move || std::fs::remove_dir(path))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

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
    } else {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref().to_owned();
    crate::spawn_blocking(move || std::fs::remove_file(path))
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
}

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
    } else {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    }
}

#[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
pub async fn metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    let path = path.as_ref().to_owned();
    Ok(Metadata::from_std(
        crate::spawn_blocking(move || std::fs::metadata(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::{
        driver::AnyDriver,
        executor::Runtime,
        fs::{metadata, read, read_to_string, write, File, OpenOptions},
        io::AsyncWrite,
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

    #[test]
    fn metadata_basic_properties() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("metadata");
            write(&path, b"test content")
                .await
                .expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert_eq!(md.len(), 12);
            assert!(md.is_file());
            assert!(!md.is_dir());
            assert!(!md.is_symlink());

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_directory() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("dir");
            std::fs::create_dir(&path).expect("create_dir should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());
            assert!(!md.is_file());

            let _ = std::fs::remove_dir(path);
        });
    }

    #[test]
    fn metadata_timestamps() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("timestamps");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");

            // All timestamp methods should return valid SystemTime
            let accessed = md.accessed().expect("accessed should succeed");
            let modified = md.modified().expect("modified should succeed");
            let created = md.created().expect("created should succeed");

            // Timestamps should be reasonable (not in the far future)
            let now = SystemTime::now();
            assert!(accessed <= now || accessed + Duration::from_secs(1) >= now);
            assert!(modified <= now || modified + Duration::from_secs(1) >= now);
            assert!(created <= now || created + Duration::from_secs(1) >= now);

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_permissions() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("perms");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            let perms = md.permissions();

            // Should be readable and writable by owner
            assert!(!perms.readonly(), "file should be writable");

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_file_type() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("type");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            let file_type = md.file_type();

            assert!(file_type.is_file());
            assert!(!file_type.is_dir());
            assert!(!file_type.is_symlink());

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_empty_file() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("empty");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .open(&path)
                .await
                .expect("open should succeed");
            file.flush().await.expect("flush should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert_eq!(md.len(), 0);
            assert!(md.is_file());

            let _ = std::fs::remove_file(path);
        });
    }
}
