//! High-level transport types for hub-side and worker-side communication.
//!
//! - [`SlotBus`]: Hub-side handle. Creates the SHM region and events, dispatches
//!   requests, watches for responses.
//! - [`SlotWorker`]: Worker-side handle. Opens the SHM region and events, runs
//!   a receive loop, sends responses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::oneshot;
use tracing::{debug, trace, warn};

use crate::config::SlotBusConfig;
use crate::error::SlotBusError;
use crate::events::NamedEvent;
use crate::region::{self, ShmRegion};
use crate::types::*;

// ---- Request / Response types ------------------------------------------------

/// A decoded request received by a worker from the hub.
#[derive(Debug)]
pub struct Request {
    /// Unique request identifier (UUID).
    pub req_id: String,
    /// HTTP method (GET, POST, etc.).
    pub method: String,
    /// Request path (e.g. `/session/abc123`).
    pub path: String,
    /// Matched route pattern (e.g. `/session/:id`).
    pub route_pattern: String,
    /// Extracted path parameters.
    pub path_params: HashMap<String, String>,
    /// Raw query string.
    pub query: Option<String>,
    /// Request body bytes.
    pub body: Vec<u8>,
    /// Request headers.
    pub headers: HashMap<String, String>,
}

/// A decoded response received by the hub from a worker.
#[derive(Debug)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// Content-Type header value.
    pub content_type: String,
    /// Additional response headers.
    pub headers: Vec<(String, String)>,
}

// ---- SlotBus (hub-side) ------------------------------------------------------

/// Pending request entry: (dispatch_time, route_label, oneshot_sender).
type PendingEntry = (Instant, String, oneshot::Sender<Response>);

/// Hub-side handle for dispatching requests to a worker via shared memory.
///
/// Create one `SlotBus` per connected worker. It owns the SHM control region
/// and named events.
///
/// # Example
///
/// ```rust,no_run
/// use slotbus::{SlotBus, SlotBusConfig};
///
/// # async fn example() -> Result<(), slotbus::SlotBusError> {
/// let config = SlotBusConfig::builder()
///     .name("my-worker")
///     .build();
///
/// let bus = SlotBus::create(config)?;
/// bus.start_response_watcher();
/// # Ok(())
/// # }
/// ```
pub struct SlotBus {
    config: SlotBusConfig,
    region: Arc<ShmRegion>,
    req_event: Arc<NamedEvent>,
    rsp_event: Arc<NamedEvent>,
    pending: Arc<std::sync::Mutex<HashMap<String, PendingEntry>>>,
    overflow_regions: Arc<std::sync::Mutex<HashMap<usize, ShmRegion>>>,
    running: Arc<AtomicBool>,
}

