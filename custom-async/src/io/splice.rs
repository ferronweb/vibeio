use std::{
    mem::MaybeUninit,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
};

use mio::Interest;

use crate::{fd_inner::InnerRawHandle, io::AsInnerRawHandle, op::SpliceOp};

pub async fn splice<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    let from_handle = unsafe { BorrowedFd::borrow_raw(from.as_raw_fd()) };
    let to_handle = to.as_inner_raw_handle();

    let mut op = SpliceOp::new(from_handle, to_handle, len);
    let result = std::future::poll_fn(|cx| to_handle.poll_op(cx, &mut op)).await;
    result
}

pub async fn splice_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    let mut total = 0;
    while total < len {
        let n = splice(from, to, len - total).await?;
        if n == 0 {
            break;
        }
        total += n;
    }

    Ok(total)
}

pub async fn sendfile_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    // splice() requires at least one of the file descriptors to be a pipe.
    // Therefore, we need to create a pipe and use it as the destination.
    let mut fds: Box<[MaybeUninit<RawFd>]> = Box::new_uninit_slice(2);
    let fds = unsafe {
        libc::pipe(fds.as_mut_ptr().cast::<RawFd>());
        fds.assume_init()
    };
    let pipe_reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let pipe_writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // We only need to poll the pipe writer for writability.
    let pipe_writer_handle = InnerRawHandle::new(pipe_writer.as_raw_fd(), Interest::WRITABLE)?;
    if !pipe_writer_handle.uses_completion() {
        // Set the pipe write side to non-blocking mode.
        let flags = unsafe { libc::fcntl(pipe_writer.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            unsafe {
                libc::fcntl(
                    pipe_writer.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            };
        }
    }

    let mut total_from_file = 0;
    let mut total_to_socket = 0;
    let mut file_eof = false;
    while (total_from_file < len && !file_eof) || total_to_socket < total_from_file {
        let splice_from_file_len = len - total_from_file;
        if !file_eof && splice_from_file_len > 0 {
            let n = splice(from, &pipe_writer_handle, len - total_from_file).await?;
            if n == 0 {
                file_eof = true;
            } else {
                total_from_file += n;
            }
        }

        let splice_to_socket_len = total_from_file - total_to_socket;
        if splice_to_socket_len > 0 {
            let n = splice(&pipe_reader, to, total_from_file - total_to_socket).await?;
            if n == 0 {
                break;
            }
            total_to_socket += n;
        }
    }

    Ok(total_to_socket)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, Write};
    use std::mem::{ManuallyDrop, MaybeUninit};
    use std::os::fd::{IntoRawFd, OwnedFd, RawFd};
    use std::os::unix::io::FromRawFd;

    use super::*;
    use crate::{fd_inner::InnerRawHandle, io::AsInnerRawHandle, RuntimeBuilder};

    const TEST_DATA_LEN: usize = 1024 * 1024; // 1MB

    struct PipeWriter {
        handle: ManuallyDrop<InnerRawHandle>,
    }

    impl PipeWriter {
        fn new(fd: i32) -> Self {
            let handle =
                ManuallyDrop::new(InnerRawHandle::new(fd, mio::Interest::WRITABLE).unwrap());
            Self { handle }
        }
    }

    impl<'a> AsInnerRawHandle<'a> for PipeWriter {
        fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
            &self.handle
        }
    }

    fn create_test_file() -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .prefix("custom-async-splice-test")
            .tempfile()
            .unwrap();
        let data: Vec<u8> = (0..TEST_DATA_LEN).map(|i| (i % 256) as u8).collect();
        file.write_all(&data).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file
    }

    fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds: Box<[MaybeUninit<RawFd>]> = Box::new_uninit_slice(2);
        let fds = unsafe {
            libc::pipe(fds.as_mut_ptr().cast::<RawFd>());
            fds.assume_init()
        };
        Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
            OwnedFd::from_raw_fd(fds[1])
        }))
    }

    #[test]
    fn test_splice_exact() {
        let runtime = RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let test_file = create_test_file();
            let (r, w) = pipe().unwrap();

            let reader_task = std::thread::spawn(move || {
                let mut reader = unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) };
                let mut received_data = vec![0u8; TEST_DATA_LEN];
                reader.read_exact(&mut received_data).unwrap();
                received_data
            });

            let writer = PipeWriter::new(w.as_raw_fd());
            let n = splice_exact(&test_file, &writer, TEST_DATA_LEN)
                .await
                .unwrap();

            // Close the writer to signal EOF to the reader.
            drop(writer);

            let received_data = reader_task.join().unwrap();

            assert_eq!(n, TEST_DATA_LEN);
            assert_eq!(received_data.len(), TEST_DATA_LEN);

            let original_data: Vec<u8> = (0..TEST_DATA_LEN).map(|i| (i % 256) as u8).collect();
            assert_eq!(received_data, original_data);
        });
    }

    #[test]
    fn test_sendfile_exact_full() {
        let runtime = RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let test_file = create_test_file();
            let (r, w) = pipe().unwrap();

            let reader_task = std::thread::spawn(move || {
                let mut reader = unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) };
                let mut received_data = vec![0u8; TEST_DATA_LEN];
                reader.read_exact(&mut received_data).unwrap();
                received_data
            });

            let writer = PipeWriter::new(w.as_raw_fd());
            let n = sendfile_exact(&test_file, &writer, TEST_DATA_LEN)
                .await
                .unwrap();

            // Close the writer to signal EOF to the reader.
            drop(writer);

            let received_data = reader_task.join().unwrap();

            assert_eq!(n, TEST_DATA_LEN);
            assert_eq!(received_data.len(), TEST_DATA_LEN);

            let original_data: Vec<u8> = (0..TEST_DATA_LEN).map(|i| (i % 256) as u8).collect();
            assert_eq!(received_data, original_data);
        });
    }

    #[test]
    fn test_sendfile_exact_zero_length() {
        let runtime = RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let test_file = create_test_file();
            let (r, w) = pipe().unwrap();

            let reader_task = std::thread::spawn(move || {
                let mut reader = unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) };
                let mut received_data = vec![0u8; 0]; // No data expected
                reader.read_exact(&mut received_data).unwrap();
                received_data
            });

            let writer = PipeWriter::new(w.as_raw_fd());
            let n = sendfile_exact(&test_file, &writer, 0).await.unwrap();

            // Close the writer to signal EOF to the reader.
            drop(writer);

            let received_data = reader_task.join().unwrap();

            assert_eq!(n, 0);
            assert_eq!(received_data.len(), 0);
        });
    }

    #[test]
    fn test_sendfile_exact_early_eof() {
        let runtime = RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let mut temp_file = tempfile::Builder::new()
                .prefix("custom-async-splice-test")
                .tempfile()
                .unwrap();
            let short_data_len = TEST_DATA_LEN / 2;
            let data: Vec<u8> = (0..short_data_len).map(|i| (i % 256) as u8).collect();
            temp_file.write_all(&data).unwrap();
            temp_file.seek(std::io::SeekFrom::Start(0)).unwrap();

            let (r, w) = pipe().unwrap();

            let reader_task = std::thread::spawn(move || {
                let mut reader = unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) };
                let mut received_data = vec![0u8; short_data_len];
                reader.read_exact(&mut received_data).unwrap();
                received_data
            });

            let writer = PipeWriter::new(w.as_raw_fd());
            let n = sendfile_exact(&temp_file, &writer, TEST_DATA_LEN)
                .await
                .unwrap();

            // Close the writer to signal EOF to the reader.
            drop(writer);

            let received_data = reader_task.join().unwrap();

            assert_eq!(n, short_data_len);
            assert_eq!(received_data.len(), short_data_len);

            let original_data: Vec<u8> = (0..short_data_len).map(|i| (i % 256) as u8).collect();
            assert_eq!(received_data, original_data);
        });
    }
}
