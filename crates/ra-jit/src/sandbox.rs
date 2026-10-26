//! Running a program in a sandbox: `ra-jit sandbox SPEC -- PROGRAM ARGS...`. The program sees
//! the machine's file system read-only, the directories [`Spec::overlays`] names as they are
//! but with its writes kept to itself, an empty `/tmp`, `/var/tmp` and `/dev/shm` of its own,
//! the environment the spec gives and no other, no network but its own loopback, and no other
//! process. Every path it opens, looks at, lists, runs or changes is recorded, for what it read
//! to be part of what its result is a function of ([`Spec::record`]). `cargo ra-jit test` runs
//! each test that reaches the world outside its code in one ([docs/cargo.md]); build scripts
//! and proc macros are to run in the same ([#147]).
//!
//! On Linux it is user, mount, network, IPC and PID namespaces, overlayfs and seccomp's user
//! notification, all of which an unprivileged user may use. Elsewhere [`available`] says why
//! there is none yet.
//!
//! [docs/cargo.md]: https://github.com/indexable-inc/ra-jit/blob/main/docs/cargo.md
//! [#147]: https://github.com/indexable-inc/ra-jit/issues/147

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// The subcommand of `ra-jit` that runs a program in a sandbox.
pub(crate) const COMMAND: &str = "sandbox";

/// The variable whose value names the run in [`Spec::record`]: a test's name.
pub(crate) const RUN: &str = "__RA_JIT_TEST";

/// Variables that pass from the command into the program besides [`Spec::env`], for the test
/// harness to read and take away.
const PASSED: &[&str] = &[RUN, "__RA_JIT_TEST_CAPTURE"];

/// The status a sandbox that could not be made ends with, as `env` ends when it can't run
/// what it is given.
const COULD_NOT: i32 = 125;

/// What a program is run with.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Spec {
    /// The directory it starts in.
    pub(crate) cwd: PathBuf,
    /// All of its environment.
    pub(crate) env: Vec<(String, String)>,
    /// Directories it sees as they are and may change, its changes seen by it alone and gone
    /// when it ends.
    pub(crate) overlays: Vec<PathBuf>,
    /// The directory where what the program touched is written as it ends, in a file named by
    /// [`record_name`] of the value of [`RUN`]: one path a line, those of the machine's own
    /// (`/proc`, `/sys`, `/dev`), of what is the program's own (`/tmp`), and of `not_inputs`
    /// left out.
    pub(crate) record: Option<PathBuf>,
    /// Paths that are not inputs of what runs: the program itself, whose code is known.
    pub(crate) not_inputs: Vec<PathBuf>,
}

/// The file [`Spec::record`] has for the run `name`.
pub(crate) fn record_name(name: &str) -> String {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(name.as_bytes());
    hash.iter().take(12).map(|it| format!("{it:02x}")).collect()
}

/// Whether `path` is the sandbox's own, not an input: the kernel's views and the directories
/// each program has empty.
fn own(path: &str) -> bool {
    ["/proc", "/sys", "/dev", "/tmp", "/var/tmp"].iter().any(|it| under(path, it))
}

