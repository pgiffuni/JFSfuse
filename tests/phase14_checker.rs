// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 14: Consistency checker tests.
//!
//! Tests the `check_consistent()` method on a real JFS image.

use std::sync::Arc;

use jfsfuse::storage::{BLOCK_SIZE, MemoryStorage, Storage};
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Option<Volume> {
    let path = "/tmp/kilo/test_jfs.img";
    if !std::path::Path::new(path).exists() {
        return None;
    }

    let data = std::fs::read(path).ok()?;
    let num_blocks = (data.len() as u64 + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
    let mem = MemoryStorage::new(num_blocks);
    let storage: Arc<dyn Storage> = Arc::new(mem);

    let vol_blocks = data.len() / BLOCK_SIZE as usize;
    for i in 0..vol_blocks {
        let start = i * BLOCK_SIZE as usize;
        let end = start + BLOCK_SIZE as usize;
        let _ = storage.write_block(i as u64, &data[start..end]);
    }

    Some(Volume::open_from_storage(storage).expect("should mount JFS"))
}

#[test]
fn test_check_consistent_clean_image() {
    let vol = load_image_to_memory();
    let vol = match vol {
        Some(v) => v,
        None => return,
    };
    let mut vol = vol;
    let report = vol.check_consistent();
    assert!(report.is_ok(), "check_consistent should succeed");
    let report = report.unwrap();
    assert!(
        report.is_clean(),
        "clean image should have no errors: {:?}",
        report.issues
    );
    assert_eq!(report.issues.len(), 0, "clean image should have no issues");
}

#[cfg(feature = "writable")]
#[test]
fn test_check_consistent_after_create() {
    let vol = load_image_to_memory();
    let vol = match vol {
        Some(v) => v,
        None => return,
    };
    let mut vol = vol;
    let _ = vol.create_file(vol.root_ino, "tf14chk").expect("create should succeed");

    let report = vol.check_consistent().expect("check should succeed");
    // New file may still be consistent.
    if !report.is_clean() {
        // At most should be warnings about xattr or mode flags, not hard errors.
        let errors: Vec<_> = report.issues.iter().filter(|i| i.level >= 2).collect();
        // The new file has mode 0x81a4 which doesn't set INLINEEA, so no xattr issues.
        assert!(errors.is_empty(), "create should not cause consistency errors");
    }
}

#[test]
fn test_check_report_default_clean() {
    let report = jfsfuse::volume::CheckReport::default();
    assert!(report.is_clean());
}

#[test]
fn test_check_issue_creation() {
    let mut report = jfsfuse::volume::CheckReport::default();
    report.add_warning("test warning".to_string());
    assert!(report.is_clean());

    report.add_error("test error".to_string());
    assert!(!report.is_clean());
}
