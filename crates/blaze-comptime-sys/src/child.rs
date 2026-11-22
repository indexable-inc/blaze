//! THE C FUNCTIONS (feature `child`): linked into blaze-launcher, never exported under the C
//! names (no `no_mangle`: the launcher's own calls keep the real C library). The launcher's
//! binder asks [`binding`] for every outside name of a compile-time run's code records and
//! binds the address it gets, or the process's own definition for [`Binding::Own`], or refuses
//! the record with the [`BindRefused`] it gets.
//!
//! Each function is thin: C arguments in, one [`Sys`] call, [`drive`] while the call is with
//! the host, the C answer out (a value, or `-1` with `errno` set through [`Real::errno`]).
//!
//! THE ONE PROCESS STATIC (the C7 exception this crate names): the C functions are reached by
//! address from code that passes them no context, so the layer lives in one cell, [`LAYER`],
//! installed once by [`install`] before the entry runs. It is sound because a compile-time
//! child has ONE thread (`pthread_create` is refused), and a reentrant call (a signal handler,
//! a callback into the layer from the layer) is a typed fault, never a second borrow.

#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_int, c_long, c_uint, c_void};

use blaze_comptime::{GuestEffect, GuestEvent, Nanos};
use blaze_own::{AVec, Alloc, Vec};

use crate::errno::{Errno, Os};
use crate::sys::{DirEnt, Fault, Fd, Kind, Limits, Ret, StatOut, Sys, SysOpts, SysState, consts, mmap_allowed, mprotect_allowed};
use crate::table::{BindRefused, How, find};

/// The launcher's side of the protocol descriptor, and the process's end.
pub trait Transport {
    /// One frame, whole (`GuestEffect::Send`).
    fn send(&mut self, frame: &[u8]) -> Result<(), TransportError>;
    /// The next frame's BODY (the length prefix read and checked by `split_frame`), into
    /// `into`, cleared first.
    fn receive(&mut self, into: &mut AVec<'static, u8>) -> Result<(), TransportError>;
    /// The launcher's clock, for the machines' `now` (never answered to the code).
    fn now(&mut self) -> Nanos;
    /// The run is over: `exit`, `abort`, a macro's `Return`. The launcher ends the process
    /// with `code` (G5: today's `run` ends with `libc::exit`).
    fn end(&mut self, code: i32) -> !;
    /// The layer cannot go on: the launcher reports `fault` on its own stream and ends.
    fn fault(&mut self, fault: ChildFault) -> !;
}

/// Why the transport failed: the host is gone or sent bytes that are not a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportError {
    Closed,
    Io { errno: i32 },
    Frame,
}

/// Why the child stops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildFault {
    Sys(Fault),
    Transport(TransportError),
    /// A C function of the layer was called from inside the layer (a signal handler).
    Reentered,
    /// A C function before [`install`].
    NotInstalled,
    /// A C function with no failure value whose host call failed (`arc4random_buf`).
    Unanswerable { name: &'static str },
}

/// The real C library's functions the layer itself needs, handed in (rule 75: nothing found
/// by the layer on its own).
#[derive(Clone, Copy)]
pub struct Real {
    /// `__error` (macOS) or `__errno_location` (Linux).
    pub errno: unsafe extern "C" fn() -> *mut c_int,
    pub mmap: unsafe extern "C" fn(*mut c_void, usize, c_int, c_int, c_int, i64) -> *mut c_void,
    pub mprotect: unsafe extern "C" fn(*mut c_void, usize, c_int) -> c_int,
    /// The real loader's lookup, used ONLY for a name the table answers [`How::Own`] (a weak
    /// lookup by the code of such a name: the table admitted it already).
    pub dlsym: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    /// `RTLD_DEFAULT` (macOS `(void *)-2`, Linux `NULL`).
    pub default_handle: *mut c_void,
}

/// [`install`]'s arguments.
pub struct ChildOpts {
    pub os: Os,
    pub limits: Limits,
    /// The process's allocator for the layer's own state: never the code's `malloc` (which
    /// the code may have replaced) and never `Global` (rule 75).
    pub alloc: Alloc<'static>,
    pub transport: &'static mut dyn Transport,
    pub real: Real,
}

/// Why [`install`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallError {
    /// A layer is installed already: one per process.
    Twice,
    Fault(ChildFault),
    /// The host refused the arguments.
    Args { errno: Errno },
}

struct Layer {
    sys: Sys<'static>,
    transport: &'static mut dyn Transport,
    real: Real,
    frame: AVec<'static, u8>,
    /// `argv`, `environ`: pointer arrays into the layer's words, NULL-ended.
    argv: AVec<'static, *const c_char>,
    argv_word: *const *const c_char,
    argc: c_int,
    /// `copyfile_state_t`'s one word.
    copied: u64,
    environ: AVec<'static, *const c_char>,
    environ_word: *const *const c_char,
    /// `readdir`'s answer: one entry, the largest of the two systems' `struct dirent`.
    dirent: [u64; 132],
    /// `posix_spawn` words: `argv[1..]` and `envp`.
    words: AVec<'static, u8>,
    envp: AVec<'static, u8>,
    gather: AVec<'static, u8>,
    busy: bool,
}

/// The cell (the module doc: one thread, reentry refused).
struct Cell(UnsafeCell<Option<Layer>>);

// SAFETY: one thread touches the cell: the launcher's first, which calls `install`, binds every
// `How::Data` row ([`binding`], the only binding that reaches the cell) and then runs the entry.
// The run's control thread binds every other row, from constants only (blaze-launch-rt
// run_mode/bind.rs `Table::bind`, which refuses a data row it was not given before the run);
// the program's code makes no thread (`pthread_create` is refused by the table).
unsafe impl Sync for Cell {}

static LAYER: Cell = Cell(UnsafeCell::new(None));

/// Installs the layer: `Hello`, the working directory and the arguments cross now.
pub fn install(opts: ChildOpts) -> Result<(), InstallError> {
    // SAFETY: the one thread (module doc); no borrow of the cell is live: install is called
    // before any C function of the layer.
    let cell = unsafe { &mut *LAYER.0.get() };
    if cell.is_some() {
        return Err(InstallError::Twice);
    }
    let alloc = opts.alloc;
    *cell = Some(Layer {
        sys: Sys::new(SysOpts { os: opts.os, limits: opts.limits, alloc, .. }),
        transport: opts.transport,
        real: opts.real,
        frame: Vec::new_in(alloc),
        argv: Vec::new_in(alloc),
        argv_word: core::ptr::null(),
        argc: 0,
        copied: 0,
        environ: Vec::new_in(alloc),
        environ_word: core::ptr::null(),
        dirent: [0; 132],
        words: Vec::new_in(alloc),
        envp: Vec::new_in(alloc),
        gather: Vec::new_in(alloc),
        busy: false,
    });
    let layer = cell.as_mut().ok_or(InstallError::Twice)?;
    start(layer).map_err(InstallError::Fault)?;
    if let Err(fault) = layer.sys.args() {
        return Err(InstallError::Fault(ChildFault::Sys(fault)));
    }
    if let Err(errno) = drive(layer) {
        return Err(InstallError::Args { errno });
    }
    build_argv(layer);
    Ok(())
}

/// `argc` and `argv` for the entry (the launcher's `comptime:` form passes them to `main`).
pub fn argv() -> Result<(c_int, *const *const c_char), ChildFault> {
    with(|layer| Ok((layer.argc, layer.argv_word)))
}

fn start(layer: &mut Layer) -> Result<(), ChildFault> {
    while layer.sys.state() == SysState::Starting {
        pump(layer)?;
    }
    Ok(())
}

/// One exchange: send what the machine has, read one frame, step it in.
fn pump(layer: &mut Layer) -> Result<(), ChildFault> {
    while let Some(GuestEffect::Send { frame }) = layer.sys.next_effect() {
        layer.transport.send(frame).map_err(ChildFault::Transport)?;
    }
    if let SysState::Ended { code } = layer.sys.state() {
        layer.transport.end(code);
    }
    layer.transport.receive(&mut layer.frame).map_err(ChildFault::Transport)?;
    let now = layer.transport.now();
    layer.sys.step(now, GuestEvent::Frame(&layer.frame)).map_err(ChildFault::Sys)
}

/// Drives the call in flight to its answer.
fn drive(layer: &mut Layer) -> Result<Ret, Errno> {
    loop {
        match layer.sys.state() {
            SysState::Done => {
                return match layer.sys.result() {
                    Ok(result) => result,
                    Err(fault) => layer.transport.fault(ChildFault::Sys(fault)),
                };
            }
            SysState::Asking | SysState::Starting | SysState::Ended { .. } => {
                if let Err(fault) = pump(layer) {
                    layer.transport.fault(fault);
                }
            }
            state => layer.transport.fault(ChildFault::Sys(Fault::Busy { state })),
        }
    }
}

/// Runs `body` on the layer, refusing reentry.
fn with<T>(body: impl FnOnce(&mut Layer) -> Result<T, ChildFault>) -> Result<T, ChildFault> {
    // SAFETY: one thread; `busy` refuses a second borrow from a handler or callback.
    let layer = unsafe { (*LAYER.0.get()).as_mut() }.ok_or(ChildFault::NotInstalled)?;
    if layer.busy {
        return Err(ChildFault::Reentered);
    }
    layer.busy = true;
    let out = body(layer);
    layer.busy = false;
    out
}

