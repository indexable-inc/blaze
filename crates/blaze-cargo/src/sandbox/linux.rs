//! The sandbox on Linux. The command's own process stays outside and is the supervisor: it
//! forks the sandbox's first process, which enters new user, mount, network and IPC
//! namespaces, makes every mount read-only, puts the overlays, the writable directories and
//! the empty directories in place, brings up its loopback and forks the first process of a
//! new PID namespace. That one mounts its `/proc`, forks the program's process and waits for
//! it; when it ends, the kernel ends every other process of the namespace. The first process
//! is a session of its own, so that nothing of the sandbox has the caller's terminal as its
//! own; the supervisor passes on the signals the terminal sends it.
//!
//! With a view ([`Spec::view`]) the first process leaves the machine's mounts as they are and
//! puts the view together instead, on a tmpfs of its own: its binds, read-only but those
//! that are written, and its own `/dev`, `/etc` and `/tmp`. The first process of the PID
//! namespace mounts its `/proc` there and makes it the root (`pivot_root`), with the machine's
//! tree detached, so that every process of the sandbox sees the view and nothing else; the
//! program's process closes the descriptors of directories and files it was given before it
//! runs the program, which could lead out of it.
//!
//! The program's process installs a seccomp filter, hands the supervisor the filter's
//! descriptor and runs the program. On every run the filter refuses what the namespaces can't:
//! pushing input into a terminal (`TIOCSTI`, `TIOCLINUX`), `io_uring` (whose operations
//! seccomp never sees) and the calls of another architecture's table; and it has the kernel
//! ask the supervisor about each `connect` and each `sendto` with an address, so that a Unix
//! socket of the machine's, which a network namespace does not hide, is refused. When there is
//! a record to write it asks about the calls the record needs too; the supervisor reads each
//! call's path or address from the program's memory, notes it and lets the call go on as it
//! would have (the namespaces refuse what is refused). Without [`Spec::record_reads`] it asks
//! only about the calls that can write (an `open` only when its flags ask to write), the
//! network's and the test marker's, so that a program pays for nothing it reads. The program
//! runs as the user who ran it, with no capability left.
//!
//! For `blaze sandbox --v2` ([`super::v2`]) the supervisor appends each line to the live record
//! before it answers, and a traced run's processes load the interposer (`linux_interpose.c`,
//! through `LD_PRELOAD`), which tells it by markers what no call shows: the variables read by
//! name and the realtime clock read through the vDSO, and that it loaded. A process that ran a
//! program and never said so (a static or setuid one, or one run without the variables) is an
//! `X` line as the run ends. Listings (`getdents`), randomness (`getrandom`, `/dev/urandom`) and
//! the clock calls the vDSO does not answer are seen by the filter. A thread is in the scope its
//! last [`super::MARK`] named; a new process is in the scope the thread that forked it had when
//! it did. With [`Also::network`] there is no network namespace (a confined sandbox is not kept
//! from TCP), what it reaches is a `T network` line, and the machine's abstract Unix sockets,
//! which it would see then, are refused.
//!
//! Where a user may not make a user namespace (Ubuntu 23.10 on keeps them from it through
//! AppArmor, a container's seccomp profile may), a sandbox without a view or overlays is
//! confined instead ([`confined`]): the first process makes no namespace and has Landlock
//! refuse every write but to the writable directories, a `TMPDIR` of its own and the terminal
//! and null devices, and every TCP `bind` and `connect`; the filter asks the supervisor about
//! every call of the network and every signal too, and the supervisor refuses each address that
//! is not a Unix socket of the sandbox's own (the loopback and the abstract sockets are the
//! machine's here) and each signal to a process outside the sandbox. The first process reaps
//! every process the program leaves (`PR_SET_CHILD_SUBREAPER`) and kills those still there when
//! it ends. What a namespace hides and this does not: the machine's temporary directories and
//! processes are seen, read-only, and SysV IPC and the mode, owner and times of a file are
//! not refused.
//!
//! Every system call is behind a safe function of `raw` but two: `fork`, sound only in a
//! process of one thread, and `close` of a descriptor by its number, which no `OwnedFd` holds
//! (a copy of the supervisor's socket each process of the sandbox ends, and what
//! [`close_machine_files`] finds in `/proc/self/fd`). The functions that call them allow
//! unsafe code, each fork saying why its process is one thread; the rest of the file denies it.

#![deny(unsafe_code)]

use std::{
    ffi::{CStr, CString, OsString},
    fmt::Write as _,
    ops::Range,
    os::{
        fd::{AsFd as _, AsRawFd as _, IntoRawFd as _, OwnedFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
};

use blaze_core::{Untraceable, utf8::push_lossy};
use blaze_hash::HashMap;

use super::{Also, Bind, MARK, Recorder, Scope, Spec, View, tidy, tidy_into};

#[allow(unsafe_code, reason = "the Linux system calls, each behind a safe function")]
mod raw;

/// The library a traced run's processes load (`LD_PRELOAD`), built from `linux_interpose.c` by
/// `build.rs`: it tells the supervisor what its filter can't see, the variables read by name
/// and the realtime clock read through the vDSO, and says it loaded, so that a program it is
/// not in (a static or setuid one) is known.
const INTERPOSER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libblaze_sandbox_interpose.so"));

/// The variable the interposer is told its own path by, which it takes out of what the program
/// sees as it loads, with itself out of `LD_PRELOAD`, and puts back in what the program runs.
const INTERPOSING: &str = "__BLAZE_INTERPOSE";

/// Where the interposer is, written there from this executable the first time: a path named
/// by its contents in the user's cache directory, as the macOS sandbox keeps its library.
fn interposer() -> Result<PathBuf, String> {
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let caches = caches
        .or_else(|| std::env::home_dir().map(|it| it.join(".cache")))
        .ok_or("no XDG_CACHE_HOME or home directory to keep the sandbox's interposer in")?;
    let dir = caches.join("blaze/sandbox").join(crate::content::hex(&blaze_ids::b3::hash(INTERPOSER)[..12]));
    let file = dir.join("libblaze_sandbox_interpose.so");
    // The name is that of the contents it should have; what is there is still compared, so a
    // file left half written, or changed since, is written again.
    if std::fs::read(&file).is_ok_and(|it| it == INTERPOSER) {
        return Ok(file);
    }
    // alloc: the error, once per run, and no sandbox is made.
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    // alloc: once per run, the first time the interposer is written.
    let writing = dir.join(format!("writing-{}", std::process::id()));
    std::fs::write(&writing, INTERPOSER)
        .and_then(|()| std::fs::rename(&writing, &file))
        // alloc: the error, once per run, and no sandbox is made.
        .map_err(|it| format!("{}: {it}", file.display()))?;
    Ok(file)
}

/// The directories that are the sandbox's own: the kernel's views and the empty ones.
const OWN: &[&str] = &["/proc", "/sys", "/dev", "/tmp", "/var/tmp", "/run"];

/// The directories a confined sandbox has as its own besides its `TMPDIR`: the kernel's
/// views, which it sees as the machine has them.
const CONFINED_OWN: &[&str] = &["/proc", "/sys", "/dev"];

/// What a sandbox's program sees, for the supervisor.
#[derive(Clone, Copy)]
enum Around {
    /// The machine, in namespaces.
    Namespaces,
    /// A view, in namespaces ([`Spec::view`]).
    View,
    /// The machine, confined ([`confined`]), by the sandbox's first process.
    Confined(libc::pid_t),
}

/// The directories the sandbox mounts empty besides `/tmp`, where they are: the machine's
/// sockets live in `/run` (`/var/run` is a link to it, or one more), and the Nix daemon's,
/// through which a program could have the store build or keep what it likes, in its own.
const EMPTIED: &[&str] =
    &["/var/tmp", "/dev/shm", "/run", "/var/run", "/nix/var/nix/daemon-socket"];

/// What a call does, for the record.
#[derive(Clone, Copy)]
enum Does {
    /// Looks at its paths: noted only when reads are recorded.
    Look,
    /// Runs the program at its path: noted, as a read of it, only when reads are recorded.
    Run,
    /// Opens its path with the flags in this argument: a write when they ask for one.
    Open(usize),
    /// Asks whether its path is there, with the mode in this argument: the test marker when
    /// that is `F_OK`.
    Access(usize),
    /// Changes the file system at the paths that are marked written.
    Change,
    /// `bind`: the address in the second argument, its length in the third.
    Address,
    /// `connect`, whose address is as `bind`'s: asked about on every run, for its Unix socket.
    Connect,
    /// `sendto`: the address in the fifth argument, none for a connected socket.
    SendTo,
    /// `sendmsg`: the address in the message the second argument points to.
    SendMsg,
    /// Sends a signal to the process this argument names: asked about only in a confined
    /// sandbox, which has no PID namespace to keep it to its own.
    Signal(Target),
    /// Lists the directory the descriptor in this argument is open on: noted only when reads
    /// are recorded.
    List(usize),
    /// Gets randomness (`getrandom`): noted only when reads are recorded.
    Random,
    /// Reads a clock: the one whose id is in this argument, noted when it is a realtime one,
    /// or always the realtime one (`None`). Only when reads are recorded; what reads it through
    /// the vDSO makes no call, and the interposer says it does ([`super::CLOCK_MARK`]).
    Clock(Option<usize>),
}

/// The clocks that tell the time of day rather than how long since something:
/// `CLOCK_REALTIME`, `CLOCK_REALTIME_COARSE`, `CLOCK_REALTIME_ALARM` and `CLOCK_TAI`.
const REALTIME: [u32; 4] = [0, 5, 8, 11];

/// How a signal's call names the process it goes to.
#[derive(Clone, Copy)]
enum Target {
    /// `kill`'s: a process, `0` the caller's group, `-1` every process, below it a group.
    Kill,
    /// A process or thread in the first argument (`tkill`, `tgkill`, `rt_sigqueueinfo`, ...).
    Process,
    /// A pidfd in the first argument (`pidfd_send_signal`).
    Pidfd,
}

/// A system call the filter may ask about: its number, which of its arguments are the
/// directory a path is relative to (`None`: the working directory) and the path, and whether
/// the call writes there; and what it does. The `*at` calls take the directory first.
struct Call {
    nr: u32,
    paths: &'static [(Option<usize>, usize, bool)],
    does: Does,
}

const fn call(nr: u32, paths: &'static [(Option<usize>, usize, bool)], does: Does) -> Call {
    Call { nr, paths, does }
}

const NO_PATH: &[(Option<usize>, usize, bool)] = &[];

#[cfg(target_arch = "x86_64")]
const CALLS: &[Call] = &[
    call(2, &[(None, 0, true)], Does::Open(1)),     // open
    call(4, &[(None, 0, false)], Does::Look),       // stat
    call(6, &[(None, 0, false)], Does::Look),       // lstat
    call(21, &[(None, 0, false)], Does::Access(1)), // access
    call(59, &[(None, 0, false)], Does::Run),       // execve
    call(76, &[(None, 0, true)], Does::Change),     // truncate
    call(80, &[(None, 0, false)], Does::Look),      // chdir
    call(82, &[(None, 0, true), (None, 1, true)], Does::Change), // rename
    call(83, &[(None, 0, true)], Does::Change),     // mkdir
    call(84, &[(None, 0, true)], Does::Change),     // rmdir
    call(85, &[(None, 0, true)], Does::Change),     // creat
    call(86, &[(None, 0, false), (None, 1, true)], Does::Change), // link
    call(87, &[(None, 0, true)], Does::Change),     // unlink
    call(88, &[(None, 1, true)], Does::Change),     // symlink
    call(89, &[(None, 0, false)], Does::Look),      // readlink
    call(90, &[(None, 0, true)], Does::Change),     // chmod
    call(257, &[(Some(0), 1, true)], Does::Open(2)), // openat
    call(258, &[(Some(0), 1, true)], Does::Change), // mkdirat
    call(262, &[(Some(0), 1, false)], Does::Look),  // newfstatat
    call(263, &[(Some(0), 1, true)], Does::Change), // unlinkat
    call(264, &[(Some(0), 1, true), (Some(2), 3, true)], Does::Change), // renameat
    call(265, &[(Some(0), 1, false), (Some(2), 3, true)], Does::Change), // linkat
    call(266, &[(Some(1), 2, true)], Does::Change), // symlinkat
    call(267, &[(Some(0), 1, false)], Does::Look),  // readlinkat
    call(268, &[(Some(0), 1, true)], Does::Change), // fchmodat
    call(269, &[(Some(0), 1, false)], Does::Access(2)), // faccessat
    call(316, &[(Some(0), 1, true), (Some(2), 3, true)], Does::Change), // renameat2
    call(322, &[(Some(0), 1, false)], Does::Run),   // execveat
    call(332, &[(Some(0), 1, false)], Does::Look),  // statx
    // openat2's flags are in a structure the filter can't read: it is always asked about.
    call(437, &[(Some(0), 1, true)], Does::Change), // openat2
    call(439, &[(Some(0), 1, false)], Does::Access(2)), // faccessat2
    call(42, NO_PATH, Does::Connect),               // connect
    call(44, NO_PATH, Does::SendTo),                // sendto
    call(46, NO_PATH, Does::SendMsg),               // sendmsg
    call(49, NO_PATH, Does::Address),               // bind
    call(62, NO_PATH, Does::Signal(Target::Kill)),  // kill
    call(129, NO_PATH, Does::Signal(Target::Process)), // rt_sigqueueinfo
    call(200, NO_PATH, Does::Signal(Target::Process)), // tkill
    call(234, NO_PATH, Does::Signal(Target::Process)), // tgkill
    call(297, NO_PATH, Does::Signal(Target::Process)), // rt_tgsigqueueinfo
    call(424, NO_PATH, Does::Signal(Target::Pidfd)), // pidfd_send_signal
    call(78, NO_PATH, Does::List(0)),               // getdents
    call(217, NO_PATH, Does::List(0)),              // getdents64
    call(318, NO_PATH, Does::Random),               // getrandom
    call(96, NO_PATH, Does::Clock(None)),           // gettimeofday
    call(201, NO_PATH, Does::Clock(None)),          // time
    call(228, NO_PATH, Does::Clock(Some(0))),       // clock_gettime
];
#[cfg(target_arch = "x86_64")]
const ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "x86_64")]
const NR_IOCTL: u32 = 16;

#[cfg(target_arch = "aarch64")]
const CALLS: &[Call] = &[
    call(34, &[(Some(0), 1, true)], Does::Change), // mkdirat
    call(35, &[(Some(0), 1, true)], Does::Change), // unlinkat
    call(36, &[(Some(1), 2, true)], Does::Change), // symlinkat
    call(37, &[(Some(0), 1, false), (Some(2), 3, true)], Does::Change), // linkat
    call(38, &[(Some(0), 1, true), (Some(2), 3, true)], Does::Change), // renameat
    call(45, &[(None, 0, true)], Does::Change),    // truncate
    call(48, &[(Some(0), 1, false)], Does::Access(2)), // faccessat
    call(49, &[(None, 0, false)], Does::Look),     // chdir
    call(53, &[(Some(0), 1, true)], Does::Change), // fchmodat
    call(56, &[(Some(0), 1, true)], Does::Open(2)), // openat
    call(78, &[(Some(0), 1, false)], Does::Look),  // readlinkat
    call(79, &[(Some(0), 1, false)], Does::Look),  // newfstatat
    call(221, &[(None, 0, false)], Does::Run),     // execve
    call(276, &[(Some(0), 1, true), (Some(2), 3, true)], Does::Change), // renameat2
    call(281, &[(Some(0), 1, false)], Does::Run),  // execveat
    call(291, &[(Some(0), 1, false)], Does::Look), // statx
    // openat2's flags are in a structure the filter can't read: it is always asked about.
    call(437, &[(Some(0), 1, true)], Does::Change), // openat2
    call(439, &[(Some(0), 1, false)], Does::Access(2)), // faccessat2
    call(200, NO_PATH, Does::Address),              // bind
    call(203, NO_PATH, Does::Connect),              // connect
    call(206, NO_PATH, Does::SendTo),               // sendto
    call(211, NO_PATH, Does::SendMsg),              // sendmsg
    call(129, NO_PATH, Does::Signal(Target::Kill)), // kill
    call(130, NO_PATH, Does::Signal(Target::Process)), // tkill
    call(131, NO_PATH, Does::Signal(Target::Process)), // tgkill
    call(138, NO_PATH, Does::Signal(Target::Process)), // rt_sigqueueinfo
    call(240, NO_PATH, Does::Signal(Target::Process)), // rt_tgsigqueueinfo
    call(424, NO_PATH, Does::Signal(Target::Pidfd)), // pidfd_send_signal
    call(61, NO_PATH, Does::List(0)),               // getdents64
    call(278, NO_PATH, Does::Random),               // getrandom
    call(169, NO_PATH, Does::Clock(None)),          // gettimeofday
    call(113, NO_PATH, Does::Clock(Some(0))),       // clock_gettime
];
#[cfg(target_arch = "aarch64")]
const ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
#[cfg(target_arch = "aarch64")]
const NR_IOCTL: u32 = 29;

