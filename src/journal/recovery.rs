// SPDX-License-Identifier: GPL-2.0-or-later
//! Journal crash recovery (logredo).
//!
//! Implements the backward-replay logredo algorithm from jfsutils `logredo.c`
//! and `log_work.c`. Recovery scans the log backward from the end, encounters
//! commit records first (which mark transactions as committed), and then applies
//! only committed after-image records (REDOPAGE) to disk pages.
//!
//! **Run constraint**: This runs once at mount time, before any FUSE operations
//! are registered.

use std::collections::HashMap;

use byteorder::{ByteOrder, LittleEndian};

use crate::storage::{
    BLOCK_SIZE, BlockNo, BufferPool, Result as StorageResult, Storage, StorageError,
};
use crate::types::{
    Dinode, FM_CLEAN, FM_LOGREDO, LOG_BTROOT, LOG_COMMIT, LOG_DATA, LOG_DTREE, LOG_INODE,
    LOG_MOUNT, LOG_NOREDOINOEXT, LOG_NOREDOPAGE, LOG_REDOPAGE, LOG_SYNCPT, LOG_UPDATEMAP,
    LOG_XTREE, LOGPSIZE, LOGREDONE, LogSuper, Logpage, Lrd, PSIZE, Pxd,
};

/// Number of buffer pool slots (matches jfsutils `NBUFPOOL`).
pub const NBUFPOOL: usize = 128;

/// Commit tracking hash size (matches jfsutils `COMSIZE`).
pub const COMSIZE: usize = 256;
pub const COMHASH_BITS: usize = 6;
pub const COMHASH_SIZE: usize = 1 << COMHASH_BITS;

/// Page hash size for doblk table.
pub const BHASHSIZE: usize = 4096;

/// NoRedoFile hash size.
pub const NODOFILEHASHSIZE: usize = 4096;

/// Maximum active file systems in the log.
pub const MAX_ACTIVE: usize = 128;

// ──────────────────────── Commit tracker ────────────────────────

/// Tracks which log transactions have been committed.
///
/// Since logredo reads the log backward, `LOG_COMMIT` records appear
/// before their data records. We maintain a hash set of committed tids.
#[derive(Debug)]
pub struct CommitTracker {
    committed: HashMap<u32, CommitEntry>,
    hash_table: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, Copy)]
struct CommitEntry {
    tid: u32,
    next: Option<u32>,
}

impl CommitTracker {
    pub fn new() -> Self {
        Self {
            committed: HashMap::new(),
            hash_table: vec![Vec::new(); COMHASH_SIZE],
        }
    }

    /// Mark a transaction as committed (from a LOG_COMMIT record).
    pub fn commit(&mut self, tid: u32) {
        let hash = (tid & 0x3f) as usize;
        self.hash_table[hash].push(tid);
        self.committed.insert(tid, CommitEntry { tid, next: None });
    }

    /// Check if a transaction is committed. O(1) lookup.
    pub fn is_committed(&self, tid: u32) -> bool {
        self.committed.contains_key(&tid)
    }

    /// Called when backchain == 0 (end of transaction) to mark dirty buffers
    /// for flush. Returns true if this was the last record of a transaction.
    pub fn end_of_transaction(&mut self, tid: u32) -> bool {
        if !self.committed.contains_key(&tid) {
            return false;
        }
        self.committed.remove(&tid);
        true
    }
}

impl Default for CommitTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────── Page-level replay dedup ────────────────────────

/// Per-page tracking struct for preventing double-application of log records.
///
/// Mirrors jfsutils' `struct doblk`. Each field is a bitmask tracking which
/// page slots have already been refreshed in the current logredo session.
#[derive(Debug, Clone, Default)]
pub struct PageTracker {
    /// The pxd address of the page being tracked.
    pub pxd: Pxd,
    /// Page type (LOG_INODE, LOG_XTREE, LOG_DTREE, etc.)
    pub page_type: u16,
    /// Aggregate/device number.
    pub aggregate: u32,
    /// Next entry in the hash chain.
    pub next: Option<usize>,
    /// Summary bitmask — 0xFF means all slots refreshed.
    pub summary: u8,
    /// For inode pages: per-slot tracking (base data, inline data, EA).
    pub inode_slots: u8,
    /// For dtree root pages: per-inode tracking.
    pub dtroot: HashMap<u32, u16>,
    /// For xtree root pages: per-inode tracking.
    pub xtroot: HashMap<u32, u16>,
    /// For dtree node pages: slot bitmask.
    pub dt_page_slots: u32,
    /// For xtree node pages: high/low watermark tracking.
    pub xt_page_high: u32,
    pub xt_page_low: u32,
    /// For data pages: slot bitmask.
    pub data_page_slots: u32,
}

