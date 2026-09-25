// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 3: Crash simulation test.
//!
//! Verifies that after a simulated crash (metadata on disk is stale/corrupt,
//! but journal records are intact), `JournalRecovery::replay` correctly
//! restores the on-disk metadata from the journal's after-images.
//!
//! Scenario:
//! 1. Initialize a storage with a journal area.
//! 2. Write a log record (REDOPAGE + data) + LOG_COMMIT to the journal.
//! 3. Simulate a crash: metadata block on disk contains zeros (old data).
//! 4. Run `JournalRecovery::replay`.
//! 5. Verify the metadata block now contains the journal's after-image.

use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use jfsfuse::journal::{JournalRecovery, LogManager};
use jfsfuse::storage::{BLOCK_SIZE, MemoryStorage, Storage};
use jfsfuse::types::{LOGMAGIC, LOGPAGES, LOGVERSION, LOGWRAP, LogSuper};

const NUM_BLOCKS: u64 = 50;

fn make_log_super(num_pages: u32) -> LogSuper {
    let mut ls = LogSuper::default();
    LittleEndian::write_u32(&mut ls.magic, LOGMAGIC);
    LittleEndian::write_u32(&mut ls.version, LOGVERSION);
    LittleEndian::write_u32(&mut ls.serial, 0);
    LittleEndian::write_u32(&mut ls.size, num_pages);
    LittleEndian::write_u32(&mut ls.bsize, BLOCK_SIZE as u32);
    LittleEndian::write_u32(&mut ls.l2bsize, 12);
    LittleEndian::write_u32(&mut ls.flag, 0);
    LittleEndian::write_u32(&mut ls.state, LOGWRAP);
    LittleEndian::write_u32(&mut ls.end, 0);
    ls
}

const META_BLOCK: u64 = 18;
const LOG_DATA_START: u64 = 2 * BLOCK_SIZE as u64;

#[test]
fn test_recovery_restores_metadata_after_crash() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(NUM_BLOCKS));

    // 1. Initialize the journal.
    let ls = make_log_super(LOGPAGES as u32);
    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    // 2. Write a transaction to the journal: REDOPAGE record + LOG_COMMIT.
    let txid = 42u64;
    let mut data = vec![0xABu8; BLOCK_SIZE];
    // Write a recognizable pattern at the beginning.
    LittleEndian::write_u32(&mut data[0..4], 0xDEADBEEF);

    lm.append_log_record(txid, META_BLOCK, &data).unwrap();
    lm.commit_transaction(txid).unwrap();

    // 3. Simulate crash: corrupt the metadata block on disk (old/stale data).
    //    The journal already has the after-image, so recovery should fix it.
    {
        let zero_block = vec![0u8; BLOCK_SIZE];
        storage.write_block(META_BLOCK, &zero_block).unwrap();
    }

    // Verify the block is indeed zeros before recovery.
    {
        let block = storage.read_block(META_BLOCK).unwrap();
        assert_eq!(block[0], 0, "metadata should be zeros before recovery");
    }

    // 4. Run recovery — replay the journal.
    let ls_reloaded = LogManager::read_super(&*storage, 0).unwrap();
    let mut recovery = JournalRecovery::new(ls_reloaded);
    recovery
        .replay(&*storage, &*storage, LOG_DATA_START)
        .unwrap();

    // 5. Verify the metadata block was restored from the journal after-image.
    let restored = storage.read_block(META_BLOCK).unwrap();
    assert_eq!(
        LittleEndian::read_u32(&restored[..4]),
        0xDEADBEEF,
        "metadata should be restored from journal after recovery"
    );
    assert_eq!(
        restored[BLOCK_SIZE - 1],
        0xAB,
        "restored page should contain original after-image data"
    );

    // 6. Verify the logsuper state was set to LOGREDONE.
    let ls_final = LogManager::read_super(&*storage, 0).unwrap();
    assert_eq!(
        ls_final.state(),
        jfsfuse::types::LOGREDONE,
        "log state should be LOGREDONE after recovery"
    );
}

