// SPDX-License-Identifier: GPL-2.0-or-later
//! Journal log manager.
//!
//! Mirrors `jfs_logmgr.c` / `jfs_logmgr.h` — the kernel's log I/O layer.
//! Manages log pages, group commit, syncpt advancement, and log wrapping.
//!
//! For read-only FUSE operation, only log scanning for recovery is needed.
//! For write support, this module provides `append_log_record()`,
//! `flush_journal()`, and the ordered write ordering required by the
//! transaction manager.

use std::sync::Arc;

use crate::storage::{BLOCK_SIZE, Result as StorageResult, Storage};
use crate::types::{
    LOGMAGIC, LOGPSIZE, LOGREDONE, LOGVERSION, LOGWRAP, LogSuper, Logpage, MAX_ACTIVE,
};

/// Log page flags
pub const TBLK_LOG_START: u32 = 1;
pub const TBLK_LOG_TAIL: u32 = 2;

/// Log write flags
pub const LOG_WRITE_DIRTY: u32 = 0x01;

/// Journal (log) manager.
///
/// Manages the on-disk journal: logsuper, log pages, group commit.
/// For the FUSE port, this provides:
/// - Log scanning for recovery (read path)
/// - Log record appending (write path)
/// - Sync point management
///
/// The internal log buffer spans two log pages so that a single journal
/// record (LRD header + full metadata block data) — up to 36 + 4096 =
/// 4132 bytes — always fits without splitting across page boundaries.
pub struct LogManager {
    /// Log superblock.
    logsuper: LogSuper,
    /// Backing storage for the log device.
    storage: Arc<dyn Storage>,
    /// Log device path or identifier.
    log_id: String,
    /// Whether the log is inline (within the filesystem).
    inline: bool,
    /// Current write offset within the log data area (in bytes).
    write_offset: u64,
    /// Byte offset of the previous log record within the log data area.
    /// Used to set `backchain` in the next record. 0 = no previous record
    /// (first in a transaction). After a commit, reset to 0.
    prev_record_offset: u64,
    /// Buffered log data currently being filled. Spans two log pages so
    /// that a single LRD + 4096-byte metadata block fits without crossing
    /// a page boundary mid-record.
    log_buffer: Vec<u8>,
    /// Whether the log buffer has unflushed data.
    buffer_dirty: bool,
    /// Number of log pages in the journal.
    log_pages: u32,
    /// Byte offset where the log area begins within the backing storage.
    /// For an external log device this is 0; for an inline log it is the
    /// PXD address from the filesystem superblock.
    log_base: u64,
}

/// Effective write-buffer size: two log page sizes so a single record
/// (LRD + full block) always fits.
const LOG_BUFFER_SIZE: usize = LOGPSIZE * 2;

/// Log record header size in bytes.
const LRD_SIZE: usize = 36;

impl LogManager {
    pub fn new(storage: Arc<dyn Storage>, logsuper: LogSuper, log_base: u64) -> Self {
        let inline = logsuper.flag().wrapping_shr(9) & 1 != 0;
        let log_pages = logsuper.page_size();
        Self {
            logsuper,
            storage,
            log_id: String::new(),
            inline,
            write_offset: 0,
            prev_record_offset: 0,
            log_buffer: vec![0u8; LOG_BUFFER_SIZE],
            buffer_dirty: false,
            log_pages,
            log_base,
        }
    }

