//! **The** unsafe code. Every `unsafe` block in Oxru is in this file, behind a
//! safe function; the rest of the crate is `deny(unsafe_code)`, so a new one
//! can't appear anywhere else without failing the build.
//!
//! All three are calls into libc for things the standard library doesn't
//! expose: asking whether a process is alive, asking what a process is called,
//! and detaching a child from the controlling terminal. There's no safe-Rust
//! way to do any of them — the crates that offer these (`nix`, `sysinfo`,
//! `libproc`) wrap the same C calls in the same `unsafe`, so depending on one
//! would move this code out of sight rather than remove it. Keeping it here,
//! three calls in one small file, means the whole unsafe surface of the app can
//! be read in a minute.
//!
//! What each wrapper has to guarantee is written above it. They take plain
//! integers and return owned values, so no pointer or lifetime obligation
//! escapes this module.

// The one exemption from the crate-wide `deny(unsafe_code)` in `main.rs`.
#![allow(unsafe_code)]

/// Whether process `pid` is still running.
///
/// Signal 0 asks the kernel to do the permission and existence checks without
/// delivering anything, which is the standard liveness probe. Used to tell a
/// live Oxru instance from a stale marker file left by one that was killed.
///
/// Safety: `kill` takes two integers and returns one. Non-positive pids are
/// rejected first because they have special meanings — 0 is "every process in
/// my group" and negative values are whole process groups, so passing one
/// through would probe something we never meant to ask about.
#[cfg(unix)]
pub fn process_alive(pid: i32) -> bool {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    if pid <= 0 {
        return false;
    }
    #[allow(unsafe_code)]
    let rc = unsafe { kill(pid, 0) };
    rc == 0
}

#[cfg(not(unix))]
pub fn process_alive(_pid: i32) -> bool {
    false
}

/// The short name of process `pid` — `node`, `cargo`, `vim` — used to label a
/// terminal tab by whatever it's running.
///
/// Safety: `proc_name` writes at most `buffersize` bytes into the buffer and
/// returns how many it wrote. The buffer is a live local array and its true
/// length is what's passed, so the call can't write past it. The return value
/// is clamped to the buffer length before it's used as a slice bound, in case
/// a future libc returns something larger, and the bytes are read with
/// `from_utf8_lossy` since a process name is not guaranteed to be UTF-8.
#[cfg(target_os = "macos")]
pub fn process_name(pid: i32) -> Option<String> {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn proc_name(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
    }
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    #[allow(unsafe_code)]
    let n = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let len = (n as usize).min(buf.len());
    Some(String::from_utf8_lossy(&buf[..len]).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn process_name(_pid: i32) -> Option<String> {
    None
}

/// Arrange for a child process to be put in its own session before it execs.
///
/// A detached GUI window must not die when the terminal that launched it
/// closes. `setsid` gives the child a new session with no controlling
/// terminal, so the SIGHUP that terminal sends on exit never reaches it.
///
/// Safety: `pre_exec` runs in the child after `fork` and before `exec`, a
/// window in which only async-signal-safe calls are allowed — the child shares
/// the parent's address space and any lock held by another thread at fork time
/// stays locked forever. `setsid` is a bare syscall and allocates nothing, so
/// it's permitted here; the closure does nothing else, which is what keeps
/// this sound.
#[cfg(all(unix, feature = "gui"))]
pub fn detach_from_terminal(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn setsid() -> i32;
    }

    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            // The return value is deliberately ignored: setsid fails only when
            // the caller is already a process-group leader, which means we
            // already have what we wanted.
            setsid();
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own process is alive; the pids that mean something else are not.
    #[test]
    fn liveness_rejects_the_special_pids() {
        assert!(!process_alive(0), "0 means \"my whole process group\"");
        assert!(!process_alive(-1), "negative pids are process groups");
        #[cfg(unix)]
        {
            let me = std::process::id() as i32;
            assert!(process_alive(me), "this process is running");
        }
    }

    /// A pid that can't exist gives no name rather than reading stale memory.
    #[test]
    fn naming_a_bogus_pid_yields_nothing() {
        assert_eq!(process_name(0), None);
        assert_eq!(process_name(-5), None);
    }

    /// Whatever this process is called, it's non-empty and printable — proof
    /// the length clamp and the lossy decode produce a sane string.
    #[test]
    #[cfg(target_os = "macos")]
    fn naming_this_process_returns_something_usable() {
        let me = std::process::id() as i32;
        let Some(name) = process_name(me) else {
            return; // sandboxed builds may refuse; not a failure of the wrapper
        };
        assert!(!name.is_empty());
        assert!(name.chars().all(|c| !c.is_control()), "got control chars: {name:?}");
    }
}
