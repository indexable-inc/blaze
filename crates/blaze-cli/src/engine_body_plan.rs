//! Which unit of a crate checks each body: decided from the units' records alone (lane RP1).
//!
//! A package's units are crates of their own (the crate list's rows: the lib, each bin, the lib's
//! test harness, each integration test), each with its cfg set (blaze-unit-graph resolve.rs
//! `unit_cfg`: `test` on a test harness, `feature = ".."` for each feature on). Which files and
//! items a unit holds is its names task's decision, recorded in its items record (its file rows,
//! and its items under the unit's cfg). The planner outlines files forward from text, cfg-blind
//! (engine_check.rs `plan`): `#[cfg(test)] mod tests;` names `src/tests.rs` for every unit of the
//! crate. Which units REACH a file, and which hold a ROW of a body, is read against what plan time
//! knows of each unit's cfg (engine_plan_cfg.rs, D191's cfg-aware holders: a unit's `test`, in
//! three values): the lib unit with no build script does not reach `src/tests.rs`
//! ([`reached_under`]), and has no row of a `#[cfg(test)]` entry; a test build has none of a
//! `#[cfg(not(test))]` one. What plan time cannot decide keeps every reacher's row, counted.
//!
//! THE RULE (D191: every unit that holds a body checks it). A body's rows are planned from text,
//! one for each member unit whose module tree reaches its file ([`Reach`]) and whose `test` does
//! not cut the body's entry (engine_plan_cfg.rs [`Cut`]), in rank
//! (non-test units first, then the crate list's order): the first keeps the body's plain name
//! (the BASE row, the name a body one unit reaches has always had), each other's row is the name
//! at its view (blaze-drive view.rs `at_view`, the one spelling; [`ViewPlan`] the strategy). Each
//! row is its own unit's alone ([`Holders::decide`]): checked in that unit's world when its
//! records hold the body, else said the way that unit's records say it. A unit's refusal (its
//! records could not be read) refuses its own row; another unit's row of the body is checked.
//! What no unit holds is no unit's body, never a refusal:
//! - a file no unit's items record lists (a `cfg` on its `mod` cut it in every unit): its rows
//!   are no unit's, each with no finding; ONE note for the file, a block of the lines record
//!   under the file's own name with no engine row (so a certificate replays it and no finding
//!   stops one), counted ([`Outside::lines`]);
//! - an entry of a held file no holding unit's records hold (a `cfg` inside the file, or a macro
//!   that replaced it): no unit's body; counted in the plan's line, no note each.
//!
//! A row whose unit does not hold the body (its file's `mod` cut by the unit's cfg, its entry
//! left out: `#[cfg(test)] fn only_in_tests` in the lib's row) is done with no finding and no
//! check: nothing of it is called checked.
//!
//! WHAT A ROW READS (rule 21). A row that is not its unit's body reads its unit's items section's
//! cell (`NameCell::Whole { section: Items }`, which moves when its file rows or items move: a
//! `#[cfg(test)]` taken off `mod tests;` adds the file to the lib unit's rows), or its names
//! record whole when its crate has no cell table ([`Holders::cells`]): the unit that comes to hold
//! the body enters its row again. A checked row reads its own unit's records through the checker,
//! so its unit's losing the body moves those reads. Which units reach a file is the plan's (each
//! row's NAME): a `mod` that moves a reach moves the reached file's plan.

use core::fmt;

use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::view::{UnitView, ViewKey, ViewPlan, at_view};
use blaze_drive::{Kind, Locate, Name};
use blaze_engine::{CellId, Task, View};
use blaze_ids::ContentId;
use blaze_own::{AVec, Vec};
use blaze_scope::names_cells::{NameCell, Section};
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::mem::Frame;
use std::alloc::Global;

use crate::engine_plan_cfg::{Cut, PlanCuts, file_cuts, part_cut};
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
use blaze_expand::Tri;

/// Where the rule is (rule 19).
const RULE: &str = "engine_body_plan.rs";

/// The words of an entry a unit holds the file of, but whose records hold no body at the entry
/// (engine_bodies.rs `groups`): not that unit's body ([`BodyIs::Absent`]), never a refusal.
pub(crate) const ENTRY_ABSENT: &str = "no body of its crate's records is at this entry (a `cfg` left it out, or a macro replaced it)";

/// The refusal of a planned body whose file a unit holds, but whose records' walk of the file
/// listed no entry at its name: a fault of the plan or the records, named (rule 3).
pub(crate) const NO_ENTRY: &str = "its crate's records list no entry with a body at this name, though they hold its file (engine_body_plan.rs `decide`)";

/// The refusal of a row whose unit the plan's reach gives the file, and which made no claim on
/// it while others did: the setup did not visit the unit whose row was entered (engine_bodies.rs
/// `groups`), a fault named (rule 3), read nothing, so owed and entered again.
pub(crate) const NO_CLAIM: &str = "its unit made no claim on its file though the plan's reach gives the unit this file (engine_bodies.rs `groups` did not visit the unit whose row was entered)";

/// The domain of the out of a body no unit holds.
const OUTSIDE_DOMAIN: &str = "blaze-cli engine bodies no unit's 1";

/// The domain of a plan note's block name ([`note_name`]).
const NOTE_DOMAIN: &str = "blaze-cli engine body plan file note 1";

/// The domain of a shared body's row source ([`source_at_views`]).
const VIEWS_DOMAIN: &str = "blaze-cli engine body rows at views 1";

/// The domain of a body's row source folded with its cumulative cfg-attribute chain
/// ([`source_with_cfg`]).
const CFG_SOURCE_DOMAIN: &str = "blaze-cli engine body row cfg 1";

/// Folds the cumulative cfg-attribute chain hash (file `#![cfg]`s + enclosing ancestors' `#[cfg]`s
/// + entry's own `#[cfg]`s) into a body's `source` so editing any enclosing `#[cfg]` moves the
/// body's task key directly without needing `Decision::Absent` to read `Section::Items` (rule 21).
pub(crate) fn source_with_cfg(source: ContentId, cfg: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(CFG_SOURCE_DOMAIN);
    h.update(&source.0);
    h.update(&cfg.0);
    h.finish()
}

/// The source of each row of a body two units or more reach (D191): its text's source and every
/// such unit's view, in rank. A row's block names the units its body's rows are at
/// (engine_lines.rs `VIEW`, how a finding only some rows hold is printed), so they are inputs of
/// its key (rule 21): a unit that comes to reach the file, or leaves it, enters every row of its
/// bodies again, and a standing row's block never names a stale set. A body one unit reaches
/// keeps its text's source: its row and its output are as before D191.
pub(crate) fn source_at_views(source: ContentId, views: &[ViewKey]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(VIEWS_DOMAIN);
    h.update(&source.0);
    for view in views.iter() {
        h.update(&view.0.0);
    }
    h.finish()
}

/// One unit of the crate list, as a decision reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unit {
    /// Its `CrateScope` task's name.
    pub(crate) name: Name,
    /// The first 8 bytes of its crate key: whose cells a `NameCell` names.
    pub(crate) prefix: [u8; 8],
    /// Its cfg holds `test` (a test harness): ranked after every unit without.
    pub(crate) test: bool,
}

/// What one unit's records say of one file its module tree reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FileIs {
    /// Its items record lists the file, and the file's entries were read through its records.
    Held,
    /// Its items record lists no file at the path: a `cfg` on the `mod` cut it in this unit.
    NotHeld,
    /// The unit's records could not say (they did not open, its scope was cut, its file's text
    /// did not read): refused by name.
    Refused(&'static str),
}

