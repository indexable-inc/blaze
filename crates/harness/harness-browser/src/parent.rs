//! The host lives only as long as its daemon: a kqueue `EVFILT_PROC` / `NOTE_EXIT` on `HARNESS_DAEMON_PID`, waited on by
//! one named thread, which asks the UI thread to quit when it fires.
use crate::host::{self, Cmd};

/// Register the watch now (so a pid already gone is a startup error, not a silent host that never exits) and start the
/// thread that waits on it.
///
/// # Errors
/// kqueue or kevent refused, naming the pid.
pub fn watch(pid: i32) -> Result<(), String> {
    // SAFETY: plain syscalls on values we own.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(format!("kqueue() for watching the daemon (pid {pid}): {}", std::io::Error::last_os_error()));
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one change, no events out, no timeout (registration does not wait).
    let r = unsafe { libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: our descriptor.
        unsafe { libc::close(kq) };
        return Err(format!("watching the daemon HARNESS_DAEMON_PID={pid} (kevent EVFILT_PROC NOTE_EXIT): {e} (ESRCH: that process is gone)"));
    }
    std::thread::Builder::new()
        .name("harness-browser.parent-watch".into())
        .spawn(move || {
            // Waits for: the daemon (pid) to exit. Ends when it does, or when kevent fails (which also quits: a host
            // that cannot tell when its daemon is gone must not outlive it). EINTR retries are bounded.
            let mut out = libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() };
            let why = 'wait: {
                for _ in 0..10_000 {
                    // SAFETY: one event out into a local, no timeout.
                    let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null()) };
                    if n == 1 {
                        break 'wait format!("the daemon (pid {pid}) exited");
                    }
                    let e = std::io::Error::last_os_error();
                    if n < 0 && e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break 'wait format!("waiting on the daemon (pid {pid}) failed: kevent returned {n}: {e}");
                }
                format!("waiting on the daemon (pid {pid}) was interrupted 10000 times")
            };
            eprintln!("harness-browser: quitting: {why}");
            host::post(Cmd::Quit { why });
        })
        .map_err(|e| format!("spawning the thread that watches the daemon (pid {pid}): {e}"))?;
    Ok(())
}