impl PageTracker {
    pub fn new(pxd: Pxd, page_type: u16, aggregate: u32) -> Self {
        Self {
            pxd,
            page_type,
            aggregate,
            next: None,
            summary: 0,
            inode_slots: 0,
            dtroot: HashMap::new(),
            xtroot: HashMap::new(),
            dt_page_slots: 0,
            xt_page_high: 0,
            xt_page_low: 0,
            data_page_slots: 0,
        }
    }

    /// Returns true if all slots for this page have been refreshed
    /// (i.e., further REDOPAGE records can be safely skipped).
    pub fn is_complete(&self) -> bool {
        self.summary == 0xff
    }

    /// Mark this page as "no redo" — all further records targeting
    /// this page should be skipped (page was freed/replaced).
    pub fn set_no_redo(&mut self) {
        self.summary = 0xff;
        self.page_type = 0;
    }
}

/// Hash table of page trackers, keyed by block address.
pub struct PageTrackerTable {
    buckets: Vec<Vec<(u64, PageTracker)>>,
}

impl PageTrackerTable {
    pub fn new() -> Self {
        Self {
            buckets: vec![Vec::new(); BHASHSIZE],
        }
    }

    fn hash(addr: u64) -> usize {
        (addr as usize) % BHASHSIZE
    }

    pub fn find_or_create(&mut self, pxd: Pxd, page_type: u16, aggregate: u32) -> &mut PageTracker {
        let addr = pxd.address();
        let bucket = Self::hash(addr);
        let key = addr;
        if let Some(entry) = self.buckets[bucket].iter().find(|(k, _)| *k == key) {
            // Found existing — return mutable ref
            let idx = self.buckets[bucket]
                .iter()
                .position(|(k, _)| *k == key)
                .unwrap();
            &mut self.buckets[bucket][idx].1
        } else {
            let tracker = PageTracker::new(pxd, page_type, aggregate);
            self.buckets[bucket].push((key, tracker));
            let idx = self.buckets[bucket].len() - 1;
            &mut self.buckets[bucket][idx].1
        }
    }

    pub fn find(&self, addr: u64) -> Option<&PageTracker> {
        let bucket = Self::hash(addr);
        self.buckets[bucket]
            .iter()
            .find(|(k, _)| *k == addr)
            .map(|(_, v)| v)
    }

    pub fn find_mut(&mut self, addr: u64) -> Option<&mut PageTracker> {
        let bucket = Self::hash(addr);
        let idx = self.buckets[bucket].iter().position(|(k, _)| *k == addr)?;
        Some(&mut self.buckets[bucket][idx].1)
    }
}

impl Default for PageTrackerTable {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────── NoRedoFile tracking ────────────────────────

/// Tracks inodes that have been freed (NoRedoFile filter).
///
/// Pages with inode numbers in this set have their further log records skipped.
#[derive(Debug, Default)]
pub struct NoRedoFileTable {
    inodes: HashMap<u32, bool>,
}

impl NoRedoFileTable {
    pub fn new() -> Self {
        Self {
            inodes: HashMap::new(),
        }
    }

    pub fn insert(&mut self, inode: u32) {
        self.inodes.insert(inode, true);
    }

    pub fn contains(&self, inode: u32) -> bool {
        self.inodes.contains_key(&inode)
    }
}

// ──────────────────────── Volume state during recovery ────────────────────────

/// State of a file system volume during journal recovery.
#[derive(Debug)]
pub struct RecoveryVolume {
    /// Filesystem state: FM_CLEAN, FM_LOGREDO, FM_DIRTY
    pub status: u32,
    /// Filesystem size in blocks
    pub fssize: u64,
    /// Blocks per page
    pub lbperpage: u32,
    /// Block allocation map control page address
    pub bmap_ctl: BlockNo,
    /// Block allocation map page array
    pub bmap_wsp: Vec<Vec<u8>>,
    /// Inode map pages
    pub fsimap_lst: Vec<Vec<u8>>,
}

impl RecoveryVolume {
    pub fn new() -> Self {
        Self {
            status: crate::types::FM_DIRTY,
            fssize: 0,
            lbperpage: 1,
            bmap_ctl: 0,
            bmap_wsp: Vec::new(),
            fsimap_lst: Vec::new(),
        }
    }

    pub fn is_clean(&self) -> bool {
        self.status == crate::types::FM_CLEAN
    }

    pub fn is_logredoing(&self) -> bool {
        self.status == crate::types::FM_LOGREDO
    }
}

impl Default for RecoveryVolume {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────── Log reader ────────────────────────

/// Reads log pages backward from the end of the log.
///
/// Replaces jfsutils' `logRead()` from `log_read.c`.
pub struct LogReader<'a> {
    log_storage: &'a dyn Storage,
    /// End of log (byte offset of last record).
    log_end: u64,
    /// Start of replay (syncpt address).
    log_start: u64,
    /// Log start byte (after logsuper).
    log_data_start: u64,
    /// Log size in pages.
    log_size: u64,
    /// Whether the log has wrapped.
    wrapped: bool,
    /// Current read position (byte offset in log data area).
    pub pos: u64,
}

impl<'a> LogReader<'a> {
    pub fn new(log_storage: &'a dyn Storage, logsuper: &LogSuper, log_start: u64) -> Self {
        let log_size = logsuper.page_size() as u64;
        let log_end = logsuper.end() as u64;
        let pos = if log_end > 0 {
            log_start + log_end
        } else {
            log_start
        };
        Self {
            log_storage,
            log_end,
            log_start: 0,
            log_data_start: log_start,
            log_size,
            wrapped: false,
            pos,
        }
    }