/// One unit's word on one file, by the file's name: a claim is made without the file's outline
/// (a standing member's claims are its members record's, engine_members_record.rs), and a
/// decision asks the claims of its task's file by name ([`Holders::decide`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FileClaim<'f> {
    /// The file's `FileItems` name.
    pub(crate) file: Name,
    pub(crate) unit: u32,
    pub(crate) is: FileIs,
    /// The cells its refusal is a function of (plan row A8), or, for a held file, those a body
    /// the records do not list is refused on.
    pub(crate) reads: &'f [CellId],
}

/// What one unit's records say of one body (an outline entry) of a file it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyIs {
    /// The records hold it: the unit can check it.
    Held,
    /// The records hold no body at the entry ([`ENTRY_ABSENT`]): not this unit's body.
    Absent,
    /// The records hold it but cannot give it a check, and why.
    Refused(&'static str),
}

impl BodyIs {
    /// A body's word from a refusal's words: [`ENTRY_ABSENT`] is no refusal.
    pub(crate) fn of_refusal(why: &'static str) -> BodyIs {
        if why == ENTRY_ABSENT { BodyIs::Absent } else { BodyIs::Refused(why) }
    }
}

/// One unit's word on one body; `took` is the caller's index of what it found for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BodyClaim {
    pub(crate) name: Name,
    pub(crate) unit: u32,
    pub(crate) is: BodyIs,
    pub(crate) took: u32,
}

/// What one unit's row of one body is ([`Holders::decide`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Decision {
    /// No member unit claimed its file (no unit's module tree reaches it).
    Unreached,
    /// The unit's records hold it: the unit checks it (`took`: its claim's index).
    Taken { took: u32, unit: u32 },
    /// The unit's records refused it, or refused its file.
    Refused { why: &'static str, took: Option<u32> },
    /// The unit holds its file, not the entry (a `cfg` in the file, or a macro replaced it).
    Absent { took: Option<u32> },
    /// The unit does not hold its file.
    NotHeld,
}

/// Every member unit's word on the files and bodies it reaches, ranked: what each body's
/// decision is made from. Built once a Bodies phase (engine_bodies.rs `groups`).
pub(crate) struct Holders<'f> {
    units: Vec<Unit, Global>,
    /// Sorted by (file, rank), one a (file, unit).
    files: Vec<FileClaim<'f>, Global>,
    /// Sorted by (name, rank), one a (name, unit).
    bodies: Vec<BodyClaim, Global>,
}

/// The two not-held proof cells for one unit: `files` (`NameCell::Files`, moving only when the
/// unit's held file list changes: `Decision::NotHeld`) and `cfg_atoms` (`NameCell::CfgAtoms`,
/// moving only when the unit's cfg atoms change: `Decision::Absent`, alongside `files` and the
/// body's own task key which folds the entry's enclosing cfg chain). Neither moves when an
/// unrelated private item is added to any file in the crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct UnitCells {
    pub(crate) files: CellId,
    pub(crate) cfg_atoms: CellId,
}

impl<'f> Holders<'f> {
    /// The holders of `units` (by index, the crate list's order), from what each said. A unit
    /// with `test` ranks after every unit without; within each, the list's order (rule 25: the
    /// graph's record fixes it).
    pub(crate) fn new(units: Vec<Unit, Global>, mut files: Vec<FileClaim<'f>, Global>, bodies: impl Iterator<Item = BodyClaim>) -> Holders<'f> {
        let mut order: Vec<u32, Global> = Vec::new_in(Global);
        order.extend(0..u32::try_from(units.len()).unwrap_or(u32::MAX));
        order.sort_by_key(|at| (units[*at as usize].test, *at));
        let mut rank: Vec<u32, Global> = Vec::new_in(Global);
        rank.resize(units.len(), u32::MAX);
        for (place, at) in order.iter().enumerate() {
            rank[*at as usize] = u32::try_from(place).unwrap_or(u32::MAX);
        }
        let rank_of = |unit: u32| rank.get(unit as usize).copied().unwrap_or(u32::MAX);
        files.sort_by_key(|one| (one.file, rank_of(one.unit)));
        files.dedup_by_key(|one| (one.file, one.unit));
        let mut claims: Vec<BodyClaim, Global> = Vec::new_in(Global);
        claims.extend(bodies);
        claims.sort_by_key(|one| (one.name, rank_of(one.unit)));
        claims.dedup_by_key(|one| (one.name, one.unit));
        Holders { units, files, bodies: claims }
    }

    /// Every unit's claim on file `file` (its `FileItems` name), in rank.
    fn file_claims(&self, file: Name) -> &[FileClaim<'f>] {
        let from = self.files.partition_point(|one| one.file < file);
        let rest = &self.files[from..];
        &rest[..rest.partition_point(|one| one.file == file)]
    }

    /// Every unit's claim on body `name` (its base name), in rank.
    fn body_claims(&self, name: Name) -> &[BodyClaim] {
        let from = self.bodies.partition_point(|one| one.name < name);
        let rest = &self.bodies[from..];
        &rest[..rest.partition_point(|one| one.name == name)]
    }

    /// The index of the unit whose `CrateScope` task is `name`: what a row's unit (the plan's,
    /// by name: [`Reacher::unit`]) is among these units. Asked once a unit a phase.
    pub(crate) fn unit_named(&self, name: Name) -> Option<u32> {
        self.units.iter().position(|one| one.name == name).and_then(|at| u32::try_from(at).ok())
    }

    /// UNIT `unit`'S ROW of body `name` (its base name) of file `file` (D191), from that unit's
    /// claims alone: no other unit's word decides it. `passed` gets the unit when the row is not
    /// its body (its not-held cell, [`Holders::cells`], is what the row reads: the unit that
    /// comes to hold it moves it), and `reads` the cells of its refusal.
    pub(crate) fn decide(&self, name: Name, file: Name, unit: u32, passed: &mut Vec<u32, Global>, reads: &mut Vec<CellId, Global>) -> Decision {
        passed.clear();
        reads.clear();
        let claims = self.file_claims(file);
        let Some(claim) = claims.iter().find(|one| one.unit == unit) else {
            return match claims.is_empty() {
                true => Decision::Unreached,
                false => Decision::Refused { why: NO_CLAIM, took: None },
            };
        };
        match claim.is {
            FileIs::NotHeld => {
                passed.push(unit);
                Decision::NotHeld
            }
            FileIs::Refused(why) => {
                reads.extend_from_slice(claim.reads);
                Decision::Refused { why, took: None }
            }
            FileIs::Held => match self.body_claims(name).iter().find(|one| one.unit == unit) {
                Some(BodyClaim { is: BodyIs::Held, took, .. }) => Decision::Taken { took: *took, unit },
                Some(BodyClaim { is: BodyIs::Absent, took, .. }) => {
                    passed.push(unit);
                    Decision::Absent { took: Some(*took) }
                }
                Some(BodyClaim { is: BodyIs::Refused(why), took, .. }) => {
                    reads.extend_from_slice(claim.reads);
                    Decision::Refused { why: *why, took: Some(*took) }
                }
                None => {
                    reads.extend_from_slice(claim.reads);
                    Decision::Refused { why: NO_ENTRY, took: None }
                }
            },
        }
    }

    /// Whether NO unit holds body `name` of file `file`, and why: the plan's note and count
    /// ([`Outside`]). Said only when each of the `reachers` units that reach the file claimed it:
    /// a unit with no claim was not visited, which a unit that does not hold the file always is
    /// (its claim is the members record's, engine_bodies.rs `groups`), so it holds the file and
    /// may hold the body. `None`: some unit holds it or refused it, or that is not known.
    pub(crate) fn outside(&self, name: Name, file: Name, reachers: usize) -> Option<OutsideIs> {
        let claims = self.file_claims(file);
        if claims.is_empty() || claims.len() < reachers {
            return None;
        }
        let bodies = self.body_claims(name);
        let mut held_file = false;
        for claim in claims.iter() {
            match claim.is {
                FileIs::Refused(_) => return None,
                FileIs::NotHeld => {}
                FileIs::Held => {
                    held_file = true;
                    if bodies.iter().find(|one| one.unit == claim.unit).map(|one| one.is) != Some(BodyIs::Absent) {
                        return None;
                    }
                }
            }
        }
        Some(if held_file { OutsideIs::EntryAbsent } else { OutsideIs::FileNotHeld })
    }

    /// The not-held proofs for each unit, shared by every decision in this phase.
    pub(crate) fn unit_cells(&self, grains: &crate::engine_scope_reads::ReadGrains<'_>, g: blaze_own::Alloc<'static>) -> blaze_own::AVec<'static, UnitCells> {
        let mut ids = Vec::with_capacity_in(self.units.len(), g);
        ids.extend(self.units.iter().map(|one| UnitCells {
            files: grains.place(files_cell(one.name, one.prefix), false),
            cfg_atoms: grains.place(cfg_atoms_cell(one.name, one.prefix), false),
        }));
        ids
    }