/// `io_uring_setup`, of the table every architecture shares from 403 on.
const NR_IO_URING_SETUP: u32 = 425;
/// The bit x32's calls have in their number on x86-64.
const X32: u32 = 0x4000_0000;
/// The ioctls that push input into a terminal, which a program could make its caller's shell
/// run: `TIOCSTI`, and `TIOCLINUX`'s paste on a virtual console.
const TIOCSTI: u32 = 0x5412;
const TIOCLINUX: u32 = 0x541c;

/// The flags of an `open` that ask to write.
const WRITES: u32 = (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) as u32;

const SYS_PIDFD_OPEN: libc::c_long = 434;
const SYS_MOUNT_SETATTR: libc::c_long = 442;
const MOUNT_ATTR_RDONLY: u64 = 1;
const MOUNT_ATTR_NOSUID: u64 = 2;
const MOUNT_ATTR_NODEV: u64 = 4;
const AT_RECURSIVE: libc::c_uint = 0x8000;
const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP: libc::c_ulong = 1;
// _IOWR('!', 0, struct seccomp_notif), _IOWR('!', 1, struct seccomp_notif_resp) and
// _IOW('!', 4, __u64).
const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;
const SECCOMP_IOCTL_NOTIF_SET_FLAGS: libc::c_ulong = 0x4008_2104;

/// Each call of [`CALLS`] by its number, one past its index (0 for none): the supervisor
/// finds a notification's call in one load.
const BY_NR: [u8; 512] = {
    let mut by = [0u8; 512];
    let mut at = 0;
    while at < CALLS.len() {
        by[CALLS[at].nr as usize] = at as u8 + 1;
        at += 1;
    }
    by
};

const _: () = assert!(CALLS.len() < u8::MAX as usize);

/// The call of [`CALLS`] numbered `nr`, if it is one.
fn call_of(nr: i32) -> Option<&'static Call> {
    let at = *BY_NR.get(usize::try_from(nr).ok()?)?;
    CALLS.get(usize::from(at).checked_sub(1)?)
}

#[repr(C)]
#[derive(Default)]
struct Notif {
    id: u64,
    /// The thread that made the call, in the supervisor's PID namespace.
    pid: u32,
    flags: u32,
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
struct NotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

#[repr(C)]
struct IfReq {
    name: [u8; 16],
    flags: libc::c_short,
    pad: [u8; 22],
}

fn os(what: &str) -> String {
    // alloc: the error, once, and the sandbox or the run stops.
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_default()
}

/// Runs `program` sandboxed as `spec` says, or with `None` makes the sandbox and runs
/// nothing in it. What it ends with, as [`super::backend`] says.
pub fn run(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    let unshared = match spec.view {
        Some(_) => None,
        None => namespaces().err(),
    };
    run_as(spec, program, unshared, also)
}

/// [`run`], in namespaces or, with why they can't be made, [`confined`].
pub(super) fn run_as(
    spec: &Spec,
    program: Option<&[OsString]>,
    unshared: Option<String>,
    also: Also<'_>,
) -> Result<i32, String> {
    // The interposer is seen where it is, read-only, and is no input of what runs.
    let interposer = match also.live.is_some() && spec.record_reads {
        true => Some(interposer()?),
        false => None,
    };
    let mut with;
    let spec = match &interposer {
        Some(file) => {
            // alloc: once per sandbox, the interposer's directory as a bind of its own.
            let dir = file.parent().unwrap_or(file).to_owned();
            // clone: once per sandbox, the spec with the interposer's directory in it.
            with = spec.clone();
            match &mut with.view {
                Some(view) => view.binds.push(Bind::read(&dir, &dir)),
                None => with.readable.push(dir),
            }
            &with
        }
        None => spec,
    };
    if let Some(view) = &spec.view {
        check_view(spec, view)?;
    }
    if let Some(why) = &unshared
        && !spec.overlays.is_empty()
    {
        // alloc: the error, once, and no sandbox is made.
        return Err(format!(
            "{why}, and a sandbox with overlays (a sandboxed test's workspace) is made in one"
        ));
    }
    if let Some(why) = &unshared
        && let Err(no) = landlock::abi()
    {
        // alloc: the error, once, and no sandbox is made.
        return Err(format!("{why}; and it can't be confined without one: {no}"));
    }
    // The confined program's own temporary directory, where a namespace would have its own
    // empty `/tmp`.
    let scratch = match unshared {
        Some(_) => {
            // alloc: once per sandbox, its temporary directory's name.
            let dir = std::env::temp_dir().join(format!(
                "blaze-sandbox-tmp-{}-{}",
                std::process::id(),
                unique()
            ));
            // alloc: the error, once, and no sandbox is made.
            std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
            Some(dir)
        }
        None => None,
    };
    let ran = run_in(spec, program, scratch.as_deref(), also, interposer.as_deref());
    if let Some(dir) = &scratch {
        _ = std::fs::remove_dir_all(dir);
    }
    ran
}

/// A number no other run of this process has.
fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether this user may make a user namespace and map itself in it, which a sandbox in
/// namespaces needs, tried in a process of its own; why not when it may not.
#[allow(unsafe_code, reason = "fork")]
fn namespaces() -> Result<(), String> {
    let (uid, gid) = (raw::uid(), raw::gid());
    // alloc: once per process, the maps the kernel is given, before the probe's fork.
    let maps = [
        // alloc: as the maps are.
        (c"/proc/self/setgroups", "deny".to_owned()),
        // alloc: as the maps are.
        (c"/proc/self/uid_map", format!("{uid} {uid} 1")),
        // alloc: as the maps are.
        (c"/proc/self/gid_map", format!("{gid} {gid} 1")),
    ];
    // SAFETY: the sandbox runs in `blaze sandbox`'s process, which is one thread
    // (`super::command` runs before anything else in it, and nothing here starts a thread), or
    // in a test's of its own, one thread too (`super::alone`). No lock is held by another
    // thread across the fork, so the child need not keep to async-signal-safe calls; it only
    // unshares, opens and writes three files (calls, no allocation) and `_exit`s.
    let child = unsafe { raw::fork() };
    if child < 0 {
        return Err(os("fork"));
    }
    if child == 0 {
        // What failed, as the step in the high bits (0 `unshare`, then each file) and the
        // error in the low six.
        let failed = |step: i32| {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(1);
            raw::exit((step << 6) | (errno & 63).max(1))
        };
        if raw::unshare(libc::CLONE_NEWUSER) < 0 {
            failed(0);
        }
        for (step, (file, text)) in (1..).zip(&maps) {
            let fd = raw::open(file, libc::O_WRONLY | libc::O_CLOEXEC);
            if fd.as_ref().is_none_or(|fd| raw::write(fd.as_fd(), text.as_bytes()) < 0) {
                failed(step);
            }
            drop(fd);
        }
        raw::exit(0);
    }
    let mut status = 0;
    while raw::wait(child, &mut status, 0) < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    let code = ended(status);
    if code == 0 {
        return Ok(());
    }
    // The error, once, and no user namespace is made.
    let what = match (code >> 6) as usize {
        // alloc: as the error is.
        0 => "unshare".to_owned(),
        step => maps[(step - 1).min(2)].0.to_string_lossy().into_owned(),
    };
    // alloc: as the error is.
    Err(format!("a user namespace ({what}): {}", std::io::Error::from_raw_os_error(code & 63)))
}

/// [`run`], confined in `scratch` when it is given ([`confined`]), in namespaces otherwise.
#[allow(unsafe_code, reason = "fork")]
fn run_in(
    spec: &Spec,
    program: Option<&[OsString]>,
    scratch: Option<&Path>,
    also: Also<'_>,
    interposer: Option<&Path>,
) -> Result<i32, String> {
    // Everything the processes of the sandbox use is made before the first fork.
    let argv: Vec<CString> = program
        .unwrap_or_default()
        .iter()
        .map(|it| CString::new(it.as_bytes()).unwrap_or_default())
        // alloc: once per sandbox, the program's command line for the kernel.
        .collect();
    // alloc: once per sandbox, the confined program's `TMPDIR`.
    let tmp = scratch.map(|it| ("TMPDIR".to_owned(), it.display().to_string()));
    let env: Vec<CString> = match also.env {
        None => spec
            .env
            .iter()
            .filter(|(name, _)| tmp.is_none() || name != "TMPDIR")
            .chain(tmp.as_ref())
            // alloc: once per sandbox, the program's environment for the kernel.
            .map(|(name, value)| CString::new(format!("{name}={value}")).unwrap_or_default())
            // alloc: as the variables are.
            .collect(),
        Some(given) => environment(given, tmp.as_ref(), interposer)?,
    };
    let Some([supervisor, child]) = raw::stream_pair() else {
        return Err(os("socketpair"));
    };
    let recorded = program.is_some() && (spec.record.is_some() || also.live.is_some());
    let confining = scratch.is_some();
    let filter = program.map(|_| {
        filter_program(recorded, spec.record_reads, confining, also.network, child.as_raw_fd())
    });
    let scratch_dir = scratch.map(tidy);
    let own: Vec<&str> = match &scratch_dir {
        // alloc: once per sandbox, its own directories, by reference.
        Some(dir) => CONFINED_OWN.iter().copied().chain([dir.as_str()]).collect(),
        // alloc: as the confined sandbox's are.
        None => OWN.to_vec(),
    };
    let mut recorder = Recorder::new(spec, &own);
    if let Some(live) = also.live {
        recorder.live(&live.path)?;
    }
    if let Some(file) = interposer {
        recorder.interposed();
        recorder.ours(file);
    }
    let (uid, gid) = (raw::uid(), raw::gid());
    // Before the fork, so that no signal ends this process before it passes them on; the
    // sandbox's processes that run no program keep the handlers, and pass nothing on.
    super::pass_signals_to(0);
    // SAFETY: the sandbox runs in `blaze sandbox`'s process, which is one thread
    // (`super::command` runs before anything else in it; the signal handlers above start
    // none), or in a test's of its own, one thread too (`super::alone`). No lock is held by
    // another thread across the fork, so the child need not keep to async-signal-safe calls:
    // it allocates, prints and reads the environment as it makes the sandbox, and forks
    // again, as one thread too.
    let first = unsafe { raw::fork() };
    if first < 0 {
        return Err(os("fork"));
    }
    if first == 0 {
        // A session of its own, with no controlling terminal: nothing of the sandbox can push
        // input into the caller's (`TIOCSTI`), which only a terminal's own session may. And
        // gone with the supervisor, which a signal that can't be passed on ends.
        raw::setsid();
        raw::die_with_parent();
        // This child's copy of the supervisor's end is never used; the supervisor keeps its own.
        drop(supervisor);
        let made = Made {
            argv: &argv,
            env: &env,
            // Owned by the processes of the sandbox from here, each closing its copy.
            socket: child.into_raw_fd(),
            filter: filter.as_deref(),
            viewed: spec.view.is_some(),
            network: also.network,
        };
        let made_it = match scratch {
            Some(scratch) => confined(spec, scratch, &made, program.is_none()),
            None => sandbox(spec, uid, gid, &made, program.is_none()),
        };
        let code = match made_it {
            Ok(code) => code,
            Err(error) => {
                eprintln!("blaze: the sandbox could not be made: {error}");
                super::COULD_NOT
            }
        };
        raw::exit(code);
    }
    super::pass_signals_to(first);
    // The child's end, which the child has its own copy of: closed first, so that the receive
    // ends when the sandbox ends without sending the filter's descriptor.
    drop(child);
    let listener = raw::received_descriptor(supervisor.as_fd());
    drop(supervisor);
    let around = match (&spec.view, confining) {
        (Some(_), _) => Around::View,
        (None, true) => Around::Confined(first),
        (None, false) => Around::Namespaces,
    };
    let mut watch = Watch { around, network: also.network, processes: HashMap::default() };
    let supervised = listener.is_some();
    if let Some(listener) = listener {
        supervise(listener.as_raw_fd(), first, &mut recorder, &mut watch);
        drop(listener);
    }
    let mut status = 0;
    while raw::wait(first, &mut status, 0) < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    // Its number may be another's now.
    super::pass_signals_to(0);
    if recorded {
        if !supervised {
            recorder.incomplete(0, "the sandbox did not see the program run");
        }
        recorder.ended();
    }
    if let (Some(file), true) = (&spec.record, recorded) {
        recorder.write(file);
    }
    if let (Some(live), Some(why)) = (also.live, recorder.live_failed()) {
        live.failed(why);
    }
    Ok(ended(status))
}

/// The program's environment: `given` as it is, with the confined sandbox's own `tmp` for
/// `TMPDIR`, and with `interposer` loaded first (`LD_PRELOAD`, in its place when the program has
/// one) and told its path ([`INTERPOSING`]). A name or a value with a nul in it can't be in one.
fn environment(
    given: &[(OsString, OsString)],
    tmp: Option<&(String, String)>,
    interposer: Option<&Path>,
) -> Result<Vec<CString>, String> {
    // Once per sandbox, the program's environment for the kernel.
    let entry = |name: &[u8], value: &[u8]| {
        let mut bytes = Vec::with_capacity(name.len() + 1 + value.len() + 1);
        bytes.extend_from_slice(name);
        bytes.push(b'=');
        bytes.extend_from_slice(value);
        CString::new(bytes).map_err(|_| {
            // alloc: the error, once, and no sandbox is made.
            format!("the variable {:?} has a nul in it", String::from_utf8_lossy(name))
        })
    };
    let mut env = Vec::with_capacity(given.len() + 3);
    let mut preloaded = false;
    for (name, value) in given {
        let (name, value) = (name.as_bytes(), value.as_bytes());
        if (name == b"TMPDIR" && tmp.is_some()) || name == INTERPOSING.as_bytes() {
            continue;
        }
        match (name, interposer) {
            // `ours:` and theirs, even empty: the interposer gives back what follows, and
            // takes the variable away only when it is `ours` alone.
            (b"LD_PRELOAD", Some(ours)) => {
                let ours = ours.as_os_str().as_bytes();
                let mut both = Vec::with_capacity(ours.len() + 1 + value.len());
                both.extend_from_slice(ours);
                both.push(b':');
                both.extend_from_slice(value);
                env.push(entry(name, &both)?);
                preloaded = true;
            }
            _ => env.push(entry(name, value)?),
        }
    }
    if let Some((name, value)) = tmp {
        env.push(entry(name.as_bytes(), value.as_bytes())?);
    }
    if let Some(ours) = interposer {
        let ours = ours.as_os_str().as_bytes();
        if !preloaded {
            env.push(entry(b"LD_PRELOAD", ours)?);
        }
        env.push(entry(INTERPOSING.as_bytes(), ours)?);
    }
    Ok(env)
}

/// What the sandbox's processes are given, made before the first fork.
struct Made<'a> {
    argv: &'a [CString],
    env: &'a [CString],
    /// Where the filter's descriptor is handed to the supervisor.
    socket: libc::c_int,
    /// The seccomp filter, when there is a record to write.
    filter: Option<&'a [libc::sock_filter]>,
    /// Whether the program sees a view ([`Spec::view`]) rather than the machine.
    viewed: bool,
    /// Whether it may reach the network: there is no network namespace, and a confined one
    /// is not kept from TCP.
    network: bool,
}

/// The map of a user namespace's id `id` to itself, alone (`uid_map`, `gid_map`), in `room`.
fn id_map(room: &mut [u8; 64], id: u32) -> &[u8] {
    use std::io::Write as _;
    let mut out = &mut room[..];
    _ = write!(out, "{id} {id} 1");
    let left = out.len();
    &room[..64 - left]
}

