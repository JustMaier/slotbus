//! Why `heap full for request/response meta` used to happen, and why per-slot
//! arenas end it.
//!
//! The inline heap was once a single bump allocator shared by every slot.
//! `alloc_heap` only ever moved `alloc_head` forward — there was no per-slot
//! free — and the one path that reclaimed space, `try_reset_heap`, fired only
//! when `has_inflight_slots()` reported every slot FREE at the same instant.
//!
//! That made reclamation an all-or-nothing event requiring global quiescence.
//! Sustained overlap alone — no leaked handle, no stale worker, no bug
//! anywhere — was enough to exhaust the heap, because the reset was vetoed on
//! every attempt by whichever slot happened to be busy.
//!
//! The heap is now divided into fixed per-slot arenas, so a slot can only ever
//! consume space it owns and no reclamation is needed at all. These tests keep
//! the original workloads and assert the new contract: the overlap case that
//! used to die at ~137 cycles must now run indefinitely, and the shared
//! allocator must sit provably untouched while it does.
//!
//! Each test states what would make it vacuous and asserts against it, because
//! a test in this area passes very easily for the wrong reason.

// Several tests deliberately exercise the retired shared-allocator API to prove
// it is no longer load-bearing. That is the point of them, so the deprecation
// warnings are expected here.
#![allow(deprecated)]

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

/// Overlapping cycles driven by the regression test. The shared allocator died
/// at ~137 on this geometry; arenas must survive orders of magnitude more.
const ITERATIONS: usize = 10_000;

