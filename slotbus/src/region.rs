//! Shared memory region wrapper.
//!
//! Provides safe(ish) access to the control region's header, slots, and inline
//! heap, plus helpers for creating/opening overflow regions.

use std::sync::atomic::Ordering;

use shared_memory::{Shmem, ShmemConf, ShmemError};

use crate::config::SlotBusConfig;
use crate::error::SlotBusError;
use crate::types::*;

// ---- ShmRegion ---------------------------------------------------------------

/// Wrapper around a named shared memory mapping.
///
/// The raw pointer + length are extracted at construction time so the struct
/// can be `Send + Sync` (the underlying `Shmem` is `!Send`).
pub struct ShmRegion {
    _shmem: Shmem,
    ptr: *mut u8,
    len: usize,
    name: String,
    num_slots: usize,
    heap_offset: usize,
    heap_size: usize,
}

// SAFETY: Shared memory is designed for cross-thread/cross-process access.
// All mutable access uses atomic operations or the protocol-level guarantee
// that only one side writes to a given offset at a time.
unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// Create a new named shared memory region.
    pub fn create(name: &str, size: usize) -> Result<Self, SlotBusError> {
        let shmem = ShmemConf::new()
            .os_id(name)
            .size(size)
            .create()
            .map_err(|e| SlotBusError::SharedMemory(format!("create '{name}': {e}")))?;

        let ptr = shmem.as_ptr();
        let len = shmem.len();
        Ok(Self {
            _shmem: shmem,
            ptr,
            len,
            name: name.to_string(),
            num_slots: 0,
            heap_offset: 0,
            heap_size: 0,
        })
    }

    /// Open an existing named shared memory region.
    pub fn open(name: &str) -> Result<Self, SlotBusError> {
        let shmem = ShmemConf::new()
            .os_id(name)
            .open()
            .map_err(|e| SlotBusError::SharedMemory(format!("open '{name}': {e}")))?;

        let ptr = shmem.as_ptr();
        let len = shmem.len();
        Ok(Self {
            _shmem: shmem,
            ptr,
            len,
            name: name.to_string(),
            num_slots: 0,
            heap_offset: 0,
            heap_size: 0,
        })
    }

    /// Try to create; if already exists, open instead.
    pub fn create_or_open(name: &str, size: usize) -> Result<Self, SlotBusError> {
        match ShmemConf::new().os_id(name).size(size).create() {
            Ok(shmem) => {
                let ptr = shmem.as_ptr();
                let len = shmem.len();
                Ok(Self {
                    _shmem: shmem,
                    ptr,
                    len,
                    name: name.to_string(),
                    num_slots: 0,
                    heap_offset: 0,
                    heap_size: 0,
                })
            }
            Err(ShmemError::MappingIdExists) => Self::open(name),
            Err(e) => Err(SlotBusError::SharedMemory(format!(
                "create_or_open '{name}': {e}"
            ))),
        }
    }

    /// The OS-level name of this region.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Total size of the mapping in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty (always false for valid regions).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Raw pointer to the start of the mapping.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Number of slots (set after init/validate).
    pub fn num_slots(&self) -> usize {
        self.num_slots
    }

    // ---- Control region: header access ---------------------------------------

    /// Get a reference to the header (offset 0).
    ///
    /// # Safety
    /// Caller must ensure this is a control region of at least `SHM_HEADER_SIZE`.
    pub unsafe fn header(&self) -> &ShmHeader {
        &*(self.ptr as *const ShmHeader)
    }

    /// Get a reference to a slot.
    ///
    /// # Safety
    /// Caller must ensure `index < num_slots` and this is a control region.
    pub unsafe fn slot(&self, index: usize) -> &SlotMeta {
        debug_assert!(index < self.num_slots);
        let offset = SHM_HEADER_SIZE + index * SLOT_META_SIZE;
        &*(self.ptr.add(offset) as *const SlotMeta)
    }

    // ---- Control region: inline heap -----------------------------------------

    /// Raw pointer to the start of the inline heap.
    fn heap_ptr(&self) -> *mut u8 {
        unsafe { self.ptr.add(self.heap_offset) }
    }

    /// Read `len` bytes from the heap at `offset`.
    ///
    /// # Safety
    /// Caller must ensure `offset + len <= heap_size`.
    pub unsafe fn heap_read(&self, offset: u32, len: usize) -> &[u8] {
        let p = self.heap_ptr().add(offset as usize);
        std::slice::from_raw_parts(p, len)
    }

    /// Bounds-checked heap read. Validates `offset + len` against the heap
    /// size before dereferencing, so a stale or garbage `offset`/`len` — e.g.
    /// a slot whose heap bytes were reset/overwritten by a concurrent
    /// `reset_heap` while this reader was mid-flight — yields a recoverable
    /// error instead of a wild `from_raw_parts` that walks off the mapping
    /// and access-violates (0xC0000005). This is the safe entry point for all
    /// read paths; prefer it over the unchecked `heap_read`.
    pub fn heap_read_checked(&self, offset: u32, len: usize) -> Result<&[u8], SlotBusError> {
        match (offset as usize).checked_add(len) {
            Some(end) if end <= self.heap_size => Ok(unsafe { self.heap_read(offset, len) }),
            _ => Err(SlotBusError::InvalidRegion(format!(
                "heap read out of bounds: offset={offset} len={len} heap_size={}",
                self.heap_size
            ))),
        }
    }

    /// Write `data` to the heap at `offset`.
    ///
    /// # Safety
    /// Caller must ensure `offset + data.len() <= heap_size`.
    pub unsafe fn heap_write(&self, offset: u32, data: &[u8]) {
        let p = self.heap_ptr().add(offset as usize);
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    }

    /// Bounds-checked heap write. Validates `offset + data.len()` against the
    /// heap size before writing, so a corrupt/stale offset can't `copy` past
    /// the mapping and access-violate (0xC0000005 WRITE). Mirrors
    /// `heap_read_checked`; the safe entry point for all write paths. Logs the
    /// offending geometry on violation so the bad value is captured.
    pub fn heap_write_checked(&self, offset: u32, data: &[u8]) -> Result<(), SlotBusError> {
        match (offset as usize).checked_add(data.len()) {
            Some(end) if end <= self.heap_size => {
                unsafe { self.heap_write(offset, data) };
                Ok(())
            }
            _ => Err(SlotBusError::InvalidRegion(format!(
                "heap write out of bounds: offset={offset} len={} heap_size={} base={:p}",
                data.len(),
                self.heap_size,
                self.ptr
            ))),
        }
    }

    /// Allocate `size` bytes from the inline heap (bump allocator).
    ///
    /// Returns the heap offset, or `None` if the heap is full.
    /// Thread-safe via CAS on `alloc_head`.
    pub fn alloc_heap(&self, size: usize) -> Option<u32> {
        let aligned = (size + 7) & !7; // align to 8 bytes
        let header = unsafe { self.header() };
        let heap_size = self.heap_size;

        loop {
            let head = header.alloc_head.load(Ordering::Acquire);
            let new_head = head as usize + aligned;

            if new_head > heap_size {
                return None; // heap full
            }

            if header
                .alloc_head
                .compare_exchange(head, new_head as u32, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(head);
            }
            // CAS failed, retry
        }
    }

    /// Reset the heap allocator to zero. Only safe when all slots are Free.
    pub fn reset_heap(&self) {
        let header = unsafe { self.header() };
        header.alloc_head.store(0, Ordering::Release);
    }

    /// Check if any slots are in-flight (not Free).
    pub fn has_inflight_slots(&self) -> bool {
        for i in 0..self.num_slots {
            let slot = unsafe { self.slot(i) };
            if slot.status.load(Ordering::Acquire) != SLOT_FREE {
                return true;
            }
        }
        false
    }

    /// Try to reset heap if no slots are in-flight.
    pub fn try_reset_heap(&self) {
        if !self.has_inflight_slots() {
            self.reset_heap();
        }
    }

    // ---- Control region: initialization --------------------------------------

    /// Initialize a freshly-created control region with the given config.
    pub fn init_control(&mut self, config: &SlotBusConfig) {
        let (heap_offset, heap_size) = compute_layout(config.num_slots, config.region_size);
        self.num_slots = config.num_slots;
        self.heap_offset = heap_offset;
        self.heap_size = heap_size;

        // Zero the entire region
        unsafe {
            std::ptr::write_bytes(self.ptr, 0, self.len);
        }

        // Write header fields
        unsafe {
            let h = self.ptr as *mut u32;
            h.write(SHM_MAGIC);
            h.add(1).write(SHM_VERSION);
            h.add(2).write(config.num_slots as u32);
            h.add(3).write(heap_offset as u32);
            h.add(4).write(heap_size as u32);
        }
        let header = unsafe { self.header() };
        header.alloc_head.store(0, Ordering::Release);
    }

    /// Validate that a control region has the correct magic/version and read layout.
    pub fn validate_control(&mut self) -> Result<(), SlotBusError> {
        unsafe {
            let h = self.ptr as *const u32;
            let magic = h.read();
            let version = h.add(1).read();
            let num_slots = h.add(2).read() as usize;
            let heap_offset = h.add(3).read() as usize;
            let heap_size = h.add(4).read() as usize;

            if magic != SHM_MAGIC {
                return Err(SlotBusError::InvalidRegion(format!(
                    "bad magic: expected 0x{SHM_MAGIC:08X}, got 0x{magic:08X}"
                )));
            }
            if version != SHM_VERSION {
                return Err(SlotBusError::InvalidRegion(format!(
                    "bad version: expected {SHM_VERSION}, got {version}"
                )));
            }

            self.num_slots = num_slots;
            self.heap_offset = heap_offset;
            self.heap_size = heap_size;
        }
        Ok(())
    }

    // ---- Overflow helpers (static) -------------------------------------------

    /// Create an overflow region and write data into it.
    pub fn create_overflow(name: &str, data: &[u8]) -> Result<Self, SlotBusError> {
        // Round up to page size (4096 on Windows)
        let size = (data.len() + 4095) & !4095;
        let size = size.max(4096);
        let region = Self::create_or_open(name, size)?;
        // create_or_open falls back to opening an EXISTING mapping when the
        // name is already in use (e.g. a prior overflow for this slot whose
        // handle is still held somewhere). That mapping keeps its original
        // size, so a larger payload here would copy past the end of the
        // mapping — a wild write (0xC0000005). Mirror read_overflow's check.
        if region.len < data.len() {
            return Err(SlotBusError::SharedMemory(format!(
                "overflow region '{name}' too small for write: need {}, have {} (stale same-name mapping still open)",
                data.len(),
                region.len
            )));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), region.ptr, data.len());
        }
        Ok(region)
    }

    /// Open an overflow region and read `len` bytes from it.
    pub fn read_overflow(name: &str, len: usize) -> Result<Vec<u8>, SlotBusError> {
        let region = Self::open(name)?;
        if len > region.len {
            return Err(SlotBusError::SharedMemory(format!(
                "overflow region '{name}' too small: need {len}, have {}",
                region.len
            )));
        }
        let data = unsafe { std::slice::from_raw_parts(region.ptr, len) };
        Ok(data.to_vec())
    }
}

