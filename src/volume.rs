// SPDX-License-Identifier: GPL-2.0-or-later
//! JFS volume management.
//!
//! Mirrors `jfs_mount.c` / `super.c` — superblock validation, inline log
//! detection, allocation map setup, and journal recovery orchestration.
//!
//! The mount flow:
//! 1. Read and validate the primary superblock (`chkSuper`)
//! 2. Read the log superblock (`logsuper`)
//! 3. If log state is not `LOGREDONE`, run journal recovery (`logredo`)
//! 4. Initialize block/inode allocation maps
//! 5. Open the root inode

use std::path::Path;
use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use crate::journal::{JournalRecovery, LogManager};
use crate::storage::{
    BLOCK_SIZE, FileStorage, PageCache, Result as StorageResult, Storage, StorageError,
};
use crate::transaction::{CommitResult, TransactionId, TransactionManager};
#[cfg(feature = "writable")]
use crate::alloc::dmap::BlockAllocMap;
use crate::types::{
    self, AGGREGATE_I, BMAP_I, FILESYSTEM_I, FM_DIRTY, FM_LOGREDO, JFS_MAGIC, JfsSuperblock, LOG_I,
    LOGMAGIC, LOGREDONE, LOGVERSION, PSIZE, ROOT_I, SUPER1_B, SUPER1_OFF,
};

/// A mounted JFS volume.
pub struct Volume {
    /// Filesystem superblock.
    pub sb: JfsSuperblock,
    /// Log manager (for journal I/O). Owned because the volume is the sole
    /// writer in the single-writer model.
    pub log: Option<LogManager>,
    /// Transaction manager — gateway for all journaled metadata writes.
    pub tx_mgr: TransactionManager,
    /// Filesystem block storage (the main device).
    pub storage: Arc<dyn Storage>,
    /// Log storage (may be separate device or inline).
    pub log_storage: Option<Arc<dyn Storage>>,
    /// Page cache for metadata (replaces metapage cache).
    pub page_cache: PageCache,
    /// Block size in bytes (4096 for modern JFS).
    pub block_size: u32,
    /// Log2 of block size.
    pub l2bsize: u16,
    /// Aggregate size in blocks.
    pub agg_size: u64,
    /// Allocation group size in blocks.
    pub ag_size: u32,
    /// Number of allocation groups.
    pub num_ag: u32,
    /// Root inode number.
    pub root_ino: u32,
    /// Block allocation map (writable builds only).
    #[cfg(feature = "writable")]
    pub bmap: Option<BlockAllocMap>,
}

impl Volume {
    /// Open and mount a JFS volume from a device/file.
    ///
    /// Reads the superblock, validates it, reads the journal,
    /// and runs recovery if needed — all before returning.
    pub fn open(path: &str) -> StorageResult<Self> {
        let storage = Arc::new(FileStorage::open(Path::new(path))?);
        Self::open_from_storage(storage)
    }

    pub fn open_from_storage(storage: Arc<dyn Storage>) -> StorageResult<Self> {
        let sb = Self::read_super(&*storage)?;
        Self::validate_super(&sb)?;

        let block_size = sb.block_size();
        if block_size != PSIZE as u32 {
            return Err(StorageError::UnsupportedBlockSize(block_size));
        }

        let l2bsize = sb.l2bsize();
        let agg_size = sb.aggregate_size();
        let ag_size = sb.agsize();
        let num_ag = if ag_size > 0 {
            (agg_size / (ag_size as u64)) as u32
        } else {
            1
        };

        log::info!(
            "JFS superblock: version={}, blocksize={}, agsize={}, ags={}",
            sb.version(),
            block_size,
            ag_size,
            num_ag
        );

        let log_storage = if sb.has_inline_log() {
            log::info!("inline log detected");
            Some(storage.clone())
        } else {
            None
        };

        let mut volume = Self {
            sb,
            log: None,
            tx_mgr: TransactionManager::new(),
            storage,
            log_storage,
            page_cache: PageCache::new(256),
            block_size,
            l2bsize,
            agg_size,
            ag_size,
            num_ag,
            root_ino: FILESYSTEM_I + ROOT_I,
            #[cfg(feature = "writable")]
            bmap: None,
        };

        volume.init_log()?;
        volume.recover_journal()?;
        #[cfg(feature = "writable")]
        {
            volume.bmap = Some(BlockAllocMap::new(volume.storage.clone())?);
        }

        Ok(volume)
    }

