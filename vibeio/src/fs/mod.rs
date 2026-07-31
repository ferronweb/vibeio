//! A file system module for `vibeio`.
//!
//! This module provides async versions of common file system operations:
//! - File operations: [`File`] with async read/write methods
//! - Path operations: [`canonicalize`], [`hard_link`], [`rename`], [`remove_dir`], [`remove_file`]
//! - Directory operations: [`create_dir`], [`create_dir_all`], [`symlink_dir`], [`symlink_file`]
//! - File content helpers: [`read`], [`read_to_string`], [`write()`]
//! - Metadata: [`metadata`], [`symlink_metadata`] for file information
//!
//! Implementation notes:
//! - On Linux with io_uring support, some operations use native async syscalls (e.g. `statx`, `linkat`)
//!   via the async driver. When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations either offload to a blocking thread pool
//!   (if file I/O offload is enabled) or fall back to synchronous std::fs calls.
//! - The runtime must be active when calling these functions; otherwise they will panic.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::fs;
//!
//! // Write to a file
//! fs::write("hello.txt", b"Hello, world!").await?;
//!
//! // Read from a file
//! let contents = fs::read_to_string("hello.txt").await?;
//! println!("File contents: {}", contents);
//!
//! // Create a directory
//! fs::create_dir("my_dir").await?;
//!
//! ```

mod dirs;
mod file;
mod links;
mod metadata;
mod open_options;
mod paths;
mod stat;

pub use dirs::*;
pub use file::*;
pub use links::*;
pub use metadata::*;
pub use open_options::*;
pub use paths::*;
pub use stat::*;

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
        std::env::temp_dir().join(format!("vibeio_{name}_{now}.tmp"))
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
            file.write_exact_at(b"abcdef".to_vec(), 0)
                .await
                .0
                .expect("write_exact_at should succeed");

            let file = File::open(&path)
                .await
                .expect("open for read should succeed");
            let (read, out) = file.read_exact_at([0u8; 4], 2).await;
            read.expect("read_exact_at should succeed");
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

    #[test]
    fn create_dir_works() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("create_dir");
            crate::fs::create_dir(&path)
                .await
                .expect("create_dir should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            crate::fs::remove_dir(path)
                .await
                .expect("remove_dir should succeed");
        });
    }

    #[test]
    fn create_dir_all_works() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let base = unique_path("create_dir_all");
            let path = base.join("a/b/c");

            crate::fs::create_dir_all(&path)
                .await
                .expect("create_dir_all should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            crate::fs::remove_dir(base.join("a/b/c"))
                .await
                .expect("remove_dir c");
            crate::fs::remove_dir(base.join("a/b"))
                .await
                .expect("remove_dir b");
            crate::fs::remove_dir(base.join("a"))
                .await
                .expect("remove_dir a");
            crate::fs::remove_dir(&base).await.expect("remove_dir base");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_metadata_works() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let target = unique_path("symlink_target");
            let link = unique_path("symlink");

            write(&target, b"test content")
                .await
                .expect("write should succeed");
            crate::fs::symlink_file(&target, &link)
                .await
                .expect("symlink_file should succeed");

            // symlink_metadata on the symlink should return info about the symlink itself
            let md = crate::fs::symlink_metadata(&link)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_symlink());

            // metadata on the symlink should follow the link and return info about the target
            let target_md = metadata(&link).await.expect("metadata should succeed");
            assert!(target_md.is_file());
            assert!(!target_md.is_symlink());
            assert_eq!(target_md.len(), 12);

            crate::fs::remove_file(&link)
                .await
                .expect("remove_file should succeed");
            crate::fs::remove_file(&target)
                .await
                .expect("remove_file should succeed");
        });
    }

    #[test]
    fn symlink_metadata_on_regular_file() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("regular_file");
            write(&path, b"content")
                .await
                .expect("write should succeed");

            // symlink_metadata on a regular file should work the same as metadata
            let md = crate::fs::symlink_metadata(&path)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_file());
            assert!(!md.is_symlink());
            assert_eq!(md.len(), 7);

            let _ = std::fs::remove_file(path);
        });
    }
}
