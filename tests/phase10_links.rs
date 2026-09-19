// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 10: Hard link and open-unlinked semantics tests.
//!
//! Tests that:
//! - Hard links increment/decrement inode nlink correctly.
//! - Unlinking a file with open handles defers inode freeing.
//! - The inode is freed only when nlink reaches 0 AND no open handles remain.
//! - Hard links to directories are rejected.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::fuse::FuseFs;
use jfsfuse::fuse::{EEXIST, EPERM, EROFS};
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[cfg(feature = "writable")]
#[test]
fn test_hard_link_increments_nlink() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf10src";

    // Create a file.
    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    // Verify nlink is 1.
    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 1, "file should have nlink=1");

    // Create a hard link.
    let link_name = "tf10link";
    let result = fs.link(parent, link_name, ino);
    assert!(result.is_ok(), "link should succeed");

    // Verify nlink is now 2.
    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 2, "file should have nlink=2 after link");

    // Both names should resolve to the same inode.
    assert_eq!(fs.lookup(parent, name), Some(ino));
    assert_eq!(fs.lookup(parent, link_name), Some(ino));

    // Clean up: remove both links.
    fs.unlink(parent, name);
    fs.unlink(parent, link_name);
}

#[cfg(feature = "writable")]
#[test]
fn test_hard_link_rejects_directory() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a directory.
    let dir_ino = fs.mkdir(parent, "tf10dir", 0o040755).expect("mkdir should succeed");

    // Try to hard-link the directory — should fail with EPERM.
    let result = fs.link(parent, "tf10dir_link", dir_ino);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EPERM, "hard-linking a directory should return EPERM");

    // Clean up.
    let _ = fs.rmdir(parent, "tf10dir");
}

#[cfg(feature = "writable")]
#[test]
fn test_hard_link_duplicate_name_fails() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    let ino = fs.create(parent, "tf10src2", 0o100644).expect("create should succeed");

    // First link succeeds.
    assert!(fs.link(parent, "tf10link1", ino).is_ok());

    // Second link with the same name should fail with EEXIST.
    let result = fs.link(parent, "tf10link1", ino);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EEXIST, "duplicate link should return EEXIST");

    // Clean up.
    fs.unlink(parent, "tf10src2");
    fs.unlink(parent, "tf10link1");
}

#[cfg(feature = "writable")]
#[test]
fn test_unlink_decrements_nlink() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    // Create a file and a hard link.
    let ino = fs.create(parent, "tf10uln", 0o100644).expect("create should succeed");
    assert!(fs.link(parent, "tf10uln2", ino).is_ok());

    // Verify nlink is 2.
    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 2);

    // Unlink one name — nlink should decrement to 1, but inode still exists.
    fs.unlink(parent, "tf10uln").expect("unlink should succeed");

    // The other name should still resolve to the same inode.
    assert_eq!(fs.lookup(parent, "tf10uln2"), Some(ino));
    let dinode = fs.getattr(ino).expect("should still getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 1, "nlink should be 1 after one unlink");

    // Clean up.
    fs.unlink(parent, "tf10uln2");
}

#[cfg(feature = "writable")]
#[test]
fn test_open_unlinked_retains_inode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf10oun";

    // Create a file and write some data.
    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");
    fs.write(ino, 0, b"Hello, open-unlinked world!").expect("write should succeed");

    // Open the file (increments open-handle count).
    fs.open(ino).expect("open should succeed");

    // Unlink the file while it's open.
    fs.unlink(parent, name).expect("unlink should succeed");

    // The name should be gone.
    assert_eq!(fs.lookup(parent, name), None, "name should be gone after unlink");

    // But the inode data should still be accessible via the open handle.
    let dinode = fs.getattr(ino).expect("inode should still exist while open");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 0, "nlink should be 0 after unlink");

    // We can still read the data.
    let data = fs.read(ino, 0, 28).expect("read should succeed");
    assert_eq!(&data[..], b"Hello, open-unlinked world!", "data should still be readable");

    // Release the open handle — this should free the inode.
    fs.release(ino).expect("release should succeed");

    // Clean up: parent is root, no cleanup needed for the freed inode.
}

#[cfg(feature = "writable")]
#[test]
fn test_link_rejected_in_readonly_mode() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    // Don't enable writable.

    let result = fs.link(1, "link", 2);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), EROFS);
}

#[cfg(feature = "writable")]
#[test]
fn test_open_release_multiple_handles() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;
    let name = "tf10mul";

    let ino = fs.create(parent, name, 0o100644).expect("create should succeed");

    // Open twice — should increment handle count to 2.
    fs.open(ino).expect("first open should succeed");
    fs.open(ino).expect("second open should succeed");

    // Unlink while open (nlink → 0).
    fs.unlink(parent, name).expect("unlink should succeed");

    // Release one handle — inode should persist.
    fs.release(ino).expect("first release should succeed");
    assert!(fs.getattr(ino).is_some(), "inode should persist after partial release");

    // Release second handle — inode should be freed.
    fs.release(ino).expect("second release should succeed");

    // Clean up: the inode is now freed.
}

#[cfg(feature = "writable")]
#[test]
fn test_multiple_hard_links() {
    let vol = load_image_to_memory();
    let mut fs = FuseFs::new(vol);
    fs.enable_writable().unwrap();

    let parent = fs.volume.root_ino;

    let ino = fs.create(parent, "tf10mhs", 0o100644).expect("create should succeed");
    fs.write(ino, 0, b"multi-link data").expect("write should succeed");

    // Create 3 hard links.
    for i in 0..3 {
        let link_name = format!("tf10hs_{}", i);
        assert!(fs.link(parent, &link_name, ino).is_ok());
    }

    // nlink should be 4 (original + 3 links).
    let dinode = fs.getattr(ino).expect("should getattr");
    assert_eq!(u32::from_le_bytes(dinode.di_nlink), 4);

    // Clean up: remove all 4 links.
    fs.unlink(parent, "tf10mhs");
    for i in 0..3 {
        let link_name = format!("tf10hs_{}", i);
        fs.unlink(parent, &link_name);
    }
}
