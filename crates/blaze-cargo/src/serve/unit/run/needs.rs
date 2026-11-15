//! The first calls of a launched run made by the checkout's front end (docs/v2/plan/design/
//! launcher.md, "where needs are served"): a sans-IO core.
//!
//! After a build's answer one task holds the checkout's front end (`Does::StoreKept`). When a
//! launcher calls a function no bind record answers (a workspace function, or one whose record
//! names what has no bind key), its reader queues a [`Need`]; the task makes it with the front
//! end (`Build::one`) and the program takes it. [`Serve`] decides what the task does next from
//! what it heard: make a need, wait, or end. Its driver (`launch::serve_needs`) owns the
//! channels, the wait and the front end; this owns no IO and reads no clock: the driver gives
//! it the time with [`Heard::Deadline`].
//!
//! The task ends ([`Served`]):
//! - [`Served::Wanted`] at the first step after a request of the checkout wants the front end
//!   ([`Heard::Woken`] with `wanted`): the needs not yet made are handed back ([`Serve::left`])
//!   and wait for the next build's task, which holds the front end again;
//! - [`Served::Detached`] once every launcher has gone and no need is queued or being made;
//! - [`Served::NeverAttached`] when no launcher attached within [`ATTACH`] of the task's start.
//!
//! **The invariant** (`tests`): over every interleaving of needs, launchers attaching and
//! detaching, a request wanting the front end and the deadline, each need gets exactly one
//! [`Step::Make`] or is among [`Serve::left`] at [`Served::Wanted`]; the step after a wanting
//! wake is `Done(Wanted)`; and the task ends.
//!
//! When no task holds the front end (after a daemon start, or once a serving task ended
//! `Detached`), [`Front`] decides: the need is queued and the daemon sends itself the
//! `blaze run` it last answered for the program, whose build loads the front end and whose
//! task after it serves the queue; or the needs are refused by name, once, when that build
//! fails, makes no start of the program, or holds nothing within [`BUILD`].
//!
//! Between a build's link, which holds the front end for the program ([`Front::held`]), and
//! the task after its answer, which serves from it ([`Front::serving`]), no one serves. The
//! needs queued then wait for that task, and the task may never come: the store before it
//! failed (ripgrep on the build VM: a launched program waited 900 s on its first call after
//! `blaze: ok`, because the task served needs only when the store succeeded), or a request
//! that wanted the front end back never linked. So a loss is said at once ([`Front::lost`]:
//! every need refused naming why, until a link holds a front end again), and what nothing says
//! is bounded ([`SERVED_WITHIN`]). The launched program's own wait for a first call has no
//! clock of its own (`blaze-launch-rt`), so this bound is the one its wait has: a run either
//! starts, or ends with a named failure, within [`SERVED_WITHIN`] of its build's answer.
//!
//! **The floor**: a need is one queue push and, at the end, one frame; the bound adds one
//! deadline to the loop's existing wait (`Launches::wants`, every turn), no thread or timer.


use std::collections::VecDeque;
use std::time::Duration;

use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_launch_host::Conn;

#[cfg(test)]
mod tests;

/// How long after the task starts a launcher has to attach before the task lets the front
/// end go: `blaze run` starts its program as soon as the build answers.
pub(crate) const ATTACH: Duration = Duration::from_secs(2);

/// A launcher's first call the front end must make: the program (its id, as its sidecar
/// names it), the launcher, the slot and the slot's identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Need {
    pub(crate) program: [u8; 32],
    pub(crate) conn: Conn,
    pub(crate) slot: u32,
    pub(crate) identity: ContentId,
}

/// What the driver heard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Heard {
    /// A first call to make.
    Need(Need),
    /// The task was woken by the loop; `wanted`: a request of the checkout waits for the
    /// front end.
    Woken { wanted: bool },
    /// How many launchers of the checkout's programs are attached now.
    Attached(u32),
    /// The time on the driver's clock.
    Deadline(Duration),
    /// The need of the last [`Step::Make`] was made (or refused) and given to its program.
    Made,
}

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// Waits for what it hears next, until `until` at the latest.
    Wait { until: Deadline },
    /// Makes this need, then says [`Heard::Made`].
    Make(Need),
    /// Lets the front end go.
    Done(Served),
}

/// Why the task let the front end go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Served {
    Wanted,
    Detached,
    NeverAttached,
}

/// A safeguard taken out, for the tests' controls: always `Plant::None` outside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum Plant {
    None,
    /// A wanting wake is not heeded while needs are queued: the front end is held from the
    /// request that waits for it.
    ServesWhenWanted,
    /// The task ends once no launcher is attached, whatever is queued or being made.
    EndsWithNeeds,
}

