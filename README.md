# slotbus

**Lock-free shared memory IPC for Rust.**

[![Crates.io](https://img.shields.io/crates/v/slotbus.svg)](https://crates.io/crates/slotbus)
[![docs.rs](https://img.shields.io/docsrs/slotbus)](https://docs.rs/slotbus)
[![CI](https://github.com/JustMaier/slotbus/actions/workflows/ci.yml/badge.svg)](https://github.com/JustMaier/slotbus/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/slotbus.svg)](LICENSE-MIT)

Slotbus provides slotted request/response communication between processes on the same machine. Instead of serializing data through HTTP or Unix sockets, processes read and write directly from shared memory pages with OS-level event signaling. The result is sub-microsecond wake latency and sub-millisecond round-trip times.

## Performance

Measured on Windows 11, AMD Ryzen 9, DDR5:

| Metric | slotbus | HTTP localhost | Unix socket | gRPC | shmem-ipc (ring) |
|---|---|---|---|---|---|
| Wake latency | **0-1 us** | ~50 us | ~20 us | ~100 us | ~1-5 us |
| Round-trip (GET, small) | **0.1-0.4 ms** | 5-15 ms | 1-3 ms | 2-5 ms | N/A (stream) |
| Round-trip (POST, body) | **0.7-0.8 ms** | 8-20 ms | 2-5 ms | 3-8 ms | N/A (stream) |
| Concurrent in-flight | 32 slots | unlimited | unlimited | unlimited | 1 (SPSC) |
| Serialization overhead | postcard (binary) | JSON + HTTP framing | protocol-dependent | protobuf + HTTP/2 | raw bytes |
| CPU while idle | 0% (event wait) | 0% (epoll/IOCP) | 0% (epoll/IOCP) | 0% (epoll/IOCP) | polling or futex |

Slotbus is 10-50x faster than HTTP localhost for request/response workloads because it eliminates kernel socket copies, HTTP parsing, and serialization round-trips. Data lives in shared memory pages that both processes can access directly.

## Architecture

```
                         Hub Process                              Worker Process
                    ┌─────────────────────┐                  ┌─────────────────────┐
                    │     SlotBus          │                  │    SlotWorker        │
                    │  (hub-side handle)   │                  │  (worker-side handle)│
                    └────────┬────────────┘                  └────────┬─────────────┘
                             │                                        │
                   dispatch_request()                       start_receive_loop()
                             │                                        │
          ┌──────────────────▼────────────────────────────────────────▼───────────┐
          │                    Shared Memory Control Region (1 MB)                │
          │                                                                      │
          │  ┌────────────────────────────────────────────────────────────────┐   │
          │  │  Header (64 bytes)                                            │   │
          │  │  magic: 0x48554231 | version: 1 | num_slots: 32              │   │
          │  │  heap_offset | heap_size | alloc_head (AtomicU32)             │   │
          │  └────────────────────────────────────────────────────────────────┘   │
          │                                                                      │
          │  ┌──────────┐ ┌──────────┐ ┌──────────┐         ┌──────────┐        │
          │  │  Slot 0  │ │  Slot 1  │ │  Slot 2  │  . . .  │  Slot 31 │        │
          │  │ 128 bytes│ │ 128 bytes│ │ 128 bytes│         │ 128 bytes│        │
          │  │          │ │          │ │          │         │          │        │
          │  │ status   │ │ status   │ │ status   │         │ status   │        │
          │  │ req_id   │ │ req_id   │ │ req_id   │         │ req_id   │        │
          │  │ method   │ │ method   │ │ method   │         │ method   │        │
          │  │ meta_ptr │ │ meta_ptr │ │ meta_ptr │         │ meta_ptr │        │
          │  │ body_ptr │ │ body_ptr │ │ body_ptr │         │ body_ptr │        │
          │  │ resp_ptr │ │ resp_ptr │ │ resp_ptr │         │ resp_ptr │        │
          │  └──────────┘ └──────────┘ └──────────┘         └──────────┘        │
          │                                                                      │
          │  ┌────────────────────────────────────────────────────────────────┐   │
          │  │  Inline Heap (~1MB - header - slots)                          │   │
          │  │  Bump-allocated. Metadata and small bodies written here.      │   │
          │  │  CAS on alloc_head for thread-safe allocation.                │   │
          │  │  Auto-reset when all slots are Free.                          │   │
          │  └────────────────────────────────────────────────────────────────┘   │
          └──────────────────────────────────────────────────────────────────────┘

          ┌──────────────────────────────────────────────────────────────────┐
          │  Overflow Regions (temporary, per-slot, created on demand)       │
          │  slotbus-{name}-req-{slot}  — large request bodies              │
          │  slotbus-{name}-rsp-{slot}  — large response bodies             │
          └──────────────────────────────────────────────────────────────────┘

          Signaling (zero-polling cross-process wakeup):
          ┌─────────────────────────────────────────────┐
          │  slotbus-{name}-req  ──►  wake worker       │   Hub signals after writing Ready slot
          │  slotbus-{name}-rsp  ──►  wake hub          │   Worker signals after writing Done slot
          └─────────────────────────────────────────────┘
          Windows: Named Events (CreateEventW / SetEvent / WaitForSingleObject)
          Linux:   eventfd (planned)
```

### Slot State Machine

Each slot transitions through four states using `AtomicU32` compare-and-swap:

```
    Hub writes request           Worker CAS            Worker writes response       Hub CAS
         into slot              Ready → Claimed             into slot             Done → Free
            │                       │                          │                      │
            ▼                       ▼                          ▼                      ▼
  ┌──────┐      ┌───────┐      ┌─────────┐      ┌──────┐      ┌──────┐
  │ Free │ ───► │ Ready │ ───► │ Claimed │ ───► │ Done │ ───► │ Free │
  └──────┘      └───────┘      └─────────┘      └──────┘      └──────┘
     0              1               2               3             0

  Free (0)    — Slot is available for a new request
  Ready (1)   — Hub has written request data; worker may claim it
  Claimed (2) — Worker is processing the request
  Done (3)    — Worker has written response data; hub may read it
```

No mutexes. No spinlocks. Just atomic CAS with `Acquire`/`Release` ordering to ensure memory visibility across processes.

## Quick Start

Add slotbus to your `Cargo.toml`:

```toml
[dependencies]
slotbus = "0.1"
```

### Hub side — create a bus and dispatch requests

```rust
use slotbus::{SlotBus, SlotBusConfig};

// Create a shared memory bus for a worker named "my-worker"
let config = SlotBusConfig::builder()
    .name("my-worker")
    .num_slots(32)          // 32 concurrent in-flight requests
    .region_size(1_048_576) // 1MB control region
    .build();

let bus = SlotBus::create(config)?;

// Start the response watcher (background thread)
bus.start_response_watcher();

// Dispatch a request — returns a oneshot receiver
let response = bus.dispatch_request(
    "req-001",          // request ID
    "GET",              // HTTP method
    "/api/status",      // path
    &[],                // request body
)?;

// Wait for the worker's response
let resp = response.await?;
println!("Status: {}, Body: {} bytes", resp.status, resp.body.len());
```

### Worker side — connect and handle requests

```rust
use slotbus::SlotWorker;

// Open the shared memory region created by the hub
let worker = SlotWorker::open("my-worker", Default::default())?;

// Start the receive loop (runs on a dedicated OS thread)
worker.start_receive_loop(|transport, slot_index, request| {
    println!("{} {}", request.method, request.path);

    // Process the request...
    let response_body = b"OK";

    // Write response back through shared memory
    transport.send_response(
        slot_index,
        200,                            // HTTP status
        response_body.to_vec(),         // body
        "text/plain",                   // content-type
        vec![],                         // extra headers
    ).unwrap();
});
```

## How It Works

### Request lifecycle

1. **Hub** finds a free slot (linear scan, typically slot 0 or 1 for low-traffic workloads).
2. **Hub** bump-allocates space on the inline heap for serialized `RequestMeta` (path, headers, query params) and the request body.
3. **Hub** writes metadata pointers and body pointers into the slot's fixed-size fields (128 bytes per slot).
4. **Hub** atomically sets the slot status from `Free` to `Ready` with `Release` ordering.
5. **Hub** signals the request event — the worker wakes up in under 1 microsecond.
6. **Worker** scans slots, finds the `Ready` one, and CAS transitions it to `Claimed`.
7. **Worker** reads request data from the heap using the slot's offset/length pointers.
8. **Worker** processes the request, writes the response into the heap (or an overflow region), and sets the slot to `Done`.
9. **Worker** signals the response event — the hub wakes up.
10. **Hub** reads the response, resolves the oneshot channel, and CAS transitions the slot back to `Free`.

### Inline heap + overflow

The control region contains a bump-allocated heap after the header and slots. Small payloads (metadata, typical JSON bodies) are written inline — no extra allocations, no extra shared memory regions.

When the heap is full, large payloads spill to **overflow regions**: temporary named shared memory mappings (`slotbus-{name}-req-{slot}` or `slotbus-{name}-rsp-{slot}`). These are created on demand and kept alive until the slot is freed.

The heap resets automatically when all slots return to `Free` — no fragmentation, no GC.

### Serialization

Metadata is serialized with [postcard](https://crates.io/crates/postcard), a compact binary format. Request and response bodies are raw bytes — slotbus does not impose a serialization format on your payloads.

### Event signaling

Each worker has two OS-level named events:
- **Request event** (`slotbus-{name}-req`): hub signals after writing a `Ready` slot
- **Response event** (`slotbus-{name}-rsp`): worker signals after writing a `Done` slot

Events are auto-reset: a single `WaitForSingleObject` call blocks until signaled, then automatically resets. No polling loops, no busy-waiting, no timer ticks. The 5-second timeout in the wait call is a safety fallback — under normal operation, the event fires in under 1 microsecond.

On Windows, these are [Named Events](https://learn.microsoft.com/en-us/windows/win32/sync/event-objects) via `CreateEventW` / `SetEvent` / `WaitForSingleObject`. Linux support via `eventfd` is planned.

## Configuration

All options are set through the builder:

```rust
let config = SlotBusConfig::builder()
    .name("my-worker")        // Required. OS identifier for the SHM region.
    .prefix("slotbus")        // Prefix for all OS names. Default: "slotbus".
    .num_slots(32)             // Concurrent request slots. Default: 32. Range: 1-256.
    .region_size(1_048_576)    // Control region size in bytes. Default: 1MB.
    .wait_timeout_ms(5_000)    // Event wait timeout (safety fallback). Default: 5000ms.
    .instrumentation(false)    // Latency logging. Default: false.
    .build();
```

| Option | Default | Description |
|---|---|---|
| `name` | *required* | Worker name. Used to derive all OS-level identifiers: `{prefix}-{name}` for the SHM region, `{prefix}-{name}-req` / `{prefix}-{name}-rsp` for events. |
| `prefix` | `"slotbus"` | Namespace prefix. Change this to run multiple independent slotbus instances on the same machine. |
| `num_slots` | `32` | Number of concurrent request/response slots. Each slot is 128 bytes of fixed metadata. More slots = more concurrency, but the heap shrinks. Clamped to 1-256. |
| `region_size` | `1,048,576` | Total size of the control region in bytes. Must fit the header (64B) + slots (128B each) + heap. With 32 slots, the heap gets ~1,044,416 bytes. |
| `wait_timeout_ms` | `5,000` | Maximum time (ms) to block on an event wait. Safety fallback only — the event signal provides sub-microsecond wakeup. |
| `instrumentation` | `false` | When enabled, logs timing data for slot claims, round-trips, and heap allocations via `tracing`. |

### Derived OS names

For a worker named `"my-worker"` with the default prefix:

| Resource | OS Name |
|---|---|
| Control region | `slotbus-my-worker` |
| Request event | `slotbus-my-worker-req` |
| Response event | `slotbus-my-worker-rsp` |
| Request overflow (slot 5) | `slotbus-my-worker-req-5` |
| Response overflow (slot 5) | `slotbus-my-worker-rsp-5` |

## slotbus-hub

[`slotbus-hub`](https://github.com/JustMaier/slotbus-hub) is a standalone HTTP-to-SHM router binary built on slotbus. Workers register routes via HTTP; clients send normal HTTP requests; the hub dispatches them through shared memory with sub-millisecond round trips.

Install it separately: `cargo install slotbus-hub` — see the [slotbus-hub repo](https://github.com/JustMaier/slotbus-hub) for full documentation.

## Comparison

| Feature | slotbus | Unix socket | HTTP localhost | gRPC | shmem-ipc | iceoryx2 |
|---|---|---|---|---|---|---|
| **Topology** | Hub/worker (req/rsp) | Point-to-point | Client/server | Client/server | Point-to-point | Pub/sub |
| **Latency** | 0.1-0.8 ms RTT | 1-5 ms RTT | 5-20 ms RTT | 2-8 ms RTT | <0.1 ms (stream) | <0.1 ms |
| **Wake mechanism** | Named events | epoll/IOCP | epoll/IOCP | epoll/IOCP | futex/polling | waitset |
| **Concurrency** | 32 slots (configurable) | unlimited | unlimited | unlimited | 1 (SPSC) | per-publisher |
| **Request/response** | Native | Manual | Native | Native | Manual | No (pub/sub) |
| **Zero-copy reads** | Inline heap | No | No | No | Yes | Yes |
| **Serialization** | postcard (meta only) | user choice | HTTP + JSON | protobuf | raw bytes | raw bytes |
| **HTTP bridge** | slotbus-hub binary | manual | native | grpc-web | manual | no |
| **Route registration** | Dynamic (runtime) | N/A | framework | protobuf schema | N/A | topic-based |
| **Overflow handling** | Auto spillover regions | N/A | chunked transfer | streaming | fixed buffer | loan mechanism |
| **Windows** | Yes | Partial | Yes | Yes | No | Yes |
| **Linux** | Yes | Yes | Yes | Yes | Yes | Yes |
| **macOS** | Yes | Yes | Yes | Yes | Yes | Yes |

**When to use slotbus:**
- You need request/response semantics (not streaming or pub/sub)
- Your processes are on the same machine
- Latency matters — you want sub-millisecond round-trips
- You want an HTTP-compatible interface without HTTP overhead (via slotbus-hub)
- You have multiple workers behind a single entry point

**When to use something else:**
- Cross-machine communication (use gRPC or HTTP)
- Pure streaming / pub-sub (use iceoryx2 or ZeroMQ)
- Single-producer single-consumer with maximum throughput (use shmem-ipc ring buffers)
- You need a non-Windows platform (slotbus supports Windows, Linux, and macOS)

## Platform Support

| Platform | Status | Signaling mechanism |
|---|---|---|
| **Windows** | Supported | Named Events (`CreateEventW` / `SetEvent` / `WaitForSingleObject`) |
| **Linux** | Supported | POSIX named semaphores (`sem_open` / `sem_post` / `sem_timedwait`) |
| **macOS** | Supported | POSIX named semaphores (`sem_open` / `sem_post` / `sem_trywait` polling) |

The shared memory layer uses the [`shared_memory`](https://crates.io/crates/shared_memory) crate, which supports all three platforms. The signaling layer uses platform-native primitives for sub-microsecond wake latency on Windows and Linux. macOS uses a polling fallback (~1ms resolution) since `sem_timedwait` is not available.

## Minimum Supported Rust Version

1.75

## Contributing

Contributions are welcome. Please open an issue first to discuss what you'd like to change.

```bash
# Clone and build
git clone https://github.com/JustMaier/slotbus.git
cd slotbus
cargo build

# Run tests
cargo test

# Run with instrumentation logging
RUST_LOG=slotbus=trace cargo test
```

The codebase is small by design. The core transport is under 1,000 lines.

## License

Licensed under the [MIT License](LICENSE-MIT).
