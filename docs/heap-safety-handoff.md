# slotbus — heap-safety investigation (closed)

**Status: resolved in 0.2.0.** This began as a handoff for open work and is kept
as the record of how it was chased. Everything it proposed is either shipped or
withdrawn — read it as history, not as a task list. Sections are preserved in the
order they were written, including the parts that turned out to be wrong, because
the wrong turns are the useful part.

**What it actually was.** Three independent faults, discovered in this order:

1. **Orphaned backing files** (the root cause of the reported symptom).
   `shared_memory` backs each mapping with a file that is removed only on a clean
   `Drop`, so any hard kill leaks it. A later run adopted the stale mapping *at
   its original size*, and a larger payload then wrote past the end. This is what
   produced `too small for write: need 76533, have 4096` — and the guard's own
   wording, *"stale same-name mapping still open"*, was wrong and sent three
   separate investigations hunting for a live handle. Nothing was open. On the
   machine where this was found, the offending files were **eight weeks old** and
   one dated to the repository's creation. Fixed by reclaiming orphans (proven
   orphaned by an exclusive open) rather than adopting them. §6's generation
   stamping made the collision survivable first; it was a fuse, not a cure.
2. **Inline heap exhaustion** (§7), which needed no leak at all: the shared bump
   allocator was reclaimed only on global quiescence, an instant that may never
   arrive under sustained overlap. Fixed by per-slot arenas (§8).
3. **Headers outliving their mapping**: `init_control` derived the layout from
   the *requested* `region_size` while the mapping might have been adopted at a
   smaller size. Since every bounds check validates against the header, they
   became rubber stamps for out-of-bounds access. `init_control` now refuses to
   write a header it cannot back, and `validate_control` enforces
   `heap_offset + heap_size <= mapping length`.

The original `0xC0000005` was never confirmed against a crash dump, and §2b's
plan to capture one was never executed. It is no longer worth doing: fault 1
fully explains the reported symptom, and the mechanisms that could produce a wild
write have been removed by construction rather than guarded against.

**Original context:** A downstream hub worker (`agent-server` in the ai-notifications project) crash-looped with Windows exit code `3221225477` = `0xC0000005` (ACCESS_VIOLATION), ~once a minute, under a high-churn reconnect storm (two clients fighting over one identity → rapid concurrent `dispatch` + slot reuse + a 3s reaper running alongside). The access violation signature points at a wild pointer deref in `unsafe` SHM code, not safe Rust.

---

## 1. What was shipped (already in tree)

**Read-side bounds checks on the inline heap** — `slotbus/src/region.rs`:

- New `ShmRegion::heap_read_checked(offset, len) -> Result<&[u8], SlotBusError>` validates `offset as usize + len <= heap_size` (checked_add) before calling the `unsafe` `heap_read` (`from_raw_parts`). Out-of-range → `SlotBusError::InvalidRegion` instead of a wild read.
- `read_request` and `read_response` now route **all four** inline-heap reads (req meta, req body, resp meta, resp body) through `heap_read_checked` instead of unchecked `unsafe heap_read`.

Effect: a stale/garbage `meta_offset`/`body_offset`/`*_len` (whatever produced it) now yields a logged error and the slot is recycled, instead of `from_raw_parts` walking off the mapping → segfault. **This converts the crash class to fail-soft. It does NOT fix the underlying cause of a bad offset/len.**

Wire-compatible: read-side only, no layout/format change. Mixed old/new workers interoperate.

Also shipped downstream (not in slotbus): `agent-server/src/channel.rs::connect()` made same-session reconnects idempotent to kill the storm that exposed this.

---

## 2. What was still unknown at the time — SUPERSEDED

> Everything below this heading was written before the root cause was known, and
> is retained only as a record of the reasoning. Item 1 was factually wrong.
> Items 2 and 3 were real observations about the shared allocator, which no
> longer exists as of §8. The actual cause was none of the three — see *What it
> actually was* at the top.

### 2a. Root trigger unconfirmed (as it stood then)
Static analysis did **not** yield a clean "reset-while-reading" window, because the slot lifecycle already protects a slot's own heap bytes:
- `claim_free_slot` CASes `FREE→WRITING` (region.rs:350) **before** `write_request` bumps/writes the heap.
- The slot stays non-FREE (`WRITING`→`READY`→`CLAIMED`→`DONE`) through the whole produce/consume cycle.
- `has_inflight_slots()` (region.rs) returns true for any non-FREE slot, so `try_reset_heap()` (region.rs `try_reset_heap`, called at transport.rs:142 in `dispatch` and transport.rs:270 in the response watcher) will **not** reset while that slot is active.
- The hub copies response data (`read_response`, transport.rs:226) **before** the `DONE→FREE` CAS (transport.rs:230). The worker copies request data while `CLAIMED` (transport.rs:472). Both copy out before releasing the slot.