/// A C call: on a fault the run ends (there is no C answer that would not lie).
fn c<T>(body: impl FnOnce(&mut Layer) -> T) -> T {
    match with(|layer| Ok(body(layer))) {
        Ok(value) => value,
        Err(fault) => {
            // SAFETY: as `with`; the fault path ends the process and borrows nothing else.
            match unsafe { (*LAYER.0.get()).as_mut() } {
                Some(layer) => layer.transport.fault(fault),
                // No layer, so no transport to report on: the launcher's panic handler reports
                // it and ends the process (rule 20: never a hang).
                None => panic!("blaze-comptime-sys: a C function of the layer ran before `install` ({fault:?})"),
            }
        }
    }
}

fn os(layer: &Layer) -> Os {
    layer.sys.os()
}

fn set_errno(layer: &Layer, errno: Errno) {
    // SAFETY: `Real::errno` is the C library's own thread errno accessor.
    unsafe { *(layer.real.errno)() = errno.code(os(layer)) };
}

/// `-1` with `errno`, or the value.
/// `int!(layer, result)`: the result is computed first (it may borrow the layer mutably),
/// then the layer is read to set `errno`; a plain call would borrow it twice at once.
macro_rules! int {
    ($layer:expr, $result:expr) => {{
        let result: Result<i64, Errno> = $result;
        int($layer, result)
    }};
}

/// `long!(layer, result)`: as `int!`, for an `ssize_t` answer.
macro_rules! long {
    ($layer:expr, $result:expr) => {{
        let result: Result<i64, Errno> = $result;
        long($layer, result)
    }};
}

fn int(layer: &Layer, result: Result<i64, Errno>) -> c_int {
    match result {
        Ok(value) => value as c_int,
        Err(errno) => {
            set_errno(layer, errno);
            -1
        }
    }
}

fn long(layer: &Layer, result: Result<i64, Errno>) -> isize {
    match result {
        Ok(value) => value as isize,
        Err(errno) => {
            set_errno(layer, errno);
            -1
        }
    }
}

/// Drives a call already started on the machine (for a start that borrows the layer's own
/// buffers, which `ask`'s closure cannot).
fn started(layer: &mut Layer, start: Result<(), Fault>) -> Result<Ret, Errno> {
    if let Err(fault) = start {
        layer.transport.fault(ChildFault::Sys(fault));
    }
    drive(layer)
}

/// Starts a call on the machine and drives it.
fn ask(layer: &mut Layer, start: impl FnOnce(&mut Sys<'static>) -> Result<(), Fault>) -> Result<Ret, Errno> {
    if let Err(fault) = start(&mut layer.sys) {
        layer.transport.fault(ChildFault::Sys(fault));
    }
    drive(layer)
}

fn as_int(ret: Result<Ret, Errno>) -> Result<i64, Errno> {
    match ret? {
        Ret::Int(value) => Ok(value),
        _ => Ok(0),
    }
}

/// The bytes of a C string.
///
/// SAFETY: `p` is a NUL-ended string the caller passed, as the C function's contract says.
unsafe fn bytes<'p>(p: *const c_char) -> &'p [u8] {
    if p.is_null() { &[] } else { unsafe { CStr::from_ptr(p) }.to_bytes() }
}

/// SAFETY: `buf` is `len` writable bytes, as the C function's contract says.
unsafe fn out<'b>(buf: *mut c_void, len: usize) -> &'b mut [u8] {
    if len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) } }
}

/// SAFETY: `buf` is `len` readable bytes.
unsafe fn input<'b>(buf: *const c_void, len: usize) -> &'b [u8] {
    if len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) } }
}

fn build_argv(layer: &mut Layer) {
    layer.argv.clear();
    let words = layer.sys.args_words().unwrap_or(&[]);
    let mut count = 0;
    for word in words.split_inclusive(|b| *b == 0) {
        layer.argv.push(word.as_ptr().cast());
        count += 1;
    }
    layer.argv.push(core::ptr::null());
    layer.argv_word = layer.argv.as_ptr();
    layer.argc = count;
}

fn build_environ(layer: &mut Layer) {
    if !layer.environ_word.is_null() && layer.sys.environ_words().is_some() {
        return;
    }
    let _ = ask(layer, |sys| sys.environ());
    layer.environ.clear();
    if let Some(words) = layer.sys.environ_words() {
        for word in words.split_inclusive(|b| *b == 0) {
            layer.environ.push(word.as_ptr().cast());
        }
    }
    layer.environ.push(core::ptr::null());
    layer.environ_word = layer.environ.as_ptr();
}

// ---- binding --------------------------------------------------------------------------------

/// How the launcher binds one outside name of a compile-time run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binding {
    /// The process's own definition (the launcher's `dlsym`).
    Own,
    /// This address.
    Here(usize),
}

/// The binding of `name` on the installed layer's system: the table's row, then this
/// module's function for it. A `How::Data` row's binding builds its word IN THE LAYER, so it is
/// asked only on the thread that installed the layer (the cell's SAFETY); every other row is
/// answered from constants and may be asked from any thread. A row with no function here is a bug of this crate, refused like a
/// missing row (a test checks every row has one).
pub fn binding(name: &str, os: Os) -> Result<Binding, BindRefused<'_>> {
    let row = find(name, os)?;
    match row.how {
        How::Own => Ok(Binding::Own),
        How::Data => data(name).map(Binding::Here).ok_or(BindRefused { name, why: crate::table::Unbound::NotInTable }),
        _ => function(name).map(Binding::Here).ok_or(BindRefused { name, why: crate::table::Unbound::NotInTable }),
    }
}

fn data(name: &str) -> Option<usize> {
    match name {
        "environ" => c(|layer| {
            build_environ(layer);
            Some(core::ptr::addr_of!(layer.environ_word) as usize)
        }),
        _ => None,
    }
}

macro_rules! table_fns {
    ($($c:literal => $f:ident),* $(,)?) => {
        /// The function bound for each `Local`, `Asks`, `Refused` and `OwnChecked` row.
        pub fn function(name: &str) -> Option<usize> {
            match name {
                $($c => Some($f as usize),)*
                _ => None,
            }
        }
    };
}

macro_rules! refused {
    ($($f:ident => $c:literal, $errno:expr;)*) => {
        $(
            /// Refused by the table: a finding, and the C failure value.
            unsafe extern "C" fn $f() -> c_int {
                c(|layer| {
                    let ret = ask(layer, |sys| sys.denied($c.as_bytes(), $errno));
                    int!(layer,as_int(ret))
                })
            }
        )*
    };
}

refused! {
    r_chmod => "chmod", Errno::Perm;
    r_chown => "chown", Errno::Perm;
    r_chroot => "chroot", Errno::Perm;
    r_execve => "execve", Errno::NoSys;
    r_execvp => "execvp", Errno::NoSys;
    r_fchmod => "fchmod", Errno::Perm;
    r_fchmodat => "fchmodat", Errno::Perm;
    r_fchown => "fchown", Errno::Perm;
    r_fork => "fork", Errno::NoSys;
    r_fsetattrlist => "fsetattrlist", Errno::Perm;
    r_futimens => "futimens", Errno::Perm;
    r_lchown => "lchown", Errno::Perm;
    r_link => "link", Errno::Perm;
    r_linkat => "linkat", Errno::Perm;
    r_mkfifo => "mkfifo", Errno::Perm;
    r_pause => "pause", Errno::Intr;
    r_setattrlist => "setattrlist", Errno::Perm;
    r_setgid => "setgid", Errno::Perm;
    r_setgroups => "setgroups", Errno::Perm;
    r_setpgid => "setpgid", Errno::Perm;
    r_setsid => "setsid", Errno::Perm;
    r_setuid => "setuid", Errno::Perm;
    r_symlink => "symlink", Errno::Perm;
    r_utimensat => "utimensat", Errno::Perm;
}

