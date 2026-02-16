# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Extracted `slotbus-hub` binary into its own repo: [slotbus-hub](https://github.com/JustMaier/slotbus-hub)

## [0.1.0] - 2026-02-16

### Added

- Lock-free slotted shared memory IPC with 32 concurrent in-flight requests per worker
- Windows Named Events for sub-microsecond cross-process signaling
- Configurable slot count, region size, and request timeouts
- Inline heap with automatic overflow regions for variable-size payloads
- Zero-copy payload access within shared memory regions
- Postcard-based binary serialization for slot metadata

[Unreleased]: https://github.com/JustMaier/slotbus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/JustMaier/slotbus/releases/tag/v0.1.0
