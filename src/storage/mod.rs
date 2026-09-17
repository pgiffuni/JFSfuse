// SPDX-License-Identifier: MIT
//! Storage abstraction for JFS.
//!
//! Replaces the Linux kernel's `struct metapage`, `address_space`, and
//! `block_device` interfaces with a Rust-native storage layer that provides:
//! - [`Storage`] trait for block-level read/write (backed by a file or device)
//! - `PageCache` — an LRU cache for metadata pages (replaces `i_mapping`)
//! - `PageHandle` — a dirty/writeback handle for individual pages
//!
//! ## Write-path design
//!
//! The current cache is not yet transaction-safe: pages are cloned on access,
//! dirty pages can be written independently of a journal transaction, and
//! `flush_all()` has a potentially incorrect cache-key assumption for pages
//! without an inode. The next implementation pass should produce:
//!
//! - a mutable, transaction-owned page handle;
//! - dirty-page tracking by physical block;
//! - a transaction manager;
//! - ordered data and metadata writeback;
//! - abort handling;
//! - crash injection;
//! - recovery tests;
//! - a consistency-checking test helper.
//!
//! **Write primitives to add:**
//! - `flush_data()` and `flush_metadata()` semantics (initially backed by
//!   `sync_all()`).
//! - A read-only storage wrapper that rejects every write.
//! - Fault injection for short writes, failed syncs, and simulated crashes.
//!
//! Until the transaction layer is complete, writable FUSE mounting remains
//! disabled.

use byteorder::{ByteOrder, LittleEndian};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::path::Path;

/// Re-export block types from the types module.
pub use crate::types::{BLOCK_SIZE, BlockLength, BlockNo, PSIZE};

/// Errors that can occur during storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid superblock magic")]
    InvalidSuperblock,
    #[error("journal not replayed (FM_DIRTY)")]
    JournalNotReplayed,
    #[error("unsupported block size: {0}")]
    UnsupportedBlockSize(u32),
    #[error("page not found in cache")]
    PageNotFound,
    #[error("read-only storage: write rejected")]
    ReadOnly,
    #[error("injected I/O failure")]
    FaultInjection,
    #[error("block number {0} exceeds volume size of {1} blocks")]
    BlockOutOfRange(BlockNo, BlockNo),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Storage trait — provides raw block-level access to the backing device.
///
/// This replaces Linux's `struct block_device` + `submit_bio` I/O path.
/// All reads and writes are in units of `BLOCK_SIZE` bytes (4096).
///
/// ## Durability ordering
///
/// For journaled write support, storage consumers must respect:
///
/// 1. `flush_data()` — flush file data blocks.
/// 2. `append_journal()` / `flush_journal()` — flush log records.
/// 3. `flush_metadata()` — flush metadata blocks.
///
/// The default `flush_metadata()` delegates to `sync()`. A real implementation
/// should separate data and metadata flush paths so that metadata never
/// reaches stable storage before its journal record.
pub trait Storage: Send + Sync {
    /// Read a single block (fsb) at the given block number.
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>>;

    /// Write a single block at the given block number.
    fn write_block(&self, block: BlockNo, data: &[u8]) -> Result<()>;

    /// Read `count` blocks starting at `block`.
    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>>;

    /// Write `count` blocks starting at `block`.
    fn write_blocks(&self, block: BlockNo, data: &[u8]) -> Result<()>;

    /// Flush all pending writes to disk.
    fn sync(&self) -> Result<()>;

    /// Total size in filesystem blocks.
    fn size_blocks(&self) -> BlockNo;

    /// Read a raw byte range (used by log redo for log pages).
    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// Write a raw byte range.
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Flush file data to durable storage.
    ///
    /// In the ordered-data journaling model, data blocks must be flushed
    /// *before* the metadata that references them is journaled and committed.
    fn flush_data(&self) -> Result<()> {
        self.sync()
    }

