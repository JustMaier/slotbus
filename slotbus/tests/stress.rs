//! Stress tests for SHM transport concurrent slot access.
//!
//! Reproduces the race conditions that caused a TTS server crash
//! (exit code 0xC0000005 / access violation) during concurrent
//! multi-slot operations.
//!
//! Key races targeted:
//! 1. find_free_slot is not atomic — two dispatchers can grab the same slot
//! 2. try_reset_heap can zero the heap while another thread is mid-write
//! 3. Heap bump allocator hands out the same offset after a reset race

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use slotbus::region::{self, ShmRegion};
use slotbus::types::*;
use slotbus::SlotBusConfig;

fn make_config(test: &str, num_slots: usize, region_size: usize) -> SlotBusConfig {
    let name = format!("test-{}-{}", test, std::process::id());
    SlotBusConfig::builder()
        .name(&name)
        .prefix("stress")
        .num_slots(num_slots)
        .region_size(region_size)
        .build()
}

fn create_region(config: &SlotBusConfig) -> ShmRegion {
    let mut region = ShmRegion::create(&config.region_name(), config.region_size).unwrap();
    region.init_control(config);
    region
}

/// Race #1: Two hub threads call claim_free_slot simultaneously.
/// Before the fix (find_free_slot), they'd get the same index and corrupt
/// each other's data. With claim_free_slot (CAS Free→Writing), each slot
/// is exclusively owned.
#[test]
fn race_find_free_slot_not_atomic() {
    // Use a 4MB region to avoid heap exhaustion — this test targets slot races,
    // not heap management. The heap race is tested separately.
    let config = Arc::new(make_config("slot-race", 32, 4 * 1024 * 1024));
    let region = Arc::new(create_region(&config));

    let num_threads = 8;
    let iterations = 2000;
    let barrier = Arc::new(Barrier::new(num_threads));
    let corruptions = Arc::new(AtomicU32::new(0));
    let double_claims = Arc::new(AtomicU32::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let region = Arc::clone(&region);
            let barrier = Arc::clone(&barrier);
            let corruptions = Arc::clone(&corruptions);
            let double_claims = Arc::clone(&double_claims);
            let config = Arc::clone(&config);

            thread::spawn(move || {
                barrier.wait();

                for iter in 0..iterations {
                    // Reclaim heap when safe (mirrors real dispatch flow)
                    region.try_reset_heap();

                    let Some(slot_index) = region::claim_free_slot(&region) else {
                        // All slots busy — expected under contention
                        continue;
                    };

                    let req_id = format!("{tid:02x}{iter:04x}-0000-0000-0000-000000000000");
                    let meta = RequestMeta {
                        path: format!("/t/{tid}/{iter}"),
                        route_pattern: "/t/:id/:i".into(),
                        path_params: vec![],
                        query: None,
                        headers: vec![],
                    };
                    let meta_bytes = postcard::to_allocvec(&meta).unwrap();

                    let Ok(_) = region::write_request(
                        &region,
                        slot_index,
                        &req_id,
                        METHOD_GET,
                        &meta_bytes,
                        &[],
                        &config,
                    ) else {
                        continue; // heap full (slot already freed by write_request error handler)
                    };

                    // Immediately read back and check for corruption.
                    // If another thread wrote to the same slot, req_id or body will differ.
                    let slot = unsafe { region.slot(slot_index) };
                    let status = slot.status.load(Ordering::Acquire);

                    if status == SLOT_READY {
                        match region::read_request(&region, slot_index, &config) {
                            Ok((read_id, _method, read_meta, _read_body)) => {
                                if read_id != req_id {
                                    double_claims.fetch_add(1, Ordering::Relaxed);
                                }
                                if read_meta.path != format!("/t/{tid}/{iter}") {
                                    corruptions.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Err(_) => {
                                corruptions.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }

                    // Free the slot for reuse
                    slot.status.store(SLOT_FREE, Ordering::Release);
                }
            })
        })
        .collect();

    for h in handles {
        h.join()
            .expect("thread panicked (possible access violation!)");
    }

    let c = corruptions.load(Ordering::Relaxed);
    let d = double_claims.load(Ordering::Relaxed);
    eprintln!("slot-race: corruptions={c}, double_claims={d}");
    assert!(
        c == 0 && d == 0,
        "Slot race detected: {d} double claims, {c} body/meta corruptions"
    );
}

/// Race #2: try_reset_heap zeros the bump allocator while another thread
/// has already allocated from the heap but hasn't finished writing yet.
/// The reset makes the offset "available" again, so a third thread gets
/// the same heap offset and both write concurrently → torn data.
#[test]
fn race_heap_reset_during_write() {
    let config = Arc::new(make_config("heap-race", 32, 1_048_576));
    let region = Arc::new(create_region(&config));

    let iterations = 3000;
    let barrier = Arc::new(Barrier::new(3));
    let corruptions = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // Thread A: continuously dispatches requests
    let h_dispatch = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let corruptions = Arc::clone(&corruptions);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            for iter in 0..iterations {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let Some(slot_index) = region::claim_free_slot(&region) else {
                    thread::yield_now();
                    continue;
                };

                let req_id = format!("aa{iter:06x}-0000-0000-0000-000000000000");
                let meta = RequestMeta {
                    path: format!("/dispatch/{iter}"),
                    route_pattern: "/dispatch/:i".into(),
                    path_params: vec![],
                    query: None,
                    headers: vec![],
                };
                let meta_bytes = postcard::to_allocvec(&meta).unwrap();
                let body = vec![0xAA_u8; 256];

                if region::write_request(
                    &region,
                    slot_index,
                    &req_id,
                    METHOD_GET,
                    &meta_bytes,
                    &body,
                    &config,
                )
                .is_err()
                {
                    // Heap full — force reset (this is the race trigger)
                    region.reset_heap();
                    continue;
                }

                // Verify
                let slot = unsafe { region.slot(slot_index) };
                if slot.status.load(Ordering::Acquire) == SLOT_READY {
                    if let Ok((_id, _m, _meta, read_body)) =
                        region::read_request(&region, slot_index, &config)
                    {
                        if !read_body.iter().all(|&b| b == 0xAA) {
                            corruptions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    slot.status.store(SLOT_FREE, Ordering::Release);
                }
            }

            stop.store(true, Ordering::Relaxed);
        })
    };

    // Thread B: aggressively resets heap (simulates response_watcher calling try_reset_heap)
    let h_resetter = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);

        thread::spawn(move || {
            barrier.wait();

            while !stop.load(Ordering::Relaxed) {
                // This is what response_watcher does after freeing slots
                region.try_reset_heap();
                thread::yield_now();
            }
        })
    };

    // Thread C: also dispatches, competing for same heap space after resets
    let h_dispatch2 = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let corruptions = Arc::clone(&corruptions);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            for iter in 0..iterations {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let Some(slot_index) = region::claim_free_slot(&region) else {
                    thread::yield_now();
                    continue;
                };

                let req_id = format!("bb{iter:06x}-0000-0000-0000-000000000000");
                let meta = RequestMeta {
                    path: format!("/other/{iter}"),
                    route_pattern: "/other/:i".into(),
                    path_params: vec![],
                    query: None,
                    headers: vec![],
                };
                let meta_bytes = postcard::to_allocvec(&meta).unwrap();
                let body = vec![0xBB_u8; 256];

                if region::write_request(
                    &region,
                    slot_index,
                    &req_id,
                    METHOD_POST,
                    &meta_bytes,
                    &body,
                    &config,
                )
                .is_err()
                {
                    continue;
                }

                let slot = unsafe { region.slot(slot_index) };
                if slot.status.load(Ordering::Acquire) == SLOT_READY {
                    if let Ok((_id, _m, _meta, read_body)) =
                        region::read_request(&region, slot_index, &config)
                    {
                        if !read_body.iter().all(|&b| b == 0xBB) {
                            corruptions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    slot.status.store(SLOT_FREE, Ordering::Release);
                }
            }

            stop.store(true, Ordering::Relaxed);
        })
    };

    h_dispatch.join().expect("dispatch thread panicked");
    h_resetter.join().expect("resetter thread panicked");
    h_dispatch2.join().expect("dispatch2 thread panicked");

    let c = corruptions.load(Ordering::Relaxed);
    eprintln!("heap-race: corruptions={c}");
    assert!(c == 0, "Heap reset race confirmed: {c} corruptions");
}

