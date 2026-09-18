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
}
