//! The sandbox on Linux. The command's own process stays outside and is the supervisor: it
//! forks the sandbox's first process, which enters new user, mount, network and IPC
//! namespaces, makes every mount read-only, puts the overlays, the writable directories and
//! the empty directories in place, brings up its loopback and forks the first process of a
//! new PID namespace. That one mounts its `/proc`, forks the program's process and waits for
//! it; when it ends, the kernel ends every other process of the namespace. The first process
//! is a session of its own, so that nothing of the sandbox has the caller's terminal as its
//! own; the supervisor passes on the signals the terminal sends it.
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

use std::{
    ffi::{CString, OsString},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use super::{Recorder, Spec, tidy};

/// The directories that are the sandbox's own: the kernel's views and the empty ones.
const OWN: &[&str] = &["/proc", "/sys", "/dev", "/tmp", "/var/tmp", "/run"];

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
    call(59, &[(None, 0, false)], Does::Look),      // execve
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
    call(322, &[(Some(0), 1, false)], Does::Look),  // execveat
    call(332, &[(Some(0), 1, false)], Does::Look),  // statx
    // openat2's flags are in a structure the filter can't read: it is always asked about.
    call(437, &[(Some(0), 1, true)], Does::Change), // openat2
    call(439, &[(Some(0), 1, false)], Does::Access(2)), // faccessat2
    call(42, NO_PATH, Does::Connect),               // connect
    call(44, NO_PATH, Does::SendTo),                // sendto
    call(46, NO_PATH, Does::SendMsg),               // sendmsg
    call(49, NO_PATH, Does::Address),               // bind
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
    call(221, &[(None, 0, false)], Does::Look),    // execve
    call(276, &[(Some(0), 1, true), (Some(2), 3, true)], Does::Change), // renameat2
    call(281, &[(Some(0), 1, false)], Does::Look), // execveat
    call(291, &[(Some(0), 1, false)], Does::Look), // statx
    // openat2's flags are in a structure the filter can't read: it is always asked about.
    call(437, &[(Some(0), 1, true)], Does::Change), // openat2
    call(439, &[(Some(0), 1, false)], Does::Access(2)), // faccessat2
    call(200, NO_PATH, Does::Address),              // bind
    call(203, NO_PATH, Does::Connect),              // connect
    call(206, NO_PATH, Does::SendTo),               // sendto
    call(211, NO_PATH, Does::SendMsg),              // sendmsg
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
const AT_RECURSIVE: libc::c_uint = 0x8000;
const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
// _IOWR('!', 0, struct seccomp_notif) and _IOWR('!', 1, struct seccomp_notif_resp).
const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;

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
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_default()
}

