//! An edit's names, by group (engine.md piece E4, the incremental half): after an edit, a
//! crate's names are brought up to date by re-solving ONLY the groups whose inputs moved, every
//! other group kept as its record, the result byte-equal to a fresh whole-crate solve of the
//! edited text (tests/names_groups_edit.rs is the proof).
//!
//! The driver reads the records alone (groups.rs `NameGroup`): each group's members with their
//! own-text digests, the cells it `defines` and the cells it read (`cells_read`, each with the
//! digest it saw). It enters:
//!
//! - each group a member of which has another own text now (or is gone), and each module that is
//!   in no group (a `mod` line added);
//! - each group that read a cell that moved: a cell an entered group defines with another digest
//!   now (or no longer), or a cell outside the crate's groups the caller says moved (a
//!   dependency's names, a call's expansion: [`Edit::moved`]); a reader whose recorded digest
//!   equals the cell's value now is not entered.
//!
//! It stops where a re-solved group defines what its old record did (equal bytes write equal
//! defines). A group is asked of the [`GroupSolver`] (the one-group solve), which may answer
//! with several groups (a split) or with a group holding modules outside the ones asked (a
//! merge: an import that closed a cycle); the old groups those modules were in are replaced. A
//! cycle the records show between a new group and the groups it reads (a read added that closes
//! one, which a solve of one side alone cannot see) is a merge the driver finds itself and asks
//! as one group. A solver that cannot solve a group alone refuses by type
//! ([`EditError::SolveWhole`]): the caller then solves the crate whole, a planned path.

#![blaze::sans_io]

use blaze_id::{ContentId, ItemKey};
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc};

use super::groups::{ABSENT, NameGroup, RecordError, globs_cell, scope_in};

/// The one-group solve the driver asks: `members`' scopes from their text and, for every other
/// module of the crate, the record of the group that holds it now (`records`).
pub trait GroupSolver<'a> {
    /// Why a group could not be solved alone (the caller then solves the crate whole).
    type Refusal: core::fmt::Debug;

    /// The groups `members` are in now, each with its record: together they hold every one of
    /// `members` (a split answers with several), and may hold others (a merge).
    fn solve(&mut self, members: &[ItemKey], records: &dyn GroupRecords) -> Result<AVec<'a, NameGroup<'a>>, Self::Refusal>;
}

/// The crate's group records as they stand now, by module: what a one-group solve answers a
/// module outside its group by.
pub trait GroupRecords {
    /// The record of the group that holds `module` now; `None` for a module in no group (one
    /// just added) or of another crate.
    fn group_of(&self, module: ItemKey) -> Option<GroupRef<'_>>;
}

/// A group record, borrowed ([`NameGroup`]'s fields a one-group solve reads).
#[derive(Clone, Copy, Debug)]
pub struct GroupRef<'r> {
    pub members: &'r [ItemKey],
    pub cells_read: &'r [(ItemKey, [u8; 16], ContentId)],
    pub defines: &'r [([u8; 16], ContentId)],
    pub bytes: &'r [u8],
}

impl<'r> GroupRef<'r> {
    pub fn of(g: &'r NameGroup<'_>) -> GroupRef<'r> {
        GroupRef { members: &g.members, cells_read: &g.cells_read, defines: &g.defines, bytes: &g.bytes }
    }

    /// `member`'s finished scope (`ModuleScope` bytes) in the record.
    pub fn scope_of(&self, member: ItemKey) -> Result<&'r [u8], RecordError> {
        scope_in(self.members, self.bytes, member)
    }

    /// The digest of `member`'s globs cell as the record defines it.
    pub fn globs_of(&self, member: ItemKey) -> Option<ContentId> {
        let cell = globs_cell(member);
        self.defines.binary_search_by_key(&cell, |d| d.0).ok().map(|i| self.defines[i].1)
    }

    /// The digest of `member`'s textual cell as the record defines it.
    pub fn textual_of(&self, member: ItemKey) -> Option<ContentId> {
        let cell = super::groups::textual_cell(member);
        self.defines.binary_search_by_key(&cell, |d| d.0).ok().map(|i| self.defines[i].1)
    }
}

/// What a crate solve is asked to solve alone ([`crate::task::Par::names_members`]): the
/// members, sorted, and the records every other module is answered from.
#[derive(Clone, Copy)]
pub struct Members<'r> {
    pub members: &'r [ItemKey],
    pub records: &'r dyn GroupRecords,
    /// The crate's module tree record ([`crate::tree_record`]) as the records were written
    /// with: the solve then reads, outlines and lowers the members' files alone. `None`: the
    /// caller holds none, and the solve walks and lowers every file to make the tree.
    pub tree: Option<&'r [u8]>,
}

