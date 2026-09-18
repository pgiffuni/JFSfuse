// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 8: Directory mutation tests.
//!
//! Tests file creation and deletion on a real JFS image loaded
//! into MemoryStorage (writable backend).
//!
//! Requires the test image at `/tmp/kilo/test_jfs.img`.
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::storage::{BLOCK_SIZE, MemoryStorage, Storage};
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Option<Volume> {
    let path = "/tmp/kilo/test_jfs.img";
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: /tmp/kilo/test_jfs.img not found");
        return None;
    }

    let data = std::fs::read(path).ok()?;
    let num_blocks = (data.len() as u64 + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
    let mem = MemoryStorage::new(num_blocks);

    let storage: Arc<dyn Storage> = Arc::new(mem);

    let vol_blocks = data.len() / BLOCK_SIZE as usize;
    for i in 0..vol_blocks {
        let start = i * BLOCK_SIZE as usize;
        let end = start + BLOCK_SIZE as usize;
        let _ = storage.write_block(i as u64, &data[start..end]);
    }

    Some(Volume::open_from_storage(storage).expect("should mount JFS image from memory"))
}

#[cfg(feature = "writable")]
#[test]
fn test_create_file() {
    let vol = load_image_to_memory();
    let vol = match vol {
        Some(v) => v,
        None => return,
    };
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
        looked_up, Some(new_ino),
        "lookup should find the newly created file"
    );

    // Verify it's a regular file with size 0.
    let dinode = fs.getattr(new_ino).expect("should getattr new file");
    assert_eq!(u32::from_le_bytes(dinode.di_mode) & 0xf000, 0x8000, "should be a regular file");
    assert_eq!(u64::from_le_bytes(dinode.di_size), 0, "file should be empty");
}

#[cfg(feature = "writable")]
#[test]
fn test_create_then_unlink() {
    let vol = load_image_to_memory();
    let vol = match vol {
        Some(v) => v,
        None => return,
    };
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tunlink";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

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
    let vol = match vol {
        Some(v) => v,
        None => return,
    };
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "dup_test";

    let ino1 = fs.create(parent, name, 0o100644).expect("first create should succeed");
    assert!(ino1 > 0);

    // Creating the same name again should fail (return None).
    let ino2 = fs.create(parent, name, 0o100644);
    assert!(ino2.is_none(), "duplicate create should fail");

    // Clean up.
    let _ = fs.unlink(parent, name);
}