/// Runs `program` sandboxed as `spec` says, or with `None` makes the sandbox and runs
/// nothing in it. What it ends with, as [`super::backend`] says.
pub(super) fn run(spec: &Spec, program: Option<&[OsString]>) -> Result<i32, String> {
    // Everything the processes of the sandbox use is made before the first fork.
    let argv: Vec<CString> = program
        .unwrap_or_default()
        .iter()
        .map(|it| CString::new(it.as_bytes()).unwrap_or_default())
        .collect();
    let env: Vec<CString> = spec
        .env
        .iter()
        .map(|(name, value)| CString::new(format!("{name}={value}")).unwrap_or_default())
        .collect();
    let mut sockets = [0; 2];
    // SAFETY: two descriptors are written into `sockets`.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    } < 0
    {
        return Err(os("socketpair"));
    }
    let recorded = program.is_some() && spec.record.is_some();
    let filter = program.map(|_| filter_program(recorded, spec.record_reads, sockets[1]));
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // Before the fork, so that no signal ends this process before it passes them on; the
    // sandbox's processes that run no program keep the handlers, and pass nothing on.
    super::pass_signals_to(0);
    // SAFETY: the process is one thread; the child only makes system calls and exits.
    let first = unsafe { libc::fork() };
    if first < 0 {
        return Err(os("fork"));
    }
    if first == 0 {
        // A session of its own, with no controlling terminal: nothing of the sandbox can push
        // input into the caller's (`TIOCSTI`), which only a terminal's own session may. And
        // gone with the supervisor, which a signal that can't be passed on ends.
        unsafe {
            libc::setsid();
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        }
        unsafe { libc::close(sockets[0]) };
        let made = Made { argv: &argv, env: &env, socket: sockets[1], filter: filter.as_deref() };
        let code = match sandbox(spec, uid, gid, &made, program.is_none()) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("ra-jit: the sandbox could not be made: {error}");
                super::COULD_NOT
            }
        };
        unsafe { libc::_exit(code) };
    }
    super::pass_signals_to(first);
    unsafe { libc::close(sockets[1]) };
    let listener = received(sockets[0]);
    unsafe { libc::close(sockets[0]) };
    let mut recorder = Recorder::new(spec, OWN);
    if let Some(listener) = listener {
        supervise(listener, first, &mut recorder);
        unsafe { libc::close(listener) };
    }
    let mut status = 0;
    while unsafe { libc::waitpid(first, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    // Its number may be another's now.
    super::pass_signals_to(0);
    if let (Some(file), true) = (&spec.record, recorded) {
        if listener.is_none() {
            recorder.incomplete(0, "the sandbox did not see the program run");
        }
        recorder.write(file);
    }
    Ok(ended(status))
}

/// What the sandbox's processes are given, made before the first fork.
struct Made<'a> {
    argv: &'a [CString],
    env: &'a [CString],
    /// Where the filter's descriptor is handed to the supervisor.
    socket: libc::c_int,
    /// The seccomp filter, when there is a record to write.
    filter: Option<&'a [libc::sock_filter]>,
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
fn sandbox(
    spec: &Spec,
    uid: libc::uid_t,
    gid: libc::gid_t,
    made: &Made<'_>,
    probe: bool,
) -> Result<i32, String> {
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC;
    if unsafe { libc::unshare(flags) } < 0 {
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
    let write =
        |file: &str, text: String| std::fs::write(file, text).map_err(|it| format!("{file}: {it}"));
    write("/proc/self/setgroups", "deny".to_owned())?;
    write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
    write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    let root = c(Path::new("/"));
    let none = std::ptr::null::<libc::c_char>();
    if unsafe {
        libc::mount(none, root.as_ptr(), none, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null())
    } < 0
    {
        return Err(os("making the mounts private"));
    }
    let opened = |at: &Path| -> Result<std::os::fd::OwnedFd, String> {
        let path = c(at);
        let fd = unsafe {
            libc::open(path.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return Err(os(&at.display().to_string()));
        }
        // SAFETY: `fd` was just opened and is owned here alone.
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
    };
    // The writable directories are reached through descriptors opened before anything is
    // made read-only or hidden, and bound back in place after: they stay the directories
    // outside, written for real.
    let writable = spec.writable.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    let readable = spec.readable.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    let attr = MountAttr { attr_set: MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
    // SAFETY: `attr` is a `struct mount_attr` of the size given.
    if unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            libc::AT_FDCWD,
            root.as_ptr(),
            AT_RECURSIVE,
            &attr as *const MountAttr,
            size_of::<MountAttr>(),
        )
    } < 0
    {
        return Err(os("making every mount read-only (mount_setattr)"));
    }
    let tmpfs = |at: &Path| -> Result<(), String> {
        let (kind, at_c) = (c(Path::new("tmpfs")), c(at));
        let options = c(Path::new("mode=1777"));
        let flags = libc::MS_NOSUID | libc::MS_NODEV;
        if unsafe {
            libc::mount(kind.as_ptr(), at_c.as_ptr(), kind.as_ptr(), flags, options.as_ptr().cast())
        } < 0
        {
            return Err(os(&format!("an empty {}", at.display())));
        }
        Ok(())
    };
    // The overlays are reached through descriptors opened before the empty `/tmp` hides
    // what is there: a workspace under `/tmp` is mounted again inside it. Their own
    // directories are on a tmpfs that the program's `/tmp` then hides as well.
    let lowers = spec.overlays.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
    let staging = Path::new("/tmp");
    tmpfs(staging)?;
    for at in 0..spec.overlays.len() {
        for it in [staging.join(format!("{at}/upper")), staging.join(format!("{at}/work"))] {
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
                .collect(),
            false => Vec::new(),
        };
        tmpfs(dir)?;
        for (link, to) in links {
            std::os::unix::fs::symlink(&to, &link).map_err(|error| {
                format!("{} in the empty {}: {error}", link.display(), dir.display())
            })?;
        }
    }
    let by_fd = |fd: &std::os::fd::OwnedFd, rest: &str| {
        PathBuf::from(format!("/proc/self/fd/{}{rest}", std::os::fd::AsRawFd::as_raw_fd(fd)))
    };
    for (at, (dir, lower)) in spec.overlays.iter().zip(&lowers).enumerate() {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let (upper, work) =
            (by_fd(&staged, &format!("/{at}/upper")), by_fd(&staged, &format!("/{at}/work")));
        overlay(dir, &by_fd(lower, ""), &upper, &work)?;
    }
    // The readable directories are bound back in place, read-only as their mount now is, where
    // an empty directory hid them; the writable ones go on top.
    for (dir, fd) in spec.readable.iter().zip(&readable) {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let (from, to) = (c(&by_fd(fd, "")), c(dir));
        let flags = libc::MS_BIND | libc::MS_REC;
        if unsafe { libc::mount(from.as_ptr(), to.as_ptr(), none, flags, std::ptr::null()) } < 0 {
            return Err(os(&format!("{} seen read-only", dir.display())));
        }
    }
    for (dir, fd) in spec.writable.iter().zip(&writable) {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let (from, to) = (c(&by_fd(fd, "")), c(dir));
        let flags = libc::MS_BIND | libc::MS_REC;
        if unsafe { libc::mount(from.as_ptr(), to.as_ptr(), none, flags, std::ptr::null()) } < 0 {
            return Err(os(&format!("{} written for real", dir.display())));
        }
        // A bind keeps the read-only mark its source was given above.
        let attr =
            MountAttr { attr_set: 0, attr_clr: MOUNT_ATTR_RDONLY, propagation: 0, userns_fd: 0 };
        // SAFETY: `attr` is a `struct mount_attr` of the size given.
        if unsafe {
            libc::syscall(
                SYS_MOUNT_SETATTR,
                libc::AT_FDCWD,
                to.as_ptr(),
                AT_RECURSIVE,
                &attr as *const MountAttr,
                size_of::<MountAttr>(),
            )
        } < 0
        {
            return Err(os(&format!("{} written for real (mount_setattr)", dir.display())));
        }
    }
    drop((lowers, staged, writable, readable));
    // A `TMPDIR` in one of the empty directories is there, empty as they are.
    let tmp = spec.env.iter().find(|(name, _)| name == "TMPDIR").map(|(_, it)| Path::new(it));
    if let Some(tmp) =
        tmp.filter(|it| ["/tmp", "/var/tmp", "/dev/shm"].iter().any(|dir| it.starts_with(dir)))
    {
        std::fs::create_dir_all(tmp).map_err(|error| format!("{}: {error}", tmp.display()))?;
    }
    loopback_up()?;
    if unsafe { libc::unshare(libc::CLONE_NEWPID) } < 0 {
        return Err(os("a PID namespace (unshare)"));
    }
    let cwd = c(&spec.cwd);
    let init = unsafe { libc::fork() };
    if init < 0 {
        return Err(os("fork"));
    }
    if init == 0 {
        // Gone with the sandbox's first process, and the whole namespace with it.
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        let code = match first_process(&cwd, made, probe) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("ra-jit: the sandbox could not be made: {error}");
                super::COULD_NOT
            }
        };
        unsafe { libc::_exit(code) };
    }
    unsafe { libc::close(made.socket) };
    let mut status = 0;
    while unsafe { libc::waitpid(init, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    Ok(ended(status))
}

/// Mounts at `dir` what is in `lower`, with its changes going to `upper`.
fn overlay(dir: &Path, lower: &Path, upper: &Path, work: &Path) -> Result<(), String> {
    // A path in an overlay's options has its commas, colons and backslashes escaped.
    let escaped = |it: &Path| {
        let mut out = Vec::new();
        for &byte in it.as_os_str().as_bytes() {
            if matches!(byte, b',' | b':' | b'\\') {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out
    };
    let (kind, at) = (c(Path::new("overlay")), c(dir));
    let mut last = String::new();
    // `userxattr` keeps what overlayfs notes in the user's attributes, which a user
    // namespace may write; a kernel or a tmpfs without them takes none.
    for extra in [&b",userxattr"[..], b""] {
        let mut options = b"lowerdir=".to_vec();
        options.extend(escaped(lower));
        options.extend(b",upperdir=");
        options.extend(escaped(upper));
        options.extend(b",workdir=");
        options.extend(escaped(work));
        options.extend(extra);
        let options = CString::new(options).unwrap_or_default();
        if unsafe {
            libc::mount(kind.as_ptr(), at.as_ptr(), kind.as_ptr(), 0, options.as_ptr().cast())
        } == 0
        {
            return Ok(());
        }
        last = os(&format!("an overlay on {}", dir.display()));
    }
    Err(last)
}

/// Brings up the network namespace's loopback, which a new one has down.
fn loopback_up() -> Result<(), String> {
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(os("a socket for the loopback"));
    }
    let mut request = IfReq { name: [0; 16], flags: 0, pad: [0; 22] };
    request.name[..2].copy_from_slice(b"lo");
    // SAFETY: `request` is a `struct ifreq`.
    let up = unsafe {
        libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request) >= 0 && {
            request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request) >= 0
        }
    };
    let error = os("bringing up the loopback");
    unsafe { libc::close(socket) };
    if up { Ok(()) } else { Err(error) }
}

/// The first process of the PID namespace: mounts its `/proc`, forks the program's
/// process and reaps every process left to it until that one ends, ending as it did.
fn first_process(cwd: &CString, made: &Made<'_>, probe: bool) -> Result<i32, String> {
    let proc_ = c(Path::new("/proc"));
    let kind = c(Path::new("proc"));
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // Where `/proc` can't be mounted again, the one there still shows each process
    // itself as `/proc/self`.
    unsafe { libc::mount(kind.as_ptr(), proc_.as_ptr(), kind.as_ptr(), flags, std::ptr::null()) };
    if unsafe { libc::chdir(cwd.as_ptr()) } < 0 {
        return Err(os(&format!("the working directory {}", cwd.to_string_lossy())));
    }
    if probe {
        return Ok(0);
    }
    let program = unsafe { libc::fork() };
    if program < 0 {
        return Err(os("fork"));
    }
    if program == 0 {
        let error = exec(made);
        eprintln!("ra-jit: could not run {}: {error}", made.argv[0].to_string_lossy());
        unsafe { libc::_exit(127) };
    }
    unsafe { libc::close(made.socket) };
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
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
fn exec(made: &Made<'_>) -> String {
    if let Some(program) = made.filter {
        let listener = match install(program) {
            Ok(it) => it,
            Err(error) => return error,
        };
        if let Err(error) = send(made.socket, listener) {
            return error;
        }
        unsafe { libc::close(listener) };
    }
    unsafe { libc::close(made.socket) };
    let mut argv: Vec<*const libc::c_char> = made.argv.iter().map(|it| it.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = made.env.iter().map(|it| it.as_ptr()).collect();
    envp.push(std::ptr::null());
    unsafe { libc::execve(argv[0], argv.as_ptr(), envp.as_ptr()) };
    std::io::Error::last_os_error().to_string()
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
/// socket of the machine's by `sendmsg` is the residual docs/cargo.md names.)
fn filter_program(recorded: bool, reads: bool, socket: libc::c_int) -> Vec<libc::sock_filter> {
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
    let mut program = vec![
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
    ];
    // A null address is a connected socket's message.
    let sendto_with_address = vec![
        insn(LD_W_ABS, 0, 0, low(4)),
        insn(JEQ_K, 1, 0, 0),
        notify,
        insn(LD_W_ABS, 0, 0, high(4)),
        insn(JEQ_K, 0, 1, 0),
        allow,
        notify,
    ];
    for call in CALLS {
        let block = match (recorded, reads, call.does) {
            (_, _, Does::Connect) => vec![notify],
            (false, _, Does::SendTo) => sendto_with_address.clone(),
            (false, _, _) => continue,
            (true, _, Does::SendMsg) => {
                vec![insn(LD_W_ABS, 0, 0, low(0)), insn(JEQ_K, 0, 1, socket as u32), allow, notify]
            }
            (true, true, _) | (true, false, Does::Change | Does::Address) => vec![notify],
            (true, false, Does::Look) => continue,
            (true, false, Does::Open(flags)) => {
                vec![insn(LD_W_ABS, 0, 0, low(flags)), insn(JSET_K, 0, 1, WRITES), notify, allow]
            }
            (true, false, Does::Access(mode)) => {
                vec![insn(LD_W_ABS, 0, 0, low(mode)), insn(JEQ_K, 0, 1, 0), notify, allow]
            }
            (true, false, Does::SendTo) => sendto_with_address.clone(),
        };
        // Past the block when it is another call. Every block returns, so the call's number
        // is still what the next comparison reads.
        program.push(insn(JEQ_K, 0, block.len() as u8, call.nr));
        program.extend(block);
    }
    program.push(allow);
    program
}

/// Installs `program` as this process's seccomp filter, and the descriptor the kernel asks
/// through.
fn install(program: &[libc::sock_filter]) -> Result<libc::c_int, String> {
    let prog = libc::sock_fprog { len: program.len() as u16, filter: program.as_ptr().cast_mut() };
    // The process holds every capability of its user namespace until it runs the program, so
    // it may install a filter without `no_new_privs`.
    let listener = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const libc::sock_fprog,
        )
    };
    if listener < 0 {
        return Err(os("a seccomp filter"));
    }
    Ok(listener as libc::c_int)
}

/// Sends the descriptor `fd` over `socket`.
fn send(socket: libc::c_int, fd: libc::c_int) -> Result<(), String> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut space = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as _;
    // SAFETY: the control buffer has room for one descriptor.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), fd);
    }
    if unsafe { libc::sendmsg(socket, &msg, 0) } < 0 {
        return Err(os("handing over the filter"));
    }
    Ok(())
}

