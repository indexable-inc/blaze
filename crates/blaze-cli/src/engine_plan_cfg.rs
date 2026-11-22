//! WHAT PLAN TIME KNOWS OF A UNIT'S CFG (D191, cfg-aware holders).
//!
//! A body's rows are planned before any stage runs (engine_check.rs `plan`; engine_bodies.rs
//! `plan_views`), one for each member unit whose module tree reaches its file (engine_body_plan.rs
//! `Reach`). The outline lists every entry whatever its cfg (it stays cfg-blind in what it lists);
//! what it keeps besides is each `#[cfg]` and `#[cfg_attr]` attribute's own bytes (blaze-surface
//! file_items.rs `AttrRow` 2, [`FileItemsView::cfgs`]). Here those are read against what plan
//! time knows of a unit's cfg set, in Kleene's three values (blaze-expand cfg.rs
//! `cfg_predicate_tri`, the one reading of a predicate):
//!
//! - `test` is the one atom known ([`test_atom`]): `Yes` for a test build (the crate list's row,
//!   blaze-world crate_list.rs `test`: blaze-unit-graph resolve.rs `unit_cfg` sets it); `No` for
//!   any other unit with no build script. A unit WITH a build script has `test` `Unknown`: a
//!   script's `cargo:rustc-cfg` lines are its run's answer (the Scripts stage), which no plan has
//!   before it runs, and a script may print `rustc-cfg=test`.
//! - every other atom (`feature = ".."`, `unix`, `target_os`, a script's cfg) is `Unknown`;
//! - a `#[cfg_attr(..)]` whose attributes name `cfg` is `Unknown` (it is not evaluated at plan
//!   time); one that names none cuts nothing; `cfg!` is an expression, never a holder's.
//!
//! An entry's CUT is the conjunction of its own cfg attributes', each enclosing entry's (an
//! inline `mod`, an `impl`, a trait), and its file's own `#![cfg]`s ([`Cut`]); a file's, for the
//! walk, is its `mod` entry's in the file that names it and its own `#![cfg]`s
//! (engine_body_plan.rs `reached_under`). A unit gets no row of a body whose cut is `No` under
//! its `test`: `No` is what every completion of the unknown atoms gives, so no unit that holds
//! the body loses its row (never a wrong `No`). An `Unknown` keeps the row (the unit's records
//! say, as before D191's cfg-aware holders) and is counted: "undecided at plan time: N"
//! (engine_body_plan.rs `Outside::profile`).
//!
//! WHAT A CUT IS A FUNCTION OF (rule 21): the outline record's bytes (its attributes' text) and
//! each unit's `test` and build script, both the crate list's, which the plan reads whole: an
//! edit of a `#[cfg]` re-plans its file, and its rows' names move with the units that reach it.

use blaze_drive::{Kind, Locate};
use blaze_expand::{Delim, Edition, TokenKind, Tri, cfg_predicate_tri, lex};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{AttrKind, FileItemsView, Item};
use blaze_world::crate_list::CrateList;

use crate::engine_tasks::Outlined;

/// What plan time knows of the `test` atom of crate list row `at`'s cfg set (the module doc).
pub(crate) fn test_atom(list: &CrateList<'_>, at: usize) -> Tri {
    match (list.test(at), list.script(at).is_some()) {
        (true, _) => Tri::Yes,
        (false, false) => Tri::No,
        (false, true) => Tri::Unknown,
    }
}

/// An entry's cfg outcome under each value plan time can give a unit's `test` (the module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Cut {
    no: Tri,
    yes: Tri,
    unknown: Tri,
}

impl Cut {
    /// No cfg: held under every `test`.
    pub(crate) const HOLDS: Cut = Cut { no: Tri::Yes, yes: Tri::Yes, unknown: Tri::Yes };

    /// Every value of `test` gives `outcome` (a predicate that does not read `test`, or one plan
    /// time cannot evaluate).
    const fn all(outcome: Tri) -> Cut {
        Cut { no: outcome, yes: outcome, unknown: outcome }
    }