impl std::fmt::Debug for ShmRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmRegion")
            .field("name", &self.name)
            .field("len", &self.len)
            .field("num_slots", &self.num_slots)
            .finish()
    }
}

// ---- Find free slot ----------------------------------------------------------

/// Atomically find and reserve a free slot via CAS `Free → Writing`.
///
/// Returns the slot index. The caller must call [`write_request`] to fill
/// the slot data and transition it to `Ready`. If the write fails, the
/// caller must set the slot back to `Free`.
///
/// This is safe to call from multiple threads concurrently — only one
/// thread can win the CAS for any given slot.
pub fn claim_free_slot(region: &ShmRegion) -> Option<usize> {
    for i in 0..region.num_slots() {
        let slot = unsafe { region.slot(i) };
        if slot
            .status
            .compare_exchange(SLOT_FREE, SLOT_WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(i);
        }
    }
    None
}

/// Scan slots for the first Free one. Returns the slot index.
///
/// **Deprecated:** Use [`claim_free_slot`] instead. This function is not safe
/// for concurrent callers — two threads can return the same index.
#[deprecated(note = "Use claim_free_slot() which atomically reserves the slot")]
pub fn find_free_slot(region: &ShmRegion) -> Option<usize> {
    for i in 0..region.num_slots() {
        let slot = unsafe { region.slot(i) };
        if slot.status.load(Ordering::Acquire) == SLOT_FREE {
            return Some(i);
        }
    }
    None
}

