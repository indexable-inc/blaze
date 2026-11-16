//! D65: a native build script's run in the operating system's sandbox, under the policy
//! `blaze_build_script::native` makes from its [`Grant`].
//!
//! - macOS: `/usr/bin/sandbox-exec -p <profile>` (the system's own launcher of
//!   `sandbox_init`, the kernel mechanism blaze-cargo's sandbox and the interim builder use),
//!   which applies a `(deny default)` Seatbelt profile and executes the script in its place.
//!   blaze's own process stays outside, so it can be multi-threaded (blaze-cargo's raw
//!   `sandbox_init` needs a single-threaded launcher, which the rebuilt path has not).
//! - Linux: in the child between fork and exec, a new user and network namespace (no network
//!   but its own down loopback), `no_new_privs`, a seccomp filter refusing `socket` (no unix
//!   socket to a daemon of the host either), and a Landlock ruleset made in the parent:
//!   read, write and execute only beneath the grant's paths and [`LINUX_LOADER_READS`].
//!
//! The run is a process group of its own; at the deadline the group is killed and the run is
//! [`NativeRefusal::Deadline`] (rule 18). A path the grant cannot say is refused before
//! anything runs ([`NativeRefusal::Grant`]). Inside the run the kernel refuses what the grant
//! does not name (`EPERM` on macOS, `EACCES`/`EPERM` on Linux); naming each such path needs a
//! witness in the process (build-scripts.md, "Native scripts (D65)"), not built yet.

use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use blaze_build_script::native::{Grant, GrantError, seatbelt};
use blaze_own::Alloc;

/// What a Linux process reads before `main`: the dynamic loader's cache and the multiarch
/// library folder (glibc, libstdc++). `/usr/lib/gcc` (gcc's `cc1`) and `/usr/bin` are not read.
pub const LINUX_LOADER_READS: [&str; 2] = ["/usr/lib/aarch64-linux-gnu", "/etc/ld.so.cache"];

/// One native run.
#[derive(Clone, Copy, Debug)]
pub struct NativeRun<'g> {
    pub grant: Grant<'g>,
    pub args: &'g [&'g str],
    /// The whole environment: nothing of blaze's is inherited.
    pub env: &'g [(&'g str, &'g str)],
    /// The working directory: the package's folder, inside the grant.
    pub dir: &'g str,
    pub deadline: Duration,
}

/// How a run ended inside its sandbox.
#[derive(Debug)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A native run refused by the sandbox around it: each names the thing and the rule.
#[derive(Debug)]
pub enum NativeRefusal {
    /// The grant names a path no profile can say (D65).
    Grant { why: std::string::String },
    /// No sandbox can be made on this host (rule 9: nothing runs then).
    NoSandbox { why: std::string::String },
    /// The run outlived its deadline and was killed with its process group (rule 18).
    Deadline { program: std::string::String, after: Duration },
    /// Starting or waiting for the process failed.
    Io { what: &'static str, error: std::io::Error },
}

impl std::fmt::Display for NativeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeRefusal::Grant { why } => write!(f, "{why}"),
            NativeRefusal::NoSandbox { why } => write!(f, "no sandbox for a native build script on this host: {why} (rule 9)"),
            NativeRefusal::Deadline { program, after } => {
                write!(f, "{program}: the native build script outlived its deadline of {after:?} and was killed (rule 18)")
            }
            NativeRefusal::Io { what, error } => write!(f, "{what}: {error}"),
        }
    }
}

impl std::error::Error for NativeRefusal {}

/// Test-only planted bugs: each switches one part of the confinement off, so a hostile test
/// that still passes would prove nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The profile allows everything (macOS) or no ruleset, namespace or filter is applied
    /// (Linux).
    NoConfinement,
    /// The deadline is not enforced.
    NoDeadline,
}

/// Runs `run` confined.
pub fn run(run: &NativeRun<'_>, alloc: Alloc<'_>) -> Result<Ran, NativeRefusal> {
    run_planted(run, Plant::None, alloc)
}