table_fns! {
    "CCRandomGenerateBytes" => cc_random, "_NSGetArgc" => ns_argc, "_NSGetArgv" => ns_argv,
    "_NSGetEnviron" => ns_environ, "_NSGetExecutablePath" => ns_exe,
    "_dyld_get_image_header" => null_ptr1, "_dyld_get_image_name" => null_ptr1,
    "_dyld_get_image_vmaddr_slide" => zero_l1, "_dyld_image_count" => zero0,
    "_exit" => c_exit, "abort" => c_abort, "exit" => c_exit,
    "accept" => sock_fd, "accept4" => sock_fd, "bind" => sock_fd, "connect" => sock_fd, "listen" => sock_fd,
    "getpeername" => sock_fd, "getsockname" => sock_fd, "getsockopt" => sock_fd, "setsockopt" => sock_fd,
    "shutdown" => sock_fd, "recv" => sock_fd_l, "recvfrom" => sock_fd_l, "recvmsg" => sock_fd_l,
    "send" => sock_fd_l, "sendto" => sock_fd_l, "sendmsg" => sock_fd_l, "getpeereid" => sock_fd,
    "arc4random_buf" => arc4random_buf, "chdir" => c_chdir,
    "chmod" => r_chmod, "chown" => r_chown, "chroot" => r_chroot, "execve" => r_execve, "execvp" => r_execvp,
    "fchmod" => r_fchmod, "fchmodat" => r_fchmodat, "fchown" => r_fchown, "fork" => r_fork,
    "fsetattrlist" => r_fsetattrlist, "futimens" => r_futimens, "lchown" => r_lchown, "link" => r_link,
    "linkat" => r_linkat, "mkfifo" => r_mkfifo, "pause" => r_pause, "setattrlist" => r_setattrlist,
    "setgid" => r_setgid, "setgroups" => r_setgroups, "setpgid" => r_setpgid, "setsid" => r_setsid,
    "setuid" => r_setuid, "symlink" => r_symlink, "utimensat" => r_utimensat,
    "clock_getres" => clock_getres, "clock_gettime" => clock_gettime, "clock_nanosleep" => clock_nanosleep,
    "close" => c_close, "closedir" => closedir, "confstr" => confstr, "copyfile_state_alloc" => copyfile_state_alloc,
    "copyfile_state_free" => zero1, "copyfile_state_get" => copyfile_state_get, "dirfd" => dirfd,
    "dl_iterate_phdr" => zero2, "dlclose" => zero1, "dlerror" => dlerror, "dlopen" => dlopen, "dlsym" => c_dlsym,
    "dup" => c_dup, "dup2" => c_dup2, "fclonefileat" => fclonefileat, "fcntl" => fcntl, "fcopyfile" => fcopyfile,
    "fdatasync" => fsync, "fsync" => fsync, "fdopendir" => fdopendir, "flock" => flock, "freeaddrinfo" => void1,
    "fstat" => c_fstat, "fstat64" => c_fstat, "fstatat" => fstatat, "fstatat64" => fstatat,
    "ftruncate" => ftruncate, "ftruncate64" => ftruncate, "getaddrinfo" => getaddrinfo, "getauxval" => getauxval,
    "getcwd" => getcwd, "getentropy" => getentropy, "getenv" => c_getenv, "getgid" => uid0, "getuid" => uid0,
    "gethostname" => gethostname, "getpgid" => pid0, "getpid" => pid0, "getppid" => ppid0, "gettid" => pid0,
    "getpwuid_r" => getpwuid_r, "getrandom" => getrandom, "gettimeofday" => gettimeofday,
    "gnu_get_libc_version" => libc_version, "ioctl" => ioctl, "isatty" => isatty, "kill" => kill, "killpg" => kill,
    "lseek" => c_lseek, "lseek64" => c_lseek, "lstat" => c_stat, "lstat64" => c_stat, "stat" => c_stat, "stat64" => c_stat,
    "mach_absolute_time" => mach_absolute_time, "mach_timebase_info" => mach_timebase_info,
    "mach_wait_until" => mach_wait_until, "mkdir" => c_mkdir, "mkdirat" => mkdirat,
    "mmap" => c_mmap, "mmap64" => c_mmap, "mprotect" => c_mprotect, "nanosleep" => nanosleep,
    "open" => c_open, "open64" => c_open, "openat" => openat, "openat64" => openat, "opendir" => c_opendir,
    "pipe" => c_pipe, "pipe2" => c_pipe2, "poll" => c_poll,
    "posix_spawn" => posix_spawn, "posix_spawnp" => posix_spawn,
    "posix_spawn_file_actions_addchdir_np" => fa_chdir, "posix_spawn_file_actions_adddup2" => fa_dup2,
    "posix_spawn_file_actions_destroy" => fa_destroy, "posix_spawn_file_actions_init" => fa_init,
    "posix_spawnattr_destroy" => zero1, "posix_spawnattr_init" => zero1, "posix_spawnattr_setflags" => zero2,
    "posix_spawnattr_setpgroup" => zero2, "posix_spawnattr_setsigdefault" => zero2,
    "prctl" => prctl, "pread" => c_pread, "pread64" => c_pread, "preadv" => c_preadv, "preadv64" => c_preadv,
    "pthread_create" => pthread_create, "pthread_detach" => esrch1, "pthread_join" => esrch1,
    "pthread_threadid_np" => threadid, "pwrite" => c_pwrite, "pwrite64" => c_pwrite, "pwritev" => c_pwritev,
    "pwritev64" => c_pwritev, "read" => c_read, "readv" => c_readv, "readdir" => c_readdir, "readdir64" => c_readdir,
    "readdir_r" => readdir_r, "readlink" => c_readlink, "realpath" => c_realpath, "realpath$DARWIN_EXTSN" => c_realpath,
    "rename" => c_rename, "renameat" => renameat, "renamex_np" => renamex_np, "rewinddir" => rewinddir,
    "rmdir" => c_rmdir, "unlink" => c_unlink, "unlinkat" => unlinkat,
    "sched_getaffinity" => sched_getaffinity, "sched_yield" => zero0, "setenv" => c_setenv, "unsetenv" => c_unsetenv,
    "sigaction" => sigaction, "sigaltstack" => sigaltstack, "signal" => signal,
    "socket" => c_socket, "socketpair" => c_socket, "statx" => statx, "syscall" => syscall,
    "sysconf" => sysconf, "sysctlbyname" => sysctlbyname,
    "sysdir_get_next_search_path_enumeration" => zero2, "sysdir_start_search_path_enumeration" => zero2,
    "waitid" => waitid, "waitpid" => waitpid, "write" => c_write, "writev" => c_writev,
}

// ---- small fixed answers ------------------------------------------------------------------

unsafe extern "C" fn zero0() -> c_int {
    0
}
unsafe extern "C" fn zero1(_: usize) -> c_int {
    0
}
unsafe extern "C" fn zero2(_: usize, _: usize) -> c_int {
    0
}
unsafe extern "C" fn zero_l1(_: c_uint) -> isize {
    0
}
unsafe extern "C" fn void1(_: usize) {}
unsafe extern "C" fn null_ptr1(_: c_uint) -> *const c_void {
    core::ptr::null()
}
/// One process with a fixed number: no host's pid is ever seen (rule 25).
unsafe extern "C" fn pid0() -> c_int {
    2
}
unsafe extern "C" fn ppid0() -> c_int {
    1
}
unsafe extern "C" fn uid0() -> c_uint {
    1000
}
unsafe extern "C" fn esrch1(_: usize, _: usize) -> c_int {
    // `pthread_join`/`_detach` return the error, not -1: there is no other thread.
    c(|layer| Errno::Srch.code(os(layer)))
}
unsafe extern "C" fn threadid(_: usize, id: *mut u64) -> c_int {
    if !id.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { *id = 1 };
    }
    0
}
unsafe extern "C" fn dlerror() -> *const c_char {
    c"dlopen is refused in a compile-time run (blaze-comptime-sys table.rs)".as_ptr()
}
unsafe extern "C" fn dlopen(path: *const c_char, _: c_int) -> *mut c_void {
    c(|layer| {
        let _ = path;
        let _ = ask(layer, |sys| sys.denied(b"dlopen", Errno::NoEnt));
        core::ptr::null_mut()
    })
}
unsafe extern "C" fn c_dlsym(_: *mut c_void, name: *const c_char) -> *mut c_void {
    let (os, real) = c(|layer| (os(layer), layer.real));
    // SAFETY: a C string.
    let text = unsafe { bytes(name) };
    let Ok(text) = core::str::from_utf8(text) else { return core::ptr::null_mut() };
    match binding(text, os) {
        Ok(Binding::Here(at)) => at as *mut c_void,
        // SAFETY: the real loader, for a name the table admitted as the process's own.
        Ok(Binding::Own) => unsafe { (real.dlsym)(real.default_handle, name) },
        // Not in the table: absent, as a weak lookup of a missing symbol is.
        Err(_) => core::ptr::null_mut(),
    }
}
unsafe extern "C" fn libc_version() -> *const c_char {
    c"2.39".as_ptr()
}
unsafe extern "C" fn isatty(_: c_int) -> c_int {
    c(|layer| int!(layer,Err(Errno::NotTy)))
}

// ---- ends -------------------------------------------------------------------------------------

unsafe extern "C" fn c_exit(code: c_int) -> ! {
    c(|layer| {
        if let Err(fault) = layer.sys.exit(code) {
            layer.transport.fault(ChildFault::Sys(fault));
        }
        // `pump` sends the `Exit` and, at `Ended`, calls `Transport::end`, which does not return.
        if let Err(fault) = pump(layer) {
            layer.transport.fault(fault);
        }
        layer.transport.end(code);
    });
    // `c` is generic over its value and `!` cannot be one on stable inference: the closure
    // above ends the process on every path.
    unreachable!("blaze-comptime-sys: Transport::end returned")
}
unsafe extern "C" fn c_abort() -> ! {
    // SAFETY: as c_exit.
    unsafe { c_exit(134) }
}

// ---- descriptors --------------------------------------------------------------------------

unsafe extern "C" fn c_open(path: *const c_char, flags: c_int) -> c_int {
    // SAFETY: as `openat`.
    unsafe { openat(c(|layer| consts(os(layer)).at_fdcwd), path, flags) }
}
/// The mode (a third, variadic argument) is not read: a file in the output root has none the
/// run can see, and on macOS arm64 it is on the stack, which a fixed signature does not reach.
unsafe extern "C" fn openat(at: c_int, path: *const c_char, flags: c_int) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.open(at, path, flags));
        int!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_close(fd: c_int) -> c_int {
    c(|layer| int!(layer,layer.sys.close(Fd(fd)).map(|()| 0)))
}
unsafe extern "C" fn c_read(fd: c_int, buf: *mut c_void, len: usize) -> isize {
    // SAFETY: `len` writable bytes.
    let buf = unsafe { out(buf, len) };
    c(|layer| long!(layer,layer.sys.read(Fd(fd), buf, None).map(|n| n as i64)))
}
unsafe extern "C" fn c_pread(fd: c_int, buf: *mut c_void, len: usize, at: i64) -> isize {
    // SAFETY: as c_read.
    let buf = unsafe { out(buf, len) };
    c(|layer| long!(layer,if at < 0 { Err(Errno::Inval) } else { layer.sys.read(Fd(fd), buf, Some(at as u64)).map(|n| n as i64) }))
}

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

