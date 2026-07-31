use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
use std::os::darwin::fs::MetadataExt as AppleMetadataExt;
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt as LinuxMetadataExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as UnixMetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as WindowsMetadataExt;

/// File metadata information.
///
/// This type mirrors a subset of [`std::fs::Metadata`]. On supported Linux
/// targets we back it by `statx` (via io_uring), otherwise we delegate to
/// `std::fs::metadata` on a blocking thread.
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   for better async performance.
/// - On other platforms, this uses the standard library's `std::fs::Metadata`.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs;
///
/// let metadata = fs::metadata("hello.txt").await?;
/// println!("File size: {} bytes", metadata.len());
/// ```
#[derive(Clone, Debug)]
pub struct Metadata {
    inner: MetadataInner,
}

#[derive(Clone, Debug)]
enum MetadataInner {
    #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
    Statx(libc::statx),
    Std(std::fs::Metadata),
}

impl Metadata {
    /// Creates a new `Metadata` from a standard library `std::fs::Metadata`.
    #[inline]
    pub(crate) fn from_std(md: std::fs::Metadata) -> Self {
        Self {
            inner: MetadataInner::Std(md),
        }
    }

    /// Creates a new `Metadata` from a `libc::statx` structure.
    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn from_statx(st: libc::statx) -> Self {
        Self {
            inner: MetadataInner::Statx(st),
        }
    }

