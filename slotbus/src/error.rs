/// Errors that can occur during slotbus operations.
#[derive(Debug, thiserror::Error)]
pub enum SlotBusError {
    /// Failed to create or open a shared memory region.
    #[error("shared memory error: {0}")]
    SharedMemory(String),

    /// Failed to create or open an OS event.
    #[error("event error: {0}")]
    Event(String),

    /// The control region has an invalid magic number or version.
    #[error("invalid control region: {0}")]
    InvalidRegion(String),

    /// No free slots available for a new request.
    #[error("no free slots available (all {0} slots in use)")]
    NoFreeSlots(usize),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A request timed out waiting for a response.
    #[error("request timed out after {0}ms")]
    Timeout(u32),

    /// The slot state machine encountered an unexpected transition.
    #[error("unexpected slot state: expected {expected}, found {found}")]
    BadSlotState { expected: u32, found: u32 },

    /// A CAS (compare-and-swap) operation on a slot failed.
    #[error("CAS failed on slot {slot}: expected {expected}, found {found}")]
    CasFailed {
        slot: usize,
        expected: u32,
        found: u32,
    },

    /// The worker receive loop has stopped.
    #[error("receive loop stopped")]
    ReceiveLoopStopped,
}

impl From<postcard::Error> for SlotBusError {
    fn from(e: postcard::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}