So the simple TOCTOU story (reset the shared bump allocator while a reader holds a heap ref) has no obvious single window given the current ordering. The real fault is therefore one of:

1. ~~**Unchecked overflow read**~~ — **This claim was wrong.** `ShmRegion::read_overflow(name, len)` has validated `len > region.len` and returned `SharedMemory` since the initial release (`775a269`); `create_overflow`'s own comment ("Mirror read_overflow's check") corroborates it. Verified by `git log -L` on the function. There was never an unchecked overflow read to close. The residual risk on this path was never memory safety — it was *identity*: a bounds-passing read of the **wrong** region after slot reuse. That is what item D below addresses, and it is now fixed.
2. **Reset/reader memory ordering** — `reset_heap` stores `alloc_head` with `Release` (region.rs:202) but no reader ever loads `alloc_head`; readers trust per-slot offset fields. There is no happens-before edge between a reset and an in-flight reader. If *any* reader-while-FREE window exists (e.g. an error path that frees a slot before a deref, or a future refactor), it is unprotected.
3. **Non-atomic slot field reads** — `read_request`/`read_response` read `meta_offset`/`meta_len`/`body_offset`/`body_len` as plain field loads. They're published via the status atomic (Release on `store(READY/DONE)`, Acquire on the claim CAS), so they're consistent *for the current request*. Audit that nothing reads these fields without first winning the status CAS.

### 2b. Get a crash dump to confirm — NOT DONE, and no longer needed
Never executed. Fault 1 (orphaned backing files) explains the reported symptom
on its own, and per-slot arenas removed the shared-allocator races by
construction. Retained in case the access violation ever recurs, in which case
this is still the right first move:
- Enable WER local dumps for the worker exe (`HKLM\...\Windows Error Reporting\LocalDumps`) or run under a debugger, reproduce the storm, read the faulting address + stack. Expect it in `heap_read`/`read_overflow`/`from_raw_parts` or `postcard::from_bytes` on a bogus length.
- Repro without the downstream app: a stress test that spawns N concurrent `dispatch` calls with bodies straddling the inline-heap/overflow boundary, while slots churn FREE↔busy and `try_reset_heap` fires. Assert no crash + no `InvalidRegion` errors.

---

## 3. Recommended permanent fix (ranked)

**A. Remove the shared-reset hazard by design (preferred).** ✅ **SHIPPED — see §8.** The inline heap is one bump allocator shared by all slots, reset based on global slot state — inherently cross-coupled. Replace with **per-slot heap arenas**: give each slot a fixed heap sub-range it owns; write/read only within it; never reset globally. Eliminates every cross-slot reset/reuse race at once. Larger change, but kills the class.

**B. If keeping the shared heap:** ❌ **Moot, and it would have made §7 worse** — it gates the reset on an *additional* condition, so it fires less often. Superseded by A. gate `reset_heap` behind an explicit active-reader count (AtomicUsize incremented around `read_request`/`read_response`, transport.rs:226 and :472) AND have readers load `alloc_head` with `Acquire` so there's a real happens-before. `try_reset_heap` resets only when `!has_inflight_slots() && active_readers == 0`.

**C. ~~Bound the overflow read path~~ — NOT NEEDED, the claim was wrong.** See §2a item 1: `read_overflow` has been bounds-checked since the initial release. No work to do.

**D. Harden the slot-reuse/overflow-name churn** (transport.rs:152/244/403) — ✅ **SHIPPED.** Overflow region naming is now generation-stamped, so a slow reader can't open a *new* region created for a reused slot under the same name. See §6.

Minimum to call it "fixed": ~~C~~ + **D** (done) + **A or B** (the actual heap race, still open) + a dump-confirmed repro test.

---

## 4. Key locations

Deliberately **no line numbers**. An earlier revision of this table carried them,
they drifted, and one stale pointer is what produced the false "item C" claim in
§2a — the line had moved, the reader trusted the number instead of the function
body, and a fix was proposed for a bug that never existed. Grep for the symbol.

| Concern | File |
|---|---|
| `heap_read` (unsafe `from_raw_parts`) | `slotbus/src/region.rs` |
| `heap_read_checked` / `heap_write_checked` (0.1.3 guards) | `slotbus/src/region.rs` |
| `slot_arenas` / arena placement (0.2.0, replaced the allocator) | `slotbus/src/region.rs` |
| `alloc_heap` / `reset_heap` / `try_reset_heap` (deprecated, unused by the protocol) | `slotbus/src/region.rs` |
| `has_inflight_slots` (still live — diagnostics only) | `slotbus/src/region.rs` |
| `read_request` / `read_response` | `slotbus/src/region.rs` |
| `read_overflow` (bounds-checked since initial release) | `slotbus/src/region.rs` |
| `create_exclusive` / `create_overflow_fresh` (generation stamping) | `slotbus/src/region.rs` |
| `reclaim_orphaned_backing_file` / `backing_file_path` (0.2.0, Windows only) | `slotbus/src/region.rs` |
| `init_control` / `validate_control` (header-vs-mapping invariant) | `slotbus/src/region.rs` |
| `claim_free_slot` (FREE→WRITING) | `slotbus/src/region.rs` |
| `dispatch` (claim + overflow remove) | `slotbus/src/transport.rs` |
| hub response watcher (read before DONE→FREE) | `slotbus/src/transport.rs` |
| worker receive loop (READY→CLAIMED, read_request, err→FREE) | `slotbus/src/transport.rs` |

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