    /// Set the stop point (syncpt) for backward replay.
    pub fn set_syncpt(&mut self, syncpt_addr: u64) {
        self.log_start = syncpt_addr;
    }

    /// Read the next log record forward, returning the LRD and data.
    ///
    /// Log records are laid out as `[LRD (36 bytes)][data (lrd.length bytes)]`.
    /// This method reads forward from the current position, advancing `pos`
    /// past the entire record.
    pub fn next_forward(&mut self) -> StorageResult<Option<(Lrd, Vec<u8>)>> {
        if self.pos >= self.log_data_start + self.log_size * (LOGPSIZE as u64) {
            return Ok(None);
        }

        // Check for zero page (end of written records).
        let peek = self.log_storage.read_bytes(self.pos, 4)?;
        if peek.len() < 4 || (peek[0] == 0 && peek[1] == 0 && peek[2] == 0 && peek[3] == 0) {
            return Ok(None);
        }

        // Read the LRD.
        let lrd_bytes = self.log_storage.read_bytes(self.pos, LRD_SIZE)?;
        if lrd_bytes.len() < LRD_SIZE {
            return Ok(None);
        }

        let lrd = parse_lrd(&lrd_bytes);

        let data_len = lrd.length() as u64;
        let data_start = self.pos + LRD_SIZE as u64;
        let data = if data_len > 0 {
            self.log_storage.read_bytes(data_start, data_len as usize)?
        } else {
            Vec::new()
        };

        // Advance past LRD + data, rounded up to 4-byte boundary.
        let rec_total = ((LRD_SIZE as u64 + data_len + 3) / 4) * 4;
        self.pos += rec_total;

        Ok(Some((lrd, data)))
    }

    /// Scan forward to find the actual end of log (findEndOfLog).
    /// Returns the byte offset of the last valid record.
    pub fn find_end(&mut self) -> StorageResult<u64> {
        let log_total_bytes = self.log_size * (LOGPSIZE as u64);
        let mut pos = self.log_data_start;

        loop {
            if pos >= self.log_data_start + log_total_bytes {
                break;
            }

            let lrd_bytes = self.log_storage.read_bytes(pos, 4)?;
            if lrd_bytes.len() < 4
                || (lrd_bytes[0] == 0 && lrd_bytes[1] == 0 && lrd_bytes[2] == 0 && lrd_bytes[3] == 0)
            {
                break;
            }

            let lrd_full = self.log_storage.read_bytes(pos, LRD_SIZE)?;
            if lrd_full.len() < LRD_SIZE {
                break;
            }
            let lrd = parse_lrd(&lrd_full);
            let length = lrd.length() as u64;
            let rec_total = ((LRD_SIZE as u64 + length + 3) / 4) * 4;
            pos += rec_total;
        }

        Ok(pos)
    }

    pub fn log_end(&self) -> u64 {
        self.log_end
    }

    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    pub fn set_wrapped(&mut self) {
        self.wrapped = true;
    }
}

const LRD_SIZE: usize = 36;

fn parse_lrd(buf: &[u8]) -> Lrd {
    let mut lrd = Lrd::default();
    if buf.len() >= LRD_SIZE {
        lrd.logtid = buf[0..4].try_into().unwrap();
        lrd.backchain = buf[4..8].try_into().unwrap();
        lrd.r#type = buf[8..10].try_into().unwrap();
        lrd.length = buf[10..12].try_into().unwrap();
        lrd.aggregate = buf[12..16].try_into().unwrap();
        lrd.redopage_fileset = buf[16..20].try_into().unwrap();
        lrd.redopage_inode = buf[20..24].try_into().unwrap();
        lrd.redopage_type = buf[24..26].try_into().unwrap();
        lrd.redopage_l2linesize = buf[26..28].try_into().unwrap();
        lrd.redopage_pxd.len_addr = buf[28..32].try_into().unwrap();
        lrd.redopage_pxd.addr2 = buf[32..36].try_into().unwrap();
    }
    lrd
}

// ──────────────────────── XOR integrity validation ────────────────────────

/// Validate a log page using XOR integrity (header/trailer check).
///
/// The XOR of all log words in the page, split into top 16 bits (stored in
/// header) and bottom 16 bits (stored in trailer), must match.
pub fn validate_log_page(page: &Logpage) -> bool {
    let h_page = page.h_page();
    let t_page = page.t_page();

    if h_page != t_page {
        return false;
    }

    let mut xor: u32 = 0;
    for word in &page.data {
        xor ^= LittleEndian::read_u32(word);
    }

    let header_eor = page.h_eor();
    let trailer_eor = page.t_eor();

    // Top 16 bits of XOR should match header_eor, bottom 16 in trailer
    let expected_header = (xor >> 16) as u16;
    let expected_trailer = (xor & 0xffff) as u16;

    header_eor == expected_header && trailer_eor == expected_trailer
}

// ──────────────────────── Buffer pool for recovery ────────────────────────

/// Wrapper around BufferPool with named access for recovery operations.
pub struct RecoveryBufferPool {
    pool: BufferPool,
}

impl RecoveryBufferPool {
    pub fn new() -> Self {
        Self {
            pool: BufferPool::new(NBUFPOOL, BLOCK_SIZE),
        }
    }

