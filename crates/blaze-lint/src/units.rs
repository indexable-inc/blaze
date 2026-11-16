//! A file's lint units, from its outline (docs/v2/plan/design/lint.md, "What the driver calls"):
//! unit 0 is the file, with its top-level items as holes; unit `i + 1` is the outline's entry
//! `i`, with its direct children as holes. A driver plans a file once, then lints each unit in
//! pre-order with the scope its parent's pass gave it ([`Units::lint_all`] does that in one
//! call, for a caller that has no per-unit records yet).

use blaze_own::{AVec, Alloc};
use blaze_surface::Item;

use crate::{Findings, LintError, Run, RuleSet, Scope, Span, TestBits, Unit, count, lint};

/// One unit: its range of the file, and its children (in [`Units::holes`] and
/// [`Units::children`], the same run of both).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitPlan {
    pub base: u32,
    pub end: u32,
    pub children: Run,
}

/// Every unit of one file, as columns in the caller's allocator.
pub struct Units<'a> {
    pub list: AVec<'a, UnitPlan>,
    /// Each unit's children's ranges, relative to the unit's `base`, in order.
    pub holes: AVec<'a, Span>,
    /// Each unit's children's unit numbers, beside [`Units::holes`].
    pub children: AVec<'a, u32>,
}

/// What [`Units::lint_all`] tells of one unit as it goes.
#[derive(Clone, Copy, Debug)]
pub struct UnitLinted {
    pub unit: usize,
    pub scope: Scope,
    /// The unit's findings, in the [`Findings`] given.
    pub findings: Run,
    /// The test code the unit's last byte is in.
    pub test_at_end: TestBits,
}

impl<'a> Units<'a> {
    /// The units of a file of `len` bytes whose outline's entries are `items` (in pre-order,
    /// each `end` one past its last descendant, as blaze-file-items writes them).
    pub fn plan(items: &[Item], len: usize, alloc: Alloc<'a>) -> Result<Units<'a>, LintError> {
        let len = u32::try_from(len).map_err(|_| LintError::TooLong { len })?;
        let mut units = Units {
            list: AVec::with_capacity_in(items.len() + 1, alloc),
            holes: AVec::with_capacity_in(items.len(), alloc),
            children: AVec::with_capacity_in(items.len(), alloc),
        };
        // Unit 0 walks the top-level entries; unit `i + 1` the entries from `i + 1` to its
        // `end`, stepping over each child's descendants. Every entry is visited once as a
        // child, so the walks together are bounded by the number of entries.
        for unit in 0..=items.len() {
            let (base, end, mut next, stop) = match unit {
                0 => (0, len, 0, items.len()),
                _ => {
                    let item = &items[unit - 1];
                    (item.range.start, item.range.end, unit, item.end.at().min(items.len()))
                }
            };
            let from = count(units.holes.len())?;
            while next < stop {
                let child = &items[next];
                let start = child.range.start.checked_sub(base);
                let finish = child.range.end.checked_sub(base);
                let (Some(start), Some(finish)) = (start, finish) else {
                    return Err(LintError::BadHole { index: next, hole: Span::new(child.range.start, child.range.end) });
                };
                units.holes.push(Span::new(start, finish));
                units.children.push(count(next + 1)?);
                // An entry's `end` is past itself; one that is not would loop, so it is refused.
                let after = child.end.at();
                if after <= next {
                    return Err(LintError::BadHole { index: next, hole: Span::new(start, finish) });
                }
                next = after;
            }
            let to = count(units.holes.len())?;
            units.list.push(UnitPlan { base, end, children: Run { from, to } });
        }
        Ok(units)
    }

    pub fn holes_of(&self, unit: usize) -> &[Span] {
        &self.holes[self.list[unit].children.range()]
    }

    pub fn children_of(&self, unit: usize) -> &[u32] {
        &self.children[self.list[unit].children.range()]
    }

    /// Unit `at` of `text` (the file the units were planned from), in `scope`.
    pub fn unit<'t>(&'t self, at: usize, text: &'t str, scope: Scope) -> Unit<'t> {
        let plan = self.list[at];
        Unit { text: &text[plan.base as usize..plan.end as usize], holes: self.holes_of(at), scope }
    }

    /// Lints every unit of `text` in pre-order, the root in `file`'s scope and each child in the
    /// scope its parent's pass gave it; `each` sees every unit as it is done. The scopes are
    /// kept in `scratch` (emptied first), in `alloc` as the passes' own scratch is.
    pub fn lint_all<'f>(
        &self,
        rules: &RuleSet,
        text: &str,
        file: Scope,
        alloc: Alloc<'f>,
        out: &mut Findings<'f>,
        scratch: &mut AVec<'_, Scope>,
        mut each: impl FnMut(UnitLinted),
    ) -> Result<(), LintError> {
        scratch.clear();
        scratch.resize(self.list.len(), file);
        for at in 0..self.list.len() {
            let scope = scratch[at];
            let linted = lint(rules, &self.unit(at, text, scope), alloc, out)?;
            for (child, given) in self.children_of(at).iter().zip(linted.children.iter()) {
                scratch[*child as usize] = *given;
            }
            each(UnitLinted { unit: at, scope, findings: linted.findings, test_at_end: linted.test_at_end });
        }
        Ok(())
    }
}
