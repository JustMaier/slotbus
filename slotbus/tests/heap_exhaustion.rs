//! Why `heap full for request/response meta` happens, and what it is not.
//!
//! The inline heap is a bump allocator. `alloc_heap` only ever moves
//! `alloc_head` forward — there is no per-slot free. The single way space is
//! reclaimed is `reset_heap`, and `try_reset_heap` calls it only when
//! `has_inflight_slots()` reports every slot FREE.
//!
//! That makes reclamation an all-or-nothing event requiring global quiescence.
//! These tests pin down the consequence: sustained overlap alone — with no
//! leaked handle, no stale worker, and no bug anywhere — is sufficient to
//! exhaust the heap, because the reset is vetoed on every attempt by whichever
//! slot happens to be busy at that moment.
//!
//! Each test states what would make it vacuous and asserts against it, because
//! a test in this area passes very easily for the wrong reason.

use slotbus::region::{self, ShmRegion};
use slotbus::types::*;
use slotbus::SlotBusConfig;

fn make_config(test: &str, num_slots: usize, region_size: usize) -> SlotBusConfig {
    let name = format!("heapx-{}-{}", test, std::process::id());
    SlotBusConfig::builder()
        .name(&name)
        .prefix("heapx")
        .num_slots(num_slots)
        .region_size(region_size)
        .build()
}

fn create_region(config: &SlotBusConfig) -> ShmRegion {
    let mut region = ShmRegion::create(&config.region_name(), config.region_size).unwrap();
    region.init_control(config);
    region
}

fn meta_bytes(path: &str) -> Vec<u8> {
    let meta = RequestMeta {
        path: path.to_string(),
        route_pattern: "/x/:id".into(),
        path_params: vec![],
        query: None,
        headers: vec![],
    };
    postcard::to_allocvec(&meta).unwrap()
}

fn free_slot(region: &ShmRegion, index: usize) {
    let slot = unsafe { region.slot(index) };
    slot.status.store(SLOT_FREE, Ordering::Release);
}

fn slot_status(region: &ShmRegion, index: usize) -> u32 {
    let slot = unsafe { region.slot(index) };
    slot.status.load(Ordering::Acquire)
}

use std::sync::atomic::Ordering;

/// Sustained overlap alone exhausts the heap. No leaked worker is involved:
/// every slot this test claims is also freed by this test, and nothing outside
/// it holds a handle to anything.
///
/// The shape mirrors a busy hub. Before claiming the next slot we call
/// `try_reset_heap`, exactly as `dispatch` does (transport.rs:142). The reset
/// is attempted on every single iteration and never once fires, because the
/// previous slot is deliberately still busy when we ask.
///
/// Vacuity guards:
///   - asserts a non-trivial number of cycles SUCCEEDED first, so a failure on
///     iteration 0 (misconfigured region, bad meta) cannot masquerade as the
///     exhaustion we are trying to demonstrate;
///   - asserts the payloads stayed INLINE (`body_overflow == OVERFLOW_INLINE`),
///     so we know we were actually consuming heap rather than silently
///     spilling to overflow regions and never touching it.
#[test]
fn sustained_overlap_exhausts_heap_with_no_leak_anywhere() {
    // Small heap so exhaustion arrives quickly: 64 KiB total, 4 slots.
    let config = make_config("overlap", 4, 64 * 1024);
    let region = create_region(&config);

    let body = vec![0xAB_u8; 512];
    let mut completed = 0usize;
    let mut exhausted = false;
    let mut observed_inline = 0usize;

    // Keep one slot busy across the boundary of the next, so there is never an
    // instant where every slot is FREE.
    let mut held = region::claim_free_slot(&region).expect("first claim");

    for i in 0..10_000 {
        // Exactly what the hub does before claiming: attempt a reset.
        region.try_reset_heap();

        let next = match region::claim_free_slot(&region) {
            Some(s) => s,
            // Slots are recycled promptly here; if this ever trips, the test is
            // measuring slot pressure rather than heap pressure. Fail loudly.
            None => panic!("ran out of slots at iteration {i} — test is not measuring the heap"),
        };

        let meta = meta_bytes(&format!("/x/{i}"));
        let wrote = region::write_request(
            &region,
            next,
            "00000000-0000-0000-0000-000000000000",
            METHOD_GET,
            &meta,
            &body,
            &config,
        );

        match wrote {
            Ok(_) => {
                let slot = unsafe { region.slot(next) };
                if slot.body_overflow == OVERFLOW_INLINE {
                    observed_inline += 1;
                }
                completed += 1;
            }
            Err(e) => {
                assert!(
                    e.to_string().contains("heap full"),
                    "expected heap exhaustion, got a different failure: {e}"
                );
                exhausted = true;
                free_slot(&region, next);
                break;
            }
        }

        // Release the PREVIOUS slot only after the next one is busy: overlap.
        free_slot(&region, held);
        held = next;
    }

    free_slot(&region, held);

    assert!(
        exhausted,
        "heap never filled in 10k iterations — raise the iteration count or shrink the region"
    );
    assert!(
        completed > 10,
        "only {completed} cycles succeeded before exhaustion; \
         that is early enough to suggest a setup fault, not genuine heap pressure"
    );
    // Not all writes stay inline, and that is the allocator behaving correctly:
    // once the heap is nearly full, `write_request` still finds room for the
    // small meta but not the 512-byte body, so the body spills to an overflow
    // region (region.rs, the `else if let Some(body_offset) = alloc_heap(..)`
    // arm). Only the *meta* allocation failing is fatal. So the guard is that
    // inline writes dominated — proving we really were consuming heap — not
    // that every single one did.
    assert!(
        observed_inline > 10 && observed_inline * 2 > completed,
        "only {observed_inline} of {completed} writes stayed inline; the body spilled to \
         overflow too often for this to demonstrate heap pressure"
    );

    eprintln!("heap-exhaustion: {completed} overlapping cycles before 'heap full'");
}