/// The core. See the module's documentation.
pub(crate) struct Serve {
    /// Heard and not made yet, in the order heard.
    queued: VecDeque<Need>,
    /// The need of the last `Make`, until `Made`.
    making: Option<Need>,
    attached: u32,
    /// Whether a launcher was ever attached (or a need heard) while the task ran.
    ever: bool,
    wanted: bool,
    now: Duration,
    /// When the task lets go if no launcher attached.
    attach: Deadline,
    done: Option<Served>,
    pub(crate) plant: Plant,
}

impl Serve {
    /// A task starting at `now`, with `attached` launchers attached already.
    pub(crate) fn new(now: Duration, attached: u32) -> Serve {
        Serve {
            queued: VecDeque::new(),
            making: None,
            attached,
            ever: attached > 0,
            wanted: false,
            now,
            attach: Deadline::after(now, ATTACH),
            done: None,
            plant: Plant::None,
        }
    }

    /// What the driver does after hearing `heard`. Once `Done`, always the same `Done`.
    pub(crate) fn step(&mut self, heard: Heard) -> Step {
        if let Some(done) = self.done {
            return Step::Done(done);
        }
        match heard {
            Heard::Need(need) => {
                self.ever = true;
                self.queued.push_back(need);
            }
            Heard::Woken { wanted } => self.wanted |= wanted,
            Heard::Attached(count) => {
                self.attached = count;
                self.ever |= count > 0;
            }
            Heard::Deadline(now) => self.now = self.now.max(now),
            Heard::Made => self.making = None,
        }
        let busy = self.making.is_some() || !self.queued.is_empty();
        if self.wanted && !(self.plant == Plant::ServesWhenWanted && busy) {
            return self.end(Served::Wanted);
        }
        if self.plant == Plant::EndsWithNeeds && self.attached == 0 && self.ever {
            return self.end(Served::Detached);
        }
        if self.making.is_some() {
            return Step::Wait { until: Deadline::after(self.now, ATTACH) };
        }
        if let Some(need) = self.queued.pop_front() {
            self.making = Some(need);
            return Step::Make(need);
        }
        if self.attached > 0 {
            // A launcher runs: the wait is renewed, each one bounded.
            return Step::Wait { until: Deadline::after(self.now, ATTACH) };
        }
        if self.ever {
            return self.end(Served::Detached);
        }
        if self.attach.passed(self.now) {
            return self.end(Served::NeverAttached);
        }
        Step::Wait { until: self.attach }
    }

    fn end(&mut self, served: Served) -> Step {
        self.done = Some(served);
        Step::Done(served)
    }

    /// The needs not made, the one being made first: what the next build's task makes, once
    /// the task ended [`Served::Wanted`]. Taken once.
    pub(crate) fn left(&mut self) -> Vec<Need> {
        // alloc: once a task, the needs it leaves
        let mut left = Vec::with_capacity(self.queued.len() + 1);
        left.extend(self.making.take());
        left.extend(self.queued.drain(..));
        left
    }
}

/// How long a first call waits for the build the daemon sent itself to hold the front end
/// ([`Front`]) before it is refused by name.
pub(crate) const BUILD: Duration = Duration::from_secs(120);

/// How long a first call queued while a build's link holds the front end and no task serves
/// it yet ([`Front::held`]) waits for the task after that build's answer to take it, before
/// it is refused by name ([`Refuse::Unserved`]). That task starts once the build's front end
/// is kept and its store is made (`Does::StoreKept`); a store that fails says so at once
/// ([`Front::lost`]), so this bounds what nothing says: a request that wanted the front end
/// back and never linked, a task never posted. The same as [`BUILD`], which bounds a whole
/// build's way to a link: a store is part of a build's end.
pub(crate) const SERVED_WITHIN: Duration = Duration::from_secs(120);

/// What a launched program's first call no record answers does ([`Front::want`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Want {
    /// It is queued: a task holds the front end for it, or the build asked for one will.
    Queue,
    /// It is queued, and the daemon sends itself the `blaze run` it last answered for the
    /// program: no task holds the front end and none was asked.
    Build,
    /// It is refused at once, for the reason [`Front::lost_why`] gives: the front end the
    /// program's last link held was lost before any task served from it.
    Refuse,
}

