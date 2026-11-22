//! The hermetic run as a sans-IO machine (D170 C5): states prepared, started, running,
//! collecting, ended; events the child's end, a pipe's bytes, the meter, the deadline (as the
//! time of any event, or [`Event::Tick`]), and the read report. The machine decides the stop
//! (deadline, budget, a stream's cap), what is read back and what is kept; its driver
//! ([`super::drive`]) only performs the effects it asks for.
//!
//! A stopped run (deadline, budget, stream) and a run ended by a signal have their output
//! folder cleared ([`Effect::DiscardOutputs`]) before the machine ends: nothing partial is
//! left for a later run or a record to take.

#![blaze::sans_io]

use core::time::Duration;

use blaze_core::wait::Deadline;
use blaze_own::{AVec, Alloc, planted::Planted};
use blaze_roots::{Digest, Limits, ReadList, ReadListOpts, RelPath, RootId, RootsPlant, Seen};

use super::{
    Budget, Capture, Fault, HermeticPlant, HermeticRun, HermeticSpec, Over, Outputs, Reads, ReportSpec, Stop,
};
use crate::procs::Exit;

/// How often a running child's use is metered against its budget.
pub const METER_EVERY: Duration = Duration::from_millis(10);

/// How long a killed group has to be reaped before the run fails naming it (rule 18).
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// `SIGXCPU`: the kernel's end of a child past `RLIMIT_CPU`'s soft limit (Linux, macOS).
const SIGXCPU: i32 = 24;

/// Where the machine is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Made, not yet asked to start.
    Prepared,
    /// Its start asked of the port, not yet confirmed.
    Started,
    /// The child runs.
    Running,
    /// The child ended; its reads and outputs are being collected, or its output cleared.
    Collecting,
    /// Done: [`HermeticMachine::finish`] gives the result.
    Ended,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Report,
    Outputs,
    Discard,
}

/// One of the child's output streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// What the child has used, as metered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Usage {
    pub cpu: Duration,
    pub memory: u64,
    pub steps: Option<u64>,
}

/// What happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'e> {
    /// The port started the child.
    Started,
    /// Bytes of one stream.
    Bytes { stream: Stream, bytes: &'e [u8] },
    /// The meter's reading.
    Usage(Usage),
    /// The child ended (and was reaped).
    Exited(Exit),
    /// A read: from the report, or a package root's file.
    Read { root: RootId, path: RelPath<'e>, seen: Seen },
    /// A path the report names that is under no mount.
    ReadUnmounted(blaze_roots::Unmounted),
    /// The report, or the package roots' listing, is whole.
    ReportEnd,
    /// One file of the output root.
    Output { path: RelPath<'e>, digest: Digest, len: u64 },
    /// The output listing is whole.
    OutputsEnd,
    /// The output root was cleared.
    Discarded,
    /// Time passed (the port's wait returned with nothing): the deadline and the meter.
    Tick,
}

impl Event<'_> {
    fn name(&self) -> &'static str {
        match self {
            Event::Started => "started",
            Event::Bytes { .. } => "bytes",
            Event::Usage(_) => "usage",
            Event::Exited(_) => "exited",
            Event::Read { .. } => "read",
            Event::ReadUnmounted(_) => "read unmounted",
            Event::ReportEnd => "report end",
            Event::Output { .. } => "output",
            Event::OutputsEnd => "outputs end",
            Event::Discarded => "discarded",
            Event::Tick => "tick",
        }
    }
}

/// What the driver is asked to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Start the child confined, with [`HermeticMachine::env`].
    Start,
    /// Read the child's use ([`Event::Usage`]).
    Meter,
    /// Kill the child's group.
    Kill,
    /// Read the program's report ([`Event::Read`]s, then [`Event::ReportEnd`]).
    ReadReport,
    /// List every package root's files as the reads.
    ReadWholeRoots,
    /// List the output root.
    ListOutputs,
    /// Clear the output root.
    DiscardOutputs,
}

/// The fields of a [`HermeticMachine`].
pub struct MachineOpts<'a, P> {
    pub spec: &'a HermeticSpec<'a>,
    pub alloc: Alloc<'a>,
    pub plant: P,
}

/// The run's machine.
pub struct HermeticMachine<'a, P> {
    state: State,
    phase: Phase,
    spec: &'a HermeticSpec<'a>,
    alloc: Alloc<'a>,
    plant: P,
    started_at: Duration,
    next_meter: Duration,
    effect: Option<Effect>,
    killed: bool,
    kill_by: Duration,
    stop: Option<Stop>,
    exit: Option<Exit>,
    /// Whether the reads are the package roots' (no report, or a crash).
    whole_roots: bool,
    env_text: AVec<'a, u8>,
    env_rows: AVec<'a, (u32, u32, u32, u32)>,
    stdout: AVec<'a, u8>,
    stderr: AVec<'a, u8>,
    reads: Option<ReadList<'a>>,
    checked: Option<Reads<'a>>,
    outputs: Outputs<'a>,
    fault: Option<Fault>,
}

impl<'a, P: Planted<HermeticPlant>> HermeticMachine<'a, P> {
    pub fn new(opts: MachineOpts<'a, P>) -> HermeticMachine<'a, P> {
        let alloc = opts.alloc;
        HermeticMachine {
            state: State::Prepared,
            phase: Phase::Report,
            spec: opts.spec,
            alloc,
            plant: opts.plant,
            started_at: Duration::ZERO,
            next_meter: Duration::ZERO,
            effect: None,
            killed: false,
            kill_by: Duration::MAX,
            stop: None,
            exit: None,
            whole_roots: matches!(opts.spec.report(), ReportSpec::WholeRoots),
            env_text: AVec::new_in(alloc),
            env_rows: AVec::new_in(alloc),
            stdout: AVec::new_in(alloc),
            stderr: AVec::new_in(alloc),
            reads: None,
            checked: None,
            outputs: Outputs::new(alloc),
            fault: None,
        }
    }

    pub fn spec(&self) -> &'a HermeticSpec<'a> {
        self.spec
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The next effect, taken: each is asked once.
    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effect.take()
    }

    /// When the machine next wants a [`Event::Tick`]: while running, the sooner of the
    /// deadline and the meter; otherwise none (it waits on an event the driver brings).
    pub fn next_deadline(&self) -> Option<Duration> {
        match self.state {
            State::Running if !self.killed => Some(self.spec.deadline().get().min(self.next_meter)),
            // Killed: the end comes from the port's wait, by the kill's grace at the latest.
            State::Running => Some(self.kill_by),
            _ => None,
        }
    }

    /// Whether a stopped run's output is left (the planted bug only).
    pub fn keeps_partial(&self) -> bool {
        self.plant.has(HermeticPlant::PartialKept)
    }

    /// Whether the start is asked without its confinement (the planted bug only).
    pub fn drops_confinement(&self) -> bool {
        self.plant.has(HermeticPlant::ConfineDropped)
    }

    /// The child's environment, as the start must give it: exactly the spec's pairs.
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.env_rows.iter().map(|&(k, kl, v, vl)| {
            const PUSHED: &str = "blaze-std hermetic machine: the env text was pushed from `&str`s";
            let key = core::str::from_utf8(&self.env_text[k as usize..(k + kl) as usize]).expect(PUSHED);
            let value = core::str::from_utf8(&self.env_text[v as usize..(v + vl) as usize]).expect(PUSHED);
            (key, value)
        })
    }

    fn push_env(&mut self, key: &str, value: &str) -> Result<(), Fault> {
        let k = self.env_text.len() as u32;
        self.env_text.try_extend_from_slice(key.as_bytes()).map_err(|_| Fault::OutOfMemory)?;
        let v = self.env_text.len() as u32;
        self.env_text.try_extend_from_slice(value.as_bytes()).map_err(|_| Fault::OutOfMemory)?;
        self.env_rows.try_push((k, key.len() as u32, v, value.len() as u32)).map_err(|_| Fault::OutOfMemory)?;
        Ok(())
    }

    /// Prepared to started: the environment is built and the start asked.
    pub fn start(&mut self, now: Duration) -> Result<(), Fault> {
        if self.state != State::Prepared {
            return self.end(Fault::Machine { state: self.state, event: "start" });
        }
        let result = self.build_env(now);
        if let Err(fault) = result {
            return self.end(fault);
        }
        self.started_at = now;
        self.state = State::Started;
        self.effect = Some(Effect::Start);
        Ok(())
    }

    fn build_env(&mut self, now: Duration) -> Result<(), Fault> {
        let mut digits = [0u8; 20];
        for &(key, value) in self.spec.env() {
            if key == "SOURCE_DATE_EPOCH" && self.plant.has(HermeticPlant::ClockLeaks) {
                let text = decimal(now.as_secs(), &mut digits);
                self.push_env(key, text)?;
                continue;
            }
            self.push_env(key, value)?;
        }
        if self.plant.has(HermeticPlant::EnvInherited) {
            self.push_env("PATH", "/usr/bin:/bin")?;
        }
        Ok(())
    }

    /// One event at `now`.
    pub fn step(&mut self, now: Duration, event: Event<'_>) -> Result<(), Fault> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        match self.step_inner(now, event) {
            Ok(()) => Ok(()),
            Err(fault) => self.end(fault),
        }
    }

    fn end(&mut self, fault: Fault) -> Result<(), Fault> {
        self.state = State::Ended;
        self.effect = None;
        self.fault = Some(fault.clone());
        Err(fault)
    }

    fn wrong(&self, event: &Event<'_>) -> Fault {
        Fault::Machine { state: self.state, event: event.name() }
    }

    fn step_inner(&mut self, now: Duration, event: Event<'_>) -> Result<(), Fault> {
        match (self.state, event) {
            (State::Started, Event::Started) => {
                self.state = State::Running;
                self.next_meter = now.saturating_add(METER_EVERY);
                self.timers(now)
            }
            (State::Running, Event::Bytes { stream, bytes }) => {
                let streams = self.spec.streams();
                let (buffer, capture) = match stream {
                    Stream::Stdout => (&mut self.stdout, streams.stdout),
                    Stream::Stderr => (&mut self.stderr, streams.stderr),
                };
                let mut over = None;
                if let Capture::Keep { most } = capture {
                    if buffer.len() + bytes.len() > most as usize {
                        over = Some(Stop::Stream { stream, most });
                    } else {
                        buffer.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory)?;
                    }
                }
                if let Some(stop) = over {
                    self.stop_with(now, stop);
                }
                self.timers(now)
            }
            (State::Running, Event::Usage(usage)) => {
                let budget: Budget = self.spec.budget();
                let over = if usage.cpu > budget.cpu {
                    Some(Over::Cpu { used: usage.cpu, most: budget.cpu })
                } else if usage.memory > budget.memory {
                    Some(Over::Memory { used: usage.memory, most: budget.memory })
                } else {
                    match (usage.steps, budget.steps) {
                        (Some(used), Some(most)) if used > most => Some(Over::Steps { used, most }),
                        _ => None,
                    }
                };
                if let Some(over) = over {
                    self.stop_with(now, Stop::Budget(over));
                }
                self.next_meter = now.saturating_add(METER_EVERY);
                self.timers(now)
            }
            (State::Running, Event::Tick) => self.timers(now),
            (State::Running, Event::Exited(exit)) => {
                if let (Exit::Signaled(SIGXCPU), None) = (exit, self.stop) {
                    // The kernel's CPU cap ended it (Linux's RLIMIT_CPU): a budget stop.
                    let most = self.spec.budget().cpu;
                    self.stop = Some(Stop::Budget(Over::Cpu { used: most, most }));
                }
                self.exit = Some(exit);
                self.state = State::Collecting;
                if self.stop.is_some() || matches!(exit, Exit::Signaled(_)) {
                    self.phase = Phase::Discard;
                    self.effect = Some(Effect::DiscardOutputs);
                } else {
                    self.begin_reads()?;
                }
                Ok(())
            }
            (State::Collecting, Event::Discarded) if self.phase == Phase::Discard => {
                if let Some(stop) = self.stop {
                    return Err(Fault::Stopped(stop));
                }
                // A crash: its report is not whole; its reads are the package roots'.
                self.outputs.clear();
                self.whole_roots = true;
                self.begin_reads()
            }
            (State::Collecting, Event::Read { root, path, seen }) if self.phase == Phase::Report => {
                let list = self.reads.as_mut().ok_or(Fault::Machine { state: State::Collecting, event: "read before the list" })?;
                list.push(root, path, seen).map_err(Fault::ReadListFull)
            }
            (State::Collecting, Event::ReadUnmounted(why)) if self.phase == Phase::Report => {
                if self.plant.has(HermeticPlant::ReportUnchecked) {
                    return Ok(());
                }
                Err(Fault::ReadUnmounted(why))
            }
            (State::Collecting, Event::ReportEnd) if self.phase == Phase::Report => {
                let list = self.reads.take().ok_or(Fault::Machine { state: State::Collecting, event: "report end twice" })?;
                let list = if self.plant.has(HermeticPlant::ReadDropped) { drop_last(list, self.alloc)? } else { list };
                let checked = if self.plant.has(HermeticPlant::ReportUnchecked) {
                    list.under_planted(&self.spec.roots(), RootsPlant::GlobsIgnored)
                } else {
                    list.under(&self.spec.roots())
                };
                let checked = checked.map_err(|refused| Fault::ReadOutside(refused.why()))?;
                self.checked = Some(if self.whole_roots { Reads::WholeRoots(checked) } else { Reads::Exact(checked) });
                if matches!(self.exit, Some(Exit::Signaled(_))) {
                    // A crash: its output was cleared; nothing to list.
                    self.state = State::Ended;
                    return Ok(());
                }
                self.phase = Phase::Outputs;
                self.effect = Some(Effect::ListOutputs);
                Ok(())
            }
            (State::Collecting, Event::Output { path, digest, len }) if self.phase == Phase::Outputs => {
                self.outputs.push(path, digest, len, Limits::SHIPPED.most_entries)
            }
            (State::Collecting, Event::OutputsEnd) if self.phase == Phase::Outputs => {
                self.outputs.sort();
                self.state = State::Ended;
                Ok(())
            }
            (_, event) => Err(self.wrong(&event)),
        }
    }

    fn begin_reads(&mut self) -> Result<(), Fault> {
        self.phase = Phase::Report;
        self.reads = Some(ReadList::new(ReadListOpts { limits: Limits::SHIPPED, alloc: self.alloc }));
        self.effect = Some(if self.whole_roots { Effect::ReadWholeRoots } else { Effect::ReadReport });
        Ok(())
    }

    fn stop_with(&mut self, now: Duration, stop: Stop) {
        if self.stop.is_none() {
            self.stop = Some(stop);
        }
        if !self.killed {
            self.killed = true;
            self.kill_by = now.saturating_add(KILL_GRACE);
            self.effect = Some(Effect::Kill);
        }
    }

    fn timers(&mut self, now: Duration) -> Result<(), Fault> {
        if self.killed {
            if now >= self.kill_by {
                return Err(Fault::Host { call: "kill: the child's group was not reaped within the kill's grace", code: 0 });
            }
            return Ok(());
        }
        if self.spec.deadline().passed(now) {
            self.stop_with(now, Stop::Deadline);
            return Ok(());
        }
        if now >= self.next_meter && self.effect.is_none() {
            self.effect = Some(Effect::Meter);
            // The next meter is set by its reading; until then, no second ask.
            self.next_meter = Deadline::NEVER.get();
        }
        Ok(())
    }

    /// The result, once [`State::Ended`].
    pub fn finish(self) -> Result<HermeticRun<'a>, Fault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match (self.state, self.exit, self.checked) {
            (State::Ended, Some(exit), Some(reads)) => {
                Ok(HermeticRun { exit, outputs: self.outputs, stdout: self.stdout, stderr: self.stderr, reads })
            }
            (state, _, _) => Err(Fault::Machine { state, event: "finish" }),
        }
    }
}

/// `list` without its last entry (the planted bug [`HermeticPlant::ReadDropped`]).
fn drop_last<'a>(list: ReadList<'a>, alloc: Alloc<'a>) -> Result<ReadList<'a>, Fault> {
    let mut kept = ReadList::new(ReadListOpts { limits: list.limits(), alloc });
    let count = list.len();
    for (at, entry) in list.entries().enumerate() {
        if at + 1 < count {
            kept.push(entry.root, entry.path, entry.seen).map_err(Fault::ReadListFull)?;
        }
    }
    Ok(kept)
}

/// `value` in decimal, in `into`.
pub(super) fn decimal(mut value: u64, into: &mut [u8; 20]) -> &str {
    let mut at = into.len();
    loop {
        at -= 1;
        into[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    core::str::from_utf8(&into[at..]).expect("blaze-std hermetic machine: decimal digits are ASCII")
}
