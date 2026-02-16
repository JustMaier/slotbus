//! # slotbus
//!
//! Lock-free shared memory IPC with slotted request/response.
//!
//! Slotbus provides sub-microsecond wake latency and sub-millisecond round-trip
//! times for local inter-process communication. Instead of serializing data through
//! sockets or HTTP, processes read and write directly from shared memory pages with
//! OS-level event signaling.
//!
//! ## Architecture
//!
//! - **Control region**: A fixed-size shared memory region (default 1MB) containing
//!   a header, 32 request/response slots, and an inline bump-allocated heap.
//! - **Slots**: Each slot is an independent request/response pair with a lock-free
//!   state machine: `Free → Ready → Claimed → Done → Free`.
//! - **Signaling**: OS-native named events (Windows) for cross-process wakeup
//!   with zero polling.
//! - **Overflow**: Large payloads that don't fit inline automatically spill to
//!   temporary shared memory regions.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use slotbus::{SlotBus, SlotBusConfig};
//!
//! // Hub side — create a bus for a worker
//! let config = SlotBusConfig::builder()
//!     .name("my-worker")
//!     .build();
//! let bus = SlotBus::create(config).unwrap();
//!
//! // Worker side — connect to the bus (same config, different process)
//! let worker_config = SlotBusConfig::builder()
//!     .name("my-worker")
//!     .build();
//! let worker = slotbus::SlotWorker::open(worker_config).unwrap();
//! ```

pub mod config;
pub mod error;
pub mod events;
pub mod region;
pub mod transport;
pub mod types;

pub use config::{SlotBusConfig, SlotBusConfigBuilder};
pub use error::SlotBusError;
pub use transport::{SlotBus, SlotWorker};