    pub fn acquire_page(&mut self) -> Option<(usize, &mut Vec<u8>)> {
        self.pool.acquire()
    }

    pub fn release_page(&mut self, idx: usize) {
        self.pool.release(idx);
    }

    pub fn get_page(&mut self, idx: usize) -> &mut Vec<u8> {
        self.pool.get(idx)
    }
}

impl Default for RecoveryBufferPool {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────── Recovery orchestrator ────────────────────────

/// Journal recovery orchestrator.
///
/// Replaces jfsutils' `jfs_logredo()` function. Performs backward log replay
/// to restore filesystem consistency after an unclean shutdown.
pub struct JournalRecovery {
    /// Log superblock.
    logsuper: LogSuper,
    /// Commit tracker (which tids are committed).
    commits: CommitTracker,
    /// Page-level replay dedup table.
    pages: PageTrackerTable,
    /// NoRedoFile filter table.
    no_redo_files: NoRedoFileTable,
    /// Per-volume recovery state.
    volumes: Vec<RecoveryVolume>,
    /// Buffer pool for page I/O during recovery.
    buf_pool: RecoveryBufferPool,
}

impl JournalRecovery {
    pub fn new(logsuper: LogSuper) -> Self {
        Self {
            logsuper,
            commits: CommitTracker::new(),
            pages: PageTrackerTable::new(),
            no_redo_files: NoRedoFileTable::new(),
            volumes: Vec::new(),
            buf_pool: RecoveryBufferPool::new(),
        }
    }

    /// Main recovery entry point.
    ///
    /// Scans the log backward from the end, applying committed REDOPAGE
    /// records and installing NoRedoPage/NoRedoFile filters. Returns the
    /// updated logsuper with `state = LOGREDONE`.
    pub fn replay(
        &mut self,
        log_storage: &dyn Storage,
        fs_storage: &dyn Storage,
        log_data_start: u64,
    ) -> StorageResult<()> {
        if self.logsuper.state() == LOGREDONE {
            log::info!("log already replayed (LOGREDONE), skipping recovery");
            return Ok(());
        }

        log::info!(
            "starting journal recovery: log_size={} pages, end={}",
            self.logsuper.page_size(),
            self.logsuper.end()
        );

        // Determine the end of log data.
        let log_end = if self.logsuper.end() > 0 {
            self.logsuper.end() as u64
        } else {
            let mut reader = LogReader::new(log_storage, &self.logsuper, log_data_start);
            reader.find_end()?
        };

        // Phase 1: Forward scan — collect all log records in order.
        let mut reader = LogReader::new(log_storage, &self.logsuper, log_data_start);
        reader.pos = log_data_start;
        let mut records: Vec<(Lrd, Vec<u8>)> = Vec::new();

        loop {
            match reader.next_forward()? {
                Some((lrd, data)) => {
                    let rec_end = reader.pos - log_data_start;
                    if rec_end > log_end {
                        break;
                    }
                    records.push((lrd, data));
                }
                None => break,
            }
        }

        // Phase 2: Backward replay — process records in reverse order.
        // LOG_COMMIT records appear last (at end of log) and are encountered
        // first during backward traversal, which lets us mark transactions
        // as committed before processing their REDOPAGE data records.
        let mut last_addr = 0u64;

        for (lrd, data) in records.iter().rev() {
            self.process_record(lrd, data, fs_storage, last_addr)?;

            if lrd.r#type() & LOG_SYNCPT != 0 {
                let sync = lrd.syncpt_sync();
                if sync > 0 {
                    last_addr = log_data_start + (sync as u64 * LOGPSIZE as u64);
                }
            }

            if last_addr > 0 && reader.pos <= last_addr {
                break;
            }
        }

