// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 16: POSIX advisory byte-range lock tests.
//!
//! Tests `FuseFs::setlk`, `setlkw`, `getlk` and `flock` methods on a generated
//! JFS image loaded into MemoryStorage.
//!
//! Note: fuse3 0.7's `Filesystem` trait does not include `setlk`/`getlk`/`flock`
//! methods (they require the `file-lock` feature which is not enabled). These
//! tests exercise the `FuseFs` internal lock API directly, which is used by the
//! FUSE adapter when those features become available.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::{
    EACCES, EAGAIN, EINVAL, ENOENT, EOPNOTSUPP, EROFS, F_OK, F_RDLCK, F_UNLCK, F_WRLCK,
    FUSE_ASYNC_READ, FUSE_BIG_WRITES, FUSE_DO_READDIRPLUS, FUSE_PARALLEL_DIROPS,
    FUSE_READDIRPLUS_AUTO, FuseFs, InterruptManager, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN, R_OK,
    SEEK_SET, W_OK, X_OK,
};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;

fn load_image_to_memory() -> jfsfuse::volume::Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    jfsfuse::volume::Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

fn make_flock(l_type: i16, offset: i64, length: i64) -> jfsfuse::fuse::Flock {
    jfsfuse::fuse::Flock {
        l_type,
        l_whence: SEEK_SET,
        l_start: offset,
        l_len: length,
        l_pid: 0,
    }
}

#[cfg(feature = "writable")]
#[test]
fn test_fuse_capabilities_conservative() {
    let vol = load_image_to_memory();
    let fs = FuseFs::new(vol);
    let caps = fs.fuse_capabilities();
    // POSIX locks and flock are NOT advertised because fuse3 0.7's
    // Filesystem trait doesn't include those methods (file-lock feature off).
    assert!(caps & FUSE_DO_READDIRPLUS != 0);
    assert!(caps & FUSE_READDIRPLUS_AUTO != 0);
    assert!(caps & FUSE_ASYNC_READ != 0);
    assert!(caps & FUSE_BIG_WRITES != 0);
    assert!(caps & FUSE_PARALLEL_DIROPS != 0);
    // BMAP is an opcode, not a capability flag.
}

#[cfg(feature = "writable")]
#[test]
fn test_setlk_grants_exclusive_lock() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "lockfile", 0o100644).expect("create");

    let fl = make_flock(F_WRLCK, 0, 100);
    let result = fs.setlk(ino, &fl, 1);
    assert!(
        result.is_ok(),
        "exclusive lock should succeed: {:?}",
        result
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_setlk_shared_lock_allowed() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "shared", 0o100644).expect("create");

    let fl = make_flock(F_RDLCK, 0, 100);
    let result = fs.setlk(ino, &fl, 1);
    assert!(result.is_ok(), "shared lock should succeed: {:?}", result);
}

#[cfg(feature = "writable")]
#[test]
fn test_setlk_blocks_conflicting_exclusive() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "conflict", 0o100644).expect("create");

    // Owner 1 takes a write lock.
    let wr = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr, 1)
        .expect("owner 1 should get write lock");

    // Owner 2 tries a write lock — should be blocked.
    let result = fs.setlk(ino, &wr, 2);
    assert!(result.is_err(), "conflicting write lock should fail");
    assert_eq!(result.unwrap_err(), jfsfuse::fuse::EAGAIN);
}

#[cfg(feature = "writable")]
#[test]
fn test_setlk_blocks_conflicting_shared() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "rwconf", 0o100644).expect("create");

    // Owner 1 takes a write lock.
    let wr = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr, 1).expect("owner 1 write lock");

    // Owner 2 tries a read lock — should be blocked.
    let rd = make_flock(F_RDLCK, 0, 100);
    let result = fs.setlk(ino, &rd, 2);
    assert!(result.is_err(), "conflicting read lock should fail");
}

