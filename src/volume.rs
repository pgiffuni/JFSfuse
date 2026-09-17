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

use crate::journal::{JournalRecovery, LogManager};
use crate::storage::{
    BLOCK_SIZE, FileStorage, PageCache, Result as StorageResult, Storage, StorageError,
};
use crate::transaction::{CommitResult, TransactionId, TransactionManager};
use crate::types::{
    self, AGGREGATE_I, BMAP_I, FILESYSTEM_I, FM_DIRTY, FM_LOGREDO, JFS_MAGIC, JfsSuperblock, LOG_I,
    LOGMAGIC, LOGREDONE, LOGVERSION, PSIZE, ROOT_I, SUPER1_B,
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
        };

        volume.init_log()?;
        volume.recover_journal()?;

        Ok(volume)
    }

    /// Read the primary superblock from disk.
    fn read_super(storage: &dyn Storage) -> StorageResult<JfsSuperblock> {
        let offset = SUPER1_B * (BLOCK_SIZE as u64);
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

        Ok(sb)
    }

    /// Validate the superblock: magic, version.
    fn validate_super(sb: &JfsSuperblock) -> StorageResult<()> {
        if !sb.is_valid_magic() {
            return Err(StorageError::InvalidSuperblock);
        }
        if sb.version() != 2 {
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
            let logsuper = LogManager::read_super(&**ls)?;
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
            let lm = LogManager::new(ls.clone(), logsuper);
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
    pub fn read_page(&mut self, block: u64) -> StorageResult<Vec<u8>> {
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
    /// to stable storage (writable builds only).
    #[cfg(feature = "writable")]
    pub fn commit_transaction(&mut self) -> StorageResult<CommitResult> {
        let journal = self.log.as_mut();
        self.tx_mgr
            .commit(&*self.storage, &mut self.page_cache, journal)
    }

    /// Abort the active transaction and discard dirty pages (writable builds only).
    #[cfg(feature = "writable")]
    pub fn abort_transaction(&mut self) {
        self.tx_mgr.abort(&mut self.page_cache)
    }

    /// Mark a cached page dirty under the current transaction (writable builds only).
    #[cfg(feature = "writable")]
    pub fn mark_page_dirty(&mut self, inode: u32, block: crate::types::BlockNo) -> StorageResult<()> {
        self.tx_mgr.mark_dirty(&mut self.page_cache, inode, block)
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
}
