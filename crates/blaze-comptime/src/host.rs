//! THE HOST'S MACHINE: blaze's side of one compile-time run, sans-IO (C5). It reads the guest's
//! frames, answers each call from the [`Grants`] alone, asks its driver for the facts it lacks
//! (a file's bytes, a token-server reply, a routed command's result) as effects, and keeps what
//! keys the record: the read list (blaze-roots), the facts (variables and commands answered)
//! and the refusals, as findings.
//!
//! States: `Starting` (no `Hello` yet), `Running` (waiting for the guest's next call),
//! `Answering` (a call in flight, waiting for the driver's fact), and four ends: `Finished`,
//! `StoppedByFuel`, `StoppedByDeadline`, `Crashed`. Only [`Host::finish`] of a `Finished` run
//! hands out output, and only with the read list checked under the roots ([`Checked`]): a stop
//! or a crash leaves no output, half-written or whole, since the output buffers are reachable
//! through [`Done`] alone.
//!
//! A refusal is never an end: the guest gets the error the real system gives a process that
//! lacks the thing ([`SysError::NotFound`] for a path outside its grants, so a crate that
//! probes goes on as it would), and a [`Finding`] names the path or name and the [`Grant`] that
//! would allow it.
//!
//! The driver's part: open beneath the root, never following a link out of it (blaze-roots
//! lib.rs); deliver a directory's names in any order (the machine sorts and filters them); read
//! the child's retired instructions from the kernel and deliver them as [`Event::Retired`]
//! (where it cannot, its CPU time as [`Event::Cpu`]); kill the child on
//! [`Effect::Kill`].

#![blaze::sans_io]

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{AVec, Alloc, Vec};
use blaze_roots::{Checked, Digest, Full, ReadList, ReadListOpts, Refused, RelPath, RootId, RootKind, Seen, listing_digest};

use crate::call::{Answer, Call, CallKind, ClockKind, FileKind, Stream, SysError, Want, Words};
use crate::grant::{EnvValue, Grants, Spawns};
use crate::plant::HostPlant;
use crate::wire::{WireError, WireLimits, read_call, write_answer};

/// A time on the request clock, in nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Nanos(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostState {
    Starting,
    Running,
    Answering,
    Finished,
    StoppedByFuel,
    StoppedByDeadline,
    Crashed,
}

impl HostState {
    pub fn ended(self) -> bool {
        matches!(self, HostState::Finished | HostState::StoppedByFuel | HostState::StoppedByDeadline | HostState::Crashed)
    }
}

/// Why a run crashed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Crash {
    /// A frame that is not this protocol's.
    Wire(WireError),
    /// The guest speaks another protocol version.
    Protocol { protocol: u32 },
    /// A call its run's kind or its moment does not allow (no `Hello` first, a second `Hello`,
    /// a build script's `Return`, a bridge call from a build script, a bridge `track_*` sent as
    /// a plain bridge call).
    Order { got: CallKind },
    /// The process ended without `Return` or `Exit` (a panic that aborts, a signal).
    Gone,
    /// A proc macro exited instead of returning.
    Exited { code: i32 },
    /// The read list reached a cap of its limits.
    Reads(Full),
    /// The output passed its cap.
    Output { most: u32 },
    /// The run ended on a listing it could not digest (a host bug: names are sorted first).
    Listing,
}

/// The grant a refused call lacked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Grant {
    /// A mount covering the path (it is outside every root the run was given).
    Mount,
    /// The path cannot be named by any grant (not UTF-8, a NUL, too long, `..` past `/`).
    Unaddressable,
    /// A glob of the package root `root` declaring the path.
    Glob { root: RootId },
    /// The output root: a write anywhere else.
    Write,
    /// The variable in the run's environment.
    Env,
    /// Commands ([`Spawns::CMode`]).
    Spawn,
    /// A second thread.
    Thread,
    /// The network.
    Network,
    /// No grant allows it: a C function the system layer refuses by name ([`Call::Denied`]).
    Never,
}

/// One refusal, as read back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Finding<'h> {
    pub call: CallKind,
    /// The path or name as the guest spelled it.
    pub name: &'h [u8],
    pub grant: Grant,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FactKind {
    /// A variable answered: its name, and its value's digest or `None` for unset.
    Env,
    /// A command answered: its request (program, arguments, environment, directory, each
    /// prefixed with its u32 LE byte length so embedded NULs cannot collide), and the digest
    /// of its status and streams.
    Spawn,
}

/// One fact the run's answers depended on, besides the files of the read list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fact<'h> {
    pub kind: FactKind,
    pub name: &'h [u8],
    pub value: Option<Digest>,
}

/// What the driver found for an [`Effect::Look`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found<'e> {
    File { bytes: &'e [u8] },
    /// A directory and its names (any order; `.` and `..` left out).
    Dir { names: Words<'e> },
    Absent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'e> {
    /// One frame's body from the guest ([`crate::wire::split_frame`] cut it).
    Frame(&'e [u8]),
    Looked(Found<'e>),
    Bridged { reply: &'e [u8] },
    Spawned { status: i32, stdout: &'e [u8], stderr: &'e [u8] },
    /// The child's CPU time so far, as the kernel accounts it (where retired instructions
    /// cannot be metered).
    Cpu { used_ns: u64 },
    /// The child's retired instructions so far (`ri_instructions`, a `perf_event_open` count).
    Retired { instructions: u64 },
    /// Time passed; nothing else happened.
    Tick,
    /// The process is gone.
    Gone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Frame,
    Looked,
    Bridged,
    Spawned,
    Cpu,
    Retired,
    Tick,
    Gone,
}

impl Event<'_> {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Frame(_) => EventKind::Frame,
            Event::Looked(_) => EventKind::Looked,
            Event::Bridged { .. } => EventKind::Bridged,
            Event::Spawned { .. } => EventKind::Spawned,
            Event::Cpu { .. } => EventKind::Cpu,
            Event::Retired { .. } => EventKind::Retired,
            Event::Tick => EventKind::Tick,
            Event::Gone => EventKind::Gone,
        }
    }
}

/// What the driver must do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect<'h> {
    /// Write this frame to the guest.
    Send { frame: &'h [u8] },
    /// Look up `path` (relative, canonical UTF-8) under `root`, then deliver [`Event::Looked`].
    Look { root: RootId, path: &'h [u8] },
    /// Ask the token server, then deliver [`Event::Bridged`].
    Bridge { method: u8, args: &'h [u8] },
    /// Route the command (the C mode), then deliver [`Event::Spawned`].
    Spawn { program: &'h [u8], args: Words<'h>, env: Words<'h>, cwd: &'h [u8] },
    /// Kill the process: the run is over.
    Kill,
}

/// The driver broke the protocol (never the guest: its faults are a [`Crash`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostError {
    /// An event the state does not wait for.
    Unexpected { state: HostState, event: EventKind },
    /// An event delivered before the last effect was taken.
    EffectPending,
}

/// The output root's contents, as the run left them: files and directories by relative path.
pub struct OutFile<'a> {
    pub path: AVec<'a, u8>,
    pub data: AVec<'a, u8>,
    pub dir: bool,
}

/// A finished run.
pub struct Done<'a> {
    /// A proc macro's output stream (bridge wire), or a build script's standard output.
    pub output: AVec<'a, u8>,
    /// The last bytes of standard error.
    pub log: AVec<'a, u8>,
    /// A build script's exit code.
    pub exit: Option<i32>,
    /// The output root, sorted by path.
    pub files: AVec<'a, OutFile<'a>>,
    /// The files read, checked under the roots: what keys the record with [`Done::facts`].
    pub reads: Checked<'a>,
    /// Sorted, each once.
    pub facts: Facts<'a>,
    pub findings: Findings<'a>,
}

/// A run that did not finish.
#[derive(Debug)]
pub enum Ended<'a> {
    /// `finish` before an end.
    NotEnded { state: HostState },
    Fuel { instructions: u64, cpu_ns: u64, calls: u32 },
    Deadline { after_ns: u64 },
    Crashed(Crash),
    /// The read list is not under the roots (a path seen two ways: it moved during the run).
    Reads(Refused<'a>),
}

#[derive(Clone, Copy)]
struct Row<K> {
    kind: K,
    at: u32,
    len: u32,
    value: Option<Digest>,
}

/// Facts, as rows over one text buffer.
pub struct Facts<'a> {
    text: AVec<'a, u8>,
    rows: AVec<'a, Row<FactKind>>,
}

impl<'a> Facts<'a> {
    fn new(alloc: Alloc<'a>) -> Facts<'a> {
        Facts { text: Vec::new_in(alloc), rows: Vec::new_in(alloc) }
    }

    fn push(&mut self, kind: FactKind, name: &[u8], value: Option<Digest>, limits: blaze_roots::Limits) -> Result<(), Full> {
        let text = self.text.as_slice();
        let pos = match self.rows.binary_search_by(|row| {
            let row_name = &text[wide(row.at)..wide(row.at) + wide(row.len)];
            (row.kind, row_name, row.value).cmp(&(kind, name, value))
        }) {
            Ok(_) => return Ok(()),
            Err(pos) => pos,
        };
        if self.rows.len() >= wide(limits.most_entries) {
            return Err(Full::Entries { most: limits.most_entries });
        }
        let Ok(at) = u32::try_from(self.text.len()) else {
            return Err(Full::Text { most: limits.most_text });
        };
        let Ok(len) = u32::try_from(name.len()) else {
            return Err(Full::Text { most: limits.most_text });
        };
        if at.checked_add(len).is_none_or(|end| end > limits.most_text) {
            return Err(Full::Text { most: limits.most_text });
        }
        self.text.extend_from_slice(name);
        self.rows.insert_shift(pos, Row { kind, at, len, value });
        Ok(())
    }

    fn name(&self, row: &Row<FactKind>) -> &[u8] {
        &self.text[wide(row.at)..wide(row.at) + wide(row.len)]
    }

    pub fn iter(&self) -> impl Iterator<Item = Fact<'_>> + '_ {
        self.rows.iter().map(|row| Fact { kind: row.kind, name: self.name(row), value: row.value })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The facts' digest, for the record's key with the read list's.
    pub fn digest(&self) -> Digest {
        let mut h = blaze_blake3::Hasher::new_derive_key("blaze-comptime 2026-11-20 facts");
        for fact in self.iter() {
            h.update(&[match fact.kind {
                FactKind::Env => 0,
                FactKind::Spawn => 1,
            }]);
            h.update(&len64(fact.name.len()).to_le_bytes());
            h.update(fact.name);
            match fact.value {
                None => h.update(&[0]),
                Some(digest) => h.update(&[1]).update(&digest.0),
            };
        }
        Digest(h.finalize())
    }
}

/// Refusals, in the order they happened.
pub struct Findings<'a> {
    text: AVec<'a, u8>,
    rows: AVec<'a, Row<(CallKind, Grant)>>,
}

impl<'a> Findings<'a> {
    fn new(alloc: Alloc<'a>) -> Findings<'a> {
        Findings { text: Vec::new_in(alloc), rows: Vec::new_in(alloc) }
    }

    fn push(&mut self, call: CallKind, name: &[u8], grant: Grant, limits: blaze_roots::Limits) {
        if self.rows.len() >= wide(limits.most_entries) {
            return;
        }
        let Ok(at) = u32::try_from(self.text.len()) else {
            return;
        };
        let Ok(len) = u32::try_from(name.len()) else {
            return;
        };
        if at.checked_add(len).is_none_or(|end| end > limits.most_text) {
            return;
        }
        self.text.extend_from_slice(name);
        self.rows.push(Row { kind: (call, grant), at, len, value: None });
    }

    pub fn iter(&self) -> impl Iterator<Item = Finding<'_>> + '_ {
        self.rows.iter().map(|row| Finding {
            call: row.kind.0,
            name: &self.text[wide(row.at)..wide(row.at) + wide(row.len)],
            grant: row.kind.1,
        })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// What `next_effect` hands out next.
#[derive(Clone, Copy)]
enum Out {
    Send,
    Look { root: RootId },
    Bridge { method: u8 },
    /// Ranges in `scratch`: program, arguments, environment, directory.
    Spawn { parts: [(u32, u32); 4] },
    Kill,
}

/// The call in flight.
#[derive(Clone, Copy)]
enum Pending {
    /// `listed`: whether the lookup goes on the read list (false only under a plant).
    Look { root: RootId, want: Want, listed: bool },
    Track { root: RootId, listed: bool },
    Bridge,
    Spawn { parts: [(u32, u32); 4] },
}

pub struct HostOpts<'g, 'a> {
    pub grants: Grants<'g>,
    pub wire: WireLimits = WireLimits::SHIPPED,
    pub alloc: Alloc<'a>,
}

pub struct Host<'g, 'a, P: Planted<HostPlant> = NoPlant> {
    grants: Grants<'g>,
    wire: WireLimits,
    alloc: Alloc<'a>,
    plant: P,
    state: HostState,
    crash: Option<Crash>,
    start: Option<Nanos>,
    now: Nanos,
    calls: u32,
    cpu_ns: u64,
    instructions: u64,
    /// The answer frame being sent.
    send: AVec<'a, u8>,
    out: Option<Out>,
    /// The relative path, bridge arguments or command of the call in flight.
    scratch: AVec<'a, u8>,
    /// Built answers: listings, word lists, random bytes, a second path.
    spare: AVec<'a, u8>,
    pending: Option<Pending>,
    reads: ReadList<'a>,
    facts: Facts<'a>,
    findings: Findings<'a>,
    output: AVec<'a, u8>,
    log: AVec<'a, u8>,
    exit: Option<i32>,
    /// Output files and directories, kept sorted bytewise by `path`.
    files: AVec<'a, OutFile<'a>>,
    /// Running sum of `file.data.len() as u64` across `files`.
    out_bytes: u64,
    random_at: u64,
}

fn wide(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl<'g, 'a> Host<'g, 'a, NoPlant> {
    pub fn new(opts: HostOpts<'g, 'a>) -> Host<'g, 'a, NoPlant> {
        Host::new_planted(opts, NoPlant)
    }
}

impl<'g, 'a, P: Planted<HostPlant>> Host<'g, 'a, P> {
    pub fn new_planted(opts: HostOpts<'g, 'a>, plant: P) -> Host<'g, 'a, P> {
        let alloc = opts.alloc;
        Host {
            grants: opts.grants,
            wire: opts.wire,
            alloc,
            plant,
            state: HostState::Starting,
            crash: None,
            start: None,
            now: Nanos(0),
            calls: 0,
            cpu_ns: 0,
            instructions: 0,
            send: Vec::new_in(alloc),
            out: None,
            scratch: Vec::new_in(alloc),
            spare: Vec::new_in(alloc),
            pending: None,
            reads: ReadList::new(ReadListOpts { limits: opts.grants.limits, alloc }),
            facts: Facts::new(alloc),
            findings: Findings::new(alloc),
            output: Vec::new_in(alloc),
            log: Vec::new_in(alloc),
            exit: None,
            files: Vec::new_in(alloc),
            out_bytes: 0,
            random_at: 0,
        }
    }

    pub fn state(&self) -> HostState {
        self.state
    }

    /// When the run's deadline falls, while it runs.
    pub fn next_deadline(&self) -> Option<Nanos> {
        match (self.state.ended(), self.start) {
            (false, Some(start)) => Some(Nanos(start.0.saturating_add(self.grants.deadline.within_ns))),
            _ => None,
        }
    }

    pub fn findings(&self) -> impl Iterator<Item = Finding<'_>> + '_ {
        self.findings.iter()
    }

    pub fn next_effect(&mut self) -> Option<Effect<'_>> {
        let out = self.out.take()?;
        let slice = |(at, len): (u32, u32)| &self.scratch[wide(at)..wide(at) + wide(len)];
        Some(match out {
            Out::Send => Effect::Send { frame: &self.send },
            Out::Look { root } => Effect::Look { root, path: &self.scratch },
            Out::Bridge { method } => Effect::Bridge { method, args: &self.scratch },
            Out::Spawn { parts } => Effect::Spawn {
                program: slice(parts[0]),
                args: Words(slice(parts[1])),
                env: Words(slice(parts[2])),
                cwd: slice(parts[3]),
            },
            Out::Kill => Effect::Kill,
        })
    }

    pub fn step(&mut self, now: Nanos, event: Event<'_>) -> Result<(), HostError> {
        if self.state.ended() {
            // Facts about a process already over: nothing is left to do with them.
            return match event {
                Event::Gone | Event::Tick | Event::Cpu { .. } | Event::Retired { .. } => Ok(()),
                _ => Err(HostError::Unexpected { state: self.state, event: event.kind() }),
            };
        }
        if self.out.is_some() {
            return Err(HostError::EffectPending);
        }
        let start = *self.start.get_or_insert(now);
        self.now = now;
        if now.0.saturating_sub(start.0) >= self.grants.deadline.within_ns {
            self.stop(HostState::StoppedByDeadline);
            return Ok(());
        }
        match (self.state, event) {
            (_, Event::Tick) => Ok(()),
            (_, Event::Gone) => {
                self.crashed(Crash::Gone);
                Ok(())
            }
            (_, Event::Cpu { used_ns }) => {
                self.cpu_ns = used_ns;
                if used_ns > self.grants.fuel.cpu_ns {
                    self.stop(HostState::StoppedByFuel);
                }
                Ok(())
            }
            (_, Event::Retired { instructions }) => {
                self.instructions = instructions;
                if instructions > self.grants.fuel.instructions {
                    self.stop(HostState::StoppedByFuel);
                }
                Ok(())
            }
            (HostState::Starting | HostState::Running, Event::Frame(body)) => {
                self.frame(body);
                Ok(())
            }
            (HostState::Answering, Event::Looked(found)) if matches!(self.pending, Some(Pending::Look { .. } | Pending::Track { .. })) => {
                self.looked(found);
                Ok(())
            }
            (HostState::Answering, Event::Bridged { reply }) if matches!(self.pending, Some(Pending::Bridge)) => {
                self.pending = None;
                self.reply(Answer::Bridge { reply });
                Ok(())
            }
            (HostState::Answering, Event::Spawned { status, stdout, stderr }) if matches!(self.pending, Some(Pending::Spawn { .. })) => {
                self.spawned(status, stdout, stderr);
                Ok(())
            }
            (state, event) => Err(HostError::Unexpected { state, event: event.kind() }),
        }
    }

    /// The run's result: output only from a finished run whose read list is under its roots.
    pub fn finish(self) -> Result<Done<'a>, Ended<'a>> {
        match self.state {
            HostState::Finished => {}
            HostState::StoppedByFuel => {
                return Err(Ended::Fuel { instructions: self.instructions, cpu_ns: self.cpu_ns, calls: self.calls });
            }
            HostState::StoppedByDeadline => {
                let after_ns = self.now.0.saturating_sub(self.start.map_or(0, |s| s.0));
                return Err(Ended::Deadline { after_ns });
            }
            HostState::Crashed => return Err(Ended::Crashed(self.crash.unwrap_or(Crash::Gone))),
            state => return Err(Ended::NotEnded { state }),
        }
        let reads = self.reads.under(&self.grants.roots).map_err(Ended::Reads)?;
        debug_assert!(self.files.windows(2).all(|w| w[0].path.as_slice() < w[1].path.as_slice()));
        Ok(Done { output: self.output, log: self.log, exit: self.exit, files: self.files, reads, facts: self.facts, findings: self.findings })
    }

    fn stop(&mut self, state: HostState) {
        self.state = state;
        self.pending = None;
        self.out = Some(Out::Kill);
    }

    fn crashed(&mut self, crash: Crash) {
        self.crash = Some(crash);
        self.stop(HostState::Crashed);
    }

    fn finished(&mut self) {
        self.state = HostState::Finished;
        self.pending = None;
        self.out = None;
    }

    /// Queues `answer` as the next frame and waits for the next call.
    fn reply(&mut self, answer: Answer<'_>) {
        put(&mut self.send, self.wire, &answer);
        self.out = Some(Out::Send);
        self.state = HostState::Running;
    }

    fn refuse(&mut self, call: CallKind, name: &[u8], grant: Grant, error: SysError) {
        self.findings.push(call, name, grant, self.grants.limits);
        self.reply(Answer::Error(error));
    }

    fn frame(&mut self, body: &[u8]) {
        self.calls = self.calls.saturating_add(1);
        if self.calls > self.grants.fuel.calls {
            self.stop(HostState::StoppedByFuel);
            return;
        }
        let call = match read_call(body, self.wire) {
            Ok(call) => call,
            Err(error) => return self.crashed(Crash::Wire(error)),
        };
        if self.state == HostState::Starting {
            return match call {
                Call::Hello { protocol } if protocol == crate::PROTOCOL => self.reply(Answer::Hello { protocol }),
                Call::Hello { protocol } => self.crashed(Crash::Protocol { protocol }),
                other => self.crashed(Crash::Order { got: other.kind() }),
            };
        }
        let grants = self.grants;
        let kind = call.kind();
        let is_macro = grants.is_macro();
        match call {
            Call::Hello { .. } => self.crashed(Crash::Order { got: kind }),
            Call::Look { path, want } => self.look(kind, path, Some(want)),
            Call::TrackPath { path } if is_macro => self.look(kind, path, None),
            Call::Env { name } => match self.env(kind, name) {
                Ok(value) => self.reply(Answer::Value { value }),
                Err(full) => self.crashed(Crash::Reads(full)),
            },
            Call::TrackEnv { name, .. } if is_macro => {
                // The value the run depends on is the grant's, whatever the macro claims.
                match self.env(kind, name) {
                    Ok(_) => self.reply(Answer::Done),
                    Err(full) => self.crashed(Crash::Reads(full)),
                }
            }
            Call::EnvAll => {
                self.spare.clear();
                for grant in grants.env {
                    let digest = match grant.value {
                        EnvValue::Set(value) => {
                            self.spare.extend_from_slice(grant.name);
                            self.spare.push(b'=');
                            self.spare.extend_from_slice(value);
                            self.spare.push(0);
                            Some(Digest(blaze_blake3::hash(value)))
                        }
                        EnvValue::Unset => None,
                    };
                    if let Err(full) = self.facts.push(FactKind::Env, grant.name, digest, grants.limits) {
                        return self.crashed(Crash::Reads(full));
                    }
                }
                put(&mut self.send, self.wire, &Answer::Words { words: Words(&self.spare) });
                self.sent();
            }
            Call::Args => {
                self.spare.clear();
                for arg in grants.args {
                    self.spare.extend_from_slice(arg);
                    self.spare.push(0);
                }
                put(&mut self.send, self.wire, &Answer::Words { words: Words(&self.spare) });
                self.sent();
            }
            Call::Cwd => self.reply(Answer::Value { value: grants.mounts.cwd() }),
            Call::Clock { which } => {
                let ns = if self.plant.has(HostPlant::HostClock) {
                    self.now.0
                } else {
                    match which {
                        ClockKind::Wall => grants.clock.wall_ns,
                        ClockKind::Monotonic => grants.clock.monotonic_ns,
                    }
                };
                self.reply(Answer::Clock { ns });
            }
            Call::Random { len } => {
                // The stream: BLAKE3's extendable output keyed by the seed, over no input.
                self.spare.clear();
                self.spare.resize(wide(len), 0);
                let mut xof = blaze_blake3::Hasher::new_keyed(&grants.seed.0).finalize_xof();
                xof.set_position(self.random_at);
                xof.fill(&mut self.spare);
                self.random_at = self.random_at.saturating_add(u64::from(len));
                put(&mut self.send, self.wire, &Answer::Random { bytes: &self.spare });
                self.sent();
            }
            Call::Write { stream, bytes } => self.write(stream, bytes),
            Call::WriteFile { path, at, bytes } => self.out_op(kind, path, OutOp::Write { at, bytes }),
            Call::SetLen { path, len } => self.out_op(kind, path, OutOp::SetLen { len }),
            Call::CreateDir { path } => self.out_op(kind, path, OutOp::CreateDir),
            Call::Remove { path } => self.out_op(kind, path, OutOp::Remove),
            Call::Rename { from, to } => self.out_op(kind, from, OutOp::Rename { to }),
            // The clock is fixed, so a sleep ends at once and nothing is waited for.
            Call::Sleep { .. } => self.reply(Answer::Done),
            Call::Thread => self.refuse(kind, b"pthread_create", Grant::Thread, SysError::Unsupported),
            Call::Socket => self.refuse(kind, b"socket", Grant::Network, SysError::Unsupported),
            Call::Denied { name } => self.refuse(kind, name, Grant::Never, SysError::Unsupported),
            Call::Spawn { program, args, env, cwd } => match grants.spawns {
                Spawns::None => self.refuse(kind, program, Grant::Spawn, SysError::NotFound),
                Spawns::CMode => {
                    self.scratch.clear();
                    let mut parts = [(0, 0); 4];
                    for (at, part) in [program, args.as_bytes(), env.as_bytes(), cwd].into_iter().enumerate() {
                        parts[at] = (len32(self.scratch.len()), len32(part.len()));
                        self.scratch.extend_from_slice(part);
                    }
                    self.pending = Some(Pending::Spawn { parts });
                    self.out = Some(Out::Spawn { parts });
                    self.state = HostState::Answering;
                }
            },
            // The two bridge methods that reach the world must come as `TrackEnv`/`TrackPath`.
            Call::Bridge { method, args } if is_macro && method > 1 => {
                self.scratch.clear();
                self.scratch.extend_from_slice(args);
                self.pending = Some(Pending::Bridge);
                self.out = Some(Out::Bridge { method });
                self.state = HostState::Answering;
            }
            Call::Return { output } if is_macro => {
                if output.len() > wide(grants.output.most_out) {
                    return self.crashed(Crash::Output { most: grants.output.most_out });
                }
                self.output.clear();
                self.output.extend_from_slice(output);
                self.finished();
            }
            Call::Exit { code } if is_macro => self.crashed(Crash::Exited { code }),
            Call::Exit { code } => {
                self.exit = Some(code);
                self.finished();
            }
            Call::Bridge { .. } | Call::TrackEnv { .. } | Call::TrackPath { .. } | Call::Return { .. } => {
                self.crashed(Crash::Order { got: kind })
            }
        }
    }

    fn sent(&mut self) {
        self.out = Some(Out::Send);
        self.state = HostState::Running;
    }

    /// The variable `name`'s value as the run sees it, recorded as a fact (or a refusal).
    fn env(&mut self, call: CallKind, name: &[u8]) -> Result<Option<&'g [u8]>, Full> {
        let limits = self.grants.limits;
        match self.grants.env_of(name) {
            Some(EnvValue::Set(value)) => {
                self.facts.push(FactKind::Env, name, Some(Digest(blaze_blake3::hash(value))), limits)?;
                Ok(Some(value))
            }
            Some(EnvValue::Unset) => {
                self.facts.push(FactKind::Env, name, None, limits)?;
                Ok(None)
            }
            // Not granted: the real system's answer (unset), the finding printed, and the
            // asked-and-absent fact kept, so granting it later reruns the run (owner, 2026-11-20).
            None => {
                self.findings.push(call, name, Grant::Env, limits);
                self.facts.push(FactKind::Env, name, None, limits)?;
                Ok(None)
            }
        }
    }

    /// A lookup (`want`) or a tracked path (`None`).
    fn look(&mut self, call: CallKind, path: &[u8], want: Option<Want>) {
        let root = match self.grants.mounts.resolve(path, self.grants.limits.most_path, &mut self.scratch) {
            Ok(root) => root,
            Err(blaze_roots::Unmounted::Path(_)) => return self.refuse(call, path, Grant::Unaddressable, SysError::NotFound),
            Err(_) => return self.refuse(call, path, Grant::Mount, SysError::NotFound),
        };
        let Some(r) = self.grants.roots.get(root) else {
            return self.refuse(call, path, Grant::Mount, SysError::NotFound);
        };
        if matches!(r.kind, RootKind::Output) {
            return match want {
                Some(want) => self.look_output(want),
                // A macro has no output root; a tracked output is not an input.
                None => self.reply(Answer::Done),
            };
        }
        let admitted = match RelPath::from_bytes(&self.scratch, u32::MAX) {
            Ok(rel) => r.admits_file(rel) || r.admits_dir(rel),
            Err(_) => return self.refuse(call, path, Grant::Unaddressable, SysError::NotFound),
        };
        if !admitted {
            if !self.plant.has(HostPlant::OutsideAnswered) {
                return self.refuse(call, path, Grant::Glob { root }, SysError::NotFound);
            }
            // The plant: the refusal is recorded, and the path is looked up and answered anyway
            // (kept off the read list, so only the answer shows the bug).
            self.findings.push(call, path, Grant::Glob { root }, self.grants.limits);
        }
        self.pending = Some(match want {
            Some(want) => Pending::Look { root, want, listed: admitted },
            None => Pending::Track { root, listed: admitted },
        });
        self.out = Some(Out::Look { root });
        self.state = HostState::Answering;
    }

    fn looked(&mut self, found: Found<'_>) {
        let (root, want, listed) = match self.pending.take() {
            Some(Pending::Look { root, want, listed }) => (root, Some(want), listed),
            Some(Pending::Track { root, listed }) => (root, None, listed),
            _ => return self.crashed(Crash::Order { got: CallKind::Look }),
        };
        let Some(r) = self.grants.roots.get(root) else {
            return self.crashed(Crash::Order { got: CallKind::Look });
        };
        let ignore_globs = self.plant.has(HostPlant::OutsideAnswered);
        let Ok(rel) = RelPath::from_bytes(&self.scratch, u32::MAX) else {
            return self.crashed(Crash::Order { got: CallKind::Look });
        };
        // What the globs do not declare is absent to the run: a file at a path only a directory
        // glob reaches, a directory no glob reaches beneath.
        let found = match found {
            Found::File { .. } if !ignore_globs && !r.admits_file(rel) => Found::Absent,
            Found::Dir { .. } if !ignore_globs && !r.admits_dir(rel) => Found::Absent,
            found => found,
        };
        let seen = match found {
            Found::File { bytes } => Ok(Seen::File { digest: Digest(blaze_blake3::hash(bytes)), len: len64(bytes.len()) }),
            Found::Absent => Ok(Seen::Absent),
            // A directory stat'ed or opened as a file is seen to exist, not listed: adding a
            // file under it moves nothing the run saw.
            Found::Dir { .. } if matches!(want, Some(Want::Stat | Want::Read)) => Ok(Seen::Dir),
            Found::Dir { names } => {
                // The names the root admits, sorted and each once, into `spare` as words.
                let mut kept: AVec<'_, &str> = Vec::new_in(self.alloc);
                let mut child: AVec<'_, u8> = Vec::new_in(self.alloc);
                for name in names.iter() {
                    let Ok(name) = core::str::from_utf8(name) else { continue };
                    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                        continue;
                    }
                    child.clear();
                    child.extend_from_slice(rel.as_str().as_bytes());
                    if !rel.is_root() {
                        child.push(b'/');
                    }
                    child.extend_from_slice(name.as_bytes());
                    let admitted = match RelPath::from_bytes(&child, u32::MAX) {
                        Ok(path) => ignore_globs || r.admits_file(path) || r.admits_dir(path),
                        Err(_) => false,
                    };
                    if admitted {
                        kept.push(name);
                    }
                }
                kept.sort_unstable();
                kept.dedup();
                self.spare.clear();
                for name in kept.iter() {
                    self.spare.extend_from_slice(name.as_bytes());
                    self.spare.push(0);
                }
                listing_digest(kept.iter().copied()).map(|digest| Seen::Listing { digest }).map_err(|_| Crash::Listing)
            }
        };
        let pushed = match seen {
            Ok(_) if !listed || self.plant.has(HostPlant::ReadUnlisted) => Ok(()),
            Ok(seen) => self.reads.push(root, rel, seen).map_err(Crash::Reads),
            Err(crash) => Err(crash),
        };
        if let Err(crash) = pushed {
            return self.crashed(crash);
        }
        let answer = match (want, found) {
            (None, _) => Answer::Done,
            (Some(Want::Read), Found::File { bytes }) => Answer::File { bytes },
            (Some(Want::Stat), Found::File { bytes }) => Answer::Stat { kind: FileKind::File, len: len64(bytes.len()) },
            (Some(Want::List), Found::File { .. }) => Answer::Error(SysError::NotADir),
            (Some(Want::Read), Found::Dir { .. }) => Answer::Error(SysError::IsADir),
            (Some(Want::Stat), Found::Dir { .. }) => Answer::Stat { kind: FileKind::Dir, len: 0 },
            (Some(Want::List), Found::Dir { .. }) => Answer::Listing { names: Words(&self.spare) },
            (Some(_), Found::Absent) => Answer::Error(SysError::NotFound),
        };
        put(&mut self.send, self.wire, &answer);
        self.sent();
    }

    fn spawned(&mut self, status: i32, stdout: &[u8], stderr: &[u8]) {
        let Some(Pending::Spawn { parts }) = self.pending.take() else {
            return self.crashed(Crash::Order { got: CallKind::Spawn });
        };
        // Each of the four request parts is prefixed with its u32 LE byte length so embedded
        // NULs in `args`/`env` cannot collide across part boundaries.
        self.spare.clear();
        for (at, len) in parts {
            self.spare.extend_from_slice(&len.to_le_bytes());
            self.spare.extend_from_slice(&self.scratch[wide(at)..wide(at) + wide(len)]);
        }
        let mut h = blaze_blake3::Hasher::new_derive_key("blaze-comptime 2026-11-20 command answer");
        h.update(&status.to_le_bytes()).update(&len64(stdout.len()).to_le_bytes()).update(stdout).update(stderr);
        let digest = Digest(h.finalize());
        if let Err(full) = self.facts.push(FactKind::Spawn, &self.spare, Some(digest), self.grants.limits) {
            return self.crashed(Crash::Reads(full));
        }
        self.reply(Answer::Spawned { status, stdout, stderr });
    }

    fn write(&mut self, stream: Stream, bytes: &[u8]) {
        if self.grants.is_macro() {
            // A macro's prints go nowhere, as on the wasm path (sandbox.rs `fd_write`).
            return self.reply(Answer::Done);
        }
        match stream {
            Stream::Out => {
                if self.output.len().saturating_add(bytes.len()) > wide(self.grants.output.most_out) {
                    return self.crashed(Crash::Output { most: self.grants.output.most_out });
                }
                self.output.extend_from_slice(bytes);
            }
            Stream::Err => {
                let most = wide(self.grants.output.most_log);
                if most == 0 {
                    self.log.clear();
                } else if bytes.len() >= most {
                    self.log.clear();
                    self.log.extend_from_slice(&bytes[bytes.len() - most..]);
                } else {
                    let need = self.log.len().saturating_add(bytes.len());
                    if need > most {
                        self.log.drain(..need - most);
                    }
                    self.log.extend_from_slice(bytes);
                }
            }
        }
        self.reply(Answer::Done);
    }

    /// The output root's entry at `path`, binary-searched in the sorted `files` table.
    fn find(&self, path: &[u8]) -> Option<usize> {
        self.files.binary_search_by(|file| file.path.as_slice().cmp(path)).ok()
    }

    fn parent_is_dir(&self, path: &[u8]) -> bool {
        match path.iter().rposition(|b| *b == b'/') {
            None => true,
            Some(cut) => self.find(&path[..cut]).is_some_and(|at| self.files[at].dir),
        }
    }

    fn look_output(&mut self, want: Want) {
        let path = self.scratch.as_slice();
        let entry = if path.is_empty() { Some(None) } else { self.find(path).map(Some) };
        let answer = match (entry, want) {
            (None, _) => Answer::Error(SysError::NotFound),
            (Some(Some(at)), Want::Read) if !self.files[at].dir => Answer::File { bytes: &self.files[at].data },
            (Some(Some(at)), Want::Stat) if !self.files[at].dir => {
                Answer::Stat { kind: FileKind::File, len: len64(self.files[at].data.len()) }
            }
            (Some(Some(at)), Want::List) if !self.files[at].dir => Answer::Error(SysError::NotADir),
            (Some(_), Want::Read) => Answer::Error(SysError::IsADir),
            (Some(_), Want::Stat) => Answer::Stat { kind: FileKind::Dir, len: 0 },
            (Some(_), Want::List) => {
                // `self.files` is kept sorted bytewise by `path`, so direct children arrive in
                // sorted order already without allocating or sorting a scratch list.
                self.spare.clear();
                if path.is_empty() {
                    for file in self.files.iter() {
                        let name = file.path.as_slice();
                        if !name.is_empty() && !name.contains(&b'/') {
                            self.spare.extend_from_slice(name);
                            self.spare.push(0);
                        }
                    }
                } else {
                    let start = self.files.partition_point(|file| cmp_with_slash(file.path.as_slice(), path).is_lt());
                    for file in &self.files[start..] {
                        let Some(rest) = file.path.strip_prefix(path).and_then(|rest| rest.strip_prefix(b"/")) else {
                            break;
                        };
                        if !rest.is_empty() && !rest.contains(&b'/') {
                            self.spare.extend_from_slice(rest);
                            self.spare.push(0);
                        }
                    }
                }
                Answer::Listing { names: Words(&self.spare) }
            }
        };
        put(&mut self.send, self.wire, &answer);
        self.sent();
    }

    fn out_op(&mut self, call: CallKind, path: &[u8], op: OutOp<'_>) {
        let output = self.grants.roots.output();
        match self.grants.mounts.resolve(path, self.grants.limits.most_path, &mut self.scratch) {
            Ok(root) if Some(root) == output => {}
            Ok(_) | Err(blaze_roots::Unmounted::Outside | blaze_roots::Unmounted::NoCwd) => {
                return self.refuse(call, path, Grant::Write, SysError::ReadOnly);
            }
            Err(blaze_roots::Unmounted::Path(_)) => return self.refuse(call, path, Grant::Unaddressable, SysError::NotFound),
        }
        let caps = self.grants.output;
        let at = self.find(&self.scratch);
        let result = match op {
            OutOp::Write { at: offset, bytes } => {
                let Some(end) = offset.checked_add(len64(bytes.len())) else {
                    return self.reply(Answer::Error(SysError::NoSpace));
                };
                let Ok(end_usize) = usize::try_from(end) else {
                    return self.reply(Answer::Error(SysError::NoSpace));
                };
                self.prepare_write(at, end, true, caps).map(|at| {
                    let data = &mut self.files[at].data;
                    if data.len() < end_usize {
                        let growth = len64(end_usize - data.len());
                        data.resize(end_usize, 0);
                        self.out_bytes += growth;
                    }
                    let from = usize::try_from(offset).unwrap_or(usize::MAX);
                    data[from..from + bytes.len()].copy_from_slice(bytes);
                })
            }
            OutOp::SetLen { len } => {
                let Ok(len_usize) = usize::try_from(len) else {
                    return self.reply(Answer::Error(SysError::NoSpace));
                };
                self.prepare_write(at, len, false, caps).map(|at| {
                    let data = &mut self.files[at].data;
                    let cur = len64(data.len());
                    data.resize(len_usize, 0);
                    if len >= cur {
                        self.out_bytes += len - cur;
                    } else {
                        self.out_bytes -= cur - len;
                    }
                })
            }
            OutOp::CreateDir => match at {
                Some(_) => Err(SysError::Exists),
                None if self.scratch.is_empty() => Err(SysError::Exists),
                None => self.create(true, caps).map(|_| ()),
            },
            OutOp::Remove => match at {
                None => Err(SysError::NotFound),
                Some(at) if self.files[at].dir && self.has_children(at) => Err(SysError::NotEmpty),
                Some(at) => {
                    let removed = self.files.remove_shift(at);
                    self.out_bytes -= len64(removed.data.len());
                    Ok(())
                }
            },
            OutOp::Rename { to } => self.rename(at, to),
        };
        match result {
            Ok(()) => self.reply(Answer::Done),
            Err(error) => self.reply(Answer::Error(error)),
        }
    }

    fn has_children(&self, at: usize) -> bool {
        let dir = self.files[at].path.as_slice();
        let start = self.files.partition_point(|file| cmp_with_slash(file.path.as_slice(), dir).is_lt());
        self.files.get(start).is_some_and(|file| file.path.strip_prefix(dir).is_some_and(|rest| rest.first() == Some(&b'/')))
    }

    /// Checks caps and returns the file at `at` (creating it only once every cap passes, so
    /// `NoSpace` never leaks a ghost 0-byte file).
    fn prepare_write(&mut self, at: Option<usize>, target_len: u64, grow_only: bool, caps: crate::grant::OutputCaps) -> Result<usize, SysError> {
        match at {
            Some(at) if self.files[at].dir => Err(SysError::IsADir),
            Some(at) => {
                let cur = len64(self.files[at].data.len());
                let next = if grow_only { target_len.max(cur) } else { target_len };
                let growth = next.saturating_sub(cur);
                if self.out_bytes.checked_add(growth).is_none_or(|total| total > caps.most_file_bytes) {
                    return Err(SysError::NoSpace);
                }
                Ok(at)
            }
            None if self.scratch.is_empty() => Err(SysError::IsADir),
            None => {
                if !self.parent_is_dir(&self.scratch) {
                    return Err(SysError::NotFound);
                }
                if len64(self.files.len()) >= u64::from(caps.most_files) {
                    return Err(SysError::NoSpace);
                }
                if self.out_bytes.checked_add(target_len).is_none_or(|total| total > caps.most_file_bytes) {
                    return Err(SysError::NoSpace);
                }
                self.insert_sorted(false)
            }
        }
    }

    fn create(&mut self, dir: bool, caps: crate::grant::OutputCaps) -> Result<usize, SysError> {
        if !self.parent_is_dir(&self.scratch) {
            return Err(SysError::NotFound);
        }
        if len64(self.files.len()) >= u64::from(caps.most_files) {
            return Err(SysError::NoSpace);
        }
        self.insert_sorted(dir)
    }

    fn insert_sorted(&mut self, dir: bool) -> Result<usize, SysError> {
        let pos = match self.files.binary_search_by(|file| file.path.as_slice().cmp(&self.scratch)) {
            Ok(_) => return Err(SysError::Exists),
            Err(pos) => pos,
        };
        let mut path = Vec::new_in(self.alloc);
        path.extend_from_slice(&self.scratch);
        self.files.insert_shift(pos, OutFile { path, data: Vec::new_in(self.alloc), dir });
        Ok(pos)
    }

    fn rename(&mut self, at: Option<usize>, to: &[u8]) -> Result<(), SysError> {
        let output = self.grants.roots.output();
        match self.grants.mounts.resolve(to, self.grants.limits.most_path, &mut self.spare) {
            Ok(root) if Some(root) == output => {}
            Ok(_) | Err(blaze_roots::Unmounted::Outside | blaze_roots::Unmounted::NoCwd) => {
                self.findings.push(CallKind::Rename, to, Grant::Write, self.grants.limits);
                return Err(SysError::ReadOnly);
            }
            Err(blaze_roots::Unmounted::Path(_)) => {
                self.findings.push(CallKind::Rename, to, Grant::Unaddressable, self.grants.limits);
                return Err(SysError::NotFound);
            }
        }
        let Some(at) = at else { return Err(SysError::NotFound) };
        let mut from = Vec::new_in(self.alloc);
        from.extend_from_slice(&self.scratch);
        // Into itself (a directory moved beneath itself) or onto the root: refused, as `rename(2)`.
        if self.spare.is_empty() || (self.spare.starts_with(from.as_slice()) && self.spare.get(from.len()) == Some(&b'/')) {
            return Err(SysError::Invalid);
        }
        let mut target = Vec::new_in(self.alloc);
        target.extend_from_slice(&self.spare);
        if !self.parent_is_dir(&target) {
            return Err(SysError::NotFound);
        }
        if let Some(existing) = self.find(&target) {
            if existing == at {
                return Ok(());
            }
            if self.files[existing].dir != self.files[at].dir || (self.files[existing].dir && self.has_children(existing)) {
                return Err(SysError::Exists);
            }
            let removed = self.files.remove_shift(existing);
            self.out_bytes -= len64(removed.data.len());
        }
        // The entry and, for a directory, everything beneath it, moved to the new prefix.
        for file in self.files.iter_mut() {
            let rest = match file.path.strip_prefix(from.as_slice()) {
                Some(rest) if rest.is_empty() || rest.first() == Some(&b'/') => rest,
                _ => continue,
            };
            let mut moved = Vec::new_in(self.alloc);
            moved.extend_from_slice(&target);
            moved.extend_from_slice(rest);
            file.path = moved;
        }
        self.files.sort_unstable_by(|a, b| a.path.as_slice().cmp(b.path.as_slice()));
        Ok(())
    }
}

/// Compares `entry` against the virtual prefix `dir + "/"` without allocating.
fn cmp_with_slash(entry: &[u8], dir: &[u8]) -> core::cmp::Ordering {
    let n = entry.len().min(dir.len());
    match entry[..n].cmp(&dir[..n]) {
        core::cmp::Ordering::Equal => {
            if entry.len() <= dir.len() {
                core::cmp::Ordering::Less
            } else {
                entry[dir.len()].cmp(&b'/')
            }
        }
        ord => ord,
    }
}

#[derive(Clone, Copy)]
enum OutOp<'f> {
    Write { at: u64, bytes: &'f [u8] },
    SetLen { len: u64 },
    CreateDir,
    Remove,
    Rename { to: &'f [u8] },
}

/// `answer` into `send` as the one frame there; an answer past the frame cap is the real
/// system's "file too large".
fn put(send: &mut AVec<'_, u8>, wire: WireLimits, answer: &Answer<'_>) {
    send.clear();
    if let Err(WireError::Frame { .. }) = write_answer(answer, send, wire) {
        send.clear();
        // Two bytes of body: always within any frame cap of at least that.
        let _ = write_answer(&Answer::Error(SysError::TooLarge), send, wire);
    }
}
