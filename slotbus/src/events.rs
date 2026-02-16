//! OS-native cross-process signaling.
//!
//! Two auto-reset events per worker:
//! - Request event: hub signals after writing a Ready slot
//! - Response event: worker signals after writing a Done slot
//!
//! ## Platform support
//!
//! - **Windows**: Named Events via kernel32 (`CreateEventW`/`SetEvent`/`WaitForSingleObject`)
//! - **Linux**: Not yet implemented (planned: `eventfd`)
//! - **macOS**: Not yet implemented (planned: named semaphores)

use crate::error::SlotBusError;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(
        lpEventAttributes: *const std::ffi::c_void,
        bManualReset: i32,
        bInitialState: i32,
        lpName: *const u16,
    ) -> isize;

    fn OpenEventW(dwDesiredAccess: u32, bInheritHandle: i32, lpName: *const u16) -> isize;

    fn SetEvent(hEvent: isize) -> i32;

    fn WaitForSingleObject(hHandle: isize, dwMilliseconds: u32) -> u32;

    fn CloseHandle(hObject: isize) -> i32;
}

#[cfg(windows)]
const EVENT_MODIFY_STATE: u32 = 0x0002;
#[cfg(windows)]
const SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(windows)]
const WAIT_OBJECT_0: u32 = 0;

/// Auto-reset named event for cross-process signaling.
///
/// On signal, exactly one waiter is released (auto-reset behavior).
/// If no thread is waiting, the event stays signaled until the next wait.
pub struct NamedEvent {
    #[cfg(windows)]
    handle: isize,
    #[cfg(not(windows))]
    _phantom: (),
}

impl NamedEvent {
    /// Create a new auto-reset named event.
    pub fn create(name: &str) -> Result<Self, SlotBusError> {
        #[cfg(windows)]
        {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = unsafe {
                CreateEventW(
                    std::ptr::null(),
                    0, // auto-reset
                    0, // initially non-signaled
                    wide.as_ptr(),
                )
            };
            if handle == 0 {
                return Err(SlotBusError::Event(format!(
                    "CreateEventW failed for '{name}'"
                )));
            }
            Ok(Self { handle })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Err(SlotBusError::Event(
                "named events only supported on Windows (Linux/macOS planned)".into(),
            ))
        }
    }

    /// Open an existing named event.
    pub fn open(name: &str) -> Result<Self, SlotBusError> {
        #[cfg(windows)]
        {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let handle =
                unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZE, 0, wide.as_ptr()) };
            if handle == 0 {
                return Err(SlotBusError::Event(format!(
                    "OpenEventW failed for '{name}'"
                )));
            }
            Ok(Self { handle })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Err(SlotBusError::Event(
                "named events only supported on Windows (Linux/macOS planned)".into(),
            ))
        }
    }

    /// Signal the event (wakes one waiter for auto-reset events).
    pub fn signal(&self) {
        #[cfg(windows)]
        unsafe {
            SetEvent(self.handle);
        }
    }

    /// Get the raw OS event handle (Windows: `HANDLE` as `isize`).
    ///
    /// Useful for integrating with foreign event loops (e.g. libuv's `uv_poll_init`)
    /// or FFI wrappers that need to expose the handle to other languages.
    #[cfg(windows)]
    pub fn raw_handle(&self) -> isize {
        self.handle
    }

    /// Wait for the event with a timeout in milliseconds.
    /// Returns `true` if the event was signaled, `false` on timeout.
    pub fn wait_timeout(&self, ms: u32) -> bool {
        #[cfg(windows)]
        {
            let result = unsafe { WaitForSingleObject(self.handle, ms) };
            result == WAIT_OBJECT_0
        }
        #[cfg(not(windows))]
        {
            let _ = ms;
            false
        }
    }
}

impl Drop for NamedEvent {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

// SAFETY: Named events are thread-safe OS primitives. The handle can be
// used from any thread (SetEvent/WaitForSingleObject are thread-safe).
unsafe impl Send for NamedEvent {}
unsafe impl Sync for NamedEvent {}