    /// Flush metadata (including journal records) to durable storage.
    ///
    /// This must be called after all log records are written, before metadata
    /// blocks are written to their final locations.
    fn flush_metadata(&self) -> Result<()> {
        self.sync()
    }
}

/// Validate that `data.len()` equals `BLOCK_SIZE` and `block < size_blocks`.
pub fn validate_block_write(storage_size: BlockNo, block: BlockNo, data: &[u8]) -> Result<()> {
    if data.len() != BLOCK_SIZE {
        return Err(StorageError::Other(format!(
            "write_block: buffer is {} bytes, expected {}",
            data.len(),
            BLOCK_SIZE
        )));
    }
    if block >= storage_size {
        return Err(StorageError::BlockOutOfRange(block, storage_size));
    }
    Ok(())
}

/// A file-backed storage implementation using positional I/O.
///
/// Uses `FileExt::read_at` / `write_at` so that multiple cloned handles never
/// share mutable seek state — this is critical for thread-safe concurrent
/// access and for crash-injection tests that clone the underlying device.
pub struct FileStorage {
    file: std::fs::File,
    size: u64,
    /// If true, all write paths return errors.
    read_only: bool,
}

impl FileStorage {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_options(path, false)
    }

    pub fn open_readonly(path: &Path) -> Result<Self> {
        Self::open_with_options(path, true)
    }

    fn open_with_options(path: &Path, read_only: bool) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size, read_only })
    }

    pub fn block_to_offset(&self, block: BlockNo) -> u64 {
        block * (BLOCK_SIZE as u64)
    }

    fn validate_write(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        validate_block_write(self.size_blocks(), block, data)
    }
}

impl Storage for FileStorage {
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>> {
        let offset = self.block_to_offset(block);
        self.read_bytes(offset, BLOCK_SIZE)
    }

    fn write_block(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        self.validate_write(block, data)?;
        let offset = self.block_to_offset(block);
        self.write_bytes(offset, data)
    }

    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>> {
        let total = count as usize * BLOCK_SIZE;
        let offset = self.block_to_offset(block);
        self.read_bytes(offset, total)
    }

    fn write_blocks(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let count = data.len() / BLOCK_SIZE;
        if !self.read_only {
            validate_block_write(self.size_blocks(), block, &data[..BLOCK_SIZE])?;
        } else {
            return Err(StorageError::ReadOnly);
        }
        let offset = self.block_to_offset(block);
        self.write_bytes(offset, data)
    }

    fn sync(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        use std::os::unix::fs::FileExt;
        self.file.sync_all()?;
        Ok(())
    }

    fn flush_data(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        use std::os::unix::fs::FileExt;
        self.file.sync_data()?;
        Ok(())
    }

    fn flush_metadata(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        use std::os::unix::fs::FileExt;
        self.file.sync_all()?;
        Ok(())
    }

    fn size_blocks(&self) -> BlockNo {
        self.size / (BLOCK_SIZE as u64)
    }

    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        self.file.read_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        use std::os::unix::fs::FileExt;
        self.file.write_at(data, offset)?;
        Ok(())
    }
}

/// Read-only storage wrapper that rejects every write operation.
///
/// Wraps any `Storage` backend and intercepts all mutation calls, returning
/// `StorageError::ReadOnly`. Read and sync calls pass through unchanged.
pub struct ReadOnlyStorage<S: Storage> {
    inner: Box<S>,
}