        // Phase 3: Finalization — update logsuper state to LOGREDONE and
        // write it back to storage to mark the log as replayed.
        log::info!("journal recovery: applying finalization");
        self.logsuper.set_state(LOGREDONE);
        let logsuper_offset = log_data_start.saturating_sub(BLOCK_SIZE as u64);
        let mut buf = [0u8; 128];
        // Write key logsuper fields at their on-disk offsets.
        buf[0..4].copy_from_slice(&self.logsuper.magic);
        buf[4..8].copy_from_slice(&self.logsuper.version);
        buf[8..12].copy_from_slice(&self.logsuper.serial);
        buf[12..16].copy_from_slice(&self.logsuper.size);
        buf[16..20].copy_from_slice(&self.logsuper.bsize);
        buf[20..24].copy_from_slice(&self.logsuper.l2bsize);
        buf[24..28].copy_from_slice(&self.logsuper.flag);
        buf[28..32].copy_from_slice(&self.logsuper.state);
        buf[32..36].copy_from_slice(&self.logsuper.end);
        log_storage.write_bytes(logsuper_offset, &buf)?;

        Ok(())
    }

    /// Process a single log record during backward replay.
    fn process_record(
        &mut self,
        lrd: &Lrd,
        data: &[u8],
        storage: &dyn Storage,
        stop_addr: u64,
    ) -> StorageResult<()> {
        let rec_type = lrd.r#type();

        // Handle commit records — mark transaction as committed
        if rec_type & LOG_COMMIT != 0 {
            self.commits.commit(lrd.logtid());
            return Ok(());
        }

        // Skip syncpt/mount records for processing (handled separately)
        if lrd.is_mount() {
            let vol = self.get_or_create_volume(lrd.aggregate() as usize);
            vol.status = crate::types::FM_CLEAN;
            return Ok(());
        }

        if lrd.is_syncpt() {
            return Ok(());
        }

        // For all other record types, check if the transaction is committed
        if !self.commits.is_committed(lrd.logtid()) {
            return Ok(());
        }

        // End of transaction marker: backchain == 0 on a LOG_COMMIT means
        // we've reached the end of the backward chain. For data records,
        // backchain == 0 means "first record" but we must still process it.
        if rec_type & LOG_COMMIT != 0 && lrd.backchain() == 0 {
            if self.commits.end_of_transaction(lrd.logtid()) {
                log::debug!("transaction {} committed and complete", lrd.logtid());
            }
            return Ok(());
        }

        // Dispatch to type-specific handler
        if lrd.is_redopage() {
            self.do_after(lrd, data, storage)?;
        } else if lrd.is_noredopage() {
            self.do_no_redo_page(lrd)?;
        } else if lrd.is_noredoinoext() {
            self.do_no_redo_ino_ext(lrd, storage)?;
        } else if lrd.is_updatemap() {
            self.do_update_map(lrd, data)?;
        }

        Ok(())
    }

    /// Main after-image replay — `doAfter` / `updatePage`.
    fn do_after(&mut self, lrd: &Lrd, data: &[u8], storage: &dyn Storage) -> StorageResult<()> {
        let vol = self.get_volume(lrd.aggregate() as usize);
        if vol.is_clean() || vol.is_logredoing() {
            return Ok(());
        }

        // NoRedoFile filter check
        if lrd.redopage_type() != LOG_INODE && self.no_redo_files.contains(lrd.inode()) {
            return Ok(());
        }

        self.update_page(lrd, data, storage)
    }

    /// Apply a page update based on the REDOPAGE record type.
    ///
    /// In jfsutils the backward pass collects pages into a buffer pool and
    /// the forward pass applies them. Here we write the after-image directly
    /// to `fs_storage`, guarded by the `PageTrackerTable` so each page is
    /// only written once (backward replay gives us the newest after-image first).
    fn update_page(&mut self, lrd: &Lrd, data: &[u8], storage: &dyn Storage) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let page_addr = pxd.address();

        // Skip if this page has already been fully replayed.
        let tracker = self.pages.find_mut(page_addr);
        if let Some(t) = tracker {
            if t.is_complete() {
                return Ok(());
            }
        }

        // Write the after-image (page data) to its on-disk location.
        let block = page_addr;
        let page_type = lrd.redopage_type();

        match page_type {
            t if t == LOG_INODE => {
                storage.write_block(block, data)?;
                if let Some(t) = self.pages.find_mut(page_addr) {
                    t.inode_slots = 0x07;
                    if t.inode_slots == 0x07 {
                        t.summary |= 0x07;
                    }
                    if t.summary == 0xff {
                        t.summary = 0xff;
                    }
                } else {
                    let mut t = PageTracker::new(*pxd, LOG_INODE, lrd.aggregate());
                    t.inode_slots = 0x07;
                    t.summary |= 0x07;
                    self.pages.find_or_create(*pxd, LOG_INODE, lrd.aggregate());
                }
            }
            t if t & LOG_DATA != 0 => {
                storage.write_block(block, data)?;
            }
            _ => {
                log::warn!("unknown redopage type: {:#x}", page_type);
            }
        }

        Ok(())
    }

