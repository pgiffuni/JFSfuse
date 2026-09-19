// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 9: FUSE mutation operation tests.
//!
//! Tests `mkdir`, `rmdir`, `setattr`, `open`/`release`, and error-code mapping
//! on a generated JFS image loaded into MemoryStorage.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::{EEXIST, ENOENT, ENOTEMPTY, EROFS};
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
    let result = fs.setattr(ino, Some(0o100600), None, None, None, None);
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
    let result = fs.setattr(ino, None, Some(1000), Some(2000), None, None);
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

    let result = fs.setattr(1, Some(0o100600), None, None, None, None);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_rename_returns_eopnotsupp() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let result = fs.rename(parent, "a", parent, "b");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), 95); // EOPNOTSUPP
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

    let _ = fs.rmdir(parent, name).expect("rmdir should succeed");

    let parent_after_rmdir = fs.getattr(parent).expect("should getattr parent");
    let nlink_after_rmdir = u32::from_le_bytes(parent_after_rmdir.di_nlink);

    assert_eq!(
        nlink_after_rmdir, nlink_after_mkdir - 1,
        "rmdir should decrement parent's link count"
    );
}