unsafe extern "C" fn c_readv(fd: c_int, iov: *const IoVec, count: c_int) -> isize {
    // SAFETY: as c_preadv.
    unsafe { c_preadv(fd, iov, count, -1) }
}
unsafe extern "C" fn c_preadv(fd: c_int, iov: *const IoVec, count: c_int, at: i64) -> isize {
    // SAFETY: `count` iovecs, each `len` writable bytes.
    let iov = unsafe { core::slice::from_raw_parts(iov, count.max(0) as usize) };
    c(|layer| {
        let mut total = 0i64;
        for one in iov {
            // SAFETY: as above.
            let buf = unsafe { out(one.base, one.len) };
            let place = if at < 0 { None } else { Some(at as u64 + total as u64) };
            match layer.sys.read(Fd(fd), buf, place) {
                Ok(n) => {
                    total += n as i64;
                    if n < one.len {
                        break;
                    }
                }
                Err(errno) if total == 0 => return long!(layer,Err(errno)),
                Err(_) => break,
            }
        }
        total as isize
    })
}
unsafe extern "C" fn c_write(fd: c_int, buf: *const c_void, len: usize) -> isize {
    // SAFETY: `len` readable bytes.
    let bytes = unsafe { input(buf, len) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.write(Fd(fd), bytes, None));
        long!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_pwrite(fd: c_int, buf: *const c_void, len: usize, at: i64) -> isize {
    // SAFETY: as c_write.
    let bytes = unsafe { input(buf, len) };
    c(|layer| {
        if at < 0 {
            return long!(layer,Err(Errno::Inval));
        }
        let ret = ask(layer, |sys| sys.write(Fd(fd), bytes, Some(at as u64)));
        long!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_writev(fd: c_int, iov: *const IoVec, count: c_int) -> isize {
    // SAFETY: as c_pwritev.
    unsafe { c_pwritev(fd, iov, count, -1) }
}
/// Gathered into one buffer, so a `writev` is one crossing.
unsafe extern "C" fn c_pwritev(fd: c_int, iov: *const IoVec, count: c_int, at: i64) -> isize {
    // SAFETY: `count` iovecs, each `len` readable bytes.
    let iov = unsafe { core::slice::from_raw_parts(iov, count.max(0) as usize) };
    c(|layer| {
        layer.gather.clear();
        for one in iov {
            // SAFETY: as above.
            layer.gather.extend_from_slice(unsafe { input(one.base, one.len) });
        }
        let place = if at < 0 { None } else { Some(at as u64) };
        let started_write = layer.sys.write(Fd(fd), &layer.gather, place);
        let ret = started(layer, started_write);
        long!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_lseek(fd: c_int, by: i64, whence: c_int) -> i64 {
    c(|layer| match layer.sys.lseek(Fd(fd), by, whence) {
        Ok(at) => at as i64,
        Err(errno) => {
            set_errno(layer, errno);
            -1
        }
    })
}
unsafe extern "C" fn c_dup(fd: c_int) -> c_int {
    c(|layer| int!(layer,layer.sys.dup(Fd(fd), 0).map(|fd| fd.0 as i64)))
}
unsafe extern "C" fn c_dup2(fd: c_int, to: c_int) -> c_int {
    c(|layer| int!(layer,layer.sys.dup2(Fd(fd), Fd(to)).map(|fd| fd.0 as i64)))
}
unsafe extern "C" fn flock(fd: c_int, _: c_int) -> c_int {
    // One process: every lock is free.
    c(|layer| int!(layer,if layer.sys.is_open(Fd(fd)) { Ok(0) } else { Err(Errno::BadF) }))
}
unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    // Each write crossed when made: nothing is held back to sync.
    c(|layer| int!(layer,if layer.sys.is_open(Fd(fd)) { Ok(0) } else { Err(Errno::BadF) }))
}
unsafe extern "C" fn ftruncate(fd: c_int, len: i64) -> c_int {
    c(|layer| {
        if len < 0 {
            return int!(layer,Err(Errno::Inval));
        }
        let ret = ask(layer, |sys| sys.ftruncate(Fd(fd), len as u64));
        int!(layer,as_int(ret))
    })
}

/// `fcntl`: variadic, because its third argument is on the stack on macOS arm64.
unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut args: ...) -> c_int {
    c(|layer| {
        let os = os(layer);
        let result = match cmd {
            // F_DUPFD, F_DUPFD_CLOEXEC.
            0 => layer.sys.dup(Fd(fd), unsafe { args.next_arg::<c_int>() }).map(|fd| fd.0 as i64),
            _ if cmd == consts(os).f_dupfd_cloexec => layer.sys.dup(Fd(fd), unsafe { args.next_arg::<c_int>() }).map(|fd| fd.0 as i64),
            // F_GETFD: every descriptor is close-on-exec (nothing is executed); F_SETFD,
            // F_GETFL, F_SETFL: accepted, no flag changes an answer here; F_FULLFSYNC (51,
            // macOS): each write crossed already.
            1 => Ok(1),
            2 | 3 | 4 => Ok(0),
            51 if os == Os::Mac => Ok(0),
            // F_GETPATH (50, macOS): the path the descriptor was opened by, into a
            // `MAXPATHLEN` (1024) buffer.
            50 if os == Os::Mac => {
                let buf = unsafe { args.next_arg::<*mut c_char>() };
                match layer.sys.path_of(Fd(fd)) {
                    Ok(path) if path.len() < 1024 => {
                        // SAFETY: a MAXPATHLEN buffer, as F_GETPATH's contract says.
                        unsafe {
                            core::ptr::copy_nonoverlapping(path.as_ptr(), buf.cast::<u8>(), path.len());
                            *buf.add(path.len()) = 0;
                        }
                        Ok(0)
                    }
                    Ok(_) => Err(Errno::NameTooLong),
                    Err(errno) => Err(errno),
                }
            }
            _ => Err(Errno::Inval),
        };
        int!(layer,if layer.sys.is_open(Fd(fd)) { result } else { Err(Errno::BadF) })
    })
}

unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong_t) -> c_int {
    c(|layer| {
        let fioclex = if os(layer) == Os::Mac { 0x2000_6601 } else { 0x5451 };
        let result = if !layer.sys.is_open(Fd(fd)) {
            Err(Errno::BadF)
        } else if request == fioclex {
            Ok(0)
        } else {
            // TIOCGWINSZ and the rest: nothing here is a terminal.
            Err(Errno::NotTy)
        };
        int!(layer,result)
    })
}

#[allow(non_camel_case_types)]
type c_ulong_t = u64;

// ---- lookups -------------------------------------------------------------------------------

fn write_stat(os: Os, st: *mut u8, out: StatOut) {
    let mode: u32 = match out.kind {
        Kind::File => 0o100_444,
        Kind::Dir => 0o040_555,
        Kind::Char => 0o020_666,
        Kind::Fifo => 0o010_600,
    };
    let blocks = out.len.div_ceil(512) as i64;
    // SAFETY: `st` is the caller's `struct stat` of this system (144 bytes on macOS arm64,
    // 128 on Linux arm64), zeroed first so every time is 0 (rule 25: no host time).
    unsafe {
        match os {
            Os::Mac => {
                core::ptr::write_bytes(st, 0, 144);
                st.add(4).cast::<u16>().write_unaligned(mode as u16);
                st.add(6).cast::<u16>().write_unaligned(1);
                st.add(8).cast::<u64>().write_unaligned(out.ino);
                st.add(16).cast::<u32>().write_unaligned(1000);
                st.add(20).cast::<u32>().write_unaligned(1000);
                st.add(96).cast::<i64>().write_unaligned(out.len as i64);
                st.add(104).cast::<i64>().write_unaligned(blocks);
                st.add(112).cast::<i32>().write_unaligned(4096);
            }
            Os::Linux => {
                core::ptr::write_bytes(st, 0, 128);
                st.add(8).cast::<u64>().write_unaligned(out.ino);
                st.add(16).cast::<u32>().write_unaligned(mode);
                st.add(20).cast::<u32>().write_unaligned(1);
                st.add(24).cast::<u32>().write_unaligned(1000);
                st.add(28).cast::<u32>().write_unaligned(1000);
                st.add(48).cast::<i64>().write_unaligned(out.len as i64);
                st.add(56).cast::<i32>().write_unaligned(4096);
                st.add(64).cast::<i64>().write_unaligned(blocks);
            }
        }
    }
}

unsafe extern "C" fn c_fstat(fd: c_int, st: *mut c_void) -> c_int {
    c(|layer| match layer.sys.fstat(Fd(fd)) {
        Ok(stat) => {
            write_stat(os(layer), st.cast(), stat);
            0
        }
        Err(errno) => int!(layer,Err(errno)),
    })
}
unsafe extern "C" fn c_stat(path: *const c_char, st: *mut c_void) -> c_int {
    // SAFETY: as fstatat.
    unsafe { fstatat(c(|layer| consts(os(layer)).at_fdcwd), path, st, 0) }
}
const LINUX_AT_EMPTY_PATH: c_int = 0x1000;

unsafe extern "C" fn fstatat(at: c_int, path: *const c_char, st: *mut c_void, flags: c_int) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let os = os(layer);
        // `AT_EMPTY_PATH` (Linux) with an empty path: the descriptor itself (or cwd for AT_FDCWD).
        if path.is_empty() && os == Os::Linux && flags & LINUX_AT_EMPTY_PATH != 0 {
            if at >= 0 {
                return match layer.sys.fstat(Fd(at)) {
                    Ok(stat) => {
                        write_stat(os, st.cast(), stat);
                        0
                    }
                    Err(errno) => int!(layer, Err(errno)),
                };
            }
            if at == consts(os).at_fdcwd {
                return match ask(layer, |sys| sys.stat(at, b".")) {
                    Ok(Ret::Stat(stat)) => {
                        write_stat(os, st.cast(), stat);
                        0
                    }
                    Ok(_) => int!(layer, Err(Errno::Io)),
                    Err(errno) => int!(layer, Err(errno)),
                };
            }
        }
        match ask(layer, |sys| sys.stat(at, path)) {
            Ok(Ret::Stat(stat)) => {
                write_stat(os, st.cast(), stat);
                0
            }
            Ok(_) => int!(layer,Err(Errno::Io)),
            Err(errno) => int!(layer,Err(errno)),
        }
    })
}
/// `statx`: `ENOSYS`, as a kernel before 4.11 answers; std then calls `fstatat64`.
unsafe extern "C" fn statx(_: c_int, _: *const c_char, _: c_int, _: c_uint, _: *mut c_void) -> c_int {
    c(|layer| int!(layer,Err(Errno::NoSys)))
}
unsafe extern "C" fn c_chdir(path: *const c_char) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.chdir(path));
        int!(layer,as_int(ret))
    })
}
unsafe extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    c(|layer| {
        let cwd = layer.sys.cwd();
        if buf.is_null() || cwd.len() + 1 > size {
            set_errno(layer, Errno::Range);
            return core::ptr::null_mut();
        }
        // SAFETY: `size` writable bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf.cast::<u8>(), cwd.len());
            *buf.add(cwd.len()) = 0;
        }
        buf
    })
}
unsafe extern "C" fn c_readlink(path: *const c_char, _: *mut c_char, _: usize) -> isize {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let at = consts(os(layer)).at_fdcwd;
        let ret = ask(layer, |sys| sys.readlink(at, path));
        long!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| match ask(layer, |sys| sys.realpath(path)) {
        Ok(_) => {
            let got = layer.sys.scratch();
            // A NULL `resolved` asks the C library to allocate: refused here, as the layer
            // allocates nothing the code must free (std passes NULL only on old systems).
            if resolved.is_null() || got.len() >= 4096 {
                set_errno(layer, Errno::NameTooLong);
                return core::ptr::null_mut();
            }
            // SAFETY: a PATH_MAX buffer, as realpath's contract says.
            unsafe {
                core::ptr::copy_nonoverlapping(got.as_ptr(), resolved.cast::<u8>(), got.len());
                *resolved.add(got.len()) = 0;
            }
            resolved
        }
        Err(errno) => {
            set_errno(layer, errno);
            core::ptr::null_mut()
        }
    })
}

