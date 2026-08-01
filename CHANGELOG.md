# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-08-01

**Wire-protocol break.** `SHM_VERSION` goes 1 → 2, so a 0.2.0 process cannot
talk to a 0.1.x process. The mismatch is rejected at startup with
`bad version: expected 2, got 1` rather than failing later on a large payload.
Every peer sharing a region must be rebuilt together. This is why the release is
0.2.0 and not 0.1.4 — the Rust API surface is nearly additive, but Cargo cannot
see a wire break, and `slotbus = "0.1"` would otherwise deliver it through a
routine `cargo update`.

### Fixed

- **Inline heap could exhaust under sustained load, with no leak involved.**
  The heap was one bump allocator shared by every slot, and the only path that
  reclaimed space required *global quiescence* — every slot FREE at the same
  instant. Under sustained overlap that instant may never arrive, and writes
  failed with `heap full for response meta`. Each slot now owns a fixed arena,
  split into a request half and a response half, with no allocator state and
  nothing to reclaim.
- **Stale backing files silently pinned regions at their old size.**
  `shared_memory` backs each mapping with a file under the system temp
  directory and only removes it on a clean `Drop`, so any hard kill leaks it
  permanently. A later run adopted the stale mapping *at its original size*.
  Orphaned files are now reclaimed — proven orphaned by an exclusive open, so a
  live region is never touched — instead of adopted.
- **Headers could describe a heap larger than the mapping.** `init_control`
  derived the layout from the *requested* `region_size` while the mapping might
  have been adopted at a smaller size. Every bounds check validates against the
  header, so they became rubber stamps for out-of-bounds access. `init_control`
  now errors rather than writing a header it cannot back, and `validate_control`
  enforces `heap_offset + heap_size <= mapping length` on open as well.
- Overflow region names are generation-stamped, so a slow reader can no longer
  open a *new* region created for a reused slot under the same name.
- Two error strings claimed `stale same-name mapping still open`. Nothing was
  open — the backing file outlived every process. The wording sent three
  separate investigations hunting for a live handle.

### Added

- Linux support via POSIX named semaphores (`sem_timedwait` — sub-microsecond wake)
- macOS support via POSIX named semaphores (`sem_trywait` polling — ~1ms resolution)
- FFI: `raw_handle()` on `NamedEvent` for Windows (exposes `HANDLE` as `isize`)
- `ShmRegion::slot_arenas` exposes the per-slot layout

### Changed

- Extracted `slotbus-hub` binary into its own repo: [slotbus-hub](https://github.com/JustMaier/slotbus-hub)
- `init_control` returns `Result` (breaking; it can now refuse an undersized mapping)

### Deprecated

- `alloc_heap`, `reset_heap`, `try_reset_heap`. The protocol no longer uses a
  shared bump allocator. Offsets returned by `alloc_heap` overlap the per-slot
  arenas and **will corrupt live slots if written to**. They remain only because
  the regression tests use them to demonstrate the failure mode arenas fix.

### Known limitations

- **Orphaned-backing-file reclaim is Windows-only.** The POSIX path
  (`shm_open` under `/dev/shm`) leaks identically after a hard kill. `shm_open`
  has no share modes and nothing in a segment proves its owner is alive, so
  every available liveness check is racy — and a wrong guess deletes a *live*
  region. No reclaim is safer than a wrong one. This affects the Linux and
  macOS support shipping for the first time in this release.

## [0.1.3] - 2026-07-09

### Fixed

- **Access violation (0xC0000005) in `write_response`/`write_request` under load.**
  `create_overflow` used `create_or_open`, which silently opens an *existing*
  mapping when the name is already in use — keeping the old mapping's original
  size. Overflow names derive from the slot index alone, so a larger payload
  reusing a slot whose previous, smaller overflow mapping was still open wrote
  past the end of the mapping and crashed the process. `create_overflow` now
  validates the mapping size before copying and returns a recoverable
  `SharedMemory` error instead. Read paths (`read_overflow`) already had this
  check; the write side now matches.

### Added

- Bounds-checked heap accessors `heap_read_checked` / `heap_write_checked`;
  all slot read/write paths now route through them, converting stale or
  corrupt offsets into recoverable `InvalidRegion` errors instead of wild
  pointer dereferences
- `slot_index` range guards at the top of `write_request` / `write_response`

## [0.1.0] - 2026-02-16

### Added

- Lock-free slotted shared memory IPC with 32 concurrent in-flight requests per worker
- Windows Named Events for sub-microsecond cross-process signaling
- Configurable slot count, region size, and request timeouts
- Inline heap with automatic overflow regions for variable-size payloads
- Zero-copy payload access within shared memory regions
- Postcard-based binary serialization for slot metadata

[Unreleased]: https://github.com/JustMaier/slotbus/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/JustMaier/slotbus/compare/v0.1.0...v0.1.3
[0.1.0]: https://github.com/JustMaier/slotbus/releases/tag/v0.1.0