    /// What a row is a function of: its `passed` unit's not-held cells (`NameCell::Files` when the
    /// file itself is not held; `NameCell::Files` + `NameCell::CfgAtoms` when the file is held but
    /// the entry is cfg'd out, as its enclosing cfg chain is already folded into its task key) and
    /// its refusal's `reads`, in the phase frame. A checked body canonicalizes these with its other IDs.
    pub(crate) fn cells<'a>(unit_cells: &[UnitCells], decided: Decision, passed: &[u32], reads: &[CellId], frame: &'a Frame) -> Vec<CellId, &'a Frame> {
        let file_only = matches!(decided, Decision::NotHeld);
        let extra = if file_only { passed.len() } else { passed.len().saturating_mul(2) };
        let mut ids = Vec::with_capacity_in(extra + reads.len(), frame);
        for &unit in passed.iter() {
            if let Some(&pair) = usize::try_from(unit).ok().and_then(|i| unit_cells.get(i)) {
                ids.push(pair.files);
                if !file_only && pair.cfg_atoms != pair.files {
                    ids.push(pair.cfg_atoms);
                }
            }
        }
        ids.extend_from_slice(reads);
        ids
    }
}

/// Which cell says a unit's items record does not list a file: the unit's `Files` cell when its
/// crate's names task (`name`, of crate prefix `prefix`) defines a cell table ([`CELLS`]), else
/// its names record whole ([`WHOLE`]). A `#[cfg(test)]` taken off a `mod` adds the file and moves
/// this cell; adding or editing items within an already held file leaves it unchanged.
pub(crate) fn file_not_held_cell(view: View<'_, '_>, name: Name, prefix: [u8; 8]) -> CellId {
    let table = CellId { of: name, cell: CELLS };
    if view.cell(&table) != ABSENT { files_cell(name, prefix) } else { CellId { of: name, cell: WHOLE } }
}

/// THE ONE HOME of "which cell says a unit's items section changed": the unit's items section's
/// cell when its crate's names task (`name`, of crate prefix `prefix`) defines a cell table
/// ([`CELLS`]), else its names record whole ([`WHOLE`]), which that section is part of.
pub(crate) fn not_held_cell(view: View<'_, '_>, name: Name, prefix: [u8; 8]) -> CellId {
    let table = CellId { of: name, cell: CELLS };
    if view.cell(&table) != ABSENT { items_cell(name, prefix) } else { CellId { of: name, cell: WHOLE } }
}

fn files_cell(of: Name, prefix: [u8; 8]) -> CellId {
    CellId { of, cell: Cell(NameCell::Files { krate: prefix }.key()) }
}

fn cfg_atoms_cell(of: Name, prefix: [u8; 8]) -> CellId {
    CellId { of, cell: Cell(NameCell::CfgAtoms { krate: prefix }.key()) }
}

fn items_cell(of: Name, prefix: [u8; 8]) -> CellId {
    CellId { of, cell: Cell(NameCell::Whole { krate: prefix, section: Section::Items }.key()) }
}

/// A file a crate's module tree reaches that none of the request's outlines holds: the planner
/// outlines the root of every member target and every file part it walks, or stops (engine_check.rs
/// `plan`, `wants`), and the ready setup reads back every file of a member it visits; so a gap is
/// the reader's bug, named, never a file quietly left out of a crate (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unoutlined {
    /// The file no outline holds (the root itself, or a file part of a reached outline).
    pub(crate) file: Name,
    /// The crate root whose walk reached it.
    pub(crate) root: Name,
}

impl fmt::Display for Unoutlined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: the module tree of the crate rooted at {:?} reaches this file, and none of the request's outlines holds it ({RULE} `reached`)", self.file, self.root)
    }
}

