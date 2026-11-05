//! The signals the sandbox's own process passes on to the program's session. Unsafe because
//! a handler is installed through `sigaction`, which std doesn't wrap; the handler itself does
//! only what is async-signal-safe (an atomic load and `kill`).

use std::sync::atomic::{AtomicI32, Ordering};

/// The process group the signals the sandbox's own process is sent are passed on to: the
/// program's session, which the terminal no longer sends them to. 0 for none.
static GROUP: AtomicI32 = AtomicI32::new(0);

extern "C" fn pass_on(signal: libc::c_int) {
    let group = GROUP.load(Ordering::Relaxed);
    if group > 0 {
        // SAFETY: `kill` takes integers only, reads no memory of this process and is
        // async-signal-safe, so this handler may call it whatever the thread it interrupts
        // holds; `group` is the program's group `pass_signals_to` stored, and 0 sends nothing.
        unsafe { libc::kill(-group, signal) };
    }
}

/// Has the signals a terminal or the caller ends a run with (`SIGINT`, `SIGTERM`, `SIGHUP`,
/// `SIGQUIT`) passed on to the process group `group`, the program's session, rather than end
/// the sandbox's own process, which then ends as the program does; 0 once the group is gone.
/// The handlers are made the first time: a process that forks after keeps them until it runs
/// a program, and passes nothing on.
pub fn pass_signals_to(group: libc::pid_t) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    GROUP.store(group, Ordering::Relaxed);
    INSTALLED.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: `sigaction` is integers, a mask of integers and a handler address, for
            // which all-zero bytes are a valid value (no handler, no flags, an empty mask).
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = pass_on as extern "C" fn(libc::c_int) as usize;
            // SAFETY: `action.sa_mask` is a live `sigset_t` of this frame for the call to fill.
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            // SAFETY: `action` is live and whole for the call, and a null old action is
            // allowed. Its handler `pass_on` only loads an atomic and calls `kill`, both
            // async-signal-safe; a handler that allocated or locked could deadlock the
            // process it interrupts.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
    });
}