impl Members<'_> {
    pub fn holds(&self, module: ItemKey) -> bool {
        self.members.binary_search(&module).is_ok()
    }
}

/// Why a crate solve could not solve its members alone, naming the module (rule 19): the
/// planned path to a whole solve ([`EditError::SolveWhole`]), never a failure of the crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemberRefusal {
    /// A `macro_rules!` lookup walked into the text of `module`, outside the group (its
    /// record's `textual` and `marks` are not read yet).
    Textual { module: ItemKey },
    /// A member read `module`, outside the group and in no record.
    NoRecord { module: ItemKey },
    /// `module`'s record did not give its scope.
    Record { module: ItemKey, error: RecordError },
    /// `module`'s scope in its record did not read.
    Scope { module: ItemKey },
    /// `module`'s record defines no globs cell of it.
    NoGlobsCell { module: ItemKey },
    /// `module`'s scope holds `#[macro_export]` macros an expansion wrote (published to the
    /// root's scope, not read from a record yet).
    MacroExports { module: ItemKey },
    /// `module`'s scope holds an ambiguous binding of its own.
    Ambiguous { module: ItemKey },
    /// A glob edge of `module` (filled from its record) names `source`, which is no module or
    /// enum held here. A module of the crate is always held (the tree record has every one;
    /// one with no record yet is then read as [`MemberRefusal::NoRecord`], a demand), and an
    /// enum its text declares is the tree record's (`TreeRecord::enums`); what is left is an
    /// enum an expansion wrote in a module outside the members, or a dependency's module or
    /// enum its names do not hold: a typed refusal (rule 3).
    GlobSource { module: ItemKey, source: ItemKey },
    /// D116's check re-decides the whole crate: never asked with members.
    DecideCheck,
    /// The module tree record handed in did not read.
    TreeRecord { error: crate::tree_record::TreeRecordError },
    /// The tree record's crate root did not read.
    TreeCrateRoot,
}

impl MemberRefusal {
    /// The module the refusal names (its path is printed with it: rule 19).
    pub fn module(&self) -> Option<ItemKey> {
        match *self {
            MemberRefusal::Textual { module }
            | MemberRefusal::NoRecord { module }
            | MemberRefusal::Record { module, .. }
            | MemberRefusal::Scope { module }
            | MemberRefusal::NoGlobsCell { module }
            | MemberRefusal::MacroExports { module }
            | MemberRefusal::Ambiguous { module }
            | MemberRefusal::GlobSource { module, .. } => Some(module),
            MemberRefusal::DecideCheck | MemberRefusal::TreeRecord { .. } | MemberRefusal::TreeCrateRoot => None,
        }
    }
}

/// What a crate solve asked with [`Members`] answers: the members' groups, and the module tree
/// record the solve ended with: the one handed in when the members matched their rows, or the
/// one it made when the record's inputs moved and it walked the tree again (tree_record.rs).
#[derive(Debug)]
pub struct Solved<'a> {
    pub groups: AVec<'a, NameGroup<'a>>,
    pub tree: &'a [u8],
}

/// The one-group solve over a crate solve: `solve_crate` solves the members it is given from
/// their text, every other module from `records` ([`Members`]), and answers with the members'
/// groups. A member that reads a module outside whose group reads back into the members (an
/// import that closed a cycle) makes a group with it: the members are widened by those groups
/// and solved again, until no read leaves the members and comes back. `alloc` holds the
/// widened members.
pub struct OneGroup<'a, F> {
    pub solve_crate: F,
    pub alloc: Alloc<'a>,
    /// The crate's module tree record, handed to each crate solve ([`Members::tree`]); after
    /// each solve, the record it ended with ([`Solved::tree`]). So a solve that walked again
    /// (a member gave a module its first `macro_rules!`, or a `mod` line) hands the new record
    /// to every later solve of the same edit, never the stale one; after the driver returns it
    /// is the crate's newest record, the one the caller keeps.
    pub tree: Option<&'a [u8]>,
}

/// Why [`OneGroup`] did not solve a group alone.
#[derive(Debug)]
pub enum OneGroupRefusal<R> {
    /// The crate solve's refusal ([`MemberRefusal`] among them).
    Crate(R),
    /// The members kept widening past `most` rounds (rule 18).
    Widened { most: usize },
}

