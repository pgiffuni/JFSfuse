// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 8: Directory mutation tests.
//!
//! Tests file creation and deletion on a generated JFS image loaded
//! into MemoryStorage (writable backend).
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_create_file() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf8";

    let result = fs.volume.create_file(parent, name);
    if let Err(e) = &result {
        eprintln!("create_file error: {:?}", e);
    }
    let ino = result.ok();
    assert!(ino.is_some(), "create should return Some");
    let new_ino = ino.unwrap();

    // Look up the file — it should exist.
    let looked_up = fs.lookup(parent, name);
    assert_eq!(
        looked_up,
        Some(new_ino),
        "lookup should find the newly created file"
    );

    // Verify it's a regular file with size 0.
    let dinode = fs.getattr(new_ino).expect("should getattr new file");
    assert_eq!(
        u32::from_le_bytes(dinode.di_mode) & 0xf000,
        0x8000,
        "should be a regular file"
    );
    assert_eq!(
        u64::from_le_bytes(dinode.di_size),
        0,
        "file should be empty"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_create_then_unlink() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tunlink";

    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Verify the file exists.
    assert_eq!(fs.lookup(parent, name), Some(ino));

    // Unlink it.
    let result = fs.unlink(parent, name);
    assert!(result.is_some(), "unlink should return Some");
    assert!(result.unwrap(), "unlink should succeed");

    // Verify the entry is gone.
    assert_eq!(fs.lookup(parent, name), None);
}

#[cfg(feature = "writable")]
#[test]
fn test_create_duplicate_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "dup_test";

    let ino1 = fs
        .create(parent, name, 0o100644)
        .expect("first create should succeed");
    assert!(ino1 > 0);

    // Creating the same name again should fail (return None).
    let ino2 = fs.create(parent, name, 0o100644);
    assert!(ino2.is_none(), "duplicate create should fail");

    // Clean up.
    let _ = fs.unlink(parent, name);
}