    /// Read the logsuper from the log device.
    ///
    /// `log_base` is the byte offset where the log area begins. The logsuper
    /// is stored at `log_base + BLOCK_SIZE` (after a reserved boot block).
    pub fn read_super(storage: &dyn Storage, log_base: u64) -> StorageResult<LogSuper> {
        let offset = log_base + (BLOCK_SIZE as u64);
        let data = storage.read_bytes(offset, BLOCK_SIZE)?;
        let mut logsuper = LogSuper::default();

        if data.len() >= 4 {
            logsuper.magic = [data[0], data[1], data[2], data[3]];
        }
        if data.len() >= 8 {
            logsuper.version = [data[4], data[5], data[6], data[7]];
        }
        if data.len() >= 12 {
            logsuper.serial = [data[8], data[9], data[10], data[11]];
        }
        if data.len() >= 16 {
            logsuper.size = [data[12], data[13], data[14], data[15]];
        }
        if data.len() >= 20 {
            logsuper.bsize = [data[16], data[17], data[18], data[19]];
        }
        if data.len() >= 24 {
            logsuper.l2bsize = [data[20], data[21], data[22], data[23]];
        }
        if data.len() >= 28 {
            logsuper.flag = [data[24], data[25], data[26], data[27]];
        }
        if data.len() >= 32 {
            logsuper.state = [data[28], data[29], data[30], data[31]];
        }
        if data.len() >= 36 {
            logsuper.end = [data[32], data[33], data[34], data[35]];
        }
        if data.len() >= 52 {
            logsuper.uuid = data[36..52].try_into().unwrap_or([0; 16]);
        }
        if data.len() >= 68 {
            logsuper.label = data[52..68].try_into().unwrap_or([0; 16]);
        }

        Ok(logsuper)
    }

    /// Validate the logsuper: magic, version, and size.
    pub fn validate(&self) -> bool {
        self.logsuper.magic_val() == LOGMAGIC && self.logsuper.version() == LOGVERSION
    }

    /// Check if the log needs replay (not LOGREDONE).
    pub fn needs_replay(&self) -> bool {
        self.logsuper.state() != LOGREDONE
    }

    /// Check if the log has wrapped.
    pub fn has_wrapped(&self) -> bool {
        self.logsuper.state() == LOGWRAP
    }

    /// Read a log page at the given page index (0-based, after logsuper).
    pub fn read_page(&self, page_num: u32) -> StorageResult<Vec<u8>> {
        let offset = self.log_base + (BLOCK_SIZE as u64) * 2 + (page_num as u64) * (LOGPSIZE as u64);
        self.storage.read_bytes(offset, LOGPSIZE)
    }

    /// Write a log page at the given page index.
    pub fn write_page(&self, page_num: u32, data: &[u8]) -> StorageResult<()> {
        let offset =
            self.log_base + (BLOCK_SIZE as u64) * 2 + (page_num as u64) * (LOGPSIZE as u64);
        self.storage.write_bytes(offset, data)
    }

    /// Compute the byte offset of a log page.
    pub fn page_offset(&self, page_num: u32) -> u64 {
        self.log_base + (BLOCK_SIZE as u64) * 2 + (page_num as u64) * (LOGPSIZE as u64)
    }

    /// Byte offset of the start of log data (after logsuper).
    pub fn data_start(&self) -> u64 {
        self.log_base + (BLOCK_SIZE as u64) * 2
    }

    /// Log page size.
    pub fn page_size(&self) -> usize {
        LOGPSIZE
    }

    /// Total log size in bytes.
    pub fn log_size_bytes(&self) -> u64 {
        self.logsuper.page_size() as u64 * (LOGPSIZE as u64)
    }

    /// Current end-of-log (byte offset within log data area).
    pub fn end(&self) -> u64 {
        self.logsuper.end() as u64
    }

    /// Write back the logsuper after recovery is complete.
    pub fn write_super(&mut self, super_block: &LogSuper) -> StorageResult<()> {
        let mut buf = vec![0u8; LOGPSIZE];
        buf[..4].copy_from_slice(&super_block.magic);
        buf[4..8].copy_from_slice(&super_block.version);
        buf[8..12].copy_from_slice(&super_block.serial);
        buf[12..16].copy_from_slice(&super_block.size);
        buf[16..20].copy_from_slice(&super_block.bsize);
        buf[20..24].copy_from_slice(&super_block.l2bsize);
        buf[24..28].copy_from_slice(&super_block.flag);
        buf[28..32].copy_from_slice(&super_block.state);
        buf[32..36].copy_from_slice(&super_block.end);
        buf[36..52].copy_from_slice(&super_block.uuid);
        buf[52..68].copy_from_slice(&super_block.label);

        let active_bytes = MAX_ACTIVE * 16;
        for (i, entry) in super_block.active.iter().enumerate() {
            let offset = 68 + i * 16;
            if offset + 16 <= buf.len() && offset + 16 <= 68 + active_bytes {
                buf[offset..offset + 16].copy_from_slice(&entry.uuid);
            }
        }

        let offset = self.log_base + BLOCK_SIZE as u64;
        self.storage.write_bytes(offset, &buf)?;
        Ok(())
    }