/// The code a process that ended with `status` is taken to have ended with.
fn ended(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// The sandbox's first process: makes the namespaces and the mounts, then waits for the
/// first process of the PID namespace.
#[allow(unsafe_code, reason = "fork, and close of the supervisor's socket by number")]
fn sandbox(
    spec: &Spec,
    uid: libc::uid_t,
    gid: libc::gid_t,
    made: &Made<'_>,
    probe: bool,
) -> Result<i32, String> {
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC;
    if !made.network {
        flags |= libc::CLONE_NEWNET;
    }
    if raw::unshare(flags) < 0 {
        let refused = std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        let mut why = os("a user namespace (unshare)");
        // A container's seccomp profile (Docker's default) refuses it with EPERM: say so, and
        // the ways on, where the process is told it is in one.
        let container = std::env::var("container").ok();
        if refused && let Some(known) = super::container(Path::new("/"), container.as_deref()) {
            why.push_str(&super::container_hint(&known));
        }
        return Err(why);
    }
    // The maps the kernel is given, once per sandbox.
    let write = |file: &str, text: &[u8]| {
        // alloc: the error, once, and no sandbox is made.
        std::fs::write(file, text).map_err(|it| format!("{file}: {it}"))
    };
    let mut map = [0u8; 64];
    write("/proc/self/setgroups", b"deny")?;
    write("/proc/self/uid_map", id_map(&mut map, uid))?;
    write("/proc/self/gid_map", id_map(&mut map, gid))?;
    let root = c(Path::new("/"));
    if raw::mount(None, &root, None, libc::MS_REC | libc::MS_PRIVATE, None) < 0 {
        return Err(os("making the mounts private"));
    }
    match &spec.view {
        Some(view) => put_view(view, uid, gid, &spec.env)?,
        None => machine(spec)?,
    }
    if !made.network {
        loopback_up()?;
    }
    if raw::unshare(libc::CLONE_NEWPID) < 0 {
        return Err(os("a PID namespace (unshare)"));
    }
    let cwd = c(&spec.cwd);
    // SAFETY: this is the sandbox's first process, forked by `run_in` from a process of one
    // thread, so it is one thread: nothing in it starts another. So the child need not keep
    // to async-signal-safe calls: it mounts, allocates and prints its error.
    let init = unsafe { raw::fork() };
    if init < 0 {
        return Err(os("fork"));
    }
    if init == 0 {
        // Gone with the sandbox's first process, and the whole namespace with it.
        raw::die_with_parent();
        let code = match first_process(&cwd, made, probe) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("blaze: the sandbox could not be made: {error}");
                super::COULD_NOT
            }
        };
        raw::exit(code);
    }
    // SAFETY: `made.socket` is the child's end of the supervisor's pair, owned by no
    // `OwnedFd`; this process never uses it after, and the PID namespace's first process has
    // its own copy.
    unsafe { raw::close(made.socket) };
    let mut status = 0;
    while raw::wait(init, &mut status, 0) < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    Ok(ended(status))
}

/// The first process of a sandbox confined where no user namespace can be made: it keeps
/// itself and what it runs from gaining privileges (`no_new_privs`, which Landlock and an
/// unprivileged seccomp filter need), reaps every process the program leaves, has Landlock
/// refuse every write but to the writable directories and `scratch`, and every TCP `bind` and
/// `connect`, runs the program and waits for it, and kills what it left when it ends.
#[allow(unsafe_code, reason = "fork, and close of the supervisor's socket by number")]
fn confined(spec: &Spec, scratch: &Path, made: &Made<'_>, probe: bool) -> Result<i32, String> {
    if raw::no_new_privs() < 0 {
        return Err(os("no new privileges (prctl)"));
    }
    if raw::child_subreaper() < 0 {
        return Err(os("reaping what the program leaves (prctl)"));
    }
    // clone: once per sandbox, the directories Landlock is told, with its `TMPDIR`.
    let mut writable = spec.writable.clone();
    // alloc: as `writable` is.
    writable.push(scratch.to_owned());
    for dir in &spec.writable {
        // alloc: the error, once, and no sandbox is made.
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    landlock::restrict(&writable, made.network)?;
    let cwd = c(&spec.cwd);
    if raw::chdir(&cwd) < 0 {
        // alloc: the error, once, and no sandbox is made.
        return Err(os(&format!("the working directory {}", cwd.to_string_lossy())));
    }
    if probe {
        return Ok(0);
    }
    // SAFETY: this is the sandbox's first process, forked by `run_in` from a process of one
    // thread, so it is one thread: Landlock and `prctl` start none. So the child need not keep
    // to async-signal-safe calls: it installs the filter, allocates and prints its error.
    let program = unsafe { raw::fork() };
    if program < 0 {
        return Err(os("fork"));
    }
    if program == 0 {
        raw::die_with_parent();
        let error = exec(made);
        eprintln!("blaze: could not run {}: {error}", made.argv[0].to_string_lossy());
        raw::exit(127);
    }
    // SAFETY: `made.socket` is the child's end of the supervisor's pair, owned by no
    // `OwnedFd`; this process never uses it after, and the program's process has its own copy.
    unsafe { raw::close(made.socket) };
    let mut code = None;
    let mut status = 0;
    while code.is_none() {
        let pid = raw::wait(-1, &mut status, 0);
        if pid == program {
            code = Some(ended(status));
        } else if pid < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return Err(os("waitpid"));
        }
    }
    // What the program left is killed, as the end of a PID namespace would: each child, and
    // each process that comes to this one as its parent ends, until none is left.
    let me = raw::pid();
    // alloc: once, as the sandbox ends.
    let children = format!("/proc/{me}/task/{me}/children");
    loop {
        // alloc: once per process the program left, as the sandbox ends.
        let listed = std::fs::read_to_string(&children).unwrap_or_default();
        for pid in listed.split_whitespace().filter_map(|it| it.parse::<libc::pid_t>().ok()) {
            raw::kill(pid, libc::SIGKILL);
        }
        if raw::wait(-1, &mut status, 0) < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            break;
        }
    }
    Ok(code.unwrap_or(1))
}

/// Landlock, which lets a process without privileges restrict itself and what it runs.
mod landlock {
    use std::{
        os::fd::{AsFd as _, AsRawFd as _},
        path::{Path, PathBuf},
    };

    pub(super) const CREATE_RULESET: libc::c_long = 444;
    pub(super) const ADD_RULE: libc::c_long = 445;
    pub(super) const RESTRICT_SELF: libc::c_long = 446;
    pub(super) const CREATE_RULESET_VERSION: libc::c_uint = 1;
    pub(super) const RULE_PATH_BENEATH: libc::c_int = 1;

    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2 on.
    const REFER: u64 = 1 << 13;
    /// ABI 3 on.
    const TRUNCATE: u64 = 1 << 14;
    /// ABI 4 on.
    const BIND_TCP: u64 = 1 << 0;
    const CONNECT_TCP: u64 = 1 << 1;
    /// ABI 6 on.
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;

    /// The devices of `/dev` a program writes as it would anywhere, without writing the machine.
    const DEVICES: &[&str] = &["null", "zero", "full", "tty", "random", "urandom", "pts"];

    #[repr(C)]
    pub(super) struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    pub(super) struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The version of Landlock this kernel has, or why it has none.
    pub(super) fn abi() -> Result<i64, String> {
        let abi = super::raw::landlock_abi();
        if abi >= 1 {
            return Ok(abi);
        }
        let error = std::io::Error::last_os_error();
        Err(match error.raw_os_error() {
            // alloc: the error, once, and no sandbox is made.
            Some(libc::ENOSYS) => "this kernel has no Landlock (Linux 5.13 on has it)".to_owned(),
            // alloc: as above.
            Some(libc::EOPNOTSUPP) => "this kernel has Landlock turned off (`lsm=`)".to_owned(),
            // alloc: as above.
            _ => format!("Landlock: {error}"),
        })
    }

    /// Restricts this process and what it runs: no write but below `writable` and to
    /// [`DEVICES`], and, where the kernel has them, no TCP `bind` or `connect` but with the
    /// `network`, no abstract Unix socket and no signal outside.
    pub(super) fn restrict(writable: &[PathBuf], network: bool) -> Result<(), String> {
        let abi = abi()?;
        let mut file = WRITE_FILE;
        let mut dir = WRITE_FILE
            | REMOVE_DIR
            | REMOVE_FILE
            | MAKE_CHAR
            | MAKE_DIR
            | MAKE_REG
            | MAKE_SOCK
            | MAKE_FIFO
            | MAKE_BLOCK
            | MAKE_SYM;
        if abi >= 2 {
            dir |= REFER;
        }
        if abi >= 3 {
            file |= TRUNCATE;
            dir |= TRUNCATE;
        }
        let attr = RulesetAttr {
            handled_access_fs: dir,
            handled_access_net: if abi >= 4 && !network { BIND_TCP | CONNECT_TCP } else { 0 },
            scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
        };
        // The structure as long as this version knows it.
        let size: usize = match abi {
            1..=3 => 8,
            4 | 5 => 16,
            _ => 24,
        };
        let Some(ruleset) = super::raw::landlock_ruleset(&attr, size) else {
            return Err(super::os("a Landlock ruleset"));
        };
        let add = |path: &Path, required: bool| -> Result<(), String> {
            let at = super::c(path);
            let Some(fd) = super::raw::open(&at, libc::O_PATH | libc::O_CLOEXEC) else {
                return match required {
                    // alloc: the error, once, and no sandbox is made.
                    true => Err(super::os(&path.display().to_string())),
                    false => Ok(()),
                };
            };
            let stat = super::raw::stat_of(fd.as_raw_fd());
            let is_dir = stat.is_some_and(|it| it.st_mode & libc::S_IFMT == libc::S_IFDIR);
            // A device's directory (`/dev/pts`) is given what its devices are.
            let allowed = if is_dir && required { dir } else { file };
            let rule = PathBeneath { allowed_access: allowed, parent_fd: fd.as_raw_fd() };
            let added = super::raw::landlock_add_rule(ruleset.as_fd(), &rule);
            // Read before `drop` can change `errno`.
            let error = (added < 0).then(std::io::Error::last_os_error);
            drop(fd);
            match error {
                // alloc: the error, once, and no sandbox is made.
                Some(error) => Err(format!("{} written (landlock_add_rule): {error}", path.display())),
                None => Ok(()),
            }
        };
        for it in writable {
            add(it, true)?;
        }
        for it in DEVICES {
            add(&Path::new("/dev").join(it), false)?;
        }
        if super::raw::landlock_restrict_self(ruleset.as_fd()) < 0 {
            return Err(super::os("restricting the sandbox (landlock_restrict_self)"));
        }
        // The restriction holds without the ruleset's descriptor.
        drop(ruleset);
        Ok(())
    }
}

/// Puts the machine's file system in place for a sandbox without a view: every mount
/// read-only, the overlays, the writable and the readable directories and the empty ones on
/// it.
fn machine(spec: &Spec) -> Result<(), String> {
    let root = c(Path::new("/"));
    let opened = |at: &Path| -> Result<std::os::fd::OwnedFd, String> {
        let path = c(at);
        raw::open(&path, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
            // alloc: the error, once, and no sandbox is made.
            .ok_or_else(|| os(&at.display().to_string()))
    };
    // The writable directories are reached through descriptors opened before anything is
    // made read-only or hidden, and bound back in place after: they stay the directories
    // outside, written for real.
    // alloc: once per sandbox, a descriptor of each.
    let writable = spec.writable.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    // alloc: as `writable`'s.
    let readable = spec.readable.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    let attr = MountAttr { attr_set: MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
    if raw::mount_setattr(&root, AT_RECURSIVE, &attr) < 0 {
        return Err(os("making every mount read-only (mount_setattr)"));
    }
    let tmpfs = |at: &Path| -> Result<(), String> {
        let (kind, at_c) = (c(Path::new("tmpfs")), c(at));
        let options = c(Path::new("mode=1777"));
        let flags = libc::MS_NOSUID | libc::MS_NODEV;
        if raw::mount(
            Some(kind.as_c_str()),
            &at_c,
            Some(kind.as_c_str()),
            flags,
            Some(options.as_c_str()),
        ) < 0
        {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&format!("an empty {}", at.display())));
        }
        Ok(())
    };
    // The overlays are reached through descriptors opened before the empty `/tmp` hides
    // what is there: a workspace under `/tmp` is mounted again inside it. Their own
    // directories are on a tmpfs that the program's `/tmp` then hides as well.
    // alloc: once per sandbox, a descriptor of each.
    let lowers = spec.overlays.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    let staging = Path::new("/tmp");
    tmpfs(staging)?;
    for at in 0..spec.overlays.len() {
        // alloc: once per overlay, its directories' names.
        for it in [staging.join(format!("{at}/upper")), staging.join(format!("{at}/work"))] {
            // alloc: the error, once, and no sandbox is made.
            std::fs::create_dir_all(&it).map_err(|error| format!("{}: {error}", it.display()))?;
        }
    }
    let staged = opened(staging)?;
    tmpfs(staging)?;
    // The other empty directories go in place before the overlays too: a workspace or a
    // `TMPDIR` under `/dev/shm` is mounted again inside its empty one. Only a directory is
    // (`/var/run` is most often a link to `/run`). The links at the top of `/run` are made
    // again in the empty one: NixOS's `PATH` goes through `/run/current-system`, its drivers
    // through `/run/opengl-driver`, and a link reaches no socket that is not seen anyway.
    for dir in EMPTIED.iter().map(Path::new) {
        if !dir.symlink_metadata().is_ok_and(|it| it.is_dir()) {
            continue;
        }
        let links: Vec<(PathBuf, PathBuf)> = match *dir == *Path::new("/run") {
            true => std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|it| it.file_type().is_ok_and(|it| it.is_symlink()))
                .filter_map(|it| Some((it.path(), std::fs::read_link(it.path()).ok()?)))
                // alloc: once per sandbox, the links at the top of `/run`.
                .collect(),
            false => Vec::with_capacity(0),
        };
        tmpfs(dir)?;
        for (link, to) in links {
            std::os::unix::fs::symlink(&to, &link).map_err(|error| {
                // alloc: the error, once, and no sandbox is made.
                format!("{} in the empty {}: {error}", link.display(), dir.display())
            })?;
        }
    }
    // Once per directory the sandbox mounts, the path it is reached by.
    let by_fd = |fd: &std::os::fd::OwnedFd, rest: &str| {
        // alloc: as above, once per sandbox.
        PathBuf::from(format!("/proc/self/fd/{}{rest}", std::os::fd::AsRawFd::as_raw_fd(fd)))
    };
    for (at, (dir, lower)) in spec.overlays.iter().zip(&lowers).enumerate() {
        // alloc: the error, once, and no sandbox is made.
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        // alloc: once per overlay, its directories' names.
        let upper = by_fd(&staged, &format!("/{at}/upper"));
        // alloc: as `upper`'s.
        let work = by_fd(&staged, &format!("/{at}/work"));
        overlay(dir, &by_fd(lower, ""), &upper, &work)?;
    }
    // The readable directories are bound back in place, read-only as their mount now is, where
    // an empty directory hid them; the writable ones go on top.
    for (dir, fd) in spec.readable.iter().zip(&readable) {
        // alloc: the error, once, and no sandbox is made.
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let (from, to) = (c(&by_fd(fd, "")), c(dir));
        let flags = libc::MS_BIND | libc::MS_REC;
        if raw::mount(Some(from.as_c_str()), &to, None, flags, None) < 0 {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&format!("{} seen read-only", dir.display())));
        }
    }
    for (dir, fd) in spec.writable.iter().zip(&writable) {
        // alloc: the error, once, and no sandbox is made.
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let (from, to) = (c(&by_fd(fd, "")), c(dir));
        let flags = libc::MS_BIND | libc::MS_REC;
        if raw::mount(Some(from.as_c_str()), &to, None, flags, None) < 0 {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&format!("{} written for real", dir.display())));
        }
        // A bind keeps the read-only mark its source was given above.
        let attr =
            MountAttr { attr_set: 0, attr_clr: MOUNT_ATTR_RDONLY, propagation: 0, userns_fd: 0 };
        if raw::mount_setattr(&to, AT_RECURSIVE, &attr) < 0 {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&format!("{} written for real (mount_setattr)", dir.display())));
        }
    }
    drop((lowers, staged, writable, readable));
    // A `TMPDIR` in one of the empty directories is there, empty as they are.
    let tmp = spec.env.iter().find(|(name, _)| name == "TMPDIR").map(|(_, it)| Path::new(it));
    if let Some(tmp) =
        tmp.filter(|it| ["/tmp", "/var/tmp", "/dev/shm"].iter().any(|dir| it.starts_with(dir)))
    {
        // alloc: the error, once, and no sandbox is made.
        std::fs::create_dir_all(tmp).map_err(|error| format!("{}: {error}", tmp.display()))?;
    }
    Ok(())
}

/// Mounts at `dir` what is in `lower`, with its changes going to `upper`.
fn overlay(dir: &Path, lower: &Path, upper: &Path, work: &Path) -> Result<(), String> {
    // A path in an overlay's options has its commas, colons and backslashes escaped.
    let escaped = |out: &mut Vec<u8>, it: &Path| {
        for &byte in it.as_os_str().as_bytes() {
            if matches!(byte, b',' | b':' | b'\\') {
                out.push(b'\\');
            }
            out.push(byte);
        }
    };
    let (kind, at) = (c(Path::new("overlay")), c(dir));
    let mut last = String::with_capacity(0);
    // `userxattr` keeps what overlayfs notes in the user's attributes, which a user
    // namespace may write; a kernel or a tmpfs without them takes none.
    for extra in [&b",userxattr"[..], b""] {
        // The options the kernel is given, once per overlay.
        let paths = [lower, upper, work].map(|it| it.as_os_str().len());
        let mut options = Vec::with_capacity(64 + 2 * paths.iter().sum::<usize>());
        options.extend_from_slice(b"lowerdir=");
        escaped(&mut options, lower);
        options.extend_from_slice(b",upperdir=");
        escaped(&mut options, upper);
        options.extend_from_slice(b",workdir=");
        escaped(&mut options, work);
        options.extend_from_slice(extra);
        let options = CString::new(options).unwrap_or_default();
        if raw::mount(
            Some(kind.as_c_str()),
            &at,
            Some(kind.as_c_str()),
            0,
            Some(options.as_c_str()),
        ) == 0
        {
            return Ok(());
        }
        // alloc: the error, at most twice, and no sandbox is made when both fail.
        last = os(&format!("an overlay on {}", dir.display()));
    }
    Err(last)
}

