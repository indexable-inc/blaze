//! The seam between the driver and the walk of a program (command.md, "Code"): what a request
//! that builds programs is given, how the driver is driven by blaze-program's walk without a
//! second walk, and what the link step is given.
//!
//! **The walk has one home**, blaze-program's `walk::Walk` with its row rule. The driver is
//! generic over a [`Walker`] (rule 78: a strategy is a type parameter at one seam), which
//! blaze-program implements over its walk; the driver never decides what a program reaches. Not
//! chosen: the driver emitting events its host feeds to a walk. Then the host would carry the
//! program's state between two machines, the simulation would hold a second orchestration to a
//! fresh build, and a plan's lookup (an answer, a read of its address, a demanded body) would
//! cross the host twice per ask. With the trait, the walk's asks are effects of the one driver,
//! in the one request, and the simulation drives both with a model walker.
//!
//! The protocol, per program `p` of the request ([`ProgramPlan`]), once every unit is complete:
//! 1. The driver reads `p`'s kept manifest (the certificate's list names it), answers every fact
//!    its plans read now, re-examines the plans that read a moved one (each found by its address,
//!    or run), and calls [`Walker::begin`] with the kept rows and the instances whose plan's
//!    result moved.
//! 2. It asks [`Walker::ask`] until `None`, answering each [`WalkAsk::Plan`] with
//!    [`Walker::planned`] (the plan found or made: its record's payload, the front end's) and
//!    each [`WalkAsk::Code`] with [`Walker::coded`], in any order the answers come in.
//!
//! **What is whose.** A plan's payload is the front end's format (hir-ty-codegen's `bzplan01`,
//! `bzsplan1`): the driver never reads it. The walker reads it, so the walker names each
//! instance (its edges carry the callee's name and ask; a root's and a static's come from the
//! front end when the walker is made) and computes each code key (blaze-command's
//! `code::code_key` over the plan's head and its callees' plans: places by kind, never a
//! callee's key). The driver finds, runs, files and diffs.
//! 3. With nothing asked and nothing out, [`Walker::end`] gives the rows the walk made or moved
//!    and the instances it no longer reaches. The driver diffs them against the kept rows and
//!    gives the link step exactly the rows that came, went or moved ([`RowChange`]), once.
//! 4. The span tables every row names (a walked row's from its code ask, [`SpanAt`]; a kept
//!    row's from the kept manifest) are found by the files they read, or made by their owner's
//!    span task; the link step is also given those that came or moved ([`SpanRecordAt`]), and
//!    runs when only they moved.
//!
//! Floors (rule 67): a request whose certificate holds asks nothing of a program. One whose
//! units are all settled by their marks reads the kept manifest once and answers its plans'
//! dictionary once (one binary search per row): no plan, code or link. A one-body edit: that
//! body, the plans whose shape read its [`crate::record::FactKind::Mir`] fact (found by the
//! kept postings), their code, one link step. A comment added above an item: no plan, no
//! code; the span task of each owner in its file (their tables read the file), one link step
//! of the tables whose bytes moved. Past the floor: the kept manifest is one record
//! rewritten whole when a row moves (`O(rows)` bytes; chunked by shard is the missing piece).

#![blaze::sans_io]

use blaze_ids::ContentId;

use crate::{
    driver::TaskFailed,
    kind::{CodeRecord, Key, Namespace, PlanRecord, ProgramManifest, SpanTable, UnitManifest},
    record::{KeptParts, KeptRow, StableId},
};

/// One program a request builds.
#[derive(Clone, Copy, Debug)]
pub struct ProgramPlan {
    /// What the program is (its package, target kind, features, target and profile): the
    /// identity its kept manifest is filed under.
    pub program: ContentId,
    /// The settings that change code, as one id the command makes: in the kept manifest's
    /// identity. The plans' names and the code keys hold them already (the front end's stage).
    pub settings: ContentId,
    pub namespace: Namespace,
    /// The kept manifest the last certified build filed for it: the certificate's list.
    pub last: Option<Key<ProgramManifest>>,
}