/// THE OUTLINE A UNIT HOLDS ITS ITEMS RECORD'S FILE AT `path` AS (D191): the file's outlines are
/// one per name it is planned by, and one file can have two: a `mod config;` plans the slot
/// `cli/config.rs`, read at `cli/config/mod.rs` (engine_bodies.rs `slot_names`), and another
/// crate's `#[path = "../../cli/config/mod.rs"] mod config;` plans `cli/config/mod.rs` itself
/// (work.rs `module_file`; turso's tursodb and tursopg, c1814). A unit's bodies' rows are named
/// from the outlines its module tree reaches ([`Reach`]), so the one it holds is the one it
/// reaches (`reach`, sorted: [`reached_under`]). The first outline at `path` held for both, so
/// the unit that reached the other said it held nothing there and its rows went unchecked, noted
/// "not part of this build". A unit that reaches no outline at `path` holds the first (the one
/// there is, but for a file named twice). `None`: no outline is at `path`.
pub(crate) fn outline_at_path(outlined: &[Outlined<'_>], path: &[u8], reach: &[Name]) -> Option<usize> {
    let mut first = None;
    for (at, one) in outlined.iter().enumerate().filter(|(_, one)| one.path.as_slice() == path) {
        if reach.binary_search(&one.name).is_ok() {
            return Some(at);
        }
        first.get_or_insert(at);
    }
    first
}

/// The files (by name, sorted) a crate's module tree reaches from its root file `root`: the root
/// and every file a reached outline names as a file part, transitively, as the planner walked
/// them (engine_check.rs `plan`), cfg-blind: a file two crates reach is in both crates' walks, and
/// a unit whose cfg cuts a `mod` still reaches its file (whether it HOLDS it is its items record's
/// word, [`not_held_cell`]). A reached file with no outline in `outlined` is [`Unoutlined`]. In `g`.
pub(crate) fn reached(outlined: &[Outlined<'_>], root: Name, g: blaze_own::Alloc<'static>) -> Result<Vec<Name, blaze_own::Alloc<'static>>, Unoutlined> {
    let mut seen: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
    let mut next: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
    next.push(root);
    while let Some(name) = next.pop() {
        let Err(slot) = seen.binary_search(&name) else { continue };
        let at = outlined.binary_search_by(|one| one.name.cmp(&name)).map_err(|_| Unoutlined { file: name, root })?;
        seen.insert_shift(slot, name);
        if let Ok(record) = &outlined[at].made {
            next.extend(record.parts.iter().filter(|part| matches!(part.locate, Locate::File { edition } if !crate::data_include::is_data(edition))).map(|part| part.name));
        }
    }
    Ok(seen)
}

/// The files (by name, sorted) the module tree of a unit whose `test` is `test`
/// (engine_plan_cfg.rs `test_atom`) reaches from its root file `root`, as [`reached`] walks them,
/// but for what plan time knows of the unit's cfg: a file whose own `#![cfg]`s, or a file part
/// whose `mod` entry's chain (engine_plan_cfg.rs [`Cut`]), is `No` under `test` is not entered,
/// so neither it nor what it names is reached. An `Unknown` is entered (the unit's items record
/// says whether it holds it, [`not_held_cell`]). What the Bodies rows and their setup ask
/// ([`Reach::walk`], engine_bodies.rs `groups`); the heads' walk stays cfg-blind (engine_heads.rs).
/// `cuts`: `outlined`'s, made once for every walk over it. A file with no `#[cfg]` is walked as
/// [`reached`] walks it. `found`: the unit's found files ([`FoundEdge`], [`Reach::found_of`]),
/// walked from as the root is (each its own `#![cfg]` read, then its file parts): the files its
/// names read that no `mod` the walk follows names. In `g`.
pub(crate) fn reached_under(outlined: &[Outlined<'_>], cuts: &PlanCuts<'_, '_, '_>, root: Name, test: Tri, found: &[FoundEdge], g: blaze_own::Alloc<'static>) -> Result<Vec<Name, blaze_own::Alloc<'static>>, Unoutlined> {
    let mut seen: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
    let mut next: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
    next.push(root);
    next.extend(found.iter().map(|edge| edge.file));
    while let Some(name) = next.pop() {
        let Err(slot) = seen.binary_search(&name) else { continue };
        let at = outlined.binary_search_by(|one| one.name.cmp(&name)).map_err(|_| Unoutlined { file: name, root })?;
        let file = cuts.file(at);
        if file.is_some_and(|file| file.own.cuts(test)) {
            continue;
        }
        seen.insert_shift(slot, name);
        if let Ok(record) = &outlined[at].made {
            next.extend(record.parts.iter().filter(|part| matches!(part.locate, Locate::File { edition } if !crate::data_include::is_data(edition)) && !crate::engine_plan_cfg::part_cut(file, part).cuts(test)).map(|part| part.name));
        }
    }
    Ok(seen)
}

/// The file crate list row `at`'s root is, by its outline's name (its path past the checkout's
/// `/0/`), as the planner outlined it: where a unit's module tree is walked from.
pub(crate) fn root_file(list: &CrateList<'_>, at: usize) -> Name {
    Name::file(list.root(at).strip_prefix("/0/").unwrap_or(list.root(at)).as_bytes())
}

/// The view of crate list row `at`, a member unit (blaze-drive view.rs `ViewKey::of_member`):
/// its root, its crate's name and whether it is a test build, never its crate key (which holds
/// its dependencies' keys: a dependency bumped would rename every view row). The one spelling
/// check (here) and run (run_ids.rs `unit_roots`) both ask rows by.
pub(crate) fn unit_view(list: &CrateList<'_>, at: usize) -> ViewKey {
    ViewKey::of_member(list.root(at).as_bytes(), CrateGraph::name(list, at).as_bytes(), list.test(at))
}

/// One member unit as the plan ranks it (D191): its `CrateScope` task's name ([`Unit::name`])
/// and its view ([`unit_view`]: the one under which run and test ask its rows: run_ids.rs
/// `views_of`).
/// `test` and `name` (its crate's name, as [`Reach`] keeps it): how a printed finding names the
/// unit (engine_lines.rs `ViewUnit`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Reacher {
    pub(crate) unit: Name,
    pub(crate) view: ViewKey,
    pub(crate) test: bool,
    /// What plan time knows of its cfg's `test` (engine_plan_cfg.rs `test_atom`): what a body's
    /// entry's cut is read under.
    pub(crate) test_atom: Tri,
    /// Its crate's name in [`Reach`]'s names: (from, past).
    name: (u32, u32),
}

/// A member unit the members record does not hold, or holds as another unit: its reach is not
/// known without its module tree ([`Reach::of_members`]); named, never guessed (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ReachUnknown {
    pub(crate) unit: Name,
}

impl fmt::Display for ReachUnknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: the members record holds no entry of this member unit, so which files its module tree reaches is not known without its outlines ({RULE} `Reach::of_members`)", self.unit)
    }
}

/// A FOUND FILE OF A MEMBER UNIT (D161): `unit`'s names (its `CrateScope` task) read `file`, and
/// no `mod` the walk follows names it: a module a macro wrote from a computed name
/// (`macro_rules! m { ($n:ident) => { mod $n; } }`), a data file of a computed path. The unit's
/// tree holds it because its resolution read it, every cfg on the way decided under the unit's
/// own cfg: a `#[cfg(test)] mod outer { m!(inner); }` gives the test unit the edge and the lib
/// unit none, as the plan's cut gives a literal `mod inner;` there. Kept in the answer as the
/// found flags of the file's place (engine_check.rs [`crate::engine_check::PLACE_FOUND_LIB`]),
/// read back by [`found_edges`]; sorted by (unit, file), so [`Reach::found_of`] is a slice.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct FoundEdge {
    pub(crate) unit: Name,
    pub(crate) file: Name,
}

/// Every member unit of `list` with its `(root, test, unit)`, sorted and deduplicated: what a
/// found flag of a place whose `unit` is that root resolves to ([`found_edges`]). Multiple member
/// units may share `(root, test)` — for example a workspace library compiled for both
/// `Side::Target` and `Side::Macro` (used by a member proc-macro) or under distinct feature sets.
/// In `g`.
pub(crate) fn found_units(list: &CrateList<'_>, g: blaze_own::Alloc<'static>) -> AVec<'static, (Name, bool, Name)> {
    let mut units: AVec<'static, (Name, bool, Name)> = Vec::new_in(g);
    for at in (0..CrateGraph::len(list)).filter(|at| list.member(*at)) {
        units.push((root_file(list, at), list.test(at), blaze_drive::key::crate_scope_name(&list.key(at))));
    }
    units.sort_unstable();
    units.dedup();
    units
}

/// The edges of `unit` in `sorted` (sorted by unit, then file).
fn edges_of(sorted: &[FoundEdge], unit: Name) -> &[FoundEdge] {
    let from = sorted.partition_point(|one| one.unit < unit);
    let rest = &sorted[from..];
    &rest[..rest.partition_point(|one| one.unit == unit)]
}

