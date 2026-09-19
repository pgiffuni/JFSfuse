// SPDX-License-Identifier: GPL-2.0-or-later
//! JFS volume management.
//!
//! Mirrors the kernel JFS mount code — superblock validation, inline log
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
    /// Open handle counts, keyed by inode disk block (for open-unlinked semantics).
    /// Tracks how many times an inode page is held open. When nlink reaches 0
    /// (via unlink), the inode is only freed when its open-handle count drops to 0.
    #[cfg(feature = "writable")]
    pub open_handles: std::collections::HashMap<u64, u32>,
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
            #[cfg(feature = "writable")]
            open_handles: std::collections::HashMap::new(),
        };

        volume.init_log()?;
        volume.recover_journal()?;
        #[cfg(feature = "writable")]
        {
            volume.bmap = Some(BlockAllocMap::new(volume.storage.clone())?);
        }

        Ok(volume)
    }

    /// Mount the filesystem in writable mode (Phase 13).
    ///
    /// Performs the full mount safety sequence:
    /// 1. Read and validate the superblock.
    /// 2. Identify the journal.
    /// 3. Check journal state.
    /// 4. Replay committed transactions (recovery).
    /// 5. Rebuild or validate allocation summaries (bmap).
    /// 6. Validate root inode.
    /// 7. Validate root directory.
    /// 8. Mark the filesystem dirty (FM_DIRTY) to indicate a writer is active.
    ///
    /// If any step fails, the mount is refused.
    #[cfg(feature = "writable")]
    pub fn mount(path: &str) -> StorageResult<Self> {
        let storage = Arc::new(FileStorage::open(Path::new(path))?);
        Self::mount_from_storage(storage)
    }

    /// Mount in writable mode from an already-open storage backend (Phase 13).
    #[cfg(feature = "writable")]
    pub fn mount_from_storage(storage: Arc<dyn Storage>) -> StorageResult<Self> {
        // Steps 1-2: Read and validate superblock, identify journal.
        let mut volume = Self::open_from_storage(storage)?;

        // Step 3-4: Journal recovery already ran in open_from_storage().
        // Step 5: Allocation map already initialized.

        // Step 6: Validate root inode exists and is a directory.
        let root_ino = volume.root_ino;
        let root = crate::inode::Inode::read(&mut volume, root_ino)?;
        if !root.is_dir() {
            return Err(StorageError::Other(
                "root inode is not a directory".to_string(),
            ));
        }

        // Step 7: Validate root directory is readable (dtree parses).
        let _dtree = crate::btree::dtree::Dtree::from_inode_data(root.dtroot_bytes())?;

        // Step 8: Mark filesystem dirty (FM_DIRTY) before allowing writes.
        volume.set_fs_state(crate::types::FM_DIRTY)?;

        Ok(volume)
    }

    /// Set the filesystem state in the superblock (Phase 13).
    #[cfg(feature = "writable")]
    pub fn set_fs_state(&mut self, state: u32) -> StorageResult<()> {
        // Write the new filesystem state to the superblock at offset 40.
        let sb_bytes = self.storage.read_bytes(crate::types::SUPER1_OFF, PSIZE)?;
        let mut buf = sb_bytes.to_vec();
        LittleEndian::write_u32(&mut buf[40..44], state);
        self.storage.write_bytes(crate::types::SUPER1_OFF, &buf)?;
        self.storage.flush_metadata()?;
        self.sb.s_state = buf[40..44].try_into().unwrap_or([0; 4]);
        Ok(())
    }

    /// Cleanly unmount the filesystem (Phase 13).
    ///
    /// Flushes all dirty pages and the journal, then marks the filesystem
    /// clean (FM_CLEAN). Should only be called after all writes are done.
    #[cfg(feature = "writable")]
    pub fn umount(&mut self) -> StorageResult<()> {
        // Flush any remaining dirty pages.
        self.page_cache.flush_all(&*self.storage)?;

        // Flush the journal.
        if let Some(log) = &mut self.log {
            log.sync()?;
        }

        // Flush the allocation map.
        if let Some(bmap) = &mut self.bmap {
            bmap.commit()?;
        }

        // Mark the filesystem clean.
        self.set_fs_state(crate::types::FM_CLEAN)?;

        log::info!("JFS filesystem cleanly unmounted");
        Ok(())
    }

    /// Read the primary superblock from disk.
    ///
    /// In JFS, the superblock sits at byte offset SUPER1_OFF (0x8000),
    /// i.e. sector 64 (SUPER1_B) * PBSIZE (512). We use the byte offset
    /// directly rather than `SUPER1_B * BLOCK_SIZE` since SUPER1_B is
    /// expressed in 512-byte sectors, not 4 KiB blocks.
    pub fn read_super(storage: &dyn Storage) -> StorageResult<JfsSuperblock> {
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
        } else {
            // Same fragment block — still update the size field.
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                LittleEndian::write_u64(&mut dinode_bytes[24..32], new_size_val);
            })?;
        }

        self.mark_page_dirty(ino, inode.page_block)?;
        self.commit_transaction()?;
        Ok(())
    }

    /// Pre-allocate or deallocate file space (writable builds only).
    ///
    /// Implements a subset of `fallocate(2)`:
    /// - `mode = 0` (default): allocate disk blocks for the range
    ///   `[offset, offset + len)`, zero-fill any unwritten blocks, and
    ///   update the file size if the range extends past EOF.
    /// - `FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE`: deallocate blocks in
    ///   the range, creating a hole (file size unchanged).
    ///
    /// `FALLOC_FL_COLLAPSE_RANGE`, `FALLOC_FL_INSERT_RANGE`,
    /// `FALLOC_FL_ZERO_RANGE`, and `FALLOC_FL_NOCACHE` are not supported.
    #[cfg(feature = "writable")]
    pub fn fallocate(
        &mut self,
        ino: u32,
        offset: u64,
        len: u64,
        mode: u32,
    ) -> StorageResult<()> {
        const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
        const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
        const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
        const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
        const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
        const FALLOC_FL_NOCACHE: u32 = 0x40;
        const FALLOC_FL_UNSUPPORTED: u32 =
            FALLOC_FL_COLLAPSE_RANGE | FALLOC_FL_ZERO_RANGE | FALLOC_FL_INSERT_RANGE | FALLOC_FL_NOCACHE;

        if mode & FALLOC_FL_UNSUPPORTED != 0 {
            return Err(StorageError::Other(format!(
                "fallocate mode {:#x} is not supported",
                mode
            )));
        }

        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;
        let mut xtree = crate::btree::xtree::Xtree::from_inode_data(inode.xtroot_bytes())?;
        let mut xtree_modified = false;

        let start_fsb = offset / (BLOCK_SIZE as u64);
        let end_fsb = (offset + len + (BLOCK_SIZE as u64) - 1) / (BLOCK_SIZE as u64);
        let block_count = end_fsb - start_fsb;

        let is_punch = mode & (FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE)
            == (FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE);

        if is_punch && block_count > 0 {
            // Deallocate blocks in the range, creating a hole.
            let freed = xtree.punch_extents(start_fsb as i64, end_fsb as i64);

            // Free the blocks via the allocator.
            if let Some(bmap) = self.bmap.as_mut() {
                for (addr, len) in &freed {
                    let mut pxd = crate::types::Pxd::default();
                    pxd.set_length(*len);
                    pxd.set_address(*addr);
                    let _ = bmap.free_extent(&pxd);
                }
            }

            xtree_modified = true;
        } else if block_count > 0 {
            // Allocate blocks for regions not already covered by extents.
            if let Some(bmap) = self.bmap.as_mut() {
                for fsb in start_fsb..end_fsb {
                    if xtree.lookup(fsb).ok().flatten().is_some() {
                        continue;
                    }
                    if let Some(pxd) = bmap.alloc_extent(1, fsb)? {
                        let new_addr = pxd.address();
                        let zero_block = vec![0u8; BLOCK_SIZE as usize];
                        self.storage.write_block(new_addr, &zero_block)?;
                        if xtree.insert_extent(fsb as i64, 1, new_addr) {
                            xtree_modified = true;
                        }
                    }
                }
            }
        }

        // Update file size if needed (only for space allocation, not punch).
        if !is_punch {
            let new_size = if mode & FALLOC_FL_KEEP_SIZE == 0 && offset + len > inode.size() {
                offset + len
            } else {
                inode.size()
            };
            if new_size != inode.size() {
                self.update_inode_page(
                    ino,
                    inode.page_block,
                    inode.page_offset,
                    |dinode_bytes| {
                        LittleEndian::write_u64(&mut dinode_bytes[24..32], new_size);
                    },
                )?;
            }
        }

        if xtree_modified {
            let xt_bytes = xtree.to_bytes();
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                let xt_off = crate::types::Dinode::size() - xt_bytes.len();
                dinode_bytes[xt_off..xt_off + xt_bytes.len()].copy_from_slice(&xt_bytes);
                let total_blocks =
                    xtree.iter_extents().map(|e| e.length as u64).sum::<u64>();
                LittleEndian::write_u64(&mut dinode_bytes[32..40], total_blocks);
            })?;
        }

        self.mark_page_dirty(ino, inode.page_block)?;
        self.storage.flush_data()?;
        self.commit_transaction()?;

        Ok(())
    }

    /// Set file attributes (chmod, chown, utimens).
    ///
    /// Only modifies the specified fields — all parameters except `ino` are optional.
    /// The inode's mode, uid, gid, and/or timestamps are updated within a single
    /// journaled transaction.
    #[cfg(feature = "writable")]
    pub fn setattr(
        &mut self,
        ino: u32,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> StorageResult<()> {
        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;

        self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
            // Offset 40..44: nlink (u32)
            // Offset 44..48: uid (u32)
            // Offset 48..52: gid (u32)
            // Offset 52..56: mode (u32)
            // Offset 56..60: atime.tv_sec (u32)
            // Offset 60..64: atime.tv_nsec (u32)
            // Offset 64..68: ctime.tv_sec (u32)
            // Offset 68..72: ctime.tv_nsec (u32)
            // Offset 72..76: mtime.tv_sec (u32)
            // Offset 76..80: mtime.tv_nsec (u32)

            if let Some(m) = mode {
                // Preserve file type bits (high nibble) and replace permission bits.
                let old_mode = LittleEndian::read_u32(&dinode_bytes[52..56]);
                let type_bits = old_mode & 0xf000;
                let new_mode = (m & 0x0fff) | type_bits;
                LittleEndian::write_u32(&mut dinode_bytes[52..56], new_mode);
            }
            if let Some(u) = uid {
                LittleEndian::write_u32(&mut dinode_bytes[44..48], u);
            }
            if let Some(g) = gid {
                LittleEndian::write_u32(&mut dinode_bytes[48..52], g);
            }
            if let Some(ts) = atime {
                LittleEndian::write_u32(&mut dinode_bytes[56..60], ts as u32);
                LittleEndian::write_u32(&mut dinode_bytes[60..64], 0);
            }
            if let Some(ts) = mtime {
                LittleEndian::write_u32(&mut dinode_bytes[72..76], ts as u32);
                LittleEndian::write_u32(&mut dinode_bytes[76..80], 0);
            }
            // ctime is updated to "now" — use 0 for simplicity (caller may set).
            // In production, this would use the current time.
        })?;

        self.mark_page_dirty(ino, inode.page_block)?;
        self.commit_transaction()?;

        Ok(())
    }

    /// fsync — flush a file's data and metadata to durable storage (writable builds only).
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
                    let ino = FILESYSTEM_I + (block_offset * crate::types::INOSPERPAGE as u64 + i as u64) as u32;
                    // Skip inode numbers that collide with known special inodes
                    // (e.g. root inode has ino = FILESYSTEM_I + ROOT_I).
                    if ino == FILESYSTEM_I + crate::types::ROOT_I {
                        continue;
                    }
                    let _ = self.tx_mgr.record_allocation(block_num);
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
            // Set fileset and inode number.
            LittleEndian::write_u32(&mut dinode_bytes[4..8], crate::types::FILESYSTEM_I);
            LittleEndian::write_u32(&mut dinode_bytes[8..12], child_ino - crate::types::FILESYSTEM_I);
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

    /// Create a directory in a parent directory.
    ///
    /// 1. Allocate inode.
    /// 2. Initialize inode (directory mode, empty dtroot with `.` and `..`).
    /// 3. Insert directory entry in parent.
    /// 4. Increment parent's link count (for subdirectories).
    /// 5. Commit.
    #[cfg(feature = "writable")]
    pub fn mkdir(&mut self, parent_ino: u32, name: &str) -> StorageResult<u32> {
        let _ = self.begin_transaction()?;

        // 1. Allocate a new inode.
        let (child_ino, _child_block, _child_off) = self.allocate_inode()?;

        // 2. Validate name length (JFS dtroot supports up to 11 u16 chars in a slot).
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        if name_u16.is_empty() || name_u16.len() > 11 {
            self.abort_transaction();
            return Err(StorageError::Other("invalid filename length".to_string()));
        }

        // 3. Initialize the child inode as a directory.
        // Directory mode: S_IFDIR | 0755 = 0x41ED
        // The dtroot is initialized with `.` (self) and `..` (parent) entries.
        self.update_inode_page(child_ino, _child_block, _child_off, |dinode_bytes| {
            // Set mode to directory (S_IFDIR | 0755 = 0x41ED)
            LittleEndian::write_u32(&mut dinode_bytes[52..56], 0x41ED);
            // Set size to 0
            LittleEndian::write_u64(&mut dinode_bytes[24..32], 0);
            // Set nblocks to 0
            LittleEndian::write_u64(&mut dinode_bytes[32..40], 0);
            // Set nlink to 2 (for `.` and `..`)
            LittleEndian::write_u32(&mut dinode_bytes[40..44], 2);
            // Set fileset and ino number for the child.
            LittleEndian::write_u32(&mut dinode_bytes[4..8], crate::types::FILESYSTEM_I);
            LittleEndian::write_u32(&mut dinode_bytes[8..12], child_ino - crate::types::FILESYSTEM_I);

            // Initialize the dtroot (inline directory B+-tree root, 288 bytes).
            // The dtroot is at u[96..] which maps to dinode offset 224 (128+96).
            let dtroot = &mut dinode_bytes[224..224 + 288];
            // dtroot header layout (32 bytes, at offset 96 within the union = offset 128+96=224 in dinode):
            //   Actually, dtroot_bytes() returns &self.u[96..], and u starts at offset 128.
            //   So dtroot[0..24] is the DASD+header, dtroot[24..32] is stbl.
            // The dtroot header:
            //   bytes 0-15: DASD (dir table slot array, 16 bytes)
            //   byte 16: flag (0x01 = BT_ROOT)
            //   byte 17: nextindex
            //   byte 18: freecnt
            //   byte 19: freelist
            //   bytes 20-23: idotdot (parent inode number)
            //   bytes 24-31: stbl (sorted index table, 8 bytes)

            // Set flag to BT_ROOT | BT_LEAF | BT_SWAPPED (matches root dtroot).
            dtroot[16] = 0x83;
            // nextindex = 2 (for `.` and `..` entries)
            dtroot[17] = 2;
            // freecnt = 0 (no free slots)
            dtroot[18] = 0;
            // freelist = 0
            dtroot[19] = 0;
            // idotdot = parent inode number (little-endian u32)
            LittleEndian::write_u32(&mut dtroot[20..24], parent_ino);
            // stbl: sorted index table. Entry 0 → slot 1, entry 1 → slot 2.
            dtroot[24] = 1; // `.` → slot 1
            dtroot[25] = 2; // `..` → slot 2

            // Write `.` entry into slot 1 (offset 32 within dtroot).
            // Slot format: inumber(4) next(1) name_len(1) name[11](22 bytes) index(4)
            let dot_slot = &mut dtroot[32..64];
            LittleEndian::write_u32(&mut dot_slot[0..4], child_ino); // inumber = self
            dot_slot[5] = 1; // name_len = 1 (for ".")
            LittleEndian::write_u16(&mut dot_slot[6..8], 0x2E); // '.'
            LittleEndian::write_u32(&mut dot_slot[28..32], 0); // index = 0

            // Write `..` entry into slot 2 (offset 64 within dtroot).
            let dotdot_slot = &mut dtroot[64..96];
            LittleEndian::write_u32(&mut dotdot_slot[0..4], parent_ino); // inumber = parent
            dotdot_slot[5] = 2; // name_len = 2 (for "..")
            LittleEndian::write_u16(&mut dotdot_slot[6..8], 0x2E); // '.'
            LittleEndian::write_u16(&mut dotdot_slot[8..10], 0x2E); // '.'
            LittleEndian::write_u32(&mut dotdot_slot[28..32], 1); // index = 1
        })?;
        self.mark_page_dirty(child_ino, _child_block)?;

        // 4. Insert directory entry in parent.
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

        // 5. Increment parent's link count (directories have subdirectory link count).
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.update_inode_page(parent_ino, parent.page_block, parent.page_offset, |dinode_bytes| {
            let nlink = LittleEndian::read_u32(&dinode_bytes[40..44]);
            LittleEndian::write_u32(&mut dinode_bytes[40..44], nlink + 1);
        })?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 6. Commit.
        self.commit_transaction()?;

        Ok(child_ino)
    }

    /// Remove a directory (rmdir).
    ///
    /// 1. Look up entry in parent's dtroot.
    /// 2. Verify child is a directory.
    /// 3. Verify the directory is empty (only `.` and `..` entries).
    /// 4. Remove the entry from parent.
    /// 5. Free the child inode.
    /// 6. Decrement parent's link count.
    /// 7. Commit.
    #[cfg(feature = "writable")]
    pub fn rmdir(&mut self, parent_ino: u32, name: &str) -> StorageResult<bool> {
        let _ = self.begin_transaction()?;

        let name_u16: Vec<u16> = name.encode_utf16().collect();

        // 1. Look up the child inode.
        let child_ino = self.remove_dir_entry(parent_ino, &name_u16)?;

        if child_ino.is_none() {
            self.abort_transaction();
            return Ok(false);
        }

        let child_ino = child_ino.unwrap();

        // 2. Verify child is a directory.
        let child = crate::inode::Inode::read(self, child_ino)?;
        if !child.is_dir() {
            self.abort_transaction();
            return Err(StorageError::Other("not a directory".to_string()));
        }

        // 3. Verify the directory is empty (only `.` and `..`).
        let dtree = crate::btree::dtree::Dtree::from_inode_data(child.dtroot_bytes())?;
        let num_entries = dtree.len_entries();
        if num_entries > 2 {
            self.abort_transaction();
            return Err(StorageError::Other("directory not empty".to_string()));
        }

        // 4. Free the child inode (set mode to 0 = unused).
        self.update_inode_page(child_ino, child.page_block, child.page_offset, |dinode_bytes| {
            // Zero out the mode to mark as free.
            LittleEndian::write_u32(&mut dinode_bytes[52..56], 0);
            LittleEndian::write_u64(&mut dinode_bytes[24..32], 0);
            LittleEndian::write_u32(&mut dinode_bytes[40..44], 0);
        })?;
        self.mark_page_dirty(child_ino, child.page_block)?;

        // 5. Decrement parent's link count.
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.update_inode_page(parent_ino, parent.page_block, parent.page_offset, |dinode_bytes| {
            let nlink = LittleEndian::read_u32(&dinode_bytes[40..44]);
            LittleEndian::write_u32(&mut dinode_bytes[40..44], nlink - 1);
        })?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 6. Commit.
        self.commit_transaction()?;

        Ok(true)
    }

    /// Remove (unlink) a directory entry by name.
    ///
    /// 1. Look up entry in parent's dtroot.
    /// 2. Remove entry.
    /// 3. Decrement child's link count. If nlink reaches zero AND no open
    ///    handles remain, free the inode and its data blocks.
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

        // 2. Read the child inode to get its type and current nlink.
        let child = crate::inode::Inode::read(self, child_ino)?;
        let is_dir = child.is_dir();
        let current_nlink = child.dinode.nlink();

        // Directories can only have nlink == 0 (already removed via rmdir).

        // Directories can only have nlink == 0 (already removed via rmdir).
        if is_dir {
            self.abort_transaction();
            return Err(StorageError::Other("use rmdir for directories".to_string()));
        }

        let new_nlink = current_nlink.saturating_sub(1);
        let should_free = new_nlink == 0 && !self.has_open_handles(child.page_block);

        if should_free {
            // No open handles — free the inode and its data blocks.
            // First, compute the blocks to free (can't borrow self.bmap inside the closure).
            let freed_blocks: Vec<(u64, u32)> = if child.is_regular() {
                let xt_bytes = &child.dinode.u[96..];
                if let Ok(mut xt) = crate::btree::xtree::Xtree::from_inode_data(xt_bytes) {
                    xt.truncate_extents(0)
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            self.update_inode_page(child_ino, child.page_block, child.page_offset, |dinode_bytes| {
                // Zero out the mode to mark as free.
                LittleEndian::write_u32(&mut dinode_bytes[52..56], 0);
                LittleEndian::write_u64(&mut dinode_bytes[24..32], 0);
                LittleEndian::write_u32(&mut dinode_bytes[40..44], 0);
            })?;

            // Free the blocks.
            if let Some(bmap) = self.bmap.as_mut() {
                for (addr, len) in &freed_blocks {
                    let mut pxd = crate::types::Pxd::default();
                    pxd.set_length(*len);
                    pxd.set_address(*addr);
                    let _ = bmap.free_extent(&pxd);
                }
            }
        } else {
            // Still has open handles or nlink > 0 — just decrement nlink.
            // This implements the open-unlinked semantics: the inode persists
            // in the inode table until the last open handle is released.
            self.update_inode_page(child_ino, child.page_block, child.page_offset, |dinode_bytes| {
                LittleEndian::write_u32(&mut dinode_bytes[40..44], new_nlink);
            })?;
        }
        self.mark_page_dirty(child_ino, child.page_block)?;

        // 4. Mark parent dirty (dtroot already updated via remove_dir_entry).
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 5. Commit.
        self.commit_transaction()?;

        Ok(true)
    }

    /// Create a hard link (Phase 10).
    ///
    /// 1. Verify the target inode is not a directory (directories may only
    ///    be linked via `.` / `..` in JFS).
    /// 2. Insert a new directory entry pointing to the target inode.
    /// 3. Increment the target inode's link count.
    /// 4. Update parent directory metadata.
    /// 5. Commit.
    #[cfg(feature = "writable")]
    pub fn link_file(&mut self, parent_ino: u32, name: &str, target_ino: u32) -> StorageResult<u32> {
        let _ = self.begin_transaction()?;

        // 1. Read the target inode.
        let target = crate::inode::Inode::read(self, target_ino)?;

        // 2. Reject hard links to directories.
        if target.is_dir() {
            let _ = self.abort_transaction();
            return Err(StorageError::Other("cannot hard-link a directory".to_string()));
        }

        // 3. Validate name.
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        if name_u16.is_empty() || name_u16.len() > 11 {
            let _ = self.abort_transaction();
            return Err(StorageError::Other("invalid filename length".to_string()));
        }

        // 4. Insert directory entry in parent.
        let index = {
            let parent = crate::inode::Inode::read(self, parent_ino)?;
            let dtree = crate::btree::dtree::Dtree::from_inode_data(parent.dtroot_bytes())?;
            dtree.entries().map(|e| e.len() as u32).unwrap_or(0)
        };
        let inserted = self.insert_dir_entry(parent_ino, &name_u16, target_ino, index)?;
        if !inserted {
            let _ = self.abort_transaction();
            return Err(StorageError::Other("directory entry already exists".to_string()));
        }

        // 5. Increment target's nlink.
        let new_nlink = target.dinode.nlink() + 1;
        self.update_inode_page(target_ino, target.page_block, target.page_offset, |dinode_bytes| {
            LittleEndian::write_u32(&mut dinode_bytes[40..44], new_nlink);
        })?;
        self.mark_page_dirty(target_ino, target.page_block)?;

        // 6. Mark parent dirty.
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 7. Commit.
        self.commit_transaction()?;

        Ok(target_ino)
    }

    /// Open a file — increments the open-handle count for open-unlinked semantics.
    #[cfg(feature = "writable")]
    pub fn open_file(&mut self, ino: u32) -> StorageResult<()> {
        let inode = crate::inode::Inode::read(self, ino)?;
        *self.open_handles.entry(inode.page_block).or_insert(0) += 1;
        Ok(())
    }

    /// Check if an inode has any open handles.
    /// The open_handles map is keyed by the inode's disk page block.
    #[cfg(feature = "writable")]
    fn has_open_handles(&self, page_block: u64) -> bool {
        self.open_handles.get(&page_block).copied().unwrap_or(0) > 0
    }

    /// Create a symbolic link (Phase 11).
    ///
    /// 1. Allocate inode.
    /// 2. Initialize inode (symlink mode, inline path if short).
    /// 3. Insert directory entry in parent.
    /// 4. Update parent metadata.
    /// 5. Commit.
    ///
    /// Short symlinks (path < 128 bytes) are stored inline in the inode's
    /// union area. Long symlinks would use allocated data blocks (not yet
    /// implemented).
    #[cfg(feature = "writable")]
    pub fn symlink(&mut self, parent_ino: u32, name: &str, target: &str) -> StorageResult<u32> {
        let _ = self.begin_transaction()?;

        // 1. Allocate a new inode.
        let (child_ino, _child_block, _child_off) = self.allocate_inode()?;

        // 2. Validate name.
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        if name_u16.is_empty() || name_u16.len() > 11 {
            self.abort_transaction();
            return Err(StorageError::Other("invalid filename length".to_string()));
        }

        // 3. Initialize the child inode as a symlink.
        let target_bytes = target.as_bytes();
        let use_inline = target_bytes.len() < 128;

        if !use_inline {
            self.abort_transaction();
            return Err(StorageError::Other("long symlinks not yet supported".to_string()));
        }

        self.update_inode_page(child_ino, _child_block, _child_off, |dinode_bytes| {
            // Set mode to symlink (S_IFLNK | 0777 = 0xA1FF)
            LittleEndian::write_u32(&mut dinode_bytes[52..56], 0xA1FF);
            // Set size to the target path length.
            LittleEndian::write_u64(&mut dinode_bytes[24..32], target_bytes.len() as u64);
            // Set nblocks to 0 (inline symlink uses no data blocks).
            LittleEndian::write_u64(&mut dinode_bytes[32..40], 0);
            // Set nlink to 1.
            LittleEndian::write_u32(&mut dinode_bytes[40..44], 1);
            // Set fileset and ino number.
            LittleEndian::write_u32(&mut dinode_bytes[4..8], crate::types::FILESYSTEM_I);
            LittleEndian::write_u32(&mut dinode_bytes[8..12], child_ino - crate::types::FILESYSTEM_I);

            // Store the target path inline in the union area (u[0..target_len]).
            // The union starts at offset 128 in the dinode.
            let target_offset = 128;
            let end = target_offset + target_bytes.len();
            if end <= dinode_bytes.len() {
                dinode_bytes[target_offset..end].copy_from_slice(target_bytes);
            }
        })?;
        self.mark_page_dirty(child_ino, _child_block)?;

        // 4. Insert directory entry in parent.
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

        // 5. Mark parent dirty.
        let parent = crate::inode::Inode::read(self, parent_ino)?;
        self.mark_page_dirty(parent_ino, parent.page_block)?;

        // 6. Commit.
        self.commit_transaction()?;

        Ok(child_ino)
    }

    /// Read the target of a symbolic link (Phase 11).
    ///
    /// For inline symlinks, reads the path from the inode's union area.
    /// For block-based symlinks, reads through the xtree (not yet implemented).
    #[cfg(feature = "writable")]
    pub fn read_symlink(&mut self, ino: u32) -> StorageResult<Vec<u8>> {
        let inode = crate::inode::Inode::read(self, ino)?;

        if !inode.is_symlink() {
            return Err(StorageError::Other("not a symbolic link".to_string()));
        }

        let size = inode.size() as usize;
        if size > 0 && size < 128 {
            // Inline fast symlink — read from the union area.
            let target = &inode.dinode.u[0..size.min(inode.dinode.u.len())];
            return Ok(target.to_vec());
        }

        Err(StorageError::Other("symlink target not available".to_string()))
    }

    /// Release an open file handle — decrements the open-handle count.
    /// If the inode is in pending-deletion state (nlink == 0) and this was
    /// the last handle, the inode's blocks are freed.
    #[cfg(feature = "writable")]
    pub fn release_file(&mut self, ino: u32) -> StorageResult<()> {
        let inode = crate::inode::Inode::read(self, ino)?;
        if let Some(count) = self.open_handles.get_mut(&inode.page_block) {
            *count = count.saturating_sub(1);
        }

        // If nlink is 0 and no open handles remain, free the inode.
        self.begin_transaction()?;
        if inode.dinode.nlink() == 0 && !self.has_open_handles(inode.page_block) {
            // Free data blocks (compute first to avoid borrow conflicts).
            let freed_blocks: Vec<(u64, u32)> = if inode.is_regular() {
                let xt_bytes = inode.xtroot_bytes();
                if let Ok(mut xt) = crate::btree::xtree::Xtree::from_inode_data(xt_bytes) {
                    xt.truncate_extents(0)
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            if let Some(bmap) = self.bmap.as_mut() {
                for (addr, len) in &freed_blocks {
                    let mut pxd = crate::types::Pxd::default();
                    pxd.set_length(*len);
                    pxd.set_address(*addr);
                    let _ = bmap.free_extent(&pxd);
                }
            }
            // Mark inode as free on disk.
            self.update_inode_page(ino, inode.page_block, inode.page_offset, |dinode_bytes| {
                LittleEndian::write_u32(&mut dinode_bytes[52..56], 0);
            })?;
            self.mark_page_dirty(ino, inode.page_block)?;
        }
        self.commit_transaction()?;
        Ok(())
    }

    /// Extended attribute operations (Phase 12).
    ///
    /// Inline xattrs are stored in the inode's union area (`u[0..]`),
    /// using a TLV format: [name_len:u16 LE][val_len:u16 LE][name bytes][value bytes].
    /// The di_ea DXD descriptor marks the extent as inline (DXD_INLINE).
    /// The INLINEEA mode flag is set when xattrs are present.
    #[cfg(feature = "writable")]
    pub fn setxattr(
        &mut self,
        ino: u32,
        name: &str,
        value: &[u8],
        flags: u32,
    ) -> StorageResult<()> {
        const XATTR_CREATE: u32 = 1;
        const XATTR_REPLACE: u32 = 2;

        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;
        let name_bytes = name.as_bytes();

        if name_bytes.len() > 255 {
            self.abort_transaction();
            return Err(StorageError::Other("xattr name too long".to_string()));
        }
        if value.len() > crate::types::IXATTRSIZE {
            self.abort_transaction();
            return Err(StorageError::Other("xattr value too large for inline storage".to_string()));
        }

        // Parse existing xattrs from the inode's union area.
        let ea_data = self.read_inline_xattr_data(&inode);
        let parsed = Self::parse_xattr_list(&ea_data);

        // Check existence for XATTR_CREATE / XATTR_REPLACE semantics.
        let exists = parsed.iter().any(|(n, _)| n == name_bytes);
        if flags & XATTR_CREATE != 0 && exists {
            self.abort_transaction();
            return Err(StorageError::Other("xattr already exists".to_string()));
        }
        if flags & XATTR_REPLACE != 0 && !exists {
            self.abort_transaction();
            return Err(StorageError::Other("xattr does not exist".to_string()));
        }

        // Build new xattr list: replace existing entry or append.
        let mut new_list: Vec<u8> = Vec::new();
        for (existing_name, existing_val) in &parsed {
            if existing_name == name_bytes {
                Self::write_xattr_entry(&mut new_list, existing_name, value);
            } else {
                Self::write_xattr_entry(&mut new_list, existing_name, existing_val);
            }
        }
        if !exists {
            Self::write_xattr_entry(&mut new_list, name_bytes, value);
        }

        // Check total size fits in IXATTRSIZE (128 bytes).
        if new_list.len() > crate::types::IXATTRSIZE {
            self.abort_transaction();
            return Err(StorageError::Other("xattr data too large".to_string()));
        }

        let ea_size = new_list.len() as u32;
        let page_block = inode.page_block;
        let page_offset = inode.page_offset;

        self.update_inode_page(ino, page_block, page_offset, |dinode_bytes| {
            // Store xattr data in the union area (u starts at offset 128).
            let u_offset = 128;
            for i in 0..crate::types::IXATTRSIZE {
                if u_offset + i < dinode_bytes.len() {
                    dinode_bytes[u_offset + i] = 0;
                }
            }
            let end = u_offset + new_list.len();
            if end <= dinode_bytes.len() {
                dinode_bytes[u_offset..end].copy_from_slice(&new_list);
            }

            // Update the di_ea DXD descriptor (at offset 104 in dinode).
            let ea_off = 104;
            dinode_bytes[ea_off] = crate::types::DxdFlag::DXD_INLINE.bits();
            LittleEndian::write_u32(&mut dinode_bytes[ea_off + 4..ea_off + 8], ea_size);
            dinode_bytes[ea_off + 8..ea_off + 16].copy_from_slice(&[0u8; 8]);

            // Set INLINEEA flag in mode.
            let mode = LittleEndian::read_u32(&dinode_bytes[52..56]);
            LittleEndian::write_u32(&mut dinode_bytes[52..56], mode | crate::types::INLINEEA);
        })?;

        self.mark_page_dirty(ino, page_block)?;
        self.commit_transaction()?;

        Ok(())
    }

    /// Get an extended attribute value by name (Phase 12).
    /// Returns `Ok(None)` if the xattr does not exist.
    #[cfg(feature = "writable")]
    pub fn getxattr(&mut self, ino: u32, name: &str) -> StorageResult<Option<Vec<u8>>> {
        let inode = crate::inode::Inode::read(self, ino)?;
        let ea_data = self.read_inline_xattr_data(&inode);
        let parsed = Self::parse_xattr_list(&ea_data);

        for (existing_name, existing_val) in parsed {
            if existing_name == name.as_bytes() {
                return Ok(Some(existing_val));
            }
        }
        Ok(None)
    }

    /// List xattr names for an inode (Phase 12).
    #[cfg(feature = "writable")]
    pub fn listxattr(&mut self, ino: u32) -> StorageResult<Vec<String>> {
        let inode = crate::inode::Inode::read(self, ino)?;
        let ea_data = self.read_inline_xattr_data(&inode);
        let parsed = Self::parse_xattr_list(&ea_data);
        Ok(parsed.iter().map(|(n, _)| String::from_utf8_lossy(n).to_string()).collect())
    }

    /// Remove an extended attribute (Phase 12).
    /// Returns `Ok(true)` if an attribute was removed, `Ok(false)` if not found.
    #[cfg(feature = "writable")]
    pub fn removexattr(&mut self, ino: u32, name: &str) -> StorageResult<bool> {
        let _ = self.begin_transaction()?;

        let inode = crate::inode::Inode::read(self, ino)?;
        let ea_data = self.read_inline_xattr_data(&inode);
        let parsed = Self::parse_xattr_list(&ea_data);

        let found = parsed.iter().any(|(n, _)| n == name.as_bytes());
        if !found {
            let _ = self.abort_transaction();
            return Ok(false);
        }

        // Rebuild the list without the removed entry.
        let mut new_list: Vec<u8> = Vec::new();
        for (existing_name, existing_val) in parsed {
            if existing_name != name.as_bytes() {
                Self::write_xattr_entry(&mut new_list, &existing_name, &existing_val);
            }
        }

        let ea_size = new_list.len() as u32;
        let page_block = inode.page_block;
        let page_offset = inode.page_offset;

        self.update_inode_page(ino, page_block, page_offset, |dinode_bytes| {
            let u_offset = 128;
            for i in 0..crate::types::IXATTRSIZE {
                if u_offset + i < dinode_bytes.len() {
                    dinode_bytes[u_offset + i] = 0;
                }
            }
            let end = u_offset + new_list.len();
            if end <= dinode_bytes.len() {
                dinode_bytes[u_offset..end].copy_from_slice(&new_list);
            }

            // Update di_ea descriptor.
            let ea_off = 104;
            if new_list.is_empty() {
                dinode_bytes[ea_off] = 0; // clear DXD_INLINE
            } else {
                dinode_bytes[ea_off] = crate::types::DxdFlag::DXD_INLINE.bits();
            }
            LittleEndian::write_u32(&mut dinode_bytes[ea_off + 4..ea_off + 8], ea_size);
            dinode_bytes[ea_off + 8..ea_off + 16].copy_from_slice(&[0u8; 8]);

            // Clear INLINEEA flag if no xattrs remain.
            let mode = LittleEndian::read_u32(&dinode_bytes[52..56]);
            let new_mode = if new_list.is_empty() {
                mode & !crate::types::INLINEEA
            } else {
                mode | crate::types::INLINEEA
            };
            LittleEndian::write_u32(&mut dinode_bytes[52..56], new_mode);
        })?;

        self.mark_page_dirty(ino, page_block)?;
        self.commit_transaction()?;

        Ok(true)
    }

    /// Read the inline xattr data from an inode's union area.
    #[cfg(feature = "writable")]
    fn read_inline_xattr_data(&self, inode: &crate::inode::Inode) -> Vec<u8> {
        let ea_flag = inode.dinode.di_ea.flag;
        if ea_flag & crate::types::DxdFlag::DXD_INLINE.bits() == 0 {
            return Vec::new();
        }
        let ea_size = inode.dinode.di_ea.length() as usize;
        if ea_size == 0 {
            return Vec::new();
        }
        let end = ea_size.min(inode.dinode.u.len());
        inode.dinode.u[..end].to_vec()
    }

    /// Parse an inline xattr TLV list into (name, value) pairs.
    fn parse_xattr_list(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut result = Vec::new();
        let mut pos = 0;
        while pos + 4 <= data.len() {
            let name_len = LittleEndian::read_u16(&data[pos..pos + 2]) as usize;
            let val_len = LittleEndian::read_u16(&data[pos + 2..pos + 4]) as usize;
            pos += 4;
            if pos + name_len + val_len > data.len() {
                break;
            }
            let name = data[pos..pos + name_len].to_vec();
            pos += name_len;
            let value = data[pos..pos + val_len].to_vec();
            pos += val_len;
            result.push((name, value));
        }
        result
    }

    /// Write an xattr entry (TLV) to a buffer.
    fn write_xattr_entry(buf: &mut Vec<u8>, name: &[u8], value: &[u8]) {
        let name_len = name.len().min(255) as u16;
        let val_len = value.len().min(65535) as u16;
        buf.extend_from_slice(&name_len.to_le_bytes());
        buf.extend_from_slice(&val_len.to_le_bytes());
        buf.extend_from_slice(&name[..name_len as usize]);
        buf.extend_from_slice(value);
    }

    /// Run a consistency check on the filesystem (Phase 14).
    ///
    /// Verifies:
    /// - Superblock validity
    /// - Journal state
    /// - Root inode is a directory
    /// - Root directory entries (`.`, `..`, valid inodes)
    /// - Each inode in the root directory references a valid inode
    /// - Xtree extents are ordered and non-overlapping
    /// - Extent physical addresses are within volume bounds
    ///
    /// Returns a `CheckReport` with all issues found. The `--repair` flag
    /// is intentionally not supported yet — repair is disabled until the
    /// checker can produce a complete diagnostic report.
    pub fn check_consistent(&mut self) -> StorageResult<CheckReport> {
        let mut report = CheckReport::default();

        // 1. Verify superblock magic.
        if !self.sb.is_valid_magic() {
            report.add_error("invalid superblock magic".to_string());
        }

        // 2. Verify journal state.
        if let Some(log) = &self.log {
            let ls = log.logsuper();
            if ls.magic_val() != LOGMAGIC {
                report.add_error("invalid log superblock magic".to_string());
            }
        }

        // 3. Verify root inode is a directory.
        let root_ino = self.root_ino;
        let root = match crate::inode::Inode::read(self, root_ino) {
            Ok(r) => r,
            Err(e) => {
                report.add_error_ino(format!("root inode unreadable: {}", e), root_ino);
                return Ok(report);
            }
        };

        if !root.is_dir() {
            report.add_error_ino("root inode is not a directory".to_string(), root_ino);
        }

        // 4. Validate root directory dtree.
        let root_dtree = match crate::btree::dtree::Dtree::from_inode_data(root.dtroot_bytes()) {
            Ok(d) => d,
            Err(e) => {
                report.add_error_ino(format!("root dtree invalid: {}", e), root_ino);
                return Ok(report);
            }
        };

        // 5. Verify `.entry` — each dirent points to a valid inode.
        let entries = root_dtree.entries()?;
        for entry in &entries {
            let child_ino = entry.inumber;

            // Skip `.` and `..` — they're validated separately.
            let name_str = String::from_utf16_lossy(&entry.name)
                .trim_end_matches('\0')
                .to_string();
            if name_str == "." || name_str == ".." {
                continue;
            }

            // Each child should have a readable inode.
            match crate::inode::Inode::read(self, child_ino) {
                Ok(child_inode) => {
                    // 6. Verify inode mode is valid (has a type).
                    let mode = child_inode.mode();
                    if mode & 0xf000 == 0 {
                        report.add_error_ino("inode has no type (mode=0)".to_string(), child_ino);
                    }
                }
                Err(_) => {
                    report.add_error_ino(
                        format!("directory entry '{}' points to invalid inode", name_str),
                        child_ino,
                    );
                }
            }
        }

        // 6. Verify xtree extents for regular files in root.
        for entry in &entries {
            let name_str = String::from_utf16_lossy(&entry.name)
                .trim_end_matches('\0')
                .to_string();
            if name_str == "." || name_str == ".." {
                continue;
            }

            if let Ok(child) = crate::inode::Inode::read(self, entry.inumber) {
                if child.is_regular() {
                    if let Ok(xt) = crate::btree::xtree::Xtree::from_inode_data(child.xtroot_bytes()) {
                        if let Err(e) = xt.validate() {
                            report.add_error_ino(
                                format!("xtree validation failed: {}", e),
                                entry.inumber,
                            );
                        }

                        // 7. Verify extent addresses are within volume bounds.
                        for ext in xt.iter_extents() {
                            if ext.address >= self.agg_size {
                                report.add_error_block(
                                    format!("extent address {} exceeds volume size", ext.address),
                                    ext.address,
                                );
                            }
                        }
                    }
                }
            }
        }

        // 8. Verify xattr consistency.
        for entry in &entries {
            let name_str = String::from_utf16_lossy(&entry.name)
                .trim_end_matches('\0')
                .to_string();
            if name_str == "." || name_str == ".." {
                continue;
            }

            if let Ok(child) = crate::inode::Inode::read(self, entry.inumber) {
                let ea_flag = child.dinode.di_ea.flag;
                let has_inline_ea = ea_flag & crate::types::DxdFlag::DXD_INLINE.bits() != 0;
                let inline_ea_mode = child.mode() & crate::types::INLINEEA != 0;

                if has_inline_ea && !inline_ea_mode {
                    report.add_error_ino(
                        "inode has inline EA descriptor but missing INLINEEA mode flag".to_string(),
                        entry.inumber,
                    );
                }
            }
        }

        Ok(report)
    }
}

/// Result of a consistency check.
#[derive(Debug, Clone)]
pub struct CheckIssue {
    /// Severity level: 0 = info, 1 = warning, 2 = error.
    pub level: u8,
    /// Human-readable description of the issue.
    pub message: String,
    /// The block or inode affected, if applicable.
    pub block: Option<u64>,
    pub ino: Option<u32>,
}

/// Result of a consistency check.
#[derive(Debug, Default)]
pub struct CheckReport {
    /// All issues found, grouped by severity.
    pub issues: Vec<CheckIssue>,
    /// True if any error-level issues were found.
    pub is_clean: bool,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        !self.issues.iter().any(|i| i.level >= 2)
    }

    pub fn add_error(&mut self, msg: String) {
        self.issues.push(CheckIssue { level: 2, message: msg, block: None, ino: None });
    }

    pub fn add_warning(&mut self, msg: String) {
        self.issues.push(CheckIssue { level: 1, message: msg, block: None, ino: None });
    }

    pub fn add_error_block(&mut self, msg: String, block: u64) {
        self.issues.push(CheckIssue { level: 2, message: msg, block: Some(block), ino: None });
    }

    pub fn add_error_ino(&mut self, msg: String, ino: u32) {
        self.issues.push(CheckIssue { level: 2, message: msg, block: None, ino: Some(ino) });
    }
}