/// What a walk asks of the driver. Borrowed from the walker until the driver copies it.
#[derive(Clone, Copy, Debug)]
pub enum WalkAsk<'w> {
    /// The plan named `instance` (`PlanRecordInputs { name }`), whose task is given `bytes`
    /// (the front end's ask: a function's `bzask001`, a static's `bzsask01`, from the caller's
    /// edge or the front end). `None`: an instance the kept manifest holds, asked by the ask its
    /// row keeps ([`Kept::ask_of`]): when nothing it read moved, that is one read of its kept
    /// plan and no task; when it moved, its plan task is given the kept ask. A `None` of an
    /// instance no kept row holds is refused. `body`: the item whose body record the plan
    /// reads, by its fact subject (as [`crate::record::mir_id`] takes it), given to the task
    /// beside the ask; `None` for an instance with no body (drop glue, a shim, a static).
    /// Answered by [`Walker::planned`] with `ask`.
    Plan { ask: u32, instance: Key<PlanRecord>, bytes: Option<&'w [u8]>, body: Option<StableId> },
    /// The code of `instance` (a plan ask was answered with it), keyed `key`, which the walker
    /// computed from the plan and its callees' plans, at `tier` (blaze-program's `Tier` by its
    /// number, which `key` holds too; kept in the row for the tier-up). `callees` are the
    /// instances it calls and `statics` those it reads the address of: the code task reads the
    /// plans of both for its places, each answered by a plan ask of this walk or a kept row
    /// neither asked nor moved. They are the row's targets in the kept manifest, apart. Answered
    /// by [`Walker::coded`] with `ask`. `spans`: the span tables the row's code can name (sorted
    /// by anchor, each once), which the walker reads off the instance's plan head as it reads
    /// its edges, with no database: the plan head lists its own anchors and those of every
    /// body its code inlines (the plan read those bodies), each with its owner, and the walker
    /// keys each ([`crate::kind::SpanTableInputs`] with the toolchain), as it keys code. The
    /// driver finds each table by address, or runs its owner's span task.
    Code {
        ask: u32,
        instance: Key<PlanRecord>,
        key: Key<CodeRecord>,
        tier: u32,
        callees: &'w [Key<PlanRecord>],
        statics: &'w [Key<PlanRecord>],
        spans: &'w [SpanAt],
    },
}

/// One span table a row's code can name: its anchor, its key, and its owner (the item, by its
/// item key, whose span task makes it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanAt {
    pub anchor: ContentId,
    pub key: Key<SpanTable>,
    pub owner: StableId,
}

/// A span table the link step is given, by its anchor and the address its record is filed at:
/// the host reads the record and hands its payload on, sorted by anchor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanRecordAt {
    pub anchor: ContentId,
    pub address: Key<SpanTable>,
}

/// A plan found or made, as the walker is given it.
#[derive(Clone, Copy, Debug)]
pub struct Planned<'a> {
    /// The instance key: the plan's name.
    pub instance: Key<PlanRecord>,
    /// The plan record's result digest.
    pub result: ContentId,
    /// The plan record's payload: the front end's plan, read by the walker.
    pub payload: &'a [u8],
}

/// A plan record a task or the link step reads, by its instance and the address it is filed
/// at: the host reads it from the store and hands its bytes on, sorted by instance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlanAt {
    pub instance: Key<PlanRecord>,
    pub address: ContentId,
}

/// A program's kept rows as the walker's prior reads them.
#[derive(Clone, Copy, Debug)]
pub struct Kept<'a> {
    pub rows: &'a [KeptRow],
    pub targets: &'a [ContentId],
    pub asks: &'a [u8],
}

impl<'a> Kept<'a> {
    pub fn of(parts: &KeptParts<'a>) -> Kept<'a> {
        Kept { rows: parts.rows, targets: parts.targets, asks: parts.asks }
    }

    /// The kept row of `instance` (one binary search).
    pub fn row(&self, instance: &ContentId) -> Option<&'a KeptRow> {
        self.rows.binary_search_by(|row| row.instance.cmp(instance)).ok().map(|at| &self.rows[at])
    }

    /// What `row` refers to: its callees' instance keys, then its statics'.
    pub fn targets_of(&self, row: &KeptRow) -> &'a [ContentId] {
        &self.targets[row.target_at as usize..(row.target_at + row.target_len) as usize]
    }

    /// The instances `row` calls, sorted.
    pub fn callees_of(&self, row: &KeptRow) -> &'a [ContentId] {
        &self.targets_of(row)[..row.call_len as usize]
    }

    /// The statics `row` reads the address of, sorted.
    pub fn statics_of(&self, row: &KeptRow) -> &'a [ContentId] {
        &self.targets_of(row)[row.call_len as usize..]
    }

    /// The front end's ask `row`'s plan task was given.
    pub fn ask_of(&self, row: &KeptRow) -> &'a [u8] {
        &self.asks[row.ask_at as usize..(row.ask_at + row.ask_len) as usize]
    }
}

/// One row a walk made or moved: an instance and its code.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Row {
    pub instance: Key<PlanRecord>,
    pub code: Key<CodeRecord>,
}

/// What an ended walk gives: the rows it walked (sorted by instance, each once) and the kept
/// instances it no longer reaches (sorted).
#[derive(Clone, Copy, Debug)]
pub struct WalkEnd<'w> {
    pub rows: &'w [Row],
    pub gone: &'w [Key<PlanRecord>],
}

/// One row the link step is given: an instance that came (`was` none), went (`now` none), or
/// whose code moved. In instance order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowChange {
    pub instance: Key<PlanRecord>,
    pub was: Option<Key<CodeRecord>>,
    pub now: Option<Key<CodeRecord>>,
}

