//! Which unit of a crate checks each body: decided from the units' records alone (lane RP1).
//!
//! A package's units are crates of their own (the crate list's rows: the lib, each bin, the lib's
//! test harness, each integration test), each with its cfg set (blaze-unit-graph resolve.rs
//! `unit_cfg`: `test` on a test harness, `feature = ".."` for each feature on). Which files and
//! items a unit holds is its names task's decision, recorded in its items record (its file rows,
//! and its items under the unit's cfg). The planner finds files forward from text, cfg-blind
//! (engine_check.rs `plan`; engine_bodies.rs `reached`): `#[cfg(test)] mod tests;` names
//! `src/tests.rs` for every unit of the crate, and the lib unit does not hold it.
//!
//! THE RULE. A body is checked by a unit exactly when that unit's records hold it; among the
//! units that hold it, by the first in rank ([`Holders::new`]: non-test units first, then the
//! crate list's order), once (a body task is named by its outline entry, never by its unit: one
//! task, one check). What no unit holds is no unit's body, never a refusal:
//! - a file no unit's items record lists (a `cfg` on its `mod` cut it in every unit): its bodies
//!   are no unit's, each a row with no finding; ONE note for the file, a block of the lines
//!   record under the file's own name with no engine row (so a certificate replays it and no
//!   finding stops one), counted ([`Outside::lines`]);
//! - an entry of a held file no holding unit's records hold (a `cfg` inside the file, or a macro
//!   that replaced it): no unit's body; counted in the plan's line, no note each.
//!
//! A unit's refusal (its records could not be read) is a refusal only where no unit holds the
//! body: a holding unit checks it, and the refusal's cells are among its reads, so the refusing
//! unit's fix enters it again (rank decides who checks, and a fresh run must agree).
//!
//! WHAT A DECISION READS (rule 21). Who checks a body is a function of every unit ranked before
//! the one that checks it: each of those that does not hold it is read by its items section's
//! cell (`NameCell::Whole { section: Items }`, which moves when its file rows or items move: a
//! `#[cfg(test)]` taken off `mod tests;` adds the file to the lib unit's rows), or by its names
//! record whole when its crate has no cell table ([`Holders::cells`]). The unit that checks a body
//! reads its own records through the checker, so its losing the body moves those reads.
//!
//! A body more than one unit holds is checked under the first alone; where the units' facts can
//! differ (`cfg!(test)`, a `#[cfg(test)]` statement item) the second unit's view is not checked
//! yet: counted in the plan's line ("held by 2 units"), the per-unit check the next piece.

use core::fmt;

use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Kind, Locate, Name};
use blaze_engine::{CellId, View};
use blaze_ids::ContentId;
use blaze_own::Vec;
use blaze_scope::names_cells::{NameCell, Section};
use bstd::mem::Frame;
use std::alloc::Global;

use crate::engine_tasks::{CELLS, Outlined, WHOLE};

/// Where the rule is (rule 19).
const RULE: &str = "engine_body_plan.rs";

/// The words of an entry a unit holds the file of, but whose records hold no body at the entry
/// (engine_bodies.rs `groups`): not that unit's body ([`BodyIs::Absent`]), never a refusal.
pub(crate) const ENTRY_ABSENT: &str = "no body of its crate's records is at this entry (a `cfg` left it out, or a macro replaced it)";

/// The refusal of a planned body whose file a unit holds, but whose records' walk of the file
/// listed no entry at its name: a fault of the plan or the records, named (rule 3).
pub(crate) const NO_ENTRY: &str = "its crate's records list no entry with a body at this name, though they hold its file (engine_body_plan.rs `decide`)";

/// The domain of the out of a body no unit holds.
const OUTSIDE_DOMAIN: &str = "blaze-cli engine bodies no unit's 1";

/// The domain of a plan note's block name ([`note_name`]).
const NOTE_DOMAIN: &str = "blaze-cli engine body plan file note 1";

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

/// One unit's word on one file (an outline, by index).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FileClaim<'f> {
    pub(crate) at: u32,
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

