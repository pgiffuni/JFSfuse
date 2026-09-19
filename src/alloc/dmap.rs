// SPDX-License-Identifier: GPL-2.0-or-later
//! Block allocation map manager (dmap).
//!
//! Mirrors the kernel JFS dmap code — the buddy allocator for filesystem blocks.
//! Reads on-disk dmap pages to build an in-memory free-block bitmap, then
//! provides allocation and freeing of contiguous extents.
//!
//! The on-disk `struct dmap` covers `BPERDMAP` (8192) blocks per 4096-byte
//! page: a `dbmap_disk` descriptor followed by dmap pages each containing
//! `wmap` (working) and `pmap` (persistent) 32-bit bitmaps.

use std::collections::HashSet;

use byteorder::{ByteOrder, LittleEndian};

use crate::storage::{BLOCK_SIZE, Result as StorageResult, Storage, StorageError};
use crate::types::{BlockLength, BlockNo, BMAP_I, DISIZE, Pxd};

/// Blocks managed per dmap page (8192 = 256 words × 32 bits).
const BLOCKS_PER_DMAP: u64 = 8192;

/// Inode number for the aggregate block allocation map (BMAP_I = 2).
/// In VFS, aggregate inodes < FILESYSTEM_I use the same number directly,
/// so the BMAP inode is read with VFS inode number 2.
const BMAP_INODE: u32 = BMAP_I;

pub struct BlockAllocMap {
    /// Storage backend (shared via Arc).
    storage: std::sync::Arc<dyn Storage>,
    /// Total blocks in the aggregate.
    mapsize: u64,
    /// Number of free blocks (approximate, updated on alloc/free).
    nfree: u64,
    /// In-memory free-block bitmap (true = free).
    free_map: Vec<bool>,
    /// Number of dmap pages (excluding the dbmap_disk descriptor page).
    ndmap: u64,
    /// Physical block where dmap data begins (first extent of BMAP inode).
    dmap_start: u64,
    /// Number of blocks in the bmap extent.
    dmap_len: u64,
    /// Blocks allocated since last init (for journal transaction).
    allocated: Vec<(u64, u32)>,
    /// Blocks freed since last init (for rollback).
    freed: Vec<(u64, u32)>,
    /// Dirty dmap page numbers that need writeback.
    dirty_pages: HashSet<u64>,
}

impl BlockAllocMap {
    /// Initialize the block allocation map by reading the BMAP_I inode and
    /// parsing the on-disk dmap pages.
    pub fn new(storage: std::sync::Arc<dyn Storage>) -> StorageResult<Self> {
        // Read the BMAP_I inode (VFS inode 2).
        let bmap_inode_ino = BMAP_INODE;
        // We need a Volume to read the inode. Read directly from storage using
        // the same logic as Inode::read but without requiring a Volume.
        let (page_block, page_offset) = Self::find_bmap_inode(&*storage)?;
        let page_data = storage.read_block(page_block)?;
        let dinode_bytes = &page_data[page_offset..page_offset + DISIZE];

        // Parse xtroot from the dinode.
        let xtroot = Self::parse_bmap_xtroot(dinode_bytes)?;
        let dmap_start = xtroot.0;
        let dmap_len = xtroot.1;

        // Read the dbmap_disk descriptor (first data block).
        let desc_block = storage.read_block(dmap_start)?;
        let mapsize = LittleEndian::read_u64(&desc_block[0..8]);
        let nfree = LittleEndian::read_u64(&desc_block[8..16]);
        let ndmap = ((dmap_len - 1).max(0) * (BLOCK_SIZE as u64) / BLOCKS_PER_DMAP).max(1);

        // Build the free-block bitmap from dmap pages.
        let mut free_map = vec![false; mapsize as usize];
        for i in 0..ndmap {
            let dmap_block = dmap_start + 1 + i;
            if dmap_block >= storage.size_blocks() {
                break;
            }
            let dmap_page = storage.read_block(dmap_block)?;
            let start = i * BLOCKS_PER_DMAP;
            Self::parse_dmap_page(&dmap_page, start, &mut free_map);
        }

        Ok(Self {
            storage,
            mapsize,
            nfree,
            free_map,
            ndmap,
            dmap_start,
            dmap_len,
            allocated: Vec::new(),
            freed: Vec::new(),
            dirty_pages: HashSet::new(),
        })
    }