/// Where a view's root is put together before it becomes `/`: the machine's `/tmp`, which the
/// view's tmpfs hides in the mount namespace.
const VIEW_ROOT: &str = "/tmp";

/// The machine's devices a view's `/dev` has: those that hold nothing of the machine's.
const DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// The links of a view's `/dev`, into its own `/proc`.
const DEVICE_LINKS: &[(&str, &str)] = &[
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
];

/// What makes `view`, in a spec with `spec`'s other directories, one that can't be made:
/// checked before the sandbox is, so that a bind whose source is not there fails saying so
/// rather than with the sandbox's own error.
fn check_view(spec: &Spec, view: &View) -> Result<(), String> {
    use std::path::Component;
    if !(spec.overlays.is_empty() && spec.writable.is_empty() && spec.readable.is_empty()) {
        return Err("a sandbox with a view sees what its binds name and nothing else: its \
                    overlays, writable and readable directories are to be binds of the view"
            // alloc: the error, once, and no sandbox is made.
            .to_owned());
    }
    let mut seen = std::collections::BTreeSet::new();
    for bind in &view.binds {
        let (inside, outside) = (bind.inside.display(), bind.outside.display());
        let whole = bind.inside.is_absolute()
            && bind
                .inside
                .components()
                .all(|it| matches!(it, Component::RootDir | Component::Normal(_)));
        let inside_tidy = tidy(&bind.inside);
        if !whole || inside_tidy == "/" {
            // alloc: the error, once, and no sandbox is made.
            return Err(format!(
                "the view binds {outside} at {inside}, which is not a whole path \
                 below `/` with no `..`"
            ));
        }
        if ["/proc", "/dev"].iter().any(|it| super::under(&inside_tidy, it)) {
            // alloc: the error, once, and no sandbox is made.
            return Err(format!(
                "the view binds {outside} at {inside}, which is in the view's own \
                 `/proc` or `/dev`"
            ));
        }
        if !seen.insert(inside_tidy) {
            // alloc: the error, once, and no sandbox is made.
            return Err(format!("the view binds two things at {inside}"));
        }
        if let Err(error) = std::fs::metadata(&bind.outside) {
            // alloc: the error, once, and no sandbox is made.
            return Err(format!(
                "the view binds {outside} at {inside}, and {outside} is not there \
                 to be bound ({error})"
            ));
        }
    }
    Ok(())
}

/// Puts `view` together at [`VIEW_ROOT`], on a tmpfs of its own, for the user `uid` of the
/// group `gid`, with the directory of `env`'s `TMPDIR` made when it is in its `/tmp` or
/// `/dev/shm`. The first process of the PID namespace mounts its `/proc` and makes it `/`
/// ([`enter_view`]).
fn put_view(
    view: &View,
    uid: libc::uid_t,
    gid: libc::gid_t,
    env: &[(String, String)],
) -> Result<(), String> {
    use std::os::fd::AsRawFd as _;
    // The sources are reached through descriptors opened before the view's tmpfs hides the
    // machine's `/tmp`, where one may be.
    let sources = view
        .binds
        .iter()
        .map(|bind| {
            let path = c(&bind.outside);
            raw::open(&path, libc::O_PATH | libc::O_CLOEXEC).ok_or_else(|| {
                // alloc: the error, once, and no sandbox is made.
                os(&format!("{}, bound at {}", bind.outside.display(), bind.inside.display()))
            })
        })
        // alloc: once per sandbox, a descriptor of each bind's source.
        .collect::<Result<Vec<_>, _>>()?;
    let root = Path::new(VIEW_ROOT);
    let made = |it: std::io::Result<()>, at: &Path| {
        // alloc: the error, once, and no sandbox is made.
        it.map_err(|error| format!("{}: {error}", at.display()))
    };
    let tmpfs = |at: &Path, flags: libc::c_ulong, mode: &str| -> Result<(), String> {
        let (kind, at_c, options) = (c(Path::new("tmpfs")), c(at), c(Path::new(mode)));
        if raw::mount(
            Some(kind.as_c_str()),
            &at_c,
            Some(kind.as_c_str()),
            flags,
            Some(options.as_c_str()),
        ) < 0
        {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&format!("the view's {}", at.display())));
        }
        Ok(())
    };
    tmpfs(root, libc::MS_NOSUID | libc::MS_NODEV, "mode=0755")?;
    for dir in ["proc", "dev", "etc", "tmp"] {
        made(std::fs::create_dir(root.join(dir)), &root.join(dir))?;
    }
    tmpfs(&root.join("tmp"), libc::MS_NOSUID | libc::MS_NODEV, "mode=1777")?;
    let dev = root.join("dev");
    tmpfs(&dev, libc::MS_NOSUID | libc::MS_NOEXEC, "mode=0755")?;
    for name in DEVICES {
        let at = dev.join(name);
        made(std::fs::File::create(&at).map(drop), &at)?;
        bind(&Path::new("/dev").join(name), &at, 0)?;
    }
    for (name, to) in DEVICE_LINKS {
        made(std::os::unix::fs::symlink(to, dev.join(name)), &dev.join(name))?;
    }
    made(std::fs::create_dir(dev.join("shm")), &dev.join("shm"))?;
    tmpfs(&dev.join("shm"), libc::MS_NOSUID | libc::MS_NODEV, "mode=1777")?;
    let (user, group) = match (uid, gid) {
        (0, 0) => ("root", "root"),
        (0, _) => ("root", "blaze"),
        (_, 0) => ("blaze", "root"),
        _ => ("blaze", "blaze"),
    };
    // The view's `/etc`, once per sandbox.
    let etc = [
        (
            "passwd",
            // alloc: as above.
            format!(
                "{user}:x:{uid}:{gid}:{user}:/homeless-shelter:/noshell\n\
                 nobody:x:65534:65534:nobody:/:/noshell\n"
            ),
        ),
        // alloc: as above.
        ("group", format!("{group}:x:{gid}:\nnogroup:x:65534:\n")),
        // alloc: as above.
        ("hosts", "127.0.0.1 localhost\n::1 localhost\n".to_owned()),
    ];
    for (name, text) in etc {
        let at = root.join("etc").join(name);
        made(std::fs::write(&at, text), &at)?;
    }
    // Outer binds first, so that one inside another lands on what the outer one shows.
    // alloc: once per sandbox, the order of its binds.
    let mut order: Vec<usize> = (0..view.binds.len()).collect();
    blaze_ids::radix::sort_by_key(&mut order, |&at| view.binds[at].inside.components().count());
    let mut bound: Vec<String> = Vec::with_capacity(view.binds.len());
    for at in order {
        let (it, source) = (&view.binds[at], &sources[at]);
        let Some(stat) = raw::stat_of(source.as_raw_fd()) else {
            // alloc: the error, once, and no sandbox is made.
            return Err(os(&it.outside.display().to_string()));
        };
        let dir = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
        let to = mount_point(root, &it.inside, dir, &bound)?;
        // alloc: once per bind, the path its source is reached by.
        let from = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
        let read_only = if it.writable { 0 } else { MOUNT_ATTR_RDONLY };
        bind(&from, &to, MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | read_only).map_err(|error| {
            // alloc: the error, once, and no sandbox is made.
            format!("binding {} at {}: {error}", it.outside.display(), it.inside.display())
        })?;
        bound.push(tidy(&it.inside));
    }
    drop(sources);
    let tmp = env.iter().find(|(name, _)| name == "TMPDIR").map(|(_, it)| Path::new(it));
    if let Some(tmp) = tmp.filter(|it| ["/tmp", "/dev/shm"].iter().any(|dir| it.starts_with(dir))) {
        let at = root.join(tmp.strip_prefix("/").unwrap_or(tmp));
        made(std::fs::create_dir_all(&at), &at)?;
    }
    for at in [dev.as_path(), root] {
        set_attr(at, MOUNT_ATTR_RDONLY, false)
            // alloc: the error, once, and no sandbox is made.
            .map_err(|error| format!("the view's {} read-only: {error}", at.display()))?;
    }
    Ok(())
}

/// Where `inside` is below the view's `root` that is being put together, made as an empty
/// directory or file (as `dir` says) where it is not there yet. Nothing is made inside what
/// `bound` already binds, which is the machine's, and no link is followed, which could lead
/// out of the view.
fn mount_point(root: &Path, inside: &Path, dir: bool, bound: &[String]) -> Result<PathBuf, String> {
    let parts = inside.components().skip(1).count();
    let room = root.as_os_str().len() + 1 + inside.as_os_str().len();
    let mut at = PathBuf::with_capacity(room);
    at.push(root);
    let mut seen = PathBuf::with_capacity(inside.as_os_str().len() + 1);
    seen.push("/");
    for (count, part) in inside.components().skip(1).enumerate() {
        at.push(part);
        seen.push(part);
        let last = count + 1 == parts;
        match at.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                // alloc: the error, once, and no sandbox is made.
                return Err(format!(
                    "the view binds something at {}, and {} is a link in what is bound there",
                    inside.display(),
                    seen.display()
                ));
            }
            Ok(meta) if meta.is_dir() == (dir || !last) => {}
            Ok(_) => {
                // alloc: the error, once, and no sandbox is made.
                return Err(format!(
                    "the view binds a {} at {}, and {} is not one",
                    if dir || !last { "directory" } else { "file" },
                    inside.display(),
                    seen.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let seen_s = tidy(&seen);
                if let Some(outer) = bound.iter().find(|it| super::under(&seen_s, it)) {
                    // alloc: the error, once, and no sandbox is made.
                    return Err(format!(
                        "the view binds something at {}, inside what it binds at {outer}, which \
                         has no {}",
                        inside.display(),
                        seen.display()
                    ));
                }
                let made = match dir || !last {
                    true => std::fs::create_dir(&at),
                    false => std::fs::File::create(&at).map(drop),
                };
                // alloc: the error, once, and no sandbox is made.
                made.map_err(|error| format!("{}: {error}", at.display()))?;
            }
            // alloc: the error, once, and no sandbox is made.
            Err(error) => return Err(format!("{}: {error}", at.display())),
        }
    }
    Ok(at)
}

/// Binds `from` at `to`, with what is mounted below it, and sets `attr` on all of it.
fn bind(from: &Path, to: &Path, attr: u64) -> Result<(), String> {
    let (from_c, to_c) = (c(from), c(to));
    let flags = libc::MS_BIND | libc::MS_REC;
    if raw::mount(Some(from_c.as_c_str()), &to_c, None, flags, None) < 0 {
        // alloc: the error, once, and no sandbox is made.
        return Err(os(&format!("binding {} at {}", from.display(), to.display())));
    }
    if attr != 0 {
        set_attr(to, attr, true)?;
    }
    Ok(())
}

/// Sets `attr` on the mount at `at`, and on those below it when `recursive`.
fn set_attr(at: &Path, attr: u64, recursive: bool) -> Result<(), String> {
    let at_c = c(at);
    let attr = MountAttr { attr_set: attr, attr_clr: 0, propagation: 0, userns_fd: 0 };
    let flags = if recursive { AT_RECURSIVE } else { 0 };
    if raw::mount_setattr(&at_c, flags, &attr) < 0 {
        // alloc: the error, once, and no sandbox is made.
        return Err(os(&format!("{} (mount_setattr)", at.display())));
    }
    Ok(())
}

/// Makes the view put together at [`VIEW_ROOT`] the root of every process of the mount
/// namespace, and detaches the machine's tree from it: no path, `..`, link or `/proc` entry
/// of the program's reaches the machine's file system after.
fn enter_view() -> Result<(), String> {
    let (root, here) = (c(Path::new(VIEW_ROOT)), c(Path::new(".")));
    if raw::chdir(&root) < 0 {
        return Err(os("the view's root"));
    }
    // The old root goes on top of the new one, and is taken off at once.
    if raw::pivot_root(&here, &here) < 0 {
        return Err(os("making the view the root (pivot_root)"));
    }
    if raw::umount_detach(&here) < 0 {
        return Err(os("detaching the machine's root from the view"));
    }
    let slash = c(Path::new("/"));
    if raw::chdir(&slash) < 0 {
        return Err(os("the view's root"));
    }
    Ok(())
}

/// Closes this process's descriptors that a program in a view could reach the machine's file
/// system through: a directory (whose `..` leads out), and above the standard three a file
/// of the machine's. What is left is pipes, sockets and devices, a jobserver's among them.
#[allow(unsafe_code, reason = "close by number of what /proc/self/fd lists")]
fn close_machine_files() {
    let fds: Vec<libc::c_int> = match std::fs::read_dir("/proc/self/fd") {
        // alloc: once, in the program's process just before it runs the program.
        Ok(it) => it.flatten().filter_map(|it| it.file_name().to_str()?.parse().ok()).collect(),
        Err(_) => return,
    };
    for fd in fds {
        let Some(stat) = raw::stat_of(fd) else {
            continue;
        };
        let kind = stat.st_mode & libc::S_IFMT;
        let file = matches!(kind, libc::S_IFREG | libc::S_IFBLK | libc::S_IFLNK);
        if kind == libc::S_IFDIR || (fd > 2 && file) {
            // SAFETY: this is the program's process just before it runs the program (`exec`).
            // The `OwnedFd`s `machine` and `put_view` opened were dropped before the forks, and
            // what other owner it has of such a descriptor (the recorder's live file, copied
            // at the fork) is never used or dropped again: next it runs the program or ends by
            // `raw::exit`, which runs no destructor. Nothing uses `fd` after.
            unsafe { raw::close(fd) };
        }
    }
}

/// Brings up the network namespace's loopback, which a new one has down.
fn loopback_up() -> Result<(), String> {
    let Some(socket) = raw::datagram_socket() else {
        return Err(os("a socket for the loopback"));
    };
    let mut request = IfReq { name: [0; 16], flags: 0, pad: [0; 22] };
    request.name[..2].copy_from_slice(b"lo");
    let up = raw::interface_flags(socket.as_fd(), &mut request) >= 0 && {
        request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        raw::set_interface_flags(socket.as_fd(), &request) >= 0
    };
    let error = os("bringing up the loopback");
    drop(socket);
    if up { Ok(()) } else { Err(error) }
}

/// The first process of the PID namespace: mounts its `/proc` (in a view, and makes the view
/// the root), forks the program's process and reaps every process left to it until that one
/// ends, ending as it did.
#[allow(unsafe_code, reason = "fork, and close of the supervisor's socket by number")]
fn first_process(cwd: &CString, made: &Made<'_>, probe: bool) -> Result<i32, String> {
    let kind = c(Path::new("proc"));
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    if made.viewed {
        // Before the machine's tree is detached: the kernel mounts a `/proc` in a user
        // namespace only where one is seen whole already.
        let proc_ = c(&Path::new(VIEW_ROOT).join("proc"));
        if raw::mount(Some(kind.as_c_str()), &proc_, Some(kind.as_c_str()), flags, None) < 0 {
            return Err(os("the view's /proc"));
        }
        enter_view()?;
    } else {
        let proc_ = c(Path::new("/proc"));
        // Where `/proc` can't be mounted again, the one there still shows each process
        // itself as `/proc/self`.
        raw::mount(Some(kind.as_c_str()), &proc_, Some(kind.as_c_str()), flags, None);
    }
    if raw::chdir(cwd) < 0 {
        // alloc: the error, once, and no sandbox is made.
        return Err(os(&format!("the working directory {}", cwd.to_string_lossy())));
    }
    if probe {
        return Ok(0);
    }
    // SAFETY: this is the PID namespace's first process, forked from the sandbox's first
    // process, which is one thread (`sandbox`), so it is one thread too. So the child need not
    // keep to async-signal-safe calls: it installs the filter, allocates and prints its error.
    let program = unsafe { raw::fork() };
    if program < 0 {
        return Err(os("fork"));
    }
    if program == 0 {
        let error = exec(made);
        eprintln!("blaze: could not run {}: {error}", made.argv[0].to_string_lossy());
        raw::exit(127);
    }
    // SAFETY: `made.socket` is the child's end of the supervisor's pair, owned by no
    // `OwnedFd`; this process never uses it after, and the program's process has its own copy.
    unsafe { raw::close(made.socket) };
    loop {
        let mut status = 0;
        let pid = raw::wait(-1, &mut status, 0);
        if pid == program {
            return Ok(ended(status));
        }
        if pid < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
}

/// Installs the filter, when there is one, hands its descriptor to the supervisor and runs
/// the program; what kept it from running.
#[allow(unsafe_code, reason = "close of the supervisor's socket by number")]
fn exec(made: &Made<'_>) -> String {
    if made.viewed {
        close_machine_files();
    }
    if let Some(program) = made.filter {
        let listener = match install(program) {
            Ok(it) => it,
            Err(error) => return error,
        };
        if let Err(error) = send(made.socket, listener.as_raw_fd()) {
            return error;
        }
        // The supervisor has its own copy now, and the program is not to have it.
        drop(listener);
    }
    // SAFETY: `made.socket` is this process's copy of the child's end of the supervisor's
    // pair, owned by no `OwnedFd`; the program is not to have it, and nothing uses it after.
    unsafe { raw::close(made.socket) };
    // The program is often the file blaze just wrote: an exec that meets a writer's
    // descriptor not yet closed (`ETXTBSY`) is made again on blaze-core's schedule.
    let (mut tried, mut waited) = (0, 0);
    loop {
        raw::execve(made.argv, made.env);
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ETXTBSY) {
            // alloc: the error, once, and the program's process ends.
            return error.to_string();
        }
        let Some(wait) = blaze_core::busy::next(tried, waited) else {
            // alloc: as above.
            return format!(
                "still open for writing after {} ms of tries: {error}",
                blaze_core::busy::MOST_NS / 1_000_000
            );
        };
        blaze_core::busy::wait(wait);
        tried += 1;
        waited += wait;
    }
}

