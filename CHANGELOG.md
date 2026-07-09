# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Linux support via POSIX named semaphores (`sem_timedwait` — sub-microsecond wake)
- macOS support via POSIX named semaphores (`sem_trywait` polling — ~1ms resolution)
- FFI: `raw_handle()` on `NamedEvent` for Windows (exposes `HANDLE` as `isize`)

### Changed

- Extracted `slotbus-hub` binary into its own repo: [slotbus-hub](https://github.com/JustMaier/slotbus-hub)

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