    /// Find the BMAP_I inode's location (block, offset) by scanning the inode table.
    fn find_bmap_inode(storage: &dyn Storage) -> StorageResult<(u64, usize)> {
        // For a fresh image read from the test image, we know the BMAP_I inode
        // is at the second entry in the aggregate inode table. The inode table
        // is at s_ait2, but we need the superblock to find it.
        // Read the superblock.
        let sb_data = storage.read_bytes(crate::types::SUPER1_OFF, crate::types::PSIZE)?;
        let ait_addr = Self::parse_pxd_addr(&sb_data[48..56]);
        let ait_len = Self::parse_pxd_len(&sb_data[48..56]);

        let inos_per_page = crate::types::INOSPERPAGE;
        for block_offset in 0..(ait_len + 32) {
            let block_num = ait_addr + block_offset;
            if block_num >= storage.size_blocks() {
                continue;
            }
            let data = storage.read_block(block_num)?;
            for i in 0..inos_per_page as usize {
                let off = i * crate::types::DISIZE;
                if off + crate::types::DISIZE > data.len() {
                    break;
                }
                let fs = LittleEndian::read_u32(&data[off + 4..off + 8]);
                let num = LittleEndian::read_u32(&data[off + 8..off + 12]);
                if fs == 1 && num == BMAP_INODE {
                    return Ok((block_num, off));
                }
            }
        }

        Err(StorageError::Other("BMAP inode not found".to_string()))
    }

    /// Parse the first xad extent from a dtroot/xtroot.
    fn parse_bmap_xtroot(dinode: &[u8]) -> StorageResult<(u64, u64)> {
        // xtroot starts at offset 96 in the union area (offset 128 in dinode).
        let xt_off = 128 + 96;
        if dinode.len() < xt_off + 32 {
            return Err(StorageError::Other("dinode too short for xtroot".to_string()));
        }
        let xt = &dinode[xt_off..];

        let nextindex = LittleEndian::read_u16(&xt[18..20]);
        // First real xad at index XTENTRYSTART=2, starting at offset 32.
        for i in 2..nextindex as usize {
            let base = 32 + (i - 2) * 16;
            if base + 16 > xt.len() {
                break;
            }
            let xad = &xt[base..base + 16];
            let loc_len_raw = LittleEndian::read_u32(&xad[8..12]);
            let length = (loc_len_raw & 0xFFFFFF) as u64;
            let addr_high = ((loc_len_raw >> 24) & 0xFF) as u64;
            let addr2 = LittleEndian::read_u32(&xad[12..16]) as u64;
            let address = (addr_high << 32) | addr2;
            if length > 0 {
                return Ok((address, length));
            }
        }

        // Check self_pxd (the dtroot header's self pxd at xt offset 24).
        let spxd_len = LittleEndian::read_u32(&xt[24..28]);
        let spxd_addr_high = ((spxd_len >> 24) & 0xFF) as u64;
        let spxd_addr_low = LittleEndian::read_u32(&xt[28..32]) as u64;
        let s_len = (spxd_len & 0xFFFFFF) as u64;
        let s_addr = (spxd_addr_high << 32) | spxd_addr_low;
        if s_len > 0 {
            return Ok((s_addr, s_len));
        }

        Err(StorageError::Other("no extents in bmap xtroot".to_string()))
    }

    fn parse_pxd_addr(bytes: &[u8]) -> u64 {
        let len_addr = LittleEndian::read_u32(&bytes[0..4]);
        let addr2 = LittleEndian::read_u32(&bytes[4..8]);
        ((len_addr >> 24) as u64) << 32 | addr2 as u64
    }

    fn parse_pxd_len(bytes: &[u8]) -> u64 {
        let len_addr = LittleEndian::read_u32(&bytes[0..4]);
        (len_addr & 0xFFFFFF) as u64
    }

