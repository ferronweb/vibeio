use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::path::Path;

use crate::executor::current_driver;
use crate::op::Op;

#[cfg(target_os = "linux")]
use crate::op::OpenOp;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(windows)]
use crate::fd_inner::RawOsHandle;

use crate::fs::file::File;

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    #[inline]
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    #[inline]
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    #[inline]
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    #[inline]
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    #[inline]
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    #[inline]
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    #[inline]
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    #[inline]
    fn validate(&self) -> io::Result<()> {
        let writing = self.write || self.append;

        if !self.read && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "must enable read, write, or append access",
            ));
        }
        if (self.truncate || self.create || self.create_new) && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "truncate/create options require write or append access",
            ));
        }

        Ok(())
    }

    #[inline]
    fn initial_cursor_for_append(&self, file: &std::fs::File) -> io::Result<u64> {
        if self.append {
            Ok(file.metadata()?.len())
        } else {
            Ok(0)
        }
    }

    #[inline]
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        self.validate()?;
        let path = path.as_ref();

        let std_file = if let Some(driver) = current_driver() {
            #[cfg(target_os = "linux")]
            {
                if driver.supports_completion() {
                    let mut op = self.build_open_op(path)?;
                    let raw = poll_fn(move |cx| op.poll(cx, &driver)).await?;
                    unsafe { std::fs::File::from_raw_fd(raw) }
                } else {
                    self.open_in_blocking_pool(path).await?
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                let _ = driver;
                self.open_in_blocking_pool(path).await?
            }
        } else {
            self.open_blocking(path)?
        };

        let cursor = self.initial_cursor_for_append(&std_file)?;
        File::from_std_with_cursor(std_file, cursor)
    }

    #[inline]
    async fn open_in_blocking_pool(&self, path: &Path) -> io::Result<std::fs::File> {
        let path = path.to_path_buf();
        let read = self.read;
        let write = self.write;
        let append = self.append;
        let truncate = self.truncate;
        let create = self.create;
        let create_new = self.create_new;

        crate::spawn_blocking(move || {
            #[cfg(windows)]
            use std::os::windows::fs::OpenOptionsExt;
            #[cfg(windows)]
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

            let mut options = std::fs::OpenOptions::new();
            options
                .read(read)
                .write(write)
                .append(append)
                .truncate(truncate)
                .create(create)
                .create_new(create_new);
            #[cfg(windows)]
            options.attributes(FILE_FLAG_OVERLAPPED);
            options.open(path)
        })
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }

    #[inline]
    fn open_blocking(&self, path: &Path) -> io::Result<std::fs::File> {
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt;
        #[cfg(windows)]
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .truncate(self.truncate)
            .create(self.create)
            .create_new(self.create_new);
        #[cfg(windows)]
        options.attributes(FILE_FLAG_OVERLAPPED);
        options.open(path)
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_open_op(&self, path: &Path) -> io::Result<OpenOp> {
        let writing = self.write || self.append;
        let mut flags = match (self.read, writing) {
            (true, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
            (false, false) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "must enable read, write, or append access",
                ))
            }
        };

        if self.append {
            flags |= libc::O_APPEND;
        }
        if self.truncate {
            flags |= libc::O_TRUNC;
        }
        if self.create_new {
            flags |= libc::O_CREAT | libc::O_EXCL;
        } else if self.create {
            flags |= libc::O_CREAT;
        }
        flags |= libc::O_CLOEXEC;

        let path_bytes = path.as_os_str().as_bytes();
        if path_bytes.contains(&0) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ));
        }

        let path = CString::new(path_bytes).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "path contains interior NUL byte")
        })?;

        Ok(OpenOp::new(path, flags, 0o666))
    }
}

impl Default for OpenOptions {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
