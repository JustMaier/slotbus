//! Shared memory layout types.
//!
//! All `#[repr(C)]` structs are cross-process safe. Atomic fields use
//! `compare_exchange` for lock-free status transitions.

use std::sync::atomic::AtomicU32;

use serde::{Deserialize, Serialize};

// ---- Constants ---------------------------------------------------------------

/// Magic number identifying a slotbus control region ("SLB1").
pub const SHM_MAGIC: u32 = 0x534C4231;

/// Current protocol version.
///
/// Bumped 1 → 2 when the inline heap changed from a globally shared bump
/// allocator to fixed per-slot arenas (see [`slot_arenas`]). The struct layout
/// is byte-identical across the two, and both record absolute heap offsets in
/// the slot, so *reads* would interoperate — which is exactly why the version
/// has to change. A v1 writer bump-allocates from the shared `alloc_head` and
/// will happily hand out an offset that lands inside a v2 writer's arena, so a
/// mixed pair corrupts each other's payloads with no error anywhere. Failing
/// loudly in [`validate_control`](crate::region::ShmRegion::validate_control)
/// is far better than that silence.
pub const SHM_VERSION: u32 = 2;

/// Size of `SlotMeta` in bytes (compile-time verified).
pub const SLOT_META_SIZE: usize = 128;

/// Size of `ShmHeader` in bytes (compile-time verified).
pub const SHM_HEADER_SIZE: usize = 64;

/// Slot status: available for a new request.
pub const SLOT_FREE: u32 = 0;

/// Slot status: hub has written a request, waiting for worker to claim.
pub const SLOT_READY: u32 = 1;

/// Slot status: worker has claimed the request, processing.
pub const SLOT_CLAIMED: u32 = 2;

/// Slot status: worker has written the response, waiting for hub to read.
pub const SLOT_DONE: u32 = 3;

/// Slot status: hub is writing request data into this slot.
///
/// Used by [`claim_free_slot`](crate::region::claim_free_slot) to atomically
/// reserve a slot before writing. Prevents two concurrent dispatchers from
/// grabbing the same slot. Transitions to `SLOT_READY` after the write completes.
pub const SLOT_WRITING: u32 = 4;

// ---- Overflow marker encoding ------------------------------------------------
//
// The `body_overflow` / `resp_body_overflow` slot bytes encode *where* a body
// lives. Historically they were a plain bool (0 = inline, 1 = overflow). The
// encoding is now a generation counter, chosen so the common case is
// byte-identical to what older peers write and read:
//
//   0        body is inline in the shared heap
//   1        body is in the overflow region at generation 0 (un-suffixed name)
//   2..=255  body is in the overflow region at generation `marker - 1`,
//            whose name carries a `-g{generation}` suffix
//
// Generations above 0 are only reached when a same-name mapping is still held
// open by someone else. A peer running an older slotbus reads any non-zero
// marker as "generation 0" and will fail to find a suffixed region — but that
// only happens in the exact situation where the older code could not have
// written the payload at all, so nothing that used to work regresses.

/// `body_overflow` marker: the body is stored inline in the shared heap.
pub const OVERFLOW_INLINE: u8 = 0;

/// Highest overflow generation that can be encoded in a marker byte.
///
/// Markers run `1..=255`, so generations run `0..=254`.
pub const MAX_OVERFLOW_GENERATION: u8 = 254;

/// Convert an overflow marker byte into its generation.
///
/// Returns `None` for [`OVERFLOW_INLINE`], which has no generation.
pub fn overflow_generation(marker: u8) -> Option<u8> {
    marker.checked_sub(1)
}

/// HTTP method encoding (single byte in slot metadata).
pub const METHOD_GET: u8 = 0;
pub const METHOD_POST: u8 = 1;
pub const METHOD_PUT: u8 = 2;
pub const METHOD_DELETE: u8 = 3;
pub const METHOD_PATCH: u8 = 4;
pub const METHOD_HEAD: u8 = 5;
pub const METHOD_OPTIONS: u8 = 6;

/// Encode an HTTP method string to a byte.
pub fn method_to_u8(method: &str) -> u8 {
    match method {
        "GET" => METHOD_GET,
        "POST" => METHOD_POST,
        "PUT" => METHOD_PUT,
        "DELETE" => METHOD_DELETE,
        "PATCH" => METHOD_PATCH,
        "HEAD" => METHOD_HEAD,
        "OPTIONS" => METHOD_OPTIONS,
        _ => METHOD_GET,
    }
}

/// Decode a method byte to an HTTP method string.
pub fn u8_to_method(m: u8) -> &'static str {
    match m {
        METHOD_GET => "GET",
        METHOD_POST => "POST",
        METHOD_PUT => "PUT",
        METHOD_DELETE => "DELETE",
        METHOD_PATCH => "PATCH",
        METHOD_HEAD => "HEAD",
        METHOD_OPTIONS => "OPTIONS",
        _ => "GET",
    }
}

/// Compute derived layout values from slot count and region size.
pub fn compute_layout(num_slots: usize, region_size: usize) -> (usize, usize) {
    let heap_offset = SHM_HEADER_SIZE + (num_slots * SLOT_META_SIZE);
    let heap_size = region_size.saturating_sub(heap_offset);
    (heap_offset, heap_size)
}

