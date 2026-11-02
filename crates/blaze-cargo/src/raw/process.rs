//! Processes: whether one runs, how a child ended, ending a worker's process group, the
//! action of `SIGPIPE`, the CPU time used, and on Windows the handles a child inherits. std
//! has none of these: each is the one system call that does it.

#[cfg(unix)]
use std::time::Duration;

/// Whether the process `pid` runs.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: `kill` takes no pointer, so no memory of this process is at stake whatever `pid`
    // is, and a signal of 0 is delivered to nothing: it only asks whether `pid` may be sent one.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits at most `long` for `fd` to be readable, or closed at its other end: whether it is.
#[cfg(unix)]
pub fn readable_within(fd: std::os::fd::BorrowedFd<'_>, long: Duration) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;
    let until = std::time::Instant::now() + long;
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        // Rounded up: a wait of less than a millisecond left is one, not none.
        let ms = libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
        let mut watched = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: `watched` is one live `pollfd` for the call to read and write, and its
        // descriptor is borrowed open for the call.
        match unsafe { libc::poll(&mut watched, 1, ms) } {
            // A timeout may come a little early (macOS's): the wait is `long`, waited again
            // for what is left.
            0 if std::time::Instant::now() < until => {}
            0 => return Ok(false),
            ready if ready > 0 => return Ok(true),
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// Waits at most `long` for the process `pid` to end, on the kernel's word that it did (a
/// pidfd), not by asking again and again: whether it ended. Any process this one may see is
/// waited for, not only a child; one that is gone, or ended and not yet waited for by its
/// parent, has ended.
#[cfg(target_os = "linux")]
pub fn ended_within(pid: u32, long: Duration) -> std::io::Result<bool> {
    use std::os::fd::{AsFd as _, FromRawFd as _, OwnedFd};
    let Ok(pid) = libc::pid_t::try_from(pid) else { return Ok(true) };
    // SAFETY: pidfd_open takes a number and flags and reads no memory of this process; the
    // descriptor it gives is owned below.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(true) } else { Err(error) };
    }
    // SAFETY: the descriptor just opened, which nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    readable_within(fd.as_fd(), long)
}

/// Waits at most `long` for the process `pid` to end, on the kernel's word that it did
/// (kqueue's `EVFILT_PROC`), not by asking again and again: whether it ended. Any process this
/// one may see is waited for, not only a child; one that is gone has ended.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn ended_within(pid: u32, long: Duration) -> std::io::Result<bool> {
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
    let Ok(pid) = usize::try_from(pid) else { return Ok(true) };
    // SAFETY: takes nothing and reads no memory of this process; the descriptor it gives is
    // owned below.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the descriptor just opened, which nothing else owns.
    let queue = unsafe { OwnedFd::from_raw_fd(queue) };
    // SAFETY: all zeros is a `kevent` of nothing, its fields then set.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: registers the one live `change`, read only, and asks for no event back: a null
    // list of none and no timeout are allowed.
    if unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(true) } else { Err(error) };
    }
    // A kqueue is readable once an event it watches has come.
    readable_within(queue.as_fd(), long)
}

/// How a child of this process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reaped {
    Code(i32),
    Signal(i32),
}

/// How `pid` ended, waited for, when it is a child of this process that ended; `None` when
/// it runs, or is no child of this one (whose end only its parent hears).
#[cfg(unix)]
pub fn reap(pid: u32) -> Option<Reaped> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut status = 0;
    // SAFETY: `status` is live for the call to write. `WNOHANG` returns at once; a `pid` that
    // is no child of this process is refused (`ECHILD`) and touches nothing. A child waited
    // for here is one no `std::process::Child` of this process waits for again: the one the
    // CLI starts the daemon as is dropped once spawned.
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != pid {
        return None;
    }
    Some(match libc::WIFSIGNALED(status) {
        true => Reaped::Signal(libc::WTERMSIG(status)),
        false => Reaped::Code(libc::WEXITSTATUS(status)),
    })
}

/// Whether the process `pid` runs.
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // SAFETY: `OpenProcess` takes no pointer; any `pid` is either opened or refused with null.
    // The handle it opens is owned below, so it is closed once.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: `process` is an open handle that nothing else owns: `OpenProcess` just made it.
    // Were it owned twice, it would be closed twice, the second time maybe another's handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the handle is open for as long as `process` lives, which outlives the call; a
    // wait of no time only asks whether the process ended.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

#[cfg(not(any(unix, windows)))]
pub fn alive(_: u32) -> bool {
    false
}

/// Ends the process group `child` leads (a worker started in a group of its own, with the
/// processes it started), while `child` has not exited: one that has exited has nothing to
/// kill, and once it is waited for its pid may be another's.
#[cfg(unix)]
pub fn kill_group(child: &mut std::process::Child) {
    if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
        // SAFETY: `killpg` takes no pointer, so no memory of this process is at stake. That
        // the group is the worker's is upheld by `child` not having been waited for, checked
        // just above: until it is, its pid is not given to another process.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// Has a write to a pipe whose reader is gone end the process quietly, as `SIGPIPE`'s
/// default action does, where std ignores the signal so that the write fails with `EPIPE`.
/// Not for a process that writes to sockets: a reset from the other end would end it.
#[cfg(unix)]
pub fn die_on_closed_pipe() {
    // SAFETY: `signal` with `SIG_DFL` installs no handler of this process's, so no code of it
    // ever runs in a signal's context, and it is one atomic change to the process, sound from
    // any thread. What it changes is how a later write to a closed pipe ends: the process ends,
    // which is no undefined behavior.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// Has the process `command` starts inherit `fd` under its own number: it is cleared of
/// close-on-exec in the child alone, so no other process this one starts gets it.
#[cfg(unix)]
pub fn inherit_in_child(command: &mut std::process::Command, fd: std::os::fd::RawFd) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: runs in the child between its fork and its exec, and calls only `fcntl`, which
    // is async-signal-safe and touches no memory; `fd` is open there, the child's copy of the
    // parent's, as the caller keeps it open until the spawn.
    unsafe {
        command.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        })
    };
}

