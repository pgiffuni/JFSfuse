// SPDX-License-Identifier: GPL-2.0-or-later
//! Integration test: read operations against a real JFS filesystem image.
//!
//! Requires the test image at `/tmp/kilo/test_jfs.img` (100MB, created by
//! `jfs_mkfs`). Skipped if the image does not exist.

use jfsfuse::fuse::FuseFs;
use jfsfuse::volume::Volume;

fn skip_if_no_image() -> Option<String> {
    let path = "/tmp/kilo/test_jfs.img";
    if std::path::Path::new(path).exists() {
        Some(path.to_string())
    } else {
        None
    }
}

#[test]
fn test_mount_real_jfs_image() {
    let path = match skip_if_no_image() {
        Some(p) => p,
        None => {
            eprintln!("skipping: /tmp/kilo/test_jfs.img not found");
            return;
        }
    };

    let vol = Volume::open(&path).expect("should mount JFS image");
    assert_eq!(vol.root_ino, 18);
    assert_eq!(vol.block_size, 4096);
}

#[test]
fn test_read_root_inode() {
    let path = match skip_if_no_image() {
        Some(p) => p,
        None => return,
    };

    let mut vol = Volume::open(&path).expect("should mount");
    let root = vol.root_inode().expect("should read root inode");
    assert!(root.is_dir(), "root should be a directory");
    assert_eq!(u32::from_le_bytes(root.dinode.di_mode), 0x141ed);
    assert_eq!(root.size(), 256);
}

#[test]
fn test_readdir_root() {
    let path = match skip_if_no_image() {
        Some(p) => p,
        None => return,
    };

    let vol = Volume::open(&path).expect("should mount");
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
    let path = match skip_if_no_image() {
        Some(p) => p,
        None => return,
    };

    let vol = Volume::open(&path).expect("should mount");
    let mut fs = FuseFs::new(vol);
    let root = fs.volume.root_ino;

    assert_eq!(fs.lookup(root, "."), Some(root));
    // `..` of root points to inode 2 (per dtroot idotdot)
    assert_eq!(fs.lookup(root, ".."), Some(2));
}