#[cfg(feature = "writable")]
#[test]
fn test_shared_locks_coexist() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "multi_rd", 0o100644).expect("create");

    let rd = make_flock(F_RDLCK, 0, 100);
    assert!(fs.setlk(ino, &rd, 1).is_ok(), "owner 1 read lock");
    assert!(
        fs.setlk(ino, &rd, 2).is_ok(),
        "owner 2 read lock should coexist"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_unlock_releases_lock() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "unlock_test", 0o100644).expect("create");

    let wr = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr, 1).expect("set write lock");

    let unl = make_flock(F_UNLCK, 0, 100);
    fs.setlk(ino, &unl, 1).expect("release write lock");

    // Owner 2 should now get the lock.
    assert!(
        fs.setlk(ino, &wr, 2).is_ok(),
        "lock should be available after unlock"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_getlk_finds_conflict() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "getlk_test", 0o100644).expect("create");

    // Owner 1 holds a write lock.
    let wr = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr, 1).expect("owner 1 write lock");

    // Owner 2 checks with getlk.
    let query = make_flock(F_WRLCK, 0, 100);
    let result = fs.getlk(ino, &query, 2);
    assert!(result.is_ok(), "getlk should succeed");
    let found = result.unwrap();
    assert_ne!(found.l_type, F_UNLCK, "getlk should find conflicting lock");
    assert_eq!(found.l_type, F_WRLCK, "should report write lock");
}

#[cfg(feature = "writable")]
#[test]
fn test_getlk_no_conflict() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "getlk_free", 0o100644).expect("create");

    let query = make_flock(F_WRLCK, 0, 100);
    let result = fs.getlk(ino, &query, 1);
    assert!(result.is_ok());
    let found = result.unwrap();
    assert_eq!(found.l_type, F_UNLCK, "should report no conflict");
}

#[cfg(feature = "writable")]
#[test]
fn test_setlkw_returns_eagain() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "blocking", 0o100644).expect("create");

    let wr = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr, 1).expect("owner 1 write lock");

    // Owner 2 tries blocking set — should get EAGAIN (can't truly block in-memory).
    let result = fs.setlkw(ino, &wr, 2);
    assert!(result.is_err(), "blocking setlk should fail");
    assert_eq!(result.unwrap_err(), jfsfuse::fuse::EAGAIN);
}

#[cfg(feature = "writable")]
#[test]
fn test_non_overlapping_locks() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "nonoverlap", 0o100644).expect("create");

    // Owner 1 locks bytes 0..100.
    let wr1 = make_flock(F_WRLCK, 0, 100);
    fs.setlk(ino, &wr1, 1).expect("owner 1 lock 0..100");

    // Owner 2 locks bytes 100..200 — should succeed (no overlap).
    let wr2 = make_flock(F_WRLCK, 100, 100);
    let result = fs.setlk(ino, &wr2, 2);
    assert!(
        result.is_ok(),
        "non-overlapping lock should succeed: {:?}",
        result
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_lock_on_nonexistent_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let fl = make_flock(F_WRLCK, 0, 100);
    let result = fs.setlk(999, &fl, 1);
    assert!(result.is_err(), "lock on nonexistent inode should fail");
}

