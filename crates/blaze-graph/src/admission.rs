//! What a task is: its kind, the subsystem it charges, and what it is predicted to cost.
//!
//! The plane admits by threads alone: a task starts when a thread of its pool is free, and the
//! kernel's limit is the one memory ceiling. Nothing here forecasts or reserves memory. What a
//! task held is still measured ([`run_as`], [`TaskSpent`]) and named by its subsystem
//! ([`tag_of`]) in spans and `blaze explain memory`, so a build's memory is answered from its
//! profile.
//!
//! A task's peak is everything it held: what its work allocated through the global allocator,
//! on its own job and on every job of the pool scopes it opened (its memory account,
//! `arena::account`), never a sample of the whole process, and what its frames held
//! (`blaze_mem::frame`).

#![forbid(unsafe_code)]

use blaze_mem::Tag;
use serde::{Deserialize, Serialize};

/// A kind of task whose tasks are alike in what they hold: a query kind's number
/// (`blaze_query::QueryKind`), or one of the plane's own (a link, a test run).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct TaskKind(pub u32);

/// The daemon's own task kinds, past every query kind (a `u16`) but the load's.
pub mod kinds {
    use super::TaskKind;

    /// The load's `Plan`: its query kind (`blaze-units`).
    pub const PLAN: TaskKind = TaskKind(0x6000);
    /// What a build fetches into the store.
    pub const FETCH: TaskKind = TaskKind(0x1_0001);
    /// The standard library built.
    pub const LIBRARY: TaskKind = TaskKind(0x1_0002);
    /// A crate's bodies lowered to MIR.
    pub const LOWER: TaskKind = TaskKind(0x1_0003);
    /// A unit's codegen: its functions compiled from its roots.
    pub const COMPILE: TaskKind = TaskKind(0x1_0004);
    /// What a binary reaches, for its compiles.
    pub const REACH: TaskKind = TaskKind(0x1_0005);
    /// A view's queries walked for `Trace`, `Verify` or `CodegenInputs`.
    pub const WALK: TaskKind = TaskKind(0x1_0006);
    /// `blaze test --ub`: the tests selected, and one test evaluated.
    pub const UB_TESTS: TaskKind = TaskKind(0x1_0007);
    pub const EVALUATE: TaskKind = TaskKind(0x1_0008);
    /// A program's codegen: of a binary, test, bench, example or `cdylib`.
    pub const PROGRAM: TaskKind = TaskKind(0x1_0009);
    /// A unit's names: whether its record is kept, its crate's def map, its check's bodies.
    pub const UNIT_NAMES: TaskKind = TaskKind(0x1_000a);
    /// A unit's check: the diagnostics of every body of its crate.
    pub const UNIT_BODIES: TaskKind = TaskKind(0x1_000b);
    /// A unit's link: its images linked from its codegen, a proc macro's expanders loaded.
    pub const UNIT_LINK: TaskKind = TaskKind(0x1_000c);
    /// A unit's outputs hashed, and its record encoded, for the store.
    pub const UNIT_STORE: TaskKind = TaskKind(0x1_000d);
    /// A program's link.
    pub const PROGRAM_LINK: TaskKind = TaskKind(0x1_000e);
    /// What a build left for the store (dependency images, check outcomes, goals) written from
    /// its front end as it is kept, before the build answers.
    pub const STORE_KEPT: TaskKind = TaskKind(0x1_000f);
    // 0x1_0010 was the give-back sweep's, deleted with memory admission: not reused, so an old
    // profile's `task` rows never read as another kind.
    /// A checkout's front-end carry (`blaze_cargo::serve::front_carry`): its kernel session
    /// carried to a build's pin and saved, on a worker after the build, never the loop.
    pub const FRONT_CARRY: TaskKind = TaskKind(0x1_0011);
    /// A build's front-end write (`blaze_cargo::serve::job`, `Does::Write`): its proc macros'
    /// loads, build scripts' runs and device builds' MSL applied, the harnesses written, the
    /// units identified and their dependencies served, on a worker holding the job's writer
    /// once no task reads its front end, never on the loop.
    pub const FRONT_WRITE: TaskKind = TaskKind(0x1_0012);
}

