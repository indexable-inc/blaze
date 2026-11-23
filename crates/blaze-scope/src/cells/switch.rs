//! The switch (feature `cells`): the cell core solves a crate, driving its expansions through
//! the same hook the old fill uses, and writes every module's scope ([`Cells::scope`]) in place
//! of the old fill's `done`. What follows (the root's published exports, the records) reads the
//! scopes as before.
//!
//! A module's in-memory read list (`Resolved::reads`) is left empty: no record holds it (the
//! records are written from the scopes; the crate's read list is the questions asked of its
//! dependencies, `crate::asked`), and the old core's incremental re-solve (`again.rs`) does not
//! run on these scopes.

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, BVec, Bump};

use blaze_surface::{ScopeBuilder, ScopeView};

use super::{BuiltDeps, CellError, Cells, EnumNames, FragId, MemberRefusal, Members};
use crate::Def;
use crate::facts::ScopeFacts;
use crate::resolve::Built;
use crate::scope::{ReadList, Resolved};
use crate::task::NamesPart;

/// The most rounds of made modules (blocks and expansions' modules found after a run) one
/// crate's solve takes: a crate past it is refused by name, never cut short.
pub const MOST_ROUNDS: u32 = 64;

/// Why the cell core wrote no scopes for a crate, naming the thing.
#[derive(Clone, Debug)]
pub enum Unwritten {
    Load(CellError),
    Run(CellError),
    /// A module with `count` imports or calls the core holds as outside what it follows, or
    /// left undecided: its scope would not be the answer (rule 3: refused, never guessed).
    Unknown { module: ItemKey, count: u32, why: &'static str },
    /// Made modules kept appearing past [`MOST_ROUNDS`] rounds.
    Rounds,
    /// D116's equality check (asked by the driver's [`crate::task::Par::decide_check`]) stopped.
    Decide(super::DecideError),
    /// The crate's groups were not found (groups.rs).
    Groups(super::GroupsError),
    /// A one-group solve could not solve its members alone (edit.rs): the caller solves the
    /// crate whole, a planned path.
    Member(super::MemberRefusal),
}

impl core::fmt::Display for Unwritten {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unwritten::Load(e) => write!(f, "the cell core's load: {e}"),
            Unwritten::Run(e) => write!(f, "the cell core's run: {e}"),
            Unwritten::Unknown { module, count, why } => {
                write!(f, "module {module:?}: {count} imports or macro calls the cell core does not follow or left undecided ({why})")
            }
            Unwritten::Rounds => write!(f, "made modules kept appearing past {MOST_ROUNDS} rounds of the cell core"),
            Unwritten::Decide(e) => write!(f, "{e}"),
            Unwritten::Groups(e) => write!(f, "{e}"),
            Unwritten::Member(r) => write!(f, "the one-group solve is refused: {r:?}"),
        }
    }
}

/// What [`solve_and_write`] gives: every module's scope, and the extern prelude's names the
/// crate's macro-written root `extern crate` items added (R8; the Reference,
/// `names.preludes.extern.decl`: an `extern crate` at the root adds its name to the extern
/// prelude), sorted by name: none of them is a name the prelude had before.
pub(crate) struct Solved<'a> {
    pub done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    pub added_externs: AVec<'a, (&'a str, ItemKey)>,
    /// D116's equality check of this crate, when the driver asked for it.
    pub decide: Option<super::DecideCheck<'a, &'a str>>,
    /// The crate's groups as the solve found them (groups.rs), written into records from the
    /// final scopes by the caller.
    pub groups: AVec<'a, super::GroupShape<'a>>,
    /// D189 step 4: for each body place the caller held (`BodySrc::place`: the fragment's
    /// owner and the place in it), the length of its module's textual sequence there
    /// ([`Cells::textual_len_at`]), sorted by place; a place not read here is left out.
    pub cuts: AVec<'a, ((ItemKey, u32), u32)>,
}