/// The driver's strategy for walking programs: blaze-program implements it over its
/// `walk::Walk` (its `Prior` from [`Kept`], its asks as [`WalkAsk`]); the simulation over a
/// model walk. Called from the driver's thread only; nothing here blocks.
///
/// The contract the driver relies on (and the simulation holds a model to):
/// - `ask` gives each ask once; `None` means nothing more until an answer comes, and once every
///   ask is answered and `ask` gives `None`, the walk has ended and `end` gives its rows.
/// - What it asks, and the rows it ends with, depend only on the kept rows, the moved instances
///   and the answers: never on their order.
/// - Every instance of `end`'s rows was asked by a plan ask of this walk, and its code by a code
///   ask; a failure is a [`TaskFailed`] naming the rule, which fails this program alone.
/// - It asks the plan of each `moved` instance it still reaches (a moved plan's edges and code
///   are only known by asking), and asks code of every instance whose row it gives. A kept row it
///   neither gives nor names as gone stays as kept: so it gives as gone every kept instance it
///   no longer reaches. An instance it asks that did not move costs one read of its kept plan.
/// - A kept row carries its ask, so a kept instance is asked with `bytes: None`, moved or not:
///   the walker need not wait for a caller's plan to name it. A new instance (no kept row) is
///   asked with the ask of the edge that reaches it.
/// - A code ask comes after a plan ask of its instance was answered, and each of its `callees`
///   and `statics` is an instance whose plan ask this walk had answered, or a kept row neither
///   asked nor moved: the driver gives the code task their plans, so a walker asks code once its
///   callees' plans are in (a cycle is planned whole first, then coded). A walker whose code
///   key needs a kept callee's plan (its place kind) asks that plan with `bytes: None`: one read
///   of its kept address, no task.
pub trait Walker {
    /// Starts program `program`'s walk over its kept rows (empty for a first build) with the
    /// kept instances whose plan's result moved, sorted. `units`: every unit's manifest as this
    /// request settled it, in plan order (programs begin only once every unit has): what a
    /// walker whose roots are listed over the units' surfaces lists them from, so the units are
    /// keyed once in the one request that builds the program (rule 77).
    fn begin(&mut self, program: u32, kept: Kept<'_>, moved: &[Key<PlanRecord>], units: &[Key<UnitManifest>]) -> Result<(), TaskFailed>;
    /// The next ask of program `program`'s walk.
    fn ask(&mut self, program: u32) -> Option<WalkAsk<'_>>;
    fn planned(&mut self, program: u32, ask: u32, plan: Planned<'_>) -> Result<(), TaskFailed>;
    fn coded(&mut self, program: u32, ask: u32, code: Key<CodeRecord>) -> Result<(), TaskFailed>;
    /// The walk's end, once nothing is asked and nothing is out.
    fn end(&mut self, program: u32) -> Result<WalkEnd<'_>, TaskFailed>;
}

/// The walker of a request that builds no program (`check`): never asked.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoWalk;

impl Walker for NoWalk {
    fn begin(&mut self, _program: u32, _kept: Kept<'_>, _moved: &[Key<PlanRecord>], _units: &[Key<UnitManifest>]) -> Result<(), TaskFailed> {
        Err(TaskFailed::Refused { what: "a program walked by a request given no walker" })
    }

    fn ask(&mut self, _program: u32) -> Option<WalkAsk<'_>> {
        None
    }

    fn planned(&mut self, _program: u32, _ask: u32, _plan: Planned<'_>) -> Result<(), TaskFailed> {
        Err(TaskFailed::Refused { what: "a plan answered to a request given no walker" })
    }

    fn coded(&mut self, _program: u32, _ask: u32, _code: Key<CodeRecord>) -> Result<(), TaskFailed> {
        Err(TaskFailed::Refused { what: "code answered to a request given no walker" })
    }

    fn end(&mut self, _program: u32) -> Result<WalkEnd<'_>, TaskFailed> {
        Ok(WalkEnd { rows: &[], gone: &[] })
    }
}

/// The rows the link step is given: each of `walked` whose code is not the kept row's (came, or
/// moved), and each of `gone` the kept rows hold (went), in instance order. `kept` and `walked`
/// sorted by instance, `gone` sorted. Floor: one merge, `O(walked + gone)` binary searches.
pub fn changes<E: Extend<RowChange>>(kept: Kept<'_>, walked: &[Row], gone: &[Key<PlanRecord>], out: &mut E) {
    let (mut w, mut g) = (0, 0);
    while w < walked.len() || g < gone.len() {
        let take_walked = match (walked.get(w), gone.get(g)) {
            (Some(row), Some(went)) => row.instance.id() <= went.id(),
            (Some(_), None) => true,
            _ => false,
        };
        if take_walked {
            let row = walked[w];
            w += 1;
            let was = kept.row(&row.instance.id()).map(|kept| Key::<CodeRecord>::from_id(kept.code));
            let moved = was.is_some_and(|was| was != row.code);
            if was.is_none() || (moved && !crate::planted(crate::Plant::LinkMissesMovedRow)) {
                out.extend([RowChange { instance: row.instance, was, now: Some(row.code) }]);
            }
        } else {
            let went = gone[g];
            g += 1;
            if let Some(kept) = kept.row(&went.id()) {
                out.extend([RowChange { instance: went, was: Some(Key::from_id(kept.code)), now: None }]);
            }
        }
    }
}
