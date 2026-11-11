//! Starting the process of an [`crate::Effect::Spawn`] and making its [`Exit`], for every
//! driver on Unix. A driver starts it here, writes its input, reads its output and learns that
//! it exited in its own way (a thread that blocks, the ring and a pidfd, a kqueue), then reaps
//! it and asks [`Finish::exited`].
//!
//! The process's memory is held by the system where a limit is asked for: a cgroup of its own
//! on Linux, which it joins before it runs its program, and on macOS a watch of its footprint.
//! Where none can be made, it is not started.
//!
//! Every child [`start`] made is listed ([`unreaped`]) from its start until [`reap`], the one
//! place a child is reaped, takes it out: so a child is reaped once, and one nothing reaps (its
//! driver's loop stuck in a turn, as the daemon's was inside a maintain pass on 72e9a0dd; or its
//! [`Finish`] dropped unreaped) is named, with what waits for its exit, by whoever asks.

#[allow(
    unsafe_code,
    reason = "fork, exec and the calls the child makes before exec, which std cannot make"
)]
mod spawn;

#[cfg(test)]
mod tests;

use std::{
    fmt, io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

use crate::lock::Lock;

use crate::{Command, Confine, Confined, Exit, Sandbox, Tag};

static RUNS: AtomicU64 = AtomicU64::new(0);

/// A process started, its pipes the driver's.
pub(crate) struct Started {
    pub(crate) pid: libc::pid_t,
    /// Readable once it exits; made with it, so it names this process and no later one.
    #[cfg(target_os = "linux")]
    pub(crate) pidfd: OwnedFd,
    /// Where its input goes, of a command with input; closing it is the end of the input.
    pub(crate) stdin: Option<OwnedFd>,
    pub(crate) stdout: OwnedFd,
    pub(crate) stderr: OwnedFd,
    pub(crate) finish: Finish,
}

/// What is needed to say how the process ended, once it has.
pub(crate) struct Finish {
    pub(crate) command: Box<Command>,
    // dyn: the platform's sandbox, given by whoever owns it; asked once per spawn.
    confine: Option<Arc<dyn Confine>>,
    scratch: Option<PathBuf>,
    limit: Limit,
    /// Its place in [`CHILDREN`] until it is reaped.
    listed: Listed,
    /// When [`start`] began, what its sandbox took, and when the spawn call returned: what a
    /// [`crate::reaped::Reaped`] says of it.
    spawned: Instant,
    sandbox: Option<Duration>,
    executed: Instant,
}

/// The children [`start`] made that [`reap`] has not reaped, in the order they started: a few
/// hundred at most (the spawns in flight), each a field update under the lock, which no wait
/// is made under.
static CHILDREN: Lock<Vec<Child>> = Lock::new("core.process.children", Vec::new());

/// A child as [`CHILDREN`] lists it.
struct Child {
    pid: libc::pid_t,
    /// Its start's number ([`RUNS`]): a pid is reused once reaped, a run never is.
    run: u64,
    program: PathBuf,
    waiter: Waiter,
    started: Instant,
}

/// What waits for a child's exit, and so reaps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiter {
    /// The thread that started it, which waits for it in place (the threads driver's errand).
    Starter(ThreadId),
    /// A driver's spawn effect of this tag: its loop reaps the child as it polls, and the
    /// asker of the effect waits for the completion.
    Effect(Tag),
    /// No one: its driver dropped its [`Finish`] without reaping it.
    Abandoned,
}

impl fmt::Display for Waiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Waiter::Starter(thread) => {
                write!(f, "the thread that started it ({thread:?}), which reaps it in place")
            }
            Waiter::Effect(tag) => write!(
                f,
                "the spawn effect tagged {:#x}, reaped only as its driver's loop polls; its asker waits for the completion",
                tag.0
            ),
            Waiter::Abandoned => f.write_str("no one: its driver let go of it unreaped"),
        }
    }
}

/// Where a listed child stands, as the system says it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    /// It exited and is a zombie until it is reaped.
    Exited,
    /// Reaped between the listing and the look, or no child of this process.
    Gone,
    /// The look failed, with this `errno`.
    Unknown(i32),
}

/// A child started and not reaped, as [`unreaped`] says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreaped {
    pub pid: libc::pid_t,
    pub program: PathBuf,
    pub waiter: Waiter,
    /// How long ago it started.
    pub age: Duration,
    pub state: State,
}

impl fmt::Display for Unreaped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            State::Running => "running",
            State::Exited => "exited and not reaped",
            State::Gone => "reaped as it was looked at",
            State::Unknown(_) => "in a state its look could not read",
        };
        write!(
            f,
            "pid {} ({}), {state}, started {} s ago, waited for by {}",
            self.pid,
            self.program.display(),
            self.age.as_secs(),
            self.waiter
        )?;
        if let State::Unknown(errno) = self.state {
            write!(f, " ({})", io::Error::from_raw_os_error(errno))?;
        }
        Ok(())
    }
}

/// Every child started and not yet reaped, oldest first, each with what waits for it and
/// whether it already exited: a zombie whose waiter is an effect says its driver's loop
/// stopped polling. Reads each child's state without reaping it.
pub fn unreaped() -> Vec<Unreaped> {
    let now = Instant::now();
    let listed: Vec<(libc::pid_t, PathBuf, Waiter, Duration)> = (CHILDREN.lock().iter())
        .map(|it| {
            (it.pid, it.program.clone(), it.waiter, now.saturating_duration_since(it.started))
        }) // clone: the program's path, out of the lock, once per report
        .collect(); // alloc: a stall report's list, once per report
    listed
        .into_iter()
        .map(|(pid, program, waiter, age)| {
            let state = match crate::raw::has_exited(pid) {
                Ok(true) => State::Exited,
                Ok(false) => State::Running,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => State::Gone,
                Err(error) => State::Unknown(error.raw_os_error().unwrap_or(0)),
            };
            Unreaped { pid, program, waiter, age, state }
        })
        .collect() // alloc: as above
}

/// A child's place in [`CHILDREN`], held by its [`Finish`]: dropped once [`reap`] took the
/// child out, it does nothing; dropped with the child still listed, the child stays listed as
/// [`Waiter::Abandoned`], a zombie or a running process no one will reap.
struct Listed {
    pid: libc::pid_t,
    run: u64,
}

impl Listed {
    fn new(pid: libc::pid_t, run: u64, program: &Path) -> Listed {
        let program = program.to_path_buf(); // alloc: the program's path, once per spawn, what a stall report names it by
        let child = Child {
            pid,
            run,
            program,
            waiter: Waiter::Starter(std::thread::current().id()),
            started: Instant::now(),
        };
        CHILDREN.lock().push(child); // alloc: the list grows to the most children in flight at once
        Listed { pid, run }
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        // By run, not pid: reaped, its pid may already be a later child's.
        if let Some(child) = CHILDREN.lock().iter_mut().find(|it| it.run == self.run) {
            child.waiter = Waiter::Abandoned;
        }
    }
}

/// Takes `pid` out of [`CHILDREN`]: reaped, so its pid may be another's from now on.
fn unlist(pid: libc::pid_t) {
    let mut children = CHILDREN.lock();
    if let Some(at) = children.iter().position(|it| it.pid == pid) {
        children.remove(at);
    }
}

enum Limit {
    None,
    #[cfg(target_os = "linux")]
    Cgroup(crate::cgroup::Cgroup),
    #[cfg(target_os = "macos")]
    Footprint(crate::footprint::Watch),
}

/// `command` started: in its sandbox, made by `confine`, unless the sandbox is off; with its
/// memory held where a limit is asked for. Its input is in `finish.command.stdin`, for the
/// driver to write to `stdin`. A sandboxed one gets a directory of its own under `spawns`.
pub(crate) fn start(
    spawns: &Path,
    command: Box<Command>,
    // dyn: as `Finish::confine`.
    confine: Option<&Arc<dyn Confine>>,
) -> io::Result<Started> {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let began = Instant::now();
    let (confined, scratch, confine): (Option<Confined>, _, _) = match &command.sandbox {
        Sandbox::Off => (None, None, None),
        Sandbox::On { .. } => {
            let Some(confine) = confine else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: an error path.
                    format!(
                        "{}: a sandbox is asked for and this driver was given nothing to make \
                         one (no Confine); nothing runs outside one",
                        command.program.display()
                    ),
                ));
            };
            // alloc: once per sandboxed spawn, its scratch directory, kept until it exits.
            let scratch = spawns.join(format!("{}-{n}", std::process::id()));
            _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            match confine.confine(&command, &scratch) {
                // clone: the sandbox maker, kept to ask what the process read.
                Ok(confined) => (Some(confined), Some(scratch), Some(Arc::clone(confine))),
                Err(why) => {
                    _ = std::fs::remove_dir_all(&scratch);
                    // alloc: an error path.
                    return Err(io::Error::other(format!(
                        "{}: no sandbox can be made here: {why}",
                        command.program.display()
                    )));
                }
            }
        }
    };
    let sandbox = confined.as_ref().map(|_| began.elapsed());
    // What the sandbox starts, else the command itself, borrowed either way.
    let program = match &confined {
        Some(it) => {
            spawn::Program { program: &it.program, args: &it.args, env: &it.env, dir: &it.dir }
        }
        None => spawn::Program {
            program: &command.program,
            args: &command.args,
            env: &command.env,
            dir: &command.dir,
        },
    };
    let removing = |error: io::Error| {
        if let Some(scratch) = &scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        error
    };
    #[cfg(target_os = "linux")]
    let cgroup = match command.memory {
        Some(bytes) => Some(crate::cgroup::Cgroup::new(bytes).map_err(|error| {
            removing(io::Error::new(
                error.kind(),
                // alloc: an error path.
                format!(
                    "{}: no cgroup can hold its memory to {bytes} bytes: {error}",
                    command.program.display()
                ),
            ))
        })?),
        None => None,
    };
    #[cfg(target_os = "linux")]
    let procs = cgroup.as_ref().map(crate::cgroup::Cgroup::procs);
    #[cfg(not(target_os = "linux"))]
    let procs = None;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if let Some(bytes) = command.memory {
        return Err(removing(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "{}: no limit of {bytes} bytes can be held on this system",
                command.program.display()
            ),
        )));
    }
    let spawned = spawn::spawn(&program, !command.stdin.is_empty(), procs).map_err(|error| {
        removing(io::Error::new(
            error.kind(),
            // alloc: an error path.
            format!("starting {}: {error}", program.program.display()),
        ))
    })?;
    let executed = Instant::now();
    let pid = spawned.pid;
    #[cfg(target_os = "linux")]
    let limit = cgroup.map_or(Limit::None, Limit::Cgroup);
    #[cfg(target_os = "macos")]
    let limit = match command.memory {
        Some(bytes) => Limit::Footprint(crate::footprint::Watch::start(pid, bytes)),
        None => Limit::None,
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let limit = Limit::None;
    let listed = Listed::new(pid, n, &command.program);
    Ok(Started {
        pid,
        #[cfg(target_os = "linux")]
        pidfd: spawned.pidfd,
        stdin: spawned.stdin,
        stdout: spawned.stdout,
        stderr: spawned.stderr,
        finish: Finish {
            command,
            confine,
            scratch,
            limit,
            listed,
            spawned: began,
            sandbox,
            executed,
        },
    })
}

impl Finish {
    /// What waits for the child's exit from now on: the driver's spawn effect `tag`, whose
    /// loop reaps it ([`unreaped`] names it so).
    pub(crate) fn waited_by(&self, tag: Tag) {
        if let Some(child) = CHILDREN.lock().iter_mut().find(|it| it.run == self.listed.run) {
            child.waiter = Waiter::Effect(tag);
        }
    }

    /// Reaps `pid`, which has exited (a zombie, so its pid is no one else's), and says how it
    /// ended from what `wait4` said of it and what it wrote. Asks its sandbox what it read,
    /// and removes what was made for it.
    pub(crate) fn exited(
        self,
        pid: libc::pid_t,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> io::Result<Exit> {
        debug_assert_eq!(pid, self.listed.pid, "a child's `Finish` reaps that child");
        // Before the reap: the footprint watch signals the pid, which a reap frees for reuse.
        let limited = match self.limit {
            Limit::None => Ok(0),
            #[cfg(target_os = "linux")]
            Limit::Cgroup(cgroup) => cgroup.peak(),
            #[cfg(target_os = "macos")]
            Limit::Footprint(watch) => Ok(watch.stop()),
        };
        // Before the reap, which unlists it: the spawn effect that waits for it, whose label
        // says what it was.
        let label = match crate::reaped::observed() {
            true => {
                CHILDREN.lock().iter().find(|it| it.run == self.listed.run).and_then(|it| match it
                    .waiter
                {
                    Waiter::Effect(tag) => Some(tag),
                    Waiter::Starter(_) | Waiter::Abandoned => None,
                })
            }
            false => None,
        }
        .and_then(crate::reaped::take);
        let reaped = reap(pid);
        let ((status, rusage), limited) = match (reaped, limited) {
            (Ok(reaped), Ok(limited)) => (reaped, limited),
            (Err(error), _) | (_, Err(error)) => {
                if let Some(scratch) = &self.scratch {
                    _ = std::fs::remove_dir_all(scratch);
                }
                // alloc: an error path.
                let why = format!("{}: {error}", self.command.program.display());
                return Err(io::Error::new(error.kind(), why));
            }
        };
        let (code, signal) = match libc::WIFSIGNALED(status) {
            true => (None, Some(libc::WTERMSIG(status))),
            false => (Some(libc::WEXITSTATUS(status)), None),
        };
        let max_rss = u64::try_from(rusage.ru_maxrss).unwrap_or(0);
        // Kibibytes on Linux, bytes on Apple's systems.
        let peak_rss = if cfg!(target_os = "macos") { max_rss } else { max_rss * 1024 };
        let peak_memory = peak_rss.max(limited);
        if crate::reaped::observed() {
            let count = |it: libc::c_long| u64::try_from(it).unwrap_or(0);
            crate::reaped::report(&crate::reaped::Reaped {
                pid: u32::try_from(pid).unwrap_or(0),
                program: &self.command.program,
                label,
                spawned: self.spawned,
                executed: self.executed,
                exited: Instant::now(),
                sandbox: self.sandbox,
                user: cpu(rusage.ru_utime),
                system: cpu(rusage.ru_stime),
                peak_rss,
                minor_faults: count(rusage.ru_minflt),
                major_faults: count(rusage.ru_majflt),
                voluntary_switches: count(rusage.ru_nvcsw),
                involuntary_switches: count(rusage.ru_nivcsw),
                code,
                signal,
            });
        }
        let traced = match (&self.command.sandbox, &self.confine, &self.scratch) {
            (Sandbox::On { trace: true, .. }, Some(confine), Some(scratch)) => {
                Some(confine.traced(&self.command, scratch).map_err(|why| {
                    // alloc: an error path.
                    io::Error::other(format!(
                        "{}: its sandbox's record of what it read: {why}",
                        self.command.program.display()
                    ))
                }))
            }
            _ => None,
        };
        if let Some(scratch) = &self.scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        let traced = traced.transpose()?;
        Ok(Exit { code, signal, stdout, stderr, traced, peak_memory })
    }
}

/// A `timeval` of CPU time, as a duration: none for the negative values no kernel gives.
fn cpu(it: libc::timeval) -> Duration {
    let secs = u64::try_from(it.tv_sec).unwrap_or(0);
    let micros = u32::try_from(it.tv_usec).unwrap_or(0);
    Duration::from_secs(secs) + Duration::from_micros(u64::from(micros))
}

/// Reaps `pid`, waiting until it has exited: its status and what it used. The one reap of a
/// child [`start`] made: it leaves [`unreaped`]'s list here, reaped, or found reaped already
/// (`ECHILD`, which names the second reap as the error it is).
pub(crate) fn reap(pid: libc::pid_t) -> io::Result<(libc::c_int, libc::rusage)> {
    // On Linux a `wait4(WNOHANG)`, and where the child still runs, its exit waited for on the
    // thread's wait ring (`IORING_OP_WAITID`, leaving it unreaped) before that reap.
    #[cfg(target_os = "linux")]
    let reaped = {
        let clock = crate::wait::Monotonic::new();
        match crate::wait::reap("a child's exit, to reap it", pid, None, &clock) {
            Ok(crate::wait::Waited::Ready(reaped)) => Ok(reaped),
            Ok(crate::wait::Waited::Deadline) => {
                unreachable!("a reap with no deadline ended at one")
            }
            Err(error) => Err(io::Error::from(error)),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let reaped = loop {
        match crate::raw::wait4(pid) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            reaped => break reaped,
        }
    };
    match &reaped {
        Ok(_) => unlist(pid),
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            unlist(pid);
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                // alloc: an error path.
                format!(
                    "reaping process {pid}: it is no child of this process, or was reaped already ({error})"
                ),
            ));
        }
        Err(_) => {}
    }
    reaped
}

/// Waits until `pid` has exited without reaping it, so its pid is not another's until it is.
pub(crate) fn wait_exited(pid: libc::pid_t) -> io::Result<()> {
    // On Linux `IORING_OP_WAITID` on the thread's wait ring.
    #[cfg(target_os = "linux")]
    {
        let clock = crate::wait::Monotonic::new();
        // wakes: the kernel, at the child's exit (`waitid`): the pid is this process's child, which exits or is killed
        match crate::wait::exited("a child's exit", pid, None, &clock) {
            Ok(_) => Ok(()),
            Err(error) => Err(io::Error::from(error)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    loop {
        match crate::raw::wait_exited(pid) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            waited => return waited,
        }
    }
}

/// Signals `pid`, which has not been reaped, so no one else's.
pub(crate) fn kill(pid: libc::pid_t) {
    crate::raw::kill(pid);
}

/// What one read of a pipe takes at most.
const CHUNK: usize = 16 << 10;

/// Writes `input` to `stdin` and reads `stdout` and `stderr` until both end, on this thread in
/// one poll loop: what a driver that blocks does in place of a thread per pipe. Returns what
/// each said. A process that ends without reading all of its input, or a pipe that fails, is
/// not an error of the driver's: what was read is what it said, and its end says how it went.
/// Fails only where the loop cannot wait, and then the process is the caller's to kill.
pub(crate) fn communicate(
    stdin: Option<OwnedFd>,
    input: &[u8],
    stdout: OwnedFd,
    stderr: OwnedFd,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let mut stdin = stdin.filter(|_| !input.is_empty());
    if let Some(fd) = &stdin {
        crate::raw::set_nonblocking(fd.as_fd())?;
    }
    let mut written = 0;
    let mut outputs = [Some(stdout), Some(stderr)];
    // alloc: none until the process writes; what it writes is the exit's payload.
    let mut said: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut chunk = [0u8; CHUNK];
    loop {
        // Which pipe each pollfd is: the input, or an output by its index.
        let mut fds = [libc::pollfd { fd: -1, events: 0, revents: 0 }; 3];
        let mut which = [None; 3];
        let mut n = 0;
        if let Some(fd) = &stdin {
            fds[n] = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
            n += 1;
        }
        for (at, fd) in outputs.iter().enumerate() {
            if let Some(fd) = fd {
                fds[n] = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                which[n] = Some(at);
                n += 1;
            }
        }
        if n == 0 {
            let [stdout, stderr] = said;
            return Ok((stdout, stderr));
        }
        match crate::raw::poll(&mut fds[..n], -1) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        for (polled, which) in fds[..n].iter().zip(which) {
            if polled.revents == 0 {
                continue;
            }
            match which {
                None => {
                    // Listed this round, so still open: only this arm closes it.
                    let Some(fd) = &stdin else { continue };
                    let broken = match crate::raw::write(fd.as_fd(), &input[written..]) {
                        Ok(wrote) => {
                            written += wrote;
                            false
                        }
                        Err(error) => !matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ),
                    };
                    // Done, or the process is not reading (EPIPE and the rest): its input ends.
                    if written == input.len() || broken || polled.revents & libc::POLLOUT == 0 {
                        stdin = None;
                    }
                }
                Some(at) => {
                    let Some(fd) = &outputs[at] else { continue };
                    match crate::raw::read(fd.as_fd(), &mut chunk) {
                        Ok(0) => outputs[at] = None,
                        Ok(read) => said[at].extend_from_slice(&chunk[..read]),
                        Err(error) => match error.kind() {
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => {}
                            _ => outputs[at] = None,
                        },
                    }
                }
            }
        }
    }
}
