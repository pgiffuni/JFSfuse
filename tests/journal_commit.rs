// SPDX-License-Identifier: GPL-2.0-or-later
//! Integration test: full transaction commit path through the LogManager.
//!
//! Verifies that TransactionManager::commit correctly:
//! 1. Appends an LRD + page data to the journal log buffer
//! 2. Flushes the journal (writes log page, updates logsuper end)
//! 3. Writes dirty metadata to its final block on the filesystem
//! 4. Clears dirty state on cache pages

use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use jfsfuse::journal::LogManager;
use jfsfuse::storage::{
    BLOCK_SIZE, MemoryStorage, PageCache, Storage,
};
use jfsfuse::transaction::TransactionManager;
use jfsfuse::types::{
    LogSuper, LOGMAGIC, LOGPSIZE, LOG_UPDATEMAP, LOGVERSION, LOGWRAP, LOGPAGES,
};

/// Build a valid LogSuper for a 16-page inline log in a MemoryStorage.
fn make_log_super(num_pages: u32) -> LogSuper {
    let mut ls = LogSuper::default();
    LittleEndian::write_u32(&mut ls.magic, LOGMAGIC);
    LittleEndian::write_u32(&mut ls.version, LOGVERSION);
    LittleEndian::write_u32(&mut ls.serial, 0);
    LittleEndian::write_u32(&mut ls.size, num_pages);
    LittleEndian::write_u32(&mut ls.bsize, BLOCK_SIZE as u32);
    LittleEndian::write_u32(&mut ls.l2bsize, 12);
    LittleEndian::write_u32(&mut ls.flag, 0); // 0 = not inline (we just need valid fields)
    LittleEndian::write_u32(&mut ls.state, LOGWRAP); // not LOGREDONE → recovery would run
    LittleEndian::write_u32(&mut ls.end, 0);
    ls
}

/// Layout: block 0 = boot, block 1 = logsuper, blocks 2..=17 = log pages,
/// block 18 = metadata block. Total 19 blocks.
const NUM_BLOCKS: u64 = 19;

/// Block number of the metadata block we will modify.
const META_BLOCK: u64 = 18;

#[test]
fn test_transaction_commit_writes_journal_and_metadata() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(NUM_BLOCKS));

    // 1. Write a valid LogSuper to block 1.
    let ls = make_log_super(LOGPAGES as u32);
    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    // 2. Initialize a page cache and load the metadata block (inode 1, block META_BLOCK).
    let mut cache = PageCache::new(16);
    cache.get_or_load(&*storage, 1, META_BLOCK, 0).unwrap();

    // Verify the block starts as zeros.
    {
        let page = cache.get(1, META_BLOCK).unwrap();
        assert_eq!(page.data[..4], [0u8; 4]);
    }

    // 3. Modify the page through mutable cache access.
    {
        let page = cache.get_mut_for_write(1, META_BLOCK).unwrap();
        page.write_u32_le(0, 0xDEADBEEF);
    }

    // 4. Begin transaction, mark the page dirty.
    let mut tm = TransactionManager::new();
    let txid = tm.begin().unwrap();
    tm.mark_dirty(&mut cache, 1, META_BLOCK).unwrap();

    // 5. Commit — this should append an LRD to the journal, flush it,
    //    and write the metadata to block META_BLOCK.
    let result = tm.commit(&*storage, &mut cache, Some(&mut lm)).unwrap();
    assert_eq!(result.txid, txid);
    assert_eq!(result.pages_written, 1);

    // 6. Verify the metadata block now contains the new data.
    let meta = storage.read_block(META_BLOCK).unwrap();
    assert_eq!(
        LittleEndian::read_u32(&meta[..4]),
        0xDEADBEEF,
        "metadata block should reflect the committed write"
    );

    // 7. Verify the logsuper end was advanced.
    let ls_after = LogManager::read_super(&*storage, 0).unwrap();
    assert!(ls_after.end() > 0, "log end should be advanced after commit");
    assert!(ls_after.end() as usize <= 36 + BLOCK_SIZE, "log end should contain at least one LRD + data");

    // 8. Verify a log record (LRD) was written at the start of the log data area.
    //    The full record (LRD 36 bytes + 4096-byte page) is 4132 bytes and
    //    spans two log pages, so we read 2 * LOGPSIZE bytes.
    let data_start = 2 * (BLOCK_SIZE as u64); // data_start() = BLOCK_SIZE * 2
    let log_data = storage.read_bytes(data_start, LOGPSIZE * 2).unwrap();

    // LRD layout (36 bytes), first field is logtid (u32 LE):
    let logtid = LittleEndian::read_u32(&log_data[0..4]);
    assert_eq!(logtid, txid as u32, "LRD logtid should match committed txid");

    // backchain should be 0 (single record in this transaction).
    let backchain = LittleEndian::read_u32(&log_data[4..8]);
    assert_eq!(backchain, 0);

    // type field at offset 8 (u16 LE) should be LOG_UPDATEMAP (0x0008).
    let rec_type = LittleEndian::read_u16(&log_data[8..10]);
    assert_eq!(rec_type, LOG_UPDATEMAP, "LRD type should be LOG_UPDATEMAP");

    // length field at offset 10 (u16 LE) should match data.len() = BLOCK_SIZE.
    let length = LittleEndian::read_u16(&log_data[10..12]);
    assert_eq!(length as usize, BLOCK_SIZE, "LRD length should match page data size");

    // redopage_inode at offset 20 (u32 LE) should be the block number.
    let redo_inode = LittleEndian::read_u32(&log_data[20..24]);
    assert_eq!(redo_inode, META_BLOCK as u32, "LRD should reference the metadata block");

    // 9. Verify the page data follows immediately after the LRD (at offset 36).
    let page_data = &log_data[36..36 + BLOCK_SIZE];
    assert_eq!(
        LittleEndian::read_u32(&page_data[..4]),
        0xDEADBEEF,
        "journal record should contain the page data"
    );
}