    /// The outcome for a unit whose `test` is `test` ([`test_atom`]).
    pub(crate) fn under(self, test: Tri) -> Tri {
        match test {
            Tri::No => self.no,
            Tri::Yes => self.yes,
            Tri::Unknown => self.unknown,
        }
    }

    /// Whether a unit whose `test` is `test` holds no row of the entry: its cut is `No`.
    pub(crate) fn cuts(self, test: Tri) -> bool {
        self.under(test) == Tri::No
    }

    /// Both hold.
    fn and(self, other: Cut) -> Cut {
        Cut { no: self.no.and(other.no), yes: self.yes.and(other.yes), unknown: self.unknown.and(other.unknown) }
    }
}

/// The cut of one cfg attribute's text `written` (`#[cfg(..)]`, `#![cfg(..)]` or
/// `#[cfg_attr(..)]`), lexed in `edition`, in `g`. Text that does not read as one (a lex error,
/// no `cfg(..)` group, a malformed predicate) is `Unknown` under every `test`: plan time does not
/// know it, and the unit's names step reports it where it is the crate's to report.
fn attr_cut(kind: AttrKind, written: &str, edition: Edition, g: Alloc<'_>) -> Cut {
    let Ok(tokens) = lex(written, edition, g) else { return Cut::all(Tri::Unknown) };
    let list = tokens.tokens();
    let ident = |at: usize, word: &str| matches!(list.get(at).map(|token| token.kind), Some(TokenKind::Ident { .. })) && tokens.text_at(at) == word;
    if kind == AttrKind::CfgAttr {
        // Not evaluated at plan time: a `cfg` among its attributes may cut the entry.
        return match (0..list.len()).any(|at| ident(at, "cfg")) {
            true => Cut::all(Tri::Unknown),
            false => Cut::HOLDS,
        };
    }
    let Some(open) = (0..list.len()).find(|at| ident(*at, "cfg") && matches!(list.get(at + 1).map(|token| token.kind), Some(TokenKind::Open(Delim::Paren)))).map(|at| at + 1) else {
        return Cut::all(Tri::Unknown);
    };
    let Some(close) = tokens.close_of(open) else { return Cut::all(Tri::Unknown) };
    let under = |test: Tri| {
        let outcome = cfg_predicate_tri(&tokens, open + 1..close, g, &mut |name, value| match (name, value) {
            ("test", None) => test,
            _ => Tri::Unknown,
        });
        outcome.unwrap_or(Tri::Unknown)
    };
    Cut { no: under(Tri::No), yes: under(Tri::Yes), unknown: under(Tri::Unknown) }
}

/// The cut of the cfg attributes of `item` (the file's own, for `None`) in `view`.
fn own_cut(view: &FileItemsView<'_>, item: Option<&Item>, edition: Edition, g: Alloc<'_>) -> Cut {
    view.cfgs(item).fold(Cut::HOLDS, |cut, (kind, written)| cut.and(attr_cut(kind, written, edition, g)))
}

/// One outline's cuts: its file's own, and each entry's whose chain cuts anything, by the
/// entry's first byte (what a part's `place` is: blaze-cli work.rs), sorted.
pub(crate) struct FileCuts<'g> {
    pub(crate) own: Cut,
    at: AVec<'g, (u32, Cut)>,
}

impl FileCuts<'_> {
    /// The cut of the part at `place`: the deepest entry starting there (its chain holds every
    /// enclosing entry's); [`Cut::HOLDS`] where no entry that cuts anything starts there (a
    /// data file's include, a part of an entry with no cfg).
    pub(crate) fn of(&self, place: u32) -> Cut {
        let past = self.at.partition_point(|one| one.0 <= place);
        match past.checked_sub(1).and_then(|at| self.at.get(at)) {
            Some((start, cut)) if *start == place => *cut,
            _ => Cut::HOLDS,
        }
    }
}

