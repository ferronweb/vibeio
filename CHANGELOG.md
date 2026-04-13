# `vibeio` change log

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