/// The seccomp filter of a run. On every run it refuses `TIOCSTI` and `TIOCLINUX`
/// (`EPERM`), `io_uring_setup` and another architecture's calls (`ENOSYS`, as a kernel without
/// them answers, so that a program falls back to the calls seccomp sees), and has the kernel
/// ask the supervisor about each `connect` and each `sendto` with an address. When the run is
/// `recorded` it asks about the calls of [`CALLS`] the record needs too: all of them when
/// `reads`, otherwise those that can write, the network's and `access` with `F_OK` (the test
/// marker). A `sendmsg` on `socket` is never asked about: it is how the filter's own
/// descriptor reaches the supervisor, after the filter is installed and before anything
/// listens. (A program that later has a socket of the same number sends on it unasked; the
/// network namespace has only the loopback, so it reaches no address outside, and a Unix
/// socket of the machine's by `sendmsg` is the residual docs/cargo.md names.) A `confined` run
/// has no network namespace and no PID namespace: it asks about every call of the network, as
/// a recorded run does, and about every signal; a run with the `network` has no network
/// namespace either, and asks about every call of the network too. When `reads` are recorded
/// it asks about listings, randomness and the realtime clock as well.
fn filter_program(
    recorded: bool,
    reads: bool,
    confined: bool,
    network: bool,
    socket: libc::c_int,
) -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const JSET_K: u16 = 0x45;
    const RET_K: u16 = 0x06;
    let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    // Where in `struct seccomp_data` the low and the high half of an argument are, on a
    // little-endian machine.
    let low = |arg: usize| 16 + 8 * arg as u32;
    let high = |arg: usize| 20 + 8 * arg as u32;
    let (notify, allow) =
        (insn(RET_K, 0, 0, SECCOMP_RET_USER_NOTIF), insn(RET_K, 0, 0, SECCOMP_RET_ALLOW));
    let errno = |it: i32| insn(RET_K, 0, 0, SECCOMP_RET_ERRNO | it as u32);
    // The head, then at most 9 instructions for each call, then the last.
    let mut program = Vec::with_capacity(16 + 9 * CALLS.len());
    program.extend([
        insn(LD_W_ABS, 0, 0, 4), // the architecture
        insn(JEQ_K, 1, 0, ARCH),
        errno(libc::ENOSYS),
        insn(LD_W_ABS, 0, 0, 0), // the call
        insn(JGE_K, 0, 1, X32),
        errno(libc::ENOSYS),
        // `ioctl`'s request is an `unsigned int`: the kernel reads the low half alone.
        insn(JEQ_K, 0, 5, NR_IOCTL),
        insn(LD_W_ABS, 0, 0, low(1)),
        insn(JEQ_K, 2, 0, TIOCSTI),
        insn(JEQ_K, 1, 0, TIOCLINUX),
        allow,
        errno(libc::EPERM),
        insn(JEQ_K, 0, 1, NR_IO_URING_SETUP),
        errno(libc::ENOSYS),
    ]);
    // A null address is a connected socket's message.
    let sendto_with_address = [
        insn(LD_W_ABS, 0, 0, low(4)),
        insn(JEQ_K, 1, 0, 0),
        notify,
        insn(LD_W_ABS, 0, 0, high(4)),
        insn(JEQ_K, 0, 1, 0),
        allow,
        notify,
    ];
    let sendmsg = [insn(LD_W_ABS, 0, 0, low(0)), insn(JEQ_K, 0, 1, socket as u32), allow, notify];
    for call in CALLS {
        let start = program.len();
        match (recorded, reads, call.does) {
            (_, _, Does::Signal(_)) if confined => program.push(notify),
            (_, _, Does::Signal(_)) => {}
            (_, _, Does::Connect) => program.push(notify),
            (false, _, Does::SendMsg) if confined || network => program.extend(sendmsg),
            (false, _, Does::SendTo) => program.extend(sendto_with_address),
            (false, _, _) => {}
            (true, _, Does::SendMsg) => program.extend(sendmsg),
            (true, true, Does::Clock(Some(id))) => {
                program.push(insn(LD_W_ABS, 0, 0, low(id)));
                for (at, clock) in REALTIME.iter().enumerate() {
                    // To the `notify` after the last comparison and the `allow`.
                    program.push(insn(JEQ_K, (REALTIME.len() - at) as u8, 0, *clock));
                }
                program.extend([allow, notify]);
            }
            (true, true, _) | (true, false, Does::Change | Does::Address) => program.push(notify),
            (true, false, Does::Look | Does::Run) => {}
            (true, false, Does::List(_) | Does::Random | Does::Clock(_)) => {}
            (true, false, Does::Open(flags)) => program.extend([
                insn(LD_W_ABS, 0, 0, low(flags)),
                insn(JSET_K, 0, 1, WRITES),
                notify,
                allow,
            ]),
            (true, false, Does::Access(mode)) => program.extend([
                insn(LD_W_ABS, 0, 0, low(mode)),
                insn(JEQ_K, 0, 1, 0),
                notify,
                allow,
            ]),
            (true, false, Does::SendTo) => program.extend(sendto_with_address),
        }
        // A call not asked about has no block. Past the block when it is another call: every
        // block returns, so the call's number is still what the next comparison reads.
        let block = program.len() - start;
        if block > 0 {
            program.insert(start, insn(JEQ_K, 0, block as u8, call.nr));
        }
    }
    program.push(allow);
    program
}

/// Installs `program` as this process's seccomp filter, and the descriptor the kernel asks
/// through.
fn install(program: &[libc::sock_filter]) -> Result<OwnedFd, String> {
    // The process holds every capability of its user namespace until it runs the program, so
    // it may install a filter without `no_new_privs`; a confined one has set it.
    let Some(listener) = raw::install_filter(program) else {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
            return Err("a seccomp filter: this process is in a sandbox already, whose seccomp \
                        listener the kernel takes as the only one a process has; run this \
                        outside it (a test with `unsandboxed-tests`)"
                // alloc: the error, once, and the program does not run.
                .to_owned());
        }
        return Err(os("a seccomp filter"));
    };
    Ok(listener)
}

/// Sends the descriptor `fd` over `socket`.
fn send(socket: libc::c_int, fd: libc::c_int) -> Result<(), String> {
    if raw::send_descriptor(socket, fd) < 0 {
        return Err(os("handing over the filter"));
    }
    Ok(())
}

/// Answers the kernel's notifications through `listener`, noting what each call reached,
/// until the process `first` has ended, or no process is left that the filter is on, the
/// program seeing what `around` says.
fn supervise(listener: libc::c_int, first: libc::pid_t, recorder: &mut Recorder, watch: &mut Watch) {
    // The kernel wakes the supervisor on the CPU of the thread it holds and switches to it
    // there, as gVisor has it, rather than waking it as any other waiter. A kernel before 6.6
    // has no such flag and refuses it: the calls are answered the same, each a wake-up later.
    raw::set_notif_flags(listener, SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP);
    let mut scratch = Scratch::default();
    let pidfd = raw::pidfd_open(first);
    // `poll` passes over a negative descriptor, whose `revents` stays 0: no pidfd.
    let ended_fd = pidfd.as_ref().map_or(-1, |it| it.as_raw_fd());
    let mut fds = [
        libc::pollfd { fd: listener, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: ended_fd, events: libc::POLLIN, revents: 0 },
    ];
    loop {
        fds.iter_mut().for_each(|it| it.revents = 0);
        let ready = raw::poll(&mut fds, -1);
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            answer(listener, recorder, watch, &mut scratch);
            continue;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 || fds[1].revents != 0 {
            break;
        }
    }
    drop(pidfd);
}

/// What the supervisor keeps beside the record.
struct Watch {
    around: Around,
    /// Whether the program may reach the network ([`Also::network`]).
    network: bool,
    /// The process of each thread seen, by the thread's id.
    processes: HashMap<u64, u64>,
}

/// The supervisor's buffers, kept from one notification to the next: a path a call names is
/// read, made whole and noted, an address read and named, what `/proc` says of a thread read
/// and a Unix socket's links followed, without allocating once each has the room.
#[derive(Default)]
struct Scratch {
    /// The path as the program named it, or the link `/proc` has for a descriptor.
    raw: Vec<u8>,
    /// The directory a relative path is in, then the path in it.
    joined: Vec<u8>,
    /// The path made whole.
    path: String,
    /// What is read of the program's memory beside a path: an address, a message's header.
    memory: Vec<u8>,
    /// A path as the program sees it, through `/proc/PID/root`.
    inside: Vec<u8>,
    /// A path with its nul, for the kernel.
    room: Vec<u8>,
    /// What a refused line names: an address as text.
    text: String,
    procs: Procs,
    resolving: Resolving,
}

/// The supervisor's buffers for what `/proc` says of a process.
#[derive(Default)]
struct Procs {
    /// A file of `/proc`, read whole.
    file: Vec<u8>,
    /// A directory of `/proc`, as `getdents64` lists it.
    listing: Vec<u8>,
    /// The threads of a process.
    tasks: Vec<u64>,
    /// The threads of a process, while `tasks` has those of the one being walked.
    others: Vec<u64>,
}

/// The supervisor's buffers for following a Unix socket's links ([`resolved`]).
#[derive(Default)]
struct Resolving {
    /// What is resolved so far, links followed: a whole path.
    done: Vec<u8>,
    /// What is left to resolve, from its front.
    rest: Vec<u8>,
    /// What is left after a link, as it is put together.
    spare: Vec<u8>,
    /// A link's target.
    link: Vec<u8>,
    /// The path a link is read at, through `/proc/PID/root`, with its nul.
    at: Vec<u8>,
    /// The path resolved, made whole.
    out: String,
}

impl Watch {
    /// The process `thread` is of. A thread not seen before starts in no scope, but the first
    /// thread of a new process: in the scope the thread that forked it had when it did, which
    /// is the one it has now, its next mark being taken only once the processes it forked
    /// before have theirs ([`Watch::forked`]).
    fn process(&mut self, recorder: &mut Recorder, thread: u64, procs: &mut Procs) -> u64 {
        if let Some(&process) = self.processes.get(&thread) {
            return process;
        }
        let (process, parent) = ids(thread, &mut procs.file).unwrap_or((thread, 0));
        let scope = match (process == thread, parent > 0) {
            (true, true) => {
                forker(parent, thread, procs).map_or(Scope::NONE, |it| recorder.scope_of(it))
            }
            (true, false) => Scope::NONE,
            (false, _) => shared_scope(recorder, process, thread, procs),
        };
        recorder.starts(thread, scope);
        self.processes.insert(thread, process);
        process
    }

    /// `thread` of `process` is about to take another mark, in a call the kernel holds until
    /// it is answered: each process it forked that is not seen yet starts in the scope it has
    /// before it does.
    fn forked(&mut self, recorder: &mut Recorder, thread: u64, process: u64, procs: &mut Procs) {
        let mut name = [0u8; 64];
        let children = proc_path(&mut name, format_args!("/proc/{process}/task/{thread}/children"));
        let listed = children.is_some_and(|it| read_file(it, &mut procs.file));
        let scope = recorder.scope_of(thread);
        if let (true, Ok(children)) = (listed, std::str::from_utf8(&procs.file)) {
            for child in children.split_whitespace().filter_map(|it| it.parse().ok()) {
                if !recorder.knows(child) {
                    recorder.starts(child, scope);
                    self.processes.insert(child, child);
                }
            }
        }
        // The threads it made before this mark start in the scope they were made in.
        let mut threads = std::mem::take(&mut procs.tasks);
        tasks(process, &mut procs.listing, &mut threads);
        for &task in &threads {
            if task == thread || recorder.knows(task) {
                continue;
            }
            let scope = shared_scope(recorder, process, task, procs);
            recorder.starts(task, scope);
            self.processes.insert(task, process);
        }
        procs.tasks = threads;
    }
}

/// A path of `/proc` as `args` writes it, with its nul, in `room`; `None` when it does not fit.
fn proc_path<'a>(room: &'a mut [u8], args: std::fmt::Arguments<'_>) -> Option<&'a CStr> {
    use std::io::Write as _;
    let size = room.len();
    let mut at = &mut room[..];
    at.write_fmt(args).ok()?;
    at.write_all(b"\0").ok()?;
    let left = at.len();
    CStr::from_bytes_with_nul(&room[..size - left]).ok()
}

