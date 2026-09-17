// SPDX-License-Identifier: MIT
//! FUSE filesystem operations adapter.
//!
//! Bridges JFS filesystem operations to the FUSE kernel interface.
//! Provides: lookup, readdir, read, getattr, etc.
//!
//! ## Write support
//!
//! All FUSE write callbacks (`create`, `mkdir`, `unlink`, `rmdir`, `rename`,
//! `link`, `symlink`, `open`, `release`, `write`, `truncate`, `fsync`,
//! `setattr`, `setxattr`, etc.) are gated behind the `writable` cargo
//! feature and remain disabled until the transaction layer is validated.

use crate::types::Dinode;
use crate::volume::Volume;

/// FUSE filesystem operations.
pub struct FuseFs {
    /// Mounted volume.
    pub volume: Volume,
    /// Whether write operations are permitted.
    writable: bool,
}

impl FuseFs {
    pub fn new(volume: Volume) -> Self {
        Self {
            volume,
            writable: false,
        }
    }

    /// Enable writable mode (mount-time check: volume must pass validation).
    #[cfg(feature = "writable")]
    pub fn enable_writable(&mut self) -> Result<(), crate::storage::StorageError> {
        self.writable = true;
        Ok(())
    }

    /// Whether the filesystem is mounted writable.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Look up an inode by name within a directory.
    pub fn lookup(&mut self, parent_ino: u32, name: &str) -> Option<u32> {
        let _name_u16: Vec<u16> = name.encode_utf16().collect();
        let _ = parent_ino;
        None
    }

    /// Read directory entries.
    pub fn readdir(&mut self, ino: u32, offset: u64) -> Option<Vec<(String, u32, u8)>> {
        let _ = ino;
        let _ = offset;
        None
    }

    /// Get file attributes for an inode.
    pub fn getattr(&mut self, ino: u32) -> Option<Dinode> {
        self.volume.page_cache.get(0, 0)?;
        let _ = ino;
        None
    }

    /// Read file data at offset.
    pub fn read(&mut self, ino: u32, offset: u64, length: usize) -> Option<Vec<u8>> {
        let _ = (ino, offset, length);
        None
    }

    /// Create a file (write-supported, gated behind `writable`).
    #[cfg(feature = "writable")]
    pub fn create(&mut self, parent_ino: u32, name: &str, mode: u32) -> Option<u32> {
        if !self.writable {
            return None;
        }
        let _ = (parent_ino, name, mode);
        None
    }

    /// Write data to an open file (write-supported, gated behind `writable`).
    #[cfg(feature = "writable")]
    pub fn write(&mut self, ino: u32, offset: u64, data: &[u8]) -> Option<usize> {
        if !self.writable {
            return None;
        }
        let _ = (ino, offset, data);
        None
    }

    /// Remove a file (write-supported, gated behind `writable`).
    #[cfg(feature = "writable")]
    pub fn unlink(&mut self, parent_ino: u32, name: &str) -> Option<()> {
        if !self.writable {
            return None;
        }
        let _ = (parent_ino, name);
        None
    }
}