/// Sustained overlap no longer exhausts the heap.
///
/// This is the regression test for the fault this suite originally documented.
/// It ran unchanged against the shared bump allocator and **failed by design**:
/// the heap filled after ~137 overlapping cycles, because `alloc_head` only
/// moved forward and the sole reclamation path — `try_reset_heap` — required
/// every slot to be FREE at the same instant, which sustained overlap never
/// allows. The assertions are now inverted: with per-slot arenas the same
/// workload must run indefinitely.
///
/// The shape still mirrors a busy hub: one slot is always kept busy across the
/// boundary of the next, so global quiescence never occurs. That used to be
/// what killed it. Now it is simply irrelevant, which is the point.
///
/// Vacuity guards, because a test in this area passes very easily for the wrong
/// reason:
///   - every cycle must COMPLETE, so a silent early exit cannot look like success;
///   - payloads must stay INLINE (`body_overflow == OVERFLOW_INLINE`), proving
///     the heap was actually exercised rather than everything quietly spilling
///     to overflow regions and never touching it;
///   - `alloc_head` must remain 0, proving no write path still allocates from
///     the shared allocator whose march to the end of the heap was the bug.
#[test]
fn sustained_overlap_no_longer_exhausts_heap() {
    // Same geometry the old failure used: 64 KiB total, 4 slots.
    let config = make_config("overlap", 4, 64 * 1024);
    let region = create_region(&config);

    let body = vec![0xAB_u8; 512];
    let mut completed = 0usize;
    let mut exhausted = false;
    let mut observed_inline = 0usize;

    // Keep one slot busy across the boundary of the next, so there is never an
    // instant where every slot is FREE.
    let mut held = region::claim_free_slot(&region).expect("first claim");

    for i in 0..ITERATIONS {
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
        !exhausted,
        "the heap exhausted after {completed} overlapping cycles — per-slot arenas are \
         supposed to make this impossible, so the allocation path has regressed to sharing \
         space across slots"
    );
    assert_eq!(
        completed, ITERATIONS,
        "only {completed} of {ITERATIONS} cycles completed; every one should succeed now \
         that slots cannot consume each other's space"
    );

    // Vacuity guard 1: the payloads must have gone INLINE. If they had all
    // spilled to overflow regions the test would pass while never touching the
    // heap at all — exactly the false pass that a previous version of this
    // suite produced.
    assert_eq!(
        observed_inline, completed,
        "only {observed_inline} of {completed} writes stayed inline; a 512-byte body must \
         fit this geometry, so overflow spilling means the test stopped exercising the heap"
    );

    // Vacuity guard 2: the shared bump allocator must be provably idle. Under
    // the old design `alloc_head` advanced on every write and never came back
    // down under overlap — that march to the end of the heap WAS the bug. If it
    // moved at all here, some write path is still allocating globally and the
    // fix is incomplete no matter what the assertions above say.
    let alloc_head = unsafe { region.header() }
        .alloc_head
        .load(Ordering::Acquire);
    assert_eq!(
        alloc_head, 0,
        "alloc_head advanced to {alloc_head} during {completed} writes; a write path is \
         still using the shared allocator instead of its slot arena"
    );

    eprintln!(
        "per-slot arenas: {completed} overlapping cycles, no exhaustion, alloc_head untouched"
    );
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

// ---- Per-slot arena contract -------------------------------------------------

/// The arenas must tile the heap without overlapping and without running past
/// its end. Everything else in this file rests on that, and an off-by-one here
/// would show up as cross-slot corruption rather than an obvious failure.
#[test]
fn slot_arenas_are_disjoint_and_within_the_heap() {
    let config = make_config("geometry", 8, 64 * 1024);
    let region = create_region(&config);
    let (_, heap_size) = compute_layout(config.num_slots, config.region_size);

    let mut prev_end = 0usize;
    for slot in 0..config.num_slots {
        let ((req_base, req_len), (resp_base, resp_len)) = region.slot_arenas(slot);

        assert_eq!(
            req_base, prev_end,
            "slot {slot} request arena starts at {req_base}, leaving a gap or overlap after {prev_end}"
        );
        assert_eq!(
            resp_base,
            req_base + req_len,
            "slot {slot} response arena must begin exactly where the request arena ends"
        );
        assert!(
            resp_base + resp_len <= heap_size,
            "slot {slot} arena ends at {} but the heap is only {heap_size} bytes",
            resp_base + resp_len
        );
        assert_eq!(req_base % 8, 0, "slot {slot} request arena is misaligned");
        assert_eq!(resp_base % 8, 0, "slot {slot} response arena is misaligned");
        assert!(
            req_len > 0 && resp_len > 0,
            "slot {slot} got an empty arena"
        );

        prev_end = resp_base + resp_len;
    }
}

/// One slot hammered forever cannot starve another. This is the property the
/// shared allocator lacked: there, slot 0's traffic consumed the same space
/// slot 3 needed, so a busy slot could exhaust a quiet one.
///
/// Vacuity guard: asserts slot 3 has NOT been written before the hammering, and
/// that the hammering actually wrote inline every time — otherwise "slot 3 still
/// works" would prove nothing about heap pressure.
#[test]
fn one_slots_traffic_cannot_starve_another() {
    let config = make_config("isolation", 4, 64 * 1024);
    let region = create_region(&config);

    let body = vec![0x5A_u8; 512];
    let hammer_slot = 0usize;
    let victim_slot = 3usize;

    for i in 0..5_000 {
        let slot = unsafe { region.slot(hammer_slot) };
        slot.status.store(SLOT_WRITING, Ordering::Release);

        let meta = meta_bytes(&format!("/hammer/{i}"));
        region::write_request(
            &region,
            hammer_slot,
            "00000000-0000-0000-0000-000000000000",
            METHOD_GET,
            &meta,
            &body,
            &config,
        )
        .unwrap_or_else(|e| panic!("hammering slot {hammer_slot} failed at iteration {i}: {e}"));

        assert_eq!(
            unsafe { region.slot(hammer_slot) }.body_overflow,
            OVERFLOW_INLINE,
            "hammer payload spilled to overflow at iteration {i}; the test would no longer \
             be applying heap pressure"
        );

        free_slot(&region, hammer_slot);
    }

    // The victim slot must be untouched and still fully usable.
    assert_eq!(
        slot_status(&region, victim_slot),
        SLOT_FREE,
        "victim slot should never have been claimed by the hammer loop"
    );

    // Reserve the victim slot specifically. `claim_free_slot` scans from index 0
    // and would hand back the hammered slot, which proves nothing.
    let claimed = victim_slot;
    unsafe { region.slot(claimed) }
        .status
        .compare_exchange(SLOT_FREE, SLOT_WRITING, Ordering::AcqRel, Ordering::Acquire)
        .expect("victim slot must still be FREE and claimable");

    let meta = meta_bytes("/victim");
    region::write_request(
        &region,
        claimed,
        "11111111-1111-1111-1111-111111111111",
        METHOD_GET,
        &meta,
        &body,
        &config,
    )
    .expect("a quiet slot must still have its full arena after another slot ran 5k cycles");

    let (_, _, read_meta, read_body) =
        region::read_request(&region, claimed, &config).expect("victim payload must read back");
    assert_eq!(read_meta.path, "/victim");
    assert_eq!(
        read_body, body,
        "victim body was corrupted by the hammering"
    );
}

/// A body too large for its arena must spill to an overflow region and still
/// round-trip. This is the boundary the smaller inline ceiling creates, so it
/// has to be exercised directly rather than assumed.
#[test]
fn body_too_large_for_its_arena_spills_to_overflow_and_round_trips() {
    let config = make_config("spill", 4, 64 * 1024);
    let region = create_region(&config);

    let ((_, req_arena), _) = region.slot_arenas(0);
    // Comfortably past the arena so the outcome cannot depend on meta length.
    let body: Vec<u8> = (0..req_arena * 2).map(|i| (i % 251) as u8).collect();

    let slot = region::claim_free_slot(&region).expect("claim");
    let meta = meta_bytes("/spill");
    let overflow = region::write_request(
        &region,
        slot,
        "22222222-2222-2222-2222-222222222222",
        METHOD_GET,
        &meta,
        &body,
        &config,
    )
    .expect("an oversized body must spill, not fail");

    // Vacuity guard: prove it really took the overflow path rather than somehow
    // fitting inline, which would make the round-trip below meaningless.
    assert!(
        overflow.is_some(),
        "write_request returned no overflow handle, so the body did not spill"
    );
    assert_ne!(
        unsafe { region.slot(slot) }.body_overflow,
        OVERFLOW_INLINE,
        "slot still marks the body as inline despite exceeding the arena"
    );

    let (_, _, read_meta, read_body) =
        region::read_request(&region, slot, &config).expect("spilled body must read back");
    assert_eq!(read_meta.path, "/spill");
    assert_eq!(read_body.len(), body.len(), "spilled body changed length");
    assert_eq!(read_body, body, "spilled body round-tripped corrupted");
}