/// A `DIR *` is the descriptor plus one: never dereferenced by a caller (it is opaque).
fn dir_ptr(fd: i64) -> *mut c_void {
    (fd as usize + 1) as *mut c_void
}
fn dir_fd(dir: *mut c_void) -> Fd {
    Fd((dir as usize).wrapping_sub(1) as i32)
}

unsafe extern "C" fn c_opendir(path: *const c_char) -> *mut c_void {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| match as_int(ask(layer, |sys| sys.opendir(path))) {
        Ok(fd) => dir_ptr(fd),
        Err(errno) => {
            set_errno(layer, errno);
            core::ptr::null_mut()
        }
    })
}
unsafe extern "C" fn fdopendir(fd: c_int) -> *mut c_void {
    c(|layer| match as_int(ask(layer, |sys| sys.fdopendir(Fd(fd)))) {
        Ok(fd) => dir_ptr(fd),
        Err(errno) => {
            set_errno(layer, errno);
            core::ptr::null_mut()
        }
    })
}
unsafe extern "C" fn closedir(dir: *mut c_void) -> c_int {
    c(|layer| int!(layer,layer.sys.close(dir_fd(dir)).map(|()| 0)))
}
unsafe extern "C" fn dirfd(dir: *mut c_void) -> c_int {
    dir_fd(dir).0
}
unsafe extern "C" fn rewinddir(dir: *mut c_void) {
    c(|layer| {
        let _ = layer.sys.rewinddir(dir_fd(dir));
    })
}

/// Lays `entry` out as this system's `struct dirent` at `to` (macOS: 1048 bytes, the name at
/// 21; Linux `dirent64`: 280 bytes, the name at 19); `d_type` is `DT_UNKNOWN`.
fn write_dirent(os: Os, to: *mut u8, entry: DirEnt<'_>) {
    let name = &entry.name[..entry.name.len() - 1];
    // SAFETY: `to` is a `struct dirent` of this system; a name is at most 255 bytes on Linux
    // and 1023 on macOS, which the host's listing respects (wire `most_name`), and longer
    // ones are cut by `min` below.
    unsafe {
        match os {
            Os::Mac => {
                let len = name.len().min(1023);
                core::ptr::write_bytes(to, 0, 1048);
                to.cast::<u64>().write_unaligned(entry.ino);
                to.add(8).cast::<u64>().write_unaligned(entry.at);
                to.add(16).cast::<u16>().write_unaligned(1048);
                to.add(18).cast::<u16>().write_unaligned(len as u16);
                core::ptr::copy_nonoverlapping(name.as_ptr(), to.add(21), len);
            }
            Os::Linux => {
                let len = name.len().min(255);
                core::ptr::write_bytes(to, 0, 280);
                to.cast::<u64>().write_unaligned(entry.ino);
                to.add(8).cast::<i64>().write_unaligned(entry.at as i64 + 1);
                to.add(16).cast::<u16>().write_unaligned(280);
                core::ptr::copy_nonoverlapping(name.as_ptr(), to.add(19), len);
            }
        }
    }
}

unsafe extern "C" fn c_readdir(dir: *mut c_void) -> *mut c_void {
    c(|layer| {
        let os = os(layer);
        let to = layer.dirent.as_mut_ptr().cast::<u8>();
        match layer.sys.readdir(dir_fd(dir)) {
            Ok(Some(entry)) => {
                write_dirent(os, to, entry);
                to.cast()
            }
            // The end: NULL with errno unchanged.
            Ok(None) => core::ptr::null_mut(),
            Err(errno) => {
                set_errno(layer, errno);
                core::ptr::null_mut()
            }
        }
    })
}
unsafe extern "C" fn readdir_r(dir: *mut c_void, entry: *mut c_void, result: *mut *mut c_void) -> c_int {
    c(|layer| {
        let os = os(layer);
        match layer.sys.readdir(dir_fd(dir)) {
            Ok(Some(found)) => {
                write_dirent(os, entry.cast(), found);
                // SAFETY: the caller's out pointer.
                unsafe { *result = entry };
                0
            }
            Ok(None) => {
                // SAFETY: as above.
                unsafe { *result = core::ptr::null_mut() };
                0
            }
            Err(errno) => errno.code(os),
        }
    })
}

unsafe extern "C" fn c_mkdir(path: *const c_char, _: c_uint) -> c_int {
    // SAFETY: as mkdirat.
    unsafe { mkdirat(c(|layer| consts(os(layer)).at_fdcwd), path, 0) }
}
unsafe extern "C" fn mkdirat(at: c_int, path: *const c_char, _: c_uint) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.create_dir(at, path));
        int!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_unlink(path: *const c_char) -> c_int {
    // SAFETY: as unlinkat.
    unsafe { unlinkat(c(|layer| consts(os(layer)).at_fdcwd), path, 0) }
}
unsafe extern "C" fn c_rmdir(path: *const c_char) -> c_int {
    // SAFETY: as unlinkat.
    unsafe { unlinkat(c(|layer| consts(os(layer)).at_fdcwd), path, c(|layer| consts(os(layer)).at_removedir)) }
}
/// `unlink` and `rmdir` are one host call: the host's output root knows a file from a
/// directory and answers `EISDIR`/`ENOTDIR`/`ENOTEMPTY` as the real system does.
unsafe extern "C" fn unlinkat(at: c_int, path: *const c_char, _: c_int) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.remove(at, path));
        int!(layer,as_int(ret))
    })
}
unsafe extern "C" fn c_rename(from: *const c_char, to: *const c_char) -> c_int {
    // SAFETY: as renameat.
    let at = c(|layer| consts(os(layer)).at_fdcwd);
    unsafe { renameat(at, from, at, to) }
}
unsafe extern "C" fn renamex_np(from: *const c_char, to: *const c_char, _: c_uint) -> c_int {
    // SAFETY: as c_rename.
    unsafe { c_rename(from, to) }
}
unsafe extern "C" fn renameat(from_at: c_int, from: *const c_char, to_at: c_int, to: *const c_char) -> c_int {
    // SAFETY: C strings.
    let (from, to) = unsafe { (bytes(from), bytes(to)) };
    c(|layer| {
        let ret = ask(layer, |sys| sys.rename(from_at, from, to_at, to));
        int!(layer,as_int(ret))
    })
}