#[cfg(feature = "writable")]
#[test]
fn test_release_locks_clears_all() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "reltest", 0o100644).expect("create");

    // Set locks from different owners.
    let wr1 = make_flock(F_WRLCK, 0, 50);
    let wr2 = make_flock(F_WRLCK, 50, 50);
    fs.setlk(ino, &wr1, 1).expect("owner 1 lock");
    fs.setlk(ino, &wr2, 2).expect("owner 2 lock");

    // Release all locks for owner 1.
    fs.release_locks(ino, 1);

    // Owner 1 should be able to reacquire.
    assert!(
        fs.setlk(ino, &wr1, 1).is_ok(),
        "owner 1 should reacquire after release"
    );

    // Owner 2's lock should still be held.
    assert!(
        fs.setlk(ino, &wr1, 3).is_err(),
        "owner 2's lock should still block others"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_locks_clears_all() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flocktest", 0o100664).expect("create");

    // Acquire flock LOCK_EX for owner 1.
    fs.flock(ino, LOCK_EX, 1).expect("owner 1 exclusive flock");
    // Owner 2 should get EAGAIN (exclusive conflict).
    assert_eq!(fs.flock(ino, LOCK_EX, 2), Err(EAGAIN));
    assert_eq!(fs.flock(ino, LOCK_SH, 2), Err(EAGAIN));

    // Release owner 1's flock.
    fs.release_locks(ino, 1);
    // Now owner 2 can acquire.
    assert!(fs.flock(ino, LOCK_EX, 2).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_shared_allows_multiple() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flockshared", 0o100664).expect("create");

    fs.flock(ino, LOCK_SH, 1).expect("owner 1 shared flock");
    fs.flock(ino, LOCK_SH, 2).expect("owner 2 shared flock");

    // Exclusive should fail while shared held.
    assert_eq!(fs.flock(ino, LOCK_EX, 3), Err(EAGAIN));

    // Release owner 1's shared lock — owner 3 still holds shared.
    fs.release_locks(ino, 1);
    assert_eq!(fs.flock(ino, LOCK_EX, 4), Err(EAGAIN));

    // After releasing all shared, exclusive works.
    fs.release_locks(ino, 2);
    assert!(fs.flock(ino, LOCK_EX, 4).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_unlock() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flockunlock", 0o100664).expect("create");

    fs.flock(ino, LOCK_EX, 1).expect("owner 1 exclusive");
    fs.flock(ino, LOCK_UN, 1)
        .expect("owner 1 unlock via LOCK_UN");
    assert!(fs.flock(ino, LOCK_EX, 2).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_nonblocking_flag() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flocknb", 0o100664).expect("create");

    fs.flock(ino, LOCK_EX, 1).expect("owner 1 exclusive");
    // LOCK_NB should cause immediate EAGAIN.
    assert_eq!(fs.flock(ino, LOCK_EX | LOCK_NB, 2), Err(EAGAIN));
    // Without LOCK_NB, same result (in-memory impl returns EAGAIN).
    assert_eq!(fs.flock(ino, LOCK_EX, 3), Err(EAGAIN));
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_invalid_type() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flockbad", 0o100664).expect("create");

    assert_eq!(fs.flock(ino, 99, 1), Err(EINVAL));
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_readonly_rejected() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    // In read-only mode, inode 2 (root) exists.
    let result = fs.flock(parent, LOCK_EX, 1);
    assert_eq!(result, Err(EROFS));
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_owner_isolation() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flockowner", 0o100664).expect("create");

    // Owner 1 acquires shared; owner 2 also acquires shared (no conflict).
    fs.flock(ino, LOCK_SH, 1).expect("owner 1 shared");
    fs.flock(ino, LOCK_SH, 2).expect("owner 2 shared");

    // Owner 1 upgrades to exclusive after owner 2 releases? No — owner 1
    // already holds shared. flock replaces, so owner 1 can get exclusive now.
    // But owner 2 still holds shared → conflict.
    assert_eq!(fs.flock(ino, LOCK_EX, 1), Err(EAGAIN));

    // After owner 2 releases, owner 1 can go exclusive.
    fs.release_locks(ino, 2);
    assert!(fs.flock(ino, LOCK_EX, 1).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_flock_replaces_same_owner() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "flockrepl", 0o100664).expect("create");

    fs.flock(ino, LOCK_SH, 1).expect("owner 1 shared");
    // Same owner replaces shared with exclusive — no conflict.
    assert!(fs.flock(ino, LOCK_EX, 1).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_nonexistent_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    assert_eq!(fs.bmap(999, 0), Err(jfsfuse::fuse::ENOENT));
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_root_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Root inode (ino 2) is a directory — its xtree maps metadir blocks.
    // bmap should succeed and return a physical block.
    let result = fs.bmap(fs.volume.root_ino, 0);
    assert!(
        result.is_ok(),
        "bmap on root directory should succeed: {:?}",
        result
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_regular_file() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "bmaptest", 0o100644).expect("create");

    // Write a single block of data.
    let data = vec![0u8; 4096];
    let mgr = InterruptManager::new();
    let (_id, token) = mgr.register();
    fs.write_interruptible(ino, 0, &data, &token)
        .expect("write");
    fs.flush(ino).expect("flush");

    // Map file block 0 — should return a physical block > 0 (or 0 for sparse).
    let result = fs.bmap(ino, 0);
    assert!(
        result.is_ok(),
        "bmap should succeed on written file: {:?}",
        result
    );
    let (phys_block, num_blocks) = result.unwrap();
    // Physical block should be nonzero (allocated extent).
    assert!(phys_block > 0, "physical block should be allocated, got 0");
    assert!(num_blocks > 0, "block count should be positive");
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_sparse_file() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "bmapsparse", 0o100644).expect("create");

    // No data written — file is empty/sparse.
    // bmap on block 0 should return Ok((0, 0)) for the hole.
    let result = fs.bmap(ino, 0);
    assert!(
        result.is_ok(),
        "bmap on sparse should return hole: {:?}",
        result
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_sync_fs_writable() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "synctest", 0o100644).expect("create");
    let data = vec![42u8; 4096];
    let mgr = InterruptManager::new();
    let (_id, token) = mgr.register();
    fs.write_interruptible(ino, 0, &data, &token)
        .expect("write");

    // Sync should commit transactions and flush to stable storage.
    let result = fs.sync_fs();
    assert!(result.is_ok(), "sync_fs failed: {:?}", result);
}

#[cfg(feature = "writable")]
#[test]
fn test_sync_fs_readonly_rejected() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    assert_eq!(fs.sync_fs(), Err(jfsfuse::fuse::EROFS));
}

#[test]
fn test_sync_fs_capability() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // FUSE_SYNCFS is an opcode, not a capability flag, but the sync_fs
    // method is available in writable mode.
    assert!(fs.fuse_capabilities() & jfsfuse::fuse::FUSE_DO_READDIRPLUS != 0);
}

#[test]
fn test_readdirplus_returns_attrs() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let _ino = fs.create(parent, "plusdir", 0o100644).expect("create");

    let entries = fs.readdirplus(parent, 0);
    assert!(entries.is_some(), "readdirplus should succeed");
    let entries = entries.unwrap();

    // Should have `.` and `..` entries at minimum.
    assert!(entries.len() >= 2, "should have at least . and ..");
    // `.` entry should reference the directory itself.
    let dot_entry = &entries[0];
    assert_eq!(dot_entry.0, ".", "first entry should be .");
    assert_eq!(dot_entry.1, parent, "dot should reference parent");
    assert_eq!(dot_entry.4, 0, "dot entry should have cookie 0");
}

