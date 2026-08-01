//! A region must never advertise a heap larger than the mapping that backs it.
//!
//! `SlotBus::create` asks `create_or_open` for `config.region_size`. When a
//! mapping of that name already exists it is adopted **at its own size**, and
//! `init_control` used to compute the header from the requested size anyway.
//! The header then described a heap running past the end of the mapping — and
//! since `heap_read_checked`/`heap_write_checked` validate against the header,
//! they waved through out-of-bounds accesses.
//!
//! Observed live: control-region files of 1,048,576 bytes while the hub ran
//! `--region-size 4194304`, i.e. a 3 MB out-of-bounds window that every bounds
//! check reported as fine. With per-slot arenas it is immediate rather than
//! latent — the top slots' arenas lie wholly outside the mapping.
//!
//! Two defences, both tested here:
//!   1. an *orphaned* backing file is reclaimed, so the full size is obtained;
//!   2. when the smaller mapping is genuinely live and must be adopted,
//!      `init_control` refuses instead of writing a header it cannot back.

use slotbus::region::ShmRegion;
use slotbus::types::compute_layout;
use slotbus::SlotBusConfig;

const SMALL: usize = 1024 * 1024; // 1 MiB — the size those stale files were pinned at
const LARGE: usize = 4 * 1024 * 1024; // 4 MiB — what the hub asks for
const SLOTS: usize = 64;

fn backing_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("shared_memory-rs").join(name)
}

fn cleanup(name: &str) {
    let _ = std::fs::remove_file(backing_path(name));
}

fn config_for(name: &str, region_size: usize) -> SlotBusConfig {
    SlotBusConfig::builder()
        .name(name)
        .num_slots(SLOTS)
        .region_size(region_size)
        .build()
}

/// A backing file with no live owner — what a `TerminateProcess` leaves behind.
#[cfg(windows)]
fn plant_orphan(name: &str, size: usize) {
    let path = backing_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, vec![0u8; size]).unwrap();
}

/// Defence 1: a stale *orphan* is reclaimed, so we get the size we asked for
/// and the header is honest. This is the common production case — the files
/// pinning those regions had no owner at all.
#[cfg(windows)]
#[test]
fn an_orphaned_smaller_region_is_reclaimed_so_the_header_is_honest() {
    let name = format!("slotbus-test-stale-orphan-{}", std::process::id());
    cleanup(&name);
    plant_orphan(&name, SMALL);

    let mut region = ShmRegion::create_or_open(&name, LARGE).expect("create_or_open");
    let actual_len = region.len();

    assert_eq!(
        actual_len, LARGE,
        "the orphaned {SMALL}-byte file should have been reclaimed, not adopted"
    );

    region
        .init_control(&config_for(&name, LARGE))
        .expect("a full-size region must initialise");

    let (heap_offset, heap) = compute_layout(SLOTS, LARGE);
    assert!(
        heap_offset + heap <= actual_len,
        "header must fit the mapping: end={} len={actual_len}",
        heap_offset + heap
    );

    drop(region);
    cleanup(&name);
}

/// Defence 2: when the smaller mapping is genuinely LIVE it cannot be
/// reclaimed, so it is adopted — and `init_control` must refuse rather than
/// write a header describing space the mapping does not have.
#[test]
fn init_control_refuses_to_describe_more_heap_than_the_mapping_has() {
    let name = format!("slotbus-test-stale-live-{}", std::process::id());
    cleanup(&name);

    // Held open for the whole test, so reclaim correctly declines to touch it.
    let live = ShmRegion::create(&name, SMALL).expect("create small region");
    assert_eq!(live.len(), SMALL);

    let mut adopted = ShmRegion::create_or_open(&name, LARGE).expect("adopt the live mapping");
    assert_eq!(
        adopted.len(),
        SMALL,
        "a live mapping is adopted at its own size"
    );

    let err = adopted
        .init_control(&config_for(&name, LARGE))
        .expect_err("init_control must reject a layout the mapping cannot hold");

    let msg = err.to_string();
    assert!(
        msg.contains("smaller mapping was adopted"),
        "unexpected error text: {msg}"
    );

    drop(adopted);
    drop(live);
    cleanup(&name);
}

/// `validate_control` catches the same lie on the *open* path — a peer that
/// merely opens an existing region never calls `init_control`, so the invariant
/// has to be enforced there too.
///
/// Built by planting a SMALL backing file whose header describes the LARGE
/// layout, which is exactly the on-disk state the old code produced.
#[cfg(windows)]
#[test]
fn validate_control_rejects_a_header_that_overruns_the_mapping() {
    use slotbus::types::{SHM_MAGIC, SHM_VERSION};

    let name = format!("slotbus-test-stale-validate-{}", std::process::id());
    cleanup(&name);

    let (heap_offset, heap_size) = compute_layout(SLOTS, LARGE);
    assert!(
        heap_offset + heap_size > SMALL,
        "sanity: the LARGE layout must not fit in SMALL, else this proves nothing"
    );

    // A 1 MiB file carrying a header that claims the 4 MiB layout.
    let mut bytes = vec![0u8; SMALL];
    for (i, word) in [
        SHM_MAGIC,
        SHM_VERSION,
        SLOTS as u32,
        heap_offset as u32,
        heap_size as u32,
    ]
    .iter()
    .enumerate()
    {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let path = backing_path(&name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let mut opened = ShmRegion::open(&name).expect("open the planted region");
    assert_eq!(opened.len(), SMALL, "sanity: mapped the planted 1 MiB file");

    let err = opened
        .validate_control()
        .expect_err("validate_control must reject a header that overruns the mapping");
    assert!(
        err.to_string()
            .contains("header describes a layout ending at"),
        "unexpected error text: {err}"
    );

    drop(opened);
    cleanup(&name);
}

/// Control: with no stale file the same path is correct end to end. Without
/// this, the tests above could pass for reasons unrelated to staleness.
#[test]
fn without_a_stale_file_the_header_matches_the_mapping() {
    let name = format!("slotbus-test-stale-clean-{}", std::process::id());
    cleanup(&name);

    let mut region = ShmRegion::create_or_open(&name, LARGE).expect("create_or_open");
    let actual_len = region.len();
    region
        .init_control(&config_for(&name, LARGE))
        .expect("clean init must succeed");

    let (heap_offset, heap) = compute_layout(SLOTS, LARGE);

    assert_eq!(actual_len, LARGE, "clean create should get the full size");
    assert!(
        heap_offset + heap <= actual_len,
        "clean case must be in bounds: end={} len={actual_len}",
        heap_offset + heap
    );

    drop(region);
    cleanup(&name);
}