/// Who checks one body ([`Holders::decide`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Decision {
    /// No member unit's module tree reaches its file.
    Unreached,
    /// The first unit in rank whose records hold it checks it (`took`: its claim's index).
    Taken { took: u32, unit: u32 },
    /// No unit holds it and checks it, and one refused it: the first refusal in rank.
    Refused { why: &'static str, took: Option<u32> },
    /// Units hold its file, none holds the entry (a `cfg` in the file, or a macro replaced it).
    Absent { took: Option<u32> },
    /// No unit holds its file.
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
        files.sort_by_key(|one| (one.at, rank_of(one.unit)));
        files.dedup_by_key(|one| (one.at, one.unit));
        let mut claims: Vec<BodyClaim, Global> = Vec::new_in(Global);
        claims.extend(bodies);
        claims.sort_by_key(|one| (one.name, rank_of(one.unit)));
        claims.dedup_by_key(|one| (one.name, one.unit));
        Holders { units, files, bodies: claims }
    }

    /// Who checks body `name` of file `at` (an outline index). `passed` gets every unit ranked
    /// before the decider that does not hold the body (or every unit, when none checks it), and
    /// `reads` the cells of every refusal met: together what the decision is a function of
    /// ([`Holders::cells`]).
    pub(crate) fn decide(&self, name: Name, at: u32, passed: &mut Vec<u32, Global>, reads: &mut Vec<CellId, Global>) -> Decision {
        passed.clear();
        reads.clear();
        let from = self.files.partition_point(|one| one.at < at);
        let bodies_from = self.bodies.partition_point(|one| one.name < name);
        let bodies = &self.bodies[bodies_from..];
        let bodies = &bodies[..bodies.partition_point(|one| one.name == name)];
        let (mut reached, mut held, mut refused, mut absent_took) = (false, false, None, None);
        for file in self.files[from..].iter().take_while(|one| one.at == at) {
            reached = true;
            match file.is {
                FileIs::NotHeld => passed.push(file.unit),
                FileIs::Refused(why) => {
                    refused.get_or_insert((why, None));
                    reads.extend_from_slice(file.reads);
                }
                FileIs::Held => {
                    held = true;
                    match bodies.iter().find(|one| one.unit == file.unit) {
                        Some(BodyClaim { is: BodyIs::Held, took, .. }) => return Decision::Taken { took: *took, unit: file.unit },
                        Some(BodyClaim { is: BodyIs::Absent, took, .. }) => {
                            absent_took.get_or_insert(*took);
                            passed.push(file.unit);
                        }
                        Some(BodyClaim { is: BodyIs::Refused(why), took, .. }) => {
                            refused.get_or_insert((*why, Some(*took)));
                            reads.extend_from_slice(file.reads);
                        }
                        None => {
                            refused.get_or_insert((NO_ENTRY, None));
                            reads.extend_from_slice(file.reads);
                        }
                    }
                }
            }
        }
        match (reached, refused, held) {
            (false, _, _) => Decision::Unreached,
            (true, Some((why, took)), _) => Decision::Refused { why, took },
            (true, None, true) => Decision::Absent { took: absent_took },
            (true, None, false) => Decision::NotHeld,
        }
    }

    /// Whether more than one unit's records hold body `name`: its unit facts (`cfg!(test)` in
    /// it, a `#[cfg(test)]` statement) may differ between them, and only the first checks it.
    pub(crate) fn held_by_more(&self, name: Name) -> bool {
        let from = self.bodies.partition_point(|one| one.name < name);
        self.bodies[from..].iter().take_while(|one| one.name == name).filter(|one| one.is == BodyIs::Held).nth(1).is_some()
    }

    /// The cell that says unit `unit` does not hold a file or an entry ([`not_held_cell`]).
    pub(crate) fn not_held_cell(&self, view: View<'_, '_>, unit: u32) -> Option<CellId> {
        self.units.get(unit as usize).map(|one| not_held_cell(view, one.name, one.prefix))
    }

    /// What a decision is a function of: each `passed` unit's not-held cell and the refusals'
    /// `reads`, sorted, each once, in the request's heap (`g`).
    pub(crate) fn cells(&self, view: View<'_, '_>, passed: &[u32], reads: &[CellId], g: blaze_own::Alloc<'static>) -> &'f [CellId] {
        let mut ids: Vec<CellId, blaze_own::Alloc<'static>> = Vec::with_capacity_in(passed.len() + reads.len(), g);
        ids.extend(passed.iter().filter_map(|unit| self.not_held_cell(view, *unit)));
        ids.extend_from_slice(reads);
        ids.sort_unstable();
        ids.dedup();
        &*ids.leak()
    }
}

/// THE ONE HOME of "which cell says a unit does not hold a file": the unit's items section's
/// cell when its crate's names task (`name`, of crate prefix `prefix`) defines a cell table
/// ([`CELLS`]), else its names record whole ([`WHOLE`]), which that section is part of. Both
/// rules that turn on a unit holding a file read it: who checks a body ([`Holders`]) and under
/// which units a head task freezes its heads (engine_heads.rs `run_items`, lane RU2): a unit
/// that comes to hold the file (a `#[cfg(test)]` taken off its `mod`) moves this cell, so the
/// row that skipped that unit is entered again (rule 21).
pub(crate) fn not_held_cell(view: View<'_, '_>, name: Name, prefix: [u8; 8]) -> CellId {
    let table = CellId { of: name, cell: CELLS };
    let cell = match view.cell(&table) != ABSENT {
        true => Cell(NameCell::Whole { krate: prefix, section: Section::Items }.key()),
        false => WHOLE,
    };
    CellId { of: name, cell }
}