/// The name of one of the daemon's own task kinds ([`kinds`]), as a profile's `task` span
/// says it; `None` for a query kind, which its query's name says.
pub const fn name_of(kind: TaskKind) -> Option<&'static str> {
    Some(match kind {
        kinds::PLAN => "plan",
        kinds::FETCH => "fetch",
        kinds::LIBRARY => "library",
        kinds::LOWER => "lower",
        kinds::COMPILE => "compile",
        kinds::REACH => "reach",
        kinds::WALK => "walk",
        kinds::UB_TESTS => "ub tests",
        kinds::EVALUATE => "evaluate",
        kinds::PROGRAM => "program",
        kinds::UNIT_NAMES => "unit names",
        kinds::UNIT_BODIES => "unit bodies",
        kinds::UNIT_LINK => "unit link",
        kinds::UNIT_STORE => "unit store",
        kinds::PROGRAM_LINK => "program link",
        kinds::STORE_KEPT => "store kept",
        kinds::FRONT_CARRY => "front carry",
        kinds::FRONT_WRITE => "front write",
        _ => return None,
    })
}

/// The subsystem whose memory a task of `kind` holds: the tag it is named by.
///
/// Query kinds go by `blaze_query::kinds`' ranges (inputs, facts `0x1000`, the front end
/// `0x2000`, codegen and link `0x4000`, user code `0x5000`, the units' loads `0x6000`), the
/// front end's and the back end's ranges split by subsystem as written here and in
/// docs/v2/plan/design/memory.md ("Kinds and tags"); the daemon's own by [`kinds`]. A kind
/// none of them names is [`Tag::Other`]'s.
pub const fn tag_of(kind: TaskKind) -> Tag {
    match kind {
        kinds::FETCH | kinds::UNIT_STORE | kinds::STORE_KEPT => Tag::Store,
        kinds::LIBRARY | kinds::COMPILE | kinds::REACH | kinds::PROGRAM => Tag::Codegen,
        kinds::UNIT_NAMES => Tag::Nameres,
        kinds::FRONT_CARRY => Tag::Parse,
        kinds::FRONT_WRITE => Tag::Nameres,
        kinds::UNIT_BODIES => Tag::Infer,
        kinds::UNIT_LINK | kinds::PROGRAM_LINK => Tag::Link,
        kinds::LOWER | kinds::UB_TESTS | kinds::EVALUATE => Tag::Mir,
        kinds::WALK | TaskKind(4) => Tag::Query,
        TaskKind(1..=3) | TaskKind(5..=6) => Tag::Vfs,
        TaskKind(0x1000..0x2000) => Tag::Nameres,
        TaskKind(0x2000..0x2200) => Tag::Parse,
        TaskKind(0x2200..0x2400) => Tag::Nameres,
        TaskKind(0x2400..0x2c00) => Tag::Infer,
        TaskKind(0x2c00..0x3000) => Tag::Expand,
        TaskKind(0x3000..0x4000) => Tag::Mir,
        TaskKind(0x4000..0x4800) => Tag::Codegen,
        TaskKind(0x4800..0x5000) => Tag::Link,
        TaskKind(0x5000..0x6000) => Tag::Usercode,
        TaskKind(0x6000..0x7000) => Tag::Vfs,
        _ => Tag::Other,
    }
}

/// Runs `f` as a task of `kind`: in a [`blaze_mem::frame::task`] of [`tag_of`]`(kind)`, so
/// what it allocates from the global heap is charged to that subsystem's line and not to the
/// heap outside any task, and says the most it held: its global allocations, its fan-out's
/// included ([`crate::arena::account`]), and its frames' together. Every runner of tasks (the
/// exec scheduler's, the daemon's plane) runs a task's work through this, so a task's memory
/// has one home whoever runs it. Says its global-heap churn too ([`TaskSpent::heap`]), for its
/// `task` span.
pub fn run_as<R>(kind: TaskKind, f: impl FnOnce() -> R) -> (R, TaskSpent) {
    let tag = tag_of(kind);
    let ((out, heap), frames) = blaze_mem::frame::measure(|| {
        crate::arena::measured(|| blaze_mem::frame::task(tag, |_| f()))
    });
    (out, TaskSpent { peak: heap.peak + frames as u64, heap })
}

