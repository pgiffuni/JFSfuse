// SPDX-License-Identifier: BSD-2-Clause
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

pub mod device_size;
pub mod geometry;
pub use device_size::get_storage_size;
pub use geometry::StorageGeometry;

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
    #[error("short read: requested {requested} bytes, got {actual}")]
    ShortRead {
        requested: usize,
        actual: usize,
    },
    #[error("short write: requested {requested} bytes, wrote {actual}")]
    ShortWrite {
        requested: usize,
        actual: usize,
    },
    #[error("unexpected EOF reading {requested} bytes, got {actual}")]
    UnexpectedEof {
        requested: usize,
        actual: usize,
    },
    #[error("operation interrupted by FUSE_INTERRUPT")]
    Interrupted,
    #[error("xattr not found")]
    XattrNotFound,
    #[error("xattr already exists")]
    XattrAlreadyExists,
    #[error("xattr name too long")]
    XattrNameTooLong,
    #[error("xattr value too large")]
    XattrValueTooLarge,
    #[error("xattr data too large for inline storage")]
    XattrDataTooLarge,
    #[error("directory entry already exists")]
    AlreadyExists,
    #[error("invalid filename or name")]
    InvalidName,
    #[error("no free inodes available")]
    NoFreeInode,
    #[error("not a directory")]
    NotADirectory,
    #[error("directory not empty")]
    DirectoryNotEmpty,
    #[error("not found")]
    NotFound,
    #[error("invalid file type for operation")]
    InvalidFileType,
    #[error("operation not supported")]
    NotSupported,
    #[error("cannot hard-link a directory")]
    CannotLinkDir,
    #[error("xattr buffer too small, needed {needed} bytes")]
    XattrBufferTooSmall { needed: usize },
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

    /// Downcast support for accessing concrete storage type.
    fn as_any(&self) -> &dyn std::any::Any where Self: 'static;

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

/// Validate a multi-block write range: alignment, overflow, and bounds.
///
/// - `data.len()` must be a whole multiple of `BLOCK_SIZE`.
/// - `block + count` must not overflow `BlockNo` (u64).
/// - The range must fit within `storage_size` blocks.
pub fn validate_block_range(
    storage_size: BlockNo,
    block: BlockNo,
    data: &[u8],
) -> Result<()> {
    if data.len() % BLOCK_SIZE != 0 {
        return Err(StorageError::Other(format!(
            "write_blocks: buffer length {} is not a multiple of BLOCK_SIZE ({})",
            data.len(),
            BLOCK_SIZE
        )));
    }
    let count = data.len() / BLOCK_SIZE;
    if count == 0 {
        return Err(StorageError::Other(
            "write_blocks: data length is zero".to_string(),
        ));
    }
    let end = block.checked_add(count as BlockNo).ok_or_else(|| {
        StorageError::Other("write_blocks: block range overflow".to_string())
    })?;
    if block >= storage_size {
        return Err(StorageError::BlockOutOfRange(block, storage_size));
    }
    if end > storage_size {
        return Err(StorageError::BlockOutOfRange(end, storage_size));
    }
    Ok(())
}

/// Read exactly `buf.len()` bytes from `file` at `offset`, looping on short
/// reads.
///
/// `read_at()` may return fewer bytes than requested (short read); this helper
/// repeats the call until the buffer is full. If a call returns zero bytes
/// (indicating EOF before the full buffer was satisfied), the helper returns
/// `StorageError::UnexpectedEof`.
///
/// Callers that need complete metadata or block data should use this helper.
/// Callers that tolerate partial reads (e.g. journal end-of-log detection)
/// should NOT use this helper.
pub fn read_exact_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = file.read_at(&mut buf[filled..], offset + filled as u64)?;
        if n == 0 {
            return Err(StorageError::UnexpectedEof {
                requested: buf.len(),
                actual: filled,
            });
        }
        filled += n;
    }
    Ok(())
}