/// The most widenings one [`OneGroup::solve`] takes.
pub const MOST_WIDENINGS: usize = 64;

impl<'a, R: core::fmt::Debug, F: FnMut(Members<'_>) -> Result<Solved<'a>, R>> GroupSolver<'a> for OneGroup<'a, F> {
    type Refusal = OneGroupRefusal<R>;

    fn solve(&mut self, members: &[ItemKey], records: &dyn GroupRecords) -> Result<AVec<'a, NameGroup<'a>>, Self::Refusal> {
        let mut asked: AVec<'a, ItemKey> = AVec::from_slice_in(members, self.alloc);
        asked.sort_unstable();
        asked.dedup();
        for _ in 0..MOST_WIDENINGS {
            let Solved { groups: got, tree } = (self.solve_crate)(Members { members: &asked, records, tree: self.tree }).map_err(OneGroupRefusal::Crate)?;
            // The newest record goes on: the texts of one edit are fixed, so the record a solve
            // ended with is the tree of those texts.
            self.tree = Some(tree);
            let more = closing(&asked, &got, records, self.alloc);
            if more.is_empty() {
                return Ok(got);
            }
            asked.extend(more.iter().copied());
            asked.sort_unstable();
            asked.dedup();
        }
        Err(OneGroupRefusal::Widened { most: MOST_WIDENINGS })
    }
}

/// The modules outside `asked` on a cycle of reads with it: of the groups `got`'s reads reach
/// through the records, those that read back into `asked`. Each group visited once.
fn closing<'a>(asked: &[ItemKey], got: &[NameGroup<'a>], records: &dyn GroupRecords, alloc: Alloc<'a>) -> AVec<'a, ItemKey> {
    let inside = |k: ItemKey| asked.binary_search(&k).is_ok();
    // Forward: each group outside reached, by its first member.
    let mut reached: ABTreeMap<'a, ItemKey, GroupRef<'_>> = ABTreeMap::new_in(alloc);
    let mut todo: AVec<'a, ItemKey> = AVec::new_in(alloc);
    for g in got.iter() {
        todo.extend(g.cells_read.iter().map(|r| r.0).filter(|&t| !inside(t)));
    }
    while let Some(t) = todo.pop() {
        let Some(r) = records.group_of(t) else { continue };
        if reached.contains_key(&r.members[0]) || r.members.iter().any(|&m| inside(m)) {
            continue;
        }
        reached.insert(r.members[0], r);
        todo.extend(r.cells_read.iter().map(|x| x.0).filter(|&u| !inside(u)));
    }
    // Backward: those that read `asked`, or a group already on the cycle; a round per group.
    let mut on: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
    for _ in 0..=reached.len() {
        let mut grew = false;
        for (&first, r) in reached.iter() {
            if on.contains(&first) {
                continue;
            }
            let back = r.cells_read.iter().any(|x| inside(x.0) || records.group_of(x.0).is_some_and(|q| on.contains(&q.members[0])));
            if back {
                on.insert(first);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let mut out: AVec<'a, ItemKey> = AVec::new_in(alloc);
    for first in on.iter() {
        if let Some(r) = reached.get(first) {
            out.extend(r.members.iter().copied());
        }
    }
    out
}

/// An edit of one crate, as the records and the outlines show it.
#[derive(Clone, Copy, Debug)]
pub struct Edit<'r, 'a> {
    /// The crate's groups before the edit.
    pub old: &'r [NameGroup<'a>],
    /// Every module of the crate now with its own-text digest (groups.rs `own_digest`), sorted
    /// by key, each once.
    pub own: &'r [(ItemKey, ContentId)],
    /// Cells no group of this crate defines that moved, with their value now ([`ABSENT`] for
    /// none): a dependency's names, a call's expansion. Sorted by cell.
    pub moved: &'r [([u8; 16], ContentId)],
}

/// The crate's names after an edit.
#[derive(Debug)]
pub struct Edited<'a> {
    /// Every group now, sorted by first member.
    pub groups: AVec<'a, NameGroup<'a>>,
    /// The modules asked of each solve, in the order asked: one entry per group entered.
    pub entered: AVec<'a, AVec<'a, ItemKey>>,
    /// The groups of before that a result replaced (a re-cut replaces more than were asked).
    pub replaced: u32,
}

/// Why an edit's names were not brought up to date by group, naming the modules (rule 19).
#[derive(Debug)]
pub enum EditError<'a, R> {
    /// The solver could not solve `members` alone: the caller solves the crate whole.
    SolveWhole { members: AVec<'a, ItemKey>, refusal: R },
    /// The solver's answer left out `member`, one of those asked.
    Lost { member: ItemKey },
    /// `own` is not sorted by key, each once.
    OwnNotSorted { at: ItemKey },
    /// More groups were entered than [`most_entries`] allows (rule 18): a solver whose
    /// answers do not settle.
    TooManyEntries { most: usize },
}

/// The most groups one edit enters: each group and module of the crate a few times over (a
/// group is entered again only when a cell it read moved since, and cells settle).
pub fn most_entries(groups: usize, modules: usize) -> usize {
    4 * (groups + modules) + 16
}

/// A planted bug of the tests: the control the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditPlant {
    None,
    /// The first reader a moved cell would enter is skipped.
    SkipOneReader,
}