/// `fclonefileat`: `ENOTSUP`, as on a volume that cannot clone; std then calls `fcopyfile`.
unsafe extern "C" fn fclonefileat(_: c_int, _: c_int, _: *const c_char, _: c_uint) -> c_int {
    c(|layer| int!(layer,Err(Errno::NotSup)))
}
/// A copy state is the layer's one word holding the bytes copied (std makes one per copy and
/// frees it before the next).
unsafe extern "C" fn copyfile_state_alloc() -> *mut c_void {
    c(|layer| {
        layer.copied = 0;
        core::ptr::addr_of_mut!(layer.copied).cast()
    })
}
unsafe extern "C" fn copyfile_state_get(state: *mut c_void, _: c_uint, to: *mut c_void) -> c_int {
    // SAFETY: the state from copyfile_state_alloc; `to` an `off_t` out pointer (the one flag
    // std asks, COPYFILE_STATE_COPIED).
    unsafe { to.cast::<u64>().write_unaligned(state.cast::<u64>().read_unaligned()) };
    0
}
/// `fcopyfile(from, to, state, flags)`: the source's held bytes written to the target, one
/// `WriteFile`.
unsafe extern "C" fn fcopyfile(from: c_int, to: c_int, state: *mut c_void, _: c_uint) -> c_int {
    c(|layer| {
        let len = match layer.sys.fstat(Fd(from)) {
            Ok(stat) => stat.len as usize,
            Err(errno) => return int!(layer,Err(errno)),
        };
        layer.gather.clear();
        layer.gather.resize(len, 0);
        let read = layer.sys.read(Fd(from), &mut layer.gather, Some(0));
        let ret = match read {
            Ok(n) => {
                layer.gather.truncate(n);
                let started_write = layer.sys.write(Fd(to), &layer.gather, Some(0));
                as_int(started(layer, started_write))
            }
            Err(errno) => Err(errno),
        };
        if let (Ok(n), false) = (ret, state.is_null()) {
            // SAFETY: the state word copyfile_state_alloc gave.
            unsafe { state.cast::<u64>().write_unaligned(n as u64) };
        }
        int!(layer,ret.map(|_| 0))
    })
}

// ---- the environment, arguments, the program's own path ---------------------------------

unsafe extern "C" fn c_getenv(name: *const c_char) -> *const c_char {
    // SAFETY: a C string.
    let name = unsafe { bytes(name) };
    c(|layer| match ask(layer, |sys| sys.getenv(name)) {
        Ok(Ret::Env(Some(at))) => layer.sys.env_value(at).map_or(core::ptr::null(), |value| value.as_ptr().cast()),
        _ => core::ptr::null(),
    })
}
unsafe extern "C" fn c_setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int {
    // SAFETY: C strings.
    let (name, value) = unsafe { (bytes(name), bytes(value)) };
    c(|layer| {
        if overwrite == 0 && layer.sys.env_needs_host_check(name) {
            let _ = ask(layer, |sys| sys.getenv(name));
        }
        int!(layer, layer.sys.setenv(name, Some(value), overwrite != 0).map(|()| 0))
    })
}
unsafe extern "C" fn c_unsetenv(name: *const c_char) -> c_int {
    // SAFETY: a C string.
    let name = unsafe { bytes(name) };
    c(|layer| int!(layer,layer.sys.setenv(name, None, true).map(|()| 0)))
}
unsafe extern "C" fn ns_argc() -> *mut c_int {
    c(|layer| core::ptr::addr_of_mut!(layer.argc))
}
unsafe extern "C" fn ns_argv() -> *mut *const *const c_char {
    // The cell holding `argv`, which lives as long as the layer.
    c(|layer| core::ptr::addr_of_mut!(layer.argv_word))
}
unsafe extern "C" fn ns_environ() -> *mut *const *const c_char {
    c(|layer| {
        build_environ(layer);
        core::ptr::addr_of_mut!(layer.environ_word)
    })
}
/// The program's path as the run sees it: one fixed spelling, never the host's.
unsafe extern "C" fn ns_exe(buf: *mut c_char, size: *mut u32) -> c_int {
    const EXE: &[u8] = b"/comptime/program\0";
    // SAFETY: the caller's buffer of `*size` bytes.
    unsafe {
        if (*size as usize) < EXE.len() {
            *size = EXE.len() as u32;
            return -1;
        }
        core::ptr::copy_nonoverlapping(EXE.as_ptr(), buf.cast::<u8>(), EXE.len());
    }
    0
}
unsafe extern "C" fn gethostname(buf: *mut c_char, len: usize) -> c_int {
    const NAME: &[u8] = b"blaze\0";
    c(|layer| {
        if len < NAME.len() {
            return int!(layer,Err(Errno::NameTooLong));
        }
        // SAFETY: `len` writable bytes.
        unsafe { core::ptr::copy_nonoverlapping(NAME.as_ptr(), buf.cast::<u8>(), NAME.len()) };
        0
    })
}
/// No account database: found nothing (std's `home_dir` then reads `HOME`, a recorded
/// variable).
unsafe extern "C" fn getpwuid_r(_: c_uint, _: *mut c_void, _: *mut c_char, _: usize, result: *mut *mut c_void) -> c_int {
    // SAFETY: the caller's out pointer.
    unsafe { *result = core::ptr::null_mut() };
    0
}
unsafe extern "C" fn confstr(_: c_int, _: *mut c_char, _: usize) -> usize {
    // `_CS_DARWIN_USER_TEMP_DIR` and the rest: none; std's `temp_dir` then reads `TMPDIR`.
    c(|layer| {
        set_errno(layer, Errno::Inval);
        0
    })
}

// ---- time and chance ------------------------------------------------------------------------

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

fn nanos(layer: &mut Layer, id: c_int) -> Result<u64, Errno> {
    match ask(layer, |sys| sys.clock(id))? {
        Ret::Nanos(ns) => Ok(ns),
        _ => Err(Errno::Io),
    }
}

unsafe extern "C" fn clock_gettime(id: c_int, ts: *mut Timespec) -> c_int {
    c(|layer| match nanos(layer, id) {
        Ok(ns) => {
            // SAFETY: the caller's timespec.
            unsafe { ts.write(Timespec { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as i64 }) };
            0
        }
        Err(errno) => int!(layer,Err(errno)),
    })
}
unsafe extern "C" fn clock_getres(_: c_int, ts: *mut Timespec) -> c_int {
    if !ts.is_null() {
        // SAFETY: the caller's timespec.
        unsafe { ts.write(Timespec { sec: 0, nsec: 1 }) };
    }
    0
}
unsafe extern "C" fn gettimeofday(tv: *mut c_void, _: *mut c_void) -> c_int {
    c(|layer| {
        let realtime = consts(os(layer)).clock_realtime;
        match nanos(layer, realtime) {
            Ok(ns) => {
                let (sec, usec) = ((ns / 1_000_000_000) as i64, ((ns % 1_000_000_000) / 1000) as i64);
                // SAFETY: the caller's timeval: `tv_usec` is an i32 on macOS, an i64 on Linux.
                unsafe {
                    tv.cast::<i64>().write_unaligned(sec);
                    match os(layer) {
                        Os::Mac => tv.cast::<u8>().add(8).cast::<i32>().write_unaligned(usec as i32),
                        Os::Linux => tv.cast::<u8>().add(8).cast::<i64>().write_unaligned(usec),
                    }
                }
                0
            }
            Err(errno) => int!(layer,Err(errno)),
        }
    })
}
unsafe extern "C" fn mach_absolute_time() -> u64 {
    c(|layer| match nanos(layer, 6) {
        Ok(ns) => ns,
        Err(_) => 0,
    })
}
unsafe extern "C" fn mach_timebase_info(info: *mut u32) -> c_int {
    // SAFETY: the caller's `mach_timebase_info_data_t` (numer, denom).
    unsafe {
        info.write(1);
        info.add(1).write(1);
    }
    0
}
unsafe extern "C" fn mach_wait_until(deadline: u64) -> c_int {
    c(|layer| {
        let _ = ask(layer, |sys| sys.wait_until(deadline));
        0
    })
}
unsafe extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int {
    // SAFETY: the caller's timespecs.
    let req = unsafe { req.read() };
    c(|layer| {
        if req.sec < 0 || !(0..1_000_000_000).contains(&req.nsec) {
            return int!(layer,Err(Errno::Inval));
        }
        let ns = (req.sec as u64).saturating_mul(1_000_000_000).saturating_add(req.nsec as u64);
        let ret = as_int(ask(layer, |sys| sys.sleep(ns)));
        if !rem.is_null() {
            // SAFETY: as above.
            unsafe { rem.write(Timespec { sec: 0, nsec: 0 }) };
        }
        int!(layer,ret)
    })
}
unsafe extern "C" fn clock_nanosleep(_: c_int, flags: c_int, req: *const Timespec, _: *mut Timespec) -> c_int {
    // SAFETY: the caller's timespec.
    let req = unsafe { req.read() };
    c(|layer| {
        let ns = (req.sec.max(0) as u64).saturating_mul(1_000_000_000).saturating_add(req.nsec.max(0) as u64);
        // TIMER_ABSTIME (1): a deadline on the monotonic clock.
        let ret = if flags & 1 != 0 { ask(layer, |sys| sys.wait_until(ns)) } else { ask(layer, |sys| sys.sleep(ns)) };
        // Returns the error, not -1.
        match ret {
            Ok(_) => 0,
            Err(errno) => errno.code(os(layer)),
        }
    })
}