/// The descriptor sent over `socket`, or `None` when the other end closed first.
fn received(socket: libc::c_int) -> Option<libc::c_int> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut space = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    msg.msg_controllen = space.len() as _;
    loop {
        let read = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if read <= 0 {
            return None;
        }
        break;
    }
    // SAFETY: the kernel wrote the control message.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return None;
        }
        Some(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>()))
    }
}

/// Answers the kernel's notifications through `listener`, noting what each call reached,
/// until the process `first` has ended, or no process is left that the filter is on.
fn supervise(listener: libc::c_int, first: libc::pid_t, recorder: &mut Recorder) {
    let pidfd = unsafe { libc::syscall(SYS_PIDFD_OPEN, first, 0) } as libc::c_int;
    let mut fds = vec![libc::pollfd { fd: listener, events: libc::POLLIN, revents: 0 }];
    if pidfd >= 0 {
        fds.push(libc::pollfd { fd: pidfd, events: libc::POLLIN, revents: 0 });
    }
    loop {
        fds.iter_mut().for_each(|it| it.revents = 0);
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            answer(listener, recorder);
            continue;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0
            || fds.get(1).is_some_and(|it| it.revents != 0)
        {
            break;
        }
    }
    if pidfd >= 0 {
        unsafe { libc::close(pidfd) };
    }
}