/// Why the queued first calls of a program are refused ([`Front::built`], [`Front::tick`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refuse {
    /// The build the daemon asked for failed, or none could be asked.
    Failed,
    /// It ended well and linked no start of the program: no task holds its front end.
    NoStart,
    /// [`BUILD`] passed since it was asked and no build's link held the front end.
    Late,
    /// [`SERVED_WITHIN`] passed since a need was queued while a link held the front end, and
    /// no task took it.
    Unserved,
}

/// A safeguard of [`Front`] taken out, for the tests' controls: always `FrontPlant::None`
/// outside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum FrontPlant {
    None,
    /// A want while a build is asked asks another.
    AsksTwice,
    /// An asked build never times out.
    NeverLate,
    /// A loss of the held front end is not heard: the needs stay queued for a task that will
    /// never run (the ripgrep hang: a store that failed after the answer).
    LeftOpen,
    /// A need queued while a link holds the front end and no task serves is never refused
    /// late: it waits as long as nothing takes it.
    Unbounded,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holds {
    /// No task holds the front end for the program and none was asked.
    None,
    /// The daemon sent itself a build, at this time on the driver's clock.
    Asked { at: Duration },
    /// A build's link held it: the task after that build serves the needs. `serving`: that
    /// task runs now. `since`: while it does not, when the first need still queued was
    /// queued, on the driver's clock ([`SERVED_WITHIN`] runs from it).
    Held { serving: bool, since: Option<Duration> },
    /// The front end the last link held was lost before a task served from it
    /// ([`Front::lost`]): every need is refused at once, by [`Front::lost_why`], until a
    /// build's link holds one again or the task after a later build of the checkout serves.
    Lost,
}

/// Who holds the front end for one launched program's needs: a sans-IO core on an injected
/// clock (the time is given to [`Front::want`] and [`Front::tick`]). Its driver
/// (`launch::wanted`, the daemon's `launch_turn`) sends the build and refuses the needs.
///
/// **The invariant** (`tests`): over every order of wants, links, the build's end, the
/// serving task's start and end, the held front end's loss and ticks, at most one build is
/// asked while one is asked or held, and from every state a queued need is served by a task
/// or refused once: within [`BUILD`] of a build asked, within [`SERVED_WITHIN`] of its
/// queueing while a link holds the front end and no task serves, and at once once the held
/// front end is lost.
pub(crate) struct Front {
    holds: Holds,
    /// Whether the build the daemon sent itself has not ended yet ([`Front::built`]), whatever
    /// became of what it was asked for: one refused late, or one whose link's task let the
    /// front end go, is still under way, and no second is sent beside it. Its end is the only
    /// one [`Front::built`] hears, so it never ends a build asked after it.
    running: bool,
    /// Why the held front end was lost, while it is ([`Holds::Lost`]).
    lost: Option<String>,
    pub(crate) plant: FrontPlant,
}

impl Front {
    pub(crate) fn new() -> Front {
        Front { holds: Holds::None, running: false, lost: None, plant: FrontPlant::None }
    }

    /// A first call no record answers, at `now`: [`Want::Build`] when none holds or was asked
    /// and no build the daemon sent itself is still under way (and it is asked from then),
    /// else [`Want::Queue`]. With one under way (refused late, or its task let go) the need
    /// waits for that build, bounded by [`BUILD`] from now: its link holds the front end, or
    /// its end refuses the need ([`Front::built`]). While a link holds it and no task serves,
    /// the need waits [`SERVED_WITHIN`] at most; once it was lost, [`Want::Refuse`].
    pub(crate) fn want(&mut self, now: Duration) -> Want {
        match self.holds {
            Holds::None if self.running => {
                self.holds = Holds::Asked { at: now };
                Want::Queue
            }
            Holds::None => {
                self.holds = Holds::Asked { at: now };
                self.running = true;
                Want::Build
            }
            Holds::Asked { .. } if self.plant == FrontPlant::AsksTwice => {
                self.holds = Holds::Asked { at: now };
                self.running = true;
                Want::Build
            }
            Holds::Held { serving: false, since: None } if self.plant != FrontPlant::Unbounded => {
                self.holds = Holds::Held { serving: false, since: Some(now) };
                Want::Queue
            }
            Holds::Asked { .. } | Holds::Held { .. } => Want::Queue,
            Holds::Lost => Want::Refuse,
        }
    }

    /// Whether a task holds the front end for the program's needs, or the next build's will.
    pub(crate) fn is_held(&self) -> bool {
        matches!(self.holds, Holds::Held { .. })
    }

    /// Whether a build was asked and has not held the front end yet.
    pub(crate) fn is_asked(&self) -> bool {
        matches!(self.holds, Holds::Asked { .. })
    }