/// Brings a crate's names up to date after `edit`, entering only the groups whose inputs moved.
/// `alloc` holds the result and the walk's state.
pub fn re_solve<'a, S: GroupSolver<'a>>(edit: Edit<'_, 'a>, solver: &mut S, alloc: Alloc<'a>) -> Result<Edited<'a>, EditError<'a, S::Refusal>> {
    re_solve_planted(edit, solver, EditPlant::None, alloc)
}

/// The driver's state: the groups now, by slot (a replaced one is `None`).
struct Now<'a> {
    slots: AVec<'a, Option<NameGroup<'a>>>,
    /// Each module's slot.
    at: ABTreeMap<'a, ItemKey, usize>,
    /// The readers of each cell: a member of each group whose record read it (checked against
    /// the group's record when used: a group replaced since may read it no more).
    readers: ABTreeMap<'a, [u8; 16], AVec<'a, ItemKey>>,
    /// Each cell that moved, with its value now.
    latest: ABTreeMap<'a, [u8; 16], ContentId>,
    alloc: Alloc<'a>,
}

impl<'a> Now<'a> {
    fn add(&mut self, g: NameGroup<'a>) -> usize {
        let slot = self.slots.len();
        for &m in g.members.iter() {
            self.at.insert(m, slot);
        }
        let first = g.members[0];
        for &(_, cell, _) in g.cells_read.iter() {
            let alloc = self.alloc;
            let list = self.readers.entry(cell).or_insert_with(|| AVec::new_in(alloc));
            if list.last() != Some(&first) {
                list.push(first);
            }
        }
        self.slots.push(Some(g));
        slot
    }

    /// Whether the group at `slot` must be entered: a member's own text moved or is gone, or a
    /// cell it read holds another value now.
    fn stale(&self, slot: usize, own: &[(ItemKey, ContentId)]) -> bool {
        let Some(g) = self.slots[slot].as_ref() else { return false };
        let own_moved = g.members.iter().zip(g.own.iter()).any(|(m, o)| own_of(own, *m) != Some(*o));
        own_moved || g.cells_read.iter().any(|(_, cell, seen)| self.latest.get(cell).is_some_and(|now| now != seen))
    }

