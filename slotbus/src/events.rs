//! OS-native cross-process signaling.
//!
//! Two auto-reset events per worker:
//! - Request event: hub signals after writing a Ready slot
//! - Response event: worker signals after writing a Done slot
//!
//! ## Platform support
//!
//! - **Windows**: Named Events via kernel32 (`CreateEventW`/`SetEvent`/`WaitForSingleObject`)
//! - **Linux**: POSIX named semaphores (`sem_open`/`sem_post`/`sem_timedwait`)
//! - **macOS**: POSIX named semaphores (`sem_open`/`sem_post`/`sem_trywait` with polling)

use crate::error::SlotBusError;

// ---- Windows FFI ----

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

// ---- Unix FFI (POSIX named semaphores) ----

#[cfg(unix)]
type SemPtr = *mut std::ffi::c_void;

#[cfg(unix)]
const SEM_FAILED: SemPtr = !0usize as SemPtr;

// O_CREAT differs between Linux and macOS
#[cfg(target_os = "linux")]
const O_CREAT: i32 = 0o100;
#[cfg(target_os = "macos")]
const O_CREAT: i32 = 0x0200;

#[cfg(unix)]
extern "C" {
    fn sem_open(name: *const i8, oflag: i32, mode: u32, value: u32) -> SemPtr;
    fn sem_close(sem: SemPtr) -> i32;
    fn sem_unlink(name: *const i8) -> i32;
    fn sem_post(sem: SemPtr) -> i32;
    fn sem_trywait(sem: SemPtr) -> i32;
}

// Linux has sem_timedwait for efficient timed waits
#[cfg(target_os = "linux")]
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
extern "C" {
    fn sem_timedwait(sem: SemPtr, abs_timeout: *const Timespec) -> i32;
    fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
}

#[cfg(target_os = "linux")]
const CLOCK_REALTIME: i32 = 0;

/// Auto-reset named event for cross-process signaling.
///
/// On signal, exactly one waiter is released (auto-reset behavior).
/// If no thread is waiting, the event stays signaled until the next wait.
pub struct NamedEvent {
    #[cfg(windows)]
    handle: isize,
    #[cfg(unix)]
    sem: SemPtr,
    #[cfg(unix)]
    name: std::ffi::CString,
    #[cfg(unix)]
    created: bool,
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
        #[cfg(unix)]
        {
            let cname = std::ffi::CString::new(format!("/{name}"))
                .map_err(|e| SlotBusError::Event(format!("invalid name: {e}")))?;
            // Remove stale semaphore from a previous crash (harmless if it doesn't exist)
            unsafe { sem_unlink(cname.as_ptr()); }
            let sem = unsafe { sem_open(cname.as_ptr(), O_CREAT, 0o644, 0) };
            if sem == SEM_FAILED {
                return Err(SlotBusError::Event(format!(
                    "sem_open(create) failed for '{name}'"
                )));
            }
            Ok(Self { sem, name: cname, created: true })
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
        #[cfg(unix)]
        {
            let cname = std::ffi::CString::new(format!("/{name}"))
                .map_err(|e| SlotBusError::Event(format!("invalid name: {e}")))?;
            // Open without O_CREAT — the semaphore must already exist
            let sem = unsafe { sem_open(cname.as_ptr(), 0, 0, 0) };
            if sem == SEM_FAILED {
                return Err(SlotBusError::Event(format!(
                    "sem_open(open) failed for '{name}'"
                )));
            }
            Ok(Self { sem, name: cname, created: false })
        }
    }

    /// Signal the event (wakes one waiter for auto-reset events).
    pub fn signal(&self) {
        #[cfg(windows)]
        unsafe {
            SetEvent(self.handle);
        }
        #[cfg(unix)]
        unsafe {
            sem_post(self.sem);
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
        // Linux: use sem_timedwait for kernel-level timed wait (sub-microsecond wake)
        #[cfg(target_os = "linux")]
        {
            let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
            unsafe { clock_gettime(CLOCK_REALTIME, &mut ts); }
            ts.tv_sec += (ms / 1000) as i64;
            ts.tv_nsec += ((ms % 1000) as i64) * 1_000_000;
            if ts.tv_nsec >= 1_000_000_000 {
                ts.tv_sec += 1;
                ts.tv_nsec -= 1_000_000_000;
            }
            unsafe { sem_timedwait(self.sem, &ts) == 0 }
        }
        // macOS: sem_timedwait is not available, poll with sem_trywait
        #[cfg(target_os = "macos")]
        {
            use std::time::{Duration, Instant};
            let deadline = Instant::now() + Duration::from_millis(ms as u64);
            loop {
                if unsafe { sem_trywait(self.sem) } == 0 {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

impl Drop for NamedEvent {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.handle);
        }
        #[cfg(unix)]
        {
            unsafe { sem_close(self.sem); }
            if self.created {
                unsafe { sem_unlink(self.name.as_ptr()); }
            }
        }
    }
}

// SAFETY: Named events / POSIX semaphores are thread-safe OS primitives.
// The handle / semaphore pointer can be used from any thread.
unsafe impl Send for NamedEvent {}
unsafe impl Sync for NamedEvent {}
