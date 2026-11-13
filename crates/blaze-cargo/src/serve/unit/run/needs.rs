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

/// What a launched program's first call no record answers does ([`Front::want`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Want {
    /// It is queued: a task holds the front end for it, or the build asked for one will.
    Queue,
    /// It is queued, and the daemon sends itself the `blaze run` it last answered for the
    /// program: no task holds the front end and none was asked.
    Build,
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
}

/// A safeguard of [`Front`] taken out, for the tests' controls: always `FrontPlant::None`
/// outside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
pub(crate) enum FrontPlant {
    None,
    /// A want while a build is asked asks another.
    AsksTwice,
    /// An asked build never times out.
    NeverLate,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holds {
    /// No task holds the front end for the program and none was asked.
    None,
    /// The daemon sent itself a build, at this time on the driver's clock.
    Asked { at: Duration },
    /// A build's link held it: the task after that build serves the needs.
    Held,
}

/// Who holds the front end for one launched program's needs: a sans-IO core on an injected
/// clock (the time is given to [`Front::want`] and [`Front::tick`]). Its driver
/// (`launch::wanted`, the daemon's `launch_turn`) sends the build and refuses the needs.
///
/// **The invariant** (`tests`): over every order of wants, links, the build's end, the
/// serving task's end and ticks, at most one build is asked while one is asked or held, and
/// from every state a queued need is served by a task or refused once, within [`BUILD`].
pub(crate) struct Front {
    holds: Holds,
    /// Whether the build the daemon sent itself has not ended yet ([`Front::built`]), whatever
    /// became of what it was asked for: one refused late, or one whose link's task let the
    /// front end go, is still under way, and no second is sent beside it. Its end is the only
    /// one [`Front::built`] hears, so it never ends a build asked after it.
    running: bool,
    pub(crate) plant: FrontPlant,
}

impl Front {
    pub(crate) fn new() -> Front {
        Front { holds: Holds::None, running: false, plant: FrontPlant::None }
    }

    /// A first call no record answers, at `now`: [`Want::Build`] when none holds or was asked
    /// and no build the daemon sent itself is still under way (and it is asked from then),
    /// else [`Want::Queue`]. With one under way (refused late, or its task let go) the need
    /// waits for that build, bounded by [`BUILD`] from now: its link holds the front end, or
    /// its end refuses the need ([`Front::built`]).
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
            Holds::Asked { .. } | Holds::Held => Want::Queue,
        }
    }

    /// Whether a task holds the front end for the program's needs, or the next build's will.
    pub(crate) fn is_held(&self) -> bool {
        self.holds == Holds::Held
    }

    /// Whether a build was asked and has not held the front end yet.
    pub(crate) fn is_asked(&self) -> bool {
        matches!(self.holds, Holds::Asked { .. })
    }

    /// A build's link: the task after it serves the needs.
    pub(crate) fn held(&mut self) {
        self.holds = Holds::Held;
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
    /// end is [`Refuse::Late`], and none holds it.
    pub(crate) fn tick(&mut self, now: Duration) -> Option<Refuse> {
        let Holds::Asked { at } = self.holds else { return None };
        if self.plant == FrontPlant::NeverLate || now.saturating_sub(at) < BUILD {
            return None;
        }
        self.holds = Holds::None;
        Some(Refuse::Late)
    }

    /// When [`Front::tick`] refuses next, if a build is asked: what its driver's wait is
    /// bounded by.
    pub(crate) fn due(&self) -> Option<Deadline> {
        match self.holds {
            Holds::Asked { at } => Some(Deadline::after(at, BUILD)),
            Holds::None | Holds::Held => None,
        }
    }
}