// ---- Per-slot heap arenas ----------------------------------------------------
//
// The inline heap is divided into `num_slots` equal arenas, one per slot, and
// each arena is split in half: the first half holds the request payload, the
// second the response. Placement inside a half is fixed — metadata at offset 0,
// body immediately after it, 8-byte aligned — so there is no allocator state
// anywhere and nothing to reclaim.
//
// This replaces a single bump allocator shared by every slot. That design could
// only reclaim space by resetting `alloc_head` to zero, which was safe only
// while every slot was simultaneously FREE. Under sustained overlap that
// instant may never arrive, so the head marched to the end of the heap and
// every subsequent write failed with "heap full for response meta" — an
// exhaustion that needed no leak and no bug to occur, just traffic.
//
// Each slot now writes only within bytes it owns, so one slot's traffic cannot
// consume another's space and no global quiescence is ever required. The cost
// is a smaller inline ceiling: a body that does not fit its half spills to an
// overflow region, which is ordinary supported behaviour.

/// Byte length of one slot's arena.
///
/// Returns 0 if `num_slots` is 0 or the heap is too small to divide.
pub fn arena_size(num_slots: usize, heap_size: usize) -> usize {
    if num_slots == 0 {
        return 0;
    }
    // Round down to 8 so every arena — and therefore every sub-arena base —
    // starts 8-byte aligned without per-slot fixups.
    (heap_size / num_slots) & !7
}

/// Heap-relative `(offset, len)` of the request and response halves of a
/// slot's arena, as `(request, response)`.
///
/// Splitting the arena rather than sharing it across both directions means the
/// response write can never disturb request bytes, regardless of whether the
/// reader has finished with them. That independence is what makes the placement
/// safe without any cross-process coordination.
pub fn slot_arenas(
    slot_index: usize,
    num_slots: usize,
    heap_size: usize,
) -> ((usize, usize), (usize, usize)) {
    let arena = arena_size(num_slots, heap_size);
    let base = slot_index * arena;
    let half = (arena / 2) & !7;
    ((base, half), (base + half, arena - half))
}

/// Round `n` up to the next multiple of 8.
pub fn align8(n: usize) -> usize {
    (n + 7) & !7
}

// ---- #[repr(C)] shared memory structs ----------------------------------------

/// Header at byte offset 0 of the control region (64 bytes).
#[repr(C)]
pub struct ShmHeader {
    /// Magic number: `0x534C4231` ("SLB1").
    pub magic: u32,
    /// Protocol version.
    pub version: u32,
    /// Number of slots in this region.
    pub num_slots: u32,
    /// Byte offset of the inline heap from region start.
    pub heap_offset: u32,
    /// Size of the inline heap in bytes.
    pub heap_size: u32,
    /// Bump allocator head (atomic, CAS for thread safety).
    pub alloc_head: AtomicU32,
    _reserved: [u8; 40],
}

/// Per-slot metadata (128 bytes each). `num_slots` of these follow the header.
///
/// Layout:
///   `[0..44]`   common fields (status, req_id, method)
///   `[44..64]`  request metadata + body pointers
///   `[64..88]`  response metadata + body pointers
///   `[88..128]` reserved
#[repr(C)]
pub struct SlotMeta {
    // ---- Common (44 bytes) ----
    /// Lock-free status: Free(0) → Ready(1) → Claimed(2) → Done(3) → Free(0).
    pub status: AtomicU32,
    /// Request ID (UUID string, up to 36 bytes, null-padded).
    pub req_id: [u8; 36],
    /// HTTP method as a single byte (see `method_to_u8`).
    pub method: u8,
    _pad0: [u8; 3],

    // ---- Request (20 bytes) ----
    /// Heap offset of serialized `RequestMeta`.
    pub meta_offset: u32,
    /// Length of serialized `RequestMeta`.
    pub meta_len: u16,
    _pad1: u16,
    /// Heap offset of request body (0 when using overflow).
    pub body_offset: u32,
    /// Length of request body.
    pub body_len: u32,
    /// Overflow marker: 0 = inline in heap, `n` = overflow generation `n - 1`.
    /// See [`OVERFLOW_INLINE`] for the full encoding.
    pub body_overflow: u8,
    _pad2: [u8; 3],

    // ---- Response (24 bytes) ----
    /// HTTP status code of the response.
    pub resp_status: u16,
    _pad3: u16,
    /// Heap offset of serialized `ResponseMeta`.
    pub resp_meta_offset: u32,
    /// Length of serialized `ResponseMeta`.
    pub resp_meta_len: u16,
    _pad4: u16,
    /// Heap offset of response body (0 when using overflow).
    pub resp_body_offset: u32,
    /// Length of response body.
    pub resp_body_len: u32,
    /// Overflow marker: 0 = inline in heap, `n` = overflow generation `n - 1`.
    /// See [`OVERFLOW_INLINE`] for the full encoding.
    pub resp_body_overflow: u8,
    _pad5: [u8; 3],

    // ---- Reserved (40 bytes) ----
    _reserved: [u8; 40],
}

// Compile-time size assertions.
const _: () = assert!(std::mem::size_of::<ShmHeader>() == SHM_HEADER_SIZE);
const _: () = assert!(std::mem::size_of::<SlotMeta>() == SLOT_META_SIZE);

// ---- Postcard-serialized metadata structs ------------------------------------

/// Serialized into the inline heap alongside each request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMeta {
    /// The request path (e.g. `/session/abc123`).
    pub path: String,
    /// The matched route pattern (e.g. `/session/:id`).
    pub route_pattern: String,
    /// Extracted path parameters.
    pub path_params: Vec<(String, String)>,
    /// Raw query string (without the leading `?`).
    pub query: Option<String>,
    /// Request headers as key-value pairs.
    pub headers: Vec<(String, String)>,
}

/// Serialized into the inline heap alongside each response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    /// Content-Type of the response body.
    pub content_type: String,
    /// Response headers as key-value pairs.
    pub headers: Vec<(String, String)>,
}
