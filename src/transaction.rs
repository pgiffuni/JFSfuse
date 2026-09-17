// SPDX-License-Identifier: MIT
//! Transaction manager — coordinates journaled metadata write transactions.
//!
//! ## Design
//!
//! The transaction manager is the gatekeeper for all metadata mutations.
//! No code outside this module (and the modules it explicitly exposes)
//! may write to the storage backend or mark pages dirty.
//!
//! ### Write ordering (ordered-data journaling)
//!
//! 1. Data blocks are written and flushed (`flush_data`).
//! 2. Journal log records are appended for metadata changes.
//! 3. Journal is flushed.
//! 4. Metadata blocks are written to their final locations.
//! 5. Metadata is flushed.
//! 6. Transaction is marked committed in the log.
//!
//! ### Concurrency
//!
//! A single filesystem-wide write lock is used initially. `begin()` blocks
//! until no other transaction is active.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::storage::{PageCache, Storage, StorageError};

pub use crate::types::{BlockNo, BlockLength, Pxd};

/// A unique transaction identifier.
pub type TransactionId = u64;

/// Internal state tracked for an in-progress transaction.
struct ActiveTxn {
    txid: TransactionId,
    /// Dirty pages owned by this transaction: (inode, block_number).
    dirty_pages: Vec<(u32, BlockNo)>,
    /// Blocks allocated during this transaction (to roll back on abort).
    allocated: Vec<BlockNo>,
    /// Blocks freed during this transaction (to roll back on abort).
    freed: Vec<BlockNo>,
}

/// Transaction manager — enforces single-writer discipline and coordinates
/// journal + metadata flush ordering.
pub struct TransactionManager {
    next_txid: AtomicU64,
    active: Option<ActiveTxn>,
    write_lock: std::sync::Mutex<()>,
}

impl TransactionManager {
    pub fn new() -> Self {
        Self {
            next_txid: AtomicU64::new(1),
            active: None,
            write_lock: std::sync::Mutex::new(()),
        }
    }

    /// Begin a new write transaction.
    ///
    /// Acquires the filesystem-wide write lock (single-writer model).
    /// Returns a `TransactionId` that callers pass to subsequent operations.
    pub fn begin(&mut self) -> Result<TransactionId, StorageError> {
        if self.active.is_some() {
            return Err(StorageError::Other(
                "transaction already in progress".to_string(),
            ));
        }
        let txid = self.next_txid.fetch_add(1, Ordering::SeqCst);
        self.active = Some(ActiveTxn {
            txid,
            dirty_pages: Vec::new(),
            allocated: Vec::new(),
            freed: Vec::new(),
        });
        Ok(txid)
    }

    /// Transaction ID of the currently active transaction, if any.
    pub fn current_txid(&self) -> Option<TransactionId> {
        self.active.as_ref().map(|a| a.txid)
    }

    /// Mark a page dirty and associate it with the current transaction.
    ///
    /// The page must already be in the cache. This method records the page
    /// in the transaction's dirty set so it will be committed or aborted.
    pub fn mark_dirty(
        &mut self,
        cache: &mut PageCache,
        inode: u32,
        block: BlockNo,
    ) -> Result<(), StorageError> {
        let txid = self
            .active
            .as_ref()
            .ok_or_else(|| StorageError::Other("no active transaction".to_string()))?
            .txid;
        cache.mark_dirty_txn(inode, block, txid);
        let active = self.active.as_mut().unwrap();
        if !active.dirty_pages.contains(&(inode, block)) {
            active.dirty_pages.push((inode, block));
        }
        Ok(())
    }

    /// Record a block allocation for potential rollback.
    pub fn record_allocation(&mut self, block: BlockNo) -> Result<(), StorageError> {
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| StorageError::Other("no active transaction".to_string()))?;
        active.allocated.push(block);
        Ok(())
    }

    /// Record a block freeing for potential rollback.
    pub fn record_free(&mut self, block: BlockNo) -> Result<(), StorageError> {
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| StorageError::Other("no active transaction".to_string()))?;
        active.freed.push(block);
        Ok(())
    }

    /// Commit the transaction.
    ///
    /// Ordering (ordered-data journaling):
    /// 1. Flush data blocks.
    /// 2. Append journal log records for all dirty metadata pages.
    /// 3. Flush the journal.
    /// 4. Write dirty metadata pages to their final disk locations.
    /// 5. Flush metadata.
    /// 6. Clear dirty/transaction state on the cache pages.
    ///
    /// On failure, the transaction is automatically aborted.
    pub fn commit(
        &mut self,
        storage: &dyn Storage,
        cache: &mut PageCache,
        journal: Option<&mut crate::journal::LogManager>,
    ) -> Result<CommitResult, StorageError> {
        let active = self
            .active
            .take()
            .ok_or_else(|| StorageError::Other("no active transaction".to_string()))?;

        let txid = active.txid;
        let dirty_pages = active.dirty_pages.clone();

        // 1. Flush data blocks first (ordered-data model).
        storage.flush_data()?;

        // 2. Append journal records for dirty metadata.
        if let Some(jm) = journal {
            for (inode, block) in &dirty_pages {
                if let Some(page) = cache.get(*inode, *block) {
                    jm.append_log_record(txid, *block, &page.data)?;
                }
            }
            jm.flush_journal()?;
        }

        // 3. Write dirty metadata pages to disk.
        let mut written = 0usize;
        for (inode, block) in &dirty_pages {
            if let Some(page) = cache.get(*inode, *block) {
                storage.write_block(*block, &page.data)?;
                written += 1;
            }
        }

        // 4. Flush metadata to durable storage.
        storage.flush_metadata()?;

        // 5. Clear dirty/transaction state on cache pages.
        for (inode, block) in &dirty_pages {
            cache.unpin_page(*inode, *block);
            let page = cache.get(*inode, *block);
            if let Some(mut p) = page {
                p.dirty = false;
                p.txid = 0;
                p.unpin();
                let _ = cache.put(*inode, p);
            }
        }

        Ok(CommitResult {
            pages_written: written,
            txid,
        })
    }

    /// Abort the transaction: discard all dirty pages, free allocated
    /// blocks, and restore freed blocks.
    pub fn abort(&mut self, cache: &mut PageCache) {
        if let Some(active) = self.active.take() {
            for (inode, block) in &active.dirty_pages {
                let page = cache.get(*inode, *block);
                if let Some(mut p) = page {
                    p.dirty = false;
                    p.txid = 0;
                    p.unpin();
                    let _ = cache.put(*inode, p);
                }
                cache.unpin_page(*inode, *block);
            }
        }
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a successful commit.
pub struct CommitResult {
    /// Number of dirty pages written.
    pub pages_written: usize,
    /// The transaction ID that was committed.
    pub txid: TransactionId,
}