// ---- Write helpers -----------------------------------------------------------

/// Write request data into a slot + heap (or overflow).
///
/// The slot must already be in `Writing` state (reserved via [`claim_free_slot`]).
/// Sets slot status to `Ready` after writing. On failure, resets the slot to `Free`.
/// Returns the overflow region handle if one was needed (caller must keep it alive).
pub fn write_request(
    region: &ShmRegion,
    slot_index: usize,
    req_id: &str,
    method: u8,
    meta_bytes: &[u8],
    body: &[u8],
    config: &SlotBusConfig,
) -> Result<Option<ShmRegion>, SlotBusError> {
    match write_request_inner(region, slot_index, req_id, method, meta_bytes, body, config) {
        Ok(overflow) => Ok(overflow),
        Err(e) => {
            // Release the reserved slot so it doesn't stay stuck in Writing.
            let slot = unsafe { region.slot(slot_index) };
            slot.status.store(SLOT_FREE, Ordering::Release);
            Err(e)
        }
    }
}

fn write_request_inner(
    region: &ShmRegion,
    slot_index: usize,
    req_id: &str,
    method: u8,
    meta_bytes: &[u8],
    body: &[u8],
    config: &SlotBusConfig,
) -> Result<Option<ShmRegion>, SlotBusError> {
    if slot_index >= region.num_slots() {
        return Err(SlotBusError::InvalidRegion(format!(
            "write_request slot_index {slot_index} >= num_slots {}",
            region.num_slots()
        )));
    }
    let slot = unsafe { region.slot(slot_index) };

    // Write req_id + method via raw pointer
    let id_bytes = req_id.as_bytes();
    let id_len = id_bytes.len().min(36);
    unsafe {
        let slot_ptr = region
            .as_ptr()
            .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
        let req_id_ptr = slot_ptr.add(4);
        std::ptr::write_bytes(req_id_ptr, 0, 36);
        std::ptr::copy_nonoverlapping(id_bytes.as_ptr(), req_id_ptr, id_len);
        slot_ptr.add(40).write(method);
    }

    // Allocate and write metadata into heap
    let meta_offset = region
        .alloc_heap(meta_bytes.len())
        .ok_or_else(|| SlotBusError::SharedMemory("heap full for request meta".into()))?;
    region.heap_write_checked(meta_offset, meta_bytes)?;

    // Write meta pointer fields
    unsafe {
        let slot_ptr = region
            .as_ptr()
            .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
        (slot_ptr.add(44) as *mut u32).write(meta_offset);
        (slot_ptr.add(48) as *mut u16).write(meta_bytes.len() as u16);
    }

    // Write body (inline or overflow)
    let mut overflow_region = None;
    if body.is_empty() {
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(52) as *mut u32).write(0);
            (slot_ptr.add(56) as *mut u32).write(0);
            slot_ptr.add(60).write(0);
        }
    } else if let Some(body_offset) = region.alloc_heap(body.len()) {
        region.heap_write_checked(body_offset, body)?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(52) as *mut u32).write(body_offset);
            (slot_ptr.add(56) as *mut u32).write(body.len() as u32);
            slot_ptr.add(60).write(0);
        }
    } else {
        let name = config.request_overflow_name(slot_index);
        let ovf = ShmRegion::create_overflow(&name, body)?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(52) as *mut u32).write(0);
            (slot_ptr.add(56) as *mut u32).write(body.len() as u32);
            slot_ptr.add(60).write(1);
        }
        overflow_region = Some(ovf);
    }

    // Set status -> Ready (Writing → Ready)
    slot.status.store(SLOT_READY, Ordering::Release);

    Ok(overflow_region)
}

