//! Shared test hygiene.
//!
//! `shared_memory` only unlinks a backing file from `Drop`, so any test that
//! panics — or any region deliberately leaked to simulate a killed process —
//! leaves a file behind in `%TEMP%\shared_memory-rs`. Test region names embed
//! the PID, so they never collide with a later run, which means the
//! reclaim-on-collision path never fires for them and they accumulate forever
//! on dev boxes and CI runners. 13 such files were found on one machine.
//!
//! Sweeping at start-up rather than guarding every construction site keeps this
//! robust against panicking tests, which are exactly the ones that leak.

/// Prefixes owned by the test suite. Only files starting with one of these are
/// ever removed, so a sweep can never touch a real region.
const TEST_PREFIXES: &[&str] = &[
    "stress-test-",
    "heapx-heapx-",
    "slotbus-test-",
    "slotbus-review-",
];

/// Remove test backing files left by *previous* runs.
///
/// Files belonging to the current process are left alone — they may be in use.
/// Everything else under a test-owned prefix is by definition from a process
/// that has already exited.
pub fn sweep_stale_test_backing_files() {
    let dir = std::env::temp_dir().join("shared_memory-rs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mine = format!("-{}", std::process::id());

    for entry in entries.flatten() {
        let raw = entry.file_name();
        let name = raw.to_string_lossy();

        let owned_by_tests = TEST_PREFIXES.iter().any(|p| name.starts_with(p));
        if !owned_by_tests {
            continue;
        }
        // `-<pid>` appears either at the end or before an overflow suffix.
        if name.contains(&mine) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}