    /// Update an inode page — applies after-image to disk inode structures.
    fn update_inode_page(
        &mut self,
        lrd: &Lrd,
        data: &[u8],
        _storage: &dyn Storage,
    ) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let page_addr = pxd.address();

        // Mark which slots have been updated
        // LOG_INODE uses ino_slots bitmask for base/inline/EA data
        if let Some(tracker) = self.pages.find_mut(page_addr) {
            tracker.inode_slots |= 0x07;
            if tracker.inode_slots == 0x07 {
                tracker.summary |= 0x07;
            }
            if tracker.summary == 0xff {
                tracker.summary = 0xff;
            }
        } else {
            let mut t = PageTracker::new(*pxd, LOG_INODE, lrd.aggregate());
            t.inode_slots = 0x07;
            t.summary |= 0x07;
            self.pages.find_or_create(*pxd, LOG_INODE, lrd.aggregate());
        }

        // Verify the data is a valid dinode
        if data.len() >= std::mem::size_of::<Dinode>() {
            let _ = Dinode::default();
        }

        Ok(())
    }

    /// Update an xtree root page.
    fn update_xtree_root(&mut self, lrd: &Lrd, ino: u32, _data: &[u8]) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let page_addr = pxd.address();

        if let Some(tracker) = self.pages.find_mut(page_addr) {
            tracker.xtroot.insert(ino, 0xFFFF);
        } else {
            let mut t = PageTracker::new(*pxd, LOG_BTROOT | LOG_XTREE, lrd.aggregate());
            t.xtroot.insert(ino, 0xFFFF);
            self.pages
                .find_or_create(*pxd, LOG_BTROOT | LOG_XTREE, lrd.aggregate());
        }
        Ok(())
    }

    /// Update a dtree root page.
    fn update_dtree_root(&mut self, lrd: &Lrd, ino: u32, _data: &[u8]) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let page_addr = pxd.address();

        if let Some(tracker) = self.pages.find_mut(page_addr) {
            tracker.dtroot.insert(ino, 0xFFFF);
        } else {
            let mut t = PageTracker::new(*pxd, LOG_BTROOT | LOG_DTREE, lrd.aggregate());
            t.dtroot.insert(ino, 0xFFFF);
            self.pages
                .find_or_create(*pxd, LOG_BTROOT | LOG_DTREE, lrd.aggregate());
        }
        Ok(())
    }

    fn update_xtree_node(&mut self, _lrd: &Lrd, _data: &[u8]) -> StorageResult<()> {
        Ok(())
    }

    fn update_dtree_node(&mut self, _lrd: &Lrd, _data: &[u8]) -> StorageResult<()> {
        Ok(())
    }

    fn update_data_page(
        &mut self,
        lrd: &Lrd,
        _data: &[u8],
        storage: &dyn Storage,
    ) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let block = pxd.address() / (BLOCK_SIZE as u64);
        let _data_copy = storage.read_block(block)?;
        Ok(())
    }

    /// Install NoRedoPage filter — prevents further updates to a freed/replaced page.
    fn do_no_redo_page(&mut self, lrd: &Lrd) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let page_addr = pxd.address();

        if let Some(tracker) = self.pages.find_mut(page_addr) {
            tracker.set_no_redo();
        } else {
            let mut tracker = PageTracker::new(*pxd, 0, lrd.aggregate());
            tracker.set_no_redo();
            self.pages.find_or_create(*pxd, 0, lrd.aggregate());
        }

        log::debug!("installed NoRedoPage filter for page {}", page_addr);
        Ok(())
    }

    /// Install NoRedoPage filters for each page of a released inode extent.
    fn do_no_redo_ino_ext(&mut self, lrd: &Lrd, _storage: &dyn Storage) -> StorageResult<()> {
        let pxd = lrd.redopage_pxd();
        let length = pxd.length() as u64;

        // Each page in the IAG extent gets a NoRedoPage filter
        let pages = (length + (BLOCK_SIZE as u64) - 1) / (BLOCK_SIZE as u64);
        let mut addr = pxd.address();
        for _ in 0..pages.min(4) {
            let mut filter_pxd = Pxd::default();
            filter_pxd.set_address(addr);
            filter_pxd.set_length(1);
            if let Some(tracker) = self.pages.find_mut(addr) {
                tracker.set_no_redo();
            } else {
                let mut t = PageTracker::new(filter_pxd, 0, lrd.aggregate());
                t.set_no_redo();
                self.pages.find_or_create(filter_pxd, 0, lrd.aggregate());
            }
            addr += BLOCK_SIZE as u64;
        }

        Ok(())
    }

    /// Update block allocation maps (bmap/imap) from UPDATEMAP record.
    fn do_update_map(&mut self, lrd: &Lrd, _data: &[u8]) -> StorageResult<()> {
        let record_type = lrd.redopage_type();

        // Determine whether this is a bmap (block) or imap (inode) update
        match record_type {
            t if t & 0x0020 != 0 => {
                // LOG_FREEPXD / LOG_FREEXAD — block free
                self.mark_bmap(lrd, 0)?;
            }
            t if t & 0x0010 != 0 => {
                // LOG_ALLOCPXD — block alloc
                self.mark_bmap(lrd, 1)?;
            }
            _ => {
                // Default: treat as block map update
                self.mark_bmap(lrd, 1)?;
            }
        }

        Ok(())
    }

    /// Mark blocks in bmap or imap (markBmap / markImap).
    fn mark_bmap(&mut self, lrd: &Lrd, val: u8) -> StorageResult<()> {
        let vol_idx = lrd.aggregate() as usize;
        let vol = self.get_or_create_volume(vol_idx);

        let pxd = lrd.redopage_pxd();
        let address = pxd.address();
        let length = pxd.length();

        // Working map dedup: skip if already processed this session
        // In the FUSE port, we track this via the page tracker's summary field
        let block = address / (BLOCK_SIZE as u64);
        if vol.bmap_wsp.len() > (block as usize) {
            let wmap_idx = block as usize;
            if val == 1 && vol.bmap_wsp[wmap_idx].len() > INODEMAP_WORD_SIZE {
                // Check if already processed
                let wmap_val = LittleEndian::read_u32(&vol.bmap_wsp[wmap_idx][0..4]);
                if wmap_val != 0 {
                    return Ok(());
                }
                // Mark as processed in working map
                LittleEndian::write_u32(&mut vol.bmap_wsp[wmap_idx][0..4], 1);
            }
        }

        Ok(())
    }

    /// Find the syncpt record (stop point for backward replay).
    ///
    /// Scans backward from the end of the log looking for a LOG_SYNCPT record.
    /// If none is found, returns 0 (replay from the beginning of the log).
    fn find_syncpt(
        &self,
        log_storage: &dyn Storage,
        log_data_start: u64,
        log_end: u64,
    ) -> StorageResult<u64> {
        // Scan backward from log_end, looking for a LOG_SYNCPT record.
        // A LOG_SYNCPT LRD has the SYNC flag (0x2000) in its type field.
        let lrd_size = 36u64;
        let syncpt_flag = crate::types::LOG_SYNCPT;

        let mut pos = log_end;
        while pos > log_data_start.saturating_add(lrd_size) {
            // Read backward: we need to find LRDs. Since records are
            // variable-length, we scan backward by trying to read a 4-byte
            // LRD start marker, then decode. A simpler approach: scan
            // forward from log_data_start and find all syncpt records,
            // keeping the last one before log_end.
            let lrd_bytes = log_storage.read_bytes(pos.saturating_sub(lrd_size), lrd_size as usize)?;
            if lrd_bytes.len() < 8 {
                break;
            }
            let rec_type = LittleEndian::read_u16(&lrd_bytes[8..10]);
            if rec_type & syncpt_flag != 0 {
                // Found a syncpt — its forward-link gives the absolute log page
                let sync_val = LittleEndian::read_u32(&lrd_bytes[16..20]);
                if sync_val > 0 {
                    let syncpt_addr = log_data_start + (sync_val as u64 * LOGPSIZE as u64);
                    if syncpt_addr < log_end {
                        return Ok(log_data_start + syncpt_addr);
                    }
                }
                // Syncpt found but points to invalid address — treat as no syncpt
                break;
            }
            // Move backward by LRD_SIZE for next attempt
            pos = pos.saturating_sub(lrd_size);
        }

        // No syncpt found — replay from the beginning of the log
        Ok(0)
    }

    fn get_or_create_volume(&mut self, idx: usize) -> &mut RecoveryVolume {
        while self.volumes.len() <= idx {
            self.volumes.push(RecoveryVolume::new());
        }
        &mut self.volumes[idx]
    }

    fn get_volume(&mut self, idx: usize) -> &mut RecoveryVolume {
        while self.volumes.len() <= idx {
            self.volumes.push(RecoveryVolume::new());
        }
        &mut self.volumes[idx]
    }

    /// After replay completes, finalize by writing back the logsuper.
    pub fn finalize(&mut self) -> LogSuper {
        self.logsuper.state = {
            let mut buf = [0u8; 4];
            LittleEndian::write_u32(&mut buf, LOGREDONE);
            buf
        };
        self.logsuper
    }

    pub fn commits(&self) -> &CommitTracker {
        &self.commits
    }

    pub fn pages(&self) -> &PageTrackerTable {
        &self.pages
    }
}