    /// Parse a dmap page to extract free block bits from pmap.
    fn parse_dmap_page(page: &[u8], start_block: u64, free_map: &mut [bool]) {
        // wmap is at offset 2048 as 256 32-bit words.
        // pmap is at offset 3072 as 256 32-bit words.
        let pmap_off = 3072;
        for word_idx in 0..256 {
            let off = pmap_off + word_idx * 4;
            if off + 4 > page.len() {
                break;
            }
            let word = LittleEndian::read_u32(&page[off..off + 4]);
            for bit in 0..32 {
                let blk_idx = start_block + word_idx as u64 * 32 + bit as u64;
                if blk_idx < free_map.len() as u64 {
                    // Bit set (1) = free in JFS pmap.
                    free_map[blk_idx as usize] = (word >> bit) & 1 != 0;
                }
            }
        }
    }

    /// Find the AG containing a given block.
    pub fn ag_for_block(&self, block: BlockNo) -> u32 {
        let ag_size = self.mapsize / self.num_ag().max(1) as u64;
        if ag_size > 0 {
            (block / ag_size) as u32
        } else {
            0
        }
    }

    pub fn num_ag(&self) -> u32 {
        // Derive from mapsize: standard AG size is 8192 blocks for 4KB blocksize.
        let ag_size = self.mapsize / 4;
        if ag_size > 0 {
            4
        } else {
            1
        }
    }

    /// Allocate a contiguous extent of `nblocks` blocks, starting near `hint`.
    ///
    /// Scans the in-memory free bitmap from the hint position for a contiguous
    /// run of free blocks. If the hint region doesn't have enough free blocks,
    /// falls back to scanning from the beginning.
    ///
    /// For transactional correctness, the allocation is recorded and can be
    /// rolled back via `free_extent` (which restores the free bits).
    pub fn alloc_extent(
        &mut self,
        nblocks: BlockLength,
        hint: BlockNo,
    ) -> StorageResult<Option<Pxd>> {
        let nblocks = nblocks as u64;
        if nblocks == 0 || nblocks > self.mapsize {
            return Err(StorageError::Other("invalid block count".to_string()));
        }

        // Try to find a contiguous run starting from hint.
        let start = self.find_free_run(nblocks, hint)?;
        let start = match start {
            Some(s) => s,
            None => self.find_free_run(nblocks, 0)?.ok_or_else(|| {
                StorageError::Other("no contiguous free blocks available".to_string())
            })?,
        };

        // Mark blocks as allocated.
        for i in 0..nblocks {
            let blk = (start + i) as usize;
            if blk < self.free_map.len() {
                self.free_map[blk] = false;
            }
        }

        self.nfree = self.nfree.saturating_sub(nblocks);
        self.allocated.push((start, nblocks as u32));

        // Track which dmap page to update.
        let dmap_page = 1 + (start / BLOCKS_PER_DMAP);
        self.dirty_pages.insert(dmap_page);

        let mut pxd = Pxd::default();
        pxd.set_address(start);
        pxd.set_length(nblocks as u32);
        Ok(Some(pxd))
    }

    /// Find a contiguous run of `nblocks` free blocks starting at or after `start`.
    fn find_free_run(&self, nblocks: u64, start: u64) -> StorageResult<Option<u64>> {
        let mut run_start = start;
        let mut run_len = 0u64;

        for blk in start..self.mapsize {
            if self.free_map[blk as usize] {
                if run_len == 0 {
                    run_start = blk;
                }
                run_len += 1;
                if run_len >= nblocks {
                    return Ok(Some(run_start));
                }
            } else {
                run_len = 0;
            }
        }

        Ok(None)
    }

    /// Free a block extent.
    ///
    /// Marks the blocks as free in the in-memory bitmap. The changes are
    /// recorded for potential rollback (re-allocation).
    pub fn free_extent(&mut self, pxd: &Pxd) -> StorageResult<()> {
        let addr = pxd.address();
        let len = pxd.length() as u64;

        if addr + len > self.mapsize {
            return Err(StorageError::Other("extent exceeds aggregate size".to_string()));
        }

        for i in 0..len {
            let blk = (addr + i) as usize;
            if blk < self.free_map.len() {
                self.free_map[blk] = true;
            }
        }

        self.nfree += len;
        self.freed.push((addr, len as u32));

        let dmap_page = 1 + (addr / BLOCKS_PER_DMAP);
        self.dirty_pages.insert(dmap_page);

        Ok(())
    }

