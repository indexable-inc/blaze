//! Zero-alloc typed linear directory watch requests (`WatchDirReq`, `NextChangeReq`,
//! `UnwatchReq`), inert generational [`WatchHandle<'io>`], and deterministic [`SimWatchHandler`].

#![blaze::sans_io]

use core::{fmt, marker::PhantomData, mem::size_of, num::NonZeroU32};

use blaze_core::FileName;
use blaze_machine::{Handler, Perform, Request};
use blaze_own::{Alloc, Brand, Vec};

/// Inert 8-byte generational handle to an active directory watch, branded to driver scope `'io`.
///
/// Allowed inside `#![blaze::sans_io]` state structs (`#[blaze::handle]`); carries zero executing
/// authority and must be released via [`WatchHandle::unwatch`] (`#[blaze::linear]`).
#[blaze::handle]
#[blaze::linear]
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct WatchHandle<'io> {
    slot: u32,
    generation: NonZeroU32,
    _brand: Brand<'io>,
}

const _: () = assert!(size_of::<WatchHandle<'_>>() == 8);
const _: () = assert!(size_of::<Option<WatchHandle<'_>>>() == 8);

impl<'io> WatchHandle<'io> {
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

    /// Builds a [`NextChangeReq`] polling the next queued change on this watch without consuming
    /// the handle.
    #[inline]
    pub const fn next_change(&self) -> NextChangeReq<'io> {
        NextChangeReq {
            slot: self.slot,
            generation: self.generation,
            _brand: self._brand,
        }
    }

    /// Consumes this linear watch handle into an [`UnwatchReq`].
    #[blaze::consumes]
    #[inline]
    pub const fn unwatch(self) -> UnwatchReq<'io> {
        UnwatchReq { watch: self }
    }

    #[blaze::consumes]
    #[inline]
    const fn consume_raw(self) -> (u32, NonZeroU32) {
        let Self { slot, generation, _brand: _ } = self;
        (slot, generation)
    }
}

/// Kind of filesystem change delivered by [`NextChangeReq`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WatchChangeKind {
    Created,
    Modified,
    Removed,
    Renamed,
    Overflow,
}

/// One observed filesystem change beneath a watched directory.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WatchChange {
    /// Relative path beneath the watched directory (`None` on [`WatchChangeKind::Overflow`]).
    pub path: Option<FileName>,
    pub kind: WatchChangeKind,
}

impl WatchChange {
    #[inline]
    pub const fn file(path: FileName, kind: WatchChangeKind) -> Self {
        Self { path: Some(path), kind }
    }

    #[inline]
    pub const fn overflow() -> Self {
        Self { path: None, kind: WatchChangeKind::Overflow }
    }
}

/// Typed refusal returned by watch request handlers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WatchError {
    /// Target directory spelling was empty or invalid.
    InvalidDir,
    /// Handler reached its bounded watch-slot cap (rule 18).
    TableFull { max_watches: u32 },
    /// Watch handle slot is invalid, unwatched, or stale (generation mismatch).
    StaleHandle { slot: u32, generation: NonZeroU32 },
    /// Explicit handler allocator refused growth.
    OutOfMemory,
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WatchError::InvalidDir => f.write_str("invalid watch directory path"),
            WatchError::TableFull { max_watches } => {
                write!(f, "watch handle table full at cap {max_watches}")
            }
            WatchError::StaleHandle { slot, generation } => {
                write!(f, "stale watch handle (slot={slot}, gen={generation})")
            }
            WatchError::OutOfMemory => f.write_str("watch handler allocator out of memory"),
        }
    }
}

impl core::error::Error for WatchError {}

/// Options for [`WatchDirReq::new`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct WatchDirOpts<'a> {
    pub dir: &'a str,
    pub recursive: bool,
}

/// Typed linear request to begin watching `dir` for filesystem changes.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct WatchDirReq<'io, 'a> {
    pub dir: &'a str,
    pub recursive: bool,
    _brand: PhantomData<Brand<'io>>,
}

impl<'io, 'a> WatchDirReq<'io, 'a> {
    #[inline]
    pub const fn new(opts: WatchDirOpts<'a>) -> Self {
        Self {
            dir: opts.dir,
            recursive: opts.recursive,
            _brand: PhantomData,
        }
    }
}