/// Takes one notification, notes what its call reached and lets the call go on.
fn answer(listener: libc::c_int, recorder: &mut Recorder) {
    let mut notif = Notif::default();
    // SAFETY: `notif` is a zeroed `struct seccomp_notif`.
    if unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV as _, &mut notif) } < 0 {
        return;
    }
    let refused = match CALLS.iter().find(|it| it.nr as i32 == notif.nr) {
        Some(call) => noted(&notif, call, recorder),
        None => None,
    };
    let resp = match refused {
        Some(error) => NotifResp { id: notif.id, val: 0, error: -error, flags: 0 },
        None => {
            NotifResp { id: notif.id, val: 0, error: 0, flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE }
        }
    };
    // SAFETY: `resp` is a `struct seccomp_notif_resp`. A process that ended meanwhile makes
    // it fail, which is nothing to answer.
    unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND as _, &resp) };
}

/// Notes in `recorder` what the call `notif` of `call` reaches, and the error the sandbox
/// refuses it with, if it does: a connection or a message to a Unix socket outside the
/// sandbox's own directories, which no namespace hides.
fn noted(notif: &Notif, call: &Call, recorder: &mut Recorder) -> Option<i32> {
    let (pid, thread) = (notif.pid, u64::from(notif.pid));
    let address = |at: usize, len: usize| sockaddr(pid, notif.args[at], len);
    let reached = match call.does {
        Does::Address | Does::Connect => address(1, notif.args[2] as usize),
        Does::SendTo => address(4, notif.args[5] as usize),
        Does::SendMsg => {
            // `struct msghdr` starts with the address and its length.
            let header = bytes_at(pid, notif.args[1], 12).unwrap_or_default();
            match header.get(..12) {
                Some(it) => {
                    let name = u64::from_ne_bytes(it[..8].try_into().unwrap_or_default());
                    let len = u32::from_ne_bytes(it[8..12].try_into().unwrap_or_default());
                    sockaddr(pid, name, len as usize)
                }
                None => None,
            }
        }
        _ => None,
    };
    // `bind`'s Unix socket is a file it makes, which the mounts refuse where it may not.
    let to_unix = matches!(call.does, Does::Connect | Does::SendTo | Does::SendMsg);
    match reached {
        Some(Address::Ip(address)) => recorder.refused_net(thread, &address),
        Some(Address::Unix(path)) if to_unix => {
            let path = whole(pid, None, path).and_then(|it| resolved(pid, &it));
            match path {
                Some(path) if recorder.may_connect(&path) => {}
                path => {
                    let path = path.unwrap_or_else(|| "a path the sandbox could not follow".into());
                    recorder.refused_net(thread, &format!("unix:{path}"));
                    return Some(libc::EACCES);
                }
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
        let Some(raw) = string_at(pid, notif.args[at]) else { continue };
        if matches!(call.does, Does::Access(_)) && recorder.marked(thread, &raw) {
            continue;
        }
        let dir = dir.map(|it| notif.args[it] as i32);
        let Some(path) = whole(pid, dir, raw) else { continue };
        if writes && written && !recorder.may_write(&path) && refused_write(&path) {
            recorder.refused_write(thread, &path);
        }
        if recorder.reads() {
            recorder.read(thread, &path);
        }
    }
    None
}

/// Whether a write to `path`, outside where the program may write, is refused by the sandbox
/// rather than by what is there: the path or its directory is there (and read-only inside).
fn refused_write(path: &str) -> bool {
    let path = Path::new(path);
    path.symlink_metadata().is_ok() || path.parent().is_some_and(|it| it.is_dir())
}

/// An address a call names that the sandbox minds.
enum Address {
    /// An IPv4 or IPv6 address that is neither the loopback nor unspecified, as
    /// `address:port`: the network namespace refuses it.
    Ip(String),
    /// A Unix socket's path, as the program named it.
    Unix(Vec<u8>),
}

/// The address of `len` bytes at `at` in `pid`, when it is one the sandbox minds. An abstract
/// Unix socket's (a name with a nul first) is not: it is the network namespace's own.
fn sockaddr(pid: u32, at: u64, len: usize) -> Option<Address> {
    if at == 0 || len < 2 {
        return None;
    }
    let bytes = bytes_at(pid, at, len.min(128))?;
    let family = u16::from_ne_bytes([*bytes.first()?, *bytes.get(1)?]) as i32;
    let port = || u16::from_be_bytes([bytes[2], bytes[3]]);
    match family {
        libc::AF_INET if bytes.len() >= 8 => {
            let ip = std::net::Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
            (!ip.is_loopback() && !ip.is_unspecified())
                .then(|| Address::Ip(format!("{ip}:{}", port())))
        }
        libc::AF_INET6 if bytes.len() >= 24 => {
            let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).ok()?);
            let local = ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|it| it.is_loopback() || it.is_unspecified());
            (!local).then(|| Address::Ip(format!("[{ip}]:{}", port())))
        }
        libc::AF_UNIX => {
            let path = bytes.get(2..)?;
            let path = &path[..path.iter().position(|&it| it == 0).unwrap_or(path.len())];
            (!path.is_empty()).then(|| Address::Unix(path.to_vec()))
        }
        _ => None,
    }
}

