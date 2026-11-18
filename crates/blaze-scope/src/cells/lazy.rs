//! Names on demand (engine.md piece E13, D134: lazy over what is not yours): a crate's groups
//! solved from the modules ASKED, never the whole crate. Measured first
//! (blaze-ladder tests/names_reach_shelf.rs, 2026-11-17): a std program's root reaches 66 of
//! its 77 dependencies' 17,260 groups (536 of 18,373 modules, 2.9%; 8.4% of the record bytes),
//! so the grain is the group, its record as it is.
//!
//! The one-group solve (edit.rs [`GroupSolver`], `OneGroup` over the crate solve with its
//! members named) answers every module outside its members from the record of its group and
//! refuses [`MemberRefusal::NoRecord`] when a member reads a module that has none. This driver
//! turns that refusal around: the module named is solved first (its own group, from its text,
//! and what IT reads, on demand again), its record kept, and the asker is asked again. The
//! demands form a stack; a refusal naming a module already on the stack is a cycle of reads
//! (a glob cycle), so the frames from that module up are merged into one member set and solved
//! as one (the solver cuts what it is asked into groups: a merge too wide is split there).
//!
//! What it holds after: exactly the groups of the closure of the asked modules' reads, each
//! record byte-equal to the same group of a whole solve (a group's record is a function of the
//! group alone: groups.rs), whatever the order of the demands (rule 25). Bounded (rule 18): the
//! stack by [`MOST_DEPTH`], the solves by [`MOST_SOLVES`], each a typed refusal naming where.
//!
//! OPEN (the next pieces): the cross-crate seam (a dependent crate's read of a dependency's
//! module with no record, cells/deps.rs `DepNames::name_in`, refused the same way and answered
//! by this driver in the dependency). A `macro_rules!` lookup walking into a module outside
//! the members reads that module's textual sequence from its record (cells/mod.rs
//! `textual_recorded`, logged as its `GroupRead::Textual`); one with no record is refused
//! [`MemberRefusal::NoRecord`] and so demanded like a scope read; one whose record does not
//! give the sequence the walk needs stays [`MemberRefusal::Textual`].

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};

use super::edit::{GroupRecords, GroupRef, GroupSolver, MemberRefusal, OneGroupRefusal};
use super::groups::NameGroup;
use super::switch::Unwritten;

/// The deepest stack of demands one [`demand`] follows (each frame a module read by the one
/// below with no record yet).
pub const MOST_DEPTH: usize = 512;

/// The most one-group solves one [`demand`] asks (each refusal is one).
pub const MOST_SOLVES: usize = 65_536;

/// A solver's refusal that may name a module read with no record: the demand the driver
/// answers. Every other refusal stops the driver.
pub trait NoRecordOf {
    fn no_record(&self) -> Option<ItemKey>;
}

impl NoRecordOf for MemberRefusal {
    fn no_record(&self) -> Option<ItemKey> {
        match *self {
            MemberRefusal::NoRecord { module } => Some(module),
            _ => None,
        }
    }
}

impl NoRecordOf for Unwritten {
    fn no_record(&self) -> Option<ItemKey> {
        match self {
            Unwritten::Member(r) => r.no_record(),
            _ => None,
        }
    }
}

impl NoRecordOf for crate::resolve::NamesError<'_> {
    fn no_record(&self) -> Option<ItemKey> {
        match self {
            crate::resolve::NamesError::Cells { error, .. } => error.no_record(),
            _ => None,
        }
    }
}

impl NoRecordOf for crate::crate_scope::CrateScopeError<'_> {
    fn no_record(&self) -> Option<ItemKey> {
        match self {
            crate::crate_scope::CrateScopeError::Names(e) => e.no_record(),
            _ => None,
        }
    }
}

impl<R: NoRecordOf> NoRecordOf for OneGroupRefusal<R> {
    fn no_record(&self) -> Option<ItemKey> {
        match self {
            OneGroupRefusal::Crate(r) => r.no_record(),
            OneGroupRefusal::Widened { .. } => None,
        }
    }
}

/// The groups solved on demand so far, by module: what the next one-group solve answers a
/// module outside its members by ([`GroupRecords`]).
pub struct Demanded<'a> {
    /// In the order solved.
    pub groups: AVec<'a, NameGroup<'a>>,
    by_module: HashMap<ItemKey, usize, Alloc<'a>>,
    /// The one-group solves asked, refused ones counted.
    pub solves: usize,
    /// The deepest stack of demands met.
    pub deepest: usize,
}

/// Options of [`Demanded::new`] (rule 83).
pub struct DemandedOpts<'a> {
    pub alloc: Alloc<'a>,
}

