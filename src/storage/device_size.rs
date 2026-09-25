// SPDX-License-Identifier: BSD-2-Clause
//! Platform-specific device size detection.
//!
//! On Unix systems, block/character special devices report `st_size = 0`
//! via `stat()`. We use platform-specific ioctls to get the real capacity:
//!
//! | Platform | Device type | ioctl                 |
//! |----------|-------------|-----------------------|
//! | Linux    | block dev   | `BLKGETSIZE64`        |
//! | FreeBSD  | char device | `DIOCGMEDIASIZE`      |
//!
//! Regular files always use `metadata().len()`.
//!
//! On FreeBSD, `DIOCGMEDIASIZE` and `DIOCGSECTORSIZE` are not exposed by
//! the Rust `libc` crate, so we use the raw ioctl numbers (matching
//! FreeBSD's `_IOR('d', 128, off_t)` and `_IOR('d', 129, u_int)`).

use std::fs::File;
use std::os::fd::AsRawFd;

/// FreeBSD `DIOCGMEDIASIZE` ioctl number: `_IOR('d', 129, off_t)`
/// Encoded as: (IOC_READ << 30) | ('d' << 8) | 129 | (sizeof(off_t) << 16)
#[cfg(target_os = "freebsd")]
const DIOCGMEDIASIZE: u64 = 0x8008_6481;

#[cfg(target_os = "freebsd")]
fn device_size(file: &File) -> Result<u64, std::io::Error> {
    let mut size: libc::off_t = 0;
    let ret = unsafe {
        libc::ioctl(file.as_raw_fd(), DIOCGMEDIASIZE, &mut size)
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(size as u64)
}

/// Linux `BLKGETSIZE64` ioctl number.
const BLKGETSIZE64: u64 = 0x8008_1272;

#[cfg(target_os = "linux")]
fn device_size(file: &File) -> Result<u64, std::io::Error> {
    let mut size: u64 = 0;
    let ret = unsafe {
        libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size as *mut u64)
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(size)
}

/// Determine the size in bytes of a storage backing file or device.
///
/// Regular files use `metadata().len()`; special devices (block devices on
/// Linux, character devices on FreeBSD) use platform-specific ioctls.
pub fn get_storage_size(file: &File) -> Result<u64, crate::storage::StorageError> {
    let meta = file.metadata()?;
    let ft = meta.file_type();

    if ft.is_file() {
        return Ok(meta.len());
    }

    // Not a regular file — assume it's a device and use the platform ioctl.
    device_size(file).map_err(crate::storage::StorageError::Io)
}
