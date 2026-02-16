/// macOS limits POSIX named semaphore and shared memory names to 31 characters
/// (including the leading `/` that the OS prepends). When names exceed this,
/// we hash them to a fixed-length string.
#[cfg(target_os = "macos")]
fn sanitize_os_name(name: &str) -> String {
    // With leading `/`, the total must be <= 31 chars, so name itself <= 30 chars.
    // shm_open on macOS also has this same limit (PSHMNAMLEN = 31).
    if name.len() <= 30 {
        return name.to_string();
    }
    // FNV-1a 64-bit hash, encoded as 11 base62 chars
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut encoded = String::with_capacity(11);
    let mut h = hash;
    for _ in 0..11 {
        encoded.push(CHARS[(h % 62) as usize] as char);
        h /= 62;
    }
    // "sb-" (3) + 11 hash chars = 14 chars, leaving 16 for suffix
    let suffix_len = 16.min(name.len());
    let suffix = &name[name.len() - suffix_len..];
    let result = format!("sb-{encoded}{suffix}");
    if result.len() > 30 {
        format!("sb-{encoded}")
    } else {
        result
    }
}

#[cfg(not(target_os = "macos"))]
fn sanitize_os_name(name: &str) -> String {
    name.to_string()
}

/// Configuration for a slotbus shared memory region.
///
/// Use the builder pattern to customize behavior:
///
/// ```rust
/// use slotbus::SlotBusConfig;
///
/// let config = SlotBusConfig::builder()
///     .name("my-worker")
///     .num_slots(16)
///     .region_size(512 * 1024) // 512KB
///     .wait_timeout_ms(10_000)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct SlotBusConfig {
    /// Name of the shared memory region. Used as the OS identifier.
    /// The control region will be named `slotbus-{name}`.
    pub name: String,

    /// Prefix for shared memory region names. Default: `"slotbus"`.
    pub prefix: String,

    /// Number of request/response slots. Default: 32.
    /// Must be between 1 and 256.
    pub num_slots: usize,

    /// Total size of the control region in bytes. Default: 1MB (1_048_576).
    /// Must be large enough to hold the header + slots + some heap space.
    pub region_size: usize,

    /// Timeout in milliseconds for event waits. Default: 5000.
    /// This is a safety fallback — events provide sub-microsecond wakeup.
    /// The timeout only fires if an event signal is missed.
    pub wait_timeout_ms: u32,

    /// Whether to enable latency instrumentation logging. Default: false.
    pub instrumentation: bool,
}

impl Default for SlotBusConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            prefix: "slotbus".to_string(),
            num_slots: 32,
            region_size: 1_048_576, // 1MB
            wait_timeout_ms: 5_000,
            instrumentation: false,
        }
    }
}

impl SlotBusConfig {
    /// Create a new config builder.
    pub fn builder() -> SlotBusConfigBuilder {
        SlotBusConfigBuilder::default()
    }

    /// The full OS-level name for the control region.
    pub fn region_name(&self) -> String {
        sanitize_os_name(&format!("{}-{}", self.prefix, self.name))
    }

    /// The OS-level name for the request event.
    pub fn request_event_name(&self) -> String {
        sanitize_os_name(&format!("{}-{}-req", self.prefix, self.name))
    }

    /// The OS-level name for the response event.
    pub fn response_event_name(&self) -> String {
        sanitize_os_name(&format!("{}-{}-rsp", self.prefix, self.name))
    }

    /// The OS-level name for a request overflow region at a given slot.
    pub fn request_overflow_name(&self, slot: usize) -> String {
        sanitize_os_name(&format!("{}-{}-req-{}", self.prefix, self.name, slot))
    }

    /// The OS-level name for a response overflow region at a given slot.
    pub fn response_overflow_name(&self, slot: usize) -> String {
        sanitize_os_name(&format!("{}-{}-rsp-{}", self.prefix, self.name, slot))
    }
}

/// Builder for [`SlotBusConfig`].
#[derive(Debug, Default)]
pub struct SlotBusConfigBuilder {
    config: SlotBusConfig,
}

impl SlotBusConfigBuilder {
    /// Set the worker name. **Required.**
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Set the region name prefix. Default: `"slotbus"`.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.config.prefix = prefix.into();
        self
    }

    /// Set the number of slots. Default: 32. Range: 1..=256.
    pub fn num_slots(mut self, n: usize) -> Self {
        self.config.num_slots = n.clamp(1, 256);
        self
    }

    /// Set the control region size in bytes. Default: 1MB.
    pub fn region_size(mut self, size: usize) -> Self {
        self.config.region_size = size;
        self
    }

    /// Set the event wait timeout in milliseconds. Default: 5000.
    pub fn wait_timeout_ms(mut self, ms: u32) -> Self {
        self.config.wait_timeout_ms = ms;
        self
    }

    /// Enable or disable latency instrumentation. Default: false.
    pub fn instrumentation(mut self, enabled: bool) -> Self {
        self.config.instrumentation = enabled;
        self
    }

    /// Build the config. Panics if `name` is empty.
    pub fn build(self) -> SlotBusConfig {
        assert!(!self.config.name.is_empty(), "slotbus: name is required");
        self.config
    }
}