fn refused(error: GrantError<'_>) -> NativeRefusal {
    NativeRefusal::Grant { why: error.to_string() }
}

/// [`run`] with a planted bug: only tests pass anything but [`Plant::None`].
pub fn run_planted(run: &NativeRun<'_>, plant: Plant, alloc: Alloc<'_>) -> Result<Ran, NativeRefusal> {
    let mut command = confined(run, plant, alloc)?;
    command.env_clear().current_dir(run.dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (name, value) in run.env {
        command.env(name, value);
    }
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[allow(clippy::disallowed_methods, reason = "D65: the confined script's process; its whole environment is given")]
    let mut child = command.spawn().map_err(|error| NativeRefusal::Io { what: "starting the confined build script", error })?;
    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(NativeRefusal::NoSandbox { why: "the child's pipes were not made".to_owned() });
    };
    let start = Instant::now();
    std::thread::scope(|scope| {
        let stdout = scope.spawn(move || {
            let mut bytes = Vec::new();
            out.read_to_end(&mut bytes).map(|_| bytes)
        });
        let stderr = scope.spawn(move || {
            let mut bytes = Vec::new();
            err.read_to_end(&mut bytes).map(|_| bytes)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => return Err(NativeRefusal::Io { what: "waiting for the confined build script", error }),
            }
            if plant != Plant::NoDeadline && start.elapsed() >= run.deadline {
                kill_group(child.id());
                _ = child.wait();
                return Err(NativeRefusal::Deadline { program: run.grant.program.to_owned(), after: run.deadline });
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        // The script is gone; what it left running in its group goes with it.
        kill_group(child.id());
        let joined = |handle: std::thread::ScopedJoinHandle<'_, std::io::Result<Vec<u8>>>| match handle.join() {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(NativeRefusal::Io { what: "reading the confined build script's output", error }),
            Err(_) => Err(NativeRefusal::NoSandbox { why: "a reader of the script's output panicked".to_owned() }),
        };
        Ok(Ran { code: status.code(), stdout: joined(stdout)?, stderr: joined(stderr)? })
    })
}

fn kill_group(pid: u32) {
    // SAFETY: `kill` takes integers only; the group is the child's own (`process_group(0)`).
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(target_os = "macos")]
fn confined(run: &NativeRun<'_>, plant: Plant, alloc: Alloc<'_>) -> Result<Command, NativeRefusal> {
    let profile = seatbelt(&run.grant, alloc).map_err(refused)?;
    let launcher = std::path::Path::new("/usr/bin/sandbox-exec");
    if !launcher.exists() {
        return Err(NativeRefusal::NoSandbox { why: "/usr/bin/sandbox-exec is missing".to_owned() });
    }
    let text = if plant == Plant::NoConfinement { "(version 1)\n(allow default)\n" } else { profile.as_str() };
    let mut command = Command::new(launcher);
    command.arg("-p").arg(text).arg(run.grant.program).args(run.args);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn confined(run: &NativeRun<'_>, plant: Plant, alloc: Alloc<'_>) -> Result<Command, NativeRefusal> {
    // The same checks the profile makes, so a grant refused on one host is refused on both.
    _ = seatbelt(&run.grant, alloc).map_err(refused)?;
    let mut command = Command::new(run.grant.program);
    command.args(run.args);
    if plant == Plant::NoConfinement {
        return Ok(command);
    }
    let ruleset = linux::Ruleset::of(&run.grant)?;
    let filter = linux::socket_filter();
    // SAFETY: the closure runs in the forked child before exec and makes only system calls
    // (no allocation, no lock): `prctl`, `unshare`, `seccomp` and `landlock_restrict_self` over
    // the ruleset descriptor and the filter made in the parent.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, move || linux::enter(&ruleset, &filter));
    }
    Ok(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn confined(_: &NativeRun<'_>, _: Plant, _: Alloc<'_>) -> Result<Command, NativeRefusal> {
    Err(NativeRefusal::NoSandbox { why: "native build scripts run only on macOS and Linux".to_owned() })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use blaze_build_script::native::Grant;

    use super::{LINUX_LOADER_READS, NativeRefusal};

    // Landlock's file-system rights (linux/landlock.h), ABI 1 to 3.
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const ALL_ABI1: u64 = (1 << 13) - 1;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// A Landlock ruleset made in the parent, applied in the child.
    pub(super) struct Ruleset {
        fd: OwnedFd,
    }

    fn os(what: &str) -> NativeRefusal {
        NativeRefusal::NoSandbox { why: format!("{what}: {}", std::io::Error::last_os_error()) }
    }

    impl Ruleset {
        pub(super) fn of(grant: &Grant<'_>) -> Result<Ruleset, NativeRefusal> {
            // SAFETY: a null attribute and size 0 with the version flag asks the ABI only.
            let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, 1u32) };
            if abi < 3 {
                return Err(NativeRefusal::NoSandbox { why: format!("Landlock ABI {abi} (3 or later needed: truncate is a write)") });
            }
            let handled = ALL_ABI1 | REFER | TRUNCATE;
            let attr = RulesetAttr { handled_access_fs: handled };
            // SAFETY: `attr` is borrowed for the call, its size given.
            let fd = unsafe {
                libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size_of::<RulesetAttr>(), 0u32)
            };
            if fd < 0 {
                return Err(os("landlock_create_ruleset"));
            }
            // SAFETY: the call returned a new descriptor, owned here alone.
            let ruleset = Ruleset { fd: unsafe { OwnedFd::from_raw_fd(fd as i32) } };
            let read = READ_FILE | READ_DIR;
            let file_read = READ_FILE;
            for path in LINUX_LOADER_READS.iter().chain(grant.read) {
                let file = std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false);
                ruleset.add(path, if file { file_read } else { read })?;
            }
            ruleset.add(grant.program, EXECUTE | READ_FILE)?;
            for program in grant.exec {
                ruleset.add(program, EXECUTE | READ_FILE)?;
            }
            for path in grant.write {
                ruleset.add(path, handled)?;
            }
            ruleset.add("/dev/null", READ_FILE | WRITE_FILE)?;
            Ok(ruleset)
        }

        fn add(&self, path: &str, access: u64) -> Result<(), NativeRefusal> {
            let opened = std::fs::File::options().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(path);
            let opened = match opened {
                Ok(file) => file,
                Err(error) => return Err(NativeRefusal::NoSandbox { why: format!("{path}: opened for the Landlock rule: {error}") }),
            };
            let rule = PathBeneath { allowed_access: access, parent_fd: opened.as_raw_fd() };
            // SAFETY: `rule` is borrowed for the call; the descriptors are open.
            let added = unsafe {
                libc::syscall(libc::SYS_landlock_add_rule, self.fd.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneath, 0u32)
            };
            if added < 0 {
                return Err(NativeRefusal::NoSandbox { why: format!("{path}: landlock_add_rule: {}", std::io::Error::last_os_error()) });
            }
            Ok(())
        }
    }

    use std::os::unix::fs::OpenOptionsExt;

    /// A seccomp filter: `socket` fails with `EPERM`, everything else is allowed.
    pub(super) fn socket_filter() -> [libc::sock_filter; 4] {
        let stmt = |code: u32, k: u32| libc::sock_filter { code: code as u16, jt: 0, jf: 0, k };
        let nr = libc::SYS_socket as u32;
        [
            stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0),
            libc::sock_filter { code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, jt: 0, jf: 1, k: nr },
            stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW),
        ]
    }

    /// In the child, before exec: only system calls.
    pub(super) fn enter(ruleset: &Ruleset, filter: &[libc::sock_filter; 4]) -> std::io::Result<()> {
        let check = |result: libc::c_long| if result < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) };
        // SAFETY: integers, and borrows held for each call.
        unsafe {
            check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) as libc::c_long)?;
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) as libc::c_long)?;
            let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
            check(libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program as *const libc::sock_fprog) as libc::c_long)?;
            check(libc::syscall(libc::SYS_landlock_restrict_self, ruleset.fd.as_raw_fd(), 0u32))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