impl<'io, 'a> Request for WatchDirReq<'io, 'a> {
    type Answer<'s> = Result<WatchHandle<'io>, WatchError>;
}

/// Typed linear request to receive the next [`WatchChange`] from an active watch (`None` when
/// no further changes are pending).
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct NextChangeReq<'io> {
    slot: u32,
    generation: NonZeroU32,
    _brand: Brand<'io>,
}

impl<'io> NextChangeReq<'io> {
    #[inline]
    pub const fn new(watch: &WatchHandle<'io>) -> Self {
        watch.next_change()
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

impl<'io> Request for NextChangeReq<'io> {
    type Answer<'s> = Result<Option<WatchChange>, WatchError>;
}

/// Typed linear request that consumes [`WatchHandle<'io>`] and stops watching its directory.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct UnwatchReq<'io> {
    pub watch: WatchHandle<'io>,
}

impl<'io> UnwatchReq<'io> {
    #[blaze::consumes]
    #[inline]
    pub const fn new(watch: WatchHandle<'io>) -> Self {
        Self { watch }
    }
}

impl<'io> Request for UnwatchReq<'io> {
    type Answer<'s> = Result<(), WatchError>;
}

/// Unified watch request union for driving sans-IO [`blaze_machine::Machine`]s.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub enum WatchReq<'io, 'a> {
    WatchDir(WatchDirReq<'io, 'a>),
    NextChange(NextChangeReq<'io>),
    Unwatch(UnwatchReq<'io>),
}

/// Unified answer matching [`WatchReq`].
#[derive(PartialEq, Eq, Debug)]
pub enum WatchAns<'io> {
    Watched(Result<WatchHandle<'io>, WatchError>),
    Change(Result<Option<WatchChange>, WatchError>),
    Unwatched(Result<(), WatchError>),
}

impl<'io, 'a> Request for WatchReq<'io, 'a> {
    type Answer<'s> = WatchAns<'io>;
}

/// Options for [`SimWatchHandler::new`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct SimWatchOpts<'io, 'a> {
    pub brand: Brand<'io>,
    pub max_watches: u32,
    pub alloc: Alloc<'a>,
}

/// Loud leak error returned by [`SimWatchHandler::finish_check`] when active watches were not
/// unwatched before task exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimWatchHandlerError {
    LeakedWatches { live: u32 },
}

impl fmt::Display for SimWatchHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SimWatchHandlerError::LeakedWatches { live } => {
                write!(f, "SimWatchHandler finished with {live} active watch handle(s)")
            }
        }
    }
}

impl core::error::Error for SimWatchHandlerError {}

enum SlotState<'a> {
    Live {
        recursive: bool,
        events: Vec<WatchChange, Alloc<'a>>,
        head: usize,
    },
    Vacant,
    Retired,
}

struct WatchSlot<'a> {
    generation: NonZeroU32,
    state: SlotState<'a>,
}

/// Deterministic `#![no_std]` handler for [`WatchDirReq`], [`NextChangeReq`], [`UnwatchReq`],
/// and [`WatchReq`] backed by an explicit caller [`Alloc`] and generational slot table.
pub struct SimWatchHandler<'io, 'a> {
    brand: Brand<'io>,
    max_watches: u32,
    alloc: Alloc<'a>,
    prequeued: Vec<(&'a str, WatchChange), Alloc<'a>>,
    slots: Vec<WatchSlot<'a>, Alloc<'a>>,
    free_list: Vec<u32, Alloc<'a>>,
    live_watches: u32,
}

impl<'io, 'a> SimWatchHandler<'io, 'a> {
    /// Creates an empty deterministic watch handler with bounded slot capacity `max_watches`.
    pub fn new(opts: SimWatchOpts<'io, 'a>) -> Self {
        Self {
            brand: opts.brand,
            max_watches: opts.max_watches,
            alloc: opts.alloc,
            prequeued: Vec::new_in(opts.alloc),
            slots: Vec::new_in(opts.alloc),
            free_list: Vec::new_in(opts.alloc),
            live_watches: 0,
        }
    }

