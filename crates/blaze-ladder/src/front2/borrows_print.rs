//! The move and borrow check of one body, PRINTED: the flow `blaze check` lowers it to
//! (blaze-flow `Flow::print`: every statement with the `subset`s added at it, `from: into`),
//! then the check's findings, one per line. The same path as [`super::borrows`]'s
//! `check_borrows` (the type check's `Checked` record, [`gather_facts`], blaze-flow-lower
//! `lower`, blaze-flow `check`), for diagnosing a finding from the flow before changing
//! anything (flow.md, "For a successor"; piece BR4). Not used by the command.

use blaze_flow_lower::lower;
use blaze_own::Alloc;
use core::fmt::Write;

use super::borrows::{BorrowLangs, gather_facts};
use super::project::{BodyRef, ProjectWorld};
use super::project_records::BodySource;

/// Why a body's flow was not printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotPrinted {
    /// Its types were not checked clean (`ProjectWorld::check`'s verdict says why).
    Untyped,
    /// An ADT's `Copy` or `Drop` answer was not known: the command refuses such a body.
    FactUnknown { adt: u128, fact: &'static str },
    /// The body's type table could not be viewed.
    TypeTable,
    /// The lowering refused a form, by name.
    Lower(blaze_flow_lower::Refusal),
    /// The checker passed its bound.
    Bound(blaze_flow::Bound<blaze_typeck::ItemKey>),
    /// The writer `out` failed.
    Write,
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// Writes `body`'s flow and its findings to `out`: the lines `flow:`, the flow, `stats:`
    /// and the lowering's counts, then `finding: <Debug>` per finding, then `findings N`.
    pub fn print_borrows(&mut self, body: BodyRef<'n>, alloc: Alloc<'_>, out: &mut impl Write) -> Result<(), NotPrinted> {
        let mut result: Result<(), NotPrinted> = Err(NotPrinted::Untyped);
        let _verdict = self.check_then(body, super::stalled_goals::Stalls::Gap, alloc, |r, lowered, done| {
            result = (|| {
                if !done.diags.is_empty() {
                    return Err(NotPrinted::Untyped);
                }
                let facts = gather_facts(r, lowered.owner, done, BorrowLangs::of(r.index), alloc);
                // Every fn's signature regions are its head record's, read in place (D148).
                let reader = r.store.regions.reader(r.store.tys.alloc());
                let facts = facts.read_by(&reader, r.store.tys.view());
                if let Some(&(adt, fact)) = facts.facts.unknown().first() {
                    return Err(NotPrinted::FactUnknown { adt, fact });
                }
                let tys = lowered.tys.view(&r.store.tys).map_err(|_| NotPrinted::TypeTable)?;
                let view = lowered.typeck.view(tys);
                let l = lower(&view, done, &facts, alloc).map_err(NotPrinted::Lower)?;
                writeln!(out, "flow:").map_err(|_| NotPrinted::Write)?;
                l.flow.print(out).map_err(|_| NotPrinted::Write)?;
                writeln!(out, "stats: {:?}", l.stats).map_err(|_| NotPrinted::Write)?;
                let record = blaze_flow::check_named(view.owner, &l.flow, alloc).map_err(NotPrinted::Bound)?;
                for m in record.moves.iter() {
                    writeln!(out, "finding: {m:?}").map_err(|_| NotPrinted::Write)?;
                }
                for b in record.borrows.iter() {
                    writeln!(out, "finding: {b:?}").map_err(|_| NotPrinted::Write)?;
                }
                writeln!(out, "findings {}", record.moves.len() + record.borrows.len()).map_err(|_| NotPrinted::Write)?;
                Ok(())
            })();
        });
        result
    }
}