impl<'a> Demanded<'a> {
    pub fn new(opts: DemandedOpts<'a>) -> Demanded<'a> {
        Demanded { groups: AVec::new_in(opts.alloc), by_module: HashMap::new_in(opts.alloc), solves: 0, deepest: 0 }
    }

    /// Whether `module` is in a group solved here.
    pub fn holds(&self, module: ItemKey) -> bool {
        self.by_module.contains_key(&module)
    }
}

impl GroupRecords for Demanded<'_> {
    fn group_of(&self, module: ItemKey) -> Option<GroupRef<'_>> {
        self.by_module.get(&module).map(|&i| GroupRef::of(&self.groups[i]))
    }
}

/// Why [`demand`] stopped, naming the module (rule 19).
#[derive(Debug)]
pub enum DemandError<R> {
    /// The solver's refusal, not a module with no record (rule 3: never solved whole instead).
    Solver(R),
    /// The demands were `most` deep at `module` (rule 18).
    Depth { module: ItemKey, most: usize },
    /// `most` solves were asked (rule 18).
    Solves { most: usize },
    /// The solver refused `module` as having no record, and it has one: a solver bug.
    Recorded { module: ItemKey },
    /// The solver answered without a group holding the asked `module`.
    NotCovered { module: ItemKey },
    /// The solver answered a group holding `module`, already in another group here, with
    /// other members or bytes: two solves of one text cut two ways (rule 25).
    Recut { module: ItemKey },
}

/// Solves the groups of `asked` and of every module their reads reach with no record yet,
/// into `into`, through `solver`. Asked modules already held are skipped. Scratch in `alloc`.
pub fn demand<'a, S>(asked: &[ItemKey], solver: &mut S, into: &mut Demanded<'a>, alloc: Alloc<'_>) -> Result<(), DemandError<S::Refusal>>
where
    S: GroupSolver<'a>,
    S::Refusal: NoRecordOf,
{
    // Each frame: a member set, sorted; each pushed because the frame below read it.
    let mut frames: AVec<'_, AVec<'_, ItemKey>> = AVec::new_in(alloc);
    for &first in asked {
        if into.holds(first) {
            continue;
        }
        let mut f = AVec::new_in(alloc);
        f.push(first);
        frames.push(f);
        while let Some(top) = frames.last() {
            if into.solves >= MOST_SOLVES {
                return Err(DemandError::Solves { most: MOST_SOLVES });
            }
            into.solves += 1;
            into.deepest = into.deepest.max(frames.len());
            match solver.solve(&top[..], &*into) {
                Ok(groups) => {
                    for &m in top.iter() {
                        if !groups.iter().any(|g| g.members.binary_search(&m).is_ok()) {
                            return Err(DemandError::NotCovered { module: m });
                        }
                    }
                    for g in groups {
                        keep(into, g)?;
                    }
                    frames.pop();
                }
                Err(e) => {
                    let Some(x) = e.no_record() else { return Err(DemandError::Solver(e)) };
                    if into.holds(x) {
                        return Err(DemandError::Recorded { module: x });
                    }
                    match frames.iter().position(|f| f.binary_search(&x).is_ok()) {
                        // A cycle of reads: the frames from `x`'s up are one member set.
                        Some(at) => {
                            let mut merged = AVec::new_in(alloc);
                            for f in frames.drain(at..) {
                                merged.extend(f.iter().copied());
                            }
                            merged.sort_unstable();
                            merged.dedup();
                            frames.push(merged);
                        }
                        None => {
                            if frames.len() >= MOST_DEPTH {
                                return Err(DemandError::Depth { module: x, most: MOST_DEPTH });
                            }
                            let mut f = AVec::new_in(alloc);
                            f.push(x);
                            frames.push(f);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Keeps a solved group; one already held must be the same record.
fn keep<'a, R>(into: &mut Demanded<'a>, g: NameGroup<'a>) -> Result<(), DemandError<R>> {
    if let Some(&i) = into.by_module.get(&g.members[0]) {
        let held = &into.groups[i];
        return if held.members[..] == g.members[..] && held.bytes[..] == g.bytes[..] { Ok(()) } else { Err(DemandError::Recut { module: g.members[0] }) };
    }
    if let Some(&m) = g.members.iter().find(|m| into.by_module.contains_key(*m)) {
        return Err(DemandError::Recut { module: m });
    }
    let at = into.groups.len();
    for &m in g.members.iter() {
        into.by_module.insert(m, at);
    }
    into.groups.push(g);
    Ok(())
}
