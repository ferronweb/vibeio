use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// File metadata information.
///
/// This type mirrors a subset of [`std::fs::Metadata`]. On supported Linux
/// targets we back it by `statx` (via io_uring), otherwise we delegate to
/// `std::fs::metadata` on a blocking thread.
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
    #[inline]
    pub(crate) fn from_std(md: std::fs::Metadata) -> Self {
        Self {
            inner: MetadataInner::Std(md),
        }
    }

    #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
    #[inline]
    pub(crate) fn from_statx(st: libc::statx) -> Self {
        Self {
            inner: MetadataInner::Statx(st),
        }
    }

    #[inline]
    pub fn len(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_size,
            MetadataInner::Std(md) => md.len(),
        }
    }

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

    #[inline]
    pub fn file_type(&self) -> FileType {
        FileType {
            is_dir: self.is_dir(),
            is_file: self.is_file(),
            is_symlink: self.is_symlink(),
        }
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => {
                (st.stx_mode as u32 & libc::S_IFMT as u32) == libc::S_IFDIR as u32
            }
            MetadataInner::Std(md) => md.is_dir(),
        }
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => {
                (st.stx_mode as u32 & libc::S_IFMT as u32) == libc::S_IFREG as u32
            }
            MetadataInner::Std(md) => md.is_file(),
        }
    }

    #[inline]
    pub fn is_symlink(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => {
                (st.stx_mode as u32 & libc::S_IFMT as u32) == libc::S_IFLNK as u32
            }
            MetadataInner::Std(md) => md.file_type().is_symlink(),
        }
    }

    #[inline]
    pub fn accessed(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_atime),
            MetadataInner::Std(md) => md.accessed(),
        }
    }

    #[inline]
    pub fn created(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_btime),
            MetadataInner::Std(md) => md.created(),
        }
    }

    #[inline]
    pub fn modified(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_mtime),
            MetadataInner::Std(md) => md.modified(),
        }
    }
}

#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
#[inline]
fn statx_timestamp_to_system_time(ts: &libc::statx_timestamp) -> io::Result<SystemTime> {
    let secs = ts.tv_sec as i64;
    let nanos = ts.tv_nsec as u32;

    if secs >= 0 {
        Ok(UNIX_EPOCH + Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new((-secs) as u64, nanos))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))
    }
}

/// A structure representing a type of file with accessors for each file type.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    /// Test whether this file type represents a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Test whether this file type represents a regular file.
    #[inline]
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Test whether this file type represents a symbolic link.
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}