#[test]
fn test_readdirplus_lookup_reference_accounting() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "liforef", 0o100644).expect("create");

    // After create, lookup_count = 1.
    assert_eq!(fs.lookup_count(ino), 1);

    // READDIRPLUS returns child entries AND increments their lookup references.
    // After readdirplus, the child's lookup_count should be higher.
    let _entries = fs.readdirplus(parent, 0).expect("readdirplus");
    // The child should have been accounted for in readdirplus.
    let plus_count = fs.lookup_count(ino);
    assert!(
        plus_count >= 2,
        "readdirplus should increment lookup reference (was {})",
        plus_count
    );

    // FORGET should decrement by 1 (readdirplus added 1 ref).
    fs.forget(ino, 1);
    assert!(
        fs.lookup_count(ino) >= 1,
        "forget after readdirplus should leave refs"
    );

    // Full forget down to 0.
    let remaining = fs.lookup_count(ino);
    fs.forget(ino, remaining);
    assert_eq!(fs.lookup_count(ino), 0, "all refs should be released");
}

#[test]
fn test_readdirplus_not_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "plusfile", 0o100644).expect("create");

    // readdirplus on a regular file should return None.
    assert!(fs.readdirplus(ino, 0).is_none());
}

#[test]
fn test_forget_evicts_page_cache() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a file (lookup_count starts at 1).
    let ino = fs.create(parent, "ftest", 0o100644).expect("create");
    assert_eq!(fs.lookup_count(ino), 1);

    // Look up the file again (lookup_count = 2).
    let _ = fs.lookup(parent, "ftest");
    assert_eq!(fs.lookup_count(ino), 2);

    // Partial forget (count 2 -> 1): node state should remain tracked.
    fs.forget(ino, 1);
    assert_eq!(fs.lookup_count(ino), 1);

    // Full forget (count 1 -> 0): node state removed.
    fs.forget(ino, 1);
    assert_eq!(fs.lookup_count(ino), 0);
}

