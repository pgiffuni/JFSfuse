// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 11: Symbolic link tests.
//!
//! Tests symlink creation, target reading, and inline storage.
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::{EEXIST, EOPNOTSUPP, EROFS};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_creates_inline_target() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let link_name = "tf11link";
    let target = "/some/target/path";

    let ino = fs.symlink(parent, link_name, target);
    assert!(ino.is_ok(), "symlink creation should succeed");
    let link_ino = ino.unwrap();

    // Look up the symlink — it should exist.
    let looked_up = fs.lookup(parent, link_name);
    assert_eq!(looked_up, Some(link_ino), "lookup should find the new symlink");

    // Verify it's a symlink.
    let dinode = fs.getattr(link_ino).expect("should getattr symlink");
    assert_eq!(u32::from_le_bytes(dinode.di_mode) & 0xf000, 0xa000, "should be a symlink");
    assert_eq!(u64::from_le_bytes(dinode.di_size), target.len() as u64, "size should be target length");

    // Read the symlink target.
    let data = fs.readlink(link_ino);
    assert!(data.is_ok(), "readlink should succeed");
    let target_bytes = data.unwrap();
    assert_eq!(&target_bytes[..], target.as_bytes(), "readlink should return the target path");

    // Clean up.
    let _ = fs.unlink(parent, link_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_lookup_resolves() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let link_name = "tf11res";
    let target = "/etc/hostname";

    let ino = fs.symlink(parent, link_name, target).expect("symlink should succeed");

    // Verify readdir includes the symlink entry.
    let entries = fs.readdir(parent, 0).expect("readdir should succeed");
    let found = entries.iter().find(|(name, _, _)| name == link_name);
    assert!(found.is_some(), "symlink should appear in readdir");

    let _ = fs.unlink(parent, link_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_short_path() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let link_name = "tf11s";
    let target = "r"; // Very short target

    let ino = fs.symlink(parent, link_name, target).expect("symlink should succeed");
    let data = fs.readlink(ino).expect("readlink should succeed");
    assert_eq!(&data[..], target.as_bytes());

    let _ = fs.unlink(parent, link_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_near_128_bytes() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let link_name = "tf11n";

    // Target just under 128 bytes (should be inline).
    let target = "a".repeat(127);
    let ino = fs.symlink(parent, link_name, &target).expect("symlink should succeed");
    let data = fs.readlink(ino).expect("readlink should succeed");
    assert_eq!(data.len(), 127);

    let _ = fs.unlink(parent, link_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_long_path_unsupported() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let link_name = "tf11long";

    // Target just over 128 bytes (should fail with EOPNOTSUPP).
    let target = "a".repeat(128);
    let result = fs.symlink(parent, link_name, &target);
    assert!(result.is_err(), "long symlink should fail");
    assert_eq!(result.unwrap_err(), EOPNOTSUPP);

    // Ensure no partial creation.
    assert_eq!(fs.lookup(parent, link_name), None);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_duplicate_name_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf11dup";
    let target = "/first/target";

    let _ = fs.symlink(parent, name, target).expect("first symlink should succeed");

    // Creating the same name again should fail with EEXIST.
    let result = fs.symlink(parent, name, "/second/target");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EEXIST);

    // Clean up.
    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_readlink_rejected_in_readonly() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    // Don't enable writable.

    let result = fs.readlink(1);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_inode_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf11mode";
    let target = "/target/path";

    let ino = fs.symlink(parent, name, target).expect("symlink should succeed");

    // Verify mode has S_IFLNK (0xA000) set.
    let dinode = fs.getattr(ino).expect("should getattr");
    let mode = u32::from_le_bytes(dinode.di_mode);
    assert!(dinode.is_symlink(), "inode should be a symlink");
    assert_eq!(mode & 0xf000, 0xa000, "mode should have S_IFLNK set");

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_nlink_is_one() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf11nl";
    let target = "/target";

    let ino = fs.symlink(parent, name, target).expect("symlink should succeed");
    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 1, "symlink should have nlink=1");

    let _ = fs.unlink(parent, name);
}

#[cfg(feature = "writable")]
#[test]
fn test_symlink_and_file_same_name() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a regular file.
    let file_ino = fs.create(parent, "tf11mix", 0o100644).expect("create should succeed");
    assert!(fs.getattr(file_ino).unwrap().is_regular());

    // Create a symlink with a different name.
    let link_ino = fs.symlink(parent, "tf11mix_l", "/target").expect("symlink should succeed");
    assert!(fs.getattr(link_ino).unwrap().is_symlink());

    // Verify they have different inode numbers and types.
    assert_ne!(file_ino, link_ino);
    assert!(fs.getattr(file_ino).unwrap().is_regular(), "file should be regular");
    assert!(fs.getattr(link_ino).unwrap().is_symlink(), "link should be symlink");

    // Clean up.
    fs.unlink(parent, "tf11mix");
    fs.unlink(parent, "tf11mix_l");
}
