//! The one native [`Port`] of [`Level::Inputs`](super::Level::Inputs): a child started through
//! bstd's [`Spawner`] (blaze-core's one linked start), its pipes read by deadline, its group
//! killed through blaze-sys. What differs per OS is only how the child is confined at its
//! start ([`Confiner`]) and whether its use can be read while it runs:
//!
//! - macOS ([`super::seatbelt`]): the program run by `/usr/bin/sandbox-exec -p <profile>`;
//!   CPU and footprint metered by `proc_pid_rusage`.
//! - Linux ([`super::landlock`]): blaze-core's start enters the prepared confinement in the
//!   child ([`Spawner::start_confined`]); CPU and address space are held by the kernel's limits
//!   (`RLIMIT_CPU`, `RLIMIT_AS`), so nothing is metered (`meter` gives no reading, and the
//!   machine asks no second time).

use core::time::Duration;

use blaze_core::wait::Deadline;
use blaze_own::Alloc;

use super::{Capture, Event, Fault, HermeticSpec, Port, StartView, Stream, Usage};
use crate::{
    clock::Clock,
    cx::Cx,
    procs::{Exit, Input, OutPipe, Output, ParentDeath, Read, Run, RunSpec, Spawner, Started, Streams, Waited},
};

/// How long one pipe is read before the other is looked at.
const SLICE: Duration = Duration::from_millis(2);

/// `SIGKILL`.
const SIGKILL: i32 = 9;

/// One OS's confined start, and its meter.
pub(super) trait Confiner {
    /// The child started confined by the plan of `start` (never without it: a start with no
    /// plan is [`Fault::StartUnconfined`]), its streams as `streams`, through `spawner`.
    fn start(
        &mut self,
        spawner: &Spawner,
        cx: &Cx,
        start: &StartView<'_>,
        streams: Streams,
        alloc: Alloc<'_>,
    ) -> Result<Started<Run>, Fault>;

    /// The child's use now, where the OS reads it while it runs.
    fn meter(&mut self, pid: i32) -> Result<Option<Usage>, Fault>;
}

/// The child's streams as the spec captures them: a pipe for what is kept, `/dev/null` else;
/// never an inherited one.
pub(super) fn streams_of(spec: &HermeticSpec<'_>) -> Streams {
    let output = |capture| match capture {
        Capture::Null => Output::Null,
        Capture::Keep { .. } => Output::Pipe,
    };
    let streams = spec.streams();
    Streams { stdin: Input::Null, stdout: output(streams.stdout), stderr: output(streams.stderr) }
}

/// The run spec every backend starts with: its own group (one kill reaches what it starts) and
/// the strongest end at this process's death.
pub(super) fn run_spec<'r>(program: &'r str, args: &'r [&'r str], start: &'r StartView<'r>, streams: Streams) -> RunSpec<'r> {
    RunSpec {
        program,
        arg0: None,
        args,
        env: start.env,
        dir: start.spec.cwd(),
        streams,
        own_group: true,
        parent_death: ParentDeath::STRONGEST,
    }
}

pub(super) struct NativePort<'s, 'a, C: Confiner> {
    pub(super) spawner: &'s Spawner,
    pub(super) clock: &'s Clock,
    pub(super) cx: &'s Cx,
    pub(super) alloc: Alloc<'a>,
    pub(super) confiner: C,
    run: Option<Run>,
    /// The child's pid, its group's id: kept after the reap, for the group's kill.
    group: Option<i32>,
    /// The program's end, held until both pipes are closed.
    ended: Option<Exit>,
    stdout: Option<OutPipe>,
    stderr: Option<OutPipe>,
    buf: [u8; 4096],
    pending: Option<Pending>,
}

#[derive(Clone, Copy)]
enum Pending {
    Bytes { stream: Stream, len: usize },
    Usage(Usage),
    Exited(Exit),
}

impl<'s, 'a, C: Confiner> NativePort<'s, 'a, C> {
    pub(super) fn new(spawner: &'s Spawner, clock: &'s Clock, cx: &'s Cx, alloc: Alloc<'a>, confiner: C) -> NativePort<'s, 'a, C> {
        NativePort { spawner, clock, cx, alloc, confiner, run: None, group: None, ended: None, stdout: None, stderr: None, buf: [0; 4096], pending: None }
    }

    fn pid(&self) -> Result<i32, Fault> {
        self.group.ok_or(Fault::Machine { state: super::State::Running, event: "no child" })
    }

    /// `SIGKILL` to the child's group (its pid: `own_group`). A group already gone (`ESRCH`:
    /// every member ended) is the end the kill asks for; any other answer is named.
    fn kill_group(&self) -> Result<(), Fault> {
        let group = self.pid()?;
        #[cfg(target_os = "macos")]
        let killed = blaze_sys::seatbelt::kill_group(group, SIGKILL);
        #[cfg(target_os = "linux")]
        let killed = blaze_sys::linux::kill_group(group, SIGKILL as u32);
        match killed {
            Ok(()) | Err(blaze_sys::Errno::Srch) => Ok(()),
            // On macOS, `killpg` returns `EPERM` when the child group has already exited/reaped.
            #[cfg(target_os = "macos")]
            Err(blaze_sys::Errno::Perm) => Ok(()),
            Err(errno) => Err(Fault::Host { call: "killpg (the child's group)", code: errno.raw() }),
        }
    }

    /// One read of `stream`'s pipe until `until`: bytes, or the pipe closed (dropped), or nothing.
    fn read_one(&mut self, stream: Stream, until: Deadline) -> Result<bool, Fault> {
        let pipe = match stream {
            Stream::Stdout => &self.stdout,
            Stream::Stderr => &self.stderr,
        };
        let Some(pipe) = pipe else { return Ok(false) };
        match pipe.read(&mut self.buf, until).map_err(Fault::Run)? {
            Read::Bytes(len) => {
                self.pending = Some(Pending::Bytes { stream, len });
                Ok(true)
            }
            Read::Closed => {
                match stream {
                    Stream::Stdout => self.stdout = None,
                    Stream::Stderr => self.stderr = None,
                }
                Ok(false)
            }
            Read::Deadline => Ok(false),
        }
    }
}

impl<C: Confiner> Port for NativePort<'_, '_, C> {
    fn now(&self) -> Duration {
        self.clock.now().since_start()
    }

    fn start(&mut self, start: &StartView<'_>) -> Result<(), Fault> {
        let streams = streams_of(start.spec);
        let started = self.confiner.start(self.spawner, self.cx, start, streams, self.alloc)?;
        self.group = Some(started.child.id().0 as i32);
        self.run = Some(started.child);
        self.stdout = started.pipes.stdout;
        self.stderr = started.pipes.stderr;
        Ok(())
    }

    /// Reads the pipes and reaps the child. When the program has ended, what it left running in
    /// its group is killed (a background process holding a pipe would otherwise keep the run
    /// open to its deadline); the end is reported once both pipes are closed.
    fn wait(&mut self, until: Duration) -> Result<(), Fault> {
        loop {
            let now = self.now();
            let slice = Deadline::at(until.min(now.saturating_add(SLICE)));
            if self.read_one(Stream::Stdout, slice)? || self.read_one(Stream::Stderr, slice)? {
                return Ok(());
            }
            let piped = self.stdout.is_some() || self.stderr.is_some();
            if let Some(run) = self.run.take() {
                // While a pipe is open, a look (the deadline now); after, a wait to `until`.
                let by = if piped { Deadline::at(now) } else { Deadline::at(until.max(now)) };
                match run.wait(by).map_err(Fault::Run)? {
                    Waited::Ended(exit) => {
                        self.ended = Some(exit);
                        self.kill_group()?;
                    }
                    Waited::Running(run) => self.run = Some(run),
                }
            }
            if !piped && let Some(exit) = self.ended.take() {
                self.pending = Some(Pending::Exited(exit));
                return Ok(());
            }
            if self.now() >= until || (!piped && self.run.is_some()) {
                return Ok(());
            }
        }
    }

    fn meter(&mut self) -> Result<(), Fault> {
        // Reaped (its end held for the pipes): nothing left of it to read; its use was in the
        // last reading before it ended.
        if self.run.is_none() && self.ended.is_some() {
            return Ok(());
        }
        let pid = self.pid()?;
        if let Some(usage) = self.confiner.meter(pid)? {
            self.pending = Some(Pending::Usage(usage));
        }
        Ok(())
    }

    fn kill(&mut self) -> Result<(), Fault> {
        self.kill_group()
    }

    fn next_event(&mut self) -> Option<Event<'_>> {
        match self.pending.take()? {
            Pending::Bytes { stream, len } => Some(Event::Bytes { stream, bytes: &self.buf[..len] }),
            Pending::Usage(usage) => Some(Event::Usage(usage)),
            Pending::Exited(exit) => Some(Event::Exited(exit)),
        }
    }
}