    /// The slots reachable from `from` by reads (a read of a cell of a module in another slot).
    fn reads_of(&self, from: usize, out: &mut AVec<'a, usize>) {
        out.clear();
        let Some(g) = self.slots[from].as_ref() else { return };
        for &(table, _, _) in g.cells_read.iter() {
            if let Some(&s) = self.at.get(&table)
                && s != from
            {
                out.push(s);
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// The slots on a cycle of reads through `slot` (itself first when there is one, else
    /// empty): the strongly connected component of the read graph that holds it, found from the
    /// records. Bounded by the slot count.
    fn cycle_through(&self, slot: usize) -> AVec<'a, usize> {
        let alloc = self.alloc;
        // Forward: every slot `slot` reaches.
        let mut reach: ABTreeSet<'a, usize> = ABTreeSet::new_in(alloc);
        let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
        let mut edges: AVec<'a, usize> = AVec::new_in(alloc);
        todo.push(slot);
        while let Some(s) = todo.pop() {
            self.reads_of(s, &mut edges);
            for &t in edges.iter() {
                if reach.insert(t) {
                    todo.push(t);
                }
            }
        }
        let mut out: AVec<'a, usize> = AVec::new_in(alloc);
        if !reach.contains(&slot) {
            return out;
        }
        // Backward within `reach`: those that reach `slot`; a round per slot at most.
        let mut back: ABTreeSet<'a, usize> = ABTreeSet::new_in(alloc);
        back.insert(slot);
        for _ in 0..=reach.len() {
            let mut grew = false;
            for &s in reach.iter() {
                if back.contains(&s) {
                    continue;
                }
                self.reads_of(s, &mut edges);
                if edges.iter().any(|t| back.contains(t)) {
                    back.insert(s);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        out.push(slot);
        out.extend(back.iter().copied().filter(|&s| s != slot));
        out
    }
}

impl GroupRecords for Now<'_> {
    fn group_of(&self, module: ItemKey) -> Option<GroupRef<'_>> {
        let &s = self.at.get(&module)?;
        self.slots[s].as_ref().map(GroupRef::of)
    }
}

fn own_of(own: &[(ItemKey, ContentId)], m: ItemKey) -> Option<ContentId> {
    own.binary_search_by_key(&m, |(k, _)| *k).ok().map(|i| own[i].1)
}

/// [`re_solve`] with a planted bug (the tests' control).
pub(crate) fn re_solve_planted<'a, S: GroupSolver<'a>>(edit: Edit<'_, 'a>, solver: &mut S, plant: EditPlant, alloc: Alloc<'a>) -> Result<Edited<'a>, EditError<'a, S::Refusal>> {
    if let Some(w) = edit.own.windows(2).find(|w| w[0].0 >= w[1].0) {
        return Err(EditError::OwnNotSorted { at: w[1].0 });
    }
    let mut now = Now { slots: AVec::new_in(alloc), at: ABTreeMap::new_in(alloc), readers: ABTreeMap::new_in(alloc), latest: ABTreeMap::new_in(alloc), alloc };
    for g in edit.old.iter() {
        now.add(g.clone());
    }
    let old_slots = now.slots.len();
    let mut queue: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
    // The seeds: members whose own text moved or is gone, modules in no group, readers of the
    // cells the caller says moved.
    for g in edit.old.iter() {
        if g.members.iter().zip(g.own.iter()).any(|(m, o)| own_of(edit.own, *m) != Some(*o)) {
            queue.insert(g.members[0]);
        }
    }
    for &(m, _) in edit.own.iter() {
        if !now.at.contains_key(&m) {
            queue.insert(m);
        }
    }
    let mut skipped = Skipping { armed: plant == EditPlant::SkipOneReader, member: None };
    for &(cell, value) in edit.moved.iter() {
        now.latest.insert(cell, value);
        if let Some(list) = now.readers.get(&cell) {
            for &m in list.iter() {
                if skipped.lets(m) {
                    queue.insert(m);
                }
            }
        }
    }
    let most = most_entries(edit.old.len(), edit.own.len());
    let mut entered: AVec<'a, AVec<'a, ItemKey>> = AVec::new_in(alloc);
    let mut replaced_old: ABTreeSet<'a, usize> = ABTreeSet::new_in(alloc);
    while let Some(k) = queue.pop_first() {
        // What to ask: the group `k` is in, without its modules gone; or `k` alone, a new module.
        let mut asked: AVec<'a, ItemKey> = AVec::new_in(alloc);
        match now.at.get(&k).copied() {
            Some(slot) => {
                if !now.stale(slot, edit.own) {
                    continue;
                }
                let Some(g) = now.slots[slot].as_ref() else { continue };
                asked.extend(g.members.iter().copied().filter(|&m| own_of(edit.own, m).is_some()));
                if asked.is_empty() {
                    // Every member is gone: what it defined is absent now.
                    let moved = replace(&mut now, &[slot], &[], &mut replaced_old, old_slots);
                    enter_readers(&mut now, &moved, &mut queue, &mut skipped);
                    continue;
                }
            }
            None if own_of(edit.own, k).is_some() => asked.push(k),
            None => continue,
        }
        // Solve; a cycle the records show through a result is asked again as one group.
        loop {
            if entered.len() >= most {
                return Err(EditError::TooManyEntries { most });
            }
            entered.push(asked.clone());
            let got = match solver.solve(&asked, &now) {
                Ok(got) => got,
                Err(refusal) => return Err(EditError::SolveWhole { members: asked, refusal }),
            };
            for &m in asked.iter() {
                if !got.iter().any(|g| g.members.binary_search(&m).is_ok()) {
                    return Err(EditError::Lost { member: m });
                }
            }
            // The slots the answer replaces: every slot of a module it holds.
            let mut gone: AVec<'a, usize> = AVec::new_in(alloc);
            for g in got.iter() {
                gone.extend(g.members.iter().filter_map(|m| now.at.get(m).copied()));
            }
            gone.sort_unstable();
            gone.dedup();
            // Modules of a replaced group the answer does not hold are asked again on their own.
            for &s in gone.iter() {
                if let Some(g) = now.slots[s].as_ref() {
                    for &m in g.members.iter() {
                        if !got.iter().any(|n| n.members.binary_search(&m).is_ok()) {
                            queue.insert(m);
                        }
                    }
                }
            }
            let first = got.len();
            let moved = replace(&mut now, &gone, &got, &mut replaced_old, old_slots);
            let new_slots = now.slots.len() - first..now.slots.len();
            enter_readers(&mut now, &moved, &mut queue, &mut skipped);
            // A merge the records show: a new group on a cycle of reads.
            let mut merged: Option<AVec<'a, ItemKey>> = None;
            for s in new_slots {
                let cycle = now.cycle_through(s);
                if cycle.len() > 1 {
                    let mut members: AVec<'a, ItemKey> = AVec::new_in(alloc);
                    for &c in cycle.iter() {
                        if let Some(g) = now.slots[c].as_ref() {
                            members.extend(g.members.iter().copied());
                        }
                    }
                    members.sort_unstable();
                    merged = Some(members);
                    break;
                }
            }
            match merged {
                Some(m) => asked = m,
                None => break,
            }
        }
    }
    let mut groups: AVec<'a, NameGroup<'a>> = AVec::new_in(alloc);
    groups.extend(now.slots.iter().flatten().cloned());
    groups.sort_unstable_by_key(|g| g.members[0]);
    Ok(Edited { groups, entered, replaced: replaced_old.len() as u32 })
}

/// Replaces the groups at `gone` by `got`: the cells whose value moved, each with its value now
/// (recorded in `latest`).
fn replace<'a>(now: &mut Now<'a>, gone: &[usize], got: &[NameGroup<'a>], replaced_old: &mut ABTreeSet<'a, usize>, old_slots: usize) -> AVec<'a, [u8; 16]> {
    let alloc = now.alloc;
    let mut before: ABTreeMap<'a, [u8; 16], ContentId> = ABTreeMap::new_in(alloc);
    for &s in gone.iter() {
        if s < old_slots {
            replaced_old.insert(s);
        }
        if let Some(g) = now.slots[s].take() {
            for &m in g.members.iter() {
                now.at.remove(&m);
            }
            for &(cell, d) in g.defines.iter() {
                before.insert(cell, d);
            }
        }
    }
    let mut after: ABTreeMap<'a, [u8; 16], ContentId> = ABTreeMap::new_in(alloc);
    for g in got.iter() {
        for &(cell, d) in g.defines.iter() {
            after.insert(cell, d);
        }
        now.add(g.clone());
    }
    let mut moved: AVec<'a, [u8; 16]> = AVec::new_in(alloc);
    for (&cell, &d) in before.iter() {
        let value = after.get(&cell).copied().unwrap_or(ABSENT);
        if value != d {
            moved.push(cell);
            now.latest.insert(cell, value);
        }
    }
    for (&cell, &d) in after.iter() {
        if !before.contains_key(&cell) {
            moved.push(cell);
            now.latest.insert(cell, d);
        }
    }
    moved.sort_unstable();
    moved
}

/// Queues the readers of each moved cell (a reader whose record saw the value now is passed
/// over when popped: [`Now::stale`]).
fn enter_readers<'a>(now: &mut Now<'a>, moved: &[[u8; 16]], queue: &mut ABTreeSet<'a, ItemKey>, skip: &mut Skipping) {
    for cell in moved.iter() {
        let Some(list) = now.readers.get(cell) else { continue };
        for &m in list.iter() {
            let reads = now.at.get(&m).and_then(|&s| now.slots[s].as_ref()).is_some_and(|g| g.cells_read.iter().any(|(_, c, _)| c == cell));
            if reads && skip.lets(m) {
                queue.insert(m);
            }
        }
    }
}

/// [`EditPlant::SkipOneReader`]: the first reader met is never entered by a read.
struct Skipping {
    armed: bool,
    member: Option<ItemKey>,
}

impl Skipping {
    fn lets(&mut self, m: ItemKey) -> bool {
        if self.member == Some(m) {
            return false;
        }
        if self.armed {
            self.armed = false;
            self.member = Some(m);
            return false;
        }
        true
    }
}