/// Solves the crate under `root` with the cell core and writes each of its modules' scopes.
/// `expand(call, def)` makes a call's expansion, which `facts.expansion` then hands back;
/// `made` lists every module made so far (expansions' modules, blocks); `places` says where
/// a block's body is written; `held` lists the places of the bodies whose blocks are left to
/// their own solve (D189 step 4, `BlockScopes::Bodies`), each counted into [`Solved::cuts`].
#[expect(clippy::too_many_arguments, reason = "the solve's inputs, each named; one caller")]
pub(crate) fn solve_and_write<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    enums: &dyn EnumNames<&'a str>,
    places: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    root: ItemKey,
    expand: &dyn Fn(ItemKey, Def),
    made: &dyn Fn(&mut AVec<'a, ItemKey>),
    held: &dyn Fn(&mut AVec<'a, (ItemKey, u32)>),
    check: Option<&dyn Fn() -> u64>,
    plant: crate::task::DecidePlant,
    groups: bool,
    only: Option<Members<'_>>,
    part: &dyn Fn(NamesPart),
    bump: Bump<'a>,
) -> Result<Solved<'a>, Unwritten> {
    let alloc = bump.alloc();
    let deps = BuiltDeps { built, alloc, asks: true, through: core::cell::Cell::new(None) };
    // A one-group solve reads its members' texts alone (edit.rs).
    let read = |k: ItemKey| only.is_none_or(|o| o.holds(k));
    // Every other module of the crate from its group's record when a member first reads it,
    // its scope kept to be written as it was.
    let records = only.map(|o| OutsideRecords { o, alloc: bump, recorded: core::cell::RefCell::new(AVec::new_in(alloc)) });
    part(NamesPart::Load);
    let mut c = Cells::load_reading(facts, root, Some(&deps), super::CellPlant::None, &read, alloc).map_err(Unwritten::Load)?;
    // A driver's control of its oracle (task/fill.rs `Par::decide_plant`): the plant is read by
    // the on-demand walk alone (decide.rs), so the whole solve below is unchanged by it.
    if check.is_some() && plant == crate::task::DecidePlant::CachesOpenCycle {
        c.plant = super::CellPlant::DecideCachesOpenCycle;
    }
    c.enums = Some(enums);
    // The root's scope gathers every module's expanded `#[macro_export]`s (task/exports.rs
    // `publish_expanded_exports` reads every scope): a group holding the root reads every
    // module's record, the rest only those its members read.
    let every = only.is_some_and(|o| o.holds(root));
    if let (Some(r), Some(_)) = (&records, only) {
        if check.is_some() {
            return Err(Unwritten::Member(MemberRefusal::DecideCheck));
        }
        c.outside = Some(r);
        part(NamesPart::Outside);
        c.pend(0..c.modules.len() as super::ModId);
        c.fill_macro_use_children();
        if every {
            for m in 0..c.modules.len() as super::ModId {
                c.fill_pending(m);
            }
        }
    }
    part(NamesPart::Run);
    c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    // The modules no `mod` line names (a block's module) and those made after the walk: solved
    // after the rest, round by round, until a run makes none.
    let mut rounds = 0u32;
    loop {
        let mut extra: AVec<'a, ItemKey> = AVec::new_in(alloc);
        made(&mut extra);
        extra.retain(|k| !c.by_key.contains_key(k));
        if extra.is_empty() {
            break;
        }
        rounds += 1;
        if rounds > MOST_ROUNDS {
            return Err(Unwritten::Rounds);
        }
        extra.sort_unstable();
        extra.dedup();
        // A made module outside the members (a block of a body outside, a module an expansion
        // outside wrote) is answered from its group's record, as a walked one is.
        part(NamesPart::Load);
        let (_, outside) = c.load_more_reading(facts, &extra, places, &read).map_err(Unwritten::Load)?;
        if only.is_some() {
            part(NamesPart::Outside);
            c.pend(outside.iter().copied());
            c.fill_macro_use_children();
            if every {
                for &m in outside.iter() {
                    c.fill_pending(m);
                }
            }
        }
        part(NamesPart::Run);
        c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    }
    // Each held body's cut of its module's textual sequence, over the final fragments.
    let mut at: AVec<'a, (ItemKey, u32)> = AVec::new_in(alloc);
    held(&mut at);
    at.sort_unstable();
    at.dedup();
    let mut cuts: AVec<'a, ((ItemKey, u32), u32)> = AVec::with_capacity_in(at.len(), alloc);
    cuts.extend(at.iter().filter_map(|&(owner, pos)| c.textual_len_at(owner, pos).map(|len| ((owner, pos), len))));
    part(NamesPart::Scopes);
    // A member that read a module the solve holds neither the text nor a record of saw it as
    // unknown: the refusal names the cause, before any scope's unknowns (E13's on-demand
    // driver, cells/lazy.rs, solves that module first and asks again).
    if let Some(r) = c.member_refusal {
        return Err(Unwritten::Member(r));
    }
    let ix = c.write_index(alloc);
    let mut keys: AVec<'a, ItemKey> = AVec::with_capacity_in(c.modules.len(), alloc);
    keys.extend(c.modules.iter().filter(|m| !m.foreign && !m.unread && m.text != FragId::MAX).map(|m| m.key));
    keys.sort_unstable();
    let mut done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>> = HashMap::new_in(alloc);
    for &k in keys.iter() {
        let Some((scope, unknown)) = c.scope(&ix, k, alloc) else { continue };
        if unknown > 0 {
            return Err(Unwritten::Unknown { module: k, count: unknown, why: c.first_unknown(&ix, k) });
        }
        done.insert(k, Resolved { scope, reads: ReadList { entries: AVec::new_in(alloc) } });
    }
    // The modules outside the group a member read: their scopes as their records hold them.
    if let Some(r) = &records {
        for (k, view) in r.recorded.borrow().iter() {
            done.insert(*k, Resolved { scope: ScopeBuilder::from(view, alloc), reads: ReadList { entries: AVec::new_in(alloc) } });
        }
    }
    let mut added_externs: AVec<'a, (&'a str, ItemKey)> = AVec::with_capacity_in(c.outer.added.len(), alloc);
    added_externs.extend(c.outer.added.iter().map(|(&name, &root)| (name, root)));
    // D116: every cell decided again on demand over the frozen tables, after the scopes are
    // written (it leaves `c` re-decided; nothing reads `c` after it).
    // D116 point 7: the records at module grain against the written scopes, before `decide`
    // re-decides `c`.
    let links = check.map(|_| c.links_agree(alloc, alloc));
    let decide = match check {
        Some(now) => {
            let mut d = super::decide::check(&mut c, now, alloc).map_err(Unwritten::Decide)?;
            d.links = links;
            Some(d)
        }
        None => None,
    };
    // E4: the groups, over the decided values (after D116's check, which re-decides `c`;
    // `groups` leaves every value as it found it), when the caller asked.
    // A member's own text: its own fragment's `ModuleDecls` record, as the facts hold it.
    let own = |k: ItemKey| -> Result<blaze_id::ContentId, super::GroupsError> {
        let fail = |error| super::GroupsError::Own { module: k, error };
        let info = facts.module(k).map_err(|e| fail(super::OwnError::Facts(e)))?;
        let decls = facts.decls(k).map_err(|e| fail(super::OwnError::Facts(e)))?;
        let bytes = decls.write(k, &info, alloc).map_err(|e| fail(super::OwnError::Write(e)))?;
        Ok(super::groups::own_digest(&bytes))
    };
    // A one-group solve writes its members' groups alone.
    part(NamesPart::Groups);
    let groups =if groups { c.groups(&|k| read(k) && done.contains_key(&k), &own, alloc).map_err(Unwritten::Groups)? } else { AVec::new_in(alloc) };
    if let Some(r) = c.member_refusal {
        return Err(Unwritten::Member(r));
    }
    Ok(Solved { done, added_externs, decide, groups, cuts })
}

