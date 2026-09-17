// SPDX-License-Identifier: GPL-2.0-or-later
//! Block allocation map manager (dmap).
//!
//! Mirrors `jfs_dmap.c` / `jfs_dmap.h` — the buddy allocator for filesystem blocks.
//! Manages allocation group (AG) descriptor pages, per-page allocation maps,
//! and the hierarchical bitmap/delta tree.

use crate::storage::{Result as StorageResult, Storage};
use crate::types::{BlockLength, BlockNo, Pxd};

/// Block allocation map control structure.
pub struct BlockAllocMap {
    /// Storage backend.
    storage: Box<dyn Storage>,
    /// Block size in bytes.
    block_size: u32,
    /// Log2 of block size.
    l2bsize: u16,
    /// Allocation group size in blocks.
    ag_size: u32,
    /// Number of allocation groups.
    num_ag: u32,
}

impl BlockAllocMap {
    pub fn new(
        storage: Box<dyn Storage>,
        block_size: u32,
        l2bsize: u16,
        ag_size: u32,
        num_ag: u32,
    ) -> Self {
        Self {
            storage,
            block_size,
            l2bsize,
            ag_size,
            num_ag,
        }
    }

    /// Find the AG for a given block number.
    pub fn ag_for_block(&self, block: BlockNo) -> u32 {
        (block / self.ag_size as u64) as u32
    }

    /// Allocate a contiguous extent of blocks.
    pub fn alloc_extent(
        &mut self,
        nblocks: BlockLength,
        hint: BlockNo,
    ) -> StorageResult<Option<Pxd>> {
        // Simplified single-AG allocation
        let ag = self.ag_for_block(hint);
        // In a full implementation, this would walk the buddy tree.
        // For read-only initial port, this returns None.
        let _ = ag;
        Ok(None)
    }

    /// Free a block extent.
    pub fn free_extent(&mut self, pxd: &Pxd) -> StorageResult<()> {
        let _ = pxd;
        Ok(())
    }
}