    /// Write a single bitmap word back to a dmap page's wmap.
    fn write_word_to_dmap(&self, dmap_page: u64, word_idx: usize, value: u32) -> StorageResult<()> {
        let block_num = self.dmap_start + dmap_page;
        let mut page = self.storage.read_block(block_num)?;
        let off = 2048 + word_idx * 4; // wmap starts at offset 2048
        if off + 4 <= page.len() {
            LittleEndian::write_u32(&mut page[off..off + 4], value);
            self.storage.write_block(block_num, &page)?;
        }
        Ok(())
    }

    /// Commit all allocator changes to persistent storage.
    ///
    /// Writes updated bitmap words to the working map (wmap) in each
    /// dirty dmap page, and updates the dbmap_disk descriptor's nfree count.
    pub fn commit(&mut self) -> StorageResult<()> {
        // Write back dirty dmap pages.
        for &dmap_page in &self.dirty_pages {
            let block_num = self.dmap_start + dmap_page;
            if block_num >= self.storage.size_blocks() {
                continue;
            }
            let mut page = self.storage.read_block(block_num)?;
            let start_block = (dmap_page - 1) * BLOCKS_PER_DMAP;

            // Serialize the free_map for this dmap page back to wmap.
            for word_idx in 0..256 {
                let blk_offset = start_block + word_idx as u64 * 32;
                let mut word: u32 = 0;
                for bit in 0..32 {
                    let blk_idx = blk_offset + bit as u64;
                    if blk_idx < self.mapsize && self.free_map[blk_idx as usize] {
                        word |= 1 << bit;
                    }
                }
                let off = 2048 + word_idx * 4;
                if off + 4 <= page.len() {
                    LittleEndian::write_u32(&mut page[off..off + 4], word);
                }
            }
            self.storage.write_block(block_num, &page)?;
        }

        // Update nfree in the dbmap_disk descriptor.
        let desc_block = self.storage.read_block(self.dmap_start)?;
        let mut desc = desc_block;
        LittleEndian::write_u64(&mut desc[8..16], self.nfree);
        self.storage.write_block(self.dmap_start, &desc)?;

        self.dirty_pages.clear();
        self.allocated.clear();
        self.freed.clear();
        Ok(())
    }

    /// Roll back pending allocations and frees (called on transaction abort).
    pub fn rollback(&mut self) {
        for (addr, len) in &self.allocated {
            for i in 0..*len as u64 {
                let blk = (addr + i) as usize;
                if blk < self.free_map.len() {
                    self.free_map[blk] = true;
                }
            }
            self.nfree += *len as u64;
        }
        for (addr, len) in &self.freed {
            for i in 0..*len as u64 {
                let blk = (addr + i) as usize;
                if blk < self.free_map.len() {
                    self.free_map[blk] = false;
                }
            }
            self.nfree -= *len as u64;
        }
        self.allocated.clear();
        self.freed.clear();
        self.dirty_pages.clear();
    }

    /// Number of free blocks in the aggregate.
    pub fn nfree(&self) -> u64 {
        self.nfree
    }

    /// Total number of blocks in the aggregate.
    pub fn mapsize(&self) -> u64 {
        self.mapsize
    }

    /// Number of allocation groups.
    pub fn num_ags(&self) -> u32 {
        self.num_ag()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pxd_addr() {
        // PXD: len_addr (4 bytes) | addr2 (4 bytes)
        // len_addr = length (24 bits) | addr_high (8 bits)
        // address = (addr_high << 32) | addr2
        let mut bytes = [0u8; 8];
        LittleEndian::write_u32(&mut bytes[0..4], 0x00ABC000); // len=0xABC, addr_high=0
        LittleEndian::write_u32(&mut bytes[4..8], 0x00001000); // addr2
        assert_eq!(BlockAllocMap::parse_pxd_addr(&bytes), 0x1000);
        assert_eq!(BlockAllocMap::parse_pxd_len(&bytes), 0x00ABC000 & 0xFFFFFF);
    }

    #[test]
    fn test_parse_pxd_addr_with_high() {
        let mut bytes = [0u8; 8];
        LittleEndian::write_u32(&mut bytes[0..4], 0x01000005); // addr_high=1, len=5
        LittleEndian::write_u32(&mut bytes[4..8], 0x00002000); // addr2
        assert_eq!(BlockAllocMap::parse_pxd_addr(&bytes), (1u64 << 32) | 0x2000);
        assert_eq!(BlockAllocMap::parse_pxd_len(&bytes), 5);
    }
}