    /// Returns the size of the file in bytes.
    #[allow(clippy::len_without_is_empty)]
    #[inline]
    pub fn len(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_size,
            MetadataInner::Std(md) => md.len(),
        }
    }

    /// Returns the file permissions.
    #[inline]
    pub fn permissions(&self) -> std::fs::Permissions {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::Permissions::from_mode(st.stx_mode as u32)
            }
            MetadataInner::Std(md) => md.permissions(),
        }
    }

    /// Returns the file type.
    #[inline]
    pub fn file_type(&self) -> FileType {
        FileType {
            is_dir: self.is_dir(),
            is_file: self.is_file(),
            is_symlink: self.is_symlink(),
        }
    }

    /// Returns `true` if this metadata is for a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFDIR,
            MetadataInner::Std(md) => md.is_dir(),
        }
    }

    /// Returns `true` if this metadata is for a regular file.
    #[inline]
    pub fn is_file(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFREG,
            MetadataInner::Std(md) => md.is_file(),
        }
    }

    /// Returns `true` if this metadata is for a symbolic link.
    #[inline]
    pub fn is_symlink(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFLNK,
            MetadataInner::Std(md) => md.file_type().is_symlink(),
        }
    }

    /// Returns the time the file was last accessed.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn accessed(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_atime),
            MetadataInner::Std(md) => md.accessed(),
        }
    }

    /// Returns the time the file was created.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn created(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_btime),
            MetadataInner::Std(md) => md.created(),
        }
    }

    /// Returns the time the file was last modified.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn modified(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_mtime),
            MetadataInner::Std(md) => md.modified(),
        }
    }

    /// Returns the device ID on which this file resides.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_dev(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => libc::makedev(st.stx_dev_major, st.stx_dev_minor) as u64,
            MetadataInner::Std(md) => md.dev(),
        }
    }

    /// Returns the inode number.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_ino(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ino, // u64
            MetadataInner::Std(st) => st.st_ino(),
        }
    }

    /// Returns the file type and mode.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_mode(&self) -> u32 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mode as u32,
            MetadataInner::Std(st) => st.st_mode(),
        }
    }

    /// Returns the number of hard links to file.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_nlink(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_nlink as u64, // u32 → u64
            MetadataInner::Std(st) => st.st_nlink(),
        }
    }

    /// Returns the user ID of the file owner.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_uid(&self) -> u32 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_uid, // u32
            MetadataInner::Std(st) => st.st_uid(),
        }
    }

    /// Returns the group ID of the file owner.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_gid(&self) -> u32 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_gid, // u32
            MetadataInner::Std(st) => st.st_gid(),
        }
    }

    /// Returns the device ID that this file represents (if it is a special one).
    ///
    /// **Note:** Not available via `statx` — the `statx` syscall does not
    /// return device information. This method is only available on the
    /// standard `Metadata` variant (non-Linux or non-io_uring paths).
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_rdev(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => libc::makedev(st.stx_rdev_major, st.stx_rdev_minor) as u64,
            MetadataInner::Std(md) => md.rdev(),
        }
    }

    /// Returns the size of the file (if it is a regular file or a symbolic link) in bytes.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_size(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_size, // u64
            MetadataInner::Std(md) => md.st_size(),
        }
    }

    /// Returns the last access time of the file, in seconds since Unix Epoch.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_atime(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_atime.tv_sec, // i64
            MetadataInner::Std(md) => md.st_atime(),
        }
    }

    /// Returns the last access time of the file, in nanoseconds since `st_atime`.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_atime_nsec(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_atime.tv_nsec as i64, // u32 → i64
            MetadataInner::Std(md) => md.st_atime_nsec(),
        }
    }

    /// Returns the last modification time of the file, in seconds since Unix Epoch.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_mtime(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mtime.tv_sec, // i64
            MetadataInner::Std(md) => md.st_mtime(),
        }
    }

    /// Returns the last modification time of the file, in nanoseconds since `st_mtime`.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_mtime_nsec(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mtime.tv_nsec as i64, // u32 → i64
            MetadataInner::Std(md) => md.st_mtime_nsec(),
        }
    }

    /// Returns the last status change time of the file, in seconds since Unix Epoch.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_ctime(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ctime.tv_sec, // i64
            MetadataInner::Std(md) => md.st_ctime(),
        }
    }

    /// Returns the last status change time of the file, in nanoseconds since `st_ctime`.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_ctime_nsec(&self) -> i64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ctime.tv_nsec as i64, // u32 → i64
            MetadataInner::Std(md) => md.st_ctime_nsec(),
        }
    }

    /// Returns the "preferred" block size for efficient filesystem I/O.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_blksize(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_blksize as u64, // u32 → u64
            MetadataInner::Std(md) => md.st_blksize(),
        }
    }

    /// Returns the number of blocks allocated to the file, 512-byte units.
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_blocks(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_blocks, // u64
            MetadataInner::Std(md) => md.st_blocks(),
        }
    }

    /// Returns the subvolume ID of the file (statx-specific).
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_subvol(&self) -> Option<u64> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => Some(st.stx_subvol), // u64
            MetadataInner::Std(_) => None,
        }
    }

    /// Returns the file attributes (statx-specific).
    #[cfg(target_os = "linux")]
    #[inline]
    pub fn st_attributes(&self) -> Option<u64> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => Some(st.stx_attributes), // u64
            MetadataInner::Std(_) => None,
        }
    }

    /// Returns the ID of the device containing the file.
    #[cfg(unix)]
    #[inline]
    pub fn dev(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.dev(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => libc::makedev(st.stx_dev_major, st.stx_dev_minor) as u64,
        }
    }

    /// Returns the inode number.
    #[cfg(unix)]
    #[inline]
    pub fn ino(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.ino(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ino,
        }
    }

    /// Returns the rights applied to this file.
    #[cfg(unix)]
    #[inline]
    pub fn mode(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.mode(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mode as u32,
        }
    }

    /// Returns the number of hard links pointing to this file.
    #[cfg(unix)]
    #[inline]
    pub fn nlink(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.nlink(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_nlink as u64,
        }
    }

    /// Returns the user ID of the owner of this file.
    #[cfg(unix)]
    #[inline]
    pub fn uid(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.uid(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_uid,
        }
    }

    /// Returns the group ID of the owner of this file.
    #[cfg(unix)]
    #[inline]
    pub fn gid(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.gid(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_gid,
        }
    }

    /// Returns the device ID of this file (if it is a special one).
    #[cfg(unix)]
    #[inline]
    pub fn rdev(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.rdev(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => libc::makedev(st.stx_rdev_major, st.stx_rdev_minor) as u64,
        }
    }

    /// Returns the total size of this file in bytes.
    #[cfg(unix)]
    #[inline]
    pub fn size(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.size(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_size,
        }
    }

    /// Returns the last access time of the file, in seconds since Unix Epoch.
    #[cfg(unix)]
    #[inline]
    pub fn atime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.atime(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_atime.tv_sec,
        }
    }

    /// Returns the last access time of the file, in nanoseconds since `atime`.
    #[cfg(unix)]
    #[inline]
    pub fn atime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.atime_nsec(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_atime.tv_nsec as i64,
        }
    }

    /// Returns the last modification time of the file, in seconds since Unix Epoch.
    #[cfg(unix)]
    #[inline]
    pub fn mtime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.mtime(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mtime.tv_sec,
        }
    }

    /// Returns the last modification time of the file, in nanoseconds since `mtime`.
    #[cfg(unix)]
    #[inline]
    pub fn mtime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.mtime_nsec(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_mtime.tv_nsec as i64,
        }
    }

    /// Returns the last status change time of the file, in seconds since Unix Epoch.
    #[cfg(unix)]
    #[inline]
    pub fn ctime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.ctime(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ctime.tv_sec,
        }
    }

    /// Returns the last status change time of the file, in nanoseconds since `ctime`.
    #[cfg(unix)]
    #[inline]
    pub fn ctime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.ctime_nsec(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_ctime.tv_nsec as i64,
        }
    }

    /// Returns the block size for filesystem I/O.
    #[cfg(unix)]
    #[inline]
    pub fn blksize(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.blksize(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_blksize as u64,
        }
    }

    /// Returns the number of blocks allocated to the file, in 512-byte units.
    #[cfg(unix)]
    #[inline]
    pub fn blocks(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.blocks(),
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_blocks,
        }
    }

    /// Returns the device ID on which this file resides.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_dev(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_dev(),
        }
    }

    /// Returns the inode number.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_ino(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_ino(),
        }
    }

    /// Returns the file type and mode.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_mode(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_mode(),
        }
    }

    /// Returns the number of hard links to file.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_nlink(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_nlink(),
        }
    }

    /// Returns the user ID of the file owner.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_uid(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_uid(),
        }
    }

    /// Returns the group ID of the file owner.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_gid(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_gid(),
        }
    }

    /// Returns the device ID that this file represents (if it is a special one).
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_rdev(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_rdev(),
        }
    }

    /// Returns the size of the file (if it is a regular file or a symbolic link) in bytes.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_size(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_size(),
        }
    }

    /// Returns the last access time of the file, in seconds since Unix Epoch.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_atime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_atime(),
        }
    }

    /// Returns the last access time of the file, in nanoseconds since `st_atime`.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_atime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_atime_nsec(),
        }
    }

    /// Returns the last modification time of the file, in seconds since Unix Epoch.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_mtime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_mtime(),
        }
    }

    /// Returns the last modification time of the file, in nanoseconds since `st_mtime`.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_mtime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_mtime_nsec(),
        }
    }

    /// Returns the last status change time of the file, in seconds since Unix Epoch.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_ctime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_ctime(),
        }
    }

    /// Returns the last status change time of the file, in nanoseconds since `st_ctime`.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_ctime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_ctime_nsec(),
        }
    }

    /// Returns the birth time of the file, in seconds since Unix Epoch.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_birthtime(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_birthtime(),
        }
    }

    /// Returns the birth time of the file, in nanoseconds since `st_birthtime`.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_birthtime_nsec(&self) -> i64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_birthtime_nsec(),
        }
    }

    /// Returns the block size for filesystem I/O.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_blksize(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_blksize(),
        }
    }

    /// Returns the number of blocks allocated to the file, in 512-byte units.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_blocks(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_blocks(),
        }
    }

    /// Returns the generation number of the file.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_gen(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_gen(),
        }
    }

    /// Returns the file flags.
    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux"))))]
    #[inline]
    pub fn st_flags(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.st_flags(),
        }
    }

    /// Returns the value of the `dwFileAttributes` field of this metadata.
    #[cfg(windows)]
    #[inline]
    pub fn file_attributes(&self) -> u32 {
        match &self.inner {
            MetadataInner::Std(md) => md.file_attributes(),
        }
    }

    /// Returns the value of the `ftCreationTime` field of this metadata.
    ///
    /// **Note:** Windows uses FILETIME (100-nanosecond intervals since Jan 1, 1601).
    /// This is converted to a Unix timestamp.
    #[cfg(windows)]
    #[inline]
    pub fn creation_time(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.creation_time(),
        }
    }

    /// Returns the value of the `ftLastAccessTime` field of this metadata.
    ///
    /// **Note:** Windows uses FILETIME (100-nanosecond intervals since Jan 1, 1601).
    /// This is converted to a Unix timestamp.
    #[cfg(windows)]
    #[inline]
    pub fn last_access_time(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.last_access_time(),
        }
    }

    /// Returns the value of the `ftLastWriteTime` field of this metadata.
    ///
    /// **Note:** Windows uses FILETIME (100-nanosecond intervals since Jan 1, 1601).
    /// This is converted to a Unix timestamp.
    #[cfg(windows)]
    #[inline]
    pub fn last_write_time(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.last_write_time(),
        }
    }

    /// Returns the value of the `nFileSize` fields of this metadata.
    #[cfg(windows)]
    #[inline]
    pub fn file_size(&self) -> u64 {
        match &self.inner {
            MetadataInner::Std(md) => md.file_size(),
        }
    }
}

