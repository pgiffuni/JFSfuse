// SPDX-License-Identifier: GPL-2.0-or-later
//! Phase 14: Consistency checker tests.
//!
//! Tests the `check_consistent()` method on a generated JFS image.

use std::sync::Arc;

use jfsfuse::mkfs;
use jfsfuse::storage::Storage;
use jfsfuse::volume::Volume;

fn load_image_to_memory() -> Volume {
    let storage: Arc<dyn Storage> = mkfs::create_filesystem();
    Volume::open_from_storage(storage).expect("should mount generated JFS image")
}

#[test]
fn test_check_consistent_clean_image() {
    let vol = load_image_to_memory();
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
    let mut vol = vol;
    let _ = vol
        .create_file(vol.root_ino, "tf14chk")
        .expect("create should succeed");

    let report = vol.check_consistent().expect("check should succeed");
    // New file may still be consistent.
    if !report.is_clean() {
        // At most should be warnings about xattr or mode flags, not hard errors.
        let errors: Vec<_> = report.issues.iter().filter(|i| i.level >= 2).collect();
        // The new file has mode 0x81a4 which doesn't set INLINEEA, so no xattr issues.
        assert!(
            errors.is_empty(),
            "create should not cause consistency errors"
        );
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
