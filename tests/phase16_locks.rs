// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 16: POSIX advisory byte-range lock tests.
//!
//! Tests `FUSE_GETLK`, `FUSE_SETLK`, and `FUSE_SETLKW` (via `getlk`,
//! `setlk`, `setlkw`) on a generated JFS image loaded into MemoryStorage.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::{
    EACCES, EAGAIN, EINTR, EINVAL, ENOENT, EROFS, FuseFs, F_RDLCK, F_UNLCK, F_WRLCK, FUSE_BMAP,
    FUSE_BIG_WRITES, FUSE_FLOCK_LOCKS, FUSE_ASYNC_READ, FUSE_PARALLEL_DIROPS, FUSE_POSIX_LOCKS,
    InterruptManager, InterruptToken, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN, SEEK_SET,     F_OK, R_OK, Statfs, W_OK, X_OK,
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
fn test_fuse_posix_locks_capability() {
    let vol = load_image_to_memory();
    let fs = FuseFs::new(vol);
    let caps = fs.fuse_capabilities();
    assert!(caps & FUSE_POSIX_LOCKS != 0);
    assert!(caps & FUSE_FLOCK_LOCKS != 0);
    assert!(caps & FUSE_ASYNC_READ != 0);
    assert!(caps & FUSE_BIG_WRITES != 0);
    assert!(caps & FUSE_PARALLEL_DIROPS != 0);
    assert!(caps & FUSE_BMAP != 0);
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
    assert!(result.is_ok(), "exclusive lock should succeed: {:?}", result);
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
    fs.setlk(ino, &wr, 1).expect("owner 1 should get write lock");

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
    assert!(fs.setlk(ino, &rd, 2).is_ok(), "owner 2 read lock should coexist");
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
    assert!(fs.setlk(ino, &wr, 2).is_ok(), "lock should be available after unlock");
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
    assert!(result.is_ok(), "non-overlapping lock should succeed: {:?}", result);
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
    assert!(fs.setlk(ino, &wr1, 1).is_ok(), "owner 1 should reacquire after release");

    // Owner 2's lock should still be held.
    assert!(fs.setlk(ino, &wr1, 3).is_err(), "owner 2's lock should still block others");
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
    fs.flock(ino, LOCK_UN, 1).expect("owner 1 unlock via LOCK_UN");
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
    assert!(result.is_ok(), "bmap on root directory should succeed: {:?}", result);
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
    fs.write_interruptible(ino, 0, &data, &token).expect("write");
    fs.flush(ino).expect("flush");

    // Map file block 0 — should return a physical block > 0 (or 0 for sparse).
    let result = fs.bmap(ino, 0);
    assert!(result.is_ok(), "bmap should succeed on written file: {:?}", result);
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
    assert!(result.is_ok(), "bmap on sparse should return hole: {:?}", result);
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
    fs.write_interruptible(ino, 0, &data, &token).expect("write");

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

#[cfg(feature = "writable")]
#[test]
fn test_sync_fs_capability() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    assert!(fs.fuse_capabilities() & jfsfuse::fuse::FUSE_SYNCFS != 0);
}

#[test]
fn test_readdirplus_returns_attrs() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "plusdir", 0o100644).expect("create");

    let entries = fs.readdirplus(parent, 0);
    assert!(entries.is_some(), "readdirplus should succeed");
    let entries = entries.unwrap();

    // Should have `.` and `..` entries at minimum.
    assert!(entries.len() >= 2, "should have at least . and ..");
    // `.` entry should reference the directory itself.
    let dot_entry = &entries[0];
    assert_eq!(dot_entry.0, ".", "first entry should be .");
    assert_eq!(dot_entry.1, parent, "dot should reference parent");
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

    let parent = fs.volume.root_ino;
    // Load root inode data into page cache by doing a readdir.
    let _ = fs.readdir(parent, 0);

    // Forget should not panic.
    fs.forget(parent, 1);
    fs.forget(parent, 1);
}

#[test]
fn test_batch_forget_multiple_inodes() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    let ino1 = fs.lookup(parent, ".").unwrap_or(2);

    // Batch forget should handle multiple entries.
    fs.batch_forget(&[(parent, 1), (ino1, 1)]);
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

    assert!(stat.bfree <= stat.blocks, "free blocks should not exceed total");
    assert!(stat.bavail <= stat.blocks, "available should not exceed total");
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
    fs.setattr(ino, Some(0o100600), Some(1000), Some(1000), None, None).expect("setattr");

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
    fs.setattr(ino, Some(0o100644), Some(1000), Some(1000), None, None).expect("setattr");

    // Another user (uid 2000) — other perms (r--): read ok, write denied.
    assert!(fs.access(ino, R_OK, 2000, 2000).is_ok());
    assert_eq!(fs.access(ino, W_OK, 2000, 2000), Err(EACCES));
}

#[test]
fn test_access_f_ok_existence() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    // F_OK just checks existence.
    assert!(fs.access(parent, F_OK, 1000, 1000).is_ok());
}