/// The outlines (by index, sorted) a crate's module tree reaches from its root file `root`: the
/// root and every file a reached outline names as a file part, transitively, as the planner
/// walked them (engine_check.rs `plan`), cfg-blind: a file two crates reach is in both crates'
/// walks, and a unit whose cfg cuts a `mod` still reaches its file (whether it HOLDS it is its
/// items record's word, [`not_held_cell`]). In `g`.
pub(crate) fn reached(outlined: &[Outlined<'_>], root: Name, g: blaze_own::Alloc<'static>) -> Vec<u32, blaze_own::Alloc<'static>> {
    let mut seen: Vec<u32, blaze_own::Alloc<'static>> = Vec::new_in(g);
    let mut next: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
    next.push(root);
    while let Some(name) = next.pop() {
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name)) else { continue };
        let at32 = u32::try_from(at).unwrap_or(u32::MAX);
        let Err(slot) = seen.binary_search(&at32) else { continue };
        seen.insert_shift(slot, at32);
        if let Ok(record) = &outlined[at].made {
            next.extend(record.parts.iter().filter(|part| matches!(part.locate, Locate::File { .. })).map(|part| part.name));
        }
    }
    seen
}

/// `cells` added to a checked body's reads (each with the digest the view holds), sorted, each
/// once: the units ranked before the one that checked it, which a fresh run would ask first.
pub(crate) fn add_reads<A: core::alloc::Allocator>(reads: &mut Vec<(CellId, ContentId), A>, cells: &[CellId], view: View<'_, '_>) {
    if cells.is_empty() {
        return;
    }
    for id in cells.iter() {
        reads.push((*id, view.cell(id)));
    }
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
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
    /// Bodies entered this request that more than one unit's records hold, checked under the
    /// first alone ([`Holders::held_by_more`]): the gap the per-unit check will close.
    pub(crate) shared: u32,
}

impl Outside {
    pub(crate) fn new() -> Outside {
        Outside { bodies: Vec::new_in(Global), shared: 0 }
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

    /// The files no unit holds, by outline index, sorted, each once.
    fn files(&self) -> Vec<u32, Global> {
        let mut files: Vec<u32, Global> = Vec::new_in(Global);
        files.extend(self.bodies.iter().filter(|one| one.2 == OutsideIs::FileNotHeld).map(|one| one.1));
        files.sort_unstable();
        files.dedup();
        files
    }

    /// The plan's notes into the request's lines: for each outline with bodies, a block under
    /// [`note_name`] of its path. A file with a body `entered` this request is decided now (its
    /// note when no unit holds it, else none); a file with none entered keeps the last request's
    /// block, if any: its bodies' rows stand, so nothing they read (each unit's items cell,
    /// [`Holders::cells`]) moved, and the fact is the same. No engine row and no finding carries
    /// it, so a request with nothing changed is certified and replays it from the lines record
    /// (engine_check.rs: the certificate prints that record's blocks alone).
    pub(crate) fn lines(&self, entered: impl Iterator<Item = u32>, outlined: &[Outlined<'_>], last: Option<&crate::engine_lines::Last<'_, '_>>, lines: &mut crate::engine_lines::Lines<'_>) {
        let mut entered_files: Vec<u32, Global> = Vec::new_in(Global);
        entered_files.extend(entered);
        entered_files.sort_unstable();
        entered_files.dedup();
        let files = self.files();
        for (at, one) in outlined.iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            if bodies_of(outlined, at) == 0 {
                continue;
            }
            let name = note_name(&one.path);
            if entered_files.binary_search(&at).is_ok() {
                if files.binary_search(&at).is_ok() {
                    lines.note(name, crate::engine_lines::Class::Told, std::format!("{}", FileWords { outlined, file: at }).as_bytes());
                }
            } else if let Some(block) = last.and_then(|last| last.block(name)) {
                lines.keep(name, block);
            }
        }
    }

    /// Every file's note, in outline order: what the command's tests read.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>]) -> std::vec::Vec<std::string::String, Global> {
        let mut notes = std::vec::Vec::new_in(Global);
        for file in self.files().iter() {
            notes.push(std::format!("{}", FileWords { outlined, file: *file }));
        }
        notes
    }

    /// The plan's line of the profile: the bodies planned, the entries no unit's records hold,
    /// the files no unit holds and their bodies, and the first files' paths (8 at most).
    pub(crate) fn profile(&self, planned: usize, outlined: &[Outlined<'_>], into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let absent = self.bodies.iter().filter(|one| one.2 == OutsideIs::EntryAbsent).count();
        let in_files = self.bodies.iter().filter(|one| one.2 == OutsideIs::FileNotHeld).count();
        let files = self.files();
        let _ = writeln!(
            into,
            "bodies: {planned} planned; {absent} entries of outlines not in their unit's records (cfg or macro); {} file(s) no unit holds under its cfg ({in_files} bodies); {} bodies held by 2 units, checked under the first",
            files.len(),
            self.shared
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

/// The note of a file no unit holds.
struct FileWords<'o, 'h> {
    outlined: &'o [Outlined<'h>],
    file: u32,
}

impl fmt::Display for FileWords<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.outlined.get(self.file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
        let count = bodies_of(self.outlined, self.file);
        write!(
            f,
            "`{path}`: no unit of its crate holds this file under its cfg (its crate's module tree names it, but no unit's items record lists it: a `#[cfg]` on its `mod`, `test` or a feature off, cut it in every unit); its {count} body(ies) are no unit's and are not checked ({RULE})"
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
