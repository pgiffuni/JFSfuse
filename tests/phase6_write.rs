// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 6: Write operations on a JFS filesystem image.
//!
//! Loads a generated JFS image into MemoryStorage (a writable backend) and
//! verifies:
//! - `write_at` can overwrite existing data blocks within a file
//! - `truncate` can change the file size
//! - `fsync` triggers a journal commit
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

/// Load a generated JFS image into a writable MemoryStorage-backed Volume.
fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

/// Find the first non-dot file entry in the root directory.
fn find_test_file(fs: &mut FuseFs) -> Option<u32> {
    let entries = fs.readdir(fs.volume.root_ino, 0)?;
    entries
        .iter()
        .find(|(name, _, _)| name != "." && name != ".." && !name.is_empty())
        .map(|(_, ino, _)| *ino)
}

#[cfg(feature = "writable")]
#[test]
fn test_write_at_overwrites_existing_blocks() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Create a test file with some initial data.
    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "testfile", 0o100644);
    assert!(ino.is_some(), "create should succeed");
    let ino = ino.unwrap();

    // Write initial data.
    let initial = b"initial data block!";
    let written = fs.write(ino, 0, initial);
    assert!(written.is_some() && written.unwrap() == initial.len());

    // Read existing data.
    let before = fs.read(ino, 0, 32).expect("should read file");
    assert!(!before.is_empty(), "file should have data");

    // Modify bytes at offset 0.
    let old_byte = before[0];
    let new_byte = if old_byte == 0xFF { 0x01 } else { old_byte + 1 };
    let write_data = [new_byte, b'W', b'R', b'I', b'T', b'E', b'_', b'O'];

    let written = fs.write(ino, 0, &write_data);
    assert!(written.is_some(), "write should return Some");
    assert_eq!(written.unwrap(), 8, "should write 8 bytes");

    // Read back and verify.
    let after = fs.read(ino, 0, 32).expect("should read file after write");
    assert_eq!(after[0], new_byte, "byte 0 mismatch");
    assert_eq!(&after[1..8], b"WRITE_O", "bytes 1-7 mismatch");
}

#[cfg(feature = "writable")]
#[test]
fn test_truncate_changes_size() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Create a test file with data.
    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "trunctest", 0o100644).unwrap();

    let initial = b"this is some data for truncation testing";
    let _ = fs.write(ino, 0, initial);

    let before_data = fs.read(ino, 0, jfsfuse::storage::BLOCK_SIZE).expect("should read file");
    let before_size = before_data.len() as u64;
    assert!(before_size > 0, "file should have non-zero size before truncate");

    let new_size = std::cmp::max(1, before_size / 2);
    let result = fs.truncate(ino, new_size);
    assert!(result.is_some(), "truncate should return Some on success");

    // Verify the file's size attribute changed.
    let dinode = fs.getattr(ino).expect("should getattr after truncate");
    let after_size = u64::from_le_bytes(dinode.di_size);
    assert_eq!(
        after_size, new_size,
        "inode size should be {} after truncate",
        new_size
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_fsync_succeeds() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Create a file — fsync on root.
    let ino = fs.create(fs.volume.root_ino, "fsync_test", 0o100644).unwrap();
    let _ = fs.write(ino, 0, b"data");

    let result = fs.flush(ino);
    assert!(result.is_some(), "fsync should return Some on success");

    // Verify a transaction was committed.
    let txid = fs.volume.tx_mgr.current_txid();
    assert!(txid.is_some(), "transaction should be committed after fsync");
}

#[cfg(feature = "writable")]
#[test]
fn test_write_past_eof_allocates_blocks() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Create a new file (empty, zero extent).
    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "wpe_test", 0o100644);
    assert!(ino.is_some(), "create should succeed");
    let new_ino = ino.unwrap();

    // Write data past EOF — this requires block allocation.
    let write_data = b"Hello, allocated world!";
    let written = fs.write(new_ino, 0, write_data);
    assert!(written.is_some(), "write should succeed");
    assert_eq!(written.unwrap(), write_data.len());

    // Read back and verify.
    let after = fs.read(new_ino, 0, write_data.len()).expect("should read");
    assert_eq!(after, write_data, "data should match what was written");

    // Verify the file size was updated.
    let dinode = fs.getattr(new_ino).expect("should getattr");
    let size = u64::from_le_bytes(dinode.di_size);
    assert_eq!(size, write_data.len() as u64, "size should be updated");

    // Cleanup.
    let _ = fs.unlink(parent, "wpe_test");
}