impl SlotBus {
    /// Create a new SHM control region and named events for a worker.
    pub fn create(config: SlotBusConfig) -> Result<Self, SlotBusError> {
        let region_name = config.region_name();
        let mut region = ShmRegion::create_or_open(&region_name, config.region_size)?;
        region.init_control(&config);

        let req_event = NamedEvent::create(&config.request_event_name())?;
        let rsp_event = NamedEvent::create(&config.response_event_name())?;

        debug!(
            name = config.name,
            region = region_name,
            slots = config.num_slots,
            "created slotbus region + events"
        );

        Ok(Self {
            config,
            region: Arc::new(region),
            req_event: Arc::new(req_event),
            rsp_event: Arc::new(rsp_event),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            overflow_regions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    /// The configuration used to create this bus.
    pub fn config(&self) -> &SlotBusConfig {
        &self.config
    }

    /// The OS-level name of the SHM region (pass to workers for connection).
    pub fn region_name(&self) -> String {
        self.config.region_name()
    }

    /// Dispatch a request to the worker via SHM.
    ///
    /// Returns a oneshot receiver that resolves when the worker responds.
    pub fn dispatch(
        &self,
        req_id: &str,
        method: &str,
        meta: &RequestMeta,
        body: &[u8],
    ) -> Result<oneshot::Receiver<Response>, SlotBusError> {
        // Try to reclaim heap space
        self.region.try_reset_heap();

        let slot_index = region::claim_free_slot(&self.region)
            .ok_or(SlotBusError::NoFreeSlots(self.config.num_slots))?;

        let meta_bytes = postcard::to_allocvec(meta)?;
        let method_u8 = method_to_u8(method);

        // Drop any stale overflow region for this slot before writing.
        // Prevents SHM name collisions when a slot is reused quickly.
        self.overflow_regions.lock().unwrap().remove(&slot_index);

        let overflow = region::write_request(
            &self.region,
            slot_index,
            req_id,
            method_u8,
            &meta_bytes,
            body,
            &self.config,
        )?;

        if let Some(ovf) = overflow {
            self.overflow_regions
                .lock()
                .unwrap()
                .insert(slot_index, ovf);
        }

        let (tx, rx) = oneshot::channel();
        let label = format!("{method} {}", meta.path);
        self.pending
            .lock()
            .unwrap()
            .insert(req_id.to_string(), (Instant::now(), label, tx));

        // Signal the worker
        self.req_event.signal();

        trace!(slot = slot_index, req_id, method, path = %meta.path, "dispatched request");

        Ok(rx)
    }

    /// Start the response watcher on a dedicated OS thread.
    ///
    /// Scans for Done slots, reads responses, resolves pending oneshots.
    /// Call this once after creating the `SlotBus`. The thread exits
    /// when the `SlotBus` is dropped.
    pub fn start_response_watcher(&self) -> std::thread::JoinHandle<()> {
        let region = Arc::clone(&self.region);
        let rsp_event = Arc::clone(&self.rsp_event);
        let pending = Arc::clone(&self.pending);
        let overflow_regions = Arc::clone(&self.overflow_regions);
        let config = self.config.clone();
        let running = Arc::clone(&self.running);

        std::thread::Builder::new()
            .name(format!("{}-slotbus-rsp", config.name))
            .spawn(move || {
                loop {
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }
                    rsp_event.wait_timeout(config.wait_timeout_ms);

                    let mut freed_any = false;

                    for i in 0..region.num_slots() {
                        let slot = unsafe { region.slot(i) };

                        // Check if this slot has a completed response.
                        if slot.status.load(Ordering::Acquire) != SLOT_DONE {
                            continue;
                        }

                        // Read req_id and response data BEFORE transitioning to Free.
                        // Once the slot is Free, a concurrent dispatch can overwrite
                        // the req_id and heap data.
                        let req_id = {
                            let raw = &slot.req_id;
                            let end = raw.iter().position(|&b| b == 0).unwrap_or(36);
                            String::from_utf8_lossy(&raw[..end]).to_string()
                        };
                        let resp_result = region::read_response(&region, i, &config);

                        // Now transition Done → Free. Since we're the only thread
                        // doing this transition, the CAS should always succeed.
                        if slot
                            .status
                            .compare_exchange(
                                SLOT_DONE,
                                SLOT_FREE,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            continue;
                        }

                        freed_any = true;
                        overflow_regions.lock().unwrap().remove(&i);

                        match resp_result {
                            Ok((status, meta, body)) => {
                                if let Some((t0, label, tx)) =
                                    pending.lock().unwrap().remove(&req_id)
                                {
                                    if config.instrumentation {
                                        let rtt_ms = t0.elapsed().as_secs_f64() * 1000.0;
                                        debug!("{label} → {status} ({rtt_ms:.1}ms)");
                                    }
                                    let _ = tx.send(Response {
                                        status,
                                        body,
                                        content_type: meta.content_type,
                                        headers: meta.headers,
                                    });
                                }
                            }
                            Err(e) => {
                                warn!(slot = i, error = %e, "failed to read response");
                            }
                        }
                    }

                    if freed_any {
                        region.try_reset_heap();
                    }
                }
            })
            .expect("failed to spawn slotbus response watcher thread")
    }

    /// Read the atomic state of every slot, returning `(index, raw_state)` pairs.
    ///
    /// Useful for diagnostics dashboards. State values:
    /// - `0` = Free
    /// - `1` = Ready (hub wrote request, waiting for worker)
    /// - `2` = Claimed (worker processing)
    /// - `3` = Done (worker wrote response, waiting for hub)
    /// - `4` = Writing (hub reserving slot)
    pub fn slot_diagnostics(&self) -> Vec<(usize, u32)> {
        (0..self.region.num_slots())
            .map(|i| {
                let slot = unsafe { self.region.slot(i) };
                (i, slot.status.load(std::sync::atomic::Ordering::Relaxed))
            })
            .collect()
    }

    /// Signal the response watcher and receive loops to stop.
    ///
    /// Called automatically when the `SlotBus` is dropped. You can also
    /// call it explicitly if you need to stop the watcher before drop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        // Signal the event to wake the watcher so it can exit promptly.
        self.rsp_event.signal();
    }
}

impl Drop for SlotBus {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.rsp_event.signal();
    }
}

impl std::fmt::Debug for SlotBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotBus")
            .field("name", &self.config.name)
            .field("num_slots", &self.config.num_slots)
            .finish()
    }
}

// ---- SlotWorker (worker-side) ------------------------------------------------

/// Worker-side handle for receiving requests and sending responses via SHM.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use slotbus::{SlotWorker, SlotBusConfig};
/// use slotbus::transport::Request;
///
/// # fn example() {
/// let config = SlotBusConfig::builder()
///     .name("my-worker")
///     .build();
///
/// let worker = SlotWorker::open(config).unwrap();
/// let worker = Arc::new(worker);
///
/// worker.clone().start_receive_loop(move |w, slot, req| {
///     // Handle request...
///     w.send_response(slot, 200, b"OK".to_vec(), "text/plain", vec![]).unwrap();
/// });
/// # }
/// ```
pub struct SlotWorker {
    config: SlotBusConfig,
    region: Arc<ShmRegion>,
    req_event: Arc<NamedEvent>,
    rsp_event: Arc<NamedEvent>,
    overflow_regions: Arc<std::sync::Mutex<HashMap<usize, ShmRegion>>>,
    running: Arc<AtomicBool>,
}