    /// Pre-queues a scripted `change` that will be seeded into the next watch opened for `dir`.
    pub fn queue_for_dir(&mut self, dir: &'a str, change: WatchChange) -> Result<(), WatchError> {
        self.prequeued.try_push((dir, change)).map_err(|_| WatchError::OutOfMemory)
    }

    /// Pushes a scripted `change` onto an already-open `watch` handle.
    pub fn push_change(
        &mut self,
        watch: &WatchHandle<'io>,
        change: WatchChange,
    ) -> Result<(), WatchError> {
        let slot = watch.slot();
        let generation = watch.generation();
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return Err(WatchError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(WatchError::StaleHandle { slot, generation });
        }
        match &mut entry.state {
            SlotState::Live { events, .. } => {
                events.try_push(change).map_err(|_| WatchError::OutOfMemory)
            }
            SlotState::Vacant | SlotState::Retired => {
                Err(WatchError::StaleHandle { slot, generation })
            }
        }
    }

    /// Returns whether `watch` was opened recursively.
    pub fn is_recursive(&self, watch: &WatchHandle<'io>) -> Result<bool, WatchError> {
        let slot = watch.slot();
        let generation = watch.generation();
        let Some(entry) = self.slots.get(slot as usize) else {
            return Err(WatchError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(WatchError::StaleHandle { slot, generation });
        }
        match entry.state {
            SlotState::Live { recursive, .. } => Ok(recursive),
            SlotState::Vacant | SlotState::Retired => {
                Err(WatchError::StaleHandle { slot, generation })
            }
        }
    }

    /// Number of currently active (not yet unwatched) handles.
    #[inline]
    pub const fn live_watches(&self) -> u32 {
        self.live_watches
    }

    /// Verifies that every watch opened on this handler was cleanly closed via [`UnwatchReq`].
    #[inline]
    pub const fn finish_check(&self) -> Result<(), SimWatchHandlerError> {
        if self.live_watches == 0 {
            Ok(())
        } else {
            Err(SimWatchHandlerError::LeakedWatches { live: self.live_watches })
        }
    }

    fn seed_events(&mut self, dir: &str) -> Result<Vec<WatchChange, Alloc<'a>>, WatchError> {
        let mut events = Vec::new_in(self.alloc);
        let mut remaining = Vec::new_in(self.alloc);
        for (queued_dir, change) in self.prequeued.drain(..) {
            if queued_dir == dir {
                events.try_push(change).map_err(|_| WatchError::OutOfMemory)?;
            } else {
                remaining.try_push((queued_dir, change)).map_err(|_| WatchError::OutOfMemory)?;
            }
        }
        self.prequeued = remaining;
        Ok(events)
    }

    fn handle_watch_dir(&mut self, req: WatchDirReq<'io, '_>) -> Result<WatchHandle<'io>, WatchError> {
        let WatchDirReq { dir, recursive, _brand: _ } = req;
        if dir.is_empty() {
            return Err(WatchError::InvalidDir);
        }

        if !self.free_list.is_empty() {
            let events = self.seed_events(dir)?;
            let slot_idx = self.free_list.pop().expect("checked non-empty free_list");
            let entry = &mut self.slots[slot_idx as usize];
            entry.state = SlotState::Live { recursive, events, head: 0 };
            self.live_watches = self.live_watches.saturating_add(1);
            return Ok(WatchHandle::new(slot_idx, entry.generation, self.brand));
        }

        let slot_idx = u32::try_from(self.slots.len())
            .map_err(|_| WatchError::TableFull { max_watches: self.max_watches })?;
        if slot_idx >= self.max_watches {
            return Err(WatchError::TableFull { max_watches: self.max_watches });
        }
        let events = self.seed_events(dir)?;
        let generation = NonZeroU32::MIN;
        self.slots
            .try_push(WatchSlot {
                generation,
                state: SlotState::Live { recursive, events, head: 0 },
            })
            .map_err(|_| WatchError::OutOfMemory)?;
        self.live_watches = self.live_watches.saturating_add(1);
        Ok(WatchHandle::new(slot_idx, generation, self.brand))
    }