/// Write response data into a slot + heap (or overflow).
///
/// Sets slot status to `Done` after writing.
pub fn write_response(
    region: &ShmRegion,
    slot_index: usize,
    status: u16,
    meta_bytes: &[u8],
    body: &[u8],
    config: &SlotBusConfig,
) -> Result<Option<ShmRegion>, SlotBusError> {
    if slot_index >= region.num_slots() {
        return Err(SlotBusError::InvalidRegion(format!(
            "write_response slot_index {slot_index} >= num_slots {}",
            region.num_slots()
        )));
    }
    let slot = unsafe { region.slot(slot_index) };

    // Write resp_status
    unsafe {
        let slot_ptr = region
            .as_ptr()
            .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
        (slot_ptr.add(64) as *mut u16).write(status);
    }

    // Allocate and write response metadata
    let meta_offset = region
        .alloc_heap(meta_bytes.len())
        .ok_or_else(|| SlotBusError::SharedMemory("heap full for response meta".into()))?;
    region.heap_write_checked(meta_offset, meta_bytes)?;

    unsafe {
        let slot_ptr = region
            .as_ptr()
            .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
        (slot_ptr.add(68) as *mut u32).write(meta_offset);
        (slot_ptr.add(72) as *mut u16).write(meta_bytes.len() as u16);
    }

    // Write response body (inline or overflow)
    let mut overflow_region = None;
    if body.is_empty() {
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(76) as *mut u32).write(0);
            (slot_ptr.add(80) as *mut u32).write(0);
            slot_ptr.add(84).write(0);
        }
    } else if let Some(body_offset) = region.alloc_heap(body.len()) {
        region.heap_write_checked(body_offset, body)?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(76) as *mut u32).write(body_offset);
            (slot_ptr.add(80) as *mut u32).write(body.len() as u32);
            slot_ptr.add(84).write(0);
        }
    } else {
        let name = config.response_overflow_name(slot_index);
        let ovf = ShmRegion::create_overflow(&name, body)?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(76) as *mut u32).write(0);
            (slot_ptr.add(80) as *mut u32).write(body.len() as u32);
            slot_ptr.add(84).write(1);
        }
        overflow_region = Some(ovf);
    }

    // Set status -> Done
    slot.status.store(SLOT_DONE, Ordering::Release);

    Ok(overflow_region)
}