**No layout change and no version bump** *(true of this change in isolation; superseded by §8)*. `SlotMeta` is untouched at 128 bytes — its 40 reserved bytes were *not* needed — `SHM_VERSION` stayed 1 for this change, and generation 0 produces the byte-identical un-suffixed name older peers derive. The uncontended path was therefore wire-identical to 0.1.3 in both directions.

> Superseded: per-slot arenas (§8) bumped `SHM_VERSION` to 2 in the same 0.2.0
> release, so there is no shipped version where generation stamping is present
> and the wire protocol is still v1. The graceful-degradation property described
> here never had to be relied on in practice.

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

---

## 7. `heap full` — mechanism established, and it is not the overflow leak

The `heap full for request/response meta` errors are a **separate fault from the
overflow-region problem in §6**, with a different cause. Tests:
`slotbus/tests/heap_exhaustion.rs`.

### The hypothesis that was wrong

It was reasonable to suspect a shared cause: a leaked `SlotWorker` pins a slot
non-FREE, `has_inflight_slots()` therefore always reports true, `try_reset_heap`
never fires, and the heap fills. Both symptoms first appeared the same night,
which made one root cause attractive.

**Refuted.** A leaked worker's `overflow_regions` map holds handles to *separate
named mappings* (`*-req-N` / `*-rsp-N`). Slot status lives in the control region
and is written only by the protocol. `holding_overflow_handles_does_not_pin_any_slot`
holds four overflow regions open, proves they are live by reading them back, and
shows `has_inflight_slots()` is still false and `try_reset_heap` still reclaims.
Holding overflow handles cannot veto a reset.

The live system corroborates it: during the burst window the agent worker's
64 slots were all FREE, in the same process that had just logged `heap full`,
with no restart in between. A permanent pin cannot produce a symptom that
recovers on its own — and every observed burst did recover.

### The actual mechanism

`alloc_heap` is a bump allocator. `alloc_head` only moves forward; there is no
per-slot free. The single reclamation path is `reset_heap`, and `try_reset_heap`
calls it **only when every slot is simultaneously FREE**.

Reclamation therefore requires *global quiescence*. Under sustained overlapping
traffic that instant may simply never arrive, and the allocator marches to the
end of the heap. No leak, no stuck slot, and no bug is required — only overlap.

`sustained_overlap_exhausts_heap_with_no_leak_anywhere` demonstrates it: every
slot it claims it also frees, nothing outside the test holds a handle, and
`try_reset_heap` is attempted on **every** iteration exactly as `dispatch` does.
The reset is vetoed each time by whichever slot is still busy. Result: **137
cycles to exhaustion.** The control,
`quiescence_reclaims_the_heap_and_the_same_workload_survives`, runs the identical
geometry and payload with full release between cycles and survives **5,000**
cycles — 36× more — because the reset actually fires.

That contrast is the falsification: overlap is the whole difference.

`a_stuck_slot_vetoes_the_reset_permanently` records that a genuinely stuck slot
*does* block the reset forever. That mechanism is real; it just is not what the
overflow leak does.

### Consequence for the ranked fixes in §3

- **Item B makes this worse, not better.** Gating the reset on an additional
  `active_readers == 0` condition makes it fire *less* often. B addresses the
  corruption race; it cannot address exhaustion.
- **Item A (per-slot heap arenas) is the only listed fix that resolves it.**
  Giving each slot its own sub-range, reset when that slot goes FREE, removes
  the global-quiescence requirement entirely.

**Tradeoff on A, which is why it is not shipped here.** Splitting the heap
`num_slots` ways shrinks the largest inline payload by the same factor — at the
production geometry below, from ~1 MiB to ~16 KiB per slot. Payloads between
those sizes would newly spill to overflow. That is a behaviour change for every
downstream worker, and it shifts load onto the overflow path that §6 has only
just stabilised. Arena sizing (equal split vs. configurable) is a design
decision for the owner, not something to land unilaterally.

### Operational lever, no code change

Production runs `--slots 64` against the default `--region-size 1048576`, giving
`compute_layout(64, 1 MiB)` → ~1016 KiB of heap shared by 64 slots.

