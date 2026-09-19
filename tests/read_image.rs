// SPDX-License-Identifier: GPL-2.0-or-later
//! Integration test: read operations against a JFS filesystem image.
//!
//! Uses `jfsfuse::mkfs::create_filesystem()` to generate a valid in-memory
//! JFS image, replacing the need for `/tmp/kilo/test_jfs.img`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_test_volume() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[test]
fn test_mount_real_jfs_image() {
    let vol = load_test_volume();
    assert_eq!(vol.root_ino, 18);
    assert_eq!(vol.block_size, 4096);
}

#[test]
fn test_read_root_inode() {
    let mut vol = load_test_volume();
    let root = vol.root_inode().expect("should read root inode");
    assert!(root.is_dir(), "root should be a directory");
    assert_eq!(u32::from_le_bytes(root.dinode.di_mode), 0x141ed);
    assert_eq!(root.size(), 256);
}

#[test]
fn test_readdir_root() {
    let vol = load_test_volume();
    let mut fs = FuseFs::new(vol);

    let entries = fs
        .readdir(fs.volume.root_ino, 0)
        .expect("readdir should succeed");

    // Root should at least have . and ..
    assert!(entries.len() >= 2, "root should have . and ..");
    assert_eq!(entries[0].0, ".");
    assert_eq!(entries[0].1, fs.volume.root_ino);
    assert_eq!(entries[1].0, "..");
    assert_eq!(entries[1].2, 0x04); // DT_DIR
}

#[test]
fn test_lookup_dot_and_dotdot() {
    let vol = load_test_volume();
    let mut fs = FuseFs::new(vol);
    let root = fs.volume.root_ino;

    assert_eq!(fs.lookup(root, "."), Some(root));
    // `..` of root points to inode 2 (per dtroot idotdot)
    assert_eq!(fs.lookup(root, ".."), Some(2));
}