/// `path`, whole, as the process `pid` of the sandbox sees it, with every link on the way
/// followed in its view of the file system (`/proc/PID/root`): where a connection to it goes.
/// `None` when a part of it could not be looked at for another reason than its not being
/// there or not being a link, which is refused. (The program may change what is there between
/// this and its call: the residual docs/cargo.md names.)
fn resolved(pid: u32, path: &str) -> Option<String> {
    use std::path::Component;
    let root = format!("/proc/{pid}/root");
    let mut done = PathBuf::from("/");
    let mut rest: Vec<OsString> =
        Path::new(path).components().rev().map(|it| it.as_os_str().to_owned()).collect();
    let mut links = 0;
    while let Some(part) = rest.pop() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => done = PathBuf::from("/"),
            Some(Component::ParentDir) => _ = done.pop(),
            Some(Component::Normal(name)) => {
                let next = done.join(name);
                let mut at = OsString::from(&root);
                at.push(next.as_os_str());
                match std::fs::read_link(&at) {
                    Ok(to) if links < 40 => {
                        links += 1;
                        rest.extend(to.components().rev().map(|it| it.as_os_str().to_owned()));
                    }
                    Ok(_) => return None,
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::EINVAL | libc::ENOENT | libc::ENOTDIR)
                        ) =>
                    {
                        done = next
                    }
                    Err(_) => return None,
                }
            }
            _ => {}
        }
    }
    Some(tidy(&done))
}

