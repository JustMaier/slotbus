# slotbus — heap-safety hardening handoff

**Status:** partial fix shipped (read-side bounds checks). Root trigger not yet confirmed with a crash dump. This doc is for whoever picks up the permanent fix.

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

1. **Unchecked overflow read** (most likely remaining OOB) — `ShmRegion::read_overflow(name, len)` (region.rs:293) opens the named overflow mapping and reads `len` bytes. `len` comes from `slot.body_len` / `slot.resp_body_len`. If that length is ever inconsistent with the actual overflow region size (slot reused, overflow region recreated under the same name between the length write and the read — see the `overflow_regions` churn at transport.rs:152, :244, :403), this reads past the mapping → access violation. **This path is still unchecked.** Bound it the same way as the heap.
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

**C. Bound the overflow read path** (region.rs:293 `read_overflow`) — validate `len` against the opened mapping's actual size before reading; return `InvalidRegion` on mismatch. Cheap, closes the most likely remaining OOB.

**D. Harden the slot-reuse/overflow-name churn** (transport.rs:152/244/403) — the existing comments ("drop stale overflow before writing", "once Free a concurrent dispatch can overwrite") show the authors know slots get reused faster than readers finish. Make overflow region naming generation-stamped (slot_index + monotonic counter) so a slow reader can't open a *new* region created for a reused slot under the same name.

Minimum to call it "fixed": **C** (parity with the heap guard now shipped) + **A or B** (the actual race) + a dump-confirmed repro test.

---

## 4. Key locations

| Concern | File:line |
|---|---|
| `heap_read` (unsafe `from_raw_parts`) | `slotbus/src/region.rs:157` |
| `heap_read_checked` (shipped guard) | `slotbus/src/region.rs` (just after `heap_read`) |
| `alloc_heap` / `reset_heap` / `try_reset_heap` / `has_inflight_slots` | `slotbus/src/region.rs:175,200,206,217` |
| `read_request` / `read_response` (now bounds-checked) | `slotbus/src/region.rs:545,580` |
| `read_overflow` (STILL unchecked) | `slotbus/src/region.rs:293` |
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
