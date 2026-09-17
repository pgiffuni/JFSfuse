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

use crate::storage::{BLOCK_SIZE, FileStorage, Result as StorageResult, Storage};
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
    /// Buffered log page currently being filled.
    /// `None` when the page is empty or has been flushed.
    log_buffer: Vec<u8>,
    /// Number of log pages in the journal.
    log_pages: u32,
}

impl LogManager {
    pub fn new(storage: Arc<dyn Storage>, logsuper: LogSuper) -> Self {
        let inline = logsuper.flag().wrapping_shr(9) & 1 != 0;
        let log_pages = logsuper.page_size();
        Self {
            logsuper,
            storage,
            log_id: String::new(),
            inline,
            write_offset: 0,
            log_buffer: vec![0u8; LOGPSIZE],
            log_pages,
        }
    }

    /// Read the logsuper from the log device (block 1).
    pub fn read_super(storage: &dyn Storage) -> StorageResult<LogSuper> {
        let mut data = storage.read_bytes((BLOCK_SIZE as u64) * 1, 4096)?;
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
        let offset = (BLOCK_SIZE as u64) * 1
            + (BLOCK_SIZE as u64)
            + (page_num as u64) * (LOGPSIZE as u64);
        self.storage.read_bytes(offset, LOGPSIZE)
    }

    /// Write a log page at the given page index.
    pub fn write_page(&self, page_num: u32, data: &[u8]) -> StorageResult<()> {
        let offset =
            (BLOCK_SIZE as u64) * 1 + (BLOCK_SIZE as u64) + (page_num as u64) * (LOGPSIZE as u64);
        self.storage.write_bytes(offset, data)
    }

    /// Compute the byte offset of a log page.
    pub fn page_offset(&self, page_num: u32) -> u64 {
        (BLOCK_SIZE as u64) * 2 + (page_num as u64) * (LOGPSIZE as u64)
    }

    /// Byte offset of the start of log data (after logsuper).
    pub fn data_start(&self) -> u64 {
        (BLOCK_SIZE as u64) * 2
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

        let offset = BLOCK_SIZE as u64;
        self.storage.write_bytes(offset, &buf)?;
        Ok(())
    }

    /// Flush the log to disk.
    pub fn sync(&self) -> StorageResult<()> {
        self.storage.sync()
    }

    /// Flush the journal specifically.
    ///
    /// This writes any buffered log page to storage and then calls
    /// `flush_metadata()` on the underlying storage. The journal must be
    /// durable before any metadata blocks can be safely written.
    pub fn flush_journal(&mut self) -> StorageResult<()> {
        // Write the current log buffer page.
        if self.write_offset > 0 {
            let current_page = (self.write_offset / (LOGPSIZE as u64)) as u32;
            self.write_page(current_page, &self.log_buffer)?;
        }
        self.storage.flush_metadata()?;
        self.sync()?;

        // Update logsuper end-of-log pointer.
        let data_start = self.data_start();
        let end = data_start + self.write_offset;
        self.logsuper.set_end(self.write_offset as u32);
        let logsuper_clone = self.logsuper;
        self.write_super(&logsuper_clone)?;
        Ok(())
    }

    /// Append a log record for a metadata block update.
    ///
    /// Serializes an LRD (log record descriptor) followed by the page data,
    /// writing it into the current log page buffer. If the current page is
    /// full, it is flushed and a new page is started.
    pub fn append_log_record(
        &mut self,
        txid: u64,
        block: u64,
        data: &[u8],
    ) -> StorageResult<()> {
        use byteorder::{ByteOrder, LittleEndian};

        let page_size = LOGPSIZE as u64;
        let offset_in_page = self.write_offset % page_size;
        let record_total = 36 + data.len() as u64; // LRD (36) + data

        // If this record won't fit in the current page, flush and wrap.
        if offset_in_page + record_total > page_size {
            let current_page = (self.write_offset / page_size) as u32;
            self.write_page(current_page, &self.log_buffer)?;
            self.write_offset = ((current_page as u64) + 1) * page_size % self.log_size_bytes();
            if self.write_offset == 0 {
                self.write_offset = page_size; // start of first data page offset
            }
            self.log_buffer.fill(0);
        }

        let offset_in_page = self.write_offset % page_size;

        // Build the LRD.
        let mut lrd = crate::types::Lrd::default();
        // logtid: transaction ID
        LittleEndian::write_u32(&mut lrd.logtid, txid as u32);
        // backchain: 0 = last record in transaction
        LittleEndian::write_u32(&mut lrd.backchain, 0);
        // type: LOG_UPDATEMAP (0x0008) — metadata update record
        LittleEndian::write_u16(&mut lrd.r#type, crate::types::LOG_UPDATEMAP);
        // length: data payload size
        LittleEndian::write_u16(&mut lrd.length, data.len() as u16);
        // aggregate: 0 (single aggregate)
        LittleEndian::write_u32(&mut lrd.aggregate, 0);
        // redopage_inode: the target block number as a u32
        LittleEndian::write_u32(&mut lrd.redopage_inode, block as u32);

        // Write LRD into the buffer.
        let buf_off = offset_in_page as usize;
        self.log_buffer[buf_off..buf_off + 36].copy_from_slice(&lrd_bytes(&lrd));

        // Write data after the LRD.
        let data_off = buf_off + 36;
        self.log_buffer[data_off..data_off + data.len()].copy_from_slice(data);

        self.write_offset += record_total;

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