/// The found edges `places` keep (engine_check.rs [`crate::engine_check::PLACE_FOUND`]): each
/// found flag of a place names every member unit of its root and kind ([`found_units`]); when
/// multiple units share `(root, test)` (e.g. target and macro side of one library), each receives
/// the edge so its reach includes the file, and any unit whose own cfg/items record does not hold
/// it settles to `FileIs::NotHeld` (`Decision::NotHeld`). A flag whose `(root, test)` `list` no
/// longer has is no edge (its file goes when no names row reads it). Sorted. In `g`.
pub(crate) fn found_edges(units: &[(Name, bool, Name)], places: &[(Name, blaze_engine::pages::FilePlace)], g: blaze_own::Alloc<'static>) -> AVec<'static, FoundEdge> {
    let mut edges: AVec<'static, FoundEdge> = Vec::new_in(g);
    for (file, place) in places.iter() {
        for test in [false, true] {
            if place.flags & crate::engine_check::found_bit(test) == 0 {
                continue;
            }
            let from = units.partition_point(|one| (one.0, one.1) < (place.unit, test));
            for &(_, _, unit) in units.iter().skip(from).take_while(|one| (one.0, one.1) == (place.unit, test)) {
                edges.push(FoundEdge { unit, file: *file });
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// WHICH MEMBER UNITS REACH EACH FILE, in rank (D191): what each body's rows are planned from
/// (engine_bodies.rs `plan_views`, [`task_rows`]) and what the Bodies setup checks each unit it
/// visits against (engine_bodies.rs `groups`). Made once a request and handed to both (rule 30):
/// the phase path walks every member's tree over its outlines ([`Reach::walk`]); the ready path,
/// which outlines only the edited files, reads each member's held and reached files from the
/// last members record ([`Reach::of_members`]), the same files the walk gave it then.
pub(crate) struct Reach {
    /// The member units in rank: non-test units first, then the crate list's order (rule 25:
    /// the graph's record fixes it; no map order).
    units: AVec<'static, Reacher>,
    /// (file, rank), sorted, each once.
    files: AVec<'static, (Name, u32)>,
    /// The units' crates' names, end to end ([`Reacher`]'s spans).
    names: AVec<'static, u8>,
    /// The member units' found files ([`FoundEdge`]), sorted: what the walk took them from, and
    /// what the Bodies setup's walk of a unit takes ([`Reach::found_of`], engine_bodies.rs `groups`).
    found: AVec<'static, FoundEdge>,
}

impl Reach {
    /// The member units of `list` in rank, each with its list row; their crates' names into
    /// `names`.
    fn ranked(list: &CrateList<'_>, names: &mut AVec<'static, u8>, g: blaze_own::Alloc<'static>) -> AVec<'static, (usize, Reacher)> {
        let mut units: AVec<'static, (usize, Reacher)> = Vec::new_in(g);
        for at in (0..CrateGraph::len(list)).filter(|at| list.member(*at)) {
            let key = list.key(at);
            let from = u32::try_from(names.len()).unwrap_or(u32::MAX);
            names.extend_from_slice(CrateGraph::name(list, at).as_bytes());
            let name = (from, u32::try_from(names.len()).unwrap_or(u32::MAX));
            units.push((at, Reacher { unit: blaze_drive::key::crate_scope_name(&key), view: unit_view(list, at), test: list.test(at), test_atom: crate::engine_plan_cfg::test_atom(list, at), name }));
        }
        units.sort_by_key(|(at, _)| (list.test(*at), *at));
        units
    }

    fn of(ranked: &[(usize, Reacher)], mut files: AVec<'static, (Name, u32)>, names: AVec<'static, u8>, found: &[FoundEdge], g: blaze_own::Alloc<'static>) -> Reach {
        files.sort_unstable();
        files.dedup();
        let mut units: AVec<'static, Reacher> = Vec::with_capacity_in(ranked.len(), g);
        units.extend(ranked.iter().map(|(_, one)| *one));
        let mut kept: AVec<'static, FoundEdge> = Vec::with_capacity_in(found.len(), g);
        kept.extend_from_slice(found);
        kept.sort_unstable();
        kept.dedup();
        Reach { units, files, names, found: kept }
    }

    /// The found files of the member unit whose `CrateScope` task is `unit` ([`FoundEdge`]): what
    /// its walk takes beside its root ([`reached_under`]).
    pub(crate) fn found_of(&self, unit: Name) -> &[FoundEdge] {
        edges_of(&self.found, unit)
    }

    /// How a printed finding names the unit at `rank` (engine_lines.rs `ViewUnit`).
    pub(crate) fn label(&self, rank: u32) -> Option<crate::engine_lines::ViewUnit<'_>> {
        let one = self.units.get(rank as usize)?;
        let name = core::str::from_utf8(self.names.get(one.name.0 as usize..one.name.1 as usize)?).ok()?;
        Some(crate::engine_lines::ViewUnit { test: one.test, name })
    }

    /// The phase path's: each member's module tree walked from its root over `outlined` under
    /// what plan time knows of its `test` ([`reached_under`]), each (root, `test`) once (a lib
    /// with no build script and its test build walk one root twice, unless their found files
    /// differ: [`FoundEdge`]). `found`: every member unit's found files. In `g`.
    pub(crate) fn walk(list: &CrateList<'_>, outlined: &[Outlined<'_>], found: &[FoundEdge], g: blaze_own::Alloc<'static>) -> Result<Reach, Unoutlined> {
        let mut names: AVec<'static, u8> = Vec::new_in(g);
        let ranked = Reach::ranked(list, &mut names, g);
        let mut sorted: AVec<'static, FoundEdge> = Vec::with_capacity_in(found.len(), g);
        sorted.extend_from_slice(found);
        sorted.sort_unstable();
        sorted.dedup();
        let found_of = |unit: Name| edges_of(&sorted, unit);
        let mut by_root: AVec<'static, (Name, u8, u32)> = Vec::with_capacity_in(ranked.len(), g);
        for (rank, (at, one)) in ranked.iter().enumerate() {
            by_root.push((root_file(list, *at), one.test_atom as u8, u32::try_from(rank).unwrap_or(u32::MAX)));
        }
        by_root.sort_unstable();
        let mut files: AVec<'static, (Name, u32)> = Vec::new_in(g);
        let cuts = PlanCuts::of(outlined, g);
        let mut seeds: AVec<'static, FoundEdge> = Vec::new_in(g);
        for group in by_root.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
            let test = ranked.get(group[0].2 as usize).map_or(Tri::Unknown, |(_, one)| one.test_atom);
            // One walk for the group while its units' found files are the same; else one each.
            let unit_of = |rank: u32| ranked.get(rank as usize).map(|(_, one)| one.unit);
            let first = unit_of(group[0].2).map_or(&[][..], found_of);
            let same = group.iter().all(|(_, _, rank)| {
                let own = unit_of(*rank).map_or(&[][..], found_of);
                own.len() == first.len() && own.iter().zip(first.iter()).all(|(a, b)| a.file == b.file)
            });
            if same {
                let tree = reached_under(outlined, &cuts, group[0].0, test, first, g)?;
                for (_, _, rank) in group.iter() {
                    files.extend(tree.iter().map(|file| (*file, *rank)));
                }
                continue;
            }
            for (_, _, rank) in group.iter() {
                seeds.clear();
                seeds.extend_from_slice(unit_of(*rank).map_or(&[][..], found_of));
                let tree = reached_under(outlined, &cuts, group[0].0, test, &seeds, g)?;
                files.extend(tree.iter().map(|file| (*file, *rank)));
            }
        }
        Ok(Reach::of(&ranked, files, names, &sorted, g))
    }

    /// The ready path's: each member's held and reached-not-held files as the last members
    /// record keeps them (engine_members_record.rs `Member`: the walk of its visit, its claims).
    /// A member the record does not hold is [`ReachUnknown`]. The record is the caller's, read
    /// over today's graph; a `mod` the edit moved is caught where the setup walks the unit
    /// (engine_bodies.rs `groups`, [`Reach::disagrees`]). `found`: every member unit's found
    /// files, kept for the setup's walk ([`Reach::found_of`]); the record's files hold them already
    /// (the walk of its visit took them). In `g`.
    pub(crate) fn of_members(list: &CrateList<'_>, members: &crate::engine_members_record::View<'_>, found: &[FoundEdge], g: blaze_own::Alloc<'static>) -> Result<Reach, ReachUnknown> {
        let mut names: AVec<'static, u8> = Vec::new_in(g);
        let ranked = Reach::ranked(list, &mut names, g);
        let mut files: AVec<'static, (Name, u32)> = Vec::new_in(g);
        for (rank, (at, one)) in ranked.iter().enumerate() {
            let rank = u32::try_from(rank).unwrap_or(u32::MAX);
            let kept = u32::try_from(*at).ok().and_then(|at| members.member(at)).filter(|kept| kept.unit.name == one.unit).ok_or(ReachUnknown { unit: one.unit })?;
            match &kept.state {
                crate::engine_members_record::MemberState::Held(held) => {
                    files.extend(held.held.iter().map(|held| (held.slot, rank)));
                }
                crate::engine_members_record::MemberState::CutScope(cut) => {
                    files.extend(cut.held.iter().map(|held| (held.slot, rank)));
                }
                crate::engine_members_record::MemberState::Refused(_) => {}
            }
            files.extend(kept.not_held.iter().map(|file| (*file, rank)));
        }
        Ok(Reach::of(&ranked, files, names, found, g))
    }

    /// The units that reach `file`: (file, rank) in rank order.
    pub(crate) fn ranks(&self, file: Name) -> &[(Name, u32)] {
        let from = self.files.partition_point(|one| one.0 < file);
        let rest = &self.files[from..];
        &rest[..rest.partition_point(|one| one.0 == file)]
    }

    /// How many member units are ranked.
    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.units.len()).unwrap_or(u32::MAX)
    }

    /// The unit at `rank`.
    pub(crate) fn unit(&self, rank: u32) -> Option<Reacher> {
        self.units.get(rank as usize).copied()
    }

    /// The rank of the unit whose `CrateScope` task is `unit`.
    pub(crate) fn rank_of(&self, unit: Name) -> Option<u32> {
        self.units.iter().position(|one| one.unit == unit).and_then(|at| u32::try_from(at).ok())
    }

    /// THE ROWS OF A BODY (D191, cfg-aware holders): the units that reach `file` and whose `test`
    /// does not cut the body's entry (`cut`, engine_plan_cfg.rs [`Cut`]), in rank, by their
    /// units. The one filter every reader of a body's rows asks (rule 30): the plan's names
    /// (engine_bodies.rs `plan_views`), the setup's ([`task_rows`], engine_bodies.rs `groups`)
    /// and the printed view of a shared body's block.
    pub(crate) fn body_ranks(&self, file: Name, cut: Cut) -> impl Iterator<Item = u32> + '_ {
        self.ranks(file).iter().map(|one| one.1).filter(move |rank| self.units.get(*rank as usize).is_some_and(|unit| !cut.cuts(unit.test_atom)))
    }

    /// The views of the units with a row of the body of `file` whose cut is `cut`, in rank,
    /// into `views` (cleared first): what [`ViewPlan::view_of`] is asked over.
    pub(crate) fn views_into<A: core::alloc::Allocator>(&self, file: Name, cut: Cut, views: &mut Vec<ViewKey, A>) {
        views.clear();
        views.extend(self.body_ranks(file, cut).filter_map(|rank| self.units.get(rank as usize)).map(|one| one.view));
    }

    /// The view of the unit at `rank` among the rows of the body of `file` whose cut is `cut`
    /// under `P`; `None` when the unit has no row of it (it does not reach the file, or its
    /// `test` cuts the entry). `views`: scratch.
    pub(crate) fn view_in<P: ViewPlan, A: core::alloc::Allocator>(&self, file: Name, cut: Cut, rank: u32, views: &mut Vec<ViewKey, A>) -> Option<UnitView> {
        let at = self.body_ranks(file, cut).position(|one| one == rank)?;
        self.views_into(file, cut, views);
        Some(P::view_of(views, at))
    }

    /// Whether the unit at `rank` keeps its row of a body whose cut is `cut` only because plan
    /// time cannot decide it: "undecided at plan time" ([`Outside::profile`]).
    pub(crate) fn undecided(&self, rank: u32, cut: Cut) -> bool {
        self.units.get(rank as usize).is_some_and(|unit| cut.under(unit.test_atom) == Tri::Unknown)
    }

    /// The first file (by name) on which unit `unit`'s module tree as walked today (`walked`,
    /// sorted: [`reached`]) and this reach disagree; `None` when they agree. A unit this reach
    /// does not rank disagrees at its first file.
    pub(crate) fn disagrees(&self, unit: Name, walked: &[Name]) -> Option<Name> {
        let Some(rank) = self.rank_of(unit) else { return walked.first().copied() };
        let mut mine = self.files.iter().filter(|one| one.1 == rank).map(|one| one.0);
        let mut today = walked.iter().copied();
        loop {
            match (mine.next(), today.next()) {
                (None, None) => return None,
                (Some(a), Some(b)) if a == b => {}
                (Some(a), Some(b)) => return Some(a.min(b)),
                (a, b) => return a.or(b),
            }
        }
    }
}