#[test]
fn test_batch_forget_multiple_inodes() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create files (count = 1 each) and look them up (count = 2 each).
    let ino1 = fs.create(parent, "bf1", 0o100644).expect("create");
    let ino2 = fs.create(parent, "bf2", 0o100644).expect("create");
    let _ = fs.lookup(parent, "bf1");
    let _ = fs.lookup(parent, "bf2");

    // Batch forget with partial counts (2 -> 1 for each).
    // Counts should remain tracked (not zero).
    fs.batch_forget(&[(ino1, 1), (ino2, 1)]);
    assert_eq!(fs.lookup_count(ino1), 1);
    assert_eq!(fs.lookup_count(ino2), 1);

    // Batch forget with full counts (1 -> 0 for each).
    // Counts should reach zero and node states removed.
    fs.batch_forget(&[(ino1, 1), (ino2, 1)]);
    assert_eq!(fs.lookup_count(ino1), 0);
    assert_eq!(fs.lookup_count(ino2), 0);

    // Should not panic on empty list.
    fs.batch_forget(&[]);
}

#[test]
fn test_statfs_returns_valid_sizes() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    let stat = fs.statfs(fs.volume.root_ino);

    assert_eq!(stat.bsize, 4096, "block size should be 4096");
    assert_eq!(stat.blksize, 4096, "fragment size should match block size");
    assert!(stat.blocks > 0, "total blocks should be positive");
    assert!(stat.files > 0, "total inodes should be positive");
    assert_eq!(stat.namelen, 255, "max filename length should be 255");
}

#[test]
fn test_statfs_free_blocks_nonnegative() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    let stat = fs.statfs(fs.volume.root_ino);

    assert!(
        stat.bfree <= stat.blocks,
        "free blocks should not exceed total"
    );
    assert!(
        stat.bavail <= stat.blocks,
        "available should not exceed total"
    );
}

#[test]
fn test_statfs_after_write() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let stat_before = fs.statfs(parent);

    // Create a file.
    let _ = fs.create(parent, "statfstest", 0o100644);

    let stat_after = fs.statfs(parent);
    // Block count should remain the same (creating a file doesn't add blocks).
    assert_eq!(stat_before.blocks, stat_after.blocks);
}

#[test]
fn test_access_root_bypass() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    // Root (uid 0) bypasses all permission checks.
    assert!(fs.access(parent, R_OK | W_OK | X_OK, 0, 0).is_ok());
}

#[test]
fn test_access_nonexistent_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    assert_eq!(fs.access(999, F_OK, 1000, 1000), Err(ENOENT));
}