fn random_into(layer: &mut Layer, buf: *mut c_void, len: usize) -> Result<(), Errno> {
    let len32 = u32::try_from(len).map_err(|_| Errno::Inval)?;
    ask(layer, |sys| sys.random(len32))?;
    let got = layer.sys.scratch();
    if got.len() != len {
        return Err(Errno::Io);
    }
    // SAFETY: `len` writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(got.as_ptr(), buf.cast::<u8>(), len) };
    Ok(())
}

unsafe extern "C" fn getentropy(buf: *mut c_void, len: usize) -> c_int {
    c(|layer| int!(layer,if len > 256 { Err(Errno::Io) } else { random_into(layer, buf, len).map(|()| 0) }))
}
unsafe extern "C" fn getrandom(buf: *mut c_void, len: usize, _: c_uint) -> isize {
    c(|layer| long!(layer,random_into(layer, buf, len).map(|()| len as i64)))
}
unsafe extern "C" fn arc4random_buf(buf: *mut c_void, len: usize) {
    c(|layer| {
        if random_into(layer, buf, len).is_err() {
            // `arc4random_buf` cannot fail: a host that will not answer ends the run.
            layer.transport.fault(ChildFault::Unanswerable { name: "arc4random_buf" });
        }
    })
}
unsafe extern "C" fn cc_random(buf: *mut c_void, len: usize) -> c_int {
    // kCCSuccess 0; kCCRNGFailure -4307.
    c(|layer| if random_into(layer, buf, len).is_ok() { 0 } else { -4307 })
}

// ---- spawned commands ---------------------------------------------------------------------

unsafe extern "C" fn c_pipe(fds: *mut c_int) -> c_int {
    c(|layer| match layer.sys.pipe() {
        Ok((read, write)) => {
            // SAFETY: the caller's two ints.
            unsafe {
                fds.write(read.0);
                fds.add(1).write(write.0);
            }
            0
        }
        Err(errno) => int!(layer,Err(errno)),
    })
}
unsafe extern "C" fn c_pipe2(fds: *mut c_int, _: c_int) -> c_int {
    // SAFETY: as c_pipe.
    unsafe { c_pipe(fds) }
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

unsafe extern "C" fn c_poll(fds: *mut PollFd, count: u32, timeout: c_int) -> c_int {
    // SAFETY: the caller's `count` pollfds.
    let fds = unsafe { core::slice::from_raw_parts_mut(fds, count as usize) };
    c(|layer| {
        let mut ready = 0;
        for one in fds.iter_mut() {
            one.revents = if one.fd < 0 { 0 } else { layer.sys.poll_one(Fd(one.fd), one.events) };
            ready += (one.revents != 0) as c_int;
        }
        if ready > 0 || timeout == 0 {
            return ready;
        }
        if timeout < 0 {
            // Nothing will ever be ready: one thread, no outside writer.
            layer.transport.fault(ChildFault::Sys(Fault::Deadlock));
        }
        let _ = ask(layer, |sys| sys.sleep((timeout as u64).saturating_mul(1_000_000)));
        0
    })
}

/// A file actions object holds the layer's handle plus one in its first word.
unsafe extern "C" fn fa_init(actions: *mut u64) -> c_int {
    c(|layer| {
        let handle = layer.sys.actions_new();
        // SAFETY: the caller's `posix_spawn_file_actions_t` (a pointer on macOS, 80 bytes on
        // Linux): its first word is ours.
        unsafe { actions.write_unaligned(handle as u64 + 1) };
        0
    })
}
fn handle(actions: *const u64) -> Option<u32> {
    if actions.is_null() {
        return None;
    }
    // SAFETY: an object fa_init filled.
    let word = unsafe { actions.read_unaligned() };
    word.checked_sub(1).map(|h| h as u32)
}
unsafe extern "C" fn fa_dup2(actions: *mut u64, fd: c_int, to: c_int) -> c_int {
    c(|layer| match handle(actions) {
        Some(h) => match layer.sys.actions_dup2(h, fd, to) {
            Ok(()) => 0,
            Err(errno) => errno.code(os(layer)),
        },
        None => Errno::Inval.code(os(layer)),
    })
}
unsafe extern "C" fn fa_chdir(actions: *mut u64, path: *const c_char) -> c_int {
    // SAFETY: a C string.
    let path = unsafe { bytes(path) };
    c(|layer| match handle(actions) {
        Some(h) => match layer.sys.actions_chdir(h, path) {
            Ok(()) => 0,
            Err(errno) => errno.code(os(layer)),
        },
        None => Errno::Inval.code(os(layer)),
    })
}
unsafe extern "C" fn fa_destroy(actions: *mut u64) -> c_int {
    c(|layer| match handle(actions) {
        Some(h) => {
            // SAFETY: the caller's `posix_spawn_file_actions_t`, cleared so double-destroy fails.
            unsafe { actions.write_unaligned(0) };
            match layer.sys.actions_destroy(h) {
                Ok(()) => 0,
                Err(errno) => errno.code(os(layer)),
            }
        }
        None => Errno::Inval.code(os(layer)),
    })
}

/// Joins a NULL-ended array of C strings (from `skip`) as NUL-ended words into `into`.
///
/// SAFETY: `list` is NULL or a NULL-ended array of C strings.
unsafe fn join(list: *const *const c_char, skip: usize, into: &mut AVec<'static, u8>) {
    into.clear();
    if list.is_null() {
        return;
    }
    let mut at = 0;
    loop {
        // SAFETY: as the function says.
        let one = unsafe { *list.add(at) };
        if one.is_null() {
            break;
        }
        if at >= skip {
            // SAFETY: a C string.
            into.extend_from_slice(unsafe { bytes(one) });
            into.push(0);
        }
        at += 1;
    }
}

/// `posix_spawn` and `posix_spawnp`: one `Spawn`; the error is returned, not set.
unsafe extern "C" fn posix_spawn(
    pid: *mut c_int,
    program: *const c_char,
    actions: *const u64,
    _: *const c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    // SAFETY: a C string.
    let program = unsafe { bytes(program) };
    c(|layer| {
        // SAFETY: NULL-ended arrays, as posix_spawn's contract says.
        unsafe {
            join(argv, 1, &mut layer.words);
            join(envp, 0, &mut layer.envp);
        }
        let h = handle(actions);
        let started_spawn = layer.sys.spawn(program, &layer.words, &layer.envp, h);
        let ret = as_int(started(layer, started_spawn));
        match ret {
            Ok(child) => {
                if !pid.is_null() {
                    // SAFETY: the caller's out pointer.
                    unsafe { pid.write(child as c_int) };
                }
                0
            }
            Err(errno) => errno.code(os(layer)),
        }
    })
}
unsafe extern "C" fn waitpid(pid: c_int, status: *mut c_int, _: c_int) -> c_int {
    c(|layer| match layer.sys.waitpid(pid) {
        Ok((child, encoded)) => {
            if !status.is_null() {
                // SAFETY: the caller's out pointer.
                unsafe { status.write(encoded) };
            }
            child
        }
        Err(errno) => int!(layer,Err(errno)),
    })
}
/// `waitid`: std uses it with pidfds only, which this layer has none of.
unsafe extern "C" fn waitid(_: c_int, _: c_uint, _: *mut c_void, _: c_int) -> c_int {
    c(|layer| int!(layer,Err(Errno::Inval)))
}
/// `kill`: signal 0 to the run itself exists; every command already ended (`ESRCH`).
unsafe extern "C" fn kill(pid: c_int, signal: c_int) -> c_int {
    c(|layer| int!(layer,if pid == 2 && signal == 0 { Ok(0) } else { Err(Errno::Srch) }))
}

// ---- threads, signals, sockets ------------------------------------------------------------

unsafe extern "C" fn pthread_create(_: *mut c_void, _: *const c_void, _: *const c_void, _: *mut c_void) -> c_int {
    c(|layer| match ask(layer, |sys| sys.thread()) {
        Ok(_) => Errno::Again.code(os(layer)),
        Err(errno) => errno.code(os(layer)),
    })
}
/// The launcher keeps its own handlers (its watchdog, its fault report): a handler the code
/// installs is accepted and never called; the old action read back is the default.
unsafe extern "C" fn sigaction(_: c_int, _: *const c_void, old: *mut u8) -> c_int {
    c(|layer| {
        if !old.is_null() {
            let size = if os(layer) == Os::Mac { 16 } else { 152 };
            // SAFETY: the caller's `struct sigaction` of this system.
            unsafe { core::ptr::write_bytes(old, 0, size) };
        }
        0
    })
}
unsafe extern "C" fn signal(_: c_int, _: usize) -> usize {
    0
}
/// Writes `SS_DISABLE` into a `stack_t` (24 bytes on both macOS arm64 and Linux arm64):
/// - macOS arm64: `{ ss_sp: 0..8, ss_size: 8..16, ss_flags: 16..20 }` (`SS_DISABLE = 4`)
/// - Linux arm64: `{ ss_sp: 0..8, ss_flags: 8..12, ss_size: 16..24 }` (`SS_DISABLE = 2`)
///
/// SAFETY: `old` points to 24 writable bytes of the caller's `stack_t`.
unsafe fn write_sigaltstack_old(os: Os, old: *mut u8) {
    // SAFETY: caller guarantees 24 writable bytes at `old`.
    unsafe {
        core::ptr::write_bytes(old, 0, 24);
        let (offset, disable) = match os {
            Os::Mac => (16, 4),
            Os::Linux => (8, 2),
        };
        old.add(offset).cast::<c_int>().write_unaligned(disable);
    }
}

unsafe extern "C" fn sigaltstack(_: *const c_void, old: *mut u8) -> c_int {
    c(|layer| {
        if !old.is_null() {
            // SAFETY: the caller's `stack_t` (24 bytes on both).
            unsafe { write_sigaltstack_old(os(layer), old) };
        }
        0
    })
}
unsafe extern "C" fn c_socket(_: c_int, _: c_int, _: c_int) -> c_int {
    c(|layer| {
        let ret = ask(layer, |sys| sys.socket());
        int!(layer,ret.and(Err(Errno::Perm)))
    })
}
unsafe extern "C" fn getaddrinfo(_: *const c_char, _: *const c_char, _: *const c_void, res: *mut *mut c_void) -> c_int {
    c(|layer| {
        let _ = ask(layer, |sys| sys.socket());
        if !res.is_null() {
            // SAFETY: the caller's out pointer.
            unsafe { *res = core::ptr::null_mut() };
        }
        // EAI_FAIL.
        if os(layer) == Os::Mac { 4 } else { -4 }
    })
}
/// A socket call on a descriptor: none of the run's is a socket.
unsafe extern "C" fn sock_fd(fd: c_int) -> c_int {
    c(|layer| int!(layer,Err(if layer.sys.is_open(Fd(fd)) { Errno::NotSock } else { Errno::BadF })))
}
unsafe extern "C" fn sock_fd_l(fd: c_int) -> isize {
    c(|layer| long!(layer,Err(if layer.sys.is_open(Fd(fd)) { Errno::NotSock } else { Errno::BadF })))
}

// ---- memory -------------------------------------------------------------------------------

unsafe extern "C" fn c_mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void {
    c(|layer| match mmap_allowed(os(layer), prot, flags, fd) {
        // SAFETY: the real `mmap`, with arguments the check admitted (anonymous, private, no
        // fixed address, no execution).
        Ok(()) => unsafe { (layer.real.mmap)(addr, len, prot, flags, fd, off) },
        Err(errno) => {
            let _ = ask(layer, |sys| sys.denied(b"mmap", errno));
            set_errno(layer, errno);
            usize::MAX as *mut c_void
        }
    })
}
unsafe extern "C" fn c_mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int {
    c(|layer| match mprotect_allowed(prot) {
        // SAFETY: the real `mprotect`, without PROT_EXEC.
        Ok(()) => unsafe { (layer.real.mprotect)(addr, len, prot) },
        Err(errno) => {
            let _ = ask(layer, |sys| sys.denied(b"mprotect", errno));
            int!(layer,Err(errno))
        }
    })
}

// ---- the machine's own facts ---------------------------------------------------------------

/// One processor, the real page size (`Limits::page`), no other limit known.
unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    c(|layer| {
        let k = consts(os(layer));
        if name == k.sc_pagesize {
            layer_page(layer) as c_long
        } else if name == k.sc_nprocessors_conf || name == k.sc_nprocessors_onln {
            1
        } else {
            set_errno(layer, Errno::Inval);
            -1
        }
    })
}
fn layer_page(layer: &Layer) -> u64 {
    layer.sys.limits().page
}
/// One processor: CPU 0 set in the caller's mask.
unsafe extern "C" fn sched_getaffinity(_: c_int, size: usize, mask: *mut u8) -> c_int {
    c(|layer| {
        if size == 0 {
            return int!(layer,Err(Errno::Inval));
        }
        // SAFETY: the caller's `size`-byte mask.
        unsafe {
            core::ptr::write_bytes(mask, 0, size);
            *mask = 1;
        }
        0
    })
}
/// `sysctlbyname`: nothing is known (std_detect's `hw.optional.*` then reads as absent, so
/// every run of one system takes the baseline branch).
unsafe extern "C" fn sysctlbyname(_: *const c_char, _: *mut c_void, _: *mut usize, _: *mut c_void, _: usize) -> c_int {
    c(|layer| int!(layer,Err(Errno::NoEnt)))
}
/// `getauxval`: the page size; the hardware capabilities 0 (the baseline, as above).
unsafe extern "C" fn getauxval(kind: u64) -> u64 {
    c(|layer| match kind {
        6 => layer_page(layer),
        16 | 26 => 0,
        _ => {
            set_errno(layer, Errno::NoEnt);
            0
        }
    })
}
/// `prctl(PR_SET_NAME)` (std names threads) is accepted; the rest are `EINVAL`.
unsafe extern "C" fn prctl(option: c_int, _: u64, _: u64, _: u64, _: u64) -> c_int {
    c(|layer| int!(layer,if option == 15 { Ok(0) } else { Err(Errno::Inval) }))
}
/// `syscall` on Linux arm64, by number: `futex` (98) for std's locks on the one thread,
/// `getrandom` (278), `gettid` (178); every other number `ENOSYS` (std's `statx`,
/// `copy_file_range`, `pidfd_*` then take their documented older branch).
unsafe extern "C" fn syscall(number: c_long, a1: u64, a2: u64, a3: u64, a4: u64, _: u64) -> c_long {
    c(|layer| match number {
        98 => {
            let op = a2 & !128;
            match op {
                // FUTEX_WAIT, FUTEX_WAIT_BITSET: a wait no thread can end, unless timed.
                0 | 9 => {
                    // SAFETY: the futex word the caller passed.
                    let now = unsafe { (a1 as *const u32).read_volatile() };
                    if now != a3 as u32 {
                        return long!(layer,Err(Errno::Again)) as c_long;
                    }
                    if a4 == 0 {
                        layer.transport.fault(ChildFault::Sys(Fault::Deadlock));
                    }
                    // SAFETY: the caller's timespec.
                    let ts = unsafe { (a4 as *const Timespec).read() };
                    let ns = futex_timeout_ns(ts);
                    let _ = ask(layer, |sys| sys.sleep(ns));
                    // ETIMEDOUT (110).
                    // SAFETY: `Real::errno`.
                    unsafe { *(layer.real.errno)() = 110 };
                    -1
                }
                // FUTEX_WAKE, FUTEX_WAKE_BITSET: nobody waits.
                1 | 10 => 0,
                _ => long!(layer,Err(Errno::NoSys)) as c_long,
            }
        }
        278 => long!(layer,random_into(layer, a1 as *mut c_void, a2 as usize).map(|()| a2 as i64)) as c_long,
        178 => 2,
        _ => long!(layer,Err(Errno::NoSys)) as c_long,
    })
}