/// `len` bytes of `pid`'s memory at `at`, as many as are there.
fn bytes_at(pid: u32, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    let local = libc::iovec { iov_base: buffer.as_mut_ptr().cast(), iov_len: len };
    let remote = libc::iovec { iov_base: at as *mut libc::c_void, iov_len: len };
    let read = unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if read <= 0 {
        return None;
    }
    buffer.truncate(read as usize);
    Some(buffer)
}

/// The string of `pid`'s memory at `address`, up to its nul. `None` for an empty one, which
/// is a call on the descriptor itself.
fn string_at(pid: u32, address: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut at = address;
    while bytes.len() < 4096 {
        // To the end of the page, which is mapped if the byte at `at` is.
        let chunk = 4096 - (at as usize % 4096);
        let read = bytes_at(pid, at, chunk)?;
        if let Some(end) = read.iter().position(|&it| it == 0) {
            bytes.extend_from_slice(&read[..end]);
            break;
        }
        bytes.extend_from_slice(&read);
        at += read.len() as u64;
    }
    (!bytes.is_empty()).then_some(bytes)
}

/// `path`, which `pid` named, made whole from the directory `dir` (`AT_FDCWD` or a
/// descriptor, or the working directory when `None`).
fn whole(pid: u32, dir: Option<i32>, path: Vec<u8>) -> Option<String> {
    let path = PathBuf::from(<OsString as std::os::unix::ffi::OsStringExt>::from_vec(path));
    if path.is_absolute() {
        return Some(tidy(&path));
    }
    let base = match dir {
        Some(fd) if fd != libc::AT_FDCWD => format!("/proc/{pid}/fd/{fd}"),
        _ => format!("/proc/{pid}/cwd"),
    };
    let base = std::fs::read_link(base).ok()?;
    Some(tidy(&base.join(path)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_unix_socket_is_where_its_links_lead() {
        let dir = std::env::temp_dir().join(format!("ra-jit-resolved-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("abs")).unwrap();
        std::os::unix::fs::symlink("real/../real", dir.join("rel")).unwrap();
        std::os::unix::fs::symlink("loop", dir.join("loop")).unwrap();
        let pid = std::process::id();
        let dir_s = super::tidy(&std::fs::canonicalize(&dir).unwrap());
        let at = |it: &str| super::resolved(pid, &format!("{dir_s}/{it}"));
        assert_eq!(at("abs/s.sock").as_deref(), Some(&*format!("{dir_s}/real/s.sock")));
        assert_eq!(at("rel/s.sock").as_deref(), Some(&*format!("{dir_s}/real/s.sock")));
        assert_eq!(at("missing/../real/s").as_deref(), Some(&*format!("{dir_s}/real/s")));
        assert_eq!(at("loop/s"), None);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_run_refuses_terminal_input_and_io_uring() {
        for (recorded, reads) in [(false, false), (true, false), (true, true)] {
            let program = super::filter_program(recorded, reads, 3);
            let has = |k: u32| program.iter().any(|it| it.k == k);
            assert!(has(super::TIOCSTI) && has(super::TIOCLINUX));
            assert!(has(super::NR_IO_URING_SETUP) && has(super::X32));
            assert!(program.len() < u16::MAX as usize);
        }
    }
}
