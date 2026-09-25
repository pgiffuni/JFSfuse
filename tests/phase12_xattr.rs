// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 12: Extended attribute tests.
//!
//! Tests xattr set/get/list/remove operations on a generated JFS image.
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::{EEXIST, ENOENT, EROFS};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_setxattr_and_getxattr() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12xattr";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Set an xattr.
    let set_result = fs.setxattr(ino, "user.comment", b"hello xattr", 0);
    assert!(
        set_result.is_ok(),
        "setxattr should succeed: {:?}",
        set_result.err()
    );

    // Get the xattr back.
    let get_result = fs.getxattr(ino, "user.comment");
    assert!(get_result.is_ok(), "getxattr should succeed");
    let value = get_result.unwrap();
    assert_eq!(value, Some(b"hello xattr".to_vec()));
}

#[cfg(feature = "writable")]
#[test]
fn test_setxattr_replace() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12rep";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Set initial value.
    fs.setxattr(ino, "user.key", b"old", 0)
        .expect("setxattr should succeed");

    // Replace with XATTR_CREATE (should fail — exists).
    let result = fs.setxattr(ino, "user.key", b"new", 1);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EEXIST);

    // Replace with XATTR_REPLACE (should succeed — exists).
    fs.setxattr(ino, "user.key", b"new", 2)
        .expect("XATTR_REPLACE should succeed");

    // Verify the value was replaced.
    let value = fs
        .getxattr(ino, "user.key")
        .expect("getxattr should succeed");
    assert_eq!(value, Some(b"new".to_vec()));

    // XATTR_REPLACE on a non-existent xattr should fail with ENOENT.
    let result = fs.setxattr(ino, "user.missing", b"val", 2);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), ENOENT);
}

#[cfg(feature = "writable")]
#[test]
fn test_listxattr() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12list";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Set multiple xattrs.
    fs.setxattr(ino, "user.a", b"1", 0)
        .expect("setxattr a should succeed");
    fs.setxattr(ino, "user.b", b"22", 0)
        .expect("setxattr b should succeed");
    fs.setxattr(ino, "user.c", b"333", 0)
        .expect("setxattr c should succeed");

    // List xattr names.
    let list = fs.listxattr(ino).expect("listxattr should succeed");
    assert_eq!(list.len(), 3, "should have 3 xattrs");
    assert!(list.contains(&"user.a".to_string()));
    assert!(list.contains(&"user.b".to_string()));
    assert!(list.contains(&"user.c".to_string()));
}

#[cfg(feature = "writable")]
#[test]
fn test_removexattr() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12rm";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Set an xattr.
    fs.setxattr(ino, "user.temp", b"data", 0)
        .expect("setxattr should succeed");

    // List — should have 1.
    let list = fs.listxattr(ino).expect("listxattr should succeed");
    assert_eq!(list.len(), 1);

    // Remove it.
    let result = fs.removexattr(ino, "user.temp");
    assert!(result.is_ok(), "removexattr should succeed");

    // List — should be empty.
    let list = fs.listxattr(ino).expect("listxattr should succeed");
    assert_eq!(list.len(), 0, "should have 0 xattrs after removal");

    // Getting the removed xattr should return None.
    let value = fs
        .getxattr(ino, "user.temp")
        .expect("getxattr should succeed");
    assert!(value.is_none(), "removed xattr should return None");

    // Removing a non-existent xattr should fail with ENOENT.
    let result = fs.removexattr(ino, "user.nope");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), ENOENT);
}

#[cfg(feature = "writable")]
#[test]
fn test_xattr_rejected_in_readonly_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    // Don't enable writable.

    let result = fs.setxattr(1, "user.test", b"val", 0);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);

    let result = fs.getxattr(1, "user.test");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_xattr_get_nonexistent() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12get";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Getting a non-existent xattr should return None (not an error).
    let value = fs
        .getxattr(ino, "user.nonexistent")
        .expect("getxattr should succeed");
    assert!(value.is_none(), "non-existent xattr should return None");

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_xattr_long_name_rejected() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12long";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // xattr name > 255 bytes should fail (EIO fallback).
    let long_name = "user.".to_string() + &"x".repeat(300);
    let result = fs.setxattr(ino, &long_name, b"val", 0);
    assert!(result.is_err());

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_xattr_multiple_on_same_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf12multi";
    let ino = fs
        .create(parent, name, 0o100644)
        .expect("create should succeed");

    // Set multiple xattrs.
    fs.setxattr(ino, "user.first", b"val1", 0)
        .expect("setxattr should succeed");
    fs.setxattr(ino, "user.second", b"val2", 0)
        .expect("setxattr should succeed");

    // Both should be retrievable.
    assert_eq!(
        fs.getxattr(ino, "user.first").unwrap(),
        Some(b"val1".to_vec())
    );
    assert_eq!(
        fs.getxattr(ino, "user.second").unwrap(),
        Some(b"val2".to_vec())
    );

    // Remove one, verify the other remains.
    fs.removexattr(ino, "user.first")
        .expect("removexattr should succeed");
    assert!(fs.getxattr(ino, "user.first").unwrap().is_none());
    assert_eq!(
        fs.getxattr(ino, "user.second").unwrap(),
        Some(b"val2".to_vec())
    );

    let _ = fs.unlink(parent, name);
}