    /// Flush the log to disk.
    pub fn sync(&self) -> StorageResult<()> {
        self.storage.sync()
    }

    /// Flush the journal specifically.
    ///
    /// This writes any buffered log data to storage as one or more log pages
    /// and then calls `flush_metadata()` on the underlying storage. The
    /// journal must be durable before any metadata blocks can be safely
    /// written.
    pub fn flush_journal(&mut self) -> StorageResult<()> {
        // Write any buffered log data to log pages.
        if self.buffer_dirty && self.write_offset > 0 {
            let bytes_per_page = LOGPSIZE as u64;
            // The buffer holds one buffer-unit (LOG_BUFFER_SIZE bytes) of data.
            // Compute how much is actually in the current buffer.
            let buffer_start =
                self.write_offset - (self.write_offset % (LOG_BUFFER_SIZE as u64));
            let bytes_in_buffer = self.write_offset - buffer_start;
            // Number of pages (complete or partial) with buffered data.
            let dirty_pages = (bytes_in_buffer + bytes_per_page - 1) / bytes_per_page;
            // Starting log page number within the log data area.
            let start_page = (buffer_start / bytes_per_page) as u32;

            for i in 0..dirty_pages {
                let page_num = start_page + i as u32;
                let buf_start = (i * bytes_per_page) as usize;
                let buf_end = (buf_start + LOGPSIZE).min(self.log_buffer.len());
                let chunk = &self.log_buffer[buf_start..buf_end];
                self.write_page(page_num, chunk)?;
            }
        }
        self.storage.flush_metadata()?;
        self.sync()?;

        // Update logsuper end-of-log pointer.
        self.logsuper.set_end(self.write_offset as u32);
        let logsuper_clone = self.logsuper;
        self.write_super(&logsuper_clone)?;
        Ok(())
    }

