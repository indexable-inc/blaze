//! The engine's tasks for `blaze engine-check` (engine.md E6): what one entered task does, by its
//! stage, writing its result into its worker's buffer.
//!
//! Outlines: the planner outlines every file before the first phase (invariant 1 of engine.rs:
//! an edit holds every planned task of each changed file, and those come from the outline), so
//! the `Outlines` task publishes the outline the planner made: its out, its one whole cell, its
//! findings' count. The task is still entered once, so entries equal tasks.
//!
//! A panic in a task fails that task alone (rule 20): a failed result, which defines nothing and
//! prints as a failure, and the phase goes on.

use blaze_drive::cells::Cell;
use blaze_drive::{Name, Why};
use blaze_engine::{Buffer, CellId, Done, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::cx::Cx;

use crate::work::{self, Record};

/// The cell of a record read whole.
pub(crate) const WHOLE: Cell = Cell([0; 16]);

/// An outline's second cell: its file's surface (blaze-file-items `Outline::surface`, the
/// outline record's `Surface` part), what name resolution reads of the file. A comment or a
/// body's statement moves the outline's [`WHOLE`] cell and leaves this one, so a crate's names,
/// which read only this, are not entered for it.
pub(crate) const SURFACE: Cell = Cell([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// An outline's third cell: its file's heads, every `Signature` part of its record (signature.rs:
/// each entry's head bytes and the file's surface) hashed in the record's order. INTERIM, until a
/// Heads stage defines one cell per head: a body's read of a head (a `Signature` cell no task
/// defines) is widened to this cell of each file of the head's crate (engine_bodies.rs `widen`),
/// so a signature edit enters every body that read a head of that crate's edited file. Coarse and
/// sound: a comment or a statement in a body moves no head, so it moves nothing here.
pub(crate) const HEADS: Cell = Cell([2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

const HEADS_DOMAIN: &str = "blaze-cli engine outline heads 1";

/// The digest of every `Signature` part of an outline record, in order ([`HEADS`]).
pub(crate) fn heads_of<A: core::alloc::Allocator + Copy>(record: &Record<A>) -> ContentId {
    let mut h = blaze_ids::Hasher::new(HEADS_DOMAIN);
    for part in record.parts.iter() {
        if let (blaze_drive::Kind::Signature, blaze_drive::Locate::Source(source)) = (part.name.kind, part.locate) {
            h.update(&part.name.subject.0).update(&source.0);
        }
    }
    h.finish()
}

/// A `CrateScope` task's mark that it defines every cell of its record's cell table
/// (blaze-scope names_cells.rs, the payload's `cells` section) beside its [`WHOLE`] cell: what
/// lets a body keep a names read as its cell (engine_bodies.rs `widen`). A crate that defines
/// it holds each cell of its table and nothing else, so a cell it does not define is ABSENT by
/// the table's contract (every cell a walk can name that holds something is a row), and adding
/// the name later defines it, which moves it for every reader that saw it absent (rule 21). A
/// crate without the mark (a dependency: no file of the checkout feeds its names, so no edit
/// moves them) defines [`WHOLE`] alone and its readers read it whole. Its digest is the cell
/// table's own, so a reader never needs it as a read. (A cell of a `CrateScope` name, never of an
/// outline's: its byte is not [`HEADS`]'s so the two are not read as one.)
pub(crate) const CELLS: Cell = Cell([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// The surface digest an outline record lists (its `Surface` part's source), or `None` for a
/// record with no surface (a data file).
pub(crate) fn surface_of<A: core::alloc::Allocator + Copy>(record: &Record<A>) -> Option<ContentId> {
    record.parts.iter().find_map(|part| match (part.name.kind, part.locate) {
        (blaze_drive::Kind::Surface, blaze_drive::Locate::Source(source)) => Some(source),
        _ => None,
    })
}

const EDITION_DOMAIN: &str = "blaze-cli engine source with edition 1";

/// A task's source made of a text's digest `text` and the edition `edition` it parses by (rule
/// 21): the `Outlines` task's (its file's bytes) and each `BodyCheck` task's (its item's text,
/// engine_bodies.rs `plan`). An edition edit in a manifest leaves every text and moves this, so
/// the file is outlined and its bodies checked again (`gen` is a name in 2021, a keyword in 2024).
pub(crate) fn edition_source(text: ContentId, edition: u8) -> ContentId {
    let mut h = blaze_ids::Hasher::new(EDITION_DOMAIN);
    h.update(&text.0).update(&[edition]);
    h.finish()
}

/// One file the planner outlined, by its `FileItems` name.
pub(crate) struct Outlined<'h> {
    pub(crate) name: Name,
    /// Its path in the checkout, as read (`sweep::real_path`).
    pub(crate) path: SVec<'h, u8>,
    /// The task's source: the content id the sweep gave its bytes with its edition folded in
    /// ([`edition_source`]).
    pub(crate) source: ContentId,
    /// The edition byte its crate's part gave it: the outline and every body parse by it.
    pub(crate) edition: u8,
    /// Its outline, or why it could not be made.
    pub(crate) made: Result<Record<SharedAlloc<'h>>, Why>,
}

/// What every task reads besides the tables: the planner's outlines, sorted by name.
pub(crate) struct Tasks<'p, 'h> {
    pub(crate) outlined: &'p [Outlined<'h>],
}

impl Tasks<'_, '_> {
    /// Runs `task` into `buffer`, its result's lists in `alloc`; a panic is the task's failure.
    pub(crate) fn work<'a>(&self, _cx: &Cx, _view: View<'_, 'a>, task: &Task, buffer: &mut Buffer<'a>, alloc: SharedAlloc<'a>) {
        let ran = work::unwinding(|| match task.planned.phase.stage {
            Stage::Outlines => self.outline(task, alloc),
            // Not planned yet (engine_check.rs `plan`): a task entered in a stage with no runner
            // here is a failure row, named, never skipped.
            // The units, scripts and names stages run on the coordinator (engine_units.rs,
            // engine_scripts.rs, engine_names.rs), never through this runner.
            Stage::Units | Stage::Scripts | Stage::Names | Stage::Heads | Stage::Bodies | Stage::Lints => failed(task, alloc),
        });
        buffer.done.push(match ran {
            Ok(done) => done,
            Err(work::Panicked) => failed(task, alloc),
        });
    }

    /// The `Outlines` task: the planner's outline of the task's file, published.
    fn outline<'a>(&self, task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
        let Ok(at) = self.outlined.binary_search_by(|one| one.name.cmp(&task.planned.name)) else {
            return failed(task, alloc);
        };
        let Ok(record) = &self.outlined[at].made else {
            return failed(task, alloc);
        };
        let mut defines = SVec::new_in(alloc);
        defines.push((WHOLE, record.out));
        if let Some(surface) = surface_of(record) {
            defines.push((SURFACE, surface));
        }
        defines.push((HEADS, heads_of(record)));
        Done {
            name: task.planned.name,
            key: task.key,
            out: record.out,
            defines,
            reads: SVec::new_in(alloc),
            findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX),
            failed: false,
        }
    }
}

/// A failed result: no cell, one finding (the failure itself), the failure's out. It reads
/// nothing, so it is owed: entered again by every request (a panic, a machine fault, a task with
/// no runner); a refusal that is a function of cells is [`failed_with`]'s (plan row A8).
pub(crate) fn failed<'a>(task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
    failed_with(task, SVec::new_in(alloc), alloc)
}

/// A failed result that stands on what its failure saw (plan row A8): `reads`, each cell with
/// the digest the view held when the task refused (an absent cell read as ABSENT). They are
/// sorted by cell id and kept once each here, whatever order the caller gathered them in, so a
/// read list names each cell once (rules 21, 25). The row stands until one of them moves, and is
/// then entered again once (invariant 7); with no read it is [`failed`]'s, owed. No cell, one
/// finding (the failure itself), the failure's out.
pub(crate) fn failed_with<'a>(task: &Task, reads: SVec<'a, (CellId, ContentId)>, alloc: SharedAlloc<'a>) -> Done<'a> {
    let mut reads = reads;
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    Done {
        name: task.planned.name,
        key: task.key,
        out: work::failed_out(),
        defines: SVec::new_in(alloc),
        reads,
        findings: 1,
        failed: true,
    }
}