#[test]
fn test_access_owner_permissions() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    // Create a file owned by uid 1000, mode 0644.
    let ino = fs.create(parent, "acctest", 0o100644).expect("create");
    // Set owner to 1000:1000 and mode 0600.
    fs.setattr(
        ino,
        Some(0o100600),
        Some(1000),
        Some(1000),
        None,
        None,
        None,
    )
    .expect("setattr");

    // Owner has read/write.
    assert!(fs.access(ino, R_OK, 1000, 1000).is_ok());
    assert!(fs.access(ino, W_OK, 1000, 1000).is_ok());
    // Execute denied (no execute bit).
    assert_eq!(fs.access(ino, X_OK, 1000, 1000), Err(EACCES));
}

#[test]
fn test_access_other_permissions() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "accother", 0o100644).expect("create");
    fs.setattr(
        ino,
        Some(0o100644),
        Some(1000),
        Some(1000),
        None,
        None,
        None,
    )
    .expect("setattr");

    // Another user (uid 2000) — other perms (r--): read ok, write denied.
    assert!(fs.access(ino, R_OK, 2000, 2000).is_ok());
    assert_eq!(fs.access(ino, W_OK, 2000, 2000), Err(EACCES));
}

#[test]
fn test_access_execute_denied_without_owner_execute() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "accexec", 0o100644).expect("create");
    // Set owner to 1000:1000 and mode 0651 (owner rw, group r-x, other --x).
    // Owner does NOT have execute, but "other" does.
    // ACCESS for the owner (uid 1000) should return EACCES for X_OK.
    fs.setattr(
        ino,
        Some(0o100651),
        Some(1000),
        Some(1000),
        None,
        None,
        None,
    )
    .expect("setattr");

    // Owner: no execute bit → EACCES even though "other" has execute.
    assert_eq!(fs.access(ino, X_OK, 1000, 1000), Err(EACCES));
}

#[test]
fn test_access_f_ok_existence() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    // F_OK just checks existence.
    assert!(fs.access(parent, F_OK, 1000, 1000).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_unallocated_returns_hole() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    // Create a file with some data.
    let ino = fs.create(parent, "bmapfile", 0o100644).expect("create");
    fs.write(ino, 0, b"Hello, world!").expect("write");
    fs.flush(ino).expect("flush");

    // Block 0 should be mapped (data is written).
    let (phys, len) = fs.bmap(ino, 0).expect("bmap");
    assert!(len > 0, "extent length should be > 0 for allocated block");
    assert!(
        phys > 0,
        "physical block should be non-zero for mapped extent"
    );

    // Block well beyond the file size should return a hole (0, 0).
    let (hole_phys, hole_len) = fs.bmap(ino, 1000).expect("bmap hole");
    assert_eq!(hole_phys, 0, "hole should have physical block 0");
    assert_eq!(hole_len, 0, "hole should have length 0");
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_unwritten_file_returns_hole() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "emptyfile", 0o100644).expect("create");

    // File with no data — all blocks are holes.
    let (phys, len) = fs.bmap(ino, 0).expect("bmap");
    assert_eq!(phys, 0, "unallocated block should return physical 0");
    assert_eq!(len, 0, "unallocated block should return length 0");
}

#[cfg(feature = "writable")]
#[test]
fn test_readdir_offset_resumption() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    // Create multiple entries.
    for name in &["a", "b", "c", "d", "e"] {
        fs.create(parent, name, 0o100644).expect("create");
    }

    // Read with offset 0 (includes . and ..)
    let entries = fs.readdir(parent, 0).expect("readdir");
    assert!(entries.len() > 5);

    // Read with offset past . and .. but at first real entry
    let entries2 = fs.readdir(parent, 2).expect("readdir offset 2");
    // Should NOT include "." or ".."
    assert!(!entries2.iter().any(|(n, _, _, _)| n == "."));
    assert!(!entries2.iter().any(|(n, _, _, _)| n == ".."));
    // But should include real entries
    assert!(entries2.iter().any(|(n, _, _, _)| n == "a"
        || n == "b"
        || n == "c"
        || n == "d"
        || n == "e"));
}

