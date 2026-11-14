//! A program run with its whole environment given, its outputs to files, under a deadline that
//! kills its whole process group: how a script that times a program (`in_process`) or checks
//! that it works (`smoke`) runs it. Reaped with `wait4`, so the kernel's peak resident memory
//! and CPU time of the child come back with its exit status.
//!
//! The child leads a process group of its own, so the kill at the deadline takes what it
//! started too (a build's helpers, a program `blaze run` runs); a daemon, which starts in a
//! group of its own, outlives it, and the caller stops it (`blaze stop`).

use std::{
    ffi::OsStr,
    fs::File,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How often a running child is asked whether it ended: the wall time's resolution.
pub const POLL: Duration = Duration::from_millis(1);

/// The kernel's account of a reaped child (`struct rusage`, the same layout on 64-bit macOS
/// and Linux: two `timeval`s of 16 bytes, then 14 `long`s, `ru_maxrss` first).
#[repr(C)]
struct Rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    rest: [i64; 13],
}

/// `struct rusage`'s size on 64-bit macOS and Linux, which [`Rusage`] must have.
const RUSAGE_BYTES: usize = 144;

const WNOHANG: i32 = 1;
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn wait4(pid: i32, status: *mut i32, options: i32, rusage: *mut Rusage) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

/// `ru_maxrss` in bytes: macOS counts bytes, Linux kibibytes.
fn rss_bytes(maxrss: i64) -> u64 {
    let it = u64::try_from(maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") { it } else { it * 1024 }
}

/// One run that ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub wall_ns: u64,
    /// The child's peak resident memory, the kernel's count, in bytes.
    pub rss_bytes: u64,
    /// User and system CPU time of the child.
    pub cpu_ns: u64,
    /// The exit status; -1 when a signal ended it.
    pub status: i32,
}

/// How a bounded run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It exited (or a signal ended it) before the deadline.
    Exited(Measured),
    /// It was still running at the deadline, `after` from its start; its process group was
    /// killed and the child reaped.
    TimedOut { after: Duration },
}

/// Runs `argv` in `cwd` with exactly `env` (nothing of this process's environment), no
/// standard input, its outputs to `<log>.out` and `<log>.err`, as the leader of a process group
/// of its own. Bounded by `limit`: at it the whole group is killed and the outcome is
/// [`Ended::TimedOut`]. `Err` is a run that could not be started or waited for, with why.
pub fn run_logged<E: AsRef<OsStr>>(argv: &[String], cwd: &Path, env: &[(&str, E)], log: &Path, limit: Duration) -> Result<Ended, String> {
    let (program, args) = argv.split_first().ok_or("no program to run")?;
    let file = |ext: &str| {
        let path = log.with_extension(ext);
        File::create(&path).map_err(|e| format!("{} could not be made: {e}", path.display()))
    };
    #[allow(clippy::disallowed_methods, reason = "the one place a script starts a bounded, logged program; its environment and `cwd` are given whole")]
    let mut command = Command::new(program);
    command.env_clear().args(args).current_dir(cwd).stdin(Stdio::null()).stdout(file("out")?).stderr(file("err")?).process_group(0);
    for (name, value) in env {
        command.env(name, value);
    }
    let began = Instant::now();
    let child = command.spawn().map_err(|e| format!("{program} did not start in {}: {e}", cwd.display()))?;
    let pid = i32::try_from(child.id()).map_err(|_| format!("pid {} is not a pid_t", child.id()))?;
    let deadline = began + limit;
    let mut status = 0i32;
    let mut usage = Rusage { utime: [0; 2], stime: [0; 2], maxrss: 0, rest: [0; 13] };
    // Bounded: it ends at the deadline at the latest, asking every `POLL`.
    loop {
        // SAFETY: `pid` is this process's unreaped child (`Child` never waits on drop); both
        // pointers are to locals that live across the call.
        let got = unsafe { wait4(pid, &mut status, WNOHANG, &mut usage) };
        match got {
            0 if Instant::now() >= deadline => {
                // SAFETY: as above; the child is not reaped yet, so its pid is still its own
                // and still names its process group (`process_group(0)`: the group's id is
                // the child's pid), which the negative pid kills whole.
                unsafe {
                    kill(-pid, SIGKILL);
                    wait4(pid, &mut status, 0, &mut usage);
                }
                return Ok(Ended::TimedOut { after: limit });
            }
            0 => {
                #[allow(clippy::disallowed_methods, reason = "a script polls its child; it is on no pool")]
                std::thread::sleep(POLL);
            }
            it if it == pid => break,
            _ => return Err(format!("wait4({pid}): {}", std::io::Error::last_os_error())),
        }
    }
    let wall_ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let tv_ns = |tv: [i64; 2]| u64::try_from(tv[0]).unwrap_or(0) * 1_000_000_000 + u64::try_from(tv[1] & 0xffff_ffff).unwrap_or(0) * 1_000;
    // `WIFEXITED`/`WEXITSTATUS`, the same on both: a signal's end is -1.
    let status = if status & 0x7f == 0 { (status >> 8) & 0xff } else { -1 };
    Ok(Ended::Exited(Measured { wall_ns, rss_bytes: rss_bytes(usage.maxrss), cpu_ns: tv_ns(usage.utime) + tv_ns(usage.stime), status }))
}

/// Whether the process `pid` is alive (signal 0: only asks).
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only asks whether `pid` exists.
    unsafe { kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rusage_has_the_kernels_layout() {
        assert_eq!(std::mem::size_of::<Rusage>(), RUSAGE_BYTES);
    }

    #[test]
    fn a_status_and_a_peak_come_back() {
        let dir = crate::TempDir::new("blaze-script-child-").unwrap();
        let log = dir.path().join("exit");
        let argv = ["/bin/sh".to_owned(), "-c".to_owned(), "echo said; exit 3".to_owned()];
        let ran = run_logged(&argv, dir.path(), &[] as &[(&str, &Path)], &log, Duration::from_secs(10)).unwrap();
        let Ended::Exited(it) = ran else { panic!("a run of `exit 3` timed out: {ran:?}") };
        assert_eq!(it.status, 3);
        assert!(it.rss_bytes > 0);
        assert_eq!(std::fs::read_to_string(log.with_extension("out")).unwrap(), "said\n");
    }

    #[test]
    fn the_deadline_kills_the_whole_group() {
        let dir = crate::TempDir::new("blaze-script-child-").unwrap();
        let log = dir.path().join("slow");
        // The grandchild holds the group; killing only the child would leave it.
        let argv = ["/bin/sh".to_owned(), "-c".to_owned(), "sleep 30 & echo $! > pid; wait".to_owned()];
        let started = Instant::now();
        let ran = run_logged(&argv, dir.path(), &[] as &[(&str, &Path)], &log, Duration::from_millis(500)).unwrap();
        assert_eq!(ran, Ended::TimedOut { after: Duration::from_millis(500) });
        assert!(started.elapsed() < Duration::from_secs(10));
        let grandchild: i32 = std::fs::read_to_string(dir.path().join("pid")).unwrap().trim().parse().unwrap();
        let gone = Instant::now() + Duration::from_secs(5);
        // Bounded: five seconds for the kernel to deliver the kill.
        while alive(grandchild) && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(grandchild), "the grandchild {grandchild} outlived the kill of its group");
    }
}