/// The file of `/proc` at `path`, read whole into `into`; false when it can't be.
fn read_file(path: &CStr, into: &mut Vec<u8>) -> bool {
    into.clear();
    let Some(fd) = raw::open(path, libc::O_RDONLY | libc::O_CLOEXEC) else { return false };
    loop {
        if into.capacity() - into.len() < 1024 {
            into.reserve(4096);
        }
        match raw::read(fd.as_fd(), into) {
            0 => return true,
            read if read > 0 => {}
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

/// The entries of the directory of `/proc` at `path` that are numbers (its processes, or a
/// process's threads), into `into`, listed into `listing`: none when it can't be opened, and
/// those listed before an error stops the listing.
fn numbered(path: &CStr, listing: &mut Vec<u8>, into: &mut Vec<u64>) {
    // `struct linux_dirent64`: `d_reclen` at 16, `d_name` at 19, nul-terminated.
    const NAME: usize = 19;
    into.clear();
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let Some(fd) = raw::open(path, flags) else { return };
    loop {
        listing.clear();
        listing.reserve(4096);
        let read = raw::dents(fd.as_fd(), listing);
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if read <= 0 {
            return;
        }
        let mut at = 0;
        while let Some(entry) = listing.get(at..at + NAME) {
            let size = usize::from(u16::from_ne_bytes([entry[16], entry[17]]));
            let Some(name) = listing.get(at + NAME..at + size) else { break };
            let name = &name[..name.iter().position(|&it| it == 0).unwrap_or(name.len())];
            if let Some(id) = std::str::from_utf8(name).ok().and_then(|it| it.parse().ok()) {
                into.push(id);
            }
            at += size;
        }
    }
}

/// The threads of `process`, by `/proc`, into `into`; none when it is gone.
fn tasks(process: u64, listing: &mut Vec<u8>, into: &mut Vec<u64>) {
    let mut name = [0u8; 64];
    match proc_path(&mut name, format_args!("/proc/{process}/task")) {
        Some(path) => numbered(path, listing, into),
        None => into.clear(),
    }
}

/// The scope a new `thread` of `process` starts in: the one every other thread of it is in,
/// when each is known and all are in one, so whichever made it was in that one; otherwise
/// none, which is every scope's.
fn shared_scope(recorder: &Recorder, process: u64, thread: u64, procs: &mut Procs) -> Scope {
    tasks(process, &mut procs.listing, &mut procs.others);
    let mut shared = None;
    for &other in procs.others.iter().filter(|it| **it != thread) {
        if !recorder.knows(other) {
            return Scope::NONE;
        }
        let scope = recorder.scope_of(other);
        match shared {
            Some(it) if it != scope => return Scope::NONE,
            Some(_) => {}
            None => shared = Some(scope),
        }
    }
    shared.unwrap_or(Scope::NONE)
}

/// The process (the thread group) and the parent process of `thread`, by `/proc`, read into
/// `file`.
fn ids(thread: u64, file: &mut Vec<u8>) -> Option<(u64, u64)> {
    let mut name = [0u8; 64];
    if !read_file(proc_path(&mut name, format_args!("/proc/{thread}/status"))?, file) {
        return None;
    }
    let status = std::str::from_utf8(file).ok()?;
    let field = |name: &str| {
        status.lines().find_map(|it| it.strip_prefix(name)?.trim().parse::<u64>().ok())
    };
    Some((field("Tgid:")?, field("PPid:")?))
}

/// The thread of `parent` that forked `child`, by the children `/proc` lists for each.
fn forker(parent: u64, child: u64, procs: &mut Procs) -> Option<u64> {
    let mut name = [0u8; 64];
    tasks(parent, &mut procs.listing, &mut procs.tasks);
    procs.tasks.iter().copied().find(|&thread| {
        let path = proc_path(&mut name, format_args!("/proc/{parent}/task/{thread}/children"));
        path.is_some_and(|it| read_file(it, &mut procs.file))
            && std::str::from_utf8(&procs.file)
                .is_ok_and(|it| it.split_whitespace().any(|it| it.parse() == Ok(child)))
    })
}

/// Takes one notification, notes what its call reached and lets the call go on.
fn answer(
    listener: libc::c_int,
    recorder: &mut Recorder,
    watch: &mut Watch,
    scratch: &mut Scratch,
) {
    let mut notif = Notif::default();
    if raw::notif_recv(listener, &mut notif) < 0 {
        return;
    }
    let refused = match call_of(notif.nr) {
        Some(call) => noted(&notif, call, recorder, watch, scratch),
        None => None,
    };
    let resp = match refused {
        Some(error) => NotifResp { id: notif.id, val: 0, error: -error, flags: 0 },
        None => {
            NotifResp { id: notif.id, val: 0, error: 0, flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE }
        }
    };
    // A process that ended meanwhile makes it fail, which is nothing to answer.
    raw::notif_send(listener, &resp);
}

/// Notes in `recorder` what the call `notif` of `call` reaches, and the error the sandbox
/// refuses it with, if it does: a connection or a message to a Unix socket outside the
/// sandbox's own directories, which no namespace hides; confined, any other address too, and a
/// signal to a process outside the sandbox; with the network, an abstract Unix socket, which
/// is the machine's then.
fn noted(
    notif: &Notif,
    call: &Call,
    recorder: &mut Recorder,
    watch: &mut Watch,
    scratch: &mut Scratch,
) -> Option<i32> {
    let (pid, thread) = (notif.pid, u64::from(notif.pid));
    let process = watch.process(recorder, thread, &mut scratch.procs);
    match call.does {
        Does::Signal(target) => {
            let Around::Confined(first) = watch.around else { return None };
            let outside = signal_outside(pid, target, notif.args[0], first, &mut scratch.procs.file);
            return outside.then_some(libc::EPERM);
        }
        Does::List(fd) => {
            match proc_link(pid, Some(notif.args[fd] as i32), &mut scratch.raw) {
                true if scratch.raw.starts_with(b"/") => {
                    tidy_into(&scratch.raw, &mut scratch.path);
                    recorder.listed(thread, &scratch.path);
                }
                _ => recorder.incomplete(thread, "a directory listed the sandbox could not follow"),
            }
            return None;
        }
        Does::Random => {
            recorder.random();
            return None;
        }
        Does::Clock(_) => {
            recorder.untraceable(thread, Untraceable::Clock);
            return None;
        }
        _ => {}
    }
    let memory = &mut scratch.memory;
    let reached = match call.does {
        Does::Address | Does::Connect => {
            sockaddr(pid, notif.args[1], notif.args[2] as usize, memory)
        }
        Does::SendTo => sockaddr(pid, notif.args[4], notif.args[5] as usize, memory),
        Does::SendMsg => {
            // `struct msghdr` starts with the address and its length.
            let read = bytes_at(pid, notif.args[1], 12, memory);
            match memory.get(..12).filter(|_| read) {
                Some(it) => {
                    let name = u64::from_ne_bytes(it[..8].try_into().unwrap_or_default());
                    let len = u32::from_ne_bytes(it[8..12].try_into().unwrap_or_default());
                    sockaddr(pid, name, len as usize, memory)
                }
                None => None,
            }
        }
        _ => None,
    };
    // `bind`'s Unix socket is a file it makes, which the mounts refuse where it may not.
    let to_unix = matches!(call.does, Does::Connect | Does::SendTo | Does::SendMsg);
    let confined = matches!(watch.around, Around::Confined(_));
    match reached {
        // With the network every address is the machine's, the loopback too: what comes back
        // is not traced.
        Some(Address::Ip { .. }) if watch.network => {
            recorder.untraceable(thread, Untraceable::Network)
        }
        // Confined, the loopback is the machine's, and every address a call reaches is refused
        // as a network namespace with only its own loopback answers it.
        Some(Address::Ip { at, local }) if confined && to_unix => {
            recorder.refused_net(thread, shown(&mut scratch.text, at));
            return Some(if local { libc::ECONNREFUSED } else { libc::ENETUNREACH });
        }
        Some(Address::Ip { at, local: false }) => {
            recorder.refused_net(thread, shown(&mut scratch.text, at))
        }
        Some(Address::Abstract(name)) if confined || watch.network => {
            scratch.text.clear();
            scratch.text.push_str("unix:@");
            push_lossy(&mut scratch.text, &scratch.memory[name]);
            recorder.refused_net(thread, &scratch.text);
            return Some(if to_unix { libc::ECONNREFUSED } else { libc::EACCES });
        }
        Some(Address::Unix(path)) if to_unix => {
            scratch.raw.clear();
            scratch.raw.extend_from_slice(&scratch.memory[path]);
            let found = whole(pid, None, scratch)
                && resolved(pid, &scratch.path, &mut scratch.resolving);
            if !(found && recorder.may_connect(&scratch.resolving.out)) {
                scratch.text.clear();
                scratch.text.push_str("unix:");
                scratch.text.push_str(match found {
                    true => scratch.resolving.out.as_str(),
                    false => "a path the sandbox could not follow",
                });
                recorder.refused_net(thread, &scratch.text);
                return Some(libc::EACCES);
            }
        }
        _ => {}
    }
    let writes = match call.does {
        Does::Open(flags) => notif.args[flags] as u32 & WRITES != 0,
        Does::Change => true,
        _ => false,
    };
    for &(dir, at, written) in call.paths {
        let dir = dir.map(|it| notif.args[it] as i32);
        if !string_at(pid, notif.args[at], &mut scratch.raw) {
            // `execveat` of the descriptor itself (`fexecve`): what runs is what it is open on.
            if !matches!(call.does, Does::Run) || !recorder.reads() {
                continue;
            }
            if !dir.is_some_and(|it| proc_link(pid, Some(it), &mut scratch.raw)) {
                recorder.incomplete(thread, "a program run from a descriptor the sandbox could not follow");
                continue;
            }
        }
        if matches!(call.does, Does::Access(_)) {
            if scratch.raw.starts_with(MARK.as_bytes()) {
                watch.forked(recorder, thread, process, &mut scratch.procs);
            }
            if recorder.marked(thread, process, &scratch.raw) {
                continue;
            }
        }
        if !whole(pid, dir, scratch) {
            continue;
        }
        let path = scratch.path.as_str();
        if writes && written && !recorder.may_write(path) {
            // In a view what is there is seen through the program's root, not the machine's.
            let refused = match watch.around {
                Around::View => {
                    inside(&mut scratch.inside, pid, path);
                    refused_write(&scratch.inside, &mut scratch.room)
                }
                Around::Namespaces | Around::Confined(_) => {
                    refused_write(path.as_bytes(), &mut scratch.room)
                }
            };
            if refused {
                recorder.refused_write(thread, path);
            }
        }
        match call.does {
            _ if !recorder.reads() => {}
            Does::Look | Does::Access(_) => recorder.looked(thread, path),
            Does::Run => {
                recorder.read(thread, path);
                ran(recorder, pid, thread, process, path, &mut scratch.inside);
            }
            Does::Open(_) | Does::Change if path == "/dev/random" || path == "/dev/urandom" => {
                recorder.random()
            }
            _ => recorder.read(thread, path),
        }
    }
    None
}

/// `thread` of `process` runs the program at `path`, made whole. One that is not there, or
/// can't be run, fails to start, and is no program the interposer has to say it is in. Run by
/// another thread than the process's first, in another scope, its image takes the first's id,
/// whose scope it can't be told to keep: the process is in none from then on.
fn ran(
    recorder: &mut Recorder,
    pid: u32,
    thread: u64,
    process: u64,
    path: &str,
    room: &mut Vec<u8>,
) {
    inside(room, pid, path);
    room.push(0);
    let runnable = CStr::from_bytes_with_nul(room).ok().and_then(|it| raw::stat(it, true));
    let runnable = runnable
        .is_some_and(|it| it.st_mode & libc::S_IFMT == libc::S_IFREG && it.st_mode & 0o111 != 0);
    if !runnable {
        return;
    }
    let elsewhere = thread != process && recorder.scope_of(thread) != recorder.scope_of(process);
    let scope = match elsewhere {
        true => {
            recorder.marked(process, process, MARK.as_bytes());
            Scope::NONE
        }
        false => recorder.scope_of(thread),
    };
    recorder.runs(process, path, scope);
}

/// `path` as the process `pid` sees it, through its root (`/proc/PID/root`), into `into`.
fn inside(into: &mut Vec<u8>, pid: u32, path: &str) {
    use std::io::Write as _;
    into.clear();
    _ = write!(into, "/proc/{pid}/root");
    into.extend_from_slice(path.as_bytes());
}

/// Whether a write to `path`, outside where the program may write, is refused by the sandbox
/// rather than by what is there: the path or its directory is there (and read-only inside).
/// `room` has each path with its nul, for the kernel.
fn refused_write(path: &[u8], room: &mut Vec<u8>) -> bool {
    let stat = |path: &[u8], follow: bool, room: &mut Vec<u8>| {
        room.clear();
        room.extend_from_slice(path);
        room.push(0);
        raw::stat(CStr::from_bytes_with_nul(room).ok()?, follow)
    };
    stat(path, false, room).is_some()
        || parent(path).is_some_and(|it| {
            stat(it, true, room).is_some_and(|it| it.st_mode & libc::S_IFMT == libc::S_IFDIR)
        })
}

/// The directory `path` is in, as `Path::parent` has it for a whole path: `None` for `/`.
fn parent(path: &[u8]) -> Option<&[u8]> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let path = &path[..end];
    if path == b"/" || path.is_empty() {
        return None;
    }
    match path.iter().rposition(|&it| it == b'/') {
        Some(0) => Some(b"/"),
        Some(at) => Some(&path[..at]),
        None => Some(b""),
    }
}

/// `it` as text, in `text`, which keeps its room from one to the next.
fn shown(text: &mut String, it: impl std::fmt::Display) -> &str {
    text.clear();
    _ = write!(text, "{it}");
    text
}

/// An address a call names that the sandbox minds.
enum Address {
    /// An IPv4 or IPv6 address, and whether it is the loopback or unspecified: a network
    /// namespace refuses every other.
    Ip { at: std::net::SocketAddr, local: bool },
    /// A Unix socket's path, as the program named it: where it is in what was read.
    Unix(Range<usize>),
    /// An abstract Unix socket's name (a nul first, not kept): the network namespace's own,
    /// the machine's when there is none. Where it is in what was read.
    Abstract(Range<usize>),
}

/// The address of `len` bytes at `at` in `pid`, read into `bytes`, when it is one the sandbox
/// minds.
fn sockaddr(pid: u32, at: u64, len: usize, bytes: &mut Vec<u8>) -> Option<Address> {
    use std::net::{SocketAddrV4, SocketAddrV6};
    if at == 0 || len < 2 || !bytes_at(pid, at, len.min(128), bytes) {
        return None;
    }
    let family = u16::from_ne_bytes([*bytes.first()?, *bytes.get(1)?]) as i32;
    let port = || u16::from_be_bytes([bytes[2], bytes[3]]);
    match family {
        libc::AF_INET if bytes.len() >= 8 => {
            let ip = std::net::Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
            let local = ip.is_loopback() || ip.is_unspecified();
            Some(Address::Ip { at: SocketAddrV4::new(ip, port()).into(), local })
        }
        libc::AF_INET6 if bytes.len() >= 24 => {
            let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).ok()?);
            let local = ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|it| it.is_loopback() || it.is_unspecified());
            // Shown as `[ip]:port`: no flow label, and no scope id, which would follow `%`.
            Some(Address::Ip { at: SocketAddrV6::new(ip, port(), 0, 0).into(), local })
        }
        libc::AF_UNIX => match bytes.get(2..)? {
            [0, ..] => Some(Address::Abstract(3..bytes.len())),
            path => {
                let len = path.iter().position(|&it| it == 0).unwrap_or(path.len());
                (len > 0).then_some(Address::Unix(2..2 + len))
            }
        },
        _ => None,
    }
}

/// Whether the signal a call of `caller` sends, to the process `target` reads its first
/// argument `arg` as naming, goes outside the confined sandbox whose first process is `first`:
/// to every process, or to one (or the group of one) that is not `first` or below it. A
/// process the sandbox can't follow counts as outside.
fn signal_outside(
    caller: u32,
    target: Target,
    arg: u64,
    first: libc::pid_t,
    file: &mut Vec<u8>,
) -> bool {
    let to = match target {
        // The caller's own group is the sandbox's: its first process is a session of its own.
        Target::Kill => match arg as i32 {
            0 => return false,
            -1 => return true,
            group if group < 0 => -group,
            it => it,
        },
        Target::Process => arg as i32,
        Target::Pidfd => {
            let mut name = [0u8; 64];
            let info = proc_path(&mut name, format_args!("/proc/{caller}/fdinfo/{}", arg as i32));
            let read = info.is_some_and(|it| read_file(it, file));
            let info = std::str::from_utf8(file).ok().filter(|_| read);
            let pid = info.and_then(|it| {
                it.lines().find_map(|it| it.strip_prefix("Pid:")?.trim().parse::<i32>().ok())
            });
            match pid {
                Some(pid) => pid,
                None => return true,
            }
        }
    };
    // What the kernel turns down itself.
    if to <= 0 {
        return false;
    }
    !below(to, first, file)
}

/// Whether the process (or thread) `pid` is `first` or one of its descendants, by the parents
/// `/proc` has, read into `file`. The first process reaps the orphans, so a program's daemon
/// stays below it.
fn below(pid: libc::pid_t, first: libc::pid_t, file: &mut Vec<u8>) -> bool {
    let mut at = pid;
    for _ in 0..4096 {
        if at == first {
            return true;
        }
        if at <= 1 {
            return false;
        }
        let mut name = [0u8; 64];
        let path = proc_path(&mut name, format_args!("/proc/{at}/stat"));
        if !path.is_some_and(|it| read_file(it, file)) {
            return false;
        }
        let Ok(stat) = std::str::from_utf8(file) else { return false };
        // `pid (comm) state ppid ...`, the name being anything up to the last `)`.
        let parent = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1));
        match parent.and_then(|it| it.parse().ok()) {
            Some(parent) => at = parent,
            None => return false,
        }
    }
    false
}

/// `path`, whole, as the process `pid` of the sandbox sees it, with every link on the way
/// followed in its view of the file system (`/proc/PID/root`): where a connection to it goes.
/// Into [`Resolving::out`]; false when a part of it could not be looked at for another reason
/// than its not being there or not being a link, which is refused. (The program may change
/// what is there between this and its call: the residual docs/cargo.md names.)
fn resolved(pid: u32, path: &str, resolving: &mut Resolving) -> bool {
    use std::io::Write as _;
    let Resolving { done, rest, spare, link, at, out } = resolving;
    rest.clear();
    rest.extend_from_slice(path.as_bytes());
    done.clear();
    done.push(b'/');
    // A link's target is shorter than `PATH_MAX`, so it fits whole.
    link.clear();
    link.reserve(libc::PATH_MAX as usize + 1);
    let (mut from, mut links) = (0, 0);
    while from < rest.len() {
        if rest[from] == b'/' {
            // A path, or a link's target, that starts at the root.
            if from == 0 {
                done.truncate(1);
            }
            from += 1;
            continue;
        }
        let end = rest[from..].iter().position(|&it| it == b'/').map_or(rest.len(), |it| from + it);
        match &rest[from..end] {
            b"." => {}
            b".." => {
                let up = done.iter().rposition(|&it| it == b'/').unwrap_or(0);
                done.truncate(up.max(1));
            }
            name => {
                let base = done.len();
                if base > 1 {
                    done.push(b'/');
                }
                done.extend_from_slice(name);
                at.clear();
                _ = write!(at, "/proc/{pid}/root");
                at.extend_from_slice(done);
                at.push(0);
                let Ok(named) = CStr::from_bytes_with_nul(at) else { return false };
                let read = raw::readlink_into(named, link);
                if read >= 0 {
                    if links >= 40 || read as usize >= link.capacity() {
                        return false;
                    }
                    links += 1;
                    // What is left is the link's target, then what was left after it.
                    done.truncate(base);
                    spare.clear();
                    spare.extend_from_slice(link);
                    spare.push(b'/');
                    spare.extend_from_slice(&rest[end..]);
                    std::mem::swap(rest, spare);
                    from = 0;
                    continue;
                }
                let error = std::io::Error::last_os_error().raw_os_error();
                if !matches!(error, Some(libc::EINVAL | libc::ENOENT | libc::ENOTDIR)) {
                    return false;
                }
            }
        }
        from = end;
    }
    tidy_into(done, out);
    true
}