/// The rank of a row no unit reaches (its file is no member's): the base row alone.
pub(crate) const NO_UNIT: u32 = u32::MAX;

/// A planned body task as the row of one body and one unit (D191): its base name (the body's
/// outline part), its file, its unit's rank in the reach ([`NO_UNIT`]: no unit reaches the file)
/// and its place among the body's rows (0: the base row), its entry's cut (engine_plan_cfg.rs
/// [`Cut`]: which reachers have a row) and how many rows the body has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TaskRow {
    pub(crate) name: Name,
    pub(crate) base: Name,
    pub(crate) file: Name,
    pub(crate) rank: u32,
    pub(crate) at: u32,
    pub(crate) cut: Cut,
    pub(crate) rows: u32,
}

/// Each of `tasks` (Bodies tasks) as a body's row at a unit, sorted by name, each once: named
/// from the tasks' files' outlines, their bodies' cuts (engine_plan_cfg.rs `file_cuts`) and
/// `reach` exactly as the plan named them (engine_bodies.rs `plan_views` under the same `P`). A
/// task that is no row of a body of its file under `reach` is the plan's and the setup's
/// disagreement: `Err` with its name (rule 3). In `g`.
pub(crate) fn task_rows<P: ViewPlan>(tasks: &[Task], outlined: &[Outlined<'_>], reach: &Reach, g: blaze_own::Alloc<'static>) -> Result<AVec<'static, TaskRow>, Name> {
    let mut files: AVec<'static, Name> = Vec::with_capacity_in(tasks.len(), g);
    files.extend(tasks.iter().map(|task| task.planned.file));
    files.sort_unstable();
    files.dedup();
    let mut all: AVec<'static, TaskRow> = Vec::new_in(g);
    let mut views: AVec<'static, ViewKey> = Vec::new_in(g);
    let mut ranks: AVec<'static, u32> = Vec::new_in(g);
    for file in files.iter().copied() {
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&file)) else { continue };
        let Ok(record) = &outlined[at].made else { continue };
        let reached = !reach.ranks(file).is_empty();
        let cuts = file_cuts(&outlined[at], g);
        for part in record.parts.iter().filter(|part| matches!((part.name.kind, part.locate), (Kind::BodyCheck, Locate::Source(_)))) {
            let cut = part_cut(cuts.as_ref(), part);
            ranks.clear();
            ranks.extend(reach.body_ranks(file, cut));
            reach.views_into(file, cut, &mut views);
            let rows = u32::try_from(ranks.len()).unwrap_or(u32::MAX);
            if !reached {
                all.push(TaskRow { name: part.name, base: part.name, file, rank: NO_UNIT, at: 0, cut, rows: 1 });
            }
            for (i, rank) in ranks.iter().enumerate() {
                all.push(TaskRow { name: at_view(part.name, P::view_of(&views, i)), base: part.name, file, rank: *rank, at: u32::try_from(i).unwrap_or(u32::MAX), cut, rows });
            }
        }
    }
    all.sort_unstable_by_key(|row| row.name);
    let mut rows: AVec<'static, TaskRow> = Vec::with_capacity_in(tasks.len(), g);
    for task in tasks.iter() {
        let at = all.binary_search_by_key(&task.planned.name, |row| row.name).map_err(|_| task.planned.name)?;
        rows.push(all[at]);
    }
    rows.sort_unstable_by_key(|row| row.name);
    rows.dedup_by_key(|row| row.name);
    Ok(rows)
}

/// The row named `name` among `rows` (sorted by name: [`task_rows`]).
pub(crate) fn row_named(rows: &[TaskRow], name: Name) -> Option<&TaskRow> {
    rows.binary_search_by_key(&name, |row| row.name).ok().map(|at| &rows[at])
}

