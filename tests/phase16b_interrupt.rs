// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 16b: FUSE_INTERRUPT tests.
//!
//! Tests that long-running operations (read, write, readdir) check
//! the interrupt flag and abort with EINTR when interrupted.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::{EINTR, EINVAL, EROFS, FuseFs};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_interrupt_manager_request_lifecycle() {
    let vol = load_image_to_memory();
    let fs = FuseFs::new(vol);

    let (req_id, token) = fs.register_request();
    assert_eq!(req_id, 1);
    assert!(!token.is_interrupted());

    // Simulate kernel interrupt.
    assert!(fs.handle_interrupt(req_id));
    assert!(token.is_interrupted());

    // Deregister — subsequent interrupt should return false.
    fs.deregister_request(req_id);
    assert!(!fs.handle_interrupt(req_id));
}

#[cfg(feature = "writable")]
#[test]
fn test_interrupt_during_read() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "bigfile", 0o100644).expect("create");

    // Write some data.
    let data = vec![0xABu8; 16384]; // 16KB = 4 blocks
    fs.write(ino, 0, &data).expect("write");

    // Register a request and immediately interrupt it.
    let (req_id, token) = fs.register_request();
    fs.handle_interrupt(req_id);

    // Read should fail with EINTR.
    let result = fs.read_interruptible(ino, 0, 16384, &token);
    assert!(result.is_err(), "interrupted read should fail");
    assert_eq!(result.unwrap_err(), EINTR);
}

#[cfg(feature = "writable")]
#[test]
fn test_interrupt_during_write() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "wrfile", 0o100644).expect("create");

    let (req_id, token) = fs.register_request();
    fs.handle_interrupt(req_id);

    let data = vec![0xCDu8; 4096];
    let result = fs.write_interruptible(ino, 0, &data, &token);
    assert!(result.is_err(), "interrupted write should fail");
    assert_eq!(result.unwrap_err(), EINTR);
}

#[cfg(feature = "writable")]
#[test]
fn test_interrupt_during_readdir() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    // Create some files to populate the directory.
    for i in 0..5 {
        let name = format!("f{}", i);
        fs.create(parent, &name, 0o100644).expect("create");
    }

    let (req_id, token) = fs.register_request();
    fs.handle_interrupt(req_id);

    let result = fs.readdir_interruptible(parent, 0, &token);
    assert!(result.is_err(), "interrupted readdir should fail");
    assert_eq!(result.unwrap_err(), EINTR);
}

#[cfg(feature = "writable")]
#[test]
fn test_uninterrupted_read_succeeds() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "okfile", 0o100644).expect("create");

    let data = vec![0x42u8; 4096];
    fs.write(ino, 0, &data).expect("write");

    let (_, token) = fs.register_request();
    let result = fs.read_interruptible(ino, 0, 4096, &token);
    assert!(result.is_ok(), "non-interrupted read should succeed");
    assert_eq!(result.unwrap(), data);
}

#[cfg(feature = "writable")]
#[test]
fn test_interrupt_only_cancels_target_request() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "isofile", 0o100644).expect("create");

    let data = vec![0x55u8; 4096];
    fs.write(ino, 0, &data).expect("write");

    let (id_a, token_a) = fs.register_request();
    let (_id_b, _token_b) = fs.register_request();

    // Only interrupt request A.
    fs.handle_interrupt(id_a);

    // Request B's token should not be interrupted.
    assert!(!_token_b.is_interrupted());

    // Reading with token A should fail.
    assert!(fs.read_interruptible(ino, 0, 4096, &token_a).is_err());

    // Reading with token B should succeed.
    assert!(fs.read_interruptible(ino, 0, 4096, &_token_b).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_lock_readonly_when_not_writable() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let flock = jfsfuse::fuse::Flock {
        l_type: jfsfuse::fuse::F_WRLCK,
        l_whence: jfsfuse::fuse::SEEK_SET,
        l_start: 0,
        l_len: 100,
        l_pid: 0,
    };

    assert_eq!(fs.setlk(1, &flock, 1), Err(EROFS));
    assert_eq!(fs.getlk(1, &flock, 1).err(), Some(EROFS));
    assert_eq!(fs.setlkw(1, &flock, 1), Err(EROFS));
}

#[cfg(feature = "writable")]
#[test]
fn test_setlk_invalid_type() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "badlock", 0o100644).expect("create");

    let flock = jfsfuse::fuse::Flock {
        l_type: 99, // invalid
        l_whence: jfsfuse::fuse::SEEK_SET,
        l_start: 0,
        l_len: 100,
        l_pid: 0,
    };

    assert_eq!(fs.setlk(ino, &flock, 1), Err(EINVAL));
}

#[cfg(feature = "writable")]
#[test]
fn test_lock_owner_isolation() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "isolate", 0o100644).expect("create");

    // Owner 1 takes a write lock on bytes 0..100.
    let wr = jfsfuse::fuse::Flock {
        l_type: jfsfuse::fuse::F_WRLCK,
        l_whence: jfsfuse::fuse::SEEK_SET,
        l_start: 0,
        l_len: 100,
        l_pid: 0,
    };
    fs.setlk(ino, &wr, 1).expect("owner 1 write lock");

    // Owner 1 re-locking the same range should succeed (upgrade/replace).
    let result = fs.setlk(ino, &wr, 1);
    assert!(
        result.is_ok(),
        "owner 1 re-locking should succeed: {:?}",
        result
    );

    // Owner 2 should still be blocked.
    let result = fs.setlk(ino, &wr, 2);
    assert!(result.is_err(), "owner 2 should be blocked");
}
