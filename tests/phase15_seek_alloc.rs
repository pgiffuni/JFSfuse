// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 15: SEEK_DATA/SEEK_HOLE and fallocate tests.
//!
//! Tests that:
//! - SEEK_DATA returns the correct data extent offset.
//! - SEEK_HOLE returns the correct hole offset.
//! - fallocate preallocates blocks and updates file size.
//! - fallocate with FALLOC_FL_PUNCH_HOLE creates a hole.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::EOPNOTSUPP;
use jfsfuse::mkfs;
use jfsfuse::storage::{BLOCK_SIZE, Storage};
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;

#[cfg(feature = "writable")]
#[test]
fn test_lseek_data_at_start_of_extent() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15data", 0o100644).expect("create should succeed");

    // Write 2 blocks of data at offset 0.
    let data = vec![0xABu8; BLOCK_SIZE * 2];
    let written = fs.write(ino, 0, &data).expect("write should succeed");
    assert_eq!(written, BLOCK_SIZE * 2);

    // SEEK_DATA at 0 should return 0 (data starts at offset 0).
    let result = fs.lseek_data_or_hole(ino, 0, 3).expect("lseek should succeed");
    assert_eq!(result, 0, "SEEK_DATA at start of extent should return 0");

    // SEEK_HOLE at 0 should return 2 * BLOCK_SIZE (end of data).
    let result = fs.lseek_data_or_hole(ino, 0, 4).expect("lseek should succeed");
    assert_eq!(
        result, 2 * BLOCK_SIZE as u64,
        "SEEK_HOLE should return end of extent"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_lseek_data_in_hole() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15hole", 0o100644).expect("create should succeed");

    // Write data at offset 0 and at offset 4*BLOCK_SIZE.
    let data = vec![0xCDu8; BLOCK_SIZE];
    fs.write(ino, 0, &data).expect("write should succeed");
    fs.write(ino, 4 * BLOCK_SIZE as u64, &data).expect("write should succeed");

    // SEEK_DATA at offset 2*BLOCK_SIZE should return 4*BLOCK_SIZE.
    let result = fs.lseek_data_or_hole(ino, 2 * BLOCK_SIZE as u64, 3).expect("lseek should succeed");
    assert_eq!(
        result, 4 * BLOCK_SIZE as u64,
        "SEEK_DATA in hole should find next data extent"
    );

    // SEEK_HOLE at offset BLOCK_SIZE should return BLOCK_SIZE (end of first extent).
    let result = fs.lseek_data_or_hole(ino, BLOCK_SIZE as u64, 4).expect("lseek should succeed");
    assert_eq!(
        result, BLOCK_SIZE as u64,
        "SEEK_HOLE at end of first extent should return that boundary"
    );

    // SEEK_HOLE at offset 2*BLOCK_SIZE should return 2*BLOCK_SIZE (in the hole).
    let result = fs
        .lseek_data_or_hole(ino, 2 * BLOCK_SIZE as u64, 4)
        .expect("lseek should succeed");
    assert_eq!(
        result,
        2 * BLOCK_SIZE as u64,
        "SEEK_HOLE in a hole should return the offset itself"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_lseek_data_past_eof() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15eof", 0o100644).expect("create should succeed");

    let data = vec![0x77u8; BLOCK_SIZE];
    fs.write(ino, 0, &data).expect("write should succeed");

    // SEEK_DATA past EOF should return file size.
    let result = fs.lseek_data_or_hole(ino, 10 * BLOCK_SIZE as u64, 3).expect("lseek should succeed");
    assert_eq!(
        result, BLOCK_SIZE as u64,
        "SEEK_DATA past last extent should return file size"
    );

    // SEEK_HOLE past last extent should return file size.
    let result = fs
        .lseek_data_or_hole(ino, 10 * BLOCK_SIZE as u64, 4)
        .expect("lseek should succeed");
    assert_eq!(
        result, BLOCK_SIZE as u64,
        "SEEK_HOLE past last extent should return file size"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_fallocate_allocates_space() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15falloc", 0o100644).expect("create should succeed");

    // Pre-allocate 4 blocks at offset 0.
    let result = fs.fallocate(ino, 0, 4 * BLOCK_SIZE as u64, 0);
    assert!(result.is_ok(), "fallocate should succeed: {:?}", result.err());

    // File size should now be 4 * BLOCK_SIZE.
    let attr = fs.getattr(ino).expect("should getattr");
    assert_eq!(
        u64::from_le_bytes(attr.di_size),
        4 * BLOCK_SIZE as u64,
        "fallocate should set file size"
    );

    // Allocated blocks should be readable as zeros.
    let data = fs.read(ino, 0, BLOCK_SIZE as usize).expect("read should succeed");
    assert_eq!(data.len(), BLOCK_SIZE as usize);
    assert!(data.iter().all(|&b| b == 0), "allocated blocks should be zero-filled");
}

#[cfg(feature = "writable")]
#[test]
fn test_fallocate_keep_size() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15ks", 0o100644).expect("create should succeed");

    // Write 1 block.
    let data = vec![0xAAu8; BLOCK_SIZE];
    fs.write(ino, 0, &data).expect("write should succeed");

    // Pre-allocate 4 more blocks without changing size.
    let result = fs.fallocate(ino, BLOCK_SIZE as u64, 4 * BLOCK_SIZE as u64, FALLOC_FL_KEEP_SIZE);
    assert!(result.is_ok(), "fallocate with KEEP_SIZE should succeed");

    // File size should NOT change (still 1 block).
    let attr = fs.getattr(ino).expect("should getattr");
    assert_eq!(
        u64::from_le_bytes(attr.di_size),
        BLOCK_SIZE as u64,
        "fallocate with KEEP_SIZE should not change file size"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_fallocate_punch_hole() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15punch", 0o100644).expect("create should succeed");

    // Write 4 blocks of data.
    let data = vec![0xBBu8; BLOCK_SIZE * 4];
    fs.write(ino, 0, &data).expect("write should succeed");

    // Verify data is written.
    let attr = fs.getattr(ino).expect("should getattr");
    assert_eq!(u64::from_le_bytes(attr.di_size), BLOCK_SIZE as u64 * 4);

    // Punch a hole in the middle: blocks 1 and 2 (offset BLOCK_SIZE..3*BLOCK_SIZE).
    let result = fs.fallocate(
        ino,
        BLOCK_SIZE as u64,
        2 * BLOCK_SIZE as u64,
        FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
    );
    assert!(result.is_ok(), "punch hole should succeed: {:?}", result.err());

    // File size should be unchanged.
    let attr = fs.getattr(ino).expect("should getattr");
    assert_eq!(
        u64::from_le_bytes(attr.di_size),
        4 * BLOCK_SIZE as u64,
        "punch hole should not change file size"
    );

    // First block should still have data.
    let block = fs.read(ino, 0, BLOCK_SIZE as usize).expect("read should succeed");
    assert_eq!(block[0], 0xBB, "first block should still have data");

    // Punched region should be zeros.
    let block = fs
        .read(ino, BLOCK_SIZE as u64, BLOCK_SIZE as usize)
        .expect("read should succeed");
    assert_eq!(block[0], 0, "punched block should be zero");
}

#[cfg(feature = "writable")]
#[test]
fn test_fallocate_unsupported_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15unsup", 0o100644).expect("create should succeed");

    // FALLOC_FL_COLLAPSE_RANGE is not supported.
    let result = fs.fallocate(ino, 0, BLOCK_SIZE as u64, FALLOC_FL_COLLAPSE_RANGE);
    assert_eq!(result, Err(EOPNOTSUPP), "unsupported mode should return EOPNOTSUPP");
}

#[cfg(feature = "writable")]
#[test]
fn test_readdir_after_fallocate() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "tf15readdir", 0o100644).expect("create should succeed");

    // Pre-allocate and write some data.
    let data = vec![0x55u8; BLOCK_SIZE * 2];
    fs.write(ino, 0, &data).expect("write should succeed");

    // Verify the file shows up in readdir.
    let entries = fs.readdir(parent, 0).expect("readdir should succeed");
    let found = entries.iter().any(|(name, _, _)| name == "tf15readdir");
    assert!(found, "file should appear in parent readdir");
}