    /// Read the primary superblock from disk.
    ///
    /// In JFS, the superblock sits at byte offset SUPER1_OFF (0x8000),
    /// i.e. sector 64 (SUPER1_B) * PBSIZE (512). We use the byte offset
    /// directly rather than `SUPER1_B * BLOCK_SIZE` since SUPER1_B is
    /// expressed in 512-byte sectors, not 4 KiB blocks.
    fn read_super(storage: &dyn Storage) -> StorageResult<JfsSuperblock> {
        let offset = SUPER1_OFF;
        let data = storage.read_bytes(offset, PSIZE)?;

        let mut sb = JfsSuperblock::default();

        if data.len() >= 4 {
            sb.s_magic = data[0..4]
                .try_into()
                .map_err(|_| StorageError::InvalidSuperblock)?;
        }
        if data.len() >= 8 {
            sb.s_version = data[4..8].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 16 {
            sb.s_size = data[8..16].try_into().unwrap_or([0; 8]);
        }
        if data.len() >= 20 {
            sb.s_bsize = data[16..20].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 24 {
            sb.s_l2bsize = data[20..22].try_into().unwrap_or([0; 2]);
            sb.s_l2bfactor = data[22..24].try_into().unwrap_or([0; 2]);
        }
        if data.len() >= 32 {
            sb.s_pbsize = data[24..28].try_into().unwrap_or([0; 4]);
            sb.s_l2pbsize = data[28..30].try_into().unwrap_or([0; 2]);
            sb.pad = data[30..32].try_into().unwrap_or([0; 2]);
        }
        if data.len() >= 36 {
            sb.s_agsize = data[32..36].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 40 {
            sb.s_flag = data[36..40].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 44 {
            sb.s_state = data[40..44].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 48 {
            sb.s_compress = data[44..48].try_into().unwrap_or([0; 4]);
        }
        if data.len() >= 56 {
            sb.s_ait2 = types::Pxd::from_bytes(&data[48..56]);
        }
        if data.len() >= 64 {
            sb.s_aim2 = types::Pxd::from_bytes(&data[56..64]);
        }
        if data.len() >= 80 {
            sb.s_logpxd = types::Pxd::from_bytes(&data[72..80]);
        }

        Ok(sb)
    }

    /// Validate the superblock: magic, version.
    fn validate_super(sb: &JfsSuperblock) -> StorageResult<()> {
        if !sb.is_valid_magic() {
            return Err(StorageError::InvalidSuperblock);
        }
        if sb.version() < 1 || sb.version() > 2 {
            return Err(StorageError::Other(format!(
                "unsupported JFS version: {}",
                sb.version()
            )));
        }
        let state = sb.state();
        if state == FM_DIRTY || state == FM_LOGREDO {
            log::warn!("filesystem was not cleanly unmounted (state={:#x})", state);
        }
        Ok(())
    }

    /// Initialize the log manager from the log superblock.
    fn init_log(&mut self) -> StorageResult<()> {
        if let Some(ls) = &self.log_storage {
            // For inline logs, the log area is at the PXD address specified
            // in the filesystem superblock. For external logs, the log device
            // starts at offset 0.
            let log_base = if self.sb.has_inline_log() {
                (self.sb.inline_log_pxd().address() * (BLOCK_SIZE as u64)) as u64
            } else {
                0
            };

            let logsuper = LogManager::read_super(&**ls, log_base)?;
            if logsuper.magic_val() != LOGMAGIC {
                return Err(StorageError::Other(
                    "invalid log superblock magic".to_string(),
                ));
            }
            if logsuper.version() != LOGVERSION {
                return Err(StorageError::Other(format!(
                    "unsupported log version: {}",
                    logsuper.version()
                )));
            }
            let lm = LogManager::new(ls.clone(), logsuper, log_base);
            self.log = Some(lm);
        }
        Ok(())
    }

    /// Run journal recovery (logredo) if the log state is not LOGREDONE.
    fn recover_journal(&mut self) -> StorageResult<()> {
        let log = match &mut self.log {
            Some(l) => l,
            None => {
                log::info!("no log found; skipping journal recovery");
                return Ok(());
            }
        };

        let logsuper = log.logsuper().clone();
        if logsuper.state() == LOGREDONE {
            log::info!("journal already replayed");
            return Ok(());
        }

        if let Some(ls) = &self.log_storage {
            let mut recovery = JournalRecovery::new(logsuper);
            recovery.replay(&**ls, &*self.storage, log.data_start())?;

            let new_super = recovery.finalize();
            log.write_super(&new_super)?;
            log.sync()?;
        }

        Ok(())
    }

    /// Read a filesystem block.
    pub fn read_page(&self, block: u64) -> StorageResult<Vec<u8>> {
        self.storage.read_block(block)
    }

    /// Get the root inode.
    pub fn root_inode(&mut self) -> StorageResult<crate::inode::Inode> {
        crate::inode::Inode::read(self, self.root_ino)
    }

    /// Start a journaled write transaction (writable builds only).
    #[cfg(feature = "writable")]
    pub fn begin_transaction(&mut self) -> StorageResult<TransactionId> {
        self.tx_mgr.begin()
    }

    /// Commit the active transaction, flushing the journal then metadata
    /// and allocation map to stable storage (writable builds only).
    #[cfg(feature = "writable")]
    pub fn commit_transaction(&mut self) -> StorageResult<CommitResult> {
        let journal = self.log.as_mut();
        let result = self
            .tx_mgr
            .commit(&*self.storage, &mut self.page_cache, journal)?;

        // Flush the allocation map changes to disk.
        if let Some(bmap) = self.bmap.as_mut() {
            bmap.commit()?;
        }

        Ok(result)
    }

    /// Abort the active transaction and discard dirty pages (writable builds only).
    #[cfg(feature = "writable")]
    pub fn abort_transaction(&mut self) {
        if let Some(bmap) = self.bmap.as_mut() {
            bmap.rollback();
        }
        self.tx_mgr.abort(&mut self.page_cache)
    }

    /// Mark a cached page dirty under the current transaction (writable builds only).
    #[cfg(feature = "writable")]
    pub fn mark_page_dirty(&mut self, inode: u32, block: crate::types::BlockNo) -> StorageResult<()> {
        self.tx_mgr.mark_dirty(&mut self.page_cache, inode, block)
    }

    /// Write file data at the given offset (writable builds only).
    ///
    /// Implements the ordered-data write policy:
    /// 1. Begin transaction
    /// 2. Allocate new blocks for sparse regions (if needed)
    /// 3. Write data blocks to their physical locations
    /// 4. Flush data blocks
    /// 5. Update inode size and xtree metadata
    /// 6. Journal metadata (inode page) via TransactionManager
    /// 7. Commit transaction (journal flush + metadata flush)
    #[cfg(feature = "writable")]
    pub fn write_at(
        &mut self,
        ino: u32,
        offset: u64,
        data: &[u8],
    ) -> StorageResult<usize> {
        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;
        let size = inode.size();
        let mut xtree = crate::btree::xtree::Xtree::from_inode_data(inode.xtroot_bytes())?;
        let mut xtree_modified = false;

        let mut written = 0usize;

        // Write blocks within existing extents.
        let byte_offset = offset % (BLOCK_SIZE as u64);
        let fsb_offset = offset / (BLOCK_SIZE as u64);
        let fsb_count = ((data.len() as u64 + byte_offset + BLOCK_SIZE as u64 - 1)
            / (BLOCK_SIZE as u64)) as u32;

        let extents = xtree.map_blocks(fsb_offset, fsb_count as u64)?;

        let mut data_pos = 0usize;
        let mut remaining = data.len();
        let mut cur_byte_offset = byte_offset as usize;
        let mut next_logical = fsb_offset;

        for (block_addr, block_count) in extents {
            if remaining == 0 {
                break;
            }

            if block_addr == 0 {
                // Sparse region — allocate new blocks if the allocator is available.
                #[cfg(feature = "writable")]
                {
                    if let Some(bmap) = self.bmap.as_mut() {
                        let alloc_count: crate::types::BlockLength = block_count;
                        if let Some(pxd) = bmap.alloc_extent(alloc_count, next_logical as u64)? {
                            let new_addr = pxd.address();
                            let new_len = pxd.length() as u64;

                            // Write data to the newly allocated blocks.
                            let mut data_pos_in_hole = data_pos;
                            let mut remaining_in_hole = remaining;
                            let mut cur_offset = cur_byte_offset;
                            for blk in 0..new_len {
                                if remaining_in_hole == 0 {
                                    break;
                                }
                                let to_copy = std::cmp::min(
                                    BLOCK_SIZE - cur_offset,
                                    remaining_in_hole,
                                );
                                let mut block_data = vec![0u8; BLOCK_SIZE as usize];
                                if data_pos_in_hole + to_copy <= data.len() {
                                    block_data[cur_offset..cur_offset + to_copy]
                                        .copy_from_slice(&data[data_pos_in_hole
                                        ..data_pos_in_hole + to_copy]);
                                }
                                self.storage.write_block(new_addr + blk, &block_data)?;

                                written += to_copy;
                                data_pos_in_hole += to_copy;
                                remaining_in_hole -= to_copy;
                                cur_offset = 0;
                            }

                            // Insert the new extent into the xtree.
                            if xtree.insert_extent(
                                next_logical as i64,
                                new_len as u32,
                                new_addr,
                            ) {
                                xtree_modified = true;
                            }

                            next_logical += new_len;
                            data_pos = data_pos_in_hole;
                            remaining = remaining_in_hole;
                            continue;
                        }
                    }
                }
                // No allocator — skip (sparse region stays unwritten).
                let zeros_to_copy = std::cmp::min(
                    block_count as usize * BLOCK_SIZE - cur_byte_offset,
                    remaining,
                );
                written += zeros_to_copy;
                data_pos += zeros_to_copy;
                remaining -= zeros_to_copy;
                cur_byte_offset = 0;
                continue;
            }

            // Existing extent — write data directly to the physical blocks.
            for blk in 0..block_count as u64 {
                if remaining == 0 {
                    break;
                }
                let mut block_data = self.storage.read_block(block_addr + blk)?;
                let to_copy = std::cmp::min(BLOCK_SIZE - cur_byte_offset, remaining);
                block_data[cur_byte_offset..cur_byte_offset + to_copy]
                    .copy_from_slice(&data[data_pos..data_pos + to_copy]);
                self.storage.write_block(block_addr + blk, &block_data)?;

                written += to_copy;
                data_pos += to_copy;
                remaining -= to_copy;
                cur_byte_offset = 0;
            }

            next_logical += block_count as u64;
        }

        // Flush data blocks (ordered-data model).
        self.storage.flush_data()?;

        // Update inode size and xtree if modified.
        let new_end = offset + written as u64;
        let mut update_size = new_end > size;
        if update_size {
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                LittleEndian::write_u64(&mut dinode_bytes[24..32], new_end);
            })?;
        }
        let _ = &mut update_size;

        if xtree_modified {
            let xt_bytes = xtree.to_bytes();
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                let xt_off = crate::types::Dinode::size() - xt_bytes.len();
                dinode_bytes[xt_off..xt_off + xt_bytes.len()].copy_from_slice(&xt_bytes);
                // Update nblocks
                let total_blocks = xtree.iter_extents().map(|e| e.length as u64).sum::<u64>();
                LittleEndian::write_u64(&mut dinode_bytes[32..40], total_blocks);
            })?;
        }

        // Mark the inode's metadata page as dirty for journaling.
        self.mark_page_dirty(ino, inode.page_block)?;

        // Commit the transaction: journal metadata, flush, write metadata.
        let result = self.commit_transaction()?;
        let _ = result;

        Ok(written)
    }

    /// Truncate or extend a regular file to `new_size` (writable builds only).
    ///
    /// For shrinking: identifies extents beyond the new EOF, frees complete
    /// trailing extents, splits the final extent if necessary, and updates
    /// the xtree. The freed blocks are recorded (actual block free is pending
    /// allocator integration).
    ///
    /// For growing: adds no new extents (the new range becomes a hole);
    /// only the inode size is updated.
    #[cfg(feature = "writable")]
    pub fn truncate(&mut self, ino: u32, new_size: u64) -> StorageResult<()> {
        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;
        let mut xtree = crate::btree::xtree::Xtree::from_inode_data(inode.xtroot_bytes())?;

        let new_size_val = new_size;
        let new_eof_fsb = (new_size + (BLOCK_SIZE as u64) - 1) / (BLOCK_SIZE as u64);
        let current_eof_fsb = (inode.size() + (BLOCK_SIZE as u64) - 1) / (BLOCK_SIZE as u64);

        if new_eof_fsb < current_eof_fsb {
            // Shrinking: truncate extents.
            let freed = xtree.truncate_extents(new_eof_fsb as i64);
            // Record freed blocks (allocator is a stub — not actually freed).
            let _ = freed;

            // Update nblocks (number of blocks allocated to the file).
            let new_nblocks = self.compute_nblocks(&xtree, inode.dinode.nblocks())? as u64;
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                let xt_bytes = xtree.to_bytes();
                let xt_off = crate::types::Dinode::size() - xt_bytes.len();
                dinode_bytes[xt_off..xt_off + xt_bytes.len()].copy_from_slice(&xt_bytes);
                LittleEndian::write_u64(&mut dinode_bytes[24..32], new_size_val);
                LittleEndian::write_u64(&mut dinode_bytes[32..40], new_nblocks);
            })?;
        } else if new_eof_fsb > current_eof_fsb {
            // Growing: add a hole, just update size.
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                LittleEndian::write_u64(&mut dinode_bytes[24..32], new_size_val);
            })?;
        }

        self.mark_page_dirty(ino, inode.page_block)?;
        self.commit_transaction()?;
        Ok(())
    }

    /// Flush a file's data and metadata to durable storage (writable builds only).
    #[cfg(feature = "writable")]
    pub fn fsync(&mut self, ino: u32) -> StorageResult<()> {
        let _ = self.begin_transaction()?;
        let inode = crate::inode::Inode::read(self, ino)?;
        self.storage.flush_metadata()?;
        self.mark_page_dirty(ino, inode.page_block)?;
        self.commit_transaction()?;
        Ok(())
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn l2bsize(&self) -> u16 {
        self.l2bsize
    }

    pub fn num_ags(&self) -> u32 {
        self.num_ag
    }

    /// Apply a mutation to the cached inode page, then unpin it.
    ///
    /// The closure receives the dinode bytes within the page (starting at
    /// `page_offset` and spanning `Dinode::size()` bytes) and may modify them
    /// in place.
    #[cfg(feature = "writable")]
    fn update_inode_page(
        &mut self,
        ino: u32,
        page_block: u64,
        page_offset: usize,
        update: impl FnOnce(&mut [u8]),
    ) -> StorageResult<()> {
        let dinode_size = crate::types::Dinode::size();
        // Ensure the page is in the cache.
        let _ = self.page_cache.get_or_load(&*self.storage, ino, page_block, 0)?;
        let page = self
            .page_cache
            .get_mut_for_write(ino, page_block)
            .map_err(|_| crate::storage::StorageError::PageNotFound)?;
        let end = page_offset + dinode_size;
        if end <= page.data.len() {
            update(&mut page.data[page_offset..end]);
        }
        self.page_cache.unpin_page(ino, page_block);
        Ok(())
    }

    /// Compute the total number of blocks allocated to a file based on its
    /// xtree extents. Falls back to the inode's current nblocks if the
    /// xtree cannot be fully parsed.
    #[cfg(feature = "writable")]
    fn compute_nblocks(
        &self,
        xtree: &crate::btree::xtree::Xtree,
        fallback: u64,
    ) -> StorageResult<u64> {
        let total = xtree.iter_extents().map(|e| e.length as u64).sum::<u64>();
        if total > 0 {
            Ok(total)
        } else {
            Ok(fallback)
        }
    }

    /// Find the first free inode in the aggregate inode table.
    /// Returns (inode_number, page_block, page_offset) for the first dinode
    /// that has a zero mode (unused).
    #[cfg(feature = "writable")]
    fn allocate_inode(&mut self) -> StorageResult<(u32, u64, usize)> {
        let pxd = &self.sb.s_ait2;
        let table_start = pxd.address();
        let table_len = pxd.length() as u64;
        let max_scan = table_len + 32;

        for block_offset in 0..max_scan {
            let block_num = table_start + block_offset;
            if block_num >= self.agg_size {
                continue;
            }
            let data = self.storage.read_block(block_num)?;
            for i in 0..crate::types::INOSPERPAGE {
                let off = (i as usize) * crate::types::DISIZE;
                if off + crate::types::DISIZE > data.len() {
                    break;
                }
                let mode = LittleEndian::read_u32(&data[off + 52..off + 56]);
                if mode == 0 {
                    // Found a free inode. Its inode number is determined by
                    // its position in the table. We compute a virtual ino.
                    let ino = FILESYSTEM_I + (block_offset * crate::types::INOSPERPAGE as u64 + i as u64) as u32;
                    return Ok((ino, block_num, off));
                }
            }
        }

        Err(StorageError::Other("no free inode found".to_string()))
    }

    /// Insert a directory entry for `child_ino` with the given name in the
    /// directory at `parent_ino`. The parent's inode page is updated and
    /// marked dirty. Does NOT commit the transaction — the caller must do so.
    #[cfg(feature = "writable")]
    fn insert_dir_entry(
        &mut self,
        parent_ino: u32,
        name: &[u16],
        child_ino: u32,
        index: u32,
    ) -> StorageResult<bool> {
        let inode = crate::inode::Inode::read(self, parent_ino)?;
        let mut dtree = crate::btree::dtree::Dtree::from_inode_data(inode.dtroot_bytes())?;
        let inserted = dtree.insert(name, child_ino, index)?;

        if inserted {
            let dt_bytes = dtree.to_bytes().to_vec();
            self.update_inode_page(parent_ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                let dt_off = crate::types::Dinode::size() - dt_bytes.len();
                dinode_bytes[dt_off..dt_off + dt_bytes.len()].copy_from_slice(&dt_bytes);
            })?;
        }

        Ok(inserted)
    }

    /// Remove a directory entry by name from the directory at `parent_ino`.
    /// Returns the child inode number if found. Does NOT commit.
    #[cfg(feature = "writable")]
    fn remove_dir_entry(&mut self, parent_ino: u32, name: &[u16]) -> StorageResult<Option<u32>> {
        let inode = crate::inode::Inode::read(self, parent_ino)?;
        let mut dtree = crate::btree::dtree::Dtree::from_inode_data(inode.dtroot_bytes())?;
        let removed = dtree.remove(name)?;

        if removed.is_some() {
            let dt_bytes = dtree.to_bytes().to_vec();
            self.update_inode_page(parent_ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                let dt_off = crate::types::Dinode::size() - dt_bytes.len();
                dinode_bytes[dt_off..dt_off + dt_bytes.len()].copy_from_slice(&dt_bytes);
            })?;
        }

        Ok(removed)
    }

    /// Create a regular file in a directory.
    ///
    /// Follows the Phase 8 plan:
    /// 1. Allocate inode
    /// 2. Initialize inode (regular file, empty)
    /// 3. Insert directory entry
    /// 4. Update parent metadata (link count, timestamps)
    /// 5. Commit
    #[cfg(feature = "writable")]
    pub fn create_file(&mut self, parent_ino: u32, name: &str) -> StorageResult<u32> {
        let _ = self.begin_transaction()?;

        // 1. Allocate a new inode.
        let (child_ino, _child_block, _child_off) = self.allocate_inode()?;

        // 2. Initialize the new inode (zero-mode dinode is already free;
        //    write a minimal regular file dinode).
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        if name_u16.is_empty() || name_u16.len() > 11 {
            self.abort_transaction();
            return Err(StorageError::Other("invalid filename length".to_string()));
        }

        // Initialize the child inode: regular file mode (0x81a4), empty size.
        self.update_inode_page(child_ino, _child_block, _child_off, |dinode_bytes| {
            // Set mode to regular file (S_IFREG | 0644 = 0x81a4)
            LittleEndian::write_u32(&mut dinode_bytes[52..56], 0x81a4);
            // Set size to 0
            LittleEndian::write_u64(&mut dinode_bytes[24..32], 0);
            // Set nblocks to 0
            LittleEndian::write_u64(&mut dinode_bytes[32..40], 0);
            // Set nlink to 1
            LittleEndian::write_u32(&mut dinode_bytes[40..44], 1);
        })?;
        self.mark_page_dirty(child_ino, _child_block)?;

        // 3. Insert directory entry in parent.
        let index = {
            let parent = crate::inode::Inode::read(self, parent_ino)?;
            let dtree = crate::btree::dtree::Dtree::from_inode_data(parent.dtroot_bytes())?;
            dtree.entries().map(|e| e.len() as u32).unwrap_or(0)
        };
        let inserted = self.insert_dir_entry(parent_ino, &name_u16, child_ino, index)?;
        if !inserted {
            self.abort_transaction();
            return Err(StorageError::Other("directory entry already exists".to_string()));
        }

        // 4. Update parent link count (directories have link counts; regular
        //    files don't, but we mark the parent page dirty regardless).
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 5. Commit.
        self.commit_transaction()?;

        Ok(child_ino)
    }

    /// Remove (unlink) a directory entry by name.
    ///
    /// 1. Look up entry in parent's dtroot.
    /// 2. Remove entry.
    /// 3. Free child inode (mark as unused).
    /// 4. Update parent metadata.
    /// 5. Commit.
    #[cfg(feature = "writable")]
    pub fn unlink_file(&mut self, parent_ino: u32, name: &str) -> StorageResult<bool> {
        let _ = self.begin_transaction()?;

        let name_u16: Vec<u16> = name.encode_utf16().collect();

        // 1. Look up and remove the entry.
        let child_ino = self.remove_dir_entry(parent_ino, &name_u16)?;

        if child_ino.is_none() {
            self.abort_transaction();
            return Ok(false);
        }

        let child_ino = child_ino.unwrap();

        // 2. Free the child inode (set mode to 0 = unused).
        let child = crate::inode::Inode::read(self, child_ino)?;
        self.update_inode_page(child_ino, child.page_block, child.page_offset, |dinode_bytes| {
            // Zero out the mode to mark as free.
            LittleEndian::write_u32(&mut dinode_bytes[52..56], 0);
            LittleEndian::write_u64(&mut dinode_bytes[24..32], 0);
        })?;
        self.mark_page_dirty(child_ino, child.page_block)?;

        // 3. Mark parent dirty (dtroot already updated via remove_dir_entry).
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 4. Commit.
        self.commit_transaction()?;

        Ok(true)
    }
}
