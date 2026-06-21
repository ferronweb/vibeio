# `vibeio` change log

## `vibeio` UNRELEASED

**Not yet released**

- Added `AsyncReadPoll` and `AsyncWritePoll` traits for poll-based async read/write readiness interfaces.
- Added `PollUdpSocket` struct for poll-based UDP socket readiness interfaces.
- Added `time` Cargo feature to `vibeio-hyper` that can be optionally disabled.
- Added `try_io_readable` and `try_io_writable` methods to `PollUdpSocket`, `PollTcpStream`, and `PollUnixStream`.
- Initial version of `vibeio-quinn` crate for poll-based QUIC socket readiness interfaces.
- `vibeio-hyper` now no longer depends on all the Cargo features of `vibeio`.

## `vibeio` 0.2.13

**Released in June 12, 2026**

- Added `TcpListener::from_std_poll` method for Windows.
- Fixed accepting TCP sockets for 32-bit Windows failing out with "unsupported socket family" error ([GitHub issue](https://github.com/ferronweb/ferron/issues/662))

## `vibeio` 0.2.12

**Released in May 27, 2026**

- Fixed a crash when mio (epoll, kqueue, poll) operation was interrupted.
- Performed several timer and executor optimizations

## `vibeio` 0.2.11

**Released in May 17, 2026**

- Fixed file descriptors not being freed when `io_uring` is used and there are pending operations on Linux 5.19+

## `vibeio` 0.2.10

**Released in May 2, 2026**

- Fixed busy looping related to timing events causing high CPU usage

## `vibeio` 0.2.9

**Released in April 27, 2026**

- Fixed an inconsistency on Windows when reading a file that has reached EOF

## `vibeio` 0.2.8

**Released in April 13, 2026**

- Added `fs::symlink_metadata` utility function

## `vibeio` 0.2.7

**Released in April 2, 2026**

- Fixed some panics related to integer underflow in the timer
- Fixed bugs related to dangling buffer pointers for stack-allocated buffers

## `vibeio` 0.2.6

**Released in March 24, 2026**

- Dropped the `tm-wheel` dependency in favor of a custom implementation

## `vibeio` 0.2.5

**Released in March 19, 2026**

- Performed some performance optimizations

## `vibeio` 0.2.4

**Released in March 19, 2026**

- Fixed compilation errors for 32-bit Windows targets

## `vibeio` 0.2.3

**Released in March 18, 2026**

- Fixed some panics when dropping the runtime with timer structs

## `vibeio` 0.2.2

**Released in March 17, 2026**

- Fixed some compilation errors on Linux targets with musl libc
 
## `vibeio` 0.2.1

**Released in March 17, 2026**

- Improved sendfile_exact correctness
 
## `vibeio` 0.2.0

**Released in March 17, 2026**

- Added support for cancelling `JoinHandle`s
- `sendfile_exact` and `splice_exact` functions now use `u64` for lengths instead of `usize`

## `vibeio` 0.1.0

**Released in March 14, 2026**

- First release