/// The cuts of `one`'s outline, in `g`: `None` when its record lists no body and no file (a
/// data file, a file that did not read: nothing of it is planned or walked). An outline that
/// lists parts and whose payload is no `FileItems` record is the outline task's fault: a panic
/// naming the file, which fails this request's job alone (rule 20; engine_tasks.rs `items`
/// reads it the same way).
pub(crate) fn file_cuts<'g>(one: &Outlined<'_>, g: Alloc<'g>) -> Option<FileCuts<'g>> {
    let record = one.made.as_ref().ok()?;
    let planned = record.parts.iter().any(|part| matches!((part.name.kind, part.locate), (Kind::BodyCheck, Locate::Source(_)) | (_, Locate::File { .. })));
    if !planned {
        return None;
    }
    let payload = record.payload_bytes().unwrap_or_else(|why| panic!("outline {:?} lists parts and has no payload: {why} (engine_plan_cfg.rs `file_cuts`)", one.name));
    let view = FileItemsView::read(payload).unwrap_or_else(|why| panic!("outline {:?} lists parts and its payload is no FileItems record: {why:?} (engine_plan_cfg.rs `file_cuts`)", one.name));
    let edition = Edition::from_byte(one.edition).unwrap_or_else(|| panic!("outline {:?} has an edition byte that is no edition (engine_plan_cfg.rs `file_cuts`)", one.name));
    let own = own_cut(&view, None, edition, g);
    // Each entry's chain, in the file's order: a parent comes before its entries.
    let mut chain: AVec<'_, Cut> = Vec::with_capacity_in(view.len(), g);
    let mut at: AVec<'g, (u32, Cut)> = Vec::new_in(g);
    for entry in view.items() {
        let above = entry.parent.and_then(|parent| chain.get(parent.at()).copied()).unwrap_or(own);
        let cut = above.and(own_cut(&view, Some(&entry.item), edition, g));
        chain.push(cut);
        if cut != Cut::HOLDS {
            at.push((entry.range.start, cut));
        }
    }
    // Entries are in the file's order, each before those inside it (which start at or after
    // it) and after its earlier siblings' (which end before it): their starts never go down,
    // and of one start the deepest is last, as [`FileCuts::of`] reads them. A view that breaks
    // it is the outline's fault, named (rule 20), never a cut read at the wrong entry.
    assert!(at.is_sorted_by_key(|one| one.0), "outline {:?} lists an entry that starts before the one above it (engine_plan_cfg.rs `file_cuts`)", one.name);
    Some(FileCuts { own, at })
}

/// The cut of the body `part` of the outline whose cuts are `cuts` ([`file_cuts`]):
/// [`Cut::HOLDS`] for a file with none.
pub(crate) fn part_cut(cuts: Option<&FileCuts<'_>>, part: &blaze_drive::record::Part) -> Cut {
    cuts.map_or(Cut::HOLDS, |cuts| cuts.of(part.place))
}

/// Each outline's cuts ([`file_cuts`]), by its index in the slice they were made from: made
/// once for a walk of many module trees over one slice (engine_body_plan.rs `Reach::walk`,
/// engine_bodies.rs `groups`), so no file's record is read once a tree (rule 77).
pub(crate) struct PlanCuts<'g> {
    files: AVec<'g, Option<FileCuts<'g>>>,
}

impl<'g> PlanCuts<'g> {
    /// The cuts of every one of `outlined`, in `g`.
    pub(crate) fn of(outlined: &[Outlined<'_>], g: Alloc<'g>) -> PlanCuts<'g> {
        let mut files: AVec<'g, Option<FileCuts<'g>>> = Vec::with_capacity_in(outlined.len(), g);
        files.extend(outlined.iter().map(|one| file_cuts(one, g)));
        PlanCuts { files }
    }

    /// The cuts of the outline at `at` in the slice they were made from; `None` for one with
    /// none ([`file_cuts`]).
    pub(crate) fn file(&self, at: usize) -> Option<&FileCuts<'g>> {
        self.files.get(at).and_then(Option::as_ref)
    }
}