/// The out of a body no unit holds: a function of its name alone (its reads say why).
pub(crate) fn outside_out(name: Name) -> ContentId {
    ContentId::of(OUTSIDE_DOMAIN, &name.subject.0)
}

/// Why a body is no unit's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OutsideIs {
    /// No unit holds its file.
    FileNotHeld,
    /// Units hold its file, none holds its entry.
    EntryAbsent,
}

/// The bodies of this request no unit holds, by name: said once a file, counted.
pub(crate) struct Outside {
    /// (name, file outline index, why), sorted by name after [`Outside::finish`].
    bodies: Vec<(Name, u32, OutsideIs), Global>,
    /// The entered rows whose unit keeps them only because plan time could not decide their
    /// entry's cfg under its `test` ([`Reach::undecided`]): the profile's "undecided at plan
    /// time: N".
    pub(crate) undecided: u32,
}

/// The body rows a request entered, by view (D191): base rows (a body's first unit, and every
/// body one unit reaches) and view rows (each other unit's), each checked (its check ran to a
/// verdict) or not (refused, no unit's, not held by its unit, failed): the plan's line.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct RowsByView {
    pub(crate) base_checked: u32,
    pub(crate) base_not: u32,
    pub(crate) view_checked: u32,
    pub(crate) view_not: u32,
}

impl Outside {
    pub(crate) fn new() -> Outside {
        Outside { bodies: Vec::new_in(Global), undecided: 0 }
    }

    pub(crate) fn push(&mut self, name: Name, file: u32, is: OutsideIs) {
        self.bodies.push((name, file, is));
    }

    /// Sorted by name, each once: after the phase.
    pub(crate) fn finish(&mut self) {
        self.bodies.sort_unstable_by_key(|one| one.0);
        self.bodies.dedup_by_key(|one| one.0);
    }

    /// How many bodies are no unit's.
    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.bodies.len()).unwrap_or(u32::MAX)
    }

    /// The outlines no unit holds this request, by index, sorted, each once. In `frame`.
    fn not_held<'f>(&self, frame: &'f Frame) -> Vec<u32, &'f Frame> {
        let mut files: Vec<u32, &'f Frame> = Vec::new_in(frame);
        files.extend(self.bodies.iter().filter(|one| one.2 == OutsideIs::FileNotHeld).map(|one| one.1));
        files.sort_unstable();
        files.dedup();
        files
    }

    /// The FILES no unit holds, each by its first outline with bodies, sorted: a file is no
    /// unit's when every outline of it ([`by_file`]: one a name it is planned by,
    /// [`outline_at_path`]) is. A file one crate holds by one name and another crate cuts by
    /// another is checked, by the first, and never noted (D191). In `frame`.
    fn files<'f>(&self, outlined: &[Outlined<'_>], frame: &'f Frame) -> Vec<u32, &'f Frame> {
        let not_held = self.not_held(frame);
        let mut files: Vec<u32, &'f Frame> = Vec::new_in(frame);
        for file in by_file(outlined, frame).chunk_by(|a, b| a.0 == b.0) {
            if file.iter().all(|one| not_held.binary_search(&one.1).is_ok()) {
                files.push(file[0].1);
            }
        }
        files.sort_unstable();
        files
    }

    /// The plan's notes into the request's lines: for each FILE with bodies, one block under
    /// [`note_name`] of its path, decided at its first outline ([`by_file`]: a file named twice
    /// has two outlines and one note). A file a body of which was `entered` this request is
    /// decided now (its note when no unit holds it, [`Outside::files`], else none); a file with
    /// none entered keeps the last request's block, if any: its bodies' rows stand, so nothing
    /// they read (each unit's items cell, [`Holders::cells`]) moved, and the fact is the same. A
    /// file named twice whose entered outlines no unit holds and whose others were not entered
    /// keeps its last block too: the others' rows stand, so the last request's word on the whole
    /// file is today's. No engine row and no finding carries it, so a request with nothing
    /// changed is certified and replays it from the lines record (engine_check.rs: the
    /// certificate prints that record's blocks alone).
    /// `unread`: the notes ([`note_name`]) of the files the request did not read back (the ready
    /// setup's: their outlines stand and no body of them was entered), each its last block, as a
    /// read-back outline with none entered keeps it.
    /// Its scratch is `frame`'s.
    pub(crate) fn lines<'a>(&self, entered: impl Iterator<Item = u32>, outlined: &[Outlined<'_>], unread: &[Name], last: Option<&crate::engine_lines::Last<'a, '_>>, lines: &mut crate::engine_lines::Lines<'a>, frame: &Frame) {
        for name in unread.iter() {
            if let Some(block) = last.and_then(|last| last.block(*name)) {
                lines.keep(*name, block);
            }
        }
        let mut entered_files: Vec<u32, &Frame> = Vec::new_in(frame);
        entered_files.extend(entered);
        entered_files.sort_unstable();
        entered_files.dedup();
        let files = self.files(outlined, frame);
        let not_held = self.not_held(frame);
        // Each file by its first outline, in outline order (the order the notes were always
        // put in): (first, entered any, entered all, every entered one not held).
        let mut decided: Vec<(u32, bool, bool, bool), &Frame> = Vec::new_in(frame);
        for file in by_file(outlined, frame).chunk_by(|a, b| a.0 == b.0) {
            let entered = |one: &(&[u8], u32)| entered_files.binary_search(&one.1).is_ok();
            let any = file.iter().any(entered);
            let all = file.iter().all(entered);
            let none_held = file.iter().filter(|one| entered(*one)).all(|one| not_held.binary_search(&one.1).is_ok());
            decided.push((file[0].1, any, all, none_held));
        }
        decided.sort_unstable_by_key(|one| one.0);
        for (at, any, all, none_held) in decided.iter().copied() {
            let Some(one) = outlined.get(at as usize) else { continue };
            let name = note_name(&one.path);
            if any && files.binary_search(&at).is_ok() {
                lines.note(name, crate::engine_lines::Class::Told, std::format!("{}", FileWords { outlined, file: at }).as_bytes());
            } else if (!any || (!all && none_held))
                && let Some(block) = last.and_then(|last| last.block(name))
            {
                lines.keep(name, block);
            }
        }
    }

    /// Every file's note, in outline order: what the command's tests read. Its scratch is
    /// `frame`'s.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>], frame: &Frame) -> std::vec::Vec<std::string::String, Global> {
        let mut notes = std::vec::Vec::new_in(Global);
        for file in self.files(outlined, frame).iter() {
            notes.push(std::format!("{}", FileWords { outlined, file: *file }));
        }
        notes
    }

    /// The plan's line of the profile: the body rows entered, by view ([`RowsByView`]), the
    /// entries no unit's records hold, the files no unit holds and their bodies, and the first
    /// files' paths (8 at most). Its scratch is `frame`'s.
    pub(crate) fn profile(&self, entered: usize, rows: RowsByView, outlined: &[Outlined<'_>], into: &mut blaze_own::String<&Frame>, frame: &Frame) {
        use core::fmt::Write as _;
        let absent = self.bodies.iter().filter(|one| one.2 == OutsideIs::EntryAbsent).count();
        let files = self.files(outlined, frame);
        // A file's bodies once: by its first outline (a file named twice has two).
        let in_files = self.bodies.iter().filter(|one| one.2 == OutsideIs::FileNotHeld && files.binary_search(&one.1).is_ok()).count();
        let _ = writeln!(
            into,
            "bodies: {entered} rows entered; {absent} entries of outlines not in their unit's records (cfg or macro); {} file(s) no unit holds (cfg or macro) ({in_files} bodies); base rows {} checked, {} not; view rows {} checked, {} not; undecided at plan time: {}",
            files.len(),
            rows.base_checked,
            rows.base_not,
            rows.view_checked,
            rows.view_not,
            self.undecided,
        );
        for file in files.iter().take(8) {
            let path = outlined.get(*file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let _ = writeln!(into, "bodies: no unit holds: {path}");
        }
    }
}

/// The name of the lines block that holds the plan's note of the file at `path`: no task's (its
/// domain is no task name's), stable across requests so a later one keeps it.
pub(crate) fn note_name(path: &[u8]) -> Name {
    Name::new(Kind::BodyCheck, ContentId::of(NOTE_DOMAIN, path))
}

/// How many body parts outline `file` has.
fn bodies_of(outlined: &[Outlined<'_>], file: u32) -> usize {
    outlined.get(file as usize).and_then(|one| one.made.as_ref().ok()).map_or(0, |record| record.parts.iter().filter(|part| matches!((part.name.kind, part.locate), (Kind::BodyCheck, Locate::Source(_)))).count())
}

/// The outlines with bodies, each with its path, sorted by (path, index): a run of one path is
/// one file's outlines, one a name it is planned by ([`outline_at_path`]).
fn by_file<'o, 'f>(outlined: &'o [Outlined<'_>], frame: &'f Frame) -> Vec<(&'o [u8], u32), &'f Frame> {
    let mut files: Vec<(&'o [u8], u32), &'f Frame> = Vec::with_capacity_in(outlined.len(), frame);
    for (at, one) in outlined.iter().enumerate() {
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        if bodies_of(outlined, at) > 0 {
            files.push((one.path.as_slice(), at));
        }
    }
    files.sort_unstable();
    files
}

/// The note of a file no unit holds. It says what the plan knows: no unit whose module tree
/// reaches the file has it in its items record (engine_bodies.rs `groups`, the not-held claim),
/// and the ways that is so, never one of them as the cause (a `#[cfg]` is not known to be it:
/// a macro call whose expansion did not write its `mod` is the same claim). Each `mod` that
/// names the file is named, of every outline of it (a file named twice, [`by_file`]).
struct FileWords<'o, 'h> {
    outlined: &'o [Outlined<'h>],
    file: u32,
}