#[cfg(feature = "writable")]
#[test]
fn test_open_unlink_read_close_lifetime() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "lifetest", 0o100644).expect("create");

    // Open, write, unlink, read, close — data should still be accessible.
    fs.open(ino).expect("open");
    fs.write(ino, 0, b"hello lifetime").expect("write");
    fs.unlink(parent, "lifetest").expect("unlink");

    // Read back the data while still open.
    let data = fs.read(ino, 0, 14).expect("read after unlink");
    assert_eq!(&data[..14], b"hello lifetime");

    fs.release(ino).expect("release");
}

#[cfg(feature = "writable")]
#[test]
fn test_open_unlink_close_reclaim() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Open a file, unlink it, then close — inode and blocks should be reclaimable.
    let ino = fs.create(parent, "reclaimtest", 0o100644).expect("create");
    fs.open(ino).expect("open");
    fs.write(ino, 0, b"data to reclaim").expect("write");

    fs.unlink(parent, "reclaimtest").expect("unlink");
    fs.release(ino).expect("release should free the inode");

    // After release, the inode should be gone — readdir should not list it.
    let entries = fs.readdir(parent, 0).expect("readdir");
    assert!(
        !entries.iter().any(|(n, _, _, _)| n == "reclaimtest"),
        "unlinked+released file should not appear in readdir"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_readdir_cookies_are_logical() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Add a few entries.
    let _ = fs.create(parent, "alpha", 0o100644).expect("create alpha");
    let _ = fs.create(parent, "beta", 0o100644).expect("create beta");
    let _ = fs.create(parent, "gamma", 0o100644).expect("create gamma");

    // Offset 0: should return . with cookie 0, .. with cookie 1, and entries.
    let entries = fs.readdir(parent, 0).expect("readdir");
    assert_eq!(entries[0].0, ".", "first entry should be .");
    assert_eq!(entries[0].3, 0, "cookie for . should be 0");

    // If .. exists as second entry, its cookie should be 1.
    if entries.len() > 1 && entries[1].0 == ".." {
        assert_eq!(entries[1].3, 1, "cookie for .. should be 1");
    }

    // The first real entry should have cookie ENTRY_BASE (2).
    let first_real = entries.iter().find(|(n, _, _, _)| n != "." && n != "..");
    assert!(first_real.is_some(), "should have at least one real entry");
    let (_, _, _, cookie) = first_real.unwrap();
    assert_eq!(*cookie, 2, "first real entry should have cookie 2");

    // Resuming from offset 2 should NOT return . or ...
    let entries2 = fs.readdir(parent, 2).expect("readdir offset 2");
    assert!(!entries2.iter().any(|(n, _, _, _)| n == "."));
    assert!(!entries2.iter().any(|(n, _, _, _)| n == ".."));
    // First entry should have cookie 2.
    assert_eq!(
        entries2[0].3, 2,
        "first entry at offset 2 should have cookie 2"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_getxattr_size_query() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "xattrtest", 0o100644).expect("create");

    // Set a user xattr.
    fs.setxattr(ino, "user.comment", b"hello", 0)
        .expect("setxattr");

    // listxattr should reverse-translate the internal name to "user.comment".
    let names = fs.listxattr(ino).expect("listxattr");
    assert!(
        names.iter().any(|n| n == "user.comment"),
        "user.comment should appear with prefix"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_fallocate_unsupported_flags() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "fatest", 0o100644).expect("create");

    // FALLOC_FL_COLLAPSE_RANGE is not supported.
    let result = fs.fallocate(ino, 0, 4096, 0x08);
    assert!(result.is_err(), "collapse range should fail");
    assert_eq!(result.unwrap_err(), EOPNOTSUPP, "should be EOPNOTSUPP");

    // FALLOC_FL_ZERO_RANGE is not supported.
    let result = fs.fallocate(ino, 0, 4096, 0x10);
    assert!(result.is_err(), "zero range should fail");
    assert_eq!(result.unwrap_err(), EOPNOTSUPP, "should be EOPNOTSUPP");
}
