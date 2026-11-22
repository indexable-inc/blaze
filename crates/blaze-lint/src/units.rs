//! A file's lint units, from its outline (docs/v2/plan/design/lint.md, "What the driver calls"):
//! unit 0 is the file, with its top-level items as holes; unit `i + 1` is the outline's entry
//! `i`, with its direct children as holes. A driver plans a file once, then lints each unit in
//! pre-order with the scope its parent's pass gave it ([`Units::lint_all`] does that in one
//! call, for a caller that has no per-unit records yet).

//!
//! A unit is linted as its OWN text ([`Units::own`]): its bytes with each child's bytes replaced
//! by [`HOLE_MARK`], so what a pass reads is exactly what the unit is, never a child's bytes or
//! length. Its findings' places are places of that own text ([`own_place`]); a printer makes them
//! places of the unit's text today by giving each child before them its length again
//! ([`real_place`], from today's outline), so a child that grows moves no stored place (rule 71).

use blaze_own::{AString, AVec, Alloc};
use blaze_surface::Item;

use crate::{Findings, LintError, Rule, Run, RuleSet, Scope, Scratch, Span, TestBits, Unit, Variants, count, lint_in_scratch};

/// What a child item is in its parent's own text ([`Units::own`]): two bytes that are no token
/// of Rust's (the lex skips a hole) and end a line's code with `}` as a child's own last line
/// does, with no newline, so the walks up a unit's lines (comment marks above a site) see the
/// line a child ends on as they saw its `}` before: a line of code that stops them. A child's
/// kind is not marked: no rule reads it (a pass sees a hole as one token, `K::Hole`).
pub const HOLE_MARK: &str = "{}";

/// [`HOLE_MARK`]'s length.
pub const HOLE_MARK_LEN: u32 = HOLE_MARK.len() as u32;

/// One unit's own text ([`Units::own`]): its bytes, each child's replaced by [`HOLE_MARK`], and
/// the marks' places in it (own-relative, in order). What its key hashes and its pass reads.
pub struct OwnUnit<'a> {
    pub text: AString<'a>,
    pub holes: AVec<'a, Span>,
}

impl OwnUnit<'_> {
    /// The unit to lint, in `scope`.
    pub fn unit(&self, scope: Scope) -> Unit<'_> {
        Unit { text: self.text.as_str(), holes: &self.holes, scope }
    }
}

/// `real`, a place of a unit's text (unit-relative) outside its children `holes` (unit-relative,
/// in order), as a place of its own text: each child before it is [`HOLE_MARK_LEN`] bytes. The
/// one map from a unit's text to its own (rule 30): `variant-count`'s sites use it.
pub fn own_place(holes: &[Span], real: u32) -> u32 {
    let (mut cut, mut marks) = (0u32, 0u32);
    for hole in holes {
        if hole.end > real {
            break;
        }
        cut = cut.saturating_add(hole.end.saturating_sub(hole.start));
        marks = marks.saturating_add(HOLE_MARK_LEN);
    }
    real.saturating_sub(cut).saturating_add(marks)
}

/// `own`, a place of a unit's own text ([`Units::own`]), as a place of the unit's text whose
/// children are `holes` (unit-relative, in order: today's, from today's outline): each child's
/// mark before it its length again. A place inside a mark is the child's end. `None` when the
/// place is past what a `u32` counts (a stale place, never clamped: rule 3). [`own_place`]'s
/// inverse: `real_place(h, own_place(h, r)) == Some(r)` for every `r` outside the holes.
pub fn real_place(holes: impl IntoIterator<Item = Span>, own: u32) -> Option<u32> {
    let own = i64::from(own);
    // Today's place minus the own place, for the marks passed so far.
    let mut shift = 0i64;
    for hole in holes {
        let mark = i64::from(hole.start) - shift;
        if own >= mark + i64::from(HOLE_MARK_LEN) {
            shift += i64::from(hole.end) - i64::from(hole.start) - i64::from(HOLE_MARK_LEN);
            continue;
        }
        if own > mark {
            return Some(hole.end);
        }
        break;
    }
    u32::try_from(own + shift).ok()
}

/// [`real_place`] of both ends of `own`.
pub fn real_range(holes: &[Span], own: Span) -> Option<Span> {
    Some(Span::new(real_place(holes.iter().copied(), own.start)?, real_place(holes.iter().copied(), own.end)?))
}

/// The direct children of one unit of a file's outline entries `items` (in pre-order, each
/// `end` one past its last descendant): each child's entry index and its range relative to the
/// unit's start, in order. The one walk [`Units::plan`] plans a unit's holes by, and the one a
/// printer gives a stored own place its children's lengths by ([`real_place`]) without planning
/// every unit. An entry out of order stops the walk and is kept in [`ChildHoles::bad`]: the
/// caller refuses it by name (rule 3), never takes the holes before it as all of them.
pub struct ChildHoles<'i> {
    items: &'i [Item],
    base: u32,
    next: usize,
    stop: usize,
    /// Why the walk stopped early, if it did.
    pub bad: Option<LintError>,
}

