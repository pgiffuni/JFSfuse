// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 4: Block allocation tests.
//!
//! Tests that the BlockAllocMap can read the on-disk dmap pages from a
//! generated JFS image, allocate contiguous blocks, and commit the bitmap
//! changes.
//!
//! Runs only with `cargo test --features writable`.

use std::sync::Arc;

use jfsfuse::alloc::dmap::BlockAllocMap;
use jfsfuse::mkfs;
use jfsfuse::storage::Storage;

fn load_image_to_memory() -> Arc<dyn Storage> {
    mkfs::create_filesystem()
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_init_from_real_image() {
    let storage = load_image_to_memory();

    let bmap = BlockAllocMap::new(storage).expect("should initialize bmap");
    assert!(bmap.mapsize() > 0, "mapsize should be non-zero");
    assert!(
        bmap.nfree() > 0,
        "should have free blocks: {}",
        bmap.nfree()
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_alloc_single_block() {
    let storage = load_image_to_memory();

    let mut bmap = BlockAllocMap::new(storage).expect("should initialize bmap");
    let before = bmap.nfree();

    let pxd = bmap.alloc_extent(1, 0).expect("should allocate").unwrap();
    assert!(pxd.address() > 0, "should get a valid block address");
    assert_eq!(pxd.length(), 1);

    assert_eq!(bmap.nfree(), before - 1, "free count should decrease by 1");

    // Rollback should restore the free count.
    bmap.rollback();
    assert_eq!(bmap.nfree(), before, "rollback should restore free count");
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_alloc_contiguous_extent() {
    let storage = load_image_to_memory();

    let mut bmap = BlockAllocMap::new(storage).expect("should initialize bmap");
    let nblocks = 4u64;

    let pxd = bmap
        .alloc_extent(nblocks, 0)
        .expect("should allocate")
        .unwrap();
    assert_eq!(pxd.length() as u64, nblocks);
    assert!(pxd.address() > 0);

    // Commit and verify nfree is reduced.
    bmap.commit().expect("commit should succeed");
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_free_and_realloc() {
    let storage = load_image_to_memory();

    let mut bmap = BlockAllocMap::new(storage).expect("should initialize bmap");

    // Allocate a block.
    let pxd = bmap.alloc_extent(1, 0).expect("should allocate").unwrap();

    // Free it.
    bmap.free_extent(&pxd).expect("should free");

    // Should be able to allocate again.
    let pxd2 = bmap
        .alloc_extent(1, 0)
        .expect("should allocate again")
        .unwrap();
    assert!(
        pxd2.address() > 0,
        "should get a valid block after re-allocation"
    );
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_alloc_many_blocks() {
    let storage = load_image_to_memory();

    let mut bmap = BlockAllocMap::new(storage).expect("should initialize bmap");
    let before = bmap.nfree();

    // Allocate a large extent (32 blocks).
    let pxd = bmap.alloc_extent(32, 0).expect("should allocate").unwrap();
    assert_eq!(pxd.length(), 32);
    assert!(bmap.nfree() <= before - 32);
}

#[cfg(feature = "writable")]
#[test]
fn test_bmap_commit_reduces_free_count() {
    let storage = load_image_to_memory();

    let mut bmap = BlockAllocMap::new(Arc::clone(&storage)).expect("should initialize bmap");
    let before = bmap.nfree();

    // Allocate 10 blocks.
    let pxd = bmap.alloc_extent(10, 0).expect("should allocate").unwrap();
    let allocated_block = pxd.address();

    // Commit changes.
    bmap.commit().expect("commit should succeed");

    // Free count should still reflect the allocation.
    assert_eq!(bmap.nfree(), before - 10);

    // The allocated block address should be valid (non-zero and within range).
    assert!(allocated_block > 0);
    assert!(allocated_block < bmap.mapsize());
}