/// Whether `path` is `dir` or below it.
fn under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `ra-jit sandbox SPEC -- PROGRAM ARGS...`, or `ra-jit sandbox --probe`: runs the program and
/// ends as it ended. Runs before anything else in the process, which is one thread: it forks.
pub(crate) fn command(mut args: impl Iterator<Item = OsString>) -> ! {
    let first = args.next();
    if first.as_deref() == Some("--probe".as_ref()) {
        std::process::exit(match probe() {
            Ok(()) => 0,
            Err(why) => {
                eprintln!("{why}");
                1
            }
        });
    }
    let spec = first.ok_or("a spec").and_then(|it| {
        let text = std::fs::read_to_string(&it).map_err(|_| "a spec it can read")?;
        serde_json::from_str::<Spec>(&text).map_err(|_| "a spec it can parse")
    });
    let program: Vec<OsString> = args.skip_while(|it| it != "--").skip(1).collect();
    let spec = match (spec, program.is_empty()) {
        (Ok(spec), false) => spec,
        (Err(what), _) => {
            eprintln!("ra-jit sandbox: needs {what}: ra-jit sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
        (_, true) => {
            eprintln!("ra-jit sandbox: needs a program: ra-jit sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
    };
    std::process::exit(run(&spec, &program))
}

/// Whether programs can be run in a sandbox here, and why not when they can't: the probe
/// makes one around nothing, in a process of its own, once per process that asks.
pub(crate) fn available() -> Result<(), String> {
    static AVAILABLE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    AVAILABLE
        .get_or_init(|| {
            if !cfg!(target_os = "linux") {
                return Err(format!("there is no sandbox on {} yet", std::env::consts::OS));
            }
            let exe =
                std::env::current_exe().map_err(|it| format!("ra-jit is not found ({it})"))?;
            let out = std::process::Command::new(exe)
                .args([COMMAND, "--probe"])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|it| format!("the probe did not run ({it})"))?;
            match out.status.success() {
                true => Ok(()),
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        })
        .clone()
}

#[cfg(target_os = "linux")]
fn probe() -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("ra-jit-sandbox-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    let spec = Spec { cwd: dir.clone(), overlays: vec![dir.clone()], ..Spec::default() };
    let ended = linux::run(&spec, None);
    _ = std::fs::remove_dir_all(&dir);
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    let hint = match restricted.is_ok_and(|it| it.trim() == "1") {
        true => {
            " (AppArmor keeps users from making namespaces; \
                 `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` lets them)"
        }
        false => "",
    };
    match ended {
        Ok(0) => Ok(()),
        Ok(_) => Err(format!("the sandbox could not be made (it said why above){hint}")),
        Err(error) => Err(format!("{error}{hint}")),
    }
}

#[cfg(not(target_os = "linux"))]
fn probe() -> Result<(), String> {
    Err(format!("there is no sandbox on {} yet", std::env::consts::OS))
}

/// Runs `program` in the sandbox `spec` says, and what it ends with: its status, or 128 and
/// the signal that ended it.
fn run(spec: &Spec, program: &[OsString]) -> i32 {
    #[cfg(target_os = "linux")]
    match linux::run(spec, Some(program)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ra-jit: the sandbox could not be made: {error}");
            COULD_NOT
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        _ = (spec, program);
        eprintln!("ra-jit: there is no sandbox on {} yet", std::env::consts::OS);
        COULD_NOT
    }
}

/// `path` with `.` and empty parts left out.
fn tidy(path: &Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

#[cfg(target_os = "linux")]
mod linux {
    //! The sandbox on Linux. The command's own process stays outside and is the supervisor:
    //! it forks the sandbox's first process, which enters new user, mount, network and IPC
    //! namespaces, makes every mount read-only, puts the overlays and the empty directories
    //! in place, brings up its loopback and forks the first process of a new PID namespace.
    //! That one mounts its `/proc`, forks the program's process and waits for it; when it
    //! ends, the kernel ends every other process of the namespace. The program's process
    //! installs a seccomp filter that has the kernel tell the supervisor of each call that
    //! names a path, hands it the filter's descriptor and runs the program; the supervisor
    //! reads each path from the program's memory, notes it and lets the call go on as it
    //! would have. The program runs as the user who ran it, with no capability left.

    use std::{
        collections::BTreeSet,
        ffi::{CString, OsString},
        os::unix::ffi::OsStrExt as _,
        path::{Path, PathBuf},
    };

    use super::{PASSED, RUN, Spec, own, record_name, tidy, under};

    /// What is done with every system call, by number and by which of its arguments are the
    /// directory a path is relative to and the path. The `*at` calls take the directory first.
    #[cfg(target_arch = "x86_64")]
    const CALLS: &[(u32, &[(Option<usize>, usize)])] = &[
        (2, &[(None, 0)]),                    // open
        (4, &[(None, 0)]),                    // stat
        (6, &[(None, 0)]),                    // lstat
        (21, &[(None, 0)]),                   // access
        (59, &[(None, 0)]),                   // execve
        (80, &[(None, 0)]),                   // chdir
        (82, &[(None, 0), (None, 1)]),        // rename
        (83, &[(None, 0)]),                   // mkdir
        (84, &[(None, 0)]),                   // rmdir
        (85, &[(None, 0)]),                   // creat
        (87, &[(None, 0)]),                   // unlink
        (89, &[(None, 0)]),                   // readlink
        (257, &[(Some(0), 1)]),               // openat
        (258, &[(Some(0), 1)]),               // mkdirat
        (262, &[(Some(0), 1)]),               // newfstatat
        (263, &[(Some(0), 1)]),               // unlinkat
        (264, &[(Some(0), 1), (Some(2), 3)]), // renameat
        (267, &[(Some(0), 1)]),               // readlinkat
        (269, &[(Some(0), 1)]),               // faccessat
        (316, &[(Some(0), 1), (Some(2), 3)]), // renameat2
        (322, &[(Some(0), 1)]),               // execveat
        (332, &[(Some(0), 1)]),               // statx
        (437, &[(Some(0), 1)]),               // openat2
        (439, &[(Some(0), 1)]),               // faccessat2
    ];
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64

    #[cfg(target_arch = "aarch64")]
    const CALLS: &[(u32, &[(Option<usize>, usize)])] = &[
        (34, &[(Some(0), 1)]),                // mkdirat
        (35, &[(Some(0), 1)]),                // unlinkat
        (38, &[(Some(0), 1), (Some(2), 3)]),  // renameat
        (48, &[(Some(0), 1)]),                // faccessat
        (49, &[(None, 0)]),                   // chdir
        (56, &[(Some(0), 1)]),                // openat
        (78, &[(Some(0), 1)]),                // readlinkat
        (79, &[(Some(0), 1)]),                // newfstatat
        (221, &[(None, 0)]),                  // execve
        (276, &[(Some(0), 1), (Some(2), 3)]), // renameat2
        (281, &[(Some(0), 1)]),               // execveat
        (291, &[(Some(0), 1)]),               // statx
        (437, &[(Some(0), 1)]),               // openat2
        (439, &[(Some(0), 1)]),               // faccessat2
    ];
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

    const SYS_PIDFD_OPEN: libc::c_long = 434;
    const SYS_MOUNT_SETATTR: libc::c_long = 442;
    const MOUNT_ATTR_RDONLY: u64 = 1;
    const AT_RECURSIVE: libc::c_uint = 0x8000;
    const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
    const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
    const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
    // _IOWR('!', 0, struct seccomp_notif) and _IOWR('!', 1, struct seccomp_notif_resp).
    const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;
    const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;

    #[repr(C)]
    #[derive(Default)]
    struct Notif {
        id: u64,
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
    /// nothing in it. What it ends with, as [`super::run`] says.
    pub(super) fn run(spec: &Spec, program: Option<&[OsString]>) -> Result<i32, String> {
        // Everything the processes of the sandbox use is made before the first fork.
        let argv: Vec<CString> = program
            .unwrap_or_default()
            .iter()
            .map(|it| CString::new(it.as_bytes()).unwrap_or_default())
            .collect();
        let mut env: Vec<CString> = spec
            .env
            .iter()
            .filter(|(name, _)| !PASSED.contains(&name.as_str()))
            .map(|(name, value)| CString::new(format!("{name}={value}")).unwrap_or_default())
            .collect();
        for name in PASSED {
            if let Some(value) = std::env::var_os(name) {
                let mut it = OsString::from(format!("{name}="));
                it.push(value);
                env.push(CString::new(it.as_bytes()).unwrap_or_default());
            }
        }
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
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        // SAFETY: the process is one thread; the child only makes system calls and exits.
        let first = unsafe { libc::fork() };
        if first < 0 {
            return Err(os("fork"));
        }
        if first == 0 {
            unsafe { libc::close(sockets[0]) };
            let code = match sandbox(spec, uid, gid, &argv, &env, sockets[1], program.is_none()) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("ra-jit: the sandbox could not be made: {error}");
                    super::COULD_NOT
                }
            };
            unsafe { libc::_exit(code) };
        }
        unsafe { libc::close(sockets[1]) };
        let listener = received(sockets[0]);
        unsafe { libc::close(sockets[0]) };
        let mut touched = BTreeSet::new();
        if let Some(listener) = listener {
            supervise(listener, first, &mut touched);
            unsafe { libc::close(listener) };
        }
        let mut status = 0;
        while unsafe { libc::waitpid(first, &mut status, 0) } < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(os("waitpid"));
            }
        }
        let code = ended(status);
        if let Some(dir) = &spec.record
            && program.is_some()
        {
            let run = std::env::var(RUN).unwrap_or_default();
            let not: BTreeSet<String> = spec.not_inputs.iter().map(|it| tidy(it)).collect();
            let overlays: Vec<String> = spec.overlays.iter().map(|it| tidy(it)).collect();
            let shown = |path: &String| {
                overlays.iter().any(|it| under(path, it)) || !own(path) && !not.contains(path)
            };
            let mut text = String::new();
            for path in touched.iter().filter(|it| shown(it) && !not.contains(*it)) {
                text.push_str(path);
                text.push('\n');
            }
            let file = dir.join(record_name(&run));
            let writing = file.with_extension("writing");
            if std::fs::write(&writing, text)
                .and_then(|()| std::fs::rename(&writing, &file))
                .is_err()
            {
                _ = std::fs::remove_file(&file);
            }
        }
        Ok(code)
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
        argv: &[CString],
        env: &[CString],
        socket: libc::c_int,
        probe: bool,
    ) -> Result<i32, String> {
        let flags =
            libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC;
        if unsafe { libc::unshare(flags) } < 0 {
            return Err(os("a user namespace (unshare)"));
        }
        let write = |file: &str, text: String| {
            std::fs::write(file, text).map_err(|it| format!("{file}: {it}"))
        };
        write("/proc/self/setgroups", "deny".to_owned())?;
        write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
        let root = c(Path::new("/"));
        let none = std::ptr::null::<libc::c_char>();
        if unsafe {
            libc::mount(
                none,
                root.as_ptr(),
                none,
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(os("making the mounts private"));
        }
        let attr =
            MountAttr { attr_set: MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
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
                libc::mount(
                    kind.as_ptr(),
                    at_c.as_ptr(),
                    kind.as_ptr(),
                    flags,
                    options.as_ptr().cast(),
                )
            } < 0
            {
                return Err(os(&format!("an empty {}", at.display())));
            }
            Ok(())
        };
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
        // The overlays are reached through descriptors opened before the empty `/tmp` hides
        // what is there: a workspace under `/tmp` is mounted again inside it. Their own
        // directories are on a tmpfs that the program's `/tmp` then hides as well.
        let lowers = spec.overlays.iter().map(|it| opened(it)).collect::<Result<Vec<_>, _>>()?;
        let staging = Path::new("/tmp");
        tmpfs(staging)?;
        for at in 0..spec.overlays.len() {
            for it in [staging.join(format!("{at}/upper")), staging.join(format!("{at}/work"))] {
                std::fs::create_dir_all(&it)
                    .map_err(|error| format!("{}: {error}", it.display()))?;
            }
        }
        let staged = opened(staging)?;
        tmpfs(staging)?;
        // The other empty directories go in place before the overlays too: a workspace or a
        // `TMPDIR` under `/dev/shm` is mounted again inside its empty one.
        for dir in ["/var/tmp", "/dev/shm"].map(Path::new).iter().filter(|it| it.is_dir()) {
            tmpfs(dir)?;
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
        drop((lowers, staged));
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
            let code = match first_process(&cwd, argv, env, socket, probe) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("ra-jit: the sandbox could not be made: {error}");
                    super::COULD_NOT
                }
            };
            unsafe { libc::_exit(code) };
        }
        unsafe { libc::close(socket) };
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
        let socket =
            unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
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
    fn first_process(
        cwd: &CString,
        argv: &[CString],
        env: &[CString],
        socket: libc::c_int,
        probe: bool,
    ) -> Result<i32, String> {
        let proc_ = c(Path::new("/proc"));
        let kind = c(Path::new("proc"));
        let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        // Where `/proc` can't be mounted again, the one there still shows each process
        // itself as `/proc/self`.
        unsafe {
            libc::mount(kind.as_ptr(), proc_.as_ptr(), kind.as_ptr(), flags, std::ptr::null())
        };
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
            let error = exec(argv, env, socket);
            eprintln!("ra-jit: could not run {}: {error}", argv[0].to_string_lossy());
            unsafe { libc::_exit(127) };
        }
        unsafe { libc::close(socket) };
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid == program {
                return Ok(ended(status));
            }
            if pid < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(os("waitpid"));
            }
        }
    }

    /// Installs the filter, hands its descriptor to the supervisor over `socket` and runs
    /// the program; what kept it from running.
    fn exec(argv: &[CString], env: &[CString], socket: libc::c_int) -> String {
        let listener = match filter() {
            Ok(it) => it,
            Err(error) => return error,
        };
        if let Err(error) = send(socket, listener) {
            return error;
        }
        unsafe { libc::close(listener) };
        unsafe { libc::close(socket) };
        let mut argv: Vec<*const libc::c_char> = argv.iter().map(|it| it.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|it| it.as_ptr()).collect();
        envp.push(std::ptr::null());
        unsafe { libc::execve(argv[0], argv.as_ptr(), envp.as_ptr()) };
        std::io::Error::last_os_error().to_string()
    }

    /// The seccomp filter that has the kernel ask the supervisor about every call of
    /// [`CALLS`], and the descriptor it asks through.
    fn filter() -> Result<libc::c_int, String> {
        const LD_W_ABS: u16 = 0x20;
        const JEQ_K: u16 = 0x15;
        const RET_K: u16 = 0x06;
        let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        let n = CALLS.len() as u8;
        let mut program = vec![
            insn(LD_W_ABS, 0, 0, 4), // the architecture
            insn(JEQ_K, 1, 0, ARCH),
            insn(RET_K, 0, 0, SECCOMP_RET_ALLOW),
            insn(LD_W_ABS, 0, 0, 0), // the call
        ];
        for (at, (nr, _)) in CALLS.iter().enumerate() {
            // To the notification after the last comparison when equal.
            program.push(insn(JEQ_K, n - at as u8, 0, *nr));
        }
        program.push(insn(RET_K, 0, 0, SECCOMP_RET_ALLOW));
        program.push(insn(RET_K, 0, 0, SECCOMP_RET_USER_NOTIF));
        let prog = libc::sock_fprog { len: program.len() as u16, filter: program.as_mut_ptr() };
        // The process holds every capability of its user namespace until it runs the
        // program, so it may install a filter without `no_new_privs`.
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
            if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
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

    /// Answers the kernel's notifications through `listener`, noting each path in `touched`,
    /// until the process `first` has ended, or no process is left that the filter is on.
    fn supervise(listener: libc::c_int, first: libc::pid_t, touched: &mut BTreeSet<String>) {
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
                answer(listener, touched);
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

    /// Takes one notification, notes the paths of its call and lets the call go on.
    fn answer(listener: libc::c_int, touched: &mut BTreeSet<String>) {
        let mut notif = Notif::default();
        // SAFETY: `notif` is a zeroed `struct seccomp_notif`.
        if unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV as _, &mut notif) } < 0 {
            return;
        }
        if let Some((_, paths)) = CALLS.iter().find(|(nr, _)| *nr as i32 == notif.nr) {
            for &(dir, path) in paths.iter() {
                let dir = dir.map(|it| notif.args[it] as i32);
                if let Some(path) = path_of(notif.pid, dir, notif.args[path]) {
                    touched.insert(path);
                }
            }
        }
        let resp =
            NotifResp { id: notif.id, val: 0, error: 0, flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE };
        // SAFETY: `resp` is a `struct seccomp_notif_resp`. A process that ended meanwhile
        // makes it fail, which is nothing to answer.
        unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND as _, &resp) };
    }

    /// The path the call of `pid` names at `address`, made whole from the directory `dir`
    /// (`AT_FDCWD` or a descriptor, or the working directory when `None`). `None` for an
    /// empty path, which is the descriptor itself.
    fn path_of(pid: u32, dir: Option<i32>, address: u64) -> Option<String> {
        let mut bytes = Vec::new();
        let mut at = address;
        while bytes.len() < 4096 {
            // To the end of the page, which is mapped if the byte at `at` is.
            let chunk = 4096 - (at as usize % 4096);
            let mut buffer = vec![0u8; chunk];
            let local = libc::iovec { iov_base: buffer.as_mut_ptr().cast(), iov_len: chunk };
            let remote = libc::iovec { iov_base: at as *mut libc::c_void, iov_len: chunk };
            let read =
                unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
            if read <= 0 {
                return None;
            }
            let read = &buffer[..read as usize];
            if let Some(end) = read.iter().position(|&it| it == 0) {
                bytes.extend_from_slice(&read[..end]);
                break;
            }
            bytes.extend_from_slice(read);
            at += read.len() as u64;
        }
        if bytes.is_empty() {
            return None;
        }
        let path = PathBuf::from(<OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes));
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
}