    fn handle_next_change(
        &mut self,
        req: NextChangeReq<'io>,
    ) -> Result<Option<WatchChange>, WatchError> {
        let NextChangeReq { slot, generation, _brand: _ } = req;
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return Err(WatchError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(WatchError::StaleHandle { slot, generation });
        }
        match &mut entry.state {
            SlotState::Live { events, head, recursive: _ } => {
                if let Some(change) = events.get(*head).cloned() {
                    *head = head.saturating_add(1);
                    Ok(Some(change))
                } else {
                    Ok(None)
                }
            }
            SlotState::Vacant | SlotState::Retired => {
                Err(WatchError::StaleHandle { slot, generation })
            }
        }
    }

    fn handle_unwatch(&mut self, req: UnwatchReq<'io>) -> Result<(), WatchError> {
        let UnwatchReq { watch } = req;
        let (slot, generation) = watch.consume_raw();
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return Err(WatchError::StaleHandle { slot, generation });
        };
        if entry.generation != generation {
            return Err(WatchError::StaleHandle { slot, generation });
        }
        if !matches!(entry.state, SlotState::Live { .. }) {
            return Err(WatchError::StaleHandle { slot, generation });
        }

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
        self.live_watches = self.live_watches.saturating_sub(1);
        Ok(())
    }
}