#[test]
fn test_recovery_skips_uncommitted_records() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(NUM_BLOCKS));

    // 1. Initialize the journal.
    let ls = make_log_super(LOGPAGES as u32);
    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    let txid = 99u64;
    let mut committed_data = vec![0xCDu8; BLOCK_SIZE];
    LittleEndian::write_u32(&mut committed_data[0..4], 0xFACEB00C);

    let mut uncommitted_data = vec![0xFEu8; BLOCK_SIZE];
    LittleEndian::write_u32(&mut uncommitted_data[0..4], 0xBAADF00D);

    // Write a committed record.
    lm.append_log_record(txid, META_BLOCK, &committed_data)
        .unwrap();
    lm.commit_transaction(txid).unwrap();

    // Write an uncommitted record (no commit after append).
    let txid2 = 100u64;
    let uncommitted_block = META_BLOCK + 1;
    lm.append_log_record(txid2, uncommitted_block, &uncommitted_data)
        .unwrap();

    // Do NOT call commit_transaction for txid2 — simulate a crash mid-transaction.
    // But flush the journal to make data durable.
    lm.flush_journal().unwrap();

    // Corrupt both metadata blocks.
    let zeros = vec![0u8; BLOCK_SIZE];
    storage.write_block(META_BLOCK, &zeros).unwrap();
    storage.write_block(uncommitted_block, &zeros).unwrap();

    // Run recovery.
    let ls_reloaded = LogManager::read_super(&*storage, 0).unwrap();
    let mut recovery = JournalRecovery::new(ls_reloaded);
    recovery
        .replay(&*storage, &*storage, LOG_DATA_START)
        .unwrap();

    // Committed record should be replayed.
    let committed = storage.read_block(META_BLOCK).unwrap();
    assert_eq!(
        LittleEndian::read_u32(&committed[..4]),
        0xFACEB00C,
        "committed metadata should be restored"
    );

    // Uncommitted record should NOT be replayed (no LOG_COMMIT for txid2).
    let uncommitted = storage.read_block(uncommitted_block).unwrap();
    assert_eq!(
        uncommitted[0], 0,
        "uncommitted metadata should remain zeros"
    );
}

/// Phase 3c: Crash during commit_transaction (LOG_COMMIT flush fails).
///
/// The REDOPAGE record is flushed successfully (write_bytes calls 1-3),
/// but the LOG_COMMIT flush fails (write_bytes call 4). Recovery should
/// see the REDOPAGE without a corresponding LOG_COMMIT and skip replay.
#[test]
fn test_crash_during_commit_flush() {
    use jfsfuse::storage::FaultInjector;

    // 1. Initialize the journal with a FaultInjector as storage.
    let ls = make_log_super(LOGPAGES as u32);
    let fault: Arc<FaultInjector<MemoryStorage>> =
        Arc::new(FaultInjector::new(Box::new(MemoryStorage::new(NUM_BLOCKS))));
    let storage: Arc<dyn Storage> = fault.clone();

    let mut lm = LogManager::new(storage.clone(), ls, 0);
    lm.write_super(&ls).unwrap();

    // 2. Write REDOPAGE record and flush (succeeds).
    let txid = 42u64;
    let mut data = vec![0xABu8; BLOCK_SIZE];
    LittleEndian::write_u32(&mut data[0..4], 0xDEADBEEF);
    lm.append_log_record(txid, META_BLOCK, &data).unwrap();
    lm.flush_journal().unwrap();

    // 3. Corrupt the metadata block.
    let zeros = vec![0u8; BLOCK_SIZE];
    storage.write_block(META_BLOCK, &zeros).unwrap();

    // 4. Attempt commit — the COMMIT flush fails (write_bytes call #4).
    fault.set_write_fail(4);

    let commit_result = lm.commit_transaction(txid);
    assert!(
        commit_result.is_err(),
        "commit_transaction should fail when flush fails"
    );

    // Reset fault injector for recovery reads.
    fault.reset();

    // 5. Run recovery.
    let ls_reloaded = LogManager::read_super(&*storage, 0).unwrap();
    let mut recovery = JournalRecovery::new(ls_reloaded);
    recovery
        .replay(&*storage, &*storage, LOG_DATA_START)
        .unwrap();

    // 6. Verify metadata was NOT restored (transaction was uncommitted).
    let block = storage.read_block(META_BLOCK).unwrap();
    assert_eq!(
        block[0], 0,
        "uncommitted metadata should not be restored after crash"
    );
}
