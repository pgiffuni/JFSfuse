// SPDX-License-Identifier: BSD-2-Clause
//! Portable storage geometry abstraction.
//!
//! Replaces the Linux kernel's `struct block_device` + `bdev_logical_blksz` /
//! `bdev_getgeo` with a single `StorageGeometry` struct that captures both
//! the *device capacity* (media size in bytes) and the *sector size* needed
//! for properly aligned I/O.
//!
//! Platform detection is performed via ioctls so that opening a block device
//! (`/dev/sda`, `/dev/loop0`, etc.) on Linux or a character device
//! (`/dev/ada0`, `/dev/nmdm0`, etc.) on FreeBSD yields the correct size even
//! when `stat()` reports `st_size == 0`.

use crate::storage::StorageError;
use std::fs::File;
use std::os::fd::AsRawFd;

/// Device geometry: capacity and sector size.
///
/// `media_size` is the total capacity of the underlying device in bytes.
/// `sector_size` is the logical block size of the device (typically 512 on
/// older hardware, 4096 on newer). The filesystem block size (`BLOCK_SIZE`,
/// always 4096 for JFS) must be a multiple of `sector_size` for correct I/O
/// alignment.
#[derive(Clone, Debug)]
pub struct StorageGeometry {
    /// Total device capacity in bytes.
    pub media_size: u64,
    /// Number of sectors.
    pub num_sectors: u64,
    /// Logical sector size in bytes (typically 512 or 4096).
    pub sector_size: u32,
}

impl StorageGeometry {
    /// Detect geometry from an open file or device handle.
    ///
    /// * Regular files: `stat().st_size` is used for both capacity and sector
    ///   size (sector size defaults to 512 for files, since alignment is less
    ///   critical and the filesystem block size dominates).
    /// * Block devices (Linux): `BLKGETSIZE64` for capacity, `BLKSSZGET` for
    ///   sector size.
    /// * Character devices (FreeBSD, macOS): `DIOCGMEDIASIZE` for capacity,
    ///   `DIOCGSECTORSIZE` for sector size.
    /// * Fallback: if the ioctl fails, fall back to `stat().st_size` and
    ///   a default sector size of 512.
    pub fn from_file(file: &File) -> Result<Self, StorageError> {
        use std::os::unix::fs::FileTypeExt;
        let meta = file.metadata()?;
        let ft = meta.file_type();

        if ft.is_file() {
            return Ok(Self {
                media_size: meta.len(),
                num_sectors: meta.len() / 512,
                sector_size: 512,
            });
        }

        // Attempt platform-specific device detection.
        Self::from_device(file).or_else(|e| {
            log::warn!(
                "device geometry detection failed ({e}); falling back to stat()"
            );
            Ok(Self {
                media_size: meta.len(),
                num_sectors: if meta.len() > 0 { meta.len() / 512 } else { 0 },
                sector_size: 512,
            })
        })
    }

    /// Number of filesystem blocks (each `BLOCK_SIZE` bytes).
    pub fn num_fs_blocks(&self) -> u64 {
        self.media_size / (crate::types::PSIZE as u64)
    }

    /// Return the geometry as `(total_bytes, block_size, sector_size)` for
    /// downstream validation.
    pub fn as_parts(&self) -> (u64, u32, u32) {
        (self.media_size, crate::types::PSIZE as u32, self.sector_size)
    }

    /// Validate that a sector size is a reasonable power of two within [1, 4096].
    pub fn is_valid_sector_size(sector_size: u32) -> bool {
        sector_size != 0
            && sector_size.is_power_of_two()
            && sector_size <= 4096
    }

    /// Validate that the filesystem block size is a multiple of the device
    /// sector size, ensuring proper I/O alignment.
    pub fn sector_size_compatible(&self) -> bool {
        crate::types::PSIZE as u32 % self.sector_size == 0
    }