Note the interaction: **raising `--slots` makes exhaustion more likely, not
less.** More slots means a lower probability that *all* of them are
simultaneously FREE, while the heap stays the same size. The hub is running
double the slotbus default of 32 with an unchanged 1 MiB region.

Raising `--region-size` buys proportionally more allocations between quiescence
windows. It does not fix the class, but it is one flag, no code, and no protocol
risk — worth doing before any redesign.

---

## 8. Per-slot heap arenas (shipped — item A)

Closes the `heap full` class described in §7, and with it the shared-reset
hazard that §2a spent so long failing to pin down.

**Design.** The inline heap is divided into `num_slots` equal arenas, one per
slot. Each arena is split in half — request payload in the first, response in
the second — and placement inside a half is fixed: metadata at offset 0, body
immediately after it, 8-byte aligned. There is no allocator state anywhere, so
there is nothing to reclaim and no quiescence to wait for. A slot rewrites the
same bytes every cycle, which is correct precisely because it owns them.

Splitting request from response, rather than letting a slot's response reuse the
whole arena, means a response write can never disturb request bytes regardless
of whether a reader has finished with them. That costs half the ceiling and buys
independence from read-completion ordering — a trade worth making in code whose
last three bugs were all ordering assumptions that turned out to be wrong.

**The global machinery is retired, not removed.** `alloc_heap`, `reset_heap` and
`try_reset_heap` are `#[deprecated]` rather than deleted: they are `pub` on a
published crate, so removing them would force a 0.2.0. Nothing in the protocol
calls them now, and `transport.rs` no longer attempts a reset at dispatch or in
the response watcher. `has_inflight_slots` is **not** deprecated — it is still
sound and useful for diagnostics. Callers should know that offsets from
`alloc_heap` now overlap live slot arenas and will corrupt them if written to;
the deprecation note says so.

**SHM_VERSION 1 → 2, deliberately.** The struct layout is byte-identical between
the two designs, and both record absolute heap offsets in the slot, so *reads*
would interoperate — which is exactly why the version had to change. A v1 writer
bump-allocates from the shared `alloc_head` and will hand out an offset that
lands inside a v2 writer's arena. The two then scribble over each other with no
error raised anywhere. `validate_control` now rejects the mismatch outright, so
a stale peer fails loudly at startup instead of corrupting payloads silently.

**Rollout consequence: every peer must be rebuilt in lockstep** — hub,
agent-server, tts-server, stt-server, discord-bridge, and the Tauri app. Unlike
the generation stamping in §6, this one cannot degrade gracefully. A worker that
misses the rebuild will fail to open the region with `bad version: expected 2,
got 1` rather than misbehaving quietly. That is the intended outcome.

**The inline ceiling drops. This is the real cost.** Bodies above it spill to an
overflow region, which is ordinary supported behaviour — and that path is the
one §6 stabilised, with a 51-hour production soak behind it.

| Geometry | Arena | Per-half | Inline body ceiling¹ |
|---|---|---|---|
| hub today: `--slots 64 --region-size 4MiB` | 65,400 | ~32.7 KiB | **~32,184 B** |
| hub before the region-size raise (1 MiB) | 16,248 | ~8.1 KiB | ~7,608 B |
| slotbus defaults: 32 slots, 1 MiB | 32,632 | ~16.3 KiB | ~15,800 B |

¹ assuming a 512-byte serialized metadata blob, which is generous for a hub route.

Previously any single payload could use the whole heap (~4 MB at production
geometry) provided no other slot needed it at that moment. That headroom was
never dependable — it was exactly the coupling that caused the exhaustion. The
ceiling is now small but *guaranteed*, and it scales linearly with
`--region-size`.

**Evidence.** `slotbus/tests/heap_exhaustion.rs`:

- `sustained_overlap_no_longer_exhausts_heap` — the §7 exhaustion test, same
  geometry and same overlap discipline, assertions inverted. 10,000 cycles, no
  exhaustion.
- `one_slots_traffic_cannot_starve_another` — 5,000 cycles hammering slot 0,
  then slot 3 writes and round-trips intact.
- `body_too_large_for_its_arena_spills_to_overflow_and_round_trips` — the new
  boundary, exercised directly.
- `slot_arenas_are_disjoint_and_within_the_heap` — the arenas tile the heap with
  no gap, no overlap, no run-off, all 8-byte aligned.

Falsified rather than assumed: reverting *only* the request write path to
`alloc_heap` fails three of these, and `sustained_overlap_no_longer_exhausts_heap`
reports exhaustion at **137 cycles** — the identical number §7 measured against
the original allocator. The tests carry their own vacuity guards, including one
asserting `alloc_head` never leaves 0, which catches any write path that quietly
goes back to allocating globally.
