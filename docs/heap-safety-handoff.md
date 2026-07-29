# slotbus — heap-safety hardening handoff

**Status:** heap read/write guards shipped (0.1.3); overflow-region generation stamping shipped (§6). Root trigger of the underlying heap race still not confirmed with a crash dump — items A/B remain. This doc is for whoever picks up the permanent fix.

**Context:** A downstream hub worker (`agent-server` in the ai-notifications project) crash-looped with Windows exit code `3221225477` = `0xC0000005` (ACCESS_VIOLATION), ~once a minute, under a high-churn reconnect storm (two clients fighting over one identity → rapid concurrent `dispatch` + slot reuse + a 3s reaper running alongside). The access violation signature points at a wild pointer deref in `unsafe` SHM code, not safe Rust.

---

## 1. What was shipped (already in tree)

**Read-side bounds checks on the inline heap** — `slotbus/src/region.rs`:

- New `ShmRegion::heap_read_checked(offset, len) -> Result<&[u8], SlotBusError>` validates `offset as usize + len <= heap_size` (checked_add) before calling the `unsafe` `heap_read` (`from_raw_parts`). Out-of-range → `SlotBusError::InvalidRegion` instead of a wild read.
- `read_request` and `read_response` now route **all four** inline-heap reads (req meta, req body, resp meta, resp body) through `heap_read_checked` instead of unchecked `unsafe heap_read`.

Effect: a stale/garbage `meta_offset`/`body_offset`/`*_len` (whatever produced it) now yields a logged error and the slot is recycled, instead of `from_raw_parts` walking off the mapping → segfault. **This converts the crash class to fail-soft. It does NOT fix the underlying cause of a bad offset/len.**

Wire-compatible: read-side only, no layout/format change. Mixed old/new workers interoperate.

Also shipped downstream (not in slotbus): `agent-server/src/channel.rs::connect()` made same-session reconnects idempotent to kill the storm that exposed this.

---

## 2. What is NOT fixed (the actual work)

### 2a. Root trigger unconfirmed
Static analysis did **not** yield a clean "reset-while-reading" window, because the slot lifecycle already protects a slot's own heap bytes:
- `claim_free_slot` CASes `FREE→WRITING` (region.rs:350) **before** `write_request` bumps/writes the heap.
- The slot stays non-FREE (`WRITING`→`READY`→`CLAIMED`→`DONE`) through the whole produce/consume cycle.
- `has_inflight_slots()` (region.rs) returns true for any non-FREE slot, so `try_reset_heap()` (region.rs `try_reset_heap`, called at transport.rs:142 in `dispatch` and transport.rs:270 in the response watcher) will **not** reset while that slot is active.
- The hub copies response data (`read_response`, transport.rs:226) **before** the `DONE→FREE` CAS (transport.rs:230). The worker copies request data while `CLAIMED` (transport.rs:472). Both copy out before releasing the slot.

So the simple TOCTOU story (reset the shared bump allocator while a reader holds a heap ref) has no obvious single window given the current ordering. The real fault is therefore one of:

1. ~~**Unchecked overflow read**~~ — **This claim was wrong.** `ShmRegion::read_overflow(name, len)` has validated `len > region.len` and returned `SharedMemory` since the initial release (`775a269`); `create_overflow`'s own comment ("Mirror read_overflow's check") corroborates it. Verified by `git log -L` on the function. There was never an unchecked overflow read to close. The residual risk on this path was never memory safety — it was *identity*: a bounds-passing read of the **wrong** region after slot reuse. That is what item D below addresses, and it is now fixed.
2. **Reset/reader memory ordering** — `reset_heap` stores `alloc_head` with `Release` (region.rs:202) but no reader ever loads `alloc_head`; readers trust per-slot offset fields. There is no happens-before edge between a reset and an in-flight reader. If *any* reader-while-FREE window exists (e.g. an error path that frees a slot before a deref, or a future refactor), it is unprotected.
3. **Non-atomic slot field reads** — `read_request`/`read_response` read `meta_offset`/`meta_len`/`body_offset`/`body_len` as plain field loads. They're published via the status atomic (Release on `store(READY/DONE)`, Acquire on the claim CAS), so they're consistent *for the current request*. Audit that nothing reads these fields without first winning the status CAS.

### 2b. Get a crash dump to confirm
Static analysis is inconclusive on the exact faulting deref. Capture it:
- Enable WER local dumps for the worker exe (`HKLM\...\Windows Error Reporting\LocalDumps`) or run under a debugger, reproduce the storm, read the faulting address + stack. Expect it in `heap_read`/`read_overflow`/`from_raw_parts` or `postcard::from_bytes` on a bogus length.
- Repro without the downstream app: a stress test that spawns N concurrent `dispatch` calls with bodies straddling the inline-heap/overflow boundary, while slots churn FREE↔busy and `try_reset_heap` fires. Assert no crash + no `InvalidRegion` errors.

---

## 3. Recommended permanent fix (ranked)

**A. Remove the shared-reset hazard by design (preferred).** The inline heap is one bump allocator shared by all slots, reset based on global slot state — inherently cross-coupled. Replace with **per-slot heap arenas**: give each slot a fixed heap sub-range it owns; write/read only within it; never reset globally. Eliminates every cross-slot reset/reuse race at once. Larger change, but kills the class.

**B. If keeping the shared heap:** gate `reset_heap` behind an explicit active-reader count (AtomicUsize incremented around `read_request`/`read_response`, transport.rs:226 and :472) AND have readers load `alloc_head` with `Acquire` so there's a real happens-before. `try_reset_heap` resets only when `!has_inflight_slots() && active_readers == 0`.

**C. ~~Bound the overflow read path~~ — NOT NEEDED, the claim was wrong.** See §2a item 1: `read_overflow` has been bounds-checked since the initial release. No work to do.