/// What a task spent ([`run_as`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskSpent {
    /// The most it held at once: its global allocations, its fan-out's included, and its
    /// frames' together.
    pub peak: u64,
    /// Its global allocations alone ([`crate::arena::Spent`]): their peak, and what it
    /// allocated and freed in all.
    pub heap: crate::arena::Spent,
}

/// What a node is predicted to cost before it runs: its [`Spec::cost`](crate::Spec::cost) for
/// the plane's critical-path order, when the predictor knows one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    /// The node's cost, in the predictor's one unit for every node of a plane; `None` keeps
    /// the spec's.
    pub cost: Option<u64>,
}

impl Prediction {
    /// `spec` with the prediction in: its cost the prediction's when it has one.
    pub fn apply(self, spec: &mut crate::Spec) {
        if let Some(cost) = self.cost {
            spec.cost = cost;
        }
    }
}

/// What the plane asks for a node's [`Prediction`] and tells what the node then did. A
/// prediction orders work only: it never changes what is built, nor when a task may start.
pub trait Predict: Send {
    /// What a task of `kind` with `key` is predicted to cost: its key's own record, or, with
    /// none, its kind's prediction (what one ask with no key says), so one ask a node is enough.
    fn predict(&self, kind: TaskKind, key: Option<crate::Key>) -> Prediction;
    /// A task of `kind` with `key` ran for `ran_ns` of its own (not its waits) and held `peak`
    /// bytes at most.
    fn observe(&mut self, kind: TaskKind, key: Option<crate::Key>, ran_ns: u64, peak: u64);
    /// What it learned, as bytes for the daemon's image.
    fn encode(&self) -> Vec<u8>;
    /// Whether it has measured a task of `kind` with `key` itself, not only others of its
    /// kind. A predictor that keeps no keys says every task is.
    fn measured(&self, _kind: TaskKind, _key: Option<crate::Key>) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each range of `blaze_query::kinds` and each kind in use goes to its subsystem: the
    /// inputs, the facts, name resolution's kinds (`hir-def`'s `0x2200..`), codegen's
    /// (`blaze-codegen`'s `0x4000..`), the units' loads and the daemon's own.
    #[test]
    fn each_kind_belongs_to_its_subsystem() {
        let cases = [
            (1, Tag::Vfs),
            (3, Tag::Vfs),
            (4, Tag::Query),
            (5, Tag::Vfs),
            (6, Tag::Vfs),
            (0x1000, Tag::Nameres),
            (0x1fff, Tag::Nameres),
            (0x2000, Tag::Parse),
            (0x2200, Tag::Nameres),
            (0x2217, Tag::Nameres),
            (0x2400, Tag::Infer),
            (0x2c00, Tag::Expand),
            (0x3000, Tag::Mir),
            (0x3fff, Tag::Mir),
            (0x4000, Tag::Codegen),
            (0x4006, Tag::Codegen),
            (0x4800, Tag::Link),
            (0x5000, Tag::Usercode),
            (0x6000, Tag::Vfs),
            (0x601d, Tag::Vfs),
            (0x1_0001, Tag::Store),
            (0x1_0004, Tag::Codegen),
            (0x1_0008, Tag::Mir),
            (0x1_0009, Tag::Codegen),
            (0x1_000a, Tag::Nameres),
            (0x1_000b, Tag::Infer),
            (0x1_000c, Tag::Link),
            (0x1_000d, Tag::Store),
            (0x1_000e, Tag::Link),
            (0x1_0010, Tag::Other),
            (0, Tag::Other),
            (0x7000, Tag::Other),
            (u32::MAX, Tag::Other),
        ];
        for (kind, tag) in cases {
            assert_eq!(tag_of(TaskKind(kind)), tag, "kind {kind:#x}");
        }
        assert_eq!(tag_of(kinds::PLAN), Tag::Vfs);
        assert_eq!(tag_of(kinds::WALK), Tag::Query);
        assert_eq!(name_of(TaskKind(0x1_0010)), None);
    }
}