/// `len` bytes of `pid`'s memory at `at` into `into`, as many as are there; false when none
/// are.
fn bytes_at(pid: u32, at: u64, len: usize, into: &mut Vec<u8>) -> bool {
    into.clear();
    into.reserve(len);
    let remote = [libc::iovec { iov_base: at as *mut libc::c_void, iov_len: len }];
    raw::read_memory_into(pid as libc::pid_t, &remote, into) > 0
}

/// Reads the string of `pid`'s memory at `address`, up to its nul and at most a path's 4096
/// bytes, into `into`, in one call. False for an empty one, which is a call on the descriptor
/// itself, or one that can't be read.
fn string_at(pid: u32, address: u64, into: &mut Vec<u8>) -> bool {
    const MOST: usize = 4096;
    into.clear();
    into.reserve(MOST);
    // To the end of the page, which is mapped if the byte at `address` is, then on: the kernel
    // reads one part whole or not at all, and stops at the first it can't.
    let first = MOST - (address as usize % MOST);
    let next = (address + first as u64) as *mut libc::c_void;
    let remote = [
        libc::iovec { iov_base: address as *mut libc::c_void, iov_len: first },
        libc::iovec { iov_base: next, iov_len: MOST - first },
    ];
    let parts = if first == MOST { 1 } else { 2 };
    // `into` is empty with room for `MOST` bytes, and the parts are `MOST` bytes in all.
    if raw::read_memory_into(pid as libc::pid_t, &remote[..parts], into) <= 0 {
        return false;
    }
    if let Ok(string) = std::ffi::CStr::from_bytes_until_nul(into) {
        let len = string.to_bytes().len();
        into.truncate(len);
    }
    !into.is_empty()
}

/// Reads the link `/proc/<pid>/fd/<fd>`, or `/proc/<pid>/cwd` for `None`, into `into`.
fn proc_link(pid: u32, fd: Option<i32>, into: &mut Vec<u8>) -> bool {
    use std::io::Write as _;
    let mut name = [0u8; 48];
    let mut at: &mut [u8] = &mut name;
    let named = match fd {
        Some(fd) => write!(at, "/proc/{pid}/fd/{fd}\0"),
        None => write!(at, "/proc/{pid}/cwd\0"),
    };
    if named.is_err() {
        return false;
    }
    let Ok(name) = std::ffi::CStr::from_bytes_until_nul(&name) else {
        return false;
    };
    into.clear();
    into.reserve(libc::PATH_MAX as usize);
    let read = raw::readlink_into(name, into);
    // A link as long as the room may be cut short.
    if read < 0 || read as usize >= into.capacity() {
        return false;
    }
    true
}

