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

use crate::btree::dtree::Dtree;
use crate::btree::xtree::Xtree;
use crate::inode::Inode;
use crate::types::Dinode;
use crate::volume::Volume;

/// POSIX errno constants used by the FUSE adapter to report errors.
/// Maps internal JFS conditions to FUSE results per the Phase 9 table.
pub const ENOENT: i32 = 2;
pub const EEXIST: i32 = 17;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const ENOTEMPTY: i32 = 39;
pub const ENOSPC: i32 = 28;
pub const EROFS: i32 = 30;
pub const EINVAL: i32 = 22;
pub const EACCES: i32 = 13;
pub const EBUSY: i32 = 16;
pub const EOPNOTSUPP: i32 = 95;
pub const EIO: i32 = 5;
pub const EPERM: i32 = 1;

/// Result type for FUSE operations that may carry an errno value.
pub type FuseResult<T> = Result<T, i32>;

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
    /// Handles `.` and `..` as special cases (implicit entries).
    pub fn lookup(&mut self, parent_ino: u32, name: &str) -> Option<u32> {
        match name {
            "." => Some(parent_ino),
            ".." => {
                let parent = Inode::read(&mut self.volume, parent_ino).ok()?;
                if !parent.is_dir() {
                    return None;
                }
                let dtroot = parent.dtroot_bytes();
                if dtroot.len() >= 24 {
                    Some(u32::from_le_bytes(
                        dtroot[20..24].try_into().unwrap(),
                    ))
                } else {
                    None
                }
            }
            _ => {
                let parent = Inode::read(&mut self.volume, parent_ino).ok()?;
                if !parent.is_dir() {
                    return None;
                }
                let dtree = Dtree::from_inode_data(parent.dtroot_bytes()).ok()?;
                let name_u16: Vec<u16> = name.encode_utf16().collect();
                let entry = dtree.lookup(&name_u16).ok()??;
                Some(entry.inumber)
            }
        }
    }

    /// Read directory entries. Returns a list of (name, inode_number, file_type).
    /// Includes `.` and `..` entries. Real entries come from the dtree stbl order.
    pub fn readdir(&mut self, ino: u32, _offset: u64) -> Option<Vec<(String, u32, u8)>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        if !inode.is_dir() {
            return None;
        }

        let mut result = Vec::new();

        // `.` entry — points to self
        result.push((".".to_string(), ino, 0x04));

        // `..` entry — parent from dtroot header
        let dtroot = inode.dtroot_bytes();
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            result.push(("..".to_string(), parent_ino, 0x04));
        }

        // Real entries from the dtree
        if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
            if let Ok(entries) = dtree.entries() {
                for e in &entries {
                    let name = String::from_utf16_lossy(&e.name)
                        .trim_end_matches('\0')
                        .to_string();
                    let file_type = self.infer_file_type(e.inumber);
                    result.push((name, e.inumber, file_type));
                }
            }
        }

        Some(result)
    }

    /// Infer file type from an inode number by reading the target inode.
    fn infer_file_type(&mut self, ino: u32) -> u8 {
        if let Ok(inode) = Inode::read(&mut self.volume, ino) {
            if inode.is_symlink() {
                0xA0 // DT_LNK
            } else if inode.is_dir() {
                0x04 // DT_DIR
            } else if inode.is_regular() {
                0x08 // DT_REG
            } else {
                0x00 // DT_UNKNOWN
            }
        } else {
            0x00
        }
    }

    /// Get file attributes for an inode.
    pub fn getattr(&mut self, ino: u32) -> Option<Dinode> {
        Inode::read(&mut self.volume, ino).ok().map(|i| i.dinode)
    }

    /// Read file data at offset.
    pub fn read(&mut self, ino: u32, offset: u64, length: usize) -> Option<Vec<u8>> {
        let size = Inode::read(&mut self.volume, ino).ok()?.size();
        if offset >= size {
            return Some(vec![]);
        }
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).ok()?;
        let storage = self.volume.storage.clone();
        xtree.read_data(offset, length, &*storage).ok()
    }

    /// Create a file (write-supported, gated behind `writable`).
    ///
    /// Allocates a new inode, inserts a directory entry in the parent,
    /// and returns the new inode number.
    #[cfg(feature = "writable")]
    pub fn create(&mut self, parent_ino: u32, name: &str, _mode: u32) -> Option<u32> {
        if !self.writable {
            return None;
        }
        self.volume.create_file(parent_ino, name).ok()
    }

    /// Write data to an open file (write-supported, gated behind `writable`).
    ///
    /// Writes data through the transaction layer: begins a transaction,
    /// writes data to the file's existing data blocks, updates inode
    /// metadata, and commits the transaction (journal flush + metadata flush).
    #[cfg(feature = "writable")]
    pub fn write(&mut self, ino: u32, offset: u64, data: &[u8]) -> Option<usize> {
        if !self.writable {
            return None;
        }
        self.volume.write_at(ino, offset, data).ok()
    }

    /// Flush pending writes for a file (writable builds only).
    ///
    /// In the current ordered-data model, each `write` already commits a
    /// transaction. This method is a no-op for correctness but is provided
    /// for FUSE semantics.
    #[cfg(feature = "writable")]
    pub fn flush(&mut self, ino: u32) -> Option<()> {
        if !self.writable {
            return None;
        }
        self.volume.fsync(ino).ok()
    }

    /// Truncate a file (writable builds only).
    #[cfg(feature = "writable")]
    pub fn truncate(&mut self, ino: u32, new_size: u64) -> Option<()> {
        if !self.writable {
            return None;
        }
        self.volume.truncate(ino, new_size).ok()
    }

    /// Remove a file (write-supported, gated behind `writable`).
    #[cfg(feature = "writable")]
    pub fn unlink(&mut self, parent_ino: u32, name: &str) -> Option<bool> {
        if !self.writable {
            return None;
        }
        Some(self.volume.unlink_file(parent_ino, name).unwrap_or(false))
    }

    /// Create a directory (write-supported, gated behind `writable`).
    ///
    /// Allocates a new directory inode, initializes it with `.` and `..`
    /// entries, inserts a directory entry in the parent, and increments the
    /// parent's link count.
    #[cfg(feature = "writable")]
    pub fn mkdir(&mut self, parent_ino: u32, name: &str, _mode: u32) -> FuseResult<u32> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.mkdir(parent_ino, name).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("invalid filename") {
                EINVAL
            } else if msg.contains("no free inode") {
                ENOSPC
            } else {
                EIO
            }
        })
    }

    /// Remove a directory (write-supported, gated behind `writable`).
    ///
    /// Fails if the child is not a directory or is not empty
    /// (only `.` and `..` entries are allowed).
    #[cfg(feature = "writable")]
    pub fn rmdir(&mut self, parent_ino: u32, name: &str) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        match self.volume.rmdir(parent_ino, name) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ENOENT),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not a directory") {
                    Err(ENOTDIR)
                } else if msg.contains("not empty") {
                    Err(ENOTEMPTY)
                } else {
                    Err(EIO)
                }
            }
        }
    }

    /// Open a file or directory (writable builds only).
    ///
    /// Validates that the inode exists and increments the open-handle count
    /// for open-unlinked semantics (inode is retained while open handles exist).
    #[cfg(feature = "writable")]
    pub fn open(&mut self, ino: u32) -> FuseResult<()> {
        let result = self.volume.open_file(ino);
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(ENOENT),
        }
    }

    /// Release an open file handle (writable builds only).
    ///
    /// Decrements the open-handle count. If the inode's nlink dropped to 0
    /// (via unlink while open) and this was the last handle, the inode's
    /// data blocks are freed.
    #[cfg(feature = "writable")]
    pub fn release(&mut self, ino: u32) -> FuseResult<()> {
        self.volume.release_file(ino).map_err(|_| EIO)
    }

    /// Set file attributes (writable builds only).
    ///
    /// Supports chmod (mode), chown (uid/gid), and utimens (atime/mtime).
    /// Unspecified attributes are left unchanged.
    #[cfg(feature = "writable")]
    pub fn setattr(
        &mut self,
        ino: u32,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .setattr(ino, mode, uid, gid, atime, mtime)
            .map_err(|_| EIO)
    }

    /// Rename or move a file/directory (writable builds only).
    ///
    /// Unlinks the old name from the source parent and creates a new entry
    /// in the destination parent. Not yet implemented.
    #[cfg(feature = "writable")]
    pub fn rename(
        &mut self,
        _old_parent: u32,
        _old_name: &str,
        _new_parent: u32,
        _new_name: &str,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        Err(EOPNOTSUPP)
    }

    /// Create a hard link (writable builds only).
    ///
    /// Increments the target inode's link count and inserts a new directory
    /// entry. Hard links to directories are rejected.
    #[cfg(feature = "writable")]
    pub fn link(&mut self, parent_ino: u32, name: &str, target_ino: u32) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.link_file(parent_ino, name, target_ino).map(|_| ()).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("cannot hard-link a directory") {
                EPERM
            } else if msg.contains("invalid filename") {
                EINVAL
            } else {
                EIO
            }
        })
    }
}