// ---- Read helpers ------------------------------------------------------------

/// Read request metadata and body from a claimed slot.
pub fn read_request(
    region: &ShmRegion,
    slot_index: usize,
    config: &SlotBusConfig,
) -> Result<(String, u8, RequestMeta, Vec<u8>), SlotBusError> {
    let slot = unsafe { region.slot(slot_index) };

    let req_id = {
        let raw = &slot.req_id;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(36);
        String::from_utf8_lossy(&raw[..end]).to_string()
    };

    let method = slot.method;

    let meta: RequestMeta = {
        let meta_bytes = region.heap_read_checked(slot.meta_offset, slot.meta_len as usize)?;
        postcard::from_bytes(meta_bytes)?
    };

    let body_len = slot.body_len as usize;
    let body = if body_len == 0 {
        Vec::new()
    } else if slot.body_overflow == 0 {
        region
            .heap_read_checked(slot.body_offset, body_len)?
            .to_vec()
    } else {
        let name = config.request_overflow_name(slot_index);
        ShmRegion::read_overflow(&name, body_len)?
    };

    Ok((req_id, method, meta, body))
}

/// Read response status, metadata, and body from a Done slot.
pub fn read_response(
    region: &ShmRegion,
    slot_index: usize,
    config: &SlotBusConfig,
) -> Result<(u16, ResponseMeta, Vec<u8>), SlotBusError> {
    let slot = unsafe { region.slot(slot_index) };
    let status = slot.resp_status;

    let meta: ResponseMeta = {
        let meta_bytes =
            region.heap_read_checked(slot.resp_meta_offset, slot.resp_meta_len as usize)?;
        postcard::from_bytes(meta_bytes)?
    };

    let body_len = slot.resp_body_len as usize;
    let body = if body_len == 0 {
        Vec::new()
    } else if slot.resp_body_overflow == 0 {
        region
            .heap_read_checked(slot.resp_body_offset, body_len)?
            .to_vec()
    } else {
        let name = config.response_overflow_name(slot_index);
        ShmRegion::read_overflow(&name, body_len)?
    };

    Ok((status, meta, body))
}
