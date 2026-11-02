//! The system calls of the macOS sandbox, each behind a function safe code can't misuse: a
//! pointer it gives the kernel is that of a borrow it holds for the call (a `&CStr`, a slice, a
//! [`Strings`] list borrowed from its strings), and a call that takes only integers is safe as
//! it is. [`fork`] stays `unsafe fn`, sound only in a process of one thread. None of these has
//! a safe std equivalent at no cost: std's `Command` forks and execs in one step, with no room
//! for the child to apply its Seatbelt profile first.

use std::{
    ffi::{CStr, CString},
    marker::PhantomData,
};

use libc::{c_char, c_int, pid_t};

// SAFETY: libsystem_sandbox's `int sandbox_init_with_parameters(const char *profile, uint64_t
// flags, const char *const parameters[], char **errorbuf)`, as `sandbox-exec` calls it; a
// declaration that differs would have every call pass its arguments wrongly.
unsafe extern "C" {
    // libsystem_sandbox, part of libSystem: what `sandbox-exec` does, without a process of
    // its own to run.
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
}

/// The null-ended list of pointers to nul-terminated strings a C call takes (`argv`, `envp`,
/// Seatbelt's parameters), borrowed from the strings so no pointer outlives its string. Made
/// before a fork, so the child allocates nothing for it.
pub(super) struct Strings<'a> {
    pointers: Vec<*const c_char>,
    strings: PhantomData<&'a [CString]>,
}

impl<'a> Strings<'a> {
    pub(super) fn new(strings: &'a [CString]) -> Strings<'a> {
        let mut pointers = Vec::with_capacity(strings.len() + 1);
        pointers.extend(strings.iter().map(|it| it.as_ptr()));
        pointers.push(std::ptr::null());
        Strings { pointers, strings: PhantomData }
    }
}

/// `fork`: the child's pid in the parent, 0 in the child, below 0 when there is none.
///
/// # Safety
///
/// This process has one thread. The child is a copy of the calling thread alone: with another
/// thread there, a lock it held (the allocator's, stdio's, the environment's) stays held in
/// the child for good, and the child, which allocates and prints here, deadlocks or sees what
/// that thread left half done.
pub(super) unsafe fn fork() -> pid_t {
    // SAFETY: the caller has this process be one thread (# Safety); `fork` takes no arguments.
    unsafe { libc::fork() }
}

/// Ends this process at once with `code` (`_exit`): no `atexit` handler runs and no stdio
/// buffer copied from the process it was forked from is flushed, which would run or be
/// written a second time.
pub(super) fn exit(code: c_int) -> ! {
    // SAFETY: `_exit` takes the code by value, touches no memory of this process and never
    // returns; what it skips (destructors) only leaks.
    unsafe { libc::_exit(code) }
}

/// Makes this process the leader of a new session, with no controlling terminal.
pub(super) fn setsid() -> pid_t {
    // SAFETY: `setsid` takes no arguments and touches no memory of this process.
    unsafe { libc::setsid() }
}

pub(super) fn chdir(path: &CStr) -> c_int {
    // SAFETY: `path` is a nul-terminated string borrowed for the call, which only reads it.
    unsafe { libc::chdir(path.as_ptr()) }
}

pub(super) fn kill(pid: pid_t, signal: c_int) -> c_int {
    // SAFETY: `kill` takes integers only and touches no memory of this process.
    unsafe { libc::kill(pid, signal) }
}

/// Waits for the child `pid` to end without reaping it (`waitid` with `WNOWAIT`), so its
/// number and its group's are no other process's until [`wait`]: what `waitid` returned.
pub(super) fn wait_unreaped(pid: pid_t) -> c_int {
    // SAFETY: `siginfo_t` is integers and a union of integers and pointers, for which all-zero
    // bytes are a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, exclusive `siginfo_t` for the call to fill, and nothing else.
    unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) }
}

/// `waitpid`, with the status written to `status`.
pub(super) fn wait(pid: pid_t, status: &mut c_int, options: c_int) -> pid_t {
    // SAFETY: `status` is a live, exclusive `c_int` for the call to write, and nothing else.
    unsafe { libc::waitpid(pid, status, options) }
}

/// `confstr(name)` into `buffer`: what it returned, the length it needs with its nul, 0 for
/// none.
pub(super) fn confstr(name: c_int, buffer: &mut [u8]) -> usize {
    // SAFETY: `buffer` is live and exclusive for the call, which writes at most `buffer.len()`
    // bytes into it (and says how many it needed, which the caller checks).
    unsafe { libc::confstr(name, buffer.as_mut_ptr().cast(), buffer.len()) }
}

/// Applies the Seatbelt `profile` with `parameters` (names and values in turn) to this
/// process and what it runs: why not, as Seatbelt said, when it refused.
pub(super) fn sandbox_init(profile: &CStr, parameters: &Strings<'_>) -> Result<(), String> {
    let mut error: *mut c_char = std::ptr::null_mut();
    // SAFETY: `profile` is a nul-terminated string and `parameters` a null-ended list of
    // nul-terminated strings, both borrowed for the call, which only reads them; `error` is a
    // live pointer for the call to set to a string it makes, or leave null.
    let applied = unsafe {
        sandbox_init_with_parameters(profile.as_ptr(), 0, parameters.pointers.as_ptr(), &mut error)
    };
    if applied == 0 {
        return Ok(());
    }
    if error.is_null() {
        // alloc: the error, once, and the program does not run.
        return Err("it said no more".to_owned());
    }
    // SAFETY: not null, so `error` is the nul-terminated string `sandbox_init_with_parameters`
    // made, which nothing frees before this reads it (and nothing after: the process ends).
    let why = unsafe { CStr::from_ptr(error) };
    Err(why.to_string_lossy().into_owned())
}

/// Runs `argv[0]` with `argv` and `env` in place of this process. It returns only where it
/// couldn't, with `errno` set.
pub(super) fn execve(argv: &Strings<'_>, env: &Strings<'_>) {
    // SAFETY: `argv` and `env` end with null and each other entry is a nul-terminated string
    // they borrow for the call; `argv`'s first is the program's path, or null with no `argv`,
    // which the kernel refuses (EFAULT). Nothing is written through them.
    unsafe { libc::execve(argv.pointers[0], argv.pointers.as_ptr(), env.pointers.as_ptr()) };
}
