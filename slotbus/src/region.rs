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
        // Exclusive create first, so an orphaned backing file left by a killed
        // process is reclaimed rather than adopted. Adoption is the dangerous
        // case: the surviving mapping keeps the size it was *originally*
        // created with, so a caller that asked for a larger region silently
        // gets a smaller one. See `init_control`, which refuses to lay a
        // requested layout into a mapping too small to hold it.
        if let Some(region) = Self::create_exclusive(name, size)? {
            return Ok(region);
        }
        Self::open(name)
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

    /// Heap-relative `(offset, len)` of this slot's request and response
    /// sub-arenas, as `(request, response)`.
    ///
    /// Every payload for a slot is written inside the bytes this returns, so
    /// slots never contend for space. See [`crate::types::slot_arenas`].
    pub fn slot_arenas(&self, slot_index: usize) -> ((usize, usize), (usize, usize)) {
        crate::types::slot_arenas(slot_index, self.num_slots, self.heap_size)
    }

    /// Largest body that fits inline alongside a `meta_len`-byte metadata
    /// blob, for either direction of a slot. Bodies above this spill to an
    /// overflow region.
    pub fn inline_body_capacity(&self, meta_len: usize) -> usize {
        let ((_, req), (_, resp)) = self.slot_arenas(0);
        req.min(resp).saturating_sub(align8(meta_len))
    }

    /// Allocate `size` bytes from the inline heap (bump allocator).
    ///
    /// Returns the heap offset, or `None` if the heap is full.
    /// Thread-safe via CAS on `alloc_head`.
    #[deprecated(
        since = "0.1.4",
        note = "the protocol no longer uses a shared bump allocator; payloads are placed in \
                per-slot arenas (see ShmRegion::slot_arenas). Offsets returned here overlap \
                those arenas and will corrupt live slots if written to."
    )]
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
    #[deprecated(
        since = "0.1.4",
        note = "per-slot arenas need no reclamation; this only moves the now-unused alloc_head"
    )]
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
    ///
    /// This was the only path that reclaimed inline-heap space, and it fired
    /// only on global quiescence — every slot FREE at the same instant. Under
    /// sustained overlap that instant may never arrive, which is how the heap
    /// used to exhaust with no leak involved. Per-slot arenas removed the need
    /// for it entirely; the protocol no longer calls it.
    #[deprecated(
        since = "0.1.4",
        note = "per-slot arenas need no reclamation; this is now a no-op on live traffic"
    )]
    pub fn try_reset_heap(&self) {
        if !self.has_inflight_slots() {
            #[allow(deprecated)]
            self.reset_heap();
        }
    }

    // ---- Control region: initialization --------------------------------------

    /// Initialize a freshly-created control region with the given config.
    pub fn init_control(&mut self, config: &SlotBusConfig) -> Result<(), SlotBusError> {
        let (heap_offset, heap_size) = compute_layout(config.num_slots, config.region_size);

        // The layout is derived from the REQUESTED region_size, but `self.len`
        // is what we actually mapped, and those differ whenever an existing
        // smaller mapping was adopted (a stale region from before a size
        // increase, say). Writing the requested layout into the header anyway
        // would advertise a heap larger than the mapping — and every bounds
        // check validates against the header, so they would wave through writes
        // running off the end of the mapping. Fail loudly instead.
        let needed = heap_offset + heap_size;
        if needed > self.len {
            return Err(SlotBusError::InvalidRegion(format!(
                "region '{}' maps {} bytes but the requested layout needs {} \
                 (num_slots={}, region_size={}); an existing smaller mapping was adopted — \
                 stop every peer and remove the stale region before retrying",
                self.name, self.len, needed, config.num_slots, config.region_size
            )));
        }

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
        Ok(())
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
            // The header describes the layout, and every bounds check validates
            // against the header — so a header describing more space than the
            // mapping actually has turns those checks into rubber stamps for
            // out-of-bounds accesses. Refuse such a region outright.
            if heap_offset + heap_size > self.len {
                return Err(SlotBusError::InvalidRegion(format!(
                    "region '{}' maps {} bytes but its header describes a layout ending at {} \
                     (heap_offset={heap_offset}, heap_size={heap_size}); \
                     the region was created against a larger region_size",
                    self.name,
                    self.len,
                    heap_offset + heap_size
                )));
            }

            self.num_slots = num_slots;
            self.heap_offset = heap_offset;
            self.heap_size = heap_size;
        }
        Ok(())
    }

    // ---- Overflow helpers (static) -------------------------------------------

    /// Round a payload length up to the mapping granularity (4KiB pages).
    fn overflow_size_for(len: usize) -> usize {
        ((len + 4095) & !4095).max(4096)
    }

    /// Try to reclaim an *orphaned* backing file for `name`, returning true if
    /// one was removed.
    ///
    /// On Windows `shared_memory` backs every mapping with a real file under
    /// `%TEMP%\shared_memory-rs\`, and reports `MappingIdExists` when that file
    /// is present — whether or not anyone still has it mapped. Its cleanup runs
    /// from `Drop`, so a hard process kill (SIGKILL, `TerminateProcess`, a
    /// crash) leaves the file behind forever. The next run then adopts a stale
    /// mapping frozen at its original size.
    ///
    /// Opening the file with no sharing tells the two cases apart. Every live
    /// user — creator or opener — keeps the file handle open, so an exclusive
    /// open fails with a sharing violation while anyone is still using it. It
    /// succeeds only when the file is genuinely orphaned, and only then do we
    /// delete it.
    ///
    /// Best-effort by design: any failure returns false, leaving the caller on
    /// its existing fallback path.
    #[cfg(windows)]
    fn backing_file_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("shared_memory-rs")
    }

    /// Resolve `name` to the backing file it would occupy, or `None` if the
    /// name could not name a file *directly inside* the backing directory.
    ///
    /// This function guards a delete, and region names are attacker-reachable:
    /// a hub takes the worker's requested name straight from its registration
    /// request. `Path::join` is not safe on untrusted input — `..` walks up out
    /// of the directory, and an absolute path replaces the base outright.
    ///
    /// A backing file always sits directly in the backing directory, so a
    /// legitimate name is exactly **one normal path component**. Anything with
    /// separators, a `..`, a root, or a drive prefix is rejected rather than
    /// normalised, because there is no legitimate name that needs them.
    #[cfg(windows)]
    fn backing_file_path(name: &str) -> Option<std::path::PathBuf> {
        use std::path::{Component, Path};

        let trimmed = name.trim_start_matches('/');

        let mut components = Path::new(trimmed).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Some(Self::backing_file_dir().join(trimmed)),
            _ => None,
        }
    }

    #[cfg(windows)]
    fn reclaim_orphaned_backing_file(name: &str) -> bool {
        use std::os::windows::fs::OpenOptionsExt;

        /// `DELETE` access right.
        const DELETE: u32 = 0x0001_0000;
        /// Unlink the file when the last handle to it closes.
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        /// Open a reparse point itself instead of following it.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

        let Some(path) = Self::backing_file_path(name) else {
            tracing::warn!(
                region = name,
                "refusing to reclaim: region name is not a single path component"
            );
            return false;
        };

        // Resolve links before trusting the location, then require the resolved
        // parent to be exactly the backing directory. Canonicalising the file
        // (rather than checking the name alone) also collapses any junction or
        // symlink planted inside the directory to its real target, which would
        // then fail this check.
        let (Ok(resolved), Ok(dir)) =
            (path.canonicalize(), Self::backing_file_dir().canonicalize())
        else {
            // A missing file is the common case: nothing to reclaim.
            return false;
        };
        if resolved.parent() != Some(dir.as_path()) {
            tracing::warn!(
                region = name,
                path = %resolved.display(),
                "refusing to reclaim a backing file outside the backing directory"
            );
            return false;
        }

        // share_mode(0) => fail if ANY other handle is open on this file. Every
        // live user, creator or opener, holds its backing-file handle for the
        // whole lifetime of the mapping, so this succeeds only when the file is
        // genuinely orphaned.
        //
        // DELETE_ON_CLOSE makes the unlink atomic with that exclusive open.
        // Deleting via a separate `remove_file` would run *after* the handle
        // dropped, so another process could reclaim the same name and create a
        // fresh file in between — which we would then delete out from under it,
        // leaving it with a live mapping no peer could ever open by name.
        match std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&resolved)
        {
            Ok(file) => {
                drop(file);
                tracing::warn!(
                    region = name,
                    path = %resolved.display(),
                    "removed orphaned shared-memory backing file left by a killed process"
                );
                true
            }
            Err(_) => false,
        }
    }

    /// Always `false` on Unix: orphaned POSIX shared memory is **not** reclaimed.
    ///
    /// Known limitation, deliberately not papered over. The Windows reclaim
    /// works because an exclusive `CreateFileW` distinguishes "in use" from
    /// "orphaned" atomically, as a property the OS enforces. POSIX
    /// `shm_open` has no equivalent: there are no share modes, slotbus takes no
    /// advisory lock a live peer would hold, and a segment carries nothing that
    /// proves its owner is alive. Inferring liveness — scanning `/proc/*/fd`,
    /// or trusting a PID written into the header — is racy and Linux-only, and
    /// guessing wrong here **deletes a live region**. A wrong reclaim is far
    /// worse than no reclaim, so Unix keeps the leak.
    ///
    /// Consequence: on Linux and macOS a hard-killed process leaves its segment
    /// in `/dev/shm` (or the macOS equivalent) forever, and each orphan
    /// permanently burns one overflow generation. Callers relying on generation
    /// stamping will walk further on each restart. Clean up out of band, e.g.
    /// `rm /dev/shm/<prefix>-*` on boot.
    #[cfg(not(windows))]
    fn reclaim_orphaned_backing_file(_name: &str) -> bool {
        false
    }

    /// Create a named region, returning `Ok(None)` if the name is already taken.
    ///
    /// Unlike [`create_or_open`](Self::create_or_open) this never adopts a
    /// mapping created by someone else, so the caller can be certain the
    /// returned region has the size it asked for.
    ///
    /// If the name is taken only by an orphaned backing file (see
    /// [`reclaim_orphaned_backing_file`](Self::reclaim_orphaned_backing_file)),
    /// the file is removed and the create is retried once. Without that, every
    /// hard kill permanently burns a name: callers using generation stamping
    /// would walk one generation further on each restart until all 255 are
    /// exhausted.
    fn create_exclusive(name: &str, size: usize) -> Result<Option<Self>, SlotBusError> {
        if let Some(region) = Self::try_create_exclusive(name, size)? {
            return Ok(Some(region));
        }
        if Self::reclaim_orphaned_backing_file(name) {
            return Self::try_create_exclusive(name, size);
        }
        Ok(None)
    }

    /// One `create` attempt. `Ok(None)` means the name is already taken.
    fn try_create_exclusive(name: &str, size: usize) -> Result<Option<Self>, SlotBusError> {
        match ShmemConf::new().os_id(name).size(size).create() {
            Ok(shmem) => {
                let ptr = shmem.as_ptr();
                let len = shmem.len();
                Ok(Some(Self {
                    _shmem: shmem,
                    ptr,
                    len,
                    name: name.to_string(),
                    num_slots: 0,
                    heap_offset: 0,
                    heap_size: 0,
                }))
            }
            Err(ShmemError::MappingIdExists) => Ok(None),
            Err(e) => Err(SlotBusError::SharedMemory(format!("create '{name}': {e}"))),
        }
    }

    /// Create an overflow region and write data into it.
    ///
    /// Fails rather than adopting an existing same-name mapping: such a
    /// mapping keeps its original size, so writing a larger payload into it
    /// would copy past the end — a wild write (0xC0000005). Callers that can
    /// tolerate a different name should prefer
    /// [`create_overflow_fresh`](Self::create_overflow_fresh), which retries
    /// under a new generation instead of failing.
    pub fn create_overflow(name: &str, data: &[u8]) -> Result<Self, SlotBusError> {
        let size = Self::overflow_size_for(data.len());
        let Some(region) = Self::create_exclusive(name, size)? else {
            return Err(SlotBusError::SharedMemory(format!(
                "overflow region '{name}' already exists: need {} bytes \
                 (held by a live peer, or an orphaned backing file that could not be reclaimed)",
                data.len()
            )));
        };
        Self::fill_overflow(region, name, data)
    }

    /// Create a *fresh* overflow region, advancing the generation until a name
    /// is found that no one else holds open.
    ///
    /// `name_for(generation)` supplies the candidate name for each attempt;
    /// generation 0 is the historical un-suffixed name, so the uncontended
    /// path is unchanged. Returns the region plus the **overflow marker** to
    /// store in the slot: `generation + 1`, matching the encoding documented
    /// on [`OVERFLOW_INLINE`].
    ///
    /// This is what makes a leaked handle survivable. A stale mapping — held
    /// by, say, a `SlotWorker` from a previous connection that never dropped —
    /// used to poison its slot permanently, because every later payload larger
    /// than the stale mapping tried to reuse that exact name and was refused.
    /// Advancing the generation sidesteps the corpse instead of dying on it.
    pub fn create_overflow_fresh<F>(name_for: F, data: &[u8]) -> Result<(Self, u8), SlotBusError>
    where
        F: Fn(u8) -> String,
    {
        let size = Self::overflow_size_for(data.len());
        let mut last_name = String::new();

        for generation in 0..=MAX_OVERFLOW_GENERATION {
            let name = name_for(generation);
            if let Some(region) = Self::create_exclusive(&name, size)? {
                let region = Self::fill_overflow(region, &name, data)?;
                return Ok((region, generation + 1));
            }
            last_name = name;
        }

        Err(SlotBusError::SharedMemory(format!(
            "exhausted all {} overflow generations for '{last_name}': need {} bytes \
             (every generation is held by a live peer or an unreclaimable orphan)",
            MAX_OVERFLOW_GENERATION as u16 + 1,
            data.len()
        )))
    }

    /// Copy `data` into a freshly created overflow region, bounds-checked.
    ///
    /// The region was created at a size rounded up from `data.len()`, so this
    /// should never fail; the check is here so a surprising allocator result
    /// surfaces as an error rather than a wild write.
    fn fill_overflow(region: Self, name: &str, data: &[u8]) -> Result<Self, SlotBusError> {
        if region.len < data.len() {
            return Err(SlotBusError::SharedMemory(format!(
                "overflow region '{name}' too small for write: need {}, have {}",
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

/// Where a body goes inside a sub-arena that already holds `meta_len` bytes of
/// metadata, or `None` if it does not fit and must spill to overflow.
///
/// The body sits immediately after the metadata, 8-byte aligned. Both are at
/// fixed positions within bytes the slot owns outright, so this is placement
/// rather than allocation — there is no head to advance and nothing to free.
fn arena_body_offset(base: usize, size: usize, meta_len: usize, body_len: usize) -> Option<u32> {
    let start = base + align8(meta_len);
    let end = start.checked_add(body_len)?;
    (end <= base + size).then_some(start as u32)
}

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

    // Place metadata at the start of this slot's request arena. Fixed
    // placement, not allocation: the bytes belong to this slot alone, so
    // rewriting them every cycle is correct and nothing accumulates.
    let ((req_base, req_size), _) = region.slot_arenas(slot_index);
    if meta_bytes.len() > req_size {
        return Err(SlotBusError::SharedMemory(format!(
            "heap full for request meta: need {}, slot arena holds {req_size}",
            meta_bytes.len()
        )));
    }
    let meta_offset = req_base as u32;
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
    } else if let Some(body_offset) =
        arena_body_offset(req_base, req_size, meta_bytes.len(), body.len())
    {
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
        let (ovf, marker) = ShmRegion::create_overflow_fresh(
            |generation| config.request_overflow_name_gen(slot_index, generation),
            body,
        )?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(52) as *mut u32).write(0);
            (slot_ptr.add(56) as *mut u32).write(body.len() as u32);
            slot_ptr.add(60).write(marker);
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

    // Place response metadata at the start of this slot's response arena. The
    // response half is disjoint from the request half, so this cannot disturb
    // request bytes even if a reader is still holding them.
    let (_, (resp_base, resp_size)) = region.slot_arenas(slot_index);
    if meta_bytes.len() > resp_size {
        return Err(SlotBusError::SharedMemory(format!(
            "heap full for response meta: need {}, slot arena holds {resp_size}",
            meta_bytes.len()
        )));
    }
    let meta_offset = resp_base as u32;
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
    } else if let Some(body_offset) =
        arena_body_offset(resp_base, resp_size, meta_bytes.len(), body.len())
    {
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
        let (ovf, marker) = ShmRegion::create_overflow_fresh(
            |generation| config.response_overflow_name_gen(slot_index, generation),
            body,
        )?;
        unsafe {
            let slot_ptr = region
                .as_ptr()
                .add(SHM_HEADER_SIZE + slot_index * SLOT_META_SIZE);
            (slot_ptr.add(76) as *mut u32).write(0);
            (slot_ptr.add(80) as *mut u32).write(body.len() as u32);
            slot_ptr.add(84).write(marker);
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
    let body = match overflow_generation(slot.body_overflow) {
        _ if body_len == 0 => Vec::new(),
        None => region
            .heap_read_checked(slot.body_offset, body_len)?
            .to_vec(),
        Some(generation) => {
            let name = config.request_overflow_name_gen(slot_index, generation);
            ShmRegion::read_overflow(&name, body_len)?
        }
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
    let body = match overflow_generation(slot.resp_body_overflow) {
        _ if body_len == 0 => Vec::new(),
        None => region
            .heap_read_checked(slot.resp_body_offset, body_len)?
            .to_vec(),
        Some(generation) => {
            let name = config.response_overflow_name_gen(slot_index, generation);
            ShmRegion::read_overflow(&name, body_len)?
        }
    };

    Ok((status, meta, body))
}