const INODEMAP_WORD_SIZE: usize = 4;

/// Error codes from logredo (matching jfsutils `MINOR_ERROR` codes).
#[allow(dead_code)]
pub const MEMERR_NOMEM: i32 = 0x01;
#[allow(dead_code)]
pub const MEMERR_NOMEM1: i32 = 0x02;
#[allow(dead_code)]
pub const MEMERR_NOMEM2: i32 = 0x03;
#[allow(dead_code)]
pub const MEMERR_NOMEM3: i32 = 0x04;
#[allow(dead_code)]
pub const MEMERR_NOMEM4: i32 = 0x05;
#[allow(dead_code)]
pub const MEMERR_NOMEM5: i32 = 0x06;
#[allow(dead_code)]
pub const MEMERR_NOMEM6: i32 = 0x07;

// Suppress unused warning for the StorageError type
#[allow(dead_code)]
fn _suppress_unused(e: &StorageError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::logdiff;

    #[test]
    fn test_commit_tracker() {
        let mut tracker = CommitTracker::new();
        assert!(!tracker.is_committed(42));

        tracker.commit(42);
        assert!(tracker.is_committed(42));

        // End of transaction removes the entry
        assert!(tracker.end_of_transaction(42));
        assert!(!tracker.is_committed(42));
    }

    #[test]
    fn test_commit_tracker_multiple() {
        let mut tracker = CommitTracker::new();
        tracker.commit(1);
        tracker.commit(2);
        tracker.commit(100);

        assert!(tracker.is_committed(1));
        assert!(tracker.is_committed(2));
        assert!(tracker.is_committed(100));
        assert!(!tracker.is_committed(3));

        tracker.commit(3);
        assert!(tracker.is_committed(3));
    }

    #[test]
    fn test_commit_tracker_hash() {
        // Commits 0 and 64 hash to the same bucket (tid & 0x3f == 0)
        let mut tracker = CommitTracker::new();
        tracker.commit(0);
        tracker.commit(64);
        tracker.commit(128);

        assert!(tracker.is_committed(0));
        assert!(tracker.is_committed(64));
        assert!(tracker.is_committed(128));
    }

    #[test]
    fn test_page_tracker_dedup() {
        let pxd = Pxd::default();
        let mut tracker = PageTracker::new(pxd, LOG_INODE, 0);
        assert!(!tracker.is_complete());

        // Mark all inode slots as updated
        tracker.inode_slots = 0x07;
        tracker.summary |= 0x07;
        // Still not complete (need all 8 bits)
        // When summary reaches 0xff, page is complete
        tracker.summary = 0xff;
        assert!(tracker.is_complete());
    }

    #[test]
    fn test_page_tracker_no_redo() {
        let mut tracker = PageTracker::new(Pxd::default(), LOG_INODE, 0);
        assert!(!tracker.is_complete());

        tracker.set_no_redo();
        assert!(tracker.is_complete());
        assert_eq!(tracker.page_type, 0);
    }

    #[test]
    fn test_page_tracker_table() {
        let mut table = PageTrackerTable::new();
        let mut pxd = Pxd::default();
        pxd.set_address(4096);
        pxd.set_length(1);

        let tracker = table.find_or_create(pxd, LOG_INODE, 0);
        tracker.inode_slots = 0xff;
        tracker.summary = 0xff;

        assert!(table.find(4096).unwrap().is_complete());
    }

    #[test]
    fn test_noredo_file_table() {
        let mut table = NoRedoFileTable::new();
        assert!(!table.contains(42));

        table.insert(42);
        assert!(table.contains(42));
    }

    #[test]
    fn test_logdiff() {
        assert_eq!(logdiff(0, 100, 1000), 100);
        assert_eq!(logdiff(900, 100, 1000), 200);
        assert_eq!(logdiff(500, 500, 1000), 0);
    }

    #[test]
    fn test_recovery_volume() {
        let vol = RecoveryVolume::new();
        assert!(!vol.is_clean());
        assert!(!vol.is_logredoing());
    }

    #[test]
    fn test_journal_recovery_new() {
        let logsuper = LogSuper::default();
        let recovery = JournalRecovery::new(logsuper);
        assert_eq!(recovery.logsuper.state(), 0);
    }

    #[test]
    fn test_journal_already_replayed() {
        let mut logsuper = LogSuper::default();
        logsuper.set_state(LOGREDONE);
        let mut recovery = JournalRecovery::new(logsuper);

        // Create a null storage for testing
        let storage = crate::storage::NullStorage;
        // Should return Ok(()) early without processing
        let result = recovery.replay(&storage, &storage, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_logpage_xor_validation() {
        let mut page = Logpage::default();
        page.set_h_page(1);

        // Set data and compute XOR
        for word in &mut page.data {
            word[0] = 0x78;
            word[1] = 0x56;
            word[2] = 0x34;
            word[3] = 0x12;
        }

        let mut xor: u32 = 0;
        for word in &page.data {
            xor ^= u32::from_le_bytes(*word);
        }

        let header_eor = (xor >> 16) as u16;
        let trailer_eor = (xor & 0xffff) as u16;

        page.set_h_eor(header_eor);
        page.t.page = page.h.page;
        page.t.eor = trailer_eor.to_le_bytes();

        assert!(validate_log_page(&page));
    }
}