/// Converts a `libc::statx_timestamp` to a `SystemTime`.
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
#[inline]
fn statx_timestamp_to_system_time(ts: &libc::statx_timestamp) -> io::Result<SystemTime> {
    let secs = ts.tv_sec;
    let nanos = ts.tv_nsec;

    if secs >= 0 {
        Ok(UNIX_EPOCH + Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new((-secs) as u64, nanos))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))
    }
}

/// A structure representing a type of file with accessors for each file type.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs;
///
/// let metadata = fs::metadata("hello.txt").await?;
/// let file_type = metadata.file_type();
///
/// if file_type.is_file() {
///     println!("It's a file!");
/// } else if file_type.is_dir() {
///     println!("It's a directory!");
/// }
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    /// Test whether this file type represents a directory.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("my_dir").await?;
    /// if metadata.file_type().is_dir() {
    ///     println!("It's a directory!");
    /// }
    /// ```
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Test whether this file type represents a regular file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("hello.txt").await?;
    /// if metadata.file_type().is_file() {
    ///     println!("It's a file!");
    /// }
    /// ```
    #[inline]
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Test whether this file type represents a symbolic link.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("link_to_file").await?;
    /// if metadata.file_type().is_symlink() {
    ///     println!("It's a symlink!");
    /// }
    /// ```
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}