impl<'i> ChildHoles<'i> {
    /// Unit `unit`'s children (unit 0 the file's top-level entries, unit `i + 1` entry `i`'s).
    /// A unit past the entries has none.
    pub fn of(items: &'i [Item], unit: usize) -> ChildHoles<'i> {
        let (base, next, stop) = match unit.checked_sub(1) {
            None => (0, 0, items.len()),
            Some(entry) => match items.get(entry) {
                Some(item) => (item.range.start, unit, item.end.at().min(items.len())),
                None => (0, 0, 0),
            },
        };
        ChildHoles { items, base, next, stop, bad: None }
    }
}

impl Iterator for ChildHoles<'_> {
    type Item = (usize, Span);

    fn next(&mut self) -> Option<(usize, Span)> {
        if self.next >= self.stop || self.bad.is_some() {
            return None;
        }
        let at = self.next;
        let child = &self.items[at];
        let (Some(start), Some(finish)) = (child.range.start.checked_sub(self.base), child.range.end.checked_sub(self.base)) else {
            self.bad = Some(LintError::BadHole { index: at, hole: Span::new(child.range.start, child.range.end) });
            return None;
        };
        // An entry's `end` is past itself; one that is not would loop, so it is refused.
        let after = child.end.at();
        if after <= at {
            self.bad = Some(LintError::BadHole { index: at, hole: Span::new(start, finish) });
            return None;
        }
        self.next = after;
        Some((at, Span::new(start, finish)))
    }
}

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
            let (base, end) = match unit {
                0 => (0, len),
                _ => (items[unit - 1].range.start, items[unit - 1].range.end),
            };
            let from = count(units.holes.len())?;
            let mut children = ChildHoles::of(items, unit);
            for (entry, hole) in children.by_ref() {
                units.holes.push(hole);
                units.children.push(count(entry + 1)?);
            }
            if let Some(bad) = children.bad {
                return Err(bad);
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

    /// Unit `at`'s own text ([`OwnUnit`]) of `text` (the file the units were planned from), in
    /// `alloc`: its bytes between its children, each child one [`HOLE_MARK`]. A hole out of
    /// order, past the unit or inside a character is refused by name (rule 3).
    pub fn own<'b>(&self, at: usize, text: &str, alloc: Alloc<'b>) -> Result<OwnUnit<'b>, LintError> {
        own_text(self.list[at], self.holes_of(at), text, alloc)
    }

    /// Unit `at`'s parent unit: `None` for the file's (unit 0). From the outline's entries the
    /// units were planned from (`items`).
    pub fn parent_of(items: &[Item], at: usize) -> Option<usize> {
        match at.checked_sub(1) {
            None => None,
            Some(entry) => Some(items[entry].parent.map_or(0, |up| up.at() + 1)),
        }
    }
}

/// The children of the unit `plan` among a file's units' `holes` ([`Units::holes`]): what
/// [`Units::holes_of`] answers, from the columns alone (borrowed slices a worker can share).
pub fn holes_in<'h>(plan: UnitPlan, holes: &'h [Span]) -> &'h [Span] {
    &holes[plan.children.from as usize..plan.children.to as usize]
}

/// The own text ([`OwnUnit`]) of the unit `plan` of `text` with children `holes`
/// ([`holes_in`]), in `alloc`: [`Units::own`] from the columns alone.
pub fn own_text<'b>(plan: UnitPlan, holes: &[Span], text: &str, alloc: Alloc<'b>) -> Result<OwnUnit<'b>, LintError> {
    let whole = text.get(plan.base as usize..plan.end as usize).ok_or(LintError::BadHole { index: 0, hole: Span::new(plan.base, plan.end) })?;
    let cut: usize = holes.iter().map(|hole| hole.end.saturating_sub(hole.start) as usize).sum();
    let mut own = AString::with_capacity_in(whole.len().saturating_sub(cut) + holes.len() * HOLE_MARK.len(), alloc);
    let mut marks = AVec::with_capacity_in(holes.len(), alloc);
    let mut from = 0usize;
    for (index, hole) in holes.iter().enumerate() {
        let piece = whole.get(from..hole.start as usize).ok_or(LintError::BadHole { index, hole: *hole })?;
        own.push_str(piece);
        let start = count(own.len())?;
        own.push_str(HOLE_MARK);
        marks.push(Span::new(start, start + HOLE_MARK_LEN));
        from = hole.end as usize;
    }
    let rest = whole.get(from..).ok_or(LintError::BadHole { index: holes.len(), hole: Span::new(count(from)?, count(whole.len())?) })?;
    own.push_str(rest);
    count(own.len())?;
    Ok(OwnUnit { text: own, holes: marks })
}

impl<'a> Units<'a> {
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
        // The file's one fact `variant-count` reads, gathered once for all its units.
        let variants = match rules.runs(Rule::VariantCount) {
            true => Variants::gather(text, alloc)?,
            false => Variants::new_in(alloc),
        };
        let mut pass_scratch = Scratch::new_in(alloc);
        for at in 0..self.list.len() {
            let scope = scratch[at];
            let done = lint_in_scratch(rules, &self.unit(at, text, scope), &variants, self.list[at].base, &mut pass_scratch, out)?;
            for (child, given) in self.children_of(at).iter().zip(pass_scratch.children().iter()) {
                scratch[*child as usize] = *given;
            }
            each(UnitLinted { unit: at, scope, findings: done.findings, test_at_end: done.test_at_end });
        }
        Ok(())
    }
}
