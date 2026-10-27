//! Running a program in a sandbox: `ra-jit sandbox SPEC -- PROGRAM ARGS...`. The program sees
//! the machine's file system read-only, the directories [`Spec::overlays`] names as they are
//! but with its writes kept to itself, those [`Spec::writable`] names written for real, empty
//! temporary directories of its own, the environment the spec gives and no other, and no
//! network but its own loopback. What it reached outside is written to [`Spec::record`] as it
//! ends: the writes and the network the sandbox refused it, and, when [`Spec::record_reads`],
//! every path it opened, looked at, listed or ran, for what it read to be part of what its
//! result is a function of. `cargo ra-jit test` runs every test executable in one
//! ([docs/cargo.md]); build scripts and proc macros are to run in the same ([#147]).
//!
//! This file is what every platform shares: the spec, the command, the probe, the record's
//! format and the marker a test harness gives the name of the test a thread runs by. Each
//! platform's sandbox is a module of its own with one entry, `run(spec, program)`: on Linux
//! namespaces, overlayfs and seccomp's user notification ([`linux`]); on macOS Seatbelt and a
//! library every process of the sandbox loads, which keeps the overlays' writes and records
//! (`sandbox/macos.rs`).
//!
//! [docs/cargo.md]: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// The subcommand of `ra-jit` that runs a program in a sandbox.
pub(crate) const COMMAND: &str = "sandbox";

/// What a test harness looks up, on the thread of a test, just before the test with the
/// test's name after it and just after it with nothing after it (`access(MARK + name, F_OK)`):
/// the sandbox takes the calls of that thread in between to be the test's. Never a path of
/// the record. Outside a sandbox it is one failed `access` a test.
pub(crate) const MARK: &str = "/.ra-jit-test/";

/// The status a sandbox that could not be made ends with, as `env` ends when it can't run
/// what it is given.
pub(crate) const COULD_NOT: i32 = 125;

/// The variable that turns sandboxes off (`off` or `0`), for tests, build scripts and
/// proc-macro servers alike. [`VAR_BLAZE`], when it is set, wins over it.
pub(crate) const VAR: &str = "RA_JIT_SANDBOX";

/// The other name of [`VAR`], which wins when both are set.
pub(crate) const VAR_BLAZE: &str = "BLAZE_SANDBOX";

/// Whether sandboxes are on, with `var` looking a variable up: unless [`VAR_BLAZE`], or
/// [`VAR`] when that is not set, is `off` or `0`. The one rule tests, build scripts and
/// proc-macro servers decide by.
pub(crate) fn on_with(var: impl Fn(&str) -> Option<String>) -> bool {
    let value = var(VAR_BLAZE).or_else(|| var(VAR));
    !matches!(value.as_deref(), Some("off" | "0"))
}

/// Whether sandboxes are on for this process, as [`on_with`] says of its environment.
pub(crate) fn on() -> bool {
    on_with(|name| std::env::var(name).ok())
}

/// What a program is run with.
#[derive(Serialize, Deserialize, Default, Clone)]
pub(crate) struct Spec {
    /// The directory it starts in.
    pub(crate) cwd: PathBuf,
    /// All of its environment. A sandbox may add variables of its own, which no identity is
    /// made of.
    pub(crate) env: Vec<(String, String)>,
    /// Directories it sees as they are and may change, its changes seen by it alone and gone
    /// when it ends.
    pub(crate) overlays: Vec<PathBuf>,
    /// Directories it writes for real.
    #[serde(default)]
    pub(crate) writable: Vec<PathBuf>,
    /// Directories it sees as they are and may not write, though they are below one of the
    /// sandbox's empty directories (a workspace or target directory under `/tmp`): the rest of
    /// the machine is seen read-only anyway.
    #[serde(default)]
    pub(crate) readable: Vec<PathBuf>,
    /// The file written as it ends with what it reached outside ([`Line`]), or nothing.
    pub(crate) record: Option<PathBuf>,
    /// Whether the record has the paths it read, which costs: every call that names a path
    /// goes through the sandbox. What was refused it is recorded either way.
    #[serde(default)]
    pub(crate) record_reads: bool,
    /// Paths that are not inputs of what runs: the program itself, whose code is known.
    pub(crate) not_inputs: Vec<PathBuf>,
}

/// What a line of a record says was reached.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Reached {
    /// `R`: a path opened, looked at, listed, run or read as a link.
    Read,
    /// `W`: a write the sandbox refused: outside the overlays, the writable directories and
    /// the program's own empty ones.
    Write,
    /// `N`: an address the sandbox refused a connection, a bind or a message to.
    Net,
    /// `X`: the record is not whole, and why: what was read is not all known.
    Incomplete,
}

impl Reached {
    fn letter(self) -> char {
        match self {
            Reached::Read => 'R',
            Reached::Write => 'W',
            Reached::Net => 'N',
            Reached::Incomplete => 'X',
        }
    }

    fn of(letter: &str) -> Option<Reached> {
        Some(match letter {
            "R" => Reached::Read,
            "W" => Reached::Write,
            "N" => Reached::Net,
            "X" => Reached::Incomplete,
            _ => return None,
        })
    }
}

/// A line of a record: `R`, `W`, `N` or `X`, the test, and the path, the address or why, by
/// tabs. The test is libtest's name of the one whose thread made the call, as [`MARK`] said,
/// or empty when that is not known (another thread, a child process, the program before or
/// after its tests): what has an empty test is taken to be every test's of the run. A record
/// has each line once, in order.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Line {
    pub(crate) reached: Reached,
    pub(crate) test: String,
    pub(crate) what: String,
}

/// The lines of the record `file`, `None` when there is none or it is not a record.
pub(crate) fn read_record(file: &Path) -> Option<Vec<Line>> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .map(|line| {
            let mut parts = line.splitn(3, '\t');
            let reached = Reached::of(parts.next()?)?;
            let (test, what) = (parts.next()?.to_owned(), parts.next()?.to_owned());
            Some(Line { reached, test, what })
        })
        .collect()
}

/// What a sandbox notes as its program runs, and writes to [`Spec::record`] when it ends.
pub(crate) struct Recorder {
    lines: BTreeSet<Line>,
    /// The test each thread runs, by the thread's id, as [`MARK`] said.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    threads: HashMap<u64, String>,
    reads: bool,
    not_inputs: BTreeSet<String>,
    /// Where writes are the program's: the overlays, the writable directories and the
    /// sandbox's own.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    written: Vec<String>,
    /// The readable directories: below one of the sandbox's own, but not written there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    readable: Vec<String>,
    overlays: Vec<String>,
    /// The sandbox's own directories, whose paths are no inputs: the kernel's views and the
    /// empty directories.
    own: Vec<String>,
}

impl Recorder {
    /// For `spec`, in a sandbox whose own directories are `own`.
    pub(crate) fn new(spec: &Spec, own: &[&str]) -> Recorder {
        let tidied = |it: &Vec<PathBuf>| it.iter().map(|it| tidy(it)).collect::<Vec<_>>();
        let overlays = tidied(&spec.overlays);
        let mut written = overlays.clone();
        written.extend(tidied(&spec.writable));
        written.extend(own.iter().map(|it| (*it).to_owned()));
        let readable = tidied(&spec.readable);
        // What is read there is an input, as what is read in an overlay is.
        let mut overlays = overlays;
        overlays.extend(readable.iter().cloned());
        Recorder {
            lines: BTreeSet::new(),
            threads: HashMap::new(),
            reads: spec.record_reads,
            not_inputs: spec.not_inputs.iter().map(|it| tidy(it)).collect(),
            written,
            readable,
            overlays,
            own: own.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    /// `test` (empty when it is not known) reached `what`, as `reached` says: for a sandbox
    /// that knows the test of a call itself, not by its thread. A read is kept only when reads
    /// are recorded and it is an input, as [`Recorder::read`] keeps it.
    pub(crate) fn reached(&mut self, reached: Reached, test: String, what: &str) {
        if reached == Reached::Read && !(self.reads && self.input(what)) {
            return;
        }
        // A tab or a line break would end the field: what has one is not recorded, and the
        // record is not whole.
        let (reached, what) = match what.contains(['\t', '\n']) {
            true => (Reached::Incomplete, "a path with a tab or a line break in it"),
            false => (reached, what),
        };
        self.lines.insert(Line { reached, test, what: what.to_owned() });
    }

    /// Whether `path`, made whole, is an input: not one of `not_inputs`, and not the sandbox's
    /// own but where an overlay is.
    fn input(&self, path: &str) -> bool {
        !self.not_inputs.contains(path)
            && (self.overlays.iter().any(|it| under(path, it))
                || !self.own.iter().any(|it| under(path, it)))
    }

    /// Writes the record to `file`, whole or not at all.
    pub(crate) fn write(&self, file: &Path) {
        let mut text = String::new();
        for line in &self.lines {
            text.push(line.reached.letter());
            text.push('\t');
            text.push_str(&line.test);
            text.push('\t');
            text.push_str(&line.what);
            text.push('\n');
        }
        let writing = file.with_extension("writing");
        if std::fs::write(&writing, text).and_then(|()| std::fs::rename(&writing, file)).is_err() {
            _ = std::fs::remove_file(file);
        }
    }
}

/// What a sandbox that knows the test of a call by its thread notes: the Linux one, whose
/// supervisor sees each call's thread and the harness's marker.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Recorder {
    /// Whether the reads are recorded.
    pub(crate) fn reads(&self) -> bool {
        self.reads
    }

    /// Takes `path`, as the program named it, as [`MARK`] when it is: `thread` runs the test
    /// after it, or none. Whether it was.
    pub(crate) fn marked(&mut self, thread: u64, path: &[u8]) -> bool {
        let Some(name) = path.strip_prefix(MARK.as_bytes()) else { return false };
        match name.is_empty() {
            true => _ = self.threads.remove(&thread),
            false => _ = self.threads.insert(thread, String::from_utf8_lossy(name).into_owned()),
        }
        true
    }

    fn test(&self, thread: u64) -> String {
        self.threads.get(&thread).cloned().unwrap_or_default()
    }

    /// `thread` read `path`, made whole: recorded when reads are and it is an input.
    pub(crate) fn read(&mut self, thread: u64, path: &str) {
        self.reached(Reached::Read, self.test(thread), path);
    }

    /// Whether a write to `path` is the program's to make.
    pub(crate) fn may_write(&self, path: &str) -> bool {
        self.written.iter().any(|it| under(path, it) && !self.only_read(path, it))
    }

    /// Whether `path` is in a readable directory that is below `dir`, one of the sandbox's
    /// own: it is seen there read-only.
    fn only_read(&self, path: &str, dir: &str) -> bool {
        self.readable.iter().any(|it| under(it, dir) && it != dir && under(path, it))
            && !self.written.iter().any(|it| under(it, dir) && it != dir && under(path, it))
    }

    /// Whether a connection to the Unix socket at `path`, made whole with its links followed,
    /// is the program's to make: one in the overlays, the writable directories or the
    /// sandbox's empty ones, which only the program's sockets can be in; not one of the
    /// kernel's views or the machine's `/dev`, which the sandbox sees as they are.
    pub(crate) fn may_connect(&self, path: &str) -> bool {
        let seen_as_is = ["/proc", "/sys", "/dev"].iter().any(|it| under(path, it));
        self.may_write(path) && (!seen_as_is || under(path, "/dev/shm"))
    }

    /// The sandbox refused `thread` a write to `path`.
    pub(crate) fn refused_write(&mut self, thread: u64, path: &str) {
        self.reached(Reached::Write, self.test(thread), path);
    }

    /// The sandbox refused `thread` the network at `address`.
    pub(crate) fn refused_net(&mut self, thread: u64, address: &str) {
        self.reached(Reached::Net, self.test(thread), address);
    }

    /// The record is not whole for what `thread` did, and why.
    pub(crate) fn incomplete(&mut self, thread: u64, why: &str) {
        self.reached(Reached::Incomplete, self.test(thread), why);
    }
}

/// The process group the signals the sandbox's own process is sent are passed on to: the
/// program's session, which the terminal no longer sends them to. 0 for none.
#[cfg(unix)]
static GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn pass_on(signal: libc::c_int) {
    let group = GROUP.load(std::sync::atomic::Ordering::Relaxed);
    if group > 0 {
        // SAFETY: `kill` is async-signal-safe.
        unsafe { libc::kill(-group, signal) };
    }
}

/// Has the signals a terminal or the caller ends a run with (`SIGINT`, `SIGTERM`, `SIGHUP`,
/// `SIGQUIT`) passed on to the process group `group`, the program's session, rather than end
/// the sandbox's own process, which then ends as the program does; 0 once the group is gone.
/// The handlers are made the first time: a process that forks after keeps them until it runs
/// a program, and passes nothing on.
#[cfg(unix)]
pub(crate) fn pass_signals_to(group: libc::pid_t) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    GROUP.store(group, std::sync::atomic::Ordering::Relaxed);
    INSTALLED.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: `pass_on` only loads an atomic and calls `kill`.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = pass_on as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    });
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
    std::process::exit(match backend(&spec, Some(&program)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ra-jit: the sandbox could not be made: {error}");
            COULD_NOT
        }
    })
}

/// Runs `program` in the sandbox `spec` says, or with `None` makes the sandbox and runs
/// nothing: what it ends with, its status or 128 and the signal that ended it.
fn backend(spec: &Spec, program: Option<&[OsString]>) -> Result<i32, String> {
    #[cfg(target_os = "linux")]
    return linux::run(spec, program);
    #[cfg(target_os = "macos")]
    return macos::run(spec, program);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        _ = (spec, program);
        Err(format!("there is no sandbox on {} yet", std::env::consts::OS))
    }
}

/// Whether programs can be run in a sandbox here, and why not when they can't: the probe
/// makes one around nothing, in a process of its own, once per process that asks.
pub(crate) fn available() -> Result<(), String> {
    static AVAILABLE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    AVAILABLE
        .get_or_init(|| {
            if !cfg!(any(target_os = "linux", target_os = "macos")) {
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

/// Makes a sandbox around nothing: `Ok` when it could be made, why not otherwise, with what
/// would let it be where that is known.
fn probe() -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("ra-jit-sandbox-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    let spec = Spec { cwd: dir.clone(), overlays: vec![dir.clone()], ..Spec::default() };
    let ended = backend(&spec, None);
    _ = std::fs::remove_dir_all(&dir);
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    let hint = match restricted.is_ok_and(|it| it.trim() == "1") {
        true => {
            " (AppArmor keeps users from making namespaces; \
             `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` lets them, \
             or an AppArmor profile that allows `userns` for ra-jit)"
        }
        false => "",
    };
    match ended {
        Ok(0) => Ok(()),
        Ok(_) => Err(format!("the sandbox could not be made (it said why above){hint}")),
        Err(error) => Err(format!("{error}{hint}")),
    }
}

/// What a container this process is in is known by, below `root` (`/` but in tests), with
/// `container` the variable of that name: `/.dockerenv` (Docker), `/run/.containerenv` (Podman),
/// the variable (systemd-nspawn, Podman, LXC), or `/proc/1/cgroup` naming a runtime (cgroup v1
/// Docker, Kubernetes, containerd). `None` outside one, as far as it can be told.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn container(root: &Path, container: Option<&str>) -> Option<String> {
    for file in [".dockerenv", "run/.containerenv"] {
        if root.join(file).exists() {
            return Some(format!("/{file}"));
        }
    }
    if let Some(name) = container.filter(|it| !it.is_empty()) {
        return Some(format!("`container={name}`"));
    }
    let cgroup = std::fs::read_to_string(root.join("proc/1/cgroup")).unwrap_or_default();
    ["docker", "kubepods", "containerd", "libpod", "lxc"]
        .into_iter()
        .find(|it| cgroup.contains(it))
        .map(|it| format!("/proc/1/cgroup naming {it}"))
}

/// What is said after a user namespace refused with `EPERM` in a container known by `known`
/// ([`container`]): the container's seccomp profile (Docker's default) is what refuses it, and
/// the ways on: a profile that allows it, or no sandbox at all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn container_hint(known: &str) -> String {
    format!(
        "; this runs in a container ({known}), and its seccomp profile (Docker's default) refuses \
         `unshare(CLONE_NEWUSER)`: run the container with `--security-opt seccomp=unconfined` \
         (or a seccomp profile that allows user namespaces; on Kubernetes \
         `securityContext.seccompProfile.type: Unconfined`), or set `{VAR_BLAZE}=off` to run \
         build scripts, proc macros and tests outside a sandbox, as cargo does"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blaze_sandbox_wins_over_ra_jit_sandbox() {
        let with = |vars: &'static [(&str, &str)]| {
            on_with(|name| vars.iter().find(|it| it.0 == name).map(|it| it.1.to_owned()))
        };
        assert!(with(&[]));
        assert!(!with(&[("RA_JIT_SANDBOX", "off")]) && !with(&[("RA_JIT_SANDBOX", "0")]));
        assert!(with(&[("RA_JIT_SANDBOX", "off"), ("BLAZE_SANDBOX", "on")]));
        assert!(!with(&[("RA_JIT_SANDBOX", "on"), ("BLAZE_SANDBOX", "off")]));
    }

    // The paths a sandbox records are a Unix machine's: there is one on Linux and macOS alone.
    #[cfg(unix)]
    #[test]
    fn a_container_is_known_by_its_marks() {
        let root =
            std::env::temp_dir().join(format!("ra-jit-container-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/1")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        assert_eq!(container(&root, None), None);
        assert_eq!(container(&root, Some("")), None);
        assert_eq!(container(&root, Some("podman")).as_deref(), Some("`container=podman`"));
        std::fs::write(root.join("proc/1/cgroup"), "0::/\n").unwrap();
        assert_eq!(container(&root, None), None);
        std::fs::write(root.join("proc/1/cgroup"), "12:pids:/kubepods/burstable/pod1/abc\n")
            .unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/proc/1/cgroup naming kubepods"));
        std::fs::write(root.join("run/.containerenv"), "").unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/run/.containerenv"));
        std::fs::write(root.join(".dockerenv"), "").unwrap();
        let known = container(&root, Some("podman")).unwrap();
        assert_eq!(known, "/.dockerenv");
        let hint = container_hint(&known);
        assert!(hint.contains("container (/.dockerenv)"), "{hint}");
        assert!(hint.contains("--security-opt seccomp=unconfined"), "{hint}");
        assert!(hint.contains("`BLAZE_SANDBOX=off`"), "{hint}");
        _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_is_its_lines_in_order_once() {
        let spec = Spec {
            overlays: vec!["/ws".into()],
            not_inputs: vec!["/ws/target/t".into(), "/bin/t".into()],
            record_reads: true,
            ..Spec::default()
        };
        let mut recorder = Recorder::new(&spec, &["/proc", "/tmp"]);
        recorder.read(1, "/etc/hosts");
        assert!(recorder.marked(7, b"/.ra-jit-test/tests::a"));
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/proc/self/maps");
        recorder.read(7, "/bin/t");
        recorder.refused_write(7, "/home/u/.config/x");
        recorder.refused_net(1, "1.2.3.4:443");
        assert!(recorder.marked(7, b"/.ra-jit-test/"));
        recorder.read(7, "/etc/passwd");
        recorder.read(7, "/ws/a\tb");
        assert!(!recorder.marked(7, b"/etc/x"));
        assert!(recorder.may_write("/ws/target/x") && recorder.may_write("/tmp"));
        assert!(!recorder.may_write("/wsx") && !recorder.may_write("/etc/x"));
        assert!(recorder.may_connect("/tmp/s.sock") && recorder.may_connect("/ws/target/s"));
        assert!(!recorder.may_connect("/proc/1/root/run/docker.sock"));
        assert!(!recorder.may_connect("/run/docker.sock") && !recorder.may_connect("/etc/s"));
        let dir = std::env::temp_dir().join(format!("ra-jit-record-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("record");
        recorder.write(&file);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            "R\t\t/etc/hosts\nR\t\t/etc/passwd\nR\ttests::a\t/ws/data.txt\n\
             W\ttests::a\t/home/u/.config/x\nN\t\t1.2.3.4:443\n\
             X\t\ta path with a tab or a line break in it\n"
        );
        let lines = read_record(&file).unwrap();
        assert_eq!(lines.len(), 6);
        assert_eq!(
            lines[2],
            Line { reached: Reached::Read, test: "tests::a".into(), what: "/ws/data.txt".into() }
        );
        _ = std::fs::remove_dir_all(&dir);
    }
}