impl SlotWorker {
    /// Open an existing SHM control region + events created by a [`SlotBus`].
    pub fn open(config: SlotBusConfig) -> Result<Self, SlotBusError> {
        let region_name = config.region_name();
        let mut region = ShmRegion::open(&region_name)?;
        region.validate_control()?;

        let req_event = NamedEvent::open(&config.request_event_name())?;
        let rsp_event = NamedEvent::open(&config.response_event_name())?;

        debug!(
            name = config.name,
            region = region_name,
            "opened slotbus region + events"
        );

        Ok(Self {
            config,
            region: Arc::new(region),
            req_event: Arc::new(req_event),
            rsp_event: Arc::new(rsp_event),
            overflow_regions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    /// The configuration for this worker connection.
    pub fn config(&self) -> &SlotBusConfig {
        &self.config
    }

    /// Write a response into a slot and signal the hub.
    pub fn send_response(
        &self,
        slot_index: usize,
        status: u16,
        body: Vec<u8>,
        content_type: &str,
        headers: Vec<(String, String)>,
    ) -> Result<(), SlotBusError> {
        let resp_meta = ResponseMeta {
            content_type: content_type.to_string(),
            headers,
        };
        let meta_bytes = postcard::to_allocvec(&resp_meta)?;

        // Drop any stale overflow region for this slot before writing.
        // Prevents SHM name collisions when a slot is reused quickly.
        self.overflow_regions.lock().unwrap().remove(&slot_index);

        let result = region::write_response(
            &self.region,
            slot_index,
            status,
            &meta_bytes,
            &body,
            &self.config,
        );

        match result {
            Ok(overflow) => {
                if let Some(ovf) = overflow {
                    self.overflow_regions
                        .lock()
                        .unwrap()
                        .insert(slot_index, ovf);
                }

                self.rsp_event.signal();

                Ok(())
            }
            Err(e) => {
                // Response write failed — free the slot to prevent permanent stall.
                let slot = unsafe { self.region.slot(slot_index) };
                slot.status.store(SLOT_FREE, Ordering::Release);
                Err(e)
            }
        }
    }

    /// Run the request receive loop on a blocking OS thread.
    ///
    /// Calls `handler` for each incoming request. The handler receives
    /// `(Arc<SlotWorker>, slot_index, Request)` and must call
    /// [`send_response`](SlotWorker::send_response) when done.
    pub fn start_receive_loop<F>(self: Arc<Self>, handler: F) -> std::thread::JoinHandle<()>
    where
        F: Fn(Arc<Self>, usize, Request) + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);

        let running = Arc::clone(&self.running);

        std::thread::Builder::new()
            .name(format!("{}-slotbus-recv", self.config.name))
            .spawn(move || loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                self.req_event.wait_timeout(self.config.wait_timeout_ms);
                let wake_time = Instant::now();

                for i in 0..self.region.num_slots() {
                    let slot = unsafe { self.region.slot(i) };

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
                        match region::read_request(&self.region, i, &self.config) {
                            Ok((req_id, method_u8, meta, body)) => {
                                let method_str = u8_to_method(method_u8);

                                if self.config.instrumentation {
                                    let claim_us = wake_time.elapsed().as_micros();
                                    debug!(
                                        slot = i,
                                        claim_us,
                                        method = method_str,
                                        path = %meta.path,
                                        "claimed request"
                                    );
                                }

                                let request = Request {
                                    req_id,
                                    method: method_str.to_string(),
                                    path: meta.path,
                                    route_pattern: meta.route_pattern,
                                    path_params: meta.path_params.into_iter().collect(),
                                    query: meta.query,
                                    body,
                                    headers: meta.headers.into_iter().collect(),
                                };

                                let transport = Arc::clone(&self);
                                let handler = Arc::clone(&handler);
                                handler(transport, i, request);
                            }
                            Err(e) => {
                                warn!(slot = i, error = %e, "failed to read request");
                                slot.status.store(SLOT_FREE, Ordering::Release);
                            }
                        }
                    }
                }
            })
            .expect("failed to spawn slotbus receive thread")
    }

    /// Signal the receive loop to stop.
    ///
    /// Called automatically when the `SlotWorker` is dropped.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.req_event.signal();
    }
}

impl Drop for SlotWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Signal the event to wake the receive loop so it can exit promptly.
        self.req_event.signal();
    }
}

impl std::fmt::Debug for SlotWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotWorker")
            .field("name", &self.config.name)
            .finish()
    }
}