/// `bytes` copied into `alloc` at a 16-byte boundary (a record is read in place, aligned).
fn aligned_copy<'a>(bytes: &[u8], alloc: Bump<'a>) -> &'a [u8] {
    const ALIGN: usize = 16;
    let mut b: BVec<'a, u8> = BVec::with_capacity_in(bytes.len() + ALIGN, alloc);
    // Within the capacity reserved: the buffer does not move while it is filled.
    let pad = b.as_ptr().align_offset(ALIGN).min(ALIGN);
    b.resize(pad, 0);
    b.extend_from_slice(bytes);
    let all: &'a [u8] = b.into_bump_slice();
    &all[pad..]
}

/// A one-group solve's records of the modules outside its members ([`super::Outside`]): each
/// scope read in place from its group's record, copied aligned into `alloc` and kept in
/// `recorded` to be written as it was.
struct OutsideRecords<'r, 'a> {
    o: Members<'r>,
    alloc: Bump<'a>,
    recorded: core::cell::RefCell<AVec<'a, (ItemKey, ScopeView<'a>)>>,
}

impl<'a> super::Outside<&'a str> for OutsideRecords<'_, 'a> {
    fn fill(&self, key: ItemKey, into: &mut super::Filled<'_, &'a str>) -> Result<Option<blaze_id::ContentId>, MemberRefusal> {
        let Some(g) = self.o.records.group_of(key) else { return Ok(None) };
        let scope = g.scope_of(key).map_err(|error| MemberRefusal::Record { module: key, error })?;
        let globs = g.globs_of(key).ok_or(MemberRefusal::NoGlobsCell { module: key })?;
        let view = ScopeView::read(aligned_copy(scope, self.alloc)).map_err(|_| MemberRefusal::Scope { module: key })?;
        into.macro_exports = view.macro_exports().next().is_some();
        into.bindings.extend(view.bindings());
        into.globs.extend(view.globs());
        into.textual = g.textual_of(key);
        if into.textual.is_some() {
            into.defs.extend(view.textual());
            into.marks.extend(view.marks());
        }
        self.recorded.borrow_mut().push((key, view));
        Ok(Some(globs))
    }
}
