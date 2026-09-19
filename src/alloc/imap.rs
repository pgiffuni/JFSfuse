// SPDX-License-Identifier: GPL-2.0-or-later
//! Inode allocation map manager (imap).
//!
//! Mirrors the kernel JFS imap code — the IAG (Inode Allocation Group)
//! buddy allocator for inode numbers.
//!
//! Each IAG manages 4096 inodes (128 extents of 32 inodes each).
//! Allocation uses summary bitmaps for O(1) free-inode lookup.

use crate::storage::{Result as StorageResult, Storage};
use crate::types::{BlockNo, Pxd};

/// IAG (Inode Allocation Group) control structure.
pub struct InodeAllocMap {
    storage: Box<dyn Storage>,
    block_size: u32,
}

impl InodeAllocMap {
    pub fn new(storage: Box<dyn Storage>, block_size: u32) -> Self {
        Self {
            storage,
            block_size,
        }
    }

    /// Compute the IAG number for a given inode number.
    pub fn ino_to_iag(ino: u32) -> u32 {
        ino >> 12
    }

    /// Get the block address of an IAG page.
    pub fn iag_block(&self, iag_num: u32) -> StorageResult<BlockNo> {
        // Inodes are laid out in the aggregate inode table
        let bytes_per_ino = 128;
        let block = (AGGR_INODE_TABLE_START + (iag_num as u64) * 4096) / (self.block_size as u64);
        Ok(block)
    }

    /// Read an IAG page from disk.
    pub fn read_iag(&self, iag_num: u32) -> StorageResult<Vec<u8>> {
        let block = self.iag_block(iag_num)?;
        self.storage.read_block(block)
    }
}

use crate::types::AGGR_INODE_TABLE_START;
