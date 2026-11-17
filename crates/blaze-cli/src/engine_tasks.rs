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
use blaze_engine::{Buffer, Done, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::cx::Cx;

use crate::work::{self, Record};

/// The cell of a record read whole: the one cell an outline defines today.
pub(crate) const WHOLE: Cell = Cell([0; 16]);

/// One file the planner outlined, by its `FileItems` name.
pub(crate) struct Outlined<'h> {
    pub(crate) name: Name,
    /// Its path in the checkout, as read (`sweep::real_path`).
    pub(crate) path: SVec<'h, u8>,
    /// The content id the sweep gave its bytes: the task's source.
    pub(crate) source: ContentId,
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
            // The units and names stages run on the coordinator (engine_units.rs,
            // engine_names.rs), never through this runner.
            Stage::Units | Stage::Names | Stage::Heads | Stage::Bodies => failed(task, alloc),
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

/// A failed result: no cell, one finding (the failure itself), the failure's out.
pub(crate) fn failed<'a>(task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
    Done {
        name: task.planned.name,
        key: task.key,
        out: work::failed_out(),
        defines: SVec::new_in(alloc),
        reads: SVec::new_in(alloc),
        findings: 1,
        failed: true,
    }
}