    /// Append a log record for a metadata block update.
    ///
    /// Serializes an LRD (log record descriptor) followed by the page data,
    /// writing it into the current log buffer. Records within the same
    /// transaction are chained via `backchain`, which stores the byte
    /// offset of the previous record (0 for the first record).
    ///
    /// If the current buffer is full, it is flushed and a new buffer
    /// segment is started.
    pub fn append_log_record(
        &mut self,
        txid: u64,
        block: u64,
        data: &[u8],
    ) -> StorageResult<()> {
        use byteorder::{ByteOrder, LittleEndian};

        let buf_size = LOG_BUFFER_SIZE as u64;
        let offset_in_buf = self.write_offset % buf_size;
        let record_total = LRD_SIZE as u64 + data.len() as u64;

        // If this record won't fit in the current buffer, flush and advance.
        if offset_in_buf + record_total > buf_size {
            self.flush_journal()?;
            self.log_buffer.fill(0);
            self.buffer_dirty = false;
            // Align write_offset to the next buffer-unit boundary.
            self.write_offset =
                ((self.write_offset + buf_size - 1) / buf_size) * buf_size;
        }

        let offset_in_buf = self.write_offset % buf_size;

        // Build the LRD.
        let mut lrd = crate::types::Lrd::default();
        // logtid: transaction ID
        LittleEndian::write_u32(&mut lrd.logtid, txid as u32);
        // backchain: offset of previous record in this transaction (0 if first)
        LittleEndian::write_u32(&mut lrd.backchain, self.prev_record_offset as u32);
        // type: LOG_REDOPAGE (0x0800) — metadata after-image record
        LittleEndian::write_u16(&mut lrd.r#type, crate::types::LOG_REDOPAGE);
        // length: data payload size
        LittleEndian::write_u16(&mut lrd.length, data.len() as u16);
        // aggregate: 0 (single aggregate)
        LittleEndian::write_u32(&mut lrd.aggregate, 0);
        // redopage_type: LOG_INODE (inode metadata)
        LittleEndian::write_u16(&mut lrd.redopage_type, crate::types::LOG_INODE);
        // redopage_pxd: encode the block number as on-disk page location
        lrd.redopage_pxd.set_address(block);
        // Set length to 1 block (4KB / 4KB = 1 fsblock)
        lrd.redopage_pxd.set_length(1);

        // Write LRD into the buffer.
        let buf_off = offset_in_buf as usize;
        self.log_buffer[buf_off..buf_off + LRD_SIZE].copy_from_slice(&lrd_bytes(&lrd));
        self.buffer_dirty = true;

        // Write data after the LRD.
        let data_off = buf_off + LRD_SIZE;
        self.log_buffer[data_off..data_off + data.len()].copy_from_slice(data);

        // Record offset for backchain chaining.
        let record_offset = self.write_offset;
        self.write_offset += record_total;
        self.prev_record_offset = record_offset;

        Ok(())
    }

    /// Append a LOG_COMMIT record to end the current transaction.
    ///
    /// This writes a zero-length commit record with backchain pointing
    /// to the last data record (or 0 if no data records), then flushes
    /// the journal to durable storage.
    pub fn commit_transaction(&mut self, txid: u64) -> StorageResult<()> {
        use byteorder::{ByteOrder, LittleEndian};

        let buf_size = LOG_BUFFER_SIZE as u64;
        let offset_in_buf = self.write_offset % buf_size;

        // If the LRD won't fit, flush first.
        if offset_in_buf + LRD_SIZE as u64 > buf_size {
            self.flush_journal()?;
            self.log_buffer.fill(0);
            self.buffer_dirty = false;
            self.write_offset =
                ((self.write_offset + buf_size - 1) / buf_size) * buf_size;
        }

        let offset_in_buf = self.write_offset % buf_size;

        // Build the COMMIT LRD.
        let mut lrd = crate::types::Lrd::default();
        LittleEndian::write_u32(&mut lrd.logtid, txid as u32);
        // backchain: offset of last record, or 0 if this is the only record
        LittleEndian::write_u32(&mut lrd.backchain, self.prev_record_offset as u32);
        // type: LOG_COMMIT (0x8000)
        LittleEndian::write_u16(&mut lrd.r#type, crate::types::LOG_COMMIT);
        // length: 0 (no data for commit)
        LittleEndian::write_u16(&mut lrd.length, 0);
        // aggregate: 0
        LittleEndian::write_u32(&mut lrd.aggregate, 0);

        // Write LRD into the buffer.
        let buf_off = offset_in_buf as usize;
        self.log_buffer[buf_off..buf_off + LRD_SIZE].copy_from_slice(&lrd_bytes(&lrd));
        self.buffer_dirty = true;

        self.write_offset += LRD_SIZE as u64;

        // Flush everything to durable storage.
        self.flush_journal()?;

        // Reset backchain tracking for the next transaction.
        self.prev_record_offset = 0;

        Ok(())
    }

    pub fn logsuper(&self) -> &LogSuper {
        &self.logsuper
    }

    pub fn is_inline_log(&self) -> bool {
        self.inline
    }

    /// Current byte offset for log writes (within log data area).
    pub fn write_offset(&self) -> u64 {
        self.write_offset
    }
}

/// Serialize an `Lrd` to its 36-byte on-disk representation.
fn lrd_bytes(lrd: &crate::types::Lrd) -> [u8; 36] {
    let mut buf = [0u8; 36];
    buf[0..4].copy_from_slice(&lrd.logtid);
    buf[4..8].copy_from_slice(&lrd.backchain);
    buf[8..10].copy_from_slice(&lrd.r#type);
    buf[10..12].copy_from_slice(&lrd.length);
    buf[12..16].copy_from_slice(&lrd.aggregate);
    buf[16..20].copy_from_slice(&lrd.redopage_fileset);
    buf[20..24].copy_from_slice(&lrd.redopage_inode);
    buf[24..26].copy_from_slice(&lrd.redopage_type);
    buf[26..28].copy_from_slice(&lrd.redopage_l2linesize);
    buf[28..32].copy_from_slice(&lrd.redopage_pxd.len_addr);
    buf[32..36].copy_from_slice(&lrd.redopage_pxd.addr2);
    buf
}