impl<S: Storage> ReadOnlyStorage<S> {
    pub fn wrap(inner: Box<S>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: Storage> Storage for ReadOnlyStorage<S> {
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>> {
        self.inner.read_block(block)
    }

    fn write_block(&self, _block: BlockNo, _data: &[u8]) -> Result<()> {
        Err(StorageError::ReadOnly)
    }

    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>> {
        self.inner.read_blocks(block, count)
    }

    fn write_blocks(&self, _block: BlockNo, _data: &[u8]) -> Result<()> {
        Err(StorageError::ReadOnly)
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn size_blocks(&self) -> BlockNo {
        self.inner.size_blocks()
    }

    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.inner.read_bytes(offset, len)
    }

    fn write_bytes(&self, _offset: u64, _data: &[u8]) -> Result<()> {
        Err(StorageError::ReadOnly)
    }
}

/// Fault-injecting storage wrapper for crash-safety testing.
///
/// Wraps a `Storage` backend and injects failures at configurable points:
///
/// - `write_fail`: fail at the *n*-th `write_*` call.
/// - `sync_fail`: fail the *n*-th `sync`/`flush_*` call.
/// - `crash_on_sync`: if true, simulate a power loss by panicking during sync.
/// - `short_write`: if true, truncate write data to a random shorter length.
///
/// Counters are atomic so the wrapper is safe to share across threads.
pub struct FaultInjector<S: Storage> {
    inner: Box<S>,
    write_count: std::sync::atomic::AtomicU64,
    write_fail_at: std::sync::atomic::AtomicU64,
    sync_count: std::sync::atomic::AtomicU64,
    sync_fail_at: std::sync::atomic::AtomicU64,
    crash_on_sync: std::sync::atomic::AtomicBool,
    short_write: std::sync::atomic::AtomicBool,
}

impl<S: Storage> FaultInjector<S> {
    pub fn new(inner: Box<S>) -> Self {
        Self {
            inner,
            write_count: std::sync::atomic::AtomicU64::new(0),
            write_fail_at: std::sync::atomic::AtomicU64::new(u64::MAX),
            sync_count: std::sync::atomic::AtomicU64::new(0),
            sync_fail_at: std::sync::atomic::AtomicU64::new(u64::MAX),
            crash_on_sync: std::sync::atomic::AtomicBool::new(false),
            short_write: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Fail (return Err) on the *n*-th write call. 0 = never, 1 = first call.
    pub fn set_write_fail(&self, n: u64) {
        self.write_fail_at.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fail on the *n*-th sync/flush call. 0 = never, 1 = first call.
    pub fn set_sync_fail(&self, n: u64) {
        self.sync_fail_at.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Simulate a power-loss crash by aborting the process on the next sync.
    pub fn set_crash_on_sync(&self, crash: bool) {
        self.crash_on_sync.store(crash, std::sync::atomic::Ordering::SeqCst);
    }

    /// Truncate writes to a random shorter length.
    pub fn set_short_write(&self, enabled: bool) {
        self.short_write.store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reset all fault counters to "never fail".
    pub fn reset(&self) {
        self.write_fail_at.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        self.sync_fail_at.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        self.crash_on_sync.store(false, std::sync::atomic::Ordering::SeqCst);
        self.short_write.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<S: Storage> Storage for FaultInjector<S> {
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>> {
        self.inner.read_block(block)
    }

    fn write_block(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        if self.short_write.load(std::sync::atomic::Ordering::SeqCst) {
            let truncated = (data.len() / 2).max(1);
            let offset = block * (BLOCK_SIZE as u64);
            return self.inner.write_bytes(offset, &data[..truncated]);
        }
        self.inner.write_block(block, data)
    }

    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>> {
        self.inner.read_blocks(block, count)
    }

    fn write_blocks(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        self.inner.write_blocks(block, data)
    }

    fn sync(&self) -> Result<()> {
        let n = self.sync_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.sync_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if self.crash_on_sync.load(std::sync::atomic::Ordering::SeqCst) {
            std::process::exit(1);
        }
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        self.inner.sync()
    }

    fn size_blocks(&self) -> BlockNo {
        self.inner.size_blocks()
    }

    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.inner.read_bytes(offset, len)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        self.inner.write_bytes(offset, data)
    }
}

/// A cached page in the metapage-like page cache.
///
/// ## Transaction safety
///
/// When the transaction layer is active, callers must obtain a page via
/// `PageCache::get_mut_for_write()` which associates the page with a
/// `TransactionId`. The page cannot be evicted while `pin_count > 0` and
/// its dirty bits are only cleared after the owning transaction commits
/// and the data reaches stable storage.
#[derive(Clone)]
pub struct CachedPage {
    /// The block number this page corresponds to (in fs blocks).
    pub block: BlockNo,
    /// Raw page data.
    pub data: Vec<u8>,
    /// Whether this page has been modified and needs writeback.
    pub dirty: bool,
    /// The source inode number (if metadata). For raw block I/O, this is None.
    pub inode: Option<u32>,
    /// The logical byte offset within the inode's address space.
    pub index: u64,
    /// Pin count — prevents eviction while > 0.
    pub pin_count: u32,
    /// Transaction ID that owns this page's dirty state, if any.
    pub txid: u64,
}

impl CachedPage {
    pub fn new(block: BlockNo, data: Vec<u8>) -> Self {
        Self {
            block,
            data,
            dirty: false,
            inode: None,
            index: 0,
            pin_count: 0,
            txid: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Mark a page as dirty, associating it with transaction `txid`.
    ///
    /// No code outside the transaction layer should call this directly —
    /// use `TransactionManager::mark_dirty()` instead.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Mark dirty and associate with a transaction.
    pub fn mark_dirty_txn(&mut self, txid: u64) {
        self.dirty = true;
        self.txid = txid;
    }

    /// Pin the page so it cannot be evicted from the cache.
    pub fn pin(&mut self) {
        self.pin_count = self.pin_count.saturating_add(1);
    }

    /// Unpin the page; returns the new pin count.
    pub fn unpin(&mut self) -> u32 {
        self.pin_count = self.pin_count.saturating_sub(1);
        self.pin_count
    }

    pub fn is_pinned(&self) -> bool {
        self.pin_count > 0
    }

    pub fn read_u32_le(&self, offset: usize) -> u32 {
        LittleEndian::read_u32(&self.data[offset..offset + 4])
    }

    pub fn write_u32_le(&mut self, offset: usize, val: u32) {
        LittleEndian::write_u32(&mut self.data[offset..offset + 4], val);
        self.dirty = true;
    }

    pub fn read_u64_le(&self, offset: usize) -> u64 {
        LittleEndian::read_u64(&self.data[offset..offset + 8])
    }

    pub fn write_u64_le(&mut self, offset: usize, val: u64) {
        LittleEndian::write_u64(&mut self.data[offset..offset + 8], val);
        self.dirty = true;
    }

    pub fn read_u16_le(&self, offset: usize) -> u16 {
        LittleEndian::read_u16(&self.data[offset..offset + 2])
    }

    pub fn write_u16_le(&mut self, offset: usize, val: u16) {
        LittleEndian::write_u16(&mut self.data[offset..offset + 2], val);
        self.dirty = true;
    }
}

/// Page cache — replaces the Linux page cache / `address_space`.
///
/// A fixed-size LRU cache keyed by (inode, block) tuples. This replaces
/// `read_metapage()`, `write_metapage()`, `release_metapage()`, and
/// `mark_metapage_dirty()`.
///
/// ## Transaction safety
///
/// `get_mut_for_write()` returns a mutable reference to the cached page,
/// allowing in-place modification instead of clone-and-replace. Pages are
/// pinned during an active write transaction and cannot be evicted.
pub struct PageCache {
    /// Capacity in number of pages.
    capacity: usize,
    /// LRU cache: key = (inode_number, block_number)
    cache: LruCache<(u32, BlockNo), CachedPage>,
}

impl PageCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            capacity,
            cache: LruCache::new(cap),
        }
    }

    /// Look up a cached page. Returns a clone if present.
    pub fn get(&mut self, inode: u32, block: BlockNo) -> Option<CachedPage> {
        self.cache.get(&(inode, block)).cloned()
    }

    /// Get a page, fetching from storage if not cached. Returns a clone.
    pub fn get_or_load(
        &mut self,
        storage: &dyn Storage,
        inode: u32,
        block: BlockNo,
        index: u64,
    ) -> Result<CachedPage> {
        let key = (inode, block);
        if let Some(page) = self.cache.get(&key) {
            return Ok(page.clone());
        }
        let data = storage.read_block(block)?;
        let page = CachedPage {
            block,
            data,
            dirty: false,
            inode: Some(inode),
            index,
            pin_count: 0,
            txid: 0,
        };
        self.cache.put(key, page.clone());
        Ok(page)
    }

    /// Get a mutable reference to a cached page, pinning it.
    ///
    /// The page cannot be evicted while it remains pinned. Call
    /// `unpin_page()` to release the pin.
    ///
    /// Returns `StorageError::PageNotFound` if the page is not cached.
    pub fn get_mut_for_write(&mut self, inode: u32, block: BlockNo) -> Result<&mut CachedPage> {
        let key = (inode, block);
        if let Some(page) = self.cache.get_mut(&key) {
            page.pin();
            Ok(page)
        } else {
            Err(StorageError::PageNotFound)
        }
    }

    /// Unpin a page previously obtained via `get_mut_for_write`.
    pub fn unpin_page(&mut self, inode: u32, block: BlockNo) {
        let key = (inode, block);
        if let Some(page) = self.cache.get_mut(&key) {
            page.unpin();
        }
    }

    /// Insert or replace a page in the cache.
    pub fn put(&mut self, inode: u32, page: CachedPage) {
        let key = (inode, page.block);
        self.cache.put(key, page);
    }

    /// Load a page into the cache (for transaction-owned pages without an inode).
    pub fn put_block(&mut self, block: BlockNo, page: CachedPage) {
        self.cache.put((0, block), page);
    }

    /// Mark a page as dirty (must already be cached).
    pub fn mark_dirty(&mut self, inode: u32, block: BlockNo) {
        let key = (inode, block);
        if let Some(page) = self.cache.get_mut(&key) {
            page.dirty = true;
        }
    }

    /// Mark a page as dirty and associate it with `txid`.
    pub fn mark_dirty_txn(&mut self, inode: u32, block: BlockNo, txid: u64) {
        let key = (inode, block);
        if let Some(page) = self.cache.get_mut(&key) {
            page.dirty = true;
            page.txid = txid;
        }
    }

    /// Write back all dirty pages belonging to a specific inode.
    pub fn flush_inode(&mut self, storage: &dyn Storage, inode: u32) -> Result<()> {
        let dirty_pages: Vec<(u32, BlockNo, Vec<u8>)> = self
            .cache
            .iter()
            .filter(|(_, p)| p.dirty && p.inode == Some(inode))
            .map(|(_, p)| (inode, p.block, p.data.clone()))
            .collect();

        for (_, block, data) in &dirty_pages {
            storage.write_block(*block, data)?;
        }
        for (_, block, _) in &dirty_pages {
            let key = (inode, *block);
            if let Some(p) = self.cache.get_mut(&key) {
                p.dirty = false;
                p.txid = 0;
            }
        }
        Ok(())
    }

    /// Write back all dirty pages.
    pub fn flush_all(&mut self, storage: &dyn Storage) -> Result<()> {
        let dirty_pages: Vec<(u32, BlockNo, Vec<u8>)> = self
            .cache
            .iter()
            .filter(|(_, p)| p.dirty)
            .map(|(_, p)| (p.inode.unwrap_or(0), p.block, p.data.clone()))
            .collect();

        for (inode, block, data) in &dirty_pages {
            storage.write_block(*block, data)?;
        }
        for (inode, block, _) in &dirty_pages {
            let key = (*inode, *block);
            if let Some(p) = self.cache.get_mut(&key) {
                p.dirty = false;
                p.txid = 0;
            }
        }
        Ok(())
    }

    /// Remove a cached page (replaces release_metapage).
    pub fn release(&mut self, inode: u32, block: BlockNo) {
        self.cache.pop(&(inode, block));
    }

    /// Clear all cached pages.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

/// Helper to convert a disk block number (in PBSIZE=512 units) to fs block
/// (in BPSIZE units). JFS uses fsb (block size) internally but the superblock
/// stores `s_l2bfactor` = log2(block_size / pbsize).
pub fn disk_to_fs_block(pbn: BlockNo, l2bfactor: u16) -> BlockNo {
    pbn << (l2cfactor_to_shift(l2bfactor))
}

pub fn fs_to_disk_block(fsb: BlockNo, l2cfactor: u16) -> BlockNo {
    fsb >> l2cfactor
}

fn l2cfactor_to_shift(l2cfactor: u16) -> u32 {
    l2cfactor as u32
}

/// A raw buffer pool for journal operations — replaces the `bufhdr[NBUFPOOL=128]`
/// buffer pool used in `jfs_logredo`.
pub struct BufferPool {
    buffers: Vec<Vec<u8>>,
    free: Vec<usize>,
}

impl BufferPool {
    pub fn new(size: usize, page_size: usize) -> Self {
        let mut buffers = Vec::with_capacity(size);
        let mut free = Vec::with_capacity(size);
        for i in 0..size {
            buffers.push(vec![0u8; page_size]);
            free.push(i);
        }
        Self { buffers, free }
    }

    pub fn acquire(&mut self) -> Option<(usize, &mut Vec<u8>)> {
        self.free.pop().map(|idx| (idx, &mut self.buffers[idx]))
    }

    pub fn release(&mut self, idx: usize) {
        self.buffers[idx].fill(0);
        self.free.push(idx);
    }

    pub fn get(&mut self, idx: usize) -> &mut Vec<u8> {
        &mut self.buffers[idx]
    }
}

/// Null/in-memory storage for testing and as a fallback.
pub struct NullStorage;

impl Storage for NullStorage {
    fn read_block(&self, _block: BlockNo) -> Result<Vec<u8>> {
        Ok(vec![0u8; BLOCK_SIZE])
    }

    fn write_block(&self, _block: BlockNo, _data: &[u8]) -> Result<()> {
        Ok(())
    }

    fn read_blocks(&self, _block: BlockNo, count: u64) -> Result<Vec<u8>> {
        Ok(vec![0u8; count as usize * BLOCK_SIZE])
    }

    fn write_blocks(&self, _block: BlockNo, _data: &[u8]) -> Result<()> {
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn size_blocks(&self) -> BlockNo {
        0
    }

    fn read_bytes(&self, _offset: u64, len: usize) -> Result<Vec<u8>> {
        Ok(vec![0u8; len])
    }

    fn write_bytes(&self, _offset: u64, _data: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// In-memory storage for testing and crash-injection.
///
/// Stores all data in a growable `Mutex<Vec<u8>>`. This is useful for
/// tests that need persistence between operations but want to simulate
/// crashes by selectively dropping or zeroing portions of the buffer.
pub struct MemoryStorage {
    data: std::sync::Mutex<Vec<u8>>,
    size: u64,
}

impl MemoryStorage {
    pub fn new(size_blocks: u64) -> Self {
        let len = (size_blocks * (BLOCK_SIZE as u64)) as usize;
        Self {
            data: std::sync::Mutex::new(vec![0u8; len]),
            size: size_blocks * (BLOCK_SIZE as u64),
        }
    }

    pub fn block_to_offset(&self, block: BlockNo) -> u64 {
        block * (BLOCK_SIZE as u64)
    }
}

impl Storage for MemoryStorage {
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>> {
        let offset = self.block_to_offset(block);
        self.read_bytes(offset, BLOCK_SIZE)
    }

    fn write_block(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let offset = self.block_to_offset(block);
        self.write_bytes(offset, data)
    }

    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>> {
        let offset = self.block_to_offset(block);
        self.read_bytes(offset, count as usize * BLOCK_SIZE)
    }

    fn write_blocks(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let offset = self.block_to_offset(block);
        self.write_bytes(offset, data)
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn flush_data(&self) -> Result<()> {
        Ok(())
    }

    fn flush_metadata(&self) -> Result<()> {
        Ok(())
    }

    fn size_blocks(&self) -> BlockNo {
        self.size / (BLOCK_SIZE as u64)
    }

    fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let data = self.data.lock().unwrap();
        let end = offset + len as u64;
        if end > self.size {
            return Err(StorageError::Other(format!(
                "read beyond end of storage: {} > {}",
                end, self.size
            )));
        }
        Ok(data[offset as usize..end as usize].to_vec())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset + data.len() as u64;
        if end > self.size {
            return Err(StorageError::Other(format!(
                "write beyond end of storage: {} > {}",
                end, self.size
            )));
        }
        let mut buf = self.data.lock().unwrap();
        buf[offset as usize..end as usize].copy_from_slice(data);
        Ok(())
    }
}

use std::io;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_read_only_storage_rejects_writes() {
        let ro = ReadOnlyStorage::wrap(Box::new(NullStorage));
        assert!(ro.write_block(0, &vec![0u8; BLOCK_SIZE]).is_err());
        assert!(ro.write_bytes(0, &[0u8; 10]).is_err());
    }

    #[test]
    fn test_read_only_storage_allows_reads() {
        let ro = ReadOnlyStorage::wrap(Box::new(NullStorage));
        assert!(ro.read_block(0).is_ok());
        assert!(ro.read_bytes(0, 10).is_ok());
    }

    #[test]
    fn test_fault_injector_write_fail() {
        let inject = FaultInjector::new(Box::new(NullStorage));
        inject.set_write_fail(1);
        assert!(inject.write_block(0, &vec![0u8; BLOCK_SIZE]).is_err());
        inject.reset();
        assert!(inject.write_block(0, &vec![0u8; BLOCK_SIZE]).is_ok());
    }

    #[test]
    fn test_fault_injector_sync_fail() {
        let inject = FaultInjector::new(Box::new(NullStorage));
        inject.set_sync_fail(1);
        assert!(inject.sync().is_err());
        inject.reset();
        assert!(inject.sync().is_ok());
    }

    #[test]
    fn test_file_storage_validation() {
        let dir = std::env::temp_dir();
        let path = dir.join("jfsfuse_test_validation.img");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&vec![0u8; BLOCK_SIZE]).unwrap();
        }
        let fs = FileStorage::open(&path).unwrap();
        let size = fs.size_blocks();
        // Valid block write
        assert!(fs.write_block(0, &vec![0u8; BLOCK_SIZE]).is_ok());
        // Block out of range
        assert!(fs.write_block(size, &vec![0u8; BLOCK_SIZE]).is_err());
        // Wrong buffer size
        assert!(fs.write_block(0, &[0u8; 100]).is_err());
        // Read-only mode rejects writes
        let ro = FileStorage::open_readonly(&path).unwrap();
        assert!(ro.write_block(0, &vec![0u8; BLOCK_SIZE]).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cached_page_pin() {
        let mut page = CachedPage::new(0, vec![0u8; BLOCK_SIZE]);
        assert!(!page.is_pinned());
        page.pin();
        page.pin();
        assert!(page.is_pinned());
        assert_eq!(page.unpin(), 1);
        assert!(page.is_pinned());
        assert_eq!(page.unpin(), 0);
        assert!(!page.is_pinned());
    }

    #[test]
    fn test_cached_page_txid() {
        let mut page = CachedPage::new(0, vec![0u8; BLOCK_SIZE]);
        page.mark_dirty();
        assert!(page.dirty);
        assert_eq!(page.txid, 0);
        page.mark_dirty_txn(42);
        assert!(page.dirty);
        assert_eq!(page.txid, 42);
    }

    #[test]
    fn test_memory_storage_roundtrip() {
        let storage = MemoryStorage::new(4);
        let data = vec![0xABu8; BLOCK_SIZE];
        storage.write_block(0, &data).unwrap();
        let read = storage.read_block(0).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn test_page_cache_get_mut() {
        let storage = MemoryStorage::new(4);
        let mut cache = PageCache::new(16);
        cache.get_or_load(&storage, 1, 0, 0).unwrap();
        // Modify via mutable reference
        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0xDEADBEEF);
            assert!(page.dirty);
        }
        // Verify the modification persisted in the cache
        let page = cache.get(1, 0).unwrap();
        assert_eq!(page.read_u32_le(0), 0xDEADBEEF);
        cache.unpin_page(1, 0);
    }

    #[test]
    fn test_transaction_commit_and_abort() {
        use crate::transaction::TransactionManager;

        // --- Commit path ---
        let storage = std::sync::Arc::new(MemoryStorage::new(8));
        let mut cache = PageCache::new(16);
        cache.get_or_load(&*storage, 1, 0, 0).unwrap();

        // Modify the page via mutable access
        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0x12345678);
        }

        let mut tm = TransactionManager::new();
        let txid = tm.begin().unwrap();
        tm.mark_dirty(&mut cache, 1, 0).unwrap();

        let result = tm.commit(&*storage, &mut cache, None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().txid, txid);

        // Verify the data was written to storage
        let block = storage.read_block(0).unwrap();
        assert_eq!(LittleEndian::read_u32(&block[0..4]), 0x12345678);

        // --- Abort path ---
        cache.get_or_load(&*storage, 1, 0, 0).unwrap();
        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0xFFFFFFFF);
        }

        let mut tm2 = TransactionManager::new();
        tm2.begin().unwrap();
        tm2.mark_dirty(&mut cache, 1, 0).unwrap();
        tm2.abort(&mut cache);

        // Storage should be unchanged (still 0x12345678 from the previous commit)
        let block = storage.read_block(0).unwrap();
        assert_eq!(LittleEndian::read_u32(&block[0..4]), 0x12345678);
    }

    #[test]
    fn test_crash_injection_before_flush() {
        use crate::transaction::TransactionManager;

        let storage = std::sync::Arc::new(MemoryStorage::new(8));
        let mut cache = PageCache::new(16);
        cache.get_or_load(&*storage, 1, 0, 0).unwrap();

        // Modify the page
        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0xAABBCCDD);
        }

        let inject = std::sync::Arc::new(FaultInjector::new(
            Box::new(MemoryStorage::new(8)),
        ));
        inject.set_write_fail(1);

        let mut tm = TransactionManager::new();
        tm.begin().unwrap();
        tm.mark_dirty(&mut cache, 1, 0).unwrap();

        // Commit should fail because write injection kills the metadata write
        let result = tm.commit(&*inject, &mut cache, None);
        assert!(result.is_err());

        tm.abort(&mut cache);

        // Original storage should be untouched (still zero)
        let block = storage.read_block(0).unwrap();
        assert_eq!(LittleEndian::read_u32(&block[0..4]), 0);
    }

    #[test]
    fn test_crash_injection_sync_fail() {
        use crate::transaction::TransactionManager;

        let storage = std::sync::Arc::new(MemoryStorage::new(8));
        let mut cache = PageCache::new(16);
        cache.get_or_load(&*storage, 2, 0, 0).unwrap();

        {
            let page = cache.get_mut_for_write(2, 0).unwrap();
            page.write_u32_le(0, 0x11223344);
        }

        let inject = std::sync::Arc::new(FaultInjector::new(
            Box::new(MemoryStorage::new(8)),
        ));
        inject.set_sync_fail(1);

        let mut tm = TransactionManager::new();
        tm.begin().unwrap();
        tm.mark_dirty(&mut cache, 2, 0).unwrap();

        // The first flush_data call should fail due to sync injection
        let result = tm.commit(&*inject, &mut cache, None);
        assert!(result.is_err());

        tm.abort(&mut cache);
    }
}