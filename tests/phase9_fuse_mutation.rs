// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 9: FUSE mutation operation tests.
//!
//! Tests `mkdir`, `rmdir`, `setattr`, `open`/`release`, and error-code mapping
//! on a generated JFS image loaded into MemoryStorage.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::{EEXIST, ENOENT, ENOTEMPTY, EROFS, R_OK, W_OK, X_OK, EACCES};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_mkdir_creates_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9dir";

    let result = fs.mkdir(parent, name, 0o040755);
    assert!(result.is_ok(), "mkdir should succeed: {:?}", result.err());
    let new_ino = result.unwrap();

    // Look up the directory — it should exist.
    let dir_ino = fs.lookup(parent, name);
    assert_eq!(
        dir_ino,
        Some(new_ino),
        "lookup should find the newly created directory"
    );

    // Verify it's a directory.
    let dinode = fs.getattr(new_ino).expect("should getattr new dir");
    assert_eq!(
        u32::from_le_bytes(dinode.di_mode) & 0xf000,
        0x4000,
        "should be a directory"
    );

    // The directory should have nlink = 2 (`.` and `..`).
    assert_eq!(
        u32::from_le_bytes(dinode.di_nlink),
        2,
        "new directory should have nlink=2"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_mkdir_creates_dotdot_entries() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9dd";

    let new_ino = fs.mkdir(parent, name, 0o040755).expect("mkdir should succeed");

    // readdir should show `.` and `..` entries.
    let entries = fs.readdir(new_ino, 0).expect("readdir should succeed");
    assert!(entries.len() >= 2, "directory should have . and ..");
    assert_eq!(entries[0].0, ".", "first entry should be `.`");
    assert_eq!(entries[1].0, "..", "second entry should be `..`");
    assert_eq!(entries[1].1, parent, "parent should be the root inode");
}

#[cfg(feature = "writable")]
#[test]
fn test_mkdir_duplicate_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9dup";

    let ino1 = fs.mkdir(parent, name, 0o040755).expect("first mkdir should succeed");
    assert!(ino1 > 0);

    // Creating the same name again should fail with EEXIST.
    let err = fs.mkdir(parent, name, 0o040755).unwrap_err();
    assert_eq!(err, EEXIST, "duplicate mkdir should return EEXIST");

    // Clean up.
    let _ = fs.rmdir(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_rmdir_removes_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9rm";

    let ino = fs.mkdir(parent, name, 0o040755).expect("mkdir should succeed");

    // Verify it exists.
    assert_eq!(fs.lookup(parent, name), Some(ino));

    // Remove it.
    let result = fs.rmdir(parent, name);
    assert!(result.is_ok(), "rmdir should succeed: {:?}", result.err());

    // Verify it's gone.
    assert_eq!(fs.lookup(parent, name), None, "directory should be gone after rmdir");
}

#[cfg(feature = "writable")]
#[test]
fn test_rmdir_nonexistent_fails_with_enoent() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let err = fs.rmdir(parent, "does_not_exist").unwrap_err();
    assert_eq!(err, ENOENT, "rmdir of missing name should return ENOENT");
}

#[cfg(feature = "writable")]
#[test]
fn test_rmdir_nonempty_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let dir_name = "tf9ne";
    let file_name = "tf9ne_inner";

    // Create a directory.
    let dir_ino = fs.mkdir(parent, dir_name, 0o040755).expect("mkdir should succeed");

    // Create a file inside it.
    let _ = fs.create(dir_ino, file_name, 0o100644).expect("create should succeed");

    // Try to rmdir the non-empty directory — should fail with ENOTEMPTY.
    let err = fs.rmdir(parent, dir_name).unwrap_err();
    assert_eq!(err, ENOTEMPTY, "rmdir of non-empty dir should return ENOTEMPTY");

    // Clean up: remove the file first, then the directory.
    let _ = fs.unlink(dir_ino, file_name);
    let _ = fs.rmdir(parent, dir_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_setattr_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9attr";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    // Change mode to 0600.
    let result = fs.setattr(ino, Some(0o100600), None, None, None, None, None);
    assert!(result.is_ok(), "setattr should succeed: {:?}", result.err());

    // Verify the mode was updated.
    let dinode = fs.getattr(ino).expect("should getattr");
    let mode = u32::from_le_bytes(dinode.di_mode);
    assert_eq!(mode & 0x0fff, 0o600, "permission bits should be 0600");
    assert_eq!(mode & 0xf000, 0x8000, "file type should still be regular");

    // Clean up.
    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_setattr_uid_gid() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9ug";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    // Change uid and gid.
    let result = fs.setattr(ino, None, Some(1000), Some(2000), None, None, None);
    assert!(result.is_ok(), "setattr should succeed: {:?}", result.err());

    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_uid), 1000);
    assert_eq!(u32::from_le_bytes(dinode.di_gid), 2000);

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_open_returns_enoent_for_missing_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let result = fs.open(999999);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), ENOENT);
}

#[cfg(feature = "writable")]
#[test]
fn test_open_succeeds_for_existing_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9open";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    let result = fs.open(ino);
    assert!(result.is_ok(), "open should succeed for existing inode");

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_release_is_noop() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9rel";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    let result = fs.release(ino);
    assert!(result.is_ok(), "release should succeed");

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_write_operations_rejected_in_readonly_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    // Don't call enable_writable — should be read-only by default.

    let parent = fs.volume.root_ino;

    let result = fs.mkdir(parent, "ro_test", 0o040755);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_setattr_rejected_in_readonly_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    // Don't call enable_writable.

    let result = fs.setattr(1, Some(0o100600), None, None, None, None, None);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_file_in_same_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a file named "foo".
    let ino = fs.create(parent, "foo", 0o100644).expect("create should succeed");

    // Rename "foo" to "bar" in the same directory.
    let result = fs.rename(parent, "foo", parent, "bar");
    assert!(result.is_ok());

    // "foo" should no longer exist.
    assert_eq!(fs.lookup(parent, "foo"), None);
    // "bar" should exist with the same inode.
    assert_eq!(fs.lookup(parent, "bar"), Some(ino));
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_to_existing_overwrites_file() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    let ino_a = fs.create(parent, "a", 0o100644).expect("create should succeed");
    let _ino_b = fs.create(parent, "b", 0o100644).expect("create should succeed");

    // Rename "a" to "b", overwriting "b".
    let result = fs.rename(parent, "a", parent, "b");
    assert!(result.is_ok());

    // "a" should no longer exist.
    assert_eq!(fs.lookup(parent, "a"), None);
    // "b" should now point to ino_a (the renamed file).
    assert_eq!(fs.lookup(parent, "b"), Some(ino_a));
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_nonexistent_source_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    let result = fs.rename(parent, "nonexistent", parent, "newname");
    assert!(result.is_err());
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_rejects_readonly() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    let result = fs.rename(parent, "a", parent, "b");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_directory_same_parent() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a directory named "mydir".
    let ino = fs.mkdir(parent, "mydir", 0o040755).expect("mkdir should succeed");

    // Rename "mydir" to "yourdir" in the same directory.
    let result = fs.rename(parent, "mydir", parent, "yourdir");
    assert!(result.is_ok());

    // "mydir" should no longer exist.
    assert_eq!(fs.lookup(parent, "mydir"), None);
    // "yourdir" should exist with the same inode.
    assert_eq!(fs.lookup(parent, "yourdir"), Some(ino));
}

#[cfg(feature = "writable")]
#[test]
fn test_mkdir_increments_parent_nlink() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9nl";

    // Get parent's initial nlink.
    let parent_before = fs.getattr(parent).expect("should getattr parent");
    let nlink_before = u32::from_le_bytes(parent_before.di_nlink);

    let _ = fs.mkdir(parent, name, 0o040755).expect("mkdir should succeed");

    // Get parent's nlink after mkdir.
    let parent_after = fs.getattr(parent).expect("should getattr parent");
    let nlink_after = u32::from_le_bytes(parent_after.di_nlink);

    assert_eq!(
        nlink_after, nlink_before + 1,
        "mkdir should increment parent's link count"
    );

    // Clean up.
    let _ = fs.rmdir(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_rmdir_decrements_parent_nlink() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf9dl";

    let _ = fs.mkdir(parent, name, 0o040755).expect("mkdir should succeed");

    let parent_after_mkdir = fs.getattr(parent).expect("should getattr parent");
    let nlink_after_mkdir = u32::from_le_bytes(parent_after_mkdir.di_nlink);

    fs.rmdir(parent, name).expect("rmdir should succeed");

    let parent_after_rmdir = fs.getattr(parent).expect("should getattr parent");
    let nlink_after_rmdir = u32::from_le_bytes(parent_after_rmdir.di_nlink);

    assert_eq!(
        nlink_after_rmdir, nlink_after_mkdir - 1,
        "rmdir should decrement parent's link count"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_basic() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create source file with data.
    let src_ino = fs.create(parent, "srcfile", 0o100644).expect("create src");

    // Write initial data to source.
    let data = b"Hello, fusejfs!";
    let written = fs.write(src_ino, 0, data).expect("write src");
    assert_eq!(written, data.len());

    // Read back from source to verify write worked.
    let src_read = fs.read(src_ino, 0, data.len()).expect("read src for verify");
    assert_eq!(&src_read, data, "source data should match after write");

    // Create destination file.
    let dst_ino = fs.create(parent, "dstfile", 0o100644).expect("create dst");

    // Copy from src to dst.
    let copied = fs.copy_file_range(src_ino, 0, dst_ino, 0, data.len(), 0);
    assert!(copied.is_ok(), "copy_file_range should succeed: {:?}", copied.err());
    assert_eq!(copied.unwrap(), data.len(), "should copy all bytes");

    // Verify destination content.
    let read_back = fs.read(dst_ino, 0, data.len()).expect("should read file");
    assert_eq!(&read_back[..], data, "destination content should match");
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_with_move() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let src_ino = fs.create(parent, "movsrc", 0o100644).expect("create src");
    let data = b"Movable data!";
    fs.write(src_ino, 0, data).expect("write src");

    let dst_ino = fs.create(parent, "iovdst", 0o100644).expect("create dst");

    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 0, dst_ino, 0, data.len(), FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "copy with MOVE should succeed");

    // Destination should have the data.
    let read_dst = fs.read(dst_ino, 0, data.len()).expect("read dst");
    assert_eq!(&read_dst[..], data, "destination content should match");

    // Source should have a hole (zero-filled).
    let read_src = fs.read(src_ino, 0, data.len()).expect("read src after move");
    assert!(
        read_src.iter().all(|&b| b == 0),
        "source should be zeroed after MOVE"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_same_file() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.create(parent, "same", 0o100644).expect("create");
    fs.write(ino, 0, b"test").expect("write");

    // Copying with same inode and offset should fail.
    let result = fs.copy_file_range(ino, 0, ino, 0, 4, 0);
    assert!(result.is_err(), "should reject same-inode copy");
    assert_eq!(result.unwrap_err(), jfsfuse::fuse::EINVAL);
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_large_copy() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a file with larger-than-64KB data.
    let src_ino = fs.create(parent, "bigsrc", 0o100644).expect("create src");
    let large_data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
    let written = fs.write(src_ino, 0, &large_data).expect("write src");
    assert_eq!(written, large_data.len());
    fs.flush(src_ino).expect("flush src");

    // Create destination file.
    let dst_ino = fs.create(parent, "bigdst", 0o100644).expect("create dst");

    // Copy the full 200KB — should use internal looping, not just one 64KB chunk.
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 0, dst_ino, 0, large_data.len(), FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "copy_file_range should succeed: {:?}", copied.err());
    assert_eq!(
        copied.unwrap(),
        large_data.len(),
        "should copy all bytes (not just one 64KB chunk)"
    );

    // Verify destination content matches.
    let read_back = fs.read(dst_ino, 0, large_data.len()).expect("should read dst");
    assert_eq!(&read_back[..], &large_data[..], "destination content should match source");
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_move_overwrites_destination() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Source file with data.
    let src_ino = fs.create(parent, "src", 0o100644).expect("create src");
    let src_data = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    fs.write(src_ino, 0, src_data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    // Destination file with different data — will be overwritten.
    let dst_ino = fs.create(parent, "dst", 0o100644).expect("create dst");
    let dst_data = b"XXXXXXXXXXXXXXXXXXXXX";
    fs.write(dst_ino, 0, dst_data).expect("write dst");
    fs.flush(dst_ino).expect("flush dst");

    // MOVE data from src to dst (overwriting dst content).
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 0, dst_ino, 0, src_data.len(), FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "copy_file_range MOVE should succeed: {:?}", copied.err());
    assert_eq!(copied.unwrap(), src_data.len());

    // Destination should now contain source data.
    let read_dst = fs.read(dst_ino, 0, src_data.len()).expect("read dst");
    assert_eq!(&read_dst[..], src_data, "destination should contain source data after MOVE");

    // Source should have holes (data was moved, not copied).
    let read_src = fs.read(src_ino, 0, src_data.len()).expect("read src after MOVE");
    assert!(
        read_src.iter().all(|&b| b == 0),
        "source should be zeroed after MOVE, got: {:?}",
        read_src
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_move_preserves_source_size() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Source file with data.
    let src_ino = fs.create(parent, "srcsize", 0o100644).expect("create src");
    let data = b"Some test data for size preservation";
    fs.write(src_ino, 0, data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    let src_attr_before = fs.getattr(src_ino).expect("getattr src");
    let src_size_before = u64::from_le_bytes(src_attr_before.di_size);

    // Destination.
    let dst_ino = fs.create(parent, "dstsize", 0o100644).expect("create dst");

    // MOVE — should preserve source file size (data moved, not deleted).
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 0, dst_ino, 0, data.len(), FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "MOVE should succeed: {:?}", copied.err());

    // Source size should be unchanged (MOVE keeps file size).
    let src_attr_after = fs.getattr(src_ino).expect("getattr src after");
    let src_size_after = u64::from_le_bytes(src_attr_after.di_size);
    assert_eq!(
        src_size_after, src_size_before,
        "source file size should be preserved after MOVE"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_block_aligned_move() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Write a full block of data (4096 bytes, block-aligned).
    let src_ino = fs.create(parent, "alignsrc", 0o100644).expect("create src");
    let block_data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    fs.write(src_ino, 0, &block_data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    let dst_ino = fs.create(parent, "aligndst", 0o100644).expect("create dst");

    // Block-aligned MOVE — should use the optimized extent-stealing path.
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 0, dst_ino, 0, 4096, FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "aligned MOVE should succeed: {:?}", copied.err());
    assert_eq!(copied.unwrap(), 4096);

    // Verify destination data matches.
    let read_dst = fs.read(dst_ino, 0, 4096).expect("read dst");
    assert_eq!(&read_dst[..], &block_data[..], "destination data should match");

    // Source should have holes.
    let read_src = fs.read(src_ino, 0, 4096).expect("read src after MOVE");
    assert!(
        read_src.iter().all(|&b| b == 0),
        "source should be zeroed after aligned MOVE"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_non_move_does_not_punch_source() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    let src_ino = fs.create(parent, "srcnm", 0o100644).expect("create src");
    let data = b"Non-move copy data";
    fs.write(src_ino, 0, data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    let dst_ino = fs.create(parent, "dstnm", 0o100644).expect("create dst");

    // Non-MOVE copy (flags=0) — source data should be preserved.
    let copied = fs.copy_file_range(src_ino, 0, dst_ino, 0, data.len(), 0);
    assert!(copied.is_ok());
    assert_eq!(copied.unwrap(), data.len());

    // Destination should contain the data.
    let read_dst = fs.read(dst_ino, 0, data.len()).expect("read dst");
    assert_eq!(&read_dst[..], data, "destination should contain source data");

    // Source should still have its original data (copy, not move).
    let read_src = fs.read(src_ino, 0, data.len()).expect("read src");
    assert_eq!(&read_src[..], data, "source should retain data after copy");
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_partial_offset_copy() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create source with data at known positions.
    let src_ino = fs.create(parent, "partsrc", 0o100644).expect("create src");
    let data = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    fs.write(src_ino, 0, data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    let dst_ino = fs.create(parent, "partdst", 0o100644).expect("create dst");

    // Copy from offset 5, length 10 (non-block-aligned) — should fall back
    // to read/write path and copy exactly the right bytes.
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(
        src_ino, 5, dst_ino, 0, 10, FUSE_COPY_FILE_RANGE_MOVE,
    );
    assert!(copied.is_ok(), "copy should succeed: {:?}", copied.err());
    assert_eq!(copied.unwrap(), 10);

    // Destination should contain bytes 5..15 from source.
    let read_dst = fs.read(dst_ino, 0, 10).expect("read dst");
    assert_eq!(&read_dst[..], &data[5..15], "destination should contain partial source data");
}

#[cfg(feature = "writable")]
#[test]
fn test_copy_file_range_move_destination_extends_size() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Small source file.
    let src_ino = fs.create(parent, "srcsmall", 0o100644).expect("create src");
    let data = b"small";
    fs.write(src_ino, 0, data).expect("write src");
    fs.flush(src_ino).expect("flush src");

    // Empty destination.
    let dst_ino = fs.create(parent, "dstdst", 0o100644).expect("create dst");

    // MOVE to destination at a non-zero offset — extends destination size.
    const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
    let copied = fs.copy_file_range(src_ino, 0, dst_ino, 100, data.len(), FUSE_COPY_FILE_RANGE_MOVE);
    assert!(copied.is_ok(), "MOVE should succeed: {:?}", copied.err());
    assert_eq!(copied.unwrap(), data.len());

    // Destination size should reflect the offset + data length.
    let dst_attr = fs.getattr(dst_ino).expect("getattr dst");
    let dst_size = u64::from_le_bytes(dst_attr.di_size);
    assert_eq!(dst_size, 100 + data.len() as u64, "destination size should be 100 + data len");

    // Data at offset 100 should match source.
    let read_dst = fs.read(dst_ino, 100, data.len()).expect("read dst at offset");
    assert_eq!(&read_dst[..], data, "destination data at offset should match");
}

#[cfg(feature = "writable")]
#[test]
fn test_mknod_creates_fifo() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // S_IFIFO | 0644 = 0x1000 | 0x1A4 = 0x11A4
    let ino = fs.mknod(parent, "fifo", 0x11A4, 0).expect("mknod should succeed");

    // The inode should exist and be a FIFO.
    let dinode = fs.getattr(ino).expect("should getattr fifo");
    let mode = u32::from_le_bytes(dinode.di_mode);
    assert_eq!(mode & 0xf000, 0x1000, "mode should be S_IFIFO");
    assert_eq!(mode & 0o777, 0o644, "permission bits should be 0644");

    // The entry should be visible via lookup.
    assert_eq!(fs.lookup(parent, "fifo"), Some(ino));
}

#[cfg(feature = "writable")]
#[test]
fn test_mknod_rejects_regular_file_type() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // S_IFREG should be rejected by mknod (use create instead).
    let result = fs.mknod(parent, "reg", 0x81A4, 0);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), jfsfuse::fuse::EPERM);
}

#[cfg(feature = "writable")]
#[test]
fn test_mknod_rejects_directory_type() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // S_IFDIR should be rejected by mknod (use mkdir instead).
    let result = fs.mknod(parent, "dir", 0x41ED, 0);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), jfsfuse::fuse::EPERM);
}

#[cfg(feature = "writable")]
#[test]
fn test_mknod_rejects_readonly() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);

    let parent = fs.volume.root_ino;
    let result = fs.mknod(parent, "fifo", 0x11A4, 0);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_forget_tracks_nlookup_refcount() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a file — FUSEFs::create increments lookup_count to 1.
    let ino = fs.create(parent, "f", 0o100644).expect("create");
    assert_eq!(fs.lookup_count(ino), 1);

    // Explicit lookup increments to 2.
    let _ = fs.lookup(parent, "f");
    assert_eq!(fs.lookup_count(ino), 2);

    // Partial FORGET: decrement by 1, count should be 1 (not zero).
    fs.forget(ino, 1);
    assert_eq!(fs.lookup_count(ino), 1);

    // Full FORGET: decrement by 1 more, count reaches 0.
    fs.forget(ino, 1);
    assert_eq!(fs.lookup_count(ino), 0);
}

#[cfg(feature = "writable")]
#[test]
fn test_batch_forget_decrements_properly() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create two files (count = 1 each).
    let ino1 = fs.create(parent, "f1", 0o100644).expect("create f1");
    let ino2 = fs.create(parent, "f2", 0o100644).expect("create f2");
    assert_eq!(fs.lookup_count(ino1), 1);
    assert_eq!(fs.lookup_count(ino2), 1);

    // Look up both (count = 2 each).
    let _ = fs.lookup(parent, "f1");
    let _ = fs.lookup(parent, "f2");
    assert_eq!(fs.lookup_count(ino1), 2);
    assert_eq!(fs.lookup_count(ino2), 2);

    // Batch forget with partial counts (2 -> 1 for each).
    fs.batch_forget(&[(ino1, 1), (ino2, 1)]);
    assert_eq!(fs.lookup_count(ino1), 1);
    assert_eq!(fs.lookup_count(ino2), 1);

    // Batch forget with full counts (1 -> 0 for each).
    fs.batch_forget(&[(ino1, 1), (ino2, 1)]);
    assert_eq!(fs.lookup_count(ino1), 0);
    assert_eq!(fs.lookup_count(ino2), 0);

    // Empty batch forget should not panic.
    fs.batch_forget(&[]);
}

#[cfg(feature = "writable")]
#[test]
fn test_forget_unknown_inode_is_noop() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    // Forgetting an inode that was never looked up should be a no-op.
    // Should not panic.
    fs.forget(99999, 1);
    assert_eq!(fs.lookup_count(99999), 0);
    fs.batch_forget(&[(99999, 1)]);
    assert_eq!(fs.lookup_count(99999), 0);
}

#[cfg(feature = "writable")]
#[test]
fn test_access_symlink_always_allows() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let _ino = fs.symlink(parent, "link", "/tmp/target").expect("symlink");
    let link_ino = fs.lookup(parent, "link").expect("should find symlink");

    // ACCESS on a symlink should always succeed (symlink mode is 0777).
    assert!(fs.access(link_ino, R_OK | W_OK | X_OK, 1000, 1000).is_ok());
}

#[cfg(feature = "writable")]
#[test]
fn test_access_directory_search_permission() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let ino = fs.mkdir(parent, "dir", 0o040700).expect("mkdir");
    fs.setattr(ino, Some(0o040700), Some(1000), Some(1000), None, None, None).expect("setattr");

    // Owner has execute (search) permission on directory.
    assert!(fs.access(ino, X_OK, 1000, 1000).is_ok());

    // Another user without search permission.
    assert_eq!(fs.access(ino, X_OK, 2000, 2000), Err(EACCES));
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_cross_directory_with_overwrite() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let root = fs.volume.root_ino;

    // Create two subdirectories.
    let dir1 = fs.mkdir(root, "dir1", 0o040755).expect("mkdir dir1");
    let dir2 = fs.mkdir(root, "dir2", 0o040755).expect("mkdir dir2");

    // Create "source" in dir1 with data.
    let src_ino = fs.create(dir1, "source", 0o100644).expect("create source");
    fs.write(src_ino, 0, b"moved data").expect("write source");

    // Create "target" in dir2 (will be overwritten by rename).
    let tgt_ino = fs.create(dir2, "target", 0o100644).expect("create target");
    fs.write(tgt_ino, 0, b"old data").expect("write target");

    // Cross-directory rename: dir1/source -> dir2/target (overwrite).
    let result = fs.rename(dir1, "source", dir2, "target");
    assert!(result.is_ok(), "cross-dir rename should succeed: {:?}", result.err());

    // "source" should be gone from dir1.
    assert_eq!(fs.lookup(dir1, "source"), None);

    // "target" in dir2 should now point to src_ino (the renamed file).
    assert_eq!(fs.lookup(dir2, "target"), Some(src_ino));

    // src_ino should have its original data (not the target's data).
    let data = fs.read(src_ino, 0, 100).expect("read renamed file");
    assert_eq!(&data[..], b"moved data", "renamed file should retain source data");
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_cross_directory_simple_move() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let root = fs.volume.root_ino;

    let dir1 = fs.mkdir(root, "cd_a", 0o040755).expect("mkdir dir1");
    let dir2 = fs.mkdir(root, "cd_b", 0o040755).expect("mkdir dir2");

    let src_ino = fs.create(dir1, "file", 0o100644).expect("create file");
    let data = b"cross dir move data";
    fs.write(src_ino, 0, data).expect("write file");

    // Move from dir1 to dir2 (no existing target).
    let result = fs.rename(dir1, "file", dir2, "moved");
    assert!(result.is_ok(), "cross-dir rename should succeed: {:?}", result.err());

    // Source name gone from dir1.
    assert_eq!(fs.lookup(dir1, "file"), None);
    // Dest name present in dir2 with same inode.
    assert_eq!(fs.lookup(dir2, "moved"), Some(src_ino));

    // Data preserved.
    let read_back = fs.read(src_ino, 0, data.len()).expect("read after rename");
    assert_eq!(&read_back[..], data, "data should be preserved after cross-dir rename");
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_cross_directory_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let root = fs.volume.root_ino;

    let dir1 = fs.mkdir(root, "xr1", 0o040755).expect("mkdir dir1");
    let dir2 = fs.mkdir(root, "xr2", 0o040755).expect("mkdir dir2");

    // Create a subdirectory in dir1 with a file inside.
    let subdir = fs.mkdir(dir1, "subdir", 0o040755).expect("mkdir subdir");
    let _ = fs.create(subdir, "inner", 0o100644).expect("create inner file");

    // Rename the subdirectory across directories.
    let result = fs.rename(dir1, "subdir", dir2, "subdir2");
    assert!(result.is_ok(), "cross-dir dir rename should succeed: {:?}", result.err());

    // Should be gone from dir1.
    assert_eq!(fs.lookup(dir1, "subdir"), None);
    // Should be present in dir2.
    assert_eq!(fs.lookup(dir2, "subdir2"), Some(subdir));

    // Inner file should still exist.
    assert!(fs.lookup(subdir, "inner").is_some(), "inner file should survive dir move");
}
