//! Reclaiming an orphaned backing file must never delete anything outside the
//! backing directory.
//!
//! `reclaim_orphaned_backing_file` deletes files, and region names are
//! attacker-reachable: a hub takes the worker's requested name straight from
//! its registration request. The path was built with `Path::join`, which
//! happily walks up through `..` and is replaced outright by an absolute path.
//!
//! Note the arithmetic when writing these: the prefix makes the first component
//! `hub-..`, which absorbs one level. A two-`..` payload therefore lands inside
//! the backing directory and proves nothing — a real traversal needs three.

#![cfg(windows)]

use slotbus::region::ShmRegion;
use slotbus::SlotBusConfig;

fn shm_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("shared_memory-rs")
}

/// Plant a victim outside the backing directory and try to make reclaim eat it.
fn victim_survives_name(crafted: &str, victim_name: &str) -> bool {
    std::fs::create_dir_all(shm_dir()).unwrap();
    let victim = std::env::temp_dir().join(victim_name);
    std::fs::write(&victim, b"not a shared-memory backing file").unwrap();
    assert!(victim.exists(), "victim must exist before the attempt");

    let _ = ShmRegion::create_overflow(crafted, b"payload");

    let survived = victim.exists();
    let _ = std::fs::remove_file(&victim);
    survived
}

#[test]
fn traversal_cannot_delete_outside_the_backing_directory() {
    assert!(
        victim_survives_name(
            "hub-../../../slotbus-test-victim-rsp-0",
            "slotbus-test-victim-rsp-0"
        ),
        "reclaim deleted a file outside {}",
        shm_dir().display()
    );
}

// Deliberately absent: deeper-traversal, backslash-separator and absolute-path
// variants. All three were written, and all three still passed with BOTH guards
// disabled — they never reach `reclaim_orphaned_backing_file` at all, because
// `shared_memory`'s own create fails earlier with something other than
// `MappingIdExists`, so the reclaim path is never entered. A test that passes
// against the vulnerable code proves nothing, and keeping it would advertise
// coverage that does not exist. The two tests here were confirmed by the same
// falsification to fail without the guards and pass with them.

/// The traversal is reachable from a worker-supplied name, so this is a
/// library-level concern and not merely API misuse. slotbus-hub rejects such
/// names at registration too; this is the defence-in-depth half.
#[test]
fn a_worker_supplied_name_still_produces_a_traversing_region_name() {
    let cfg = SlotBusConfig::builder()
        .name("../../../slotbus-test-escape")
        .prefix("hub")
        .build();

    let overflow = cfg.response_overflow_name(0);
    assert!(
        overflow.contains(".."),
        "expected the raw name to still carry the traversal ({overflow}) — \
         slotbus does not rewrite names, it refuses to act on dangerous ones"
    );

    // And acting on it is refused: nothing outside the directory is touched.
    let victim = std::env::temp_dir().join("slotbus-test-escape-rsp-0");
    std::fs::write(&victim, b"victim").unwrap();
    let _ = ShmRegion::create_overflow(&overflow, b"payload");
    let survived = victim.exists();
    let _ = std::fs::remove_file(&victim);
    assert!(
        survived,
        "reclaim acted on a traversing worker-supplied name"
    );
}
