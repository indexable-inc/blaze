//! What each child process cost, said once it is reaped: the one place a profiler learns of
//! every child a driver started (`blaze_profile`'s `processes` table).
//!
//! A spawner names what it asks for with [`label`], keyed by its spawn effect's [`Tag`], before
//! it hands the effect to its driver; the driver's reap ([`crate::process`]) takes the label
//! back and hands a [`Reaped`] to the observer [`observe`] installed. With no observer, nothing
//! is kept: a label is dropped at once and a reap reads no extra state.
//!
//! The labels are a bounded list ([`MOST_LABELS`]): a label whose spawn never started (the
//! driver failed it before it ran) is never taken back, so the oldest are let go once the list
//! is full, and such a child, were it to run after all, is reaped unlabelled.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use crate::lock::Lock;

use crate::Tag;

/// What a spawner says of a child it asks for: what it is, for which unit, and where in the
/// build it was asked for.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: none; taken by value once per reap
pub struct Label {
    /// `build_script`, `test`, `run`, `proc_macro_host`, `linker`, `wrapper`.
    pub kind: &'static str,
    /// The unit or test binary it runs for, as the spawner names it.
    pub unit: Option<String>,
    /// The build it works for, 0 for none: what the installed context said as it was labelled.
    pub build: u64,
    /// The span it was asked for inside, 0 for none: as the build.
    pub span: u64,
}

/// A child, reaped: what it was and what the OS says it cost.
#[derive(Debug)]
pub struct Reaped<'a> {
    pub pid: u32,
    pub program: &'a std::path::Path,
    /// What its spawner said of it; `None` for a spawn no one labelled.
    pub label: Option<Label>,
    /// When the driver began to start it, before its sandbox was made.
    pub spawned: Instant,
    /// When the spawn call returned, the child having exec'd.
    pub executed: Instant,
    /// When it was reaped.
    pub exited: Instant,
    /// How long making its sandbox took; `None` for a child run without one.
    pub sandbox: Option<Duration>,
    pub user: Duration,
    pub system: Duration,
    /// Its peak resident set, in bytes.
    pub peak_rss: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// The most labels held at once: thousands of spawns in flight is past any build's.
pub const MOST_LABELS: usize = 4096;

/// The labels no reap has taken yet, oldest first: each a field update under the lock.
static LABELS: Lock<Vec<(Tag, Label)>> = Lock::new("core.reaped.labels", Vec::new());

/// Where a reaped child goes, and what says the build and span a label is asked inside.
struct Observer {
    context: fn() -> (u64, u64),
    reaped: fn(&Reaped<'_>),
}

static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Hands every child reaped from now on to `reaped`, and asks `context` for the build and span
/// each [`label`] is made in. Once a process; a second call is ignored, the first observer
/// keeping every child (it is the profiler's, installed at start-up).
pub fn observe(context: fn() -> (u64, u64), reaped: fn(&Reaped<'_>)) {
    _ = OBSERVER.set(Observer { context, reaped });
}

/// Whether an observer is installed: a reap reads its child's label only then.
pub(crate) fn observed() -> bool {
    OBSERVER.get().is_some()
}

/// Names the child the spawn effect `tag` will start: a `kind` of child, for `unit`. Call it
/// before the effect goes to the driver. Does nothing with no observer installed.
pub fn label(tag: Tag, kind: &'static str, unit: Option<&str>) {
    let Some(observer) = OBSERVER.get() else { return };
    let (build, span) = (observer.context)();
    let unit = unit.map(str::to_owned); // alloc: the unit's name, once per labelled spawn, kept until its reap
    let mut labels = LABELS.lock();
    labels.retain(|it| it.0 != tag);
    if labels.len() >= MOST_LABELS {
        labels.remove(0);
    }
    labels.push((tag, Label { kind, unit, build, span })); // alloc: the list grows to the spawns in flight at once
}

/// The label of `tag`, taken: `None` when it has none.
pub(crate) fn take(tag: Tag) -> Option<Label> {
    let mut labels = LABELS.lock();
    let at = labels.iter().position(|it| it.0 == tag)?;
    Some(labels.remove(at).1)
}

/// Hands `reaped` to the observer, if one is installed.
pub(crate) fn report(reaped: &Reaped<'_>) {
    if let Some(observer) = OBSERVER.get() {
        (observer.reaped)(reaped);
    }
}