/// The same workload does NOT exhaust the heap when quiescence is allowed.
///
/// This is the control for the test above, and it is the part a leaked-handle
/// explanation cannot account for: the observed production bursts ended and the
/// system recovered on its own. A permanently pinned slot would never recover.
#[test]
fn quiescence_reclaims_the_heap_and_the_same_workload_survives() {
    let config = make_config("quiesce", 4, 64 * 1024);
    let region = create_region(&config);

    let body = vec![0xCD_u8; 512];

    // Ten times the iteration count that exhausted the heap under overlap.
    for i in 0..5_000 {
        region.try_reset_heap();

        let slot = region::claim_free_slot(&region).expect("claim");
        let meta = meta_bytes(&format!("/x/{i}"));
        region::write_request(
            &region,
            slot,
            "00000000-0000-0000-0000-000000000000",
            METHOD_GET,
            &meta,
            &body,
            &config,
        )
        .unwrap_or_else(|e| panic!("iteration {i} failed with quiescence available: {e}"));

        // Fully release before the next claim: every slot is FREE at the top of
        // the next loop, so try_reset_heap actually fires.
        free_slot(&region, slot);
    }

    // Proof the reset was genuinely happening rather than the heap being large
    // enough to absorb 5k iterations: the overlap test above blew up on the
    // same geometry in far fewer.
    assert!(!region.has_inflight_slots());
    region.try_reset_heap();
    assert!(
        region.alloc_heap(32 * 1024).is_some(),
        "a 32 KiB allocation should succeed immediately after a reset"
    );
}

/// Directly refutes the premise that a leaked `SlotWorker` blocks the reset.
///
/// A leaked worker's `overflow_regions` map holds `ShmRegion` handles to
/// SEPARATE named mappings (`*-req-N` / `*-rsp-N`). Slot status lives in the
/// control region and is written only by the protocol. Holding overflow handles
/// open — which is exactly what the leak does, and what keeps stale mappings
/// alive for days — has no effect on `has_inflight_slots()`, so it cannot veto
/// `try_reset_heap`.
///
/// Vacuity guard: asserts the overflow regions were really created and are
/// really still mapped at the moment of the check, so this cannot pass by
/// having quietly created nothing.
#[test]
fn holding_overflow_handles_does_not_pin_any_slot() {
    let config = make_config("leakpin", 4, 64 * 1024);
    let region = create_region(&config);

    // Simulate the leak: a live map of overflow regions, held open.
    let payload = vec![0xEE_u8; 8192];
    let mut leaked: Vec<ShmRegion> = Vec::new();
    for slot in 0..4 {
        let name = config.response_overflow_name(slot);
        leaked.push(ShmRegion::create_overflow(&name, &payload).unwrap());
    }

    // Vacuity: the mappings exist and are readable right now.
    for slot in 0..4 {
        let name = config.response_overflow_name(slot);
        let got = ShmRegion::read_overflow(&name, payload.len())
            .unwrap_or_else(|e| panic!("overflow region for slot {slot} not live: {e}"));
        assert_eq!(got.len(), payload.len());
    }
    assert_eq!(leaked.len(), 4, "leaked handles must still be held here");

    // The actual claim under test.
    assert!(
        !region.has_inflight_slots(),
        "holding overflow handles must not make any slot appear in-flight"
    );

    // And the reset therefore still fires.
    region.alloc_heap(1024).expect("prime the allocator");
    region.try_reset_heap();
    let after = region.alloc_heap(32 * 1024);
    assert!(
        after.is_some(),
        "try_reset_heap should have reclaimed the heap despite the leaked handles"
    );
    assert_eq!(
        after,
        Some(0),
        "reset should return the allocator to offset 0"
    );

    // Keep the leak alive until after every assertion, so the compiler cannot
    // drop it early and turn this into a test of nothing.
    drop(leaked);
}

/// A slot genuinely stuck non-FREE *does* veto the reset forever. This is the
/// mechanism the leak hypothesis assumed — it is real, it is just not what the
/// leak does. Recorded so the distinction is testable rather than argued.
#[test]
fn a_stuck_slot_vetoes_the_reset_permanently() {
    let config = make_config("stuck", 4, 64 * 1024);
    let region = create_region(&config);

    let stuck = region::claim_free_slot(&region).expect("claim");
    assert_ne!(slot_status(&region, stuck), SLOT_FREE);

    region.alloc_heap(16 * 1024).expect("prime");

    for _ in 0..100 {
        region.try_reset_heap();
    }
    assert!(
        region.alloc_heap(48 * 1024).is_none(),
        "reset must NOT have fired while a slot is stuck non-FREE"
    );

    // Release it and the very next attempt reclaims everything.
    free_slot(&region, stuck);
    region.try_reset_heap();
    assert_eq!(
        region.alloc_heap(48 * 1024),
        Some(0),
        "reset should fire the moment the stuck slot is released"
    );
}