/// Race #3: Full pipeline stress — simulates the actual crash scenario:
/// hub dispatches a burst of requests across many slots while the worker
/// is claiming and responding, and the response watcher is freeing slots
/// and resetting the heap. All four roles run concurrently.
#[test]
fn full_pipeline_burst() {
    let num_slots = 32;
    let config = Arc::new(make_config("pipeline", num_slots, 1_048_576));
    let region = Arc::new(create_region(&config));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(4));
    let dispatched = Arc::new(AtomicU32::new(0));
    let responded = Arc::new(AtomicU32::new(0));
    let freed = Arc::new(AtomicU32::new(0));
    let corruptions = Arc::new(AtomicU32::new(0));
    let total_requests = 5000_u32;

    // Hub dispatcher: burst of concurrent requests (simulates avatar fetch flood)
    let h_hub = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let dispatched = Arc::clone(&dispatched);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            while dispatched.load(Ordering::Relaxed) < total_requests {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let Some(slot_index) = region::claim_free_slot(&region) else {
                    thread::yield_now();
                    continue;
                };

                let i = dispatched.fetch_add(1, Ordering::Relaxed);
                let req_id = format!("{i:08x}-0000-0000-0000-000000000000");
                let meta = RequestMeta {
                    path: format!("/personas/voice{}/avatar", i % 28),
                    route_pattern: "/personas/:name/avatar".into(),
                    path_params: vec![("name".into(), format!("voice{}", i % 28))],
                    query: None,
                    headers: vec![],
                };
                let meta_bytes = postcard::to_allocvec(&meta).unwrap();

                // try_reset_heap is called at the start of dispatch_request
                region.try_reset_heap();

                if region::write_request(
                    &region,
                    slot_index,
                    &req_id,
                    METHOD_GET,
                    &meta_bytes,
                    &[],
                    &config,
                )
                .is_err()
                {
                    continue;
                }
            }

            stop.store(true, Ordering::Relaxed);
        })
    };

    // Second hub dispatcher: concurrent burst (simulates second HTTP connection)
    let h_hub2 = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let dispatched = Arc::clone(&dispatched);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            while dispatched.load(Ordering::Relaxed) < total_requests {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let Some(slot_index) = region::claim_free_slot(&region) else {
                    thread::yield_now();
                    continue;
                };

                let i = dispatched.fetch_add(1, Ordering::Relaxed);
                let req_id = format!("{i:08x}-0000-0000-0000-000000000001");
                let meta = RequestMeta {
                    path: format!("/session/test-{}", i % 10),
                    route_pattern: "/session/:id".into(),
                    path_params: vec![("id".into(), format!("test-{}", i % 10))],
                    query: None,
                    headers: vec![],
                };
                let meta_bytes = postcard::to_allocvec(&meta).unwrap();

                region.try_reset_heap();

                if region::write_request(
                    &region,
                    slot_index,
                    &req_id,
                    METHOD_GET,
                    &meta_bytes,
                    &[],
                    &config,
                )
                .is_err()
                {
                    continue;
                }
            }

            stop.store(true, Ordering::Relaxed);
        })
    };

    // Worker: claims Ready slots, reads request, writes response
    let h_worker = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let responded = Arc::clone(&responded);
        let corruptions = Arc::clone(&corruptions);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            while !stop.load(Ordering::Relaxed) || {
                // Drain remaining Ready/Claimed slots after stop
                (0..num_slots).any(|i| {
                    let s = unsafe { region.slot(i) }.status.load(Ordering::Acquire);
                    s == SLOT_READY || s == SLOT_CLAIMED
                })
            } {
                for i in 0..num_slots {
                    let slot = unsafe { region.slot(i) };

                    // CAS Ready → Claimed (same as worker_shm.rs)
                    if slot
                        .status
                        .compare_exchange(
                            SLOT_READY,
                            SLOT_CLAIMED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        // Read request — this is where the crash happens with bad heap offsets
                        match region::read_request(&region, i, &config) {
                            Ok((_req_id, _method, _meta, _body)) => {
                                // Write a small response
                                let resp_meta = ResponseMeta {
                                    content_type: "application/octet-stream".into(),
                                    headers: vec![],
                                };
                                let resp_bytes = postcard::to_allocvec(&resp_meta).unwrap();
                                let resp_body = vec![0xFF_u8; 32];

                                match region::write_response(
                                    &region,
                                    i,
                                    200,
                                    &resp_bytes,
                                    &resp_body,
                                    &config,
                                ) {
                                    Ok(_) => {
                                        responded.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(_) => {
                                        // Heap full — free slot to prevent stall
                                        slot.status.store(SLOT_FREE, Ordering::Release);
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Worker read_request failed on slot {i}: {e}");
                                corruptions.fetch_add(1, Ordering::Relaxed);
                                slot.status.store(SLOT_FREE, Ordering::Release);
                            }
                        }
                    }
                }
                thread::yield_now();
            }
        })
    };

    // Response watcher: reads Done slots, frees them, resets heap
    let h_watcher = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let freed = Arc::clone(&freed);
        let corruptions = Arc::clone(&corruptions);
        let stop = Arc::clone(&stop);
        let config = Arc::clone(&config);

        thread::spawn(move || {
            barrier.wait();

            while !stop.load(Ordering::Relaxed) || {
                (0..num_slots).any(|i| {
                    let s = unsafe { region.slot(i) }.status.load(Ordering::Acquire);
                    s == SLOT_DONE
                })
            } {
                let mut freed_any = false;

                for i in 0..num_slots {
                    let slot = unsafe { region.slot(i) };

                    if slot.status.load(Ordering::Acquire) != SLOT_DONE {
                        continue;
                    }

                    // Read response BEFORE freeing (same as shm.rs)
                    match region::read_response(&region, i, &config) {
                        Ok((status, _meta, body)) => {
                            if status != 200 {
                                corruptions.fetch_add(1, Ordering::Relaxed);
                            }
                            if !body.iter().all(|&b| b == 0xFF) {
                                corruptions.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            eprintln!("Watcher read_response failed on slot {i}: {e}");
                            corruptions.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    // CAS Done → Free (same as shm.rs)
                    if slot
                        .status
                        .compare_exchange(SLOT_DONE, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        freed_any = true;
                        freed.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // Same as shm.rs: reset heap after freeing
                if freed_any {
                    region.try_reset_heap();
                }

                thread::yield_now();
            }
        })
    };

    // Give it a reasonable timeout
    let _timeout = thread::spawn(move || {
        thread::sleep(Duration::from_secs(30));
    });

    h_hub
        .join()
        .expect("hub thread panicked (access violation?)");
    h_hub2
        .join()
        .expect("hub2 thread panicked (access violation?)");

    // Wait for worker and watcher to drain
    let drain_start = std::time::Instant::now();
    loop {
        let has_inflight = region.has_inflight_slots();
        if !has_inflight || drain_start.elapsed() > Duration::from_secs(10) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Signal stop for worker/watcher threads
    stop.store(true, Ordering::Relaxed);
    h_worker
        .join()
        .expect("worker thread panicked (access violation?)");
    h_watcher
        .join()
        .expect("watcher thread panicked (access violation?)");

    let d = dispatched.load(Ordering::Relaxed);
    let r = responded.load(Ordering::Relaxed);
    let f = freed.load(Ordering::Relaxed);
    let c = corruptions.load(Ordering::Relaxed);

    eprintln!("pipeline: dispatched={d}, responded={r}, freed={f}, corruptions={c}");
    assert!(
        c == 0,
        "Full pipeline race confirmed: {c} corruptions (dispatched={d}, responded={r}, freed={f})"
    );
}

/// Race #4: Targeted heap exhaustion + reset race.
/// Rapidly fills heap, resets it from another thread, then reads stale offsets.
#[test]
fn race_heap_exhaustion_stale_offset() {
    let config = make_config("heap-exhaust", 32, 1_048_576);
    let region = Arc::new(create_region(&config));

    let barrier = Arc::new(Barrier::new(2));
    let corruptions = Arc::new(AtomicU32::new(0));
    let iterations = 5000;

    // Thread A: allocates from heap and writes known patterns
    let h_writer = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);
        let corruptions = Arc::clone(&corruptions);

        thread::spawn(move || {
            barrier.wait();

            for i in 0..iterations {
                let size = 128;
                if let Some(offset) = region.alloc_heap(size) {
                    let pattern = (i % 256) as u8;
                    let data = vec![pattern; size];
                    unsafe {
                        region.heap_write(offset, &data);
                    }

                    // Immediately read back
                    let read = unsafe { region.heap_read(offset, size) };
                    if !read.iter().all(|&b| b == pattern) {
                        corruptions.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // else: heap full, ok
            }
        })
    };

    // Thread B: aggressively resets heap (simulating response watcher)
    let h_resetter = {
        let region = Arc::clone(&region);
        let barrier = Arc::clone(&barrier);

        thread::spawn(move || {
            barrier.wait();

            for _ in 0..iterations {
                // Force reset even with inflight (the race)
                region.reset_heap();
                thread::yield_now();
            }
        })
    };

    h_writer.join().expect("writer panicked");
    h_resetter.join().expect("resetter panicked");

    let c = corruptions.load(Ordering::Relaxed);
    eprintln!("heap-exhaust: corruptions={c}");
    assert!(
        c == 0,
        "Heap exhaustion stale offset race confirmed: {c} corruptions"
    );
}