impl fmt::Display for FileWords<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.outlined.get(self.file as usize).map_or(&b"?"[..], |one| one.path.as_slice());
        let count = bodies_of(self.outlined, self.file);
        // The files whose outlines list an outline of this file as a file part (`outlined` is
        // sorted by name): where each of its `mod`s is written, each once, in outline order.
        let outlined = self.outlined;
        let names_it = |one: &Outlined<'_>| {
            one.made.as_ref().is_ok_and(|record| {
                record.parts.iter().any(|part| {
                    matches!(part.locate, Locate::File { edition } if !crate::data_include::is_data(edition))
                        && outlined.binary_search_by(|o| o.name.cmp(&part.name)).ok().and_then(|at| outlined.get(at)).is_some_and(|o| o.path.as_slice() == path)
                })
            })
        };
        let text = core::str::from_utf8(path).unwrap_or("?");
        f.write_fmt(format_args!("`{text}`: no unit checks this file: no unit whose module tree reaches it ("))?;
        let mut said = 0u32;
        for (at, one) in outlined.iter().enumerate() {
            // A parent file named twice (two outlines) is said at its first.
            if !names_it(one) || outlined[..at].iter().any(|before| before.path == one.path && names_it(before)) {
                continue;
            }
            let parent = core::str::from_utf8(&one.path).unwrap_or("?");
            f.write_fmt(format_args!("{}the `mod` in `{parent}`", if said == 0 { "by " } else { ", or " }))?;
            said += 1;
        }
        if said == 0 {
            f.write_str("by the `mod` that names it")?;
        }
        write!(
            f,
            ") has it in its items record, which is so when a `#[cfg]` on that `mod` or in the file is false in the unit (a feature off, `test`, another target), or when the `mod` is in a macro call whose expansion did not write it; its {count} body(ies) are no unit's: not checked, and not counted among the items blaze could not check ({RULE})"
        )
    }
}

/// A body as a note names it (rule 19): its item's name as written, or, when it was not found
/// in any records, its place in its file as the outline gave it.
pub(crate) struct Named<'l> {
    label: &'l str,
    place: Option<u32>,
}

impl<'l> Named<'l> {
    /// Body `name` of outline `file`, labelled `label` ("" when no records named it).
    pub(crate) fn of(label: &'l str, outlined: &[Outlined<'_>], file: u32, name: Name) -> Named<'l> {
        let place = match label.is_empty() {
            true => outlined.get(file as usize).and_then(|one| one.made.as_ref().ok()).and_then(|record| record.parts.iter().find(|part| part.name == name)).map(|part| part.place),
            false => None,
        };
        Named { label, place }
    }
}

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.label.is_empty(), self.place) {
            (false, _) => f.write_str(self.label),
            (true, Some(place)) => write!(f, "{{the item at byte {place} of its file}}"),
            (true, None) => f.write_str("{an item its file's outline no longer lists}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_engine::pages::FilePlace;

    #[test]
    fn found_edges_fans_out_to_all_units_sharing_root_and_kind() {
        let heap = blaze_own::testing::heap();
        let root_a = Name::file(b"crates/shared/src/lib.rs");
        let root_b = Name::file(b"crates/other/src/lib.rs");
        let found_a = Name::file(b"crates/shared/src/generated.rs");
        let found_b = Name::file(b"crates/other/src/generated.rs");
        let unit_a_target = Name::new(Kind::CrateScope, ContentId([1; 32]));
        let unit_a_macro = Name::new(Kind::CrateScope, ContentId([2; 32]));
        let unit_a_test = Name::new(Kind::CrateScope, ContentId([3; 32]));
        let unit_b_lib = Name::new(Kind::CrateScope, ContentId([4; 32]));

        let mut units = [
            (root_a, false, unit_a_target),
            (root_a, false, unit_a_macro),
            (root_a, true, unit_a_test),
            (root_b, false, unit_b_lib),
        ];
        units.sort_unstable();

        // 1. A found file on root_a (non-test) fans out to BOTH non-test units sharing root_a,
        //    never to the test unit on root_a or to root_b.
        let places = [(found_a, FilePlace { unit: root_a, edition: 0, flags: crate::engine_check::PLACE_FOUND_LIB })];
        let edges = found_edges(&units, &places, heap.alloc());
        let mut expected = [
            FoundEdge { unit: unit_a_target, file: found_a },
            FoundEdge { unit: unit_a_macro, file: found_a },
        ];
        expected.sort_unstable();
        assert_eq!(edges.as_slice(), expected.as_slice(), "both non-test units sharing root_a receive the edge");

        // 2. A found file on root_b alone is unaffected by root_a having two non-test units.
        let places_b = [(found_b, FilePlace { unit: root_b, edition: 0, flags: crate::engine_check::PLACE_FOUND_LIB })];
        let edges_b = found_edges(&units, &places_b, heap.alloc());
        assert_eq!(edges_b.as_slice(), &[FoundEdge { unit: unit_b_lib, file: found_b }]);

        // 3. Both flags set on root_a emit edges for both non-test units and the test unit.
        let places_both = [(found_a, FilePlace { unit: root_a, edition: 0, flags: crate::engine_check::PLACE_FOUND })];
        let edges_both = found_edges(&units, &places_both, heap.alloc());
        let mut expected_both = [
            FoundEdge { unit: unit_a_target, file: found_a },
            FoundEdge { unit: unit_a_macro, file: found_a },
            FoundEdge { unit: unit_a_test, file: found_a },
        ];
        expected_both.sort_unstable();
        assert_eq!(edges_both.as_slice(), expected_both.as_slice());
    }
}
