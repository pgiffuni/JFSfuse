// SPDX-License-Identifier: GPL-2.0-or-later
//! Crash recovery test: verify journal records persist across a simulated
//! process restart and the logsuper end-of-log pointer is durable.

use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use jfsfuse::journal::LogManager;
use jfsfuse::storage::{BLOCK_SIZE, MemoryStorage, Storage};
use jfsfuse::transaction::TransactionManager;
use jfsfuse::types::{LOGMAGIC, LOGPSIZE, LOG_UPDATEMAP, LOGVERSION, LOGWRAP, LOGPAGES};

const NUM_BLOCKS: u64 = 20;

fn make_log_super(num_pages: u32) -> jfsfuse::types::LogSuper {
    let mut ls = jfsfuse::types::LogSuper::default();
    LittleEndian::write_u32(&mut ls.magic, LOGMAGIC);
    LittleEndian::write_u32(&mut ls.version, LOGVERSION);
    LittleEndian::write_u32(&mut ls.serial, 1);
    LittleEndian::write_u32(&mut ls.size, num_pages);
    LittleEndian::write_u32(&mut ls.bsize, BLOCK_SIZE as u32);
    LittleEndian::write_u32(&mut ls.l2bsize, 12);
    LittleEndian::write_u32(&mut ls.flag, 0);
    LittleEndian::write_u32(&mut ls.state, LOGWRAP);
    LittleEndian::write_u32(&mut ls.end, 0);
    ls
}

#[test]
fn test_journal_survives_restart() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(NUM_BLOCKS));

    // Phase 1: write a transaction to the journal.
    let ls = make_log_super(LOGPAGES as u32);
    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    let txid = 42u64;
    let block: u64 = 10;
    let data = vec![0xABu8; BLOCK_SIZE];
    lm.append_log_record(txid, block, &data).unwrap();
    lm.flush_journal().unwrap();

    // Verify logsuper end was persisted.
    let ls_after = LogManager::read_super(&*storage, 0).unwrap();
    assert_eq!(ls_after.end(), 36 + BLOCK_SIZE as u32, "end should cover LRD + data");

    // Phase 2: simulate restart — read logsuper from storage.
    let ls_reloaded = LogManager::read_super(&*storage, 0).unwrap();
    assert_eq!(ls_reloaded.magic_val(), LOGMAGIC);
    assert_eq!(ls_reloaded.version(), LOGVERSION);
    assert_eq!(ls_reloaded.end(), 36 + BLOCK_SIZE as u32);
    assert_eq!(ls_reloaded.state(), LOGWRAP);

    // Phase 3: verify log record is readable from log pages.
    let data_start = 2 * (BLOCK_SIZE as u64);
    let log_bytes = storage.read_bytes(data_start, LOGPSIZE * 2).unwrap();

    let logtid = LittleEndian::read_u32(&log_bytes[0..4]);
    assert_eq!(logtid, txid as u32);

    let rec_type = LittleEndian::read_u16(&log_bytes[8..10]);
    assert_eq!(rec_type, LOG_UPDATEMAP);

    let length = LittleEndian::read_u16(&log_bytes[10..12]);
    assert_eq!(length as usize, BLOCK_SIZE);

    let redo_inode = LittleEndian::read_u32(&log_bytes[20..24]);
    assert_eq!(redo_inode, block as u32);

    // Verify page data in log matches what was written.
    let page_data = &log_bytes[36..36 + BLOCK_SIZE];
    assert_eq!(page_data[0], 0xAB, "page data in log should match");
}

#[test]
fn test_transaction_abort_leaves_no_journal_record() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(NUM_BLOCKS));

    let ls = make_log_super(LOGPAGES as u32);
    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    let mut cache = jfsfuse::storage::PageCache::new(16);
    cache.get_or_load(&*storage, 1, 18, 0).unwrap();

    {
        let page = cache.get_mut_for_write(1, 18).unwrap();
        page.write_u32_le(0, 0xDEAD);
    }

    let mut tm = TransactionManager::new();
    let _txid = tm.begin().unwrap();
    tm.mark_dirty(&mut cache, 1, 18).unwrap();
    tm.abort(&mut cache);

    // Abort should not have written anything to the journal.
    let ls_after = LogManager::read_super(&*storage, 0).unwrap();
    assert_eq!(ls_after.end(), 0, "journal end should be unchanged after abort");

    // Storage should still be zeros at block 18.
    let block = storage.read_block(18).unwrap();
    assert_eq!(block[0], 0);
}