    /// Validate that the device geometry is compatible with a JFS superblock.
    ///
    /// Checks:
    /// 1. Sector size is a valid power of two.
    /// 2. Filesystem block size is a multiple of sector size.
    /// 3. Device is large enough to hold the superblock at `SUPER1_OFF`.
    /// 4. Device has enough blocks for the filesystem's declared size.
    pub fn validate_against_superblock(
        &self,
        sb: &crate::types::JfsSuperblock,
    ) -> Result<(), StorageError> {
        if !Self::is_valid_sector_size(self.sector_size) {
            return Err(StorageError::Other(format!(
                "invalid sector size: {}",
                self.sector_size
            )));
        }
        if !self.sector_size_compatible() {
            return Err(StorageError::Other(format!(
                "filesystem block size {} is not a multiple of sector size {}",
                crate::types::PSIZE, self.sector_size
            )));
        }
        if self.media_size < crate::types::SUPER1_OFF + (crate::types::PSIZE as u64) {
            return Err(StorageError::Other(
                "device too small to hold JFS superblock".to_string(),
            ));
        }
        let fs_declared_size = sb.aggregate_size() * (sb.block_size() as u64);
        if self.media_size < fs_declared_size {
            return Err(StorageError::Other(format!(
                "device media size {} is smaller than filesystem declared size {}",
                self.media_size, fs_declared_size
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Platform-specific device geometry detection
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod sys {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    // ioctl number for BLKGETSIZE64
    const BLKGETSIZE64: u64 = 0x80081272;
    // ioctl number for BLKSSZGET
    const BLKSSZGET: u64 = 0x80041272;

    pub fn from_device(file: &File) -> Result<StorageGeometry, StorageError> {
        let ft = file.metadata()?.file_type();

        // On Linux, block devices should be used with BLKGETSIZE64/BLKSSZGET.
        if ft.is_block_device() {
            let mut size: u64 = 0;
            let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size) };
            if ret < 0 {
                return Err(StorageError::Io(std::io::Error::last_os_error()));
            }

            let mut sector_size: u32 = 512;
            let ret = unsafe {
                libc::ioctl(file.as_raw_fd(), BLKSSZGET, &mut sector_size)
            };
            if ret < 0 {
                log::warn!("BLKSSZGET failed; assuming sector size 512");
                sector_size = 512;
            }

            let sector_size = sector_size.max(1);
            return Ok(StorageGeometry {
                media_size: size,
                num_sectors: size / sector_size as u64,
                sector_size,
            });
        }

        // Character devices: try using stat size.
        let meta = file.metadata()?;
        let size = if ft.is_char_device() {
            // Try BLKGETSIZE64 on char devices too (e.g., /dev/loop-control)
            let mut size_val: u64 = 0;
            let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size_val) };
            if ret == 0 && size_val > 0 {
                size_val
            } else {
                // Fall back to fd 0 or just use file size
                meta.len()
            }
        } else {
            meta.len()
        };

        Ok(StorageGeometry {
            media_size: size,
            num_sectors: if size > 0 { size / 512 } else { 0 },
            sector_size: 512,
        })
    }
}

#[cfg(target_os = "freebsd")]
mod sys {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    /// FreeBSD `DIOCGMEDIASIZE` ioctl: `_IOR('d', 128, off_t)`
    /// = (IOC_READ << 30) | ('d' << 8) | 128 | (8 << 16) = 0x80086480
    const DIOCGMEDIASIZE: u64 = 0x8008_6480;

    /// FreeBSD `DIOCGSECTORSIZE` ioctl: `_IOR('d', 129, u_int)`
    /// = (IOC_READ << 30) | ('d' << 8) | 129 | (4 << 16) = 0x80046481
    const DIOCGSECTORSIZE: u64 = 0x8004_6481;

    pub fn from_device(file: &File) -> Result<StorageGeometry, StorageError> {
        let ft = file.metadata()?.file_type();

        if ft.is_char_device() {
            let mut size: libc::off_t = 0;
            let ret = unsafe {
                libc::ioctl(file.as_raw_fd(), DIOCGMEDIASIZE, &mut size)
            };
            if ret < 0 {
                return Err(StorageError::Io(std::io::Error::last_os_error()));
            }

            let mut sector_size: u32 = 512;
            let ret = unsafe {
                libc::ioctl(file.as_raw_fd(), DIOCGSECTORSIZE, &mut sector_size)
            };
            if ret < 0 {
                log::warn!("DIOCGSECTORSIZE failed; assuming sector size 512");
                sector_size = 512;
            }

            let sector_size = sector_size.max(1);
            return Ok(StorageGeometry {
                media_size: size as u64,
                num_sectors: (size as u64) / sector_size as u64,
                sector_size,
            });
        }

        // Block devices on FreeBSD (e.g., mdconfig) — fall back to stat.
        let meta = file.metadata()?;
        Ok(StorageGeometry {
            media_size: meta.len(),
            num_sectors: if meta.len() > 0 { meta.len() / 512 } else { 0 },
            sector_size: 512,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
mod sys {
    use super::*;

    pub fn from_device(file: &File) -> Result<StorageGeometry, StorageError> {
        let meta = file.metadata()?;
        Ok(StorageGeometry {
            media_size: meta.len(),
            num_sectors: if meta.len() > 0 { meta.len() / 512 } else { 0 },
            sector_size: 512,
        })
    }
}

// Re-export for `StorageGeometry::from_device`.
impl StorageGeometry {
    fn from_device(file: &File) -> Result<Self, StorageError> {
        sys::from_device(file)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geometry_from_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("geom_test");
        let data = vec![0u8; 4096 * 16];
        std::fs::write(&path, &data).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap();
        let geom = StorageGeometry::from_file(&file).unwrap();
        assert_eq!(geom.media_size, 4096 * 16);
        assert_eq!(geom.sector_size, 512);
        assert_eq!(geom.num_fs_blocks(), 16);
    }

    #[test]
    fn test_geometry_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_test");
        std::fs::write(&path, &[]).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap();
        let geom = StorageGeometry::from_file(&file).unwrap();
        assert_eq!(geom.media_size, 0);
        assert_eq!(geom.num_sectors, 0);
        assert_eq!(geom.num_fs_blocks(), 0);
    }

    #[test]
    fn test_geometry_as_parts() {
        let geom = StorageGeometry {
            media_size: 4096 * 100,
            num_sectors: 100,
            sector_size: 512,
        };
        let (size, block_size, sector_size) = geom.as_parts();
        assert_eq!(size, 4096 * 100);
        assert_eq!(block_size, crate::types::PSIZE as u32);
        assert_eq!(sector_size, 512);
    }

    #[test]
    fn test_geometry_sector_size_power_of_two() {
        assert!(StorageGeometry::is_valid_sector_size(512));
        assert!(StorageGeometry::is_valid_sector_size(4096));
        assert!(!StorageGeometry::is_valid_sector_size(0));
        assert!(!StorageGeometry::is_valid_sector_size(7));
    }
}