fn futex_timeout_ns(ts: Timespec) -> u64 {
    (ts.sec.max(0) as u64).saturating_mul(1_000_000_000).saturating_add(ts.nsec.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::{Timespec, futex_timeout_ns, write_sigaltstack_old};
    use crate::errno::Os;

    /// Every row the table binds to a function here has one, and no function is bound to a
    /// name the table lacks.
    #[test]
    fn every_row_that_needs_a_function_has_one() {
        for row in crate::NAMES {
            let needs = !matches!(row.how, crate::How::Own | crate::How::Data);
            assert_eq!(needs, super::function(row.c).is_some(), "{}: {:?}", row.c, row.how);
        }
    }

    #[test]
    fn sigaltstack_ss_flags_offset_matches_os_abi() {
        let mut mac = [0xffu8; 24];
        // SAFETY: `mac` is a 24-byte writable buffer.
        unsafe { write_sigaltstack_old(Os::Mac, mac.as_mut_ptr()) };
        assert_eq!(&mac[0..16], &[0u8; 16], "macOS ss_sp and ss_size must be zeroed");
        assert_eq!(i32::from_ne_bytes(mac[16..20].try_into().unwrap()), 4, "macOS ss_flags at offset 16 must be SS_DISABLE (4)");
        assert_eq!(&mac[20..24], &[0u8; 4]);

        let mut linux = [0xffu8; 24];
        // SAFETY: `linux` is a 24-byte writable buffer.
        unsafe { write_sigaltstack_old(Os::Linux, linux.as_mut_ptr()) };
        assert_eq!(&linux[0..8], &[0u8; 8], "Linux ss_sp must be zeroed");
        assert_eq!(i32::from_ne_bytes(linux[8..12].try_into().unwrap()), 2, "Linux ss_flags at offset 8 must be SS_DISABLE (2)");
        assert_eq!(&linux[12..24], &[0u8; 12]);
    }

    #[test]
    fn futex_timeout_saturates_instead_of_overflowing() {
        assert_eq!(futex_timeout_ns(Timespec { sec: i64::MAX, nsec: 999_999_999 }), u64::MAX);
        assert_eq!(futex_timeout_ns(Timespec { sec: -5, nsec: -10 }), 0);
        assert_eq!(futex_timeout_ns(Timespec { sec: 2, nsec: 500 }), 2_000_000_500);
    }
}