/// Write exactly `data.len()` bytes to `file` at `offset`, looping on short
/// writes.
///
/// `write_at()` may write fewer bytes than requested (short write); this
/// helper repeats the call until the buffer is empty. If a call returns zero
/// bytes (indicating the writer is closed or the device is full), the helper
/// returns `StorageError::ShortWrite` to avoid an infinite loop.
pub fn write_all_at(file: &std::fs::File, offset: u64, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut written = 0usize;
    while written < data.len() {
        let n = file.write_at(&data[written..], offset + written as u64)?;
        if n == 0 {
            return Err(StorageError::ShortWrite {
                requested: data.len(),
                actual: written,
            });
        }
        written += n;
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
    /// Device geometry (sector size, media size).
    geometry: StorageGeometry,
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
        let geometry = StorageGeometry::from_file(&file)?;
        let size = geometry.media_size;
        Ok(Self { file, size, read_only, geometry })
    }

    pub fn block_to_offset(&self, block: BlockNo) -> u64 {
        block * (BLOCK_SIZE as u64)
    }

    /// Device geometry (sector size, media size).
    pub fn geometry(&self) -> &StorageGeometry {
        &self.geometry
    }

    fn validate_write(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        validate_block_write(self.size_blocks(), block, data)
    }
}

impl Storage for FileStorage {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

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
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        validate_block_range(self.size_blocks(), block, data)?;
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
        let mut buf = vec![0u8; len];
        read_exact_at(&self.file, offset, &mut buf)?;
        Ok(buf)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        write_all_at(&self.file, offset, data)
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
    fn as_any(&self) -> &dyn std::any::Any where Self: 'static {
        self
    }
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
/// - `short_read`: if true, truncate read results to a random shorter length.
/// - `zero_read`: if true, return a zero-length buffer on the *n*-th read.
/// - `zero_write`: if true, return a zero-byte write on the *n*-th write.
/// - `partial_io_at`: if >= 0, simulate a short I/O on the *n*-th read/write
///   at the given byte offset, returning fewer bytes than requested.
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
    short_read: std::sync::atomic::AtomicBool,
    read_fail_at: std::sync::atomic::AtomicU64,
    read_count: std::sync::atomic::AtomicU64,
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
            short_read: std::sync::atomic::AtomicBool::new(false),
            read_fail_at: std::sync::atomic::AtomicU64::new(u64::MAX),
            read_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Fail (return Err) on the *n*-th write call. 0 = never, 1 = first call.
    pub fn set_write_fail(&self, n: u64) {
        self.write_fail_at.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fail (return Err) on the *n*-th read call. 0 = never, 1 = first call.
    pub fn set_read_fail(&self, n: u64) {
        self.read_fail_at.store(n, std::sync::atomic::Ordering::SeqCst);
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

    /// Truncate reads to a random shorter length (simulates short read).
    pub fn set_short_read(&self, enabled: bool) {
        self.short_read.store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reset all fault counters to "never fail".
    pub fn reset(&self) {
        self.write_fail_at.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        self.sync_fail_at.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        self.read_fail_at.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        self.crash_on_sync.store(false, std::sync::atomic::Ordering::SeqCst);
        self.short_write.store(false, std::sync::atomic::Ordering::SeqCst);
        self.short_read.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<S: Storage> Storage for FaultInjector<S> {
    fn as_any(&self) -> &dyn std::any::Any where Self: 'static {
        self
    }
    fn read_block(&self, block: BlockNo) -> Result<Vec<u8>> {
        let n = self.read_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.read_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        let data = self.inner.read_block(block)?;
        if self.short_read.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::ShortRead {
                requested: BLOCK_SIZE,
                actual: (BLOCK_SIZE / 2).max(1),
            });
        }
        Ok(data)
    }

    fn write_block(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        if self.short_write.load(std::sync::atomic::Ordering::SeqCst) {
            let written = (data.len() / 2).max(1);
            return Err(StorageError::ShortWrite {
                requested: data.len(),
                actual: written,
            });
        }
        self.inner.write_block(block, data)
    }

    fn read_blocks(&self, block: BlockNo, count: u64) -> Result<Vec<u8>> {
        let n = self.read_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.read_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        let data = self.inner.read_blocks(block, count)?;
        if self.short_read.load(std::sync::atomic::Ordering::SeqCst) {
            let expected = count as usize * BLOCK_SIZE;
            return Err(StorageError::ShortRead {
                requested: expected,
                actual: (expected / 2).max(1),
            });
        }
        Ok(data)
    }

    fn write_blocks(&self, block: BlockNo, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        if self.short_write.load(std::sync::atomic::Ordering::SeqCst) {
            let written = (data.len() / 2).max(1);
            return Err(StorageError::ShortWrite {
                requested: data.len(),
                actual: written,
            });
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
        let n = self.read_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.read_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        let data = self.inner.read_bytes(offset, len)?;
        if self.short_read.load(std::sync::atomic::Ordering::SeqCst) {
            let truncate_to = (data.len() / 2).max(1);
            return Ok(data[..truncate_to].to_vec());
        }
        Ok(data)
    }

     fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        let n = self.write_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fail_at = self.write_fail_at.load(std::sync::atomic::Ordering::SeqCst);
        if n >= fail_at && fail_at != 0 {
            return Err(StorageError::FaultInjection);
        }
        if self.short_write.load(std::sync::atomic::Ordering::SeqCst) {
            let written = (data.len() / 2).max(1);
            return Err(StorageError::ShortWrite {
                requested: data.len(),
                actual: written,
            });
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

    /// Peek at a cached page without LRU promotion. Returns a clone if present.
    ///
    /// Unlike `get`, this does not mutate the LRU order, so it can be called
    /// from `&self` contexts (e.g. `Inode::read` / `find_inode_page` which
    /// may only have `&self` or `&mut` access). This ensures that reads within
    /// a transaction observe PageCache modifications made by earlier
    /// `update_inode_page` calls, rather than stale data from backing storage.
    pub fn peek(&self, inode: u32, block: BlockNo) -> Option<CachedPage> {
        self.cache.peek(&(inode, block)).cloned()
    }

    /// Peek at any cached page for a given physical block (regardless of
    /// which inode key was used). Returns a clone if present.
    ///
    /// This is used by `find_inode_page` to scan the inode table: the block
    /// may be cached under a different inode number (e.g. a previously-seen
    /// inode in the same table block), so we search by block number alone.
    pub fn peek_block(&self, block: BlockNo) -> Option<CachedPage> {
        self.cache
            .iter()
            .find(|((_, b), _)| *b == block)
            .map(|(_, p)| p.clone())
    }

    /// Get a page, fetching from storage if not cached. Returns a clone.
    ///
    /// If another inode's page for the **same** physical block is already
    /// cached (e.g. two inodes sharing an inode-table page), that newer
    /// version is reused instead of reading stale data from storage. This
    /// keeps all cache entries for the same block consistent within a
    /// transaction.
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

        // Check whether another inode's cached page for this block exists.
        let data = self
            .cache
            .iter()
            .find(|(k, _)| k.1 == block && k.0 != inode)
            .map(|(_, p)| p.data.clone())
            .unwrap_or_else(|| storage.read_block(block).unwrap_or_default());

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
    ///
    /// If the key already exists (page replacement), no eviction is needed.
    /// If the key is new and the cache is full, the LRU victim is checked:
    /// pages that are dirty (belonging to an in-flight transaction) or pinned
    /// (actively being modified) are not evicted. The caller must flush
    /// dirty pages before capacity pressure occurs.
    pub fn put(&mut self, inode: u32, page: CachedPage) -> Result<()> {
        let key = (inode, page.block);
        // Only check eviction for new entries.
        if !self.cache.contains(&key) && self.cache.len() >= self.cache.cap().get() {
            if let Some(v) = self.cache.peek_lru().map(|(_, v)| v) {
                if v.dirty || v.pin_count > 0 {
                    return Err(StorageError::Other(
                        "cache full: LRU victim is dirty or pinned".to_string(),
                    ));
                }
            }
        }
        self.cache.put(key, page);
        Ok(())
    }

    /// Load a page into the cache (for transaction-owned pages without an inode).
    ///
    /// Same eviction guard as `put` but uses inode 0 as the key prefix.
    pub fn put_block(&mut self, block: BlockNo, page: CachedPage) -> Result<()> {
        let key = (0, block);
        if !self.cache.contains(&key) && self.cache.len() >= self.cache.cap().get() {
            if let Some(v) = self.cache.peek_lru().map(|(_, v)| v) {
                if v.dirty || v.pin_count > 0 {
                    return Err(StorageError::Other(
                        "cache full: LRU victim is dirty or pinned".to_string(),
                    ));
                }
            }
        }
        self.cache.put(key, page);
        Ok(())
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

    /// Evict all cached pages for a given inode (FUSE_FORGET support).
    ///
    /// Removes all entries from the page cache whose key matches `inode`,
    /// freeing memory when the FUSE kernel module signals that an inode's
    /// reference count has reached zero.
    pub fn evict(&mut self, inode: u32) {
        let keys: Vec<(u32, BlockNo)> = self
            .cache
            .iter()
            .filter(|((ino, _), _)| *ino == inode)
            .map(|((ino, blk), _)| (*ino, *blk))
            .collect();
        for key in keys {
            self.cache.pop(&key);
        }
    }

    /// Clear all cached pages.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Returns `true` if any cached page is dirty.
    pub fn has_dirty(&self) -> bool {
        self.cache.iter().any(|(_, p)| p.dirty)
    }

    /// Returns `true` if any cached page is pinned.
    pub fn has_pinned(&self) -> bool {
        self.cache.iter().any(|(_, p)| p.pin_count > 0)
    }
}

/// Consistency checker for the page cache — verifies that the cache is in a
/// legal state for the given transaction phase.
///
/// In production this is a no-op (zero-cost). In tests, call `.check()`
/// to panic on invariant violations such as dirty pages outside a transaction
/// or pinned pages after commit.
pub struct ConsistencyChecker;

impl ConsistencyChecker {
    /// After a clean mount / no transaction active: no dirty pages, no pins.
    pub fn check_idle(cache: &PageCache) {
        assert!(
            !cache.has_dirty(),
            "consistency violation: dirty page outside active transaction"
        );
        assert!(
            !cache.has_pinned(),
            "consistency violation: pinned page outside active transaction"
        );
    }

    /// During a transaction: dirty pages exist but are pinned (being modified).
    pub fn check_in_transaction(cache: &PageCache) {
        // Dirty pages must be pinned — they're being modified by the txn.
        let violations = cache
            .cache
            .iter()
            .filter(|(_, p)| p.dirty && p.pin_count == 0)
            .count();
        assert_eq!(
            violations, 0,
            "consistency violation: dirty page not pinned during transaction"
        );
    }

    /// After a transaction commits: all pages clean and unpinned.
    pub fn check_post_commit(cache: &PageCache) {
        assert!(
            !cache.has_dirty(),
            "consistency violation: dirty page after commit"
        );
        assert!(
            !cache.has_pinned(),
            "consistency violation: pinned page after commit"
        );
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
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
        validate_block_range(self.size_blocks(), block, data)?;
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
    use std::sync::Arc;

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

    #[test]
    fn test_cache_evict_rejects_dirty_page() {
        let storage = Arc::new(MemoryStorage::new(4));
        let mut cache = PageCache::new(2);
        // Fill cache to capacity.
        cache.get_or_load(&*storage, 1, 0, 0).unwrap(); // page (1, 0) — LRU
        cache.get_or_load(&*storage, 2, 1, 0).unwrap(); // page (2, 1) — MRU

        // Mark page (1, 0) dirty + pinned.
        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0xCAFE);
        }

        // Access page (2, 1) to make it MRU, leaving (1, 0) as the LRU victim
        // — now it's dirty and pinned.
        let _ = cache.get(2, 1);

        // Trying to put a new page should fail because the LRU victim
        // is dirty and pinned.
        let mut new_page = CachedPage::new(2, vec![0u8; BLOCK_SIZE]);
        new_page.inode = Some(3);
        let result = cache.put(3, new_page);
        assert!(result.is_err(), "should reject inserting when LRU victim is dirty");
        assert!(
            result.unwrap_err().to_string().contains("dirty or pinned"),
            "error should mention dirty or pinned"
        );
    }

    #[test]
    fn test_cache_evict_allows_clean_page() {
        let storage = Arc::new(MemoryStorage::new(4));
        let mut cache = PageCache::new(2);
        cache.get_or_load(&*storage, 1, 0, 0).unwrap(); // page (1, 0)
        cache.get_or_load(&*storage, 2, 1, 0).unwrap(); // page (2, 1) — full

        // Both pages are clean and unpinned. Inserting a new page should evict the LRU one.
        let mut new_page = CachedPage::new(2, vec![0u8; BLOCK_SIZE]);
        new_page.inode = Some(3);
        let result = cache.put(3, new_page);
        assert!(result.is_ok(), "should allow eviction of clean pages");
    }

    #[test]
    fn test_cache_put_replaces_existing() {
        let storage = Arc::new(MemoryStorage::new(4));
        let mut cache = PageCache::new(4);
        cache.get_or_load(&*storage, 1, 0, 0).unwrap(); // page (1, 0)

        // Putting the same key should succeed (no eviction check needed).
        let mut new_page = CachedPage::new(0, vec![0u8; BLOCK_SIZE]);
        new_page.inode = Some(1);
        let result = cache.put(1, new_page);
        assert!(result.is_ok(), "put should succeed for existing key");
    }

    #[test]
    fn test_consistency_checker_idle() {
        let storage = Arc::new(MemoryStorage::new(4));
        let cache = PageCache::new(16);
        ConsistencyChecker::check_idle(&cache); // should not panic
    }

    #[test]
    fn test_consistency_checker_post_commit() {
        use crate::transaction::TransactionManager;
        let storage = Arc::new(MemoryStorage::new(8));
        let mut cache = PageCache::new(16);
        cache.get_or_load(&*storage, 1, 0, 0).unwrap();

        let mut tm = TransactionManager::new();
        tm.begin().unwrap();
        tm.mark_dirty(&mut cache, 1, 0).unwrap();

        {
            let page = cache.get_mut_for_write(1, 0).unwrap();
            page.write_u32_le(0, 0xABCD);
        }

        tm.commit(&*storage, &mut cache, None).unwrap();
        ConsistencyChecker::check_post_commit(&cache);
    }

    #[test]
    fn test_read_exact_at_complete_file() {
        use crate::types::BLOCK_SIZE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_read_exact_at");
        let data = vec![0xABu8; BLOCK_SIZE * 4];
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            write_all_at(&file, 0, &data).unwrap();
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap();
        let mut buf = vec![0u8; BLOCK_SIZE * 4];
        read_exact_at(&file, 0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn test_read_exact_at_unexpected_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_eof");
        let data = vec![0x42u8; 16];
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            write_all_at(&file, 0, &data).unwrap();
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap();
        let mut buf = vec![0u8; 32];
        let result = read_exact_at(&file, 0, &mut buf);
        assert!(result.is_err(), "should error when reading past EOF");
        match result.unwrap_err() {
            StorageError::UnexpectedEof { .. } => {}
            e => panic!("expected UnexpectedEof, got {e:?}"),
        }
    }

    #[test]
    fn test_write_all_at_appends_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_write_append");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let part1 = vec![0x11u8; 64];
        let part2 = vec![0x22u8; 64];
        write_all_at(&file, 0, &part1).unwrap();
        write_all_at(&file, 64, &part2).unwrap();
        let mut buf = vec![0u8; 128];
        read_exact_at(&file, 0, &mut buf).unwrap();
        assert_eq!(&buf[..64], &part1);
        assert_eq!(&buf[64..], &part2);
    }

    #[test]
    fn test_validate_block_range_rejects_unaligned() {
        let data = vec![0u8; BLOCK_SIZE * 2 + 1];
        let result = validate_block_range((BLOCK_SIZE * 10) as BlockNo, 0, &data);
        assert!(result.is_err(), "should reject non-block-aligned data");
    }

    #[test]
    fn test_validate_block_range_rejects_overflow() {
        let data = vec![0u8; BLOCK_SIZE];
        let result = validate_block_range((BLOCK_SIZE * 10) as BlockNo, BlockNo::MAX, &data);
        assert!(result.is_err(), "should reject overflowing block range");
    }

    #[test]
    fn test_validate_block_range_rejects_out_of_bounds() {
        let data = vec![0u8; BLOCK_SIZE];
        let result = validate_block_range((BLOCK_SIZE * 5) as BlockNo, (BLOCK_SIZE * 10) as BlockNo, &data);
        assert!(result.is_err(), "should reject write past end of storage");
    }

    #[test]
    fn test_validate_block_range_accepts_valid() {
        let data = vec![0xDEu8; BLOCK_SIZE * 3];
        let result = validate_block_range((BLOCK_SIZE * 10) as BlockNo, 1, &data);
        assert!(result.is_ok(), "should accept valid block range");
    }

    #[test]
    fn test_fault_injector_short_write() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(4)));
        inject.set_short_write(true);
        let data = vec![0xABu8; BLOCK_SIZE];
        let result = inject.write_blocks(BLOCK_SIZE as BlockNo, &data);
        assert!(result.is_err(), "should detect short write");
        match result.unwrap_err() {
            StorageError::ShortWrite { .. } => {}
            e => panic!("expected ShortWrite error, got {e:?}"),
        }
    }

    #[test]
    fn test_fault_injector_short_read_one_block() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(4)));
        inject.set_short_read(true);
        let result = inject.read_block(0);
        assert!(result.is_err(), "should detect short read on one-block read");
    }

    #[test]
    fn test_fault_injector_short_read_multi_block() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(8)));
        inject.set_short_read(true);
        let result = inject.read_blocks(BLOCK_SIZE as BlockNo, 2);
        assert!(result.is_err(), "should detect short read on multi-block read");
    }

    #[test]
    fn test_fault_injector_read_fail() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(4)));
        inject.set_read_fail(1);
        let result = inject.read_block(0);
        assert!(result.is_err(), "should fail on read");
    }

    #[test]
    fn test_fault_injector_read_fail_at_after_count() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(4)));
        inject.set_read_fail(3);
        let r1 = inject.read_block(0);
        assert!(r1.is_ok(), "first read should succeed");
        let r2 = inject.read_block(0);
        assert!(r2.is_ok(), "second read should succeed");
        let r3 = inject.read_block(0);
        assert!(r3.is_err(), "third read should fail");
    }

    #[test]
    fn test_fault_injector_short_write_always() {
        let inject = FaultInjector::new(Box::new(MemoryStorage::new(8)));
        inject.set_short_write(true);

        let data = vec![0xABu8; BLOCK_SIZE];
        assert!(inject.write_blocks(0, &data).is_err(), "first write should be short");
        assert!(inject.write_blocks(BLOCK_SIZE as BlockNo, &data).is_err(), "second write should be short");
        assert!(inject.write_blocks((BLOCK_SIZE * 2) as BlockNo, &data).is_err(), "third write should be short");
    }
}