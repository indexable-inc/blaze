//! Zero-alloc typed linear process requests (`SpawnReq`, `WaitExitReq`, `KillReq`), inert
//! generational [`ChildHandle<'io>`], and deterministic [`SimProcsHandler`].

#![blaze::sans_io]

use core::{fmt, marker::PhantomData, mem::size_of, num::NonZeroU32};

use blaze_machine::{Handler, Perform, Request};
use blaze_own::{Alloc, Brand, Vec};

use super::Signal;

/// Inert 8-byte generational handle to a spawned child process, branded to driver scope `'io`.
///
/// Allowed inside `#![blaze::sans_io]` state structs (`#[blaze::handle]`); carries zero executing
/// authority and must be reaped via [`ChildHandle::wait_exit`] (`#[blaze::linear]`).
#[blaze::handle]
#[blaze::linear]
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct ChildHandle<'io> {
    slot: u32,
    generation: NonZeroU32,
    _brand: Brand<'io>,
}

const _: () = assert!(size_of::<ChildHandle<'_>>() == 8);
const _: () = assert!(size_of::<Option<ChildHandle<'_>>>() == 8);

impl<'io> ChildHandle<'io> {
    #[inline]
    pub(crate) const fn new(slot: u32, generation: NonZeroU32, brand: Brand<'io>) -> Self {
        Self { slot, generation, _brand: brand }
    }

    #[inline]
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    #[inline]
    pub const fn generation(&self) -> NonZeroU32 {
        self.generation
    }

    #[inline]
    pub const fn brand(&self) -> Brand<'io> {
        self._brand
    }

    /// Builds a [`KillReq`] delivering `signal` to this child without consuming the handle
    /// (the child must still be reaped with [`ChildHandle::wait_exit`]).
    #[inline]
    pub const fn kill(&self, signal: Signal) -> KillReq<'io> {
        KillReq {
            slot: self.slot,
            generation: self.generation,
            signal,
            _brand: self._brand,
        }
    }

    /// Consumes this linear child handle into a [`WaitExitReq`] that reaps the child's
    /// [`ExitCode`].
    #[blaze::consumes]
    #[inline]
    pub const fn wait_exit(self) -> WaitExitReq<'io> {
        WaitExitReq { child: self }
    }

    #[blaze::consumes]
    #[inline]
    const fn consume_raw(self) -> (u32, NonZeroU32) {
        let Self { slot, generation, _brand: _ } = self;
        (slot, generation)
    }
}

/// Exit status of a reaped child process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitCode {
    /// Child exited normally with `code`.
    Code(i32),
    /// Child was terminated by `signal`.
    Signaled(Signal),
}

impl ExitCode {
    /// Returns `true` iff the child exited normally with status `0`.
    #[inline]
    pub const fn is_success(self) -> bool {
        matches!(self, ExitCode::Code(0))
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ExitCode::Code(code) => write!(f, "exit code {code}"),
            ExitCode::Signaled(sig) => write!(f, "terminated by signal {sig:?}"),
        }
    }
}

/// Typed refusal returned by process request handlers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcsReqError {
    /// Program name was empty or not registered with the handler.
    NotFound,
    /// Handler reached its bounded child-slot cap (rule 18).
    TableFull { max_children: u32 },
    /// Handle slot is invalid, already reaped, or stale (generation mismatch).
    StaleHandle { slot: u32, generation: NonZeroU32 },
    /// Child has already exited or been signaled to exit.
    AlreadyExited,
    /// Explicit handler allocator refused growth.
    OutOfMemory,
}

impl fmt::Display for ProcsReqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProcsReqError::NotFound => f.write_str("program not found"),
            ProcsReqError::TableFull { max_children } => {
                write!(f, "process handle table full at cap {max_children}")
            }
            ProcsReqError::StaleHandle { slot, generation } => {
                write!(f, "stale child handle (slot={slot}, gen={generation})")
            }
            ProcsReqError::AlreadyExited => f.write_str("child process has already exited"),
            ProcsReqError::OutOfMemory => f.write_str("process handler allocator out of memory"),
        }
    }
}

impl core::error::Error for ProcsReqError {}

/// Typed outcome of a process request.
pub type Outcome<T> = Result<T, ProcsReqError>;

/// Options for [`SpawnReq::new`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct SpawnOpts<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
}

/// Typed linear request to spawn a child process.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct SpawnReq<'io, 'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    _brand: PhantomData<Brand<'io>>,
}

impl<'io, 'a> SpawnReq<'io, 'a> {
    #[inline]
    pub const fn new(opts: SpawnOpts<'a>) -> Self {
        Self {
            program: opts.program,
            args: opts.args,
            env: opts.env,
            _brand: PhantomData,
        }
    }
}

impl<'io, 'a> Request for SpawnReq<'io, 'a> {
    type Answer<'s> = Outcome<ChildHandle<'io>>;
}

/// Typed linear request that consumes [`ChildHandle<'io>`] and waits for its [`ExitCode`].
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct WaitExitReq<'io> {
    pub child: ChildHandle<'io>,
}

impl<'io> WaitExitReq<'io> {
    #[blaze::consumes]
    #[inline]
    pub const fn new(child: ChildHandle<'io>) -> Self {
        Self { child }
    }
}

impl<'io> Request for WaitExitReq<'io> {
    type Answer<'s> = Outcome<ExitCode>;
}

/// Typed linear request to deliver a [`Signal`] to a live child process.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct KillReq<'io> {
    slot: u32,
    generation: NonZeroU32,
    pub signal: Signal,
    _brand: Brand<'io>,
}

impl<'io> KillReq<'io> {
    #[inline]
    pub const fn new(child: &ChildHandle<'io>, signal: Signal) -> Self {
        child.kill(signal)
    }

    #[inline]
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    #[inline]
    pub const fn generation(&self) -> NonZeroU32 {
        self.generation
    }
}

impl<'io> Request for KillReq<'io> {
    type Answer<'s> = Outcome<()>;
}

/// Unified process request union for driving sans-IO [`blaze_machine::Machine`]s.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub enum ProcsReq<'io, 'a> {
    Spawn(SpawnReq<'io, 'a>),
    WaitExit(WaitExitReq<'io>),
    Kill(KillReq<'io>),
}

/// Unified answer matching [`ProcsReq`].
#[derive(PartialEq, Eq, Debug)]
pub enum ProcsAns<'io> {
    Spawned(Outcome<ChildHandle<'io>>),
    Exited(Outcome<ExitCode>),
    Killed(Outcome<()>),
}

impl<'io, 'a> Request for ProcsReq<'io, 'a> {
    type Answer<'s> = ProcsAns<'io>;
}

/// Options for [`SimProcsHandler::new`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct SimProcsOpts<'io, 'a> {
    pub brand: Brand<'io>,
    pub max_children: u32,
    pub alloc: Alloc<'a>,
}

/// Loud leak error returned by [`SimProcsHandler::finish_check`] when spawned children were not
/// reaped before task exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimProcsHandlerError {
    LeakedChildren { live: u32 },
}

impl fmt::Display for SimProcsHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SimProcsHandlerError::LeakedChildren { live } => {
                write!(f, "SimProcsHandler finished with {live} unreaped child handle(s)")
            }
        }
    }
}

impl core::error::Error for SimProcsHandlerError {}

enum SlotState {
    Live { exit: ExitCode, killed_by: Option<Signal> },
    Vacant,
    Retired,
}

struct ChildSlot {
    generation: NonZeroU32,
    state: SlotState,
}

/// Deterministic `#![no_std]` handler for [`SpawnReq`], [`WaitExitReq`], [`KillReq`], and
/// [`ProcsReq`] backed by an explicit caller [`Alloc`] and generational slot table.
pub struct SimProcsHandler<'io, 'a> {
    brand: Brand<'io>,
    max_children: u32,
    scripts: Vec<(&'a str, ExitCode), Alloc<'a>>,
    slots: Vec<ChildSlot, Alloc<'a>>,
    free_list: Vec<u32, Alloc<'a>>,
    signals: Vec<(u32, Signal), Alloc<'a>>,
    live_children: u32,
    spawned_count: u32,
}

impl<'io, 'a> SimProcsHandler<'io, 'a> {
    /// Creates an empty deterministic process handler with bounded slot capacity `max_children`.
    pub fn new(opts: SimProcsOpts<'io, 'a>) -> Self {
        Self {
            brand: opts.brand,
            max_children: opts.max_children,
            scripts: Vec::new_in(opts.alloc),
            slots: Vec::new_in(opts.alloc),
            free_list: Vec::new_in(opts.alloc),
            signals: Vec::new_in(opts.alloc),
            live_children: 0,
            spawned_count: 0,
        }
    }

    /// Registers a scripted `program` outcome; spawning `program` yields a child whose
    /// [`WaitExitReq`] returns `exit` unless terminated by a signal first.
    pub fn register(&mut self, program: &'a str, exit: ExitCode) -> Outcome<()> {
        for (existing, code) in self.scripts.as_mut_slice() {
            if *existing == program {
                *code = exit;
                return Ok(());
            }
        }
        self.scripts.try_push((program, exit)).map_err(|_| ProcsReqError::OutOfMemory)
    }

    /// Number of currently live (unreaped) child handles.
    #[inline]
    pub const fn live_children(&self) -> u32 {
        self.live_children
    }

    /// Total number of children successfully spawned by this handler.
    #[inline]
    pub const fn spawned_count(&self) -> u32 {
        self.spawned_count
    }

    /// Recorded `(slot, signal)` pairs delivered through [`KillReq`].
    #[inline]
    pub fn signals(&self) -> &[(u32, Signal)] {
        self.signals.as_slice()
    }

    /// Verifies that every spawned child handle was cleanly reaped via [`WaitExitReq`].
    #[inline]
    pub const fn finish_check(&self) -> Result<(), SimProcsHandlerError> {
        if self.live_children == 0 {
            Ok(())
        } else {
            Err(SimProcsHandlerError::LeakedChildren { live: self.live_children })
        }
    }

    fn handle_spawn(&mut self, req: SpawnReq<'io, '_>) -> Outcome<ChildHandle<'io>> {
        let SpawnReq { program, args: _, env: _, _brand: _ } = req;
        if program.is_empty() {
            return Err(ProcsReqError::NotFound);
        }
        let mut scripted = None;
        for &(name, code) in self.scripts.as_slice() {
            if name == program {
                scripted = Some(code);
                break;
            }
        }
        let Some(exit) = scripted else {
            return Err(ProcsReqError::NotFound);
        };

        if let Some(slot_idx) = self.free_list.pop() {
            let entry = &mut self.slots[slot_idx as usize];
            entry.state = SlotState::Live { exit, killed_by: None };
            self.live_children = self.live_children.saturating_add(1);
            self.spawned_count = self.spawned_count.saturating_add(1);
            return Ok(ChildHandle::new(slot_idx, entry.generation, self.brand));
        }

        let slot_idx = u32::try_from(self.slots.len())
            .map_err(|_| ProcsReqError::TableFull { max_children: self.max_children })?;
        if slot_idx >= self.max_children {
            return Err(ProcsReqError::TableFull { max_children: self.max_children });
        }
        let generation = NonZeroU32::MIN;
        self.slots
            .try_push(ChildSlot {
                generation,
                state: SlotState::Live { exit, killed_by: None },
            })
            .map_err(|_| ProcsReqError::OutOfMemory)?;
        self.live_children = self.live_children.saturating_add(1);
        self.spawned_count = self.spawned_count.saturating_add(1);
        Ok(ChildHandle::new(slot_idx, generation, self.brand))
    }

    fn handle_kill(&mut self, req: KillReq<'io>) -> Outcome<()> {
        let KillReq { slot, generation, signal, _brand: _ } = req;
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return Err(ProcsReqError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(ProcsReqError::StaleHandle { slot, generation });
        }
        match &mut entry.state {
            SlotState::Live { killed_by, .. } => {
                if killed_by.is_some() {
                    return Err(ProcsReqError::AlreadyExited);
                }
                self.signals.try_push((slot, signal)).map_err(|_| ProcsReqError::OutOfMemory)?;
                if matches!(signal, Signal::Interrupt | Signal::Terminate | Signal::Kill) {
                    *killed_by = Some(signal);
                }
                Ok(())
            }
            SlotState::Vacant | SlotState::Retired => {
                Err(ProcsReqError::StaleHandle { slot, generation })
            }
        }
    }

    fn handle_wait_exit(&mut self, req: WaitExitReq<'io>) -> Outcome<ExitCode> {
        let WaitExitReq { child } = req;
        let (slot, generation) = child.consume_raw();
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return Err(ProcsReqError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(ProcsReqError::StaleHandle { slot, generation });
        }
        let outcome = match entry.state {
            SlotState::Live { exit, killed_by } => match killed_by {
                Some(sig) => ExitCode::Signaled(sig),
                None => exit,
            },
            SlotState::Vacant | SlotState::Retired => {
                return Err(ProcsReqError::StaleHandle { slot, generation });
            }
        };

        match entry.generation.get().checked_add(1).and_then(NonZeroU32::new) {
            Some(next_gen) => {
                entry.generation = next_gen;
                entry.state = SlotState::Vacant;
                let _ = self.free_list.try_push(slot);
            }
            None => {
                entry.state = SlotState::Retired;
            }
        }
        self.live_children = self.live_children.saturating_sub(1);
        Ok(outcome)
    }
}

impl<'io> Handler<SpawnReq<'io, '_>> for SimProcsHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: SpawnReq<'io, '_>,
        deliver: impl FnOnce(Perform<Outcome<ChildHandle<'io>>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_spawn(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<WaitExitReq<'io>> for SimProcsHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: WaitExitReq<'io>,
        deliver: impl FnOnce(Perform<Outcome<ExitCode>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_wait_exit(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<KillReq<'io>> for SimProcsHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: KillReq<'io>,
        deliver: impl FnOnce(Perform<Outcome<()>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_kill(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<ProcsReq<'io, '_>> for SimProcsHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: ProcsReq<'io, '_>,
        deliver: impl FnOnce(Perform<ProcsAns<'io>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = match request {
            ProcsReq::Spawn(req) => ProcsAns::Spawned(self.handle_spawn(req)),
            ProcsReq::WaitExit(req) => ProcsAns::Exited(self.handle_wait_exit(req)),
            ProcsReq::Kill(req) => ProcsAns::Killed(self.handle_kill(req)),
        };
        Ok(deliver(Perform::Answered(ans)))
    }
}

#[cfg(test)]
mod tests {
    use core::{marker::PhantomData, time::Duration};

    use blaze_machine::{CancelAction, DriveOpts, Machine, Stopped, Turn};
    use blaze_own::{Brand, testing::heap, with_brand};

    use super::{
        ExitCode, ProcsAns, ProcsReq, ProcsReqError, SimProcsHandler, SimProcsOpts, SpawnOpts,
        SpawnReq,
    };
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
        procs::Signal,
    };

    enum SpawnAndWaitMachine<'io, 'a> {
        Init { program: &'a str, args: &'a [&'a str], _brand: PhantomData<Brand<'io>> },
        Spawning,
        Waiting,
    }

    impl<'io, 'a> Machine for SpawnAndWaitMachine<'io, 'a> {
        type Req = ProcsReq<'io, 'a>;
        type Out = ExitCode;
        type Err = ProcsReqError;

        fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match self {
                SpawnAndWaitMachine::Init { program, args, _brand: _ } => Turn::Ask {
                    request: ProcsReq::Spawn(SpawnReq::new(SpawnOpts {
                        program,
                        args,
                        env: &[],
                    })),
                    next: SpawnAndWaitMachine::Spawning,
                },
                SpawnAndWaitMachine::Spawning | SpawnAndWaitMachine::Waiting => {
                    unreachable!("SpawnAndWaitMachine::start called on non-Init state")
                }
            }
        }

        fn resume(
            self,
            answer: ProcsAns<'io>,
            _cx: &mut (),
        ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match (self, answer) {
                (SpawnAndWaitMachine::Spawning, ProcsAns::Spawned(Ok(child))) => Turn::Ask {
                    request: ProcsReq::WaitExit(child.wait_exit()),
                    next: SpawnAndWaitMachine::Waiting,
                },
                (SpawnAndWaitMachine::Spawning, ProcsAns::Spawned(Err(err))) => Turn::Fail(err),
                (SpawnAndWaitMachine::Waiting, ProcsAns::Exited(Ok(code))) => Turn::Done(code),
                (SpawnAndWaitMachine::Waiting, ProcsAns::Exited(Err(err))) => Turn::Fail(err),
                _ => unreachable!("typed ProcsAns always matches active state"),
            }
        }

        fn on_cancel(
            self,
            _stopped: Stopped,
            _cx: &mut (),
        ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
            CancelAction::Abandon
        }
    }

    #[test]
    fn cx_run_procs_spawn_and_wait_exit() {
        let h = heap();
        let io = Io::simulated(SimWorld::new(83, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");

        with_brand(|brand| {
            let mut handler = SimProcsHandler::new(SimProcsOpts {
                brand,
                max_children: 4,
                alloc: h.alloc(),
            });
            handler.register("/bin/true", ExitCode::Code(0)).expect("register /bin/true");
            handler.register("/bin/sleep", ExitCode::Code(0)).expect("register /bin/sleep");
            handler.register("/bin/fail", ExitCode::Code(7)).expect("register /bin/fail");

            // 1. Direct Cx::run lifecycle: SpawnReq -> WaitExitReq.
            let child = cx
                .run(
                    "bstd.procs.spawn",
                    &mut handler,
                    SpawnReq::new(SpawnOpts {
                        program: "/bin/true",
                        args: &["--ok"],
                        env: &[],
                    }),
                )
                .expect("handler perform spawn")
                .expect("spawn ok");
            assert_eq!(handler.live_children(), 1);

            let stale_kill = child.kill(Signal::Terminate);
            let exit = cx
                .run("bstd.procs.wait_exit", &mut handler, child.wait_exit())
                .expect("handler perform wait_exit")
                .expect("wait_exit ok");
            assert_eq!(exit, ExitCode::Code(0));
            assert!(exit.is_success());
            assert_eq!(handler.live_children(), 0);

            // Stale handle after reap must fail with StaleHandle.
            let stale_res = cx
                .run("bstd.procs.kill_stale", &mut handler, stale_kill)
                .expect("handler perform stale kill");
            assert!(matches!(stale_res, Err(ProcsReqError::StaleHandle { .. })));

            // 2. SpawnReq -> KillReq(Signal::Kill) -> WaitExitReq yields Signaled(Signal::Kill).
            let sleeper = cx
                .run(
                    "bstd.procs.spawn_sleep",
                    &mut handler,
                    SpawnReq::new(SpawnOpts {
                        program: "/bin/sleep",
                        args: &["60"],
                        env: &[],
                    }),
                )
                .expect("handler perform spawn sleeper")
                .expect("spawn sleeper ok");
            let kill_res = cx
                .run("bstd.procs.kill", &mut handler, sleeper.kill(Signal::Kill))
                .expect("handler perform kill");
            assert_eq!(kill_res, Ok(()));
            let killed_exit = cx
                .run("bstd.procs.wait_killed", &mut handler, sleeper.wait_exit())
                .expect("handler perform wait killed")
                .expect("wait killed ok");
            assert_eq!(killed_exit, ExitCode::Signaled(Signal::Kill));
            assert!(!killed_exit.is_success());

            // 3. Unknown program is refused with NotFound without leaking a slot.
            let missing = cx
                .run(
                    "bstd.procs.spawn_missing",
                    &mut handler,
                    SpawnReq::new(SpawnOpts {
                        program: "/bin/does-not-exist",
                        args: &[],
                        env: &[],
                    }),
                )
                .expect("handler perform missing");
            assert_eq!(missing, Err(ProcsReqError::NotFound));

            // 4. Driving SpawnAndWaitMachine over ProcsReq through Cx::drive_machine.
            let machine = SpawnAndWaitMachine::Init {
                program: "/bin/fail",
                args: &[],
                _brand: PhantomData,
            };
            let driven = cx.drive_machine(
                "bstd.procs.spawn_and_wait_machine",
                &mut handler,
                &mut (),
                machine,
                DriveOpts { max_steps: 8 },
            );
            assert_eq!(driven, Ok(ExitCode::Code(7)));
            assert_eq!(handler.finish_check(), Ok(()));
        });
    }
}