impl<'io> Handler<WatchDirReq<'io, '_>> for SimWatchHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: WatchDirReq<'io, '_>,
        deliver: impl FnOnce(Perform<Result<WatchHandle<'io>, WatchError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_watch_dir(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<NextChangeReq<'io>> for SimWatchHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: NextChangeReq<'io>,
        deliver: impl FnOnce(Perform<Result<Option<WatchChange>, WatchError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_next_change(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<UnwatchReq<'io>> for SimWatchHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: UnwatchReq<'io>,
        deliver: impl FnOnce(Perform<Result<(), WatchError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.handle_unwatch(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io> Handler<WatchReq<'io, '_>> for SimWatchHandler<'io, '_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: WatchReq<'io, '_>,
        deliver: impl FnOnce(Perform<WatchAns<'io>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = match request {
            WatchReq::WatchDir(req) => WatchAns::Watched(self.handle_watch_dir(req)),
            WatchReq::NextChange(req) => WatchAns::Change(self.handle_next_change(req)),
            WatchReq::Unwatch(req) => WatchAns::Unwatched(self.handle_unwatch(req)),
        };
        Ok(deliver(Perform::Answered(ans)))
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_core::FileName;
    use blaze_machine::{CancelAction, DriveOpts, Machine, Stopped, Turn};
    use blaze_own::{testing::heap, with_brand};

    use super::{
        SimWatchHandler, SimWatchOpts, WatchAns, WatchChange, WatchChangeKind, WatchDirOpts,
        WatchDirReq, WatchError, WatchHandle, WatchReq,
    };
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    enum WatchOneChangeMachine<'io, 'a> {
        Init { dir: &'a str },
        Opening,
        Waiting { watch: WatchHandle<'io> },
        Closing { out: Result<Option<WatchChange>, WatchError> },
    }

    impl<'io, 'a> Machine for WatchOneChangeMachine<'io, 'a> {
        type Req = WatchReq<'io, 'a>;
        type Out = Option<WatchChange>;
        type Err = WatchError;

        fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match self {
                WatchOneChangeMachine::Init { dir } => Turn::Ask {
                    request: WatchReq::WatchDir(WatchDirReq::new(WatchDirOpts {
                        dir,
                        recursive: true,
                    })),
                    next: WatchOneChangeMachine::Opening,
                },
                _ => unreachable!("WatchOneChangeMachine::start called on non-Init state"),
            }
        }

        fn resume(
            self,
            answer: WatchAns<'io>,
            _cx: &mut (),
        ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match (self, answer) {
                (WatchOneChangeMachine::Opening, WatchAns::Watched(Ok(watch))) => {
                    let poll = watch.next_change();
                    Turn::Ask {
                        request: WatchReq::NextChange(poll),
                        next: WatchOneChangeMachine::Waiting { watch },
                    }
                }
                (WatchOneChangeMachine::Opening, WatchAns::Watched(Err(err))) => Turn::Fail(err),
                (WatchOneChangeMachine::Waiting { watch }, WatchAns::Change(res)) => Turn::Ask {
                    request: WatchReq::Unwatch(watch.unwatch()),
                    next: WatchOneChangeMachine::Closing { out: res },
                },
                (WatchOneChangeMachine::Closing { out: Ok(change) }, WatchAns::Unwatched(Ok(()))) => {
                    Turn::Done(change)
                }
                (WatchOneChangeMachine::Closing { out: Ok(_) }, WatchAns::Unwatched(Err(err))) => {
                    Turn::Fail(err)
                }
                (WatchOneChangeMachine::Closing { out: Err(err) }, WatchAns::Unwatched(_)) => {
                    Turn::Fail(err)
                }
                _ => unreachable!("typed WatchAns always matches active state"),
            }
        }

        fn on_cancel(
            self,
            _stopped: Stopped,
            _cx: &mut (),
        ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
            match self {
                WatchOneChangeMachine::Waiting { watch } => CancelAction::Cleanup(Turn::Ask {
                    request: WatchReq::Unwatch(watch.unwatch()),
                    next: WatchOneChangeMachine::Closing { out: Ok(None) },
                }),
                WatchOneChangeMachine::Init { .. }
                | WatchOneChangeMachine::Opening
                | WatchOneChangeMachine::Closing { .. } => CancelAction::Abandon,
            }
        }
    }

    #[test]
    fn cx_run_watch_dir_and_unwatch() {
        let h = heap();
        let io = Io::simulated(SimWorld::new(97, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");

        with_brand(|brand| {
            let mut handler = SimWatchHandler::new(SimWatchOpts {
                brand,
                max_watches: 4,
                alloc: h.alloc(),
            });

            // 1. Direct Cx::run lifecycle: WatchDirReq -> push_change -> NextChangeReq -> UnwatchReq.
            let watch = cx
                .run(
                    "bstd.watch.dir",
                    &mut handler,
                    WatchDirReq::new(WatchDirOpts { dir: "src", recursive: true }),
                )
                .expect("handler perform watch_dir")
                .expect("watch_dir ok");
            assert_eq!(handler.live_watches(), 1);
            assert_eq!(handler.is_recursive(&watch), Ok(true));

            let created = WatchChange::file(
                FileName::new("src/main.rs").expect("valid filename"),
                WatchChangeKind::Created,
            );
            let modified = WatchChange::file(
                FileName::new("src/lib.rs").expect("valid filename"),
                WatchChangeKind::Modified,
            );
            handler.push_change(&watch, created.clone()).expect("push created");
            handler.push_change(&watch, modified.clone()).expect("push modified");

            let first = cx
                .run("bstd.watch.next_1", &mut handler, watch.next_change())
                .expect("handler perform next 1")
                .expect("next 1 ok");
            assert_eq!(first, Some(created));

            let second = cx
                .run("bstd.watch.next_2", &mut handler, watch.next_change())
                .expect("handler perform next 2")
                .expect("next 2 ok");
            assert_eq!(second, Some(modified));

            let empty = cx
                .run("bstd.watch.next_empty", &mut handler, watch.next_change())
                .expect("handler perform next empty")
                .expect("next empty ok");
            assert_eq!(empty, None);

            // Capture a NextChangeReq before unwatching to prove generational invalidation.
            let stale_poll = watch.next_change();
            cx.run("bstd.watch.unwatch", &mut handler, watch.unwatch())
                .expect("handler perform unwatch")
                .expect("unwatch ok");
            assert_eq!(handler.live_watches(), 0);

            let stale_res = cx
                .run("bstd.watch.next_stale", &mut handler, stale_poll)
                .expect("handler perform stale poll");
            assert!(matches!(stale_res, Err(WatchError::StaleHandle { .. })));

            // 2. Driving WatchOneChangeMachine over WatchReq through Cx::drive_machine.
            let expected = WatchChange::file(
                FileName::new("Cargo.toml").expect("valid filename"),
                WatchChangeKind::Modified,
            );
            handler.queue_for_dir("pkg", expected.clone()).expect("queue_for_dir");

            let got = cx
                .drive_machine(
                    "bstd.watch.machine",
                    &mut handler,
                    &mut (),
                    WatchOneChangeMachine::Init { dir: "pkg" },
                    DriveOpts { max_steps: 8 },
                )
                .expect("watch machine succeeds");
            assert_eq!(got, Some(expected));
            assert_eq!(handler.finish_check(), Ok(()));
        });
    }
}
