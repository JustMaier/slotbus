//! An orphaned backing file must not freeze a region at a stale size.
//!
//! On Windows `shared_memory` backs each mapping with a file under
//! `%TEMP%\shared_memory-rs\`, and its cleanup only runs from `Drop`. A hard
//! kill therefore leaves the file behind, and every later run sees
//! `MappingIdExists` for a name nobody actually holds.
//!
//! Observed in production: backing files dated eight weeks earlier still
//! pinning `hub-agent-rsp-1` at 4096 bytes, surviving dozens of restarts, which
//! is what produced
//!
//!     overflow region 'hub-agent-rsp-1' too small for write:
//!     need 76533, have 4096 (stale same-name mapping still open)
//!
//! Nothing was open. The file was on disk.

#![cfg(windows)]

use slotbus::region::ShmRegion;

fn backing_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("shared_memory-rs").join(name)
}

/// Simulate a killed process: a backing file of `size` bytes with no live
/// owner, exactly what `Drop` failing to run leaves behind.
fn plant_orphan(name: &str, size: usize) {
    let path = backing_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, vec![0u8; size]).unwrap();
    assert!(path.is_file(), "orphan should exist before the test runs");
}

fn cleanup(name: &str) {
    let _ = std::fs::remove_file(backing_path(name));
}

#[test]
fn orphaned_backing_file_is_reclaimed_not_adopted() {
    let name = "slotbus-test-orphan-reclaim";
    cleanup(name);
    plant_orphan(name, 4096);

    // 76_533 is the exact production payload that could not fit the stale 4096.
    let payload = vec![0xCDu8; 76_533];
    let region = ShmRegion::create_overflow(name, &payload)
        .expect("a stale orphan must not block a larger write");

    assert!(
        region.len() >= payload.len(),
        "region should be sized for the payload, got {} for {} bytes — \
         the stale 4096-byte mapping was adopted instead of reclaimed",
        region.len(),
        payload.len()
    );

    drop(region);
    cleanup(name);
}

#[test]
fn a_live_region_is_never_reclaimed() {
    // The guard rail: reclaiming must only ever touch orphans. While a region
    // is genuinely alive, its backing file is held open, so an exclusive open
    // fails and the file must survive.
    let name = "slotbus-test-orphan-live";
    cleanup(name);

    let live = ShmRegion::create_overflow(name, &vec![0xAAu8; 1024]).expect("create live region");
    let planted = backing_path(name);
    assert!(planted.is_file(), "live region should have a backing file");

    // A second create for the same name must NOT delete the live file.
    let second = ShmRegion::create_overflow(name, &vec![0xBBu8; 76_533]);
    assert!(
        second.is_err(),
        "must refuse rather than steal a live region's name"
    );
    assert!(
        planted.is_file(),
        "the live region's backing file was deleted — reclaim is unsafe"
    );

    drop(live);
    cleanup(name);
}