**D. Harden the slot-reuse/overflow-name churn** (transport.rs:152/244/403) — ✅ **SHIPPED.** Overflow region naming is now generation-stamped, so a slow reader can't open a *new* region created for a reused slot under the same name. See §6.

Minimum to call it "fixed": ~~C~~ + **D** (done) + **A or B** (the actual heap race, still open) + a dump-confirmed repro test.

---

## 4. Key locations

| Concern | File:line |
|---|---|
| `heap_read` (unsafe `from_raw_parts`) | `slotbus/src/region.rs:157` |
| `heap_read_checked` (shipped guard) | `slotbus/src/region.rs` (just after `heap_read`) |
| `alloc_heap` / `reset_heap` / `try_reset_heap` / `has_inflight_slots` | `slotbus/src/region.rs:175,200,206,217` |
| `read_request` / `read_response` (now bounds-checked) | `slotbus/src/region.rs:545,580` |
| `read_overflow` (bounds-checked since initial release) | `slotbus/src/region.rs` |
| `create_exclusive` / `create_overflow_fresh` (generation stamping) | `slotbus/src/region.rs` |
| `claim_free_slot` (FREE→WRITING) | `slotbus/src/region.rs:345` |
| status stores (WRITING/READY/DONE/FREE) | `slotbus/src/region.rs:395,478,556` |
| `dispatch` (try_reset_heap + claim + overflow remove) | `slotbus/src/transport.rs:142,144,152` |
| hub response watcher (read before DONE→FREE, then try_reset_heap) | `slotbus/src/transport.rs:218-271` |
| worker receive loop (READY→CLAIMED, read_request, err→FREE) | `slotbus/src/transport.rs:459-505` |

## 5. Downstream rebuild note
slotbus reaches the downstream app (ai-notifications) as a path dep of every hub worker. After any slotbus change, rebuild them all — an un-rebuilt worker stays wire-compatible but takes the wild write instead of the clean error.

**Updated 2026-07-29:** every worker now carries the guard — hub, agent-server, tts-server, stt-server, and discord-bridge. The earlier note here claimed tts-server and stt-server were still stale; that was wrong by the time anyone read it.

To audit a binary rather than trusting a doc, grep it for the guard's error string, which exists in no commit before `c22d74d`:

```
grep -c "too small for write" <binary>
```

`1` means the binary was built after the fix, `0` means it wasn't. Pair it with a control that should be present (`shared memory error`) and one that shouldn't, so a silently-failing grep can't read as a pass.

The same applies to the generation stamping in section 6: a straggler degrades to current behaviour rather than breaking, but only a rebuilt binary stops emitting the error.

---

## 6. Overflow generation stamping (shipped)

Fixes the live production symptom, which was **not** the segfault but its fail-soft successor:

```
[AGENT] Failed to write response to slot 1: shared memory error:
overflow region 'hub-agent-rsp-1' too small for write:
need 76533, have 4096 (stale same-name mapping still open)
```

**Cause.** Overflow regions were named purely from the slot index (`hub-agent-rsp-1`). `create_overflow` used `create_or_open`, and on Windows creating a mapping under an existing name returns *the existing mapping at its original size*. Once any peer leaked a handle to a small overflow region — a `SlotWorker` from a previous connection that was never dropped is the likely culprit, given the reconnect churn in Context — that slot was poisoned permanently: every later payload larger than the stale mapping was refused. Slots 1 and 2 pinned at 4096 and 40960 bytes across days of logs is exactly that signature.

**Fix.**

1. `create_overflow` no longer adopts a mapping it did not create. A new private `create_exclusive` maps `MappingIdExists` to `None` instead of silently opening someone else's region.
2. New `create_overflow_fresh(name_for, data)` walks generations until it wins a create, returning the region plus a **marker** byte to store in the slot.
3. The `body_overflow` / `resp_body_overflow` slot bytes now encode `0 = inline`, `n = overflow generation n - 1`. Readers derive the region name from the marker instead of assuming the base name.

**No layout change and no version bump.** `SlotMeta` is untouched at 128 bytes — its 40 reserved bytes were *not* needed — `SHM_VERSION` stays 1, and generation 0 produces the byte-identical un-suffixed name older peers derive. The uncontended path is therefore wire-identical to 0.1.3 in both directions.

**Mixed-version behaviour.** A peer running older slotbus reads any non-zero marker as "generation 0" and would fail to find a suffixed region. That only happens once a generation above 0 is in use — i.e. exactly the situation where the older code could not have written the payload at all, since it returned the "too small for write" error above. So nothing that previously worked regresses, and **no lockstep rebuild is required**. Rebuilding everything is still *preferable*, since that is what actually cures the symptom, but a straggler degrades to current behaviour rather than breaking.

**Tests** (`slotbus/tests/stress.rs`):

| Test | Pins |
|---|---|
| `overflow_survives_stale_same_name_mapping` | The production scenario byte-for-byte: 1 KiB mapping pinned open, 76 533-byte payload |
| `write_response_round_trips_past_a_stale_overflow` | The real `write_response`/`read_response` path, not just the region helper |
| `slow_reader_is_not_handed_a_reused_slots_region` | The identity property — a slow reader resolves *its own* payload |
| `create_overflow_refuses_to_adopt_existing_mapping` | Never write into a mapping we did not create |
| `generation_zero_name_is_unchanged` | The wire-compat guarantee |
| `overflow_marker_encoding_round_trips` | Marker encode/decode |

Falsified by reverting the naming to pre-fix behaviour: 4 of the 6 fail, each with the original production error text. The other 2 are pure encoding/`create_exclusive` checks that are independent of naming.

**Still open:** items A/B — the shared inline-heap reset race. Generation stamping does not touch it.