/// Whose CPU time [`cpu_time`] reads.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub enum Whose {
    /// This process's own.
    Itself,
    /// That of its children which it waited for, and theirs.
    Waited,
}

/// The user and system CPU time `whose` had; None where the kernel says nothing of it.
#[cfg(unix)]
pub fn cpu_time(whose: Whose) -> Option<Duration> {
    let who = match whose {
        Whose::Itself => libc::RUSAGE_SELF,
        Whose::Waited => libc::RUSAGE_CHILDREN,
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a live, writable `rusage`, and an `rusage` is all `getrusage` writes;
    // a smaller buffer would be written past its end.
    if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: an `rusage` is integers alone, for which the zeros `usage` began as are a value,
    // and `getrusage` wrote another over them. A type with a niche (a reference, an enum) would
    // have no such value in its zeros, and reading it would be undefined.
    let usage = unsafe { usage.assume_init() };
    let time = |it: libc::timeval| {
        let micros = Duration::from_micros(u64::try_from(it.tv_usec).unwrap_or(0));
        Duration::from_secs(u64::try_from(it.tv_sec).unwrap_or(0)) + micros
    };
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

/// The ticks of the clock in a second, the unit of `/proc/<pid>/stat`'s times.
#[cfg(target_os = "linux")]
pub fn clock_ticks() -> Option<u64> {
    // SAFETY: `sysconf` takes no pointer and reads a constant of the system; a name it does
    // not know is answered with -1, which is refused below.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).ok().filter(|it| *it > 0)
}

/// The handles of this process that a child would inherit, made not inheritable until this
/// is dropped: [`withhold_inheritance`].
#[cfg(windows)]
pub struct Withheld(Vec<windows_sys::Win32::Foundation::HANDLE>);

/// Has a process started until the result is dropped inherit no handle of this one but those
/// it is given. A process is started with every handle of its parent that can be inherited,
/// and those of a pipe this command writes to are among them, under their own numbers and
/// under those the commands above this one had: whoever reads the pipe would wait for the
/// child to end.
#[cfg(windows)]
pub fn withhold_inheritance() -> Withheld {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    /// Handles are multiples of four, and a command as short as this one has few.
    const HANDLES: std::ops::RangeInclusive<usize> = 4..=0x1_0000;

    let mut withheld = Vec::new();
    for handle in HANDLES.step_by(4).map(|it| it as HANDLE) {
        let mut flags = 0;
        // SAFETY: `GetHandleInformation` checks `handle` and fails on a number that is no open
        // handle, and `flags` is a `u32` it only writes; no Rust memory rests on the handle.
        let held = unsafe { GetHandleInformation(handle, &mut flags) } != 0
            && flags & HANDLE_FLAG_INHERIT != 0
            // SAFETY: `handle` was just found open and inheritable; clearing the flag touches no
            // memory, and `Withheld`'s drop sets it again. A number closed and reused meanwhile
            // only has that handle's inheritance changed.
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0;
        if held {
            withheld.push(handle);
        }
    }
    Withheld(withheld)
}

#[cfg(windows)]
impl Drop for Withheld {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for &handle in &self.0 {
            // SAFETY: the handle was open and inheritable when it was withheld; one closed
            // since is refused, and one whose number another took has its inheritance set,
            // which is no memory of this process's.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive() {
        if cfg!(any(unix, windows)) {
            assert!(alive(std::process::id()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn this_process_used_some_cpu() {
        let mut spin = 0u64;
        for it in 0..1_000_000u64 {
            spin = std::hint::black_box(spin.wrapping_add(it));
        }
        assert!(cpu_time(Whose::Itself).is_some_and(|it| it > Duration::ZERO));
        assert!(cpu_time(Whose::Waited).is_some());
    }

    /// A child that runs until its standard input is closed.
    #[cfg(unix)]
    fn until_closed() -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_wait_for_a_process_ends_on_its_exit_not_its_limit() {
        let mut child = until_closed();
        let stdin = child.stdin.take().unwrap();
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                blaze_graph::spin::busy(Duration::from_millis(100));
                drop(stdin);
            });
            assert!(ended_within(child.id(), Duration::from_secs(60)).unwrap());
        });
        assert!(started.elapsed() < Duration::from_secs(10), "ended after {:?}", started.elapsed());
        child.wait().unwrap();
        assert!(ended_within(child.id(), Duration::from_secs(60)).unwrap(), "one reaped and gone has ended");
    }

    #[cfg(unix)]
    #[test]
    fn a_wait_for_a_process_that_runs_on_gives_up_at_its_limit() {
        let mut child = until_closed();
        let started = std::time::Instant::now();
        assert!(!ended_within(child.id(), Duration::from_millis(50)).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(50));
        drop(child.stdin.take());
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_is_readable_once_written_or_closed_and_not_before() {
        use std::{io::Write as _, os::fd::AsFd as _};
        let (reader, mut writer) = std::io::pipe().unwrap();
        assert!(!readable_within(reader.as_fd(), Duration::from_millis(20)).unwrap());
        writer.write_all(b"\n").unwrap();
        assert!(readable_within(reader.as_fd(), Duration::from_secs(60)).unwrap());
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let started = std::time::Instant::now();
        assert!(readable_within(reader.as_fd(), Duration::from_secs(60)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_clock_ticks() {
        assert!(clock_ticks().is_some());
    }
}