    /// A build's link, at `now`: the task after it serves the needs. `waiting`: needs are
    /// queued already, and [`SERVED_WITHIN`] runs for them from now.
    pub(crate) fn held(&mut self, now: Duration, waiting: bool) {
        let since = (waiting && self.plant != FrontPlant::Unbounded).then_some(now);
        self.holds = Holds::Held { serving: false, since };
        self.lost = None;
    }

    /// Whether the task after a build's answer of the program's checkout serves its needs
    /// from the front end it holds: a link held it for them, or held one that was lost (the
    /// checkout's later build kept a front end again, which serves the program on).
    pub(crate) fn servable(&self) -> bool {
        matches!(self.holds, Holds::Held { .. } | Holds::Lost)
    }

    /// The task after a build's answer started serving the needs ([`Front::servable`]):
    /// nothing waits unserved, and nothing is lost.
    pub(crate) fn serving(&mut self) {
        if self.servable() {
            self.holds = Holds::Held { serving: true, since: None };
            self.lost = None;
        }
    }

    /// The serving task handed the front end to a request that wants it (`Wanted`), at `now`:
    /// the link still holds it for the task after that request's build. `waiting`: it handed
    /// back needs not made, and [`SERVED_WITHIN`] runs for them from now.
    pub(crate) fn handed_back(&mut self, now: Duration, waiting: bool) {
        if let Holds::Held { .. } = self.holds {
            let since = (waiting && self.plant != FrontPlant::Unbounded).then_some(now);
            self.holds = Holds::Held { serving: false, since };
        }
    }

    /// The front end a link held is lost: the store after the build's answer failed or
    /// panicked (a panic while the task served from it too), or the front end was taken
    /// before its task ran, so no task after that build will serve. Says whether the queued
    /// needs are refused now, by `why`, as every later one is ([`Want::Refuse`]) until a link
    /// holds a front end again. Nothing when no link holds it. Only the task that holds the
    /// checkout's one kept front end says so, so a task serving is that task.
    pub(crate) fn lost(&mut self, why: &str) -> bool {
        if self.plant == FrontPlant::LeftOpen || !self.is_held() {
            return false;
        }
        self.holds = Holds::Lost;
        // Kept, not borrowed: every first call until the next link is refused by it.
        self.lost = Some(why.to_owned());
        true
    }

    /// Why the held front end was lost, while it is.
    pub(crate) fn lost_why(&self) -> Option<&str> {
        self.lost.as_deref()
    }

    /// The serving task ended with no request wanting the front end (`Detached`,
    /// `NeverAttached`): no one holds it.
    pub(crate) fn let_go(&mut self) {
        self.holds = Holds::None;
    }

    /// The daemon's own request ended, `ok` or not: where it never held the front end, the
    /// queued needs are refused ([`Refuse::Failed`], [`Refuse::NoStart`]) and none holds it.
    pub(crate) fn built(&mut self, ok: bool) -> Option<Refuse> {
        self.running = false;
        if !self.is_asked() {
            return None;
        }
        self.holds = Holds::None;
        Some(if ok { Refuse::NoStart } else { Refuse::Failed })
    }

    /// The time on the driver's clock: a build asked [`BUILD`] ago that never held the front
    /// end is [`Refuse::Late`], and a need queued [`SERVED_WITHIN`] ago while a link held it
    /// and no task took it is [`Refuse::Unserved`]; either way none holds it after.
    pub(crate) fn tick(&mut self, now: Duration) -> Option<Refuse> {
        match self.holds {
            Holds::Asked { at } if self.plant != FrontPlant::NeverLate && now.saturating_sub(at) >= BUILD => {
                self.holds = Holds::None;
                Some(Refuse::Late)
            }
            Holds::Held { serving: false, since: Some(at) } if now.saturating_sub(at) >= SERVED_WITHIN => {
                self.holds = Holds::None;
                Some(Refuse::Unserved)
            }
            Holds::None | Holds::Asked { .. } | Holds::Held { .. } | Holds::Lost => None,
        }
    }

    /// When [`Front::tick`] refuses next, if a build is asked or a need waits unserved: what
    /// its driver's wait is bounded by.
    pub(crate) fn due(&self) -> Option<Deadline> {
        match self.holds {
            Holds::Asked { at } => Some(Deadline::after(at, BUILD)),
            Holds::Held { serving: false, since: Some(at) } => Some(Deadline::after(at, SERVED_WITHIN)),
            Holds::None | Holds::Held { .. } | Holds::Lost => None,
        }
    }
}