/// [`Scratch::raw`], which `pid` named, made whole into [`Scratch::path`] from the directory
/// `dir` (`AT_FDCWD` or a descriptor, or the working directory when `None`).
fn whole(pid: u32, dir: Option<i32>, scratch: &mut Scratch) -> bool {
    if scratch.raw.starts_with(b"/") {
        tidy_into(&scratch.raw, &mut scratch.path);
        return true;
    }
    if !proc_link(pid, dir.filter(|&it| it != libc::AT_FDCWD), &mut scratch.joined) {
        return false;
    }
    scratch.joined.push(b'/');
    scratch.joined.extend_from_slice(&scratch.raw);
    tidy_into(&scratch.joined, &mut scratch.path);
    true
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        os::unix::ffi::OsStrExt as _,
        path::{Path, PathBuf},
    };

    use crate::sandbox::{Bind, Reached, Spec, View, alone, read_record};

    #[test]
    fn a_unix_socket_is_where_its_links_lead() {
        let dir = std::env::temp_dir().join(format!("blaze-resolved-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("abs")).unwrap();
        std::os::unix::fs::symlink("real/../real", dir.join("rel")).unwrap();
        std::os::unix::fs::symlink("loop", dir.join("loop")).unwrap();
        let pid = std::process::id();
        let dir_s = super::tidy(&std::fs::canonicalize(&dir).unwrap());
        let mut resolving = super::Resolving::default();
        let mut at = |it: &str| {
            let found = super::resolved(pid, &format!("{dir_s}/{it}"), &mut resolving);
            found.then(|| resolving.out.clone())
        };
        assert_eq!(at("abs/s.sock").as_deref(), Some(&*format!("{dir_s}/real/s.sock")));
        assert_eq!(at("rel/s.sock").as_deref(), Some(&*format!("{dir_s}/real/s.sock")));
        assert_eq!(at("missing/../real/s").as_deref(), Some(&*format!("{dir_s}/real/s")));
        assert_eq!(at("loop/s"), None);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_call_is_found_by_its_number_and_no_other_number_is_one() {
        for call in super::CALLS {
            assert!(std::ptr::eq(super::call_of(call.nr as i32).unwrap(), call), "{}", call.nr);
        }
        let numbers: Vec<i32> = super::CALLS.iter().map(|it| it.nr as i32).collect();
        for nr in (-1..600).filter(|it| !numbers.contains(it)) {
            assert!(super::call_of(nr).is_none(), "{nr}");
        }
        assert!(super::call_of(numbers[0] | super::X32 as i32).is_none());
    }

    #[test]
    #[allow(unsafe_code, reason = "a mapping whose second page is taken away, to read up to")]
    fn a_string_is_read_to_its_nul_or_as_far_as_it_is_mapped() {
        // A multiple of the 4096 bytes `string_at` reads at most, on every architecture.
        // SAFETY: `sysconf` takes an integer and touches no memory of this process.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert_eq!(page % 4096, 0);
        // SAFETY: a new anonymous private mapping of two pages, where the kernel picks the
        // address (null), so it overlaps nothing of this process; nothing but this test has
        // its address. The second page is given back before the last reads, which stop there.
        let pages = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(pages, libc::MAP_FAILED);
        let base = pages as u64;
        let put = |at: usize, bytes: &[u8]| {
            // SAFETY: every `at` below is less than `2 * page`, so the offset stays inside the
            // mapping.
            let to = unsafe { pages.cast::<u8>().add(at) };
            // SAFETY: `to..to + bytes.len()` is mapped and writable at each call (the writes
            // across the end of the first page come before the second is given back), and
            // `bytes` is a literal, not in it.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
        };
        let pid = std::process::id();
        let mut into = Vec::new();
        let read = |at: usize, into: &mut Vec<u8>| super::string_at(pid, base + at as u64, into);
        put(page - 2, b"/ab\0");
        assert!(read(page - 2, &mut into));
        assert_eq!(into, b"/ab", "across the end of a page");
        put(0, b"/x/y\0");
        assert!(read(0, &mut into));
        assert_eq!(into, b"/x/y");
        put(10, b"\0");
        assert!(!read(10, &mut into), "an empty string");
        // SAFETY: `page` bytes in is the start of the second page of the mapping.
        let second = unsafe { pages.cast::<u8>().add(page) };
        // SAFETY: `second` is page-aligned and the `page` bytes from it are the mapping's
        // second page, which nothing writes after; `string_at` reads it only through the
        // kernel, which finds it unmapped.
        assert_eq!(unsafe { libc::munmap(second.cast(), page) }, 0);
        put(page - 3, b"/zz");
        assert!(read(page - 3, &mut into));
        assert_eq!(into, b"/zz", "to the end of what is mapped");
        assert!(!read(page, &mut into), "nothing mapped");
        // SAFETY: `pages` is the start of the mapping, whose first page is all that is left;
        // nothing uses it after.
        unsafe { libc::munmap(pages, page) };
    }

    #[test]
    fn a_relative_path_is_made_whole_from_the_working_directory() {
        let pid = std::process::id();
        let cwd = super::tidy(&std::env::current_dir().unwrap());
        let mut scratch = super::Scratch::default();
        scratch.raw = b"./a//b/.".to_vec();
        assert!(super::whole(pid, None, &mut scratch));
        assert_eq!(scratch.path, format!("{cwd}/a/b"));
        scratch.raw = b"/c/./d/".to_vec();
        assert!(super::whole(pid, Some(libc::AT_FDCWD), &mut scratch));
        assert_eq!(scratch.path, "/c/d");
    }

    /// The supervisor's hot path allocates nothing for what it has read before, each into the
    /// buffer it keeps: the path a call names, read from the program's memory and made whole
    /// from its working directory; what `/proc` says of its thread; a Unix socket's links
    /// followed; whether a write is refused; and an address read and named.
    #[test]
    fn a_path_read_into_the_supervisors_buffers_allocates_nothing() {
        let pid = std::process::id();
        let named = c"./a//b/.";
        let mut v4 = [0u8; 16];
        v4[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        v4[2..4].copy_from_slice(&443u16.to_be_bytes());
        v4[4..8].copy_from_slice(&[1, 2, 3, 4]);
        let mut scratch = super::Scratch::default();
        // Room for the threads other tests start meanwhile.
        scratch.procs.tasks.reserve(4096);
        let read = |scratch: &mut super::Scratch| {
            assert!(super::string_at(pid, named.as_ptr() as u64, &mut scratch.raw));
            assert!(super::whole(pid, None, scratch));
            let ids = super::ids(u64::from(pid), &mut scratch.procs.file);
            assert_eq!(ids.map(|it| it.0), Some(u64::from(pid)));
            let mut threads = std::mem::take(&mut scratch.procs.tasks);
            super::tasks(u64::from(pid), &mut scratch.procs.listing, &mut threads);
            assert!(threads.contains(&u64::from(pid)));
            scratch.procs.tasks = threads;
            assert!(super::resolved(pid, &scratch.path, &mut scratch.resolving));
            assert!(super::refused_write(b"/proc/no-such-file", &mut scratch.room));
            assert!(!super::refused_write(b"/proc/no-such-dir/x", &mut scratch.room));
            let at = v4.as_ptr() as u64;
            match super::sockaddr(pid, at, v4.len(), &mut scratch.memory) {
                Some(super::Address::Ip { at, local: false }) => {
                    assert_eq!(super::shown(&mut scratch.text, at), "1.2.3.4:443");
                }
                _ => panic!("not the address"),
            }
        };
        read(&mut scratch);
        let path = scratch.path.clone();
        blaze_alloc_count::assert_at_most(0, "a path read into the supervisor's buffers", || {
            for _ in 0..10 {
                read(&mut scratch);
            }
        });
        assert_eq!(scratch.path, path);
        assert!(path.ends_with("/a/b"), "{path}");
    }

    #[test]
    fn every_run_refuses_terminal_input_and_io_uring() {
        for (recorded, reads) in [(false, false), (true, false), (true, true)] {
            for (confined, network) in [(false, false), (true, false), (false, true), (true, true)] {
                let program = super::filter_program(recorded, reads, confined, network, 3);
                let has = |k: u32| program.iter().any(|it| it.k == k);
                assert!(has(super::TIOCSTI) && has(super::TIOCLINUX));
                assert!(has(super::NR_IO_URING_SETUP) && has(super::X32));
                assert!(program.len() < u16::MAX as usize);
            }
        }
    }

    /// A recorded run's filter asks about the realtime clocks and lets the others go by, and
    /// asks about listings and randomness, only when it records reads.
    #[test]
    fn a_filter_asks_about_the_realtime_clock_alone() {
        let clock_gettime = super::CALLS
            .iter()
            .find(|it| matches!(it.does, super::Does::Clock(Some(_))))
            .expect("clock_gettime")
            .nr;
        // Runs the filter as the kernel would over a call's number and its first argument.
        let run = |program: &[libc::sock_filter], nr: u32, arg: u32| -> u32 {
            let mut at = 0usize;
            let mut a = 0u32;
            loop {
                let insn = program[at];
                match insn.code {
                    0x20 => {
                        a = match insn.k {
                            0 => nr,
                            4 => super::ARCH,
                            16 => arg,
                            _ => 0,
                        }
                    }
                    0x06 => return insn.k,
                    0x15 | 0x35 | 0x45 => {
                        let taken = match insn.code {
                            0x15 => a == insn.k,
                            0x35 => a >= insn.k,
                            _ => a & insn.k != 0,
                        };
                        at += usize::from(if taken { insn.jt } else { insn.jf });
                    }
                    other => panic!("an instruction the test does not know: {other:#x}"),
                }
                at += 1;
            }
        };
        let reads = super::filter_program(true, true, false, false, 3);
        for clock in super::REALTIME {
            assert_eq!(run(&reads, clock_gettime, clock), super::SECCOMP_RET_USER_NOTIF);
        }
        assert_eq!(run(&reads, clock_gettime, 1), super::SECCOMP_RET_ALLOW, "CLOCK_MONOTONIC");
        let no_reads = super::filter_program(true, false, false, false, 3);
        assert_eq!(run(&no_reads, clock_gettime, 0), super::SECCOMP_RET_ALLOW);
        for does in [super::Does::Random, super::Does::List(0)] {
            let nr = super::CALLS
                .iter()
                .find(|it| std::mem::discriminant(&it.does) == std::mem::discriminant(&does))
                .unwrap()
                .nr;
            assert_eq!(run(&reads, nr, 0), super::SECCOMP_RET_USER_NOTIF);
            assert_eq!(run(&no_reads, nr, 0), super::SECCOMP_RET_ALLOW);
        }
    }

    /// A traced run of `blaze sandbox --v2` loads the interposer in every process: the program
    /// sees none of its variables, says it loaded (no `X` line), and its listings are `D` lines.
    /// Skipped where no sandbox can be made, unless `BLAZE_VIEW_TESTS=require`.
    #[test]
    fn a_traced_run_loads_the_interposer_and_sees_none_of_it() {
        use blaze_usercode::{confine, trace::{self, Seen}};
        if let Err(why) = alone::probe(crate::sandbox::Probe::V2) {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            return;
        }
        let dir = scratch("interposer");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a"), "").unwrap();
        let script = r#"printf '%s|%s' "${LD_PRELOAD-unset}" "${__BLAZE_INTERPOSE-unset}" > "$OUT/env"
for it in "$OUT"/*; do :; done
/bin/sh -c 'printf "%s" "${LD_PRELOAD-unset}" > "$OUT/child"'"#;
        let spec = confine::Spec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("OUT".into(), out.clone().into()), ("PATH".into(), "/usr/bin:/bin".into())],
            dir: out.clone(),
            read: vec![dir.clone()],
            write: vec![out.clone()],
            network: false,
            trace: true,
            services: Vec::new(),
            live: dir.join("live"),
            report: dir.join("report"),
            not_inputs: vec!["/bin/sh".into()],
        };
        assert_eq!(alone::launch(&spec), 0);
        let read = |it: &Path| std::fs::read_to_string(it).unwrap();
        assert_eq!(read(&out.join("env")), "unset|unset");
        assert_eq!(read(&out.join("child")), "unset");
        let text = read(&spec.live);
        let (lines, _) = trace::parse(&text).unwrap();
        assert!(!lines.iter().any(|it| it.seen == Seen::Incomplete), "{text}");
        let listed = super::tidy(&out);
        assert!(lines.iter().any(|it| it.seen == Seen::Listed && it.what == listed), "{text}");
        assert_eq!(lines[0], trace::Line::new(Seen::Env, "", trace::ALL_ENV));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_programs_environment_is_what_it_was_given_with_the_interposer_first() {
        use std::os::unix::ffi::OsStringExt as _;
        let lib = Path::new("/c/blaze/sandbox/h/libblaze_sandbox_interpose.so");
        let given: Vec<(OsString, OsString)> = vec![
            ("A".into(), OsString::from_vec(b"\xff".to_vec())),
            ("LD_PRELOAD".into(), "".into()),
            ("TMPDIR".into(), "/tmp".into()),
        ];
        let tmp = ("TMPDIR".to_owned(), "/own".to_owned());
        let env = super::environment(&given, Some(&tmp), Some(lib)).unwrap();
        let env: Vec<&[u8]> = env.iter().map(|it| it.as_bytes()).collect();
        assert_eq!(
            env,
            [
                &b"A=\xff"[..],
                b"LD_PRELOAD=/c/blaze/sandbox/h/libblaze_sandbox_interpose.so:",
                b"TMPDIR=/own",
                b"__BLAZE_INTERPOSE=/c/blaze/sandbox/h/libblaze_sandbox_interpose.so",
            ]
        );
        let plain = super::environment(&given[..1], None, Some(lib)).unwrap();
        assert_eq!(plain[1].as_bytes(), b"LD_PRELOAD=/c/blaze/sandbox/h/libblaze_sandbox_interpose.so");
        let nul = vec![("A".into(), OsString::from_vec(b"x\0y".to_vec()))];
        assert!(super::environment(&nul, None, None).unwrap_err().contains("nul"));
    }

    /// A sandbox confined as it is where no user namespace can be made (Ubuntu's AppArmor):
    /// it writes its writable directory, its own `TMPDIR` and `/dev/null` and nothing else,
    /// reaches no address, not even the machine's loopback, signals only its own processes,
    /// and what it leaves running is killed as it ends. Skipped where the kernel has no
    /// Landlock, unless `BLAZE_VIEW_TESTS=require`.
    #[test]
    fn a_confined_sandbox_writes_only_where_it_may_and_reaches_nothing() {
        if let Err(why) = super::landlock::abi() {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            return;
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let bash = std::env::split_paths(&path).map(|it| it.join("bash")).find(|it| it.exists());
        let Some(bash) = bash else {
            eprintln!("skipped: no bash on PATH");
            return;
        };
        let dir = scratch("confined");
        let (out, outside) = (dir.join("out"), dir.join("outside"));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let shown = |it: &Path| it.display().to_string();
        let env = [
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            ("OUT", shown(&out)),
            ("OUTSIDE", shown(&outside)),
            ("PORT", port.to_string()),
            ("OUTER", std::process::id().to_string()),
            ("TMPDIR", "/tmp".to_owned()),
        ];
        let script = format!(
            "{FAIL}{}",
            r#"
echo hi > "$OUT/written" || fail "it wrote nothing in OUT"
mkdir "$OUT/d" && mv "$OUT/written" "$OUT/d/moved" || fail "it moved nothing in OUT"
if (echo x > "$OUTSIDE/x") 2>/dev/null; then fail "it wrote outside"; fi
if (mkdir "$OUTSIDE/d") 2>/dev/null; then fail "it made a directory outside"; fi
echo x > /dev/null || fail "it wrote no /dev/null"
[ "$TMPDIR" != /tmp ] && echo x > "$TMPDIR/x" || fail "it has no TMPDIR of its own"
echo "$TMPDIR" > "$OUT/tmpdir"
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then fail "it reached the loopback"; fi
if kill -0 "$OUTER" 2>/dev/null; then fail "it signalled a process outside"; fi
sleep 5 & kill -0 "$!" || fail "it could not signal its own child"
( sleep 60 & echo "$!" > "$OUT/left" )
"#
        );
        let env = env.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect();
        let record = dir.join("record");
        let spec = Spec {
            cwd: out.clone(),
            env,
            writable: vec![out.clone()],
            record: Some(record.clone()),
            record_reads: true,
            ..Spec::default()
        };
        let program: Vec<OsString> = vec![bash.into(), "-c".into(), script.into()];
        let forced = "a user namespace: not made in this test".to_owned();
        assert_eq!(alone::run_as(&spec, Some(&program), forced), Ok(0));
        let read = |it: &str| std::fs::read_to_string(out.join(it)).unwrap();
        assert_eq!(read("d/moved"), "hi\n");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        assert!(!Path::new(read("tmpdir").trim()).exists(), "its TMPDIR is left");
        let left = read("left");
        assert!(!Path::new(&format!("/proc/{}", left.trim())).exists(), "process {left} is left");
        assert!(listener.accept().is_err(), "the loopback was reached");
        let lines = read_record(&record).expect("a record");
        let has =
            |reached, what: &str| lines.iter().any(|it| it.reached == reached && it.what == what);
        assert!(has(Reached::Write, &format!("{}/x", shown(&outside))), "{lines:?}");
        assert!(has(Reached::Net, &format!("127.0.0.1:{port}")), "{lines:?}");
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A bash of the Nix store and the store paths it runs with: its own and those its
    /// interpreter loads (`ld.so --list`). `None` where there is no such bash or no sandbox can
    /// be made, which the test says and passes on, unless `BLAZE_VIEW_TESTS=require` (as in
    /// an ix VM), when it fails.
    fn store_bash() -> Option<(PathBuf, Vec<PathBuf>)> {
        let skip = |why: &str| {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            None
        };
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some(bash) = std::env::split_paths(&path)
            .filter_map(|it| std::fs::canonicalize(it.join("bash")).ok())
            .find(|it| it.starts_with("/nix/store"))
        else {
            return skip("no bash of the Nix store on PATH");
        };
        if alone::run(&Spec { cwd: "/".into(), ..Spec::default() }, None) != Ok(0) {
            return skip("no sandbox can be made here");
        }
        let Some(interpreter) = interpreter(&bash) else {
            return skip("bash has no interpreter");
        };
        let listed = std::process::Command::new(&interpreter).arg("--list").arg(&bash).output();
        let listed = listed.map(|it| String::from_utf8_lossy(&it.stdout).into_owned());
        let mut paths = vec![store_path(&bash)];
        for word in listed.unwrap_or_default().split_whitespace() {
            if word.starts_with("/nix/store/") {
                paths.push(store_path(Path::new(word)));
            }
        }
        paths.sort();
        paths.dedup();
        Some((bash, paths))
    }

    /// The store path `path` is in: `/nix/store/HASH-NAME`.
    fn store_path(path: &Path) -> PathBuf {
        path.components().take(4).collect()
    }

    /// The interpreter a 64-bit little-endian ELF program names (`PT_INTERP`).
    fn interpreter(elf: &Path) -> Option<PathBuf> {
        let bytes = std::fs::read(elf).ok()?;
        let at = |at: usize, len: usize| bytes.get(at..at + len);
        let u16_at = |it: usize| Some(u16::from_le_bytes(at(it, 2)?.try_into().ok()?) as usize);
        let u32_at = |it: usize| Some(u32::from_le_bytes(at(it, 4)?.try_into().ok()?));
        let u64_at = |it: usize| Some(u64::from_le_bytes(at(it, 8)?.try_into().ok()?) as usize);
        let (table, size, count) = (u64_at(0x20)?, u16_at(0x36)?, u16_at(0x38)?);
        (0..count).find_map(|header| {
            let header = table + header * size;
            (u32_at(header)? == 3).then_some(())?;
            let name = at(u64_at(header + 8)?, u64_at(header + 0x20)?)?;
            Some(PathBuf::from(OsStr::from_bytes(name.strip_suffix(&[0]).unwrap_or(name))))
        })
    }

    /// A program still open for writing when the sandbox execs it (`ETXTBSY`, as a file blaze
    /// just wrote can be) runs once the writer closes it, and the sandbox stops trying, naming
    /// it, when the writer does not close it within blaze-core's waits.
    #[test]
    fn a_program_still_open_for_writing_runs_once_it_is_closed() {
        if let Err(why) = super::landlock::abi() {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            return;
        }
        let dir = scratch("busy");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let spec = Spec {
            cwd: out.clone(),
            env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            writable: vec![out],
            ..Spec::default()
        };
        let run = |name: &str, hold: std::time::Duration| {
            let sh = dir.join(name);
            std::fs::copy("/bin/sh", &sh).unwrap();
            let writer = std::fs::OpenOptions::new().write(true).open(&sh).unwrap();
            let held = std::thread::spawn(move || {
                blaze_graph::spin::busy(hold);
                drop(writer);
            });
            let program: Vec<OsString> = vec![sh.clone().into(), "-c".into(), "exit 3".into()];
            let ran = alone::run_said(&spec, Some(&program));
            held.join().unwrap();
            (sh, ran)
        };
        let (_, (code, said)) = run("sh-briefly", std::time::Duration::from_millis(30));
        assert_eq!(code, Ok(3), "{said}");
        let most = std::time::Duration::from_nanos(blaze_core::busy::MOST_NS);
        let (sh, (code, said)) = run("sh-held", most + most / 2);
        assert_eq!(code, Ok(127), "{said}");
        assert!(said.contains(&sh.display().to_string()) && said.contains("still open for writing"), "{said}");
        _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs `script` with `bash` in a sandbox that sees `view`, from `/out`, with `env`.
    fn run_in(
        bash: &Path,
        view: View,
        env: &[(&str, String)],
        script: &str,
        record: Option<PathBuf>,
    ) -> Result<i32, String> {
        let env = env.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect();
        let spec = Spec { cwd: "/out".into(), env, record, view: Some(view), ..Spec::default() };
        let program: Vec<OsString> = vec![bash.into(), "-c".into(), script.into()];
        alone::run(&spec, Some(&program))
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-view-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const FAIL: &str = "fail() { echo \"$*\" >&2; exit 1; }\n";

    #[test]
    fn a_view_shows_its_binds_and_nothing_of_the_machine() {
        let Some((bash, closure)) = store_bash() else { return };
        let dir = scratch("binds");
        let (out, src) = (dir.join("out"), dir.join("src"));
        for it in [&out, &src.join("gen"), &dir.join("gen")] {
            std::fs::create_dir_all(it).unwrap();
        }
        std::fs::write(dir.join("note"), "a file bound alone\n").unwrap();
        let undeclared = std::fs::read_dir("/nix/store")
            .unwrap()
            .flatten()
            .map(|it| it.path())
            .find(|it| {
                let hidden = it.file_name().is_some_and(|it| it.as_bytes().starts_with(b"."));
                !hidden && !closure.contains(it)
            })
            .expect("a store path bash does not need");
        let mut binds: Vec<Bind> = closure.iter().map(|it| Bind::read(it, it)).collect();
        binds.push(Bind::write("/out", &out));
        // Written inside what is read-only, as a build script's `OUT_DIR` in its target
        // directory is; listed before what it is in, which is bound first all the same.
        binds.push(Bind::write("/src/gen", dir.join("gen")));
        binds.push(Bind::read("/src", &src));
        binds.push(Bind::read("/etc/note", dir.join("note")));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
        let env = [
            ("MACHINE_HOME", home),
            ("UNDECLARED", undeclared.display().to_string()),
            ("BASH_STORE", store_path(&bash).display().to_string()),
        ];
        let script = format!(
            "{FAIL}{}",
            r#"
for it in /usr /bin /lib /home /root /var /run /sys /nix/var /etc/shadow /etc/resolv.conf \
    /etc/nsswitch.conf "$MACHINE_HOME" "$UNDECLARED"; do
  if [ -e "$it" ] || [ -L "$it" ]; then fail "$it is there"; fi
done
[ "$(pwd)" = /out ] || fail "it started in $(pwd)"
printf '%s\n' /* > /out/root
printf '%s\n' /nix/store/* > /out/store
printf '%s\n' /etc/* /dev/* > /out/system
{ while IFS= read -r line; do printf '%s\n' "$line"; done; } < /etc/passwd > /out/passwd
{ read -r note < /etc/note && [ "$note" = "a file bound alone" ]; } || fail "no /etc/note"
"$BASH" -c '[ -e /usr ] || [ -e "$MACHINE_HOME" ] || echo child > /out/child'
for it in "$BASH_STORE/x" /etc/x /x /src/x /etc/note; do
  if (echo x > "$it") 2>/dev/null; then fail "it wrote $it"; fi
done
echo x > /tmp/x && echo x > /dev/shm/x && echo x > /dev/null || fail "it wrote no temporary file"
echo generated > /src/gen/made || fail "it did not write /src/gen"
if (exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null; then fail "it reached the network"; fi
for it in /proc/[0-9]*; do
  [ "${it#/proc/}" -lt 100 ] || fail "process ${it#/proc/} of the machine's is seen"
done
echo hi > /out/written
"#
        );
        let record = dir.join("record");
        let ended = run_in(&bash, View { binds }, &env, &script, Some(record.clone()));
        assert_eq!(ended, Ok(0));
        let read = |it: &str| std::fs::read_to_string(out.join(it)).unwrap();
        assert_eq!(read("written"), "hi\n");
        assert_eq!(read("child"), "child\n");
        assert_eq!(std::fs::read_to_string(dir.join("gen/made")).unwrap(), "generated\n");
        assert!(!src.join("x").exists() && !src.join("gen/made").exists());
        assert_eq!(read("root"), "/dev\n/etc\n/nix\n/out\n/proc\n/src\n/tmp\n");
        let store: Vec<PathBuf> = read("store").lines().map(PathBuf::from).collect();
        assert_eq!(store, closure);
        assert_eq!(
            read("system"),
            "/etc/group\n/etc/hosts\n/etc/note\n/etc/passwd\n/dev/fd\n/dev/full\n/dev/null\n\
             /dev/random\n/dev/shm\n/dev/stderr\n/dev/stdin\n/dev/stdout\n/dev/tty\n\
             /dev/urandom\n/dev/zero\n"
        );
        let (uid, gid) = (super::raw::uid(), super::raw::gid());
        let user = if uid == 0 { "root" } else { "blaze" };
        assert_eq!(
            read("passwd"),
            format!(
                "{user}:x:{uid}:{gid}:{user}:/homeless-shelter:/noshell\n\
                 nobody:x:65534:65534:nobody:/:/noshell\n"
            )
        );
        // The record still has what the sandbox refused, by the paths the program named.
        let lines = read_record(&record).expect("a record");
        let has =
            |reached, what: &str| lines.iter().any(|it| it.reached == reached && it.what == what);
        let bash_store = store_path(&bash).display().to_string();
        for written in [&*format!("{bash_store}/x"), "/etc/x", "/x", "/src/x"] {
            assert!(has(Reached::Write, written), "no W line for {written}: {lines:?}");
        }
        assert!(has(Reached::Net, "1.1.1.1:80"), "{lines:?}");
        assert!(!lines.iter().any(|it| it.what.starts_with("/out") || it.what.starts_with("/tmp")));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_view_binds_a_store_path_from_anywhere() {
        let Some((bash, closure)) = store_bash() else { return };
        let dir = scratch("relocated");
        let (out, copy) = (dir.join("out"), dir.join("copy"));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&copy).unwrap();
        let copied = std::process::Command::new("cp").arg("-a").args(&closure).arg(&copy).status();
        assert!(copied.unwrap().success());
        let name = |it: &Path| it.file_name().unwrap().to_owned();
        let own = copy.join(name(&store_path(&bash)));
        let chmod = std::process::Command::new("chmod").arg("u+w").arg(&own).status();
        assert!(chmod.unwrap().success());
        std::fs::write(own.join("relocated"), "").unwrap();
        assert!(!store_path(&bash).join("relocated").exists());
        let mut binds: Vec<Bind> =
            closure.iter().map(|it| Bind::read(it, copy.join(name(it)))).collect();
        binds.push(Bind::write("/out", &out));
        let env = [("BASH_STORE", store_path(&bash).display().to_string())];
        let script = format!(
            "{FAIL}{}",
            r#"
[ -e "$BASH_STORE/relocated" ] || fail "the store path is not the copy"
"$BASH" -c '[ -e "$BASH_STORE/relocated" ] && echo child > /out/child' || fail "no child"
"#
        );
        assert_eq!(run_in(&bash, View { binds }, &env, &script, None), Ok(0));
        assert_eq!(std::fs::read_to_string(out.join("child")).unwrap(), "child\n");
        _ = std::process::Command::new("chmod").arg("-R").arg("u+w").arg(&dir).status();
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_view_that_cannot_be_made_says_why_before_anything_runs() {
        let dir = scratch("refused");
        let missing = dir.join("missing");
        let made = |binds: Vec<Bind>, overlays: Vec<PathBuf>| {
            let spec =
                Spec { cwd: "/".into(), overlays, view: Some(View { binds }), ..Spec::default() };
            alone::run(&spec, Some(&["/bin/true".into()])).unwrap_err()
        };
        let why = made(vec![Bind::read("/nix/store/x", &missing)], Vec::new());
        assert!(
            why.contains(&*missing.display().to_string()) && why.contains("is not there"),
            "{why}"
        );
        let why = made(vec![Bind::read("/proc/x", &dir)], Vec::new());
        assert!(why.contains("the view's own"), "{why}");
        let why = made(vec![Bind::read("/a/../b", &dir)], Vec::new());
        assert!(why.contains("no `..`"), "{why}");
        let why = made(vec![Bind::read("/a", &dir), Bind::read("/a/", &dir)], Vec::new());
        assert!(why.contains("two things at"), "{why}");
        let why = made(Vec::new(), vec![dir.clone()]);
        assert!(why.contains("binds of the view"), "{why}");
        _ = std::fs::remove_dir_all(&dir);
    }
}
