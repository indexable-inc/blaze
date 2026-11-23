//! The auto traits of another fn's opaque type, from its defining body's check.
//!
//! The Reference, "Impl trait", "Auto trait leakage": "Auto traits ... of the hidden type are
//! leaked through the opaque type": `fn f() -> impl Sized { 7u8 }` is `Send` to a caller because
//! `u8` is, and `fn g() -> impl Sized { Rc::new(7u8) }` is not, though neither bound says so.
//! The hidden type is the defining body's (blaze-typeck rules/opaque_def.rs writes it to
//! `Checked::hidden` by writeback); an `async fn`'s is its root block's coroutine by identity
//! (`PNode::Coroutine { owner, ordinal, args }`), whose output and interior are that block's
//! `PNode::Future(_, output, interior)` in the same record.
//!
//! THE STORE ([`HiddenTable`], `Store::hidden`): each clean body that defines an opaque,
//! an async block or an async closure files its hidden types and coroutine facts here, copied into
//! the table's own type table ([`file`], called where a clean check's read list is filed:
//! front2 body_reads.rs `check_body_asked` and `check_body_filed`). So a defining body the
//! driver already checked is read from here and never checked again (rule 77); only a body not
//! checked yet is checked for it, once, and that check files it the same way.
//!
//! LAZY, and the same answers in any order (rule 21): a check sees every opaque as itself
//! (`opaque_hidden` answers `None`) except those in ITS OWN leak list, which grows only from its
//! own gaps. When a round stops on the checker's typed gap `CheckError::OpaqueHiddenUnknown {
//! opaque, .. }` (an auto trait goal on an opaque, rules/opaque_def.rs `opaque_ambiguity_gap`),
//! the check loop ([`after_auto_gap`]) makes sure the opaque's defining body is filed (checking
//! it if not), adds the opaque to the check's leak list, and runs the round again: the world now
//! answers its hidden type, and `World::coroutine` from the same row. A body's answers thus
//! depend only on its own text and the records, never on which bodies were checked before it.
//!
//! A WORK LIST, never a recursion ([`define`]): a body checked to define an opaque that itself
//! stops on the gap for a third fn's opaque not filed yet does not define it inside; it fails,
//! names the opaque in [`HiddenTable::wanted`], the list defines that one first and checks the
//! body again.
//!
//! A CYCLE is refused by name ([`HiddenFail::Cycle`], rustc's E0391 class; the Reference,
//! "Abstract return types": the defining body chooses the hidden type, so its auto traits are
//! not known while that body is being checked): an auto trait of a fn's own opaque asked in its
//! own body, or of an opaque whose defining body is on the work list. A body that only NAMES its
//! own opaque (a recursive call) sees it opaque and is fine.
//!
//! Refused by name, never guessed: an opaque no fn head made, a defining body that does not
//! check (its refusal quoted), a record that gives no hidden type for the opaque, a hole or an
//! error type in a filed type, a body to check with no prepared macros (`Records::consts`
//! `None`), a work list past [`MOST_DEFINES`].

use std::boxed::Box;
use std::format;

use blaze_flow_lower::InteriorPart;
use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};
use blaze_typeck as tc;
use blaze_typeck::{Checked, CoroutineFacts, Hidden, ItemKey, List, PNode, PTy, PTys, PortableTys};

use super::body::{Body, BodyError};
use super::lower_made::{LowerTarget, lower_made};
use super::check::{CheckFail, Records, item_shown};
use super::exact_interior::{BlockInterior, Exacts, exact_interiors};
use super::source::NamesSource;
use super::spell::Spelling;
use super::world_items::Gap;

/// The most defining bodies one work list checks (rule 18).
pub const MOST_DEFINES: u32 = 256;

/// One defining body's filed facts, their types in [`HiddenTable`]'s table.
struct Row<'a> {
    /// Each opaque it defines and its hidden type (`Checked::hidden`, copied).
    hidden: Vec<(ItemKey, PTy), Alloc<'a>>,
    /// Each block or async closure's call future, by its owner-relative coroutine ordinal.
    coroutines: Vec<(u32, PTy, FiledInterior), Alloc<'a>>,
}

/// What a filed coroutine row's interior is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FiledInterior {
    /// The captured places are not known (`CoroutineWitness::NeedsCapturePlaces`): a capture
    /// held as a whole binding may be a field, or by value where the state holds a reference,
    /// so the check's tuple is no superset of the state. No reader is answered.
    CapturesUnknown(PTy),
    /// The captures are known and the flow that names the saved locals was refused
    /// (exact_interior.rs `BlockInterior::Refused`). The check's candidate tuple lacks
    /// flow's reference wrappers. Its precision tag keeps it out of structural selection.
    Superset(PTy),
    Exact(PTy),
}

impl FiledInterior {
    fn is_exact(self) -> bool {
        matches!(self, FiledInterior::Exact(_))
    }
}

/// The answer to a reader's question for one coroutine ([`HiddenTable::coroutine`]).
pub enum CoroutineAnswer<'t> {
    Facts(CoroutineFacts<'t>),
    /// No clean check of the owner is filed in this command.
    Unfiled,
    /// The owner's filed row holds no coroutine of this ordinal.
    NoOrdinal,
    /// The owner's row holds it, but its captured places are not known.
    CapturesUnknown,
}

impl CoroutineAnswer<'_> {
    /// Why no facts were given, for the reader's gap (rule 19); `None` for facts.
    pub fn why(&self) -> Option<&'static str> {
        match self {
            CoroutineAnswer::Facts(_) => None,
            CoroutineAnswer::Unfiled => Some("no clean check of its defining body is filed"),
            CoroutineAnswer::NoOrdinal => Some("its defining body's clean check holds no coroutine of that ordinal"),
            CoroutineAnswer::CapturesUnknown => {
                Some("its defining body's check did not establish its captured places and their modes, so its interior is no superset of what it stores")
            }
        }
    }
}

/// The hidden types of the opaques defined by the bodies checked clean in this command, and the
/// running check's leak list. Held in the store (`Store::hidden`).
pub struct HiddenTable<'a> {
    alloc: Alloc<'a>,
    /// Every filed type, copied out of its defining record's table (only what the rows reach).
    tys: PortableTys<'a>,
    /// By the defining fn's checker key.
    rows: HashMap<ItemKey, Row<'a>, Alloc<'a>>,
    /// The opaques the running check reads the hidden type of (its own leak list, sorted).
    leaking: Vec<ItemKey, Alloc<'a>>,
    /// The defining fns on the work list while [`define`] runs.
    defining: Vec<ItemKey, Alloc<'a>>,
    /// The opaque a body checked by the work list stopped on, not filed yet.
    wanted: Option<ItemKey>,
    /// The cycle a body checked by the work list stopped on (`opaque`, the body asking): the
    /// work list's own answer, typed ([`HiddenFail::Cycle`]), never its refusal as text.
    cycle: Option<(ItemKey, ItemKey)>,
}

/// Which opaques a world sees through to their hidden types (rustc's typing modes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reveal {
    /// Checking a body: an opaque is itself, except those in the running check's leak list
    /// (rustc's analysis mode; the auto-trait leak of the module doc).
    InBody,
    /// A resolution at ground arguments after checking (an instance's `resolve_instance` or
    /// `select_impl`, a constant's evaluation): every opaque is its hidden type (rustc's
    /// post-analysis mode). A where-clause such as `<F as FnOnce<_>>::Output == U` with `F` a fn
    /// returning `impl Trait` and `U` its revealed hidden type holds only so.
    All,
}

/// What the world answers for an opaque.
pub enum HiddenAnswer<'t> {
    /// Its defining body's hidden type.
    Known(Hidden<'t>),
    /// Opaque to this check: not in its leak list (only under [`Reveal::InBody`]).
    Opaque,
    /// Revealed (in the leak list, or [`Reveal::All`]), and no filed row of its defining body
    /// gives a hidden type for it.
    NoneGiven,
}

impl<'a> HiddenTable<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> HiddenTable<'a> {
        HiddenTable {
            alloc,
            tys: PortableTys::new_in(alloc),
            rows: HashMap::new_in(alloc),
            leaking: Vec::new_in(alloc),
            defining: Vec::new_in(alloc),
            wanted: None,
            cycle: None,
        }
    }

    /// An empty leak list for one check, in the table's allocator.
    pub fn new_leaks(&self) -> Vec<ItemKey, Alloc<'a>> {
        Vec::new_in(self.alloc)
    }

    /// The leak list last set, sorted: after a check, its last round's (front2 stalled_goals.rs
    /// `decide_stalled_goals` starts its own from it).
    pub fn leaking(&self) -> &[ItemKey] {
        &self.leaking
    }

    /// The running check's leak list (the check loop sets it before each round's world).
    pub fn set_leaking(&mut self, leaks: &[ItemKey]) {
        self.leaking.clear();
        for &k in leaks {
            self.leaking.push(k);
        }
    }

    /// The hidden type of `opaque`, defined by fn `owner`, to a world that sees `reveal`: the
    /// row filed for `filed`, which is `opaque` itself or, for the default of a provided trait
    /// fn's `impl Trait`, the fn's opaque its body defines (front2 rpitit.rs `filed_as`). The
    /// leak list holds what the check asked, `opaque`.
    pub fn answer(&self, opaque: ItemKey, filed: ItemKey, owner: ItemKey, reveal: Reveal) -> HiddenAnswer<'_> {
        let seen = match reveal {
            Reveal::InBody => self.leaking.binary_search(&opaque).is_ok(),
            Reveal::All => true,
        };
        if !seen {
            return HiddenAnswer::Opaque;
        }
        match self.rows.get(&owner).and_then(|row| row.hidden.iter().find(|h| h.0 == filed)) {
            Some(&(_, ty)) => HiddenAnswer::Known(Hidden { ty, types: self.tys.view() }),
            None => HiddenAnswer::NoneGiven,
        }
    }

    /// The facts of async block `ordinal` of `owner`, from its filed row: its exact interior,
    /// or the candidate tuple marked [`tc::InteriorIs::Superset`] where flow was refused.
    /// Structural auto-trait selection requires the exact row.
    pub fn coroutine(&self, owner: ItemKey, ordinal: u32) -> CoroutineAnswer<'_> {
        let Some(row) = self.rows.get(&owner) else { return CoroutineAnswer::Unfiled };
        let Some(&(_, output, interior)) = row.coroutines.iter().find(|c| c.0 == ordinal) else { return CoroutineAnswer::NoOrdinal };
        let (interior, is) = match interior {
            FiledInterior::Exact(t) => (t, tc::InteriorIs::Exact),
            FiledInterior::Superset(t) => (t, tc::InteriorIs::Superset),
            FiledInterior::CapturesUnknown(_) => return CoroutineAnswer::CapturesUnknown,
        };
        CoroutineAnswer::Facts(CoroutineFacts { output, interior, types: self.tys.view(), is })
    }

    fn filed(&self, owner: ItemKey) -> bool {
        self.rows.contains_key(&owner)
    }

    /// The facts this owner can give another body's check, with exactly the
    /// digests that check records in its read list.
    pub(super) fn published(
        &self, owner: ItemKey, scratch: Alloc<'_>, mut put: impl FnMut(super::reads::Fact, u64),
    ) -> Result<(), tc::CheckError> {
        use super::reads::{Fact, FactKind};
        let Some(row) = self.rows.get(&owner) else { return Ok(()) };
        let types = self.tys.view();
        for &(opaque, ty) in &row.hidden {
            let digest = Hidden { ty, types }.answer_digest(scratch)?;
            put(Fact { kind: FactKind::Opaque, key: opaque.0, detail: 0 }, digest);
        }
        for &(ordinal, output, interior) in &row.coroutines {
            // The answer `coroutine` gives, digested as the reader's list digests it: a
            // superset record is a read too, and its digest differs from the exact one's
            // (blaze-typeck world.rs `InteriorIs`), so a repaired flow enters its readers again.
            let (interior, is) = match interior {
                FiledInterior::Exact(t) => (t, tc::InteriorIs::Exact),
                FiledInterior::Superset(t) => (t, tc::InteriorIs::Superset),
                FiledInterior::CapturesUnknown(_) => continue,
            };
            let digest = CoroutineFacts { output, interior, types, is }.answer_digest(scratch)?;
            put(Fact { kind: FactKind::Coroutine, key: owner.0, detail: u64::from(ordinal) }, digest);
        }
        Ok(())
    }

    fn gives(&self, owner: ItemKey, opaque: ItemKey) -> bool {
        self.rows.get(&owner).is_some_and(|row| row.hidden.iter().any(|h| h.0 == opaque))
    }

    /// Other bodies may read a coroutine even when its owner defines no opaque.
    fn wants(&self, owner: ItemKey, exprs: &[tc::Expr], done: &Checked<'_>) -> bool {
        if let Some(row) = self.rows.get(&owner) {
            // Only a newly checked producer can supply more precise facts. A read of its
            // existing row does no flow work. The flow pass currently lowers async blocks.
            return row.coroutines.iter().any(|&(at, _, interior)| {
                at < tc::coroutine_id::CLOSURE_BASE && !interior.is_exact()
            });
        }
        !done.hidden.is_empty() || exprs.iter().any(|e| {
            matches!(e, tc::Expr::AsyncBlock { .. } | tc::Expr::Closure { is_async: true, .. })
        })
    }

    /// The clean check `done` of `body` filed when [`Self::wants`]
    /// it. Each async block's interior is its EXACT interior where `exacts` gives one
    /// (exact_interior.rs: what the state stores), else the check's superset (a block whose
    /// flow was refused: exact_interior.rs's module doc).
    fn file_row(
        &mut self, body: &tc::BodyView<'_>, head: &super::heads::FnHead<'_>, done: &Checked<'_>, exacts: Option<&Exacts<'_>>,
    ) -> Result<(), HiddenFail> {
        let owner = body.owner;
        let from = done.types.view();
        // The roots: each hidden type, then each async block's output and its interior (the
        // superset, one root, or the exact interior's parts, one root each).
        let mut roots: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(done.hidden.len(), self.alloc);
        for &(_, t) in done.hidden.iter() {
            roots.push(t);
        }
        #[derive(Clone, Copy)]
        enum InteriorRoots<'r> {
            Checked,
            Flow(&'r [InteriorPart]),
            /// The captures are known. The saved locals and their reference types are open.
            Superset,
            /// The check's tuple, no superset: the captured places are not known.
            CapturesUnknown,
        }
        let mut blocks = Vec::new_in(self.alloc);
        for i in 0..body.exprs.len() {
            let e = tc::ExprId(i as u32);
            if let Some((at, output, interior)) = done.coroutine_at(body, e).map_err(HiddenFail::Coroutine)? {
                let source = match done.coroutine_witness(e) {
                    Some(tc::CoroutineWitness::Complete) => InteriorRoots::Checked,
                    Some(tc::CoroutineWitness::NeedsSavedLocals) => match exacts.and_then(|x| x.of(e)) {
                        Some(BlockInterior::Exact(parts)) => InteriorRoots::Flow(parts),
                        // Flow refused or was unavailable for the async closure's coroutine.
                        Some(BlockInterior::Refused(_)) | None => InteriorRoots::Superset,
                    },
                    // Flow currently takes its captures from the same binding rows.
                    Some(tc::CoroutineWitness::NeedsCapturePlaces) => InteriorRoots::CapturesUnknown,
                    None => return Err(HiddenFail::Coroutine(tc::CheckError::Malformed(tc::Malformed::Expr(e)))),
                };
                blocks.push((at, roots.len(), source));
                roots.push(output);
                match source {
                    InteriorRoots::Flow(parts) => {
                        for p in parts {
                            roots.push(p.ty);
                        }
                    }
                    InteriorRoots::Checked | InteriorRoots::Superset | InteriorRoots::CapturesUnknown => roots.push(interior),
                }
            }
        }
        let copied = copy_reached(from, &roots, &mut self.tys, self.alloc, body, head).map_err(|why| HiddenFail::Copy { owner, why })?;
        let mut hidden = Vec::with_capacity_in(done.hidden.len(), self.alloc);
        for (i, &(k, _)) in done.hidden.iter().enumerate() {
            hidden.push((k, copied[i]));
        }
        let mut coroutines = Vec::with_capacity_in(blocks.len(), self.alloc);
        for &(at, first, source) in blocks.iter() {
            let interior = match source {
                InteriorRoots::Superset => FiledInterior::Superset(copied[first + 1]),
                InteriorRoots::CapturesUnknown => FiledInterior::CapturesUnknown(copied[first + 1]),
                InteriorRoots::Checked => FiledInterior::Exact(copied[first + 1]),
                InteriorRoots::Flow(parts) => FiledInterior::Exact(self.exact_tuple(owner, parts, &copied[first + 1..first + 1 + parts.len()])?),
            };
            coroutines.push((at, copied[first], interior));
        }
        // A later flow result may complete an earlier check. Its owner, output and opaque
        // definitions still belong to the same snapshot. An exact witness never regresses.
        if let Some(prior) = self.rows.get(&owner) {
            let changed = HiddenFail::Copy { owner, why: "two checks of the same owner gave different coroutine or opaque facts" };
            if prior.hidden != hidden || prior.coroutines.len() != coroutines.len() {
                return Err(changed);
            }
            for (&(old_at, old_output, previous), (at, output, next)) in prior.coroutines.iter().zip(coroutines.iter_mut()) {
                if old_at != *at || old_output != *output {
                    return Err(changed);
                }
                // Exact over Superset over CapturesUnknown: a row only gains precision.
                let rank = |f: FiledInterior| match f {
                    FiledInterior::CapturesUnknown(_) => 0u8,
                    FiledInterior::Superset(_) => 1,
                    FiledInterior::Exact(_) => 2,
                };
                match (previous, *next) {
                    _ if previous == *next => {}
                    (FiledInterior::Exact(_), FiledInterior::Exact(_)) => return Err(changed),
                    _ if rank(previous) > rank(*next) => *next = previous,
                    _ if rank(previous) < rank(*next) => {}
                    _ => return Err(changed),
                }
            }
        }
        self.rows.insert(owner, Row { hidden, coroutines });
        Ok(())
    }

    /// The exact interior is a set for structural auto traits. Two callers file that
    /// set in the same order even when their tables interned its elements differently.
    fn exact_tuple(&mut self, owner: ItemKey, parts: &[InteriorPart], tys: &[PTy]) -> Result<PTy, HiddenFail> {
        let full = || HiddenFail::Copy { owner, why: "the hidden types' table is full" };
        let mut elems: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(parts.len(), self.alloc);
        for (p, &t) in parts.iter().zip(tys) {
            elems.push(match p.wrap {
                Some(m) => self.tys.intern(PNode::Ref(m, t)).map_err(|_| full())?,
                None => t,
            });
        }
        if elems.len() > 1 {
            let types = self.tys.view();
            let mut ordered = Vec::with_capacity_in(elems.len(), self.alloc);
            for &ty in &elems {
                let digest = Hidden { ty, types }.answer_digest(self.alloc).map_err(HiddenFail::Coroutine)?;
                ordered.push((digest, ty));
            }
            ordered.sort_unstable();
            ordered.dedup();
            if ordered.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(HiddenFail::Copy { owner, why: "distinct coroutine interior types have one content digest" });
            }
            elems.clear();
            elems.extend(ordered.iter().map(|&(_, ty)| ty));
        }
        let list = self.tys.intern_list(&elems).map_err(|_| full())?;
        self.tys.intern(PNode::Tuple(list)).map_err(|_| full())
    }
}

/// Why an opaque's hidden type could not be given.
#[derive(Clone, Debug)]
pub enum HiddenFail {
    /// No fn head of the program made this opaque (`Store::opaques` holds no row).
    NoHead { opaque: ItemKey },
    /// An auto trait of a type alias's `impl Trait` (front2/tait.rs): its hidden type is its
    /// defining uses', which are not checked as defining yet.
    AliasOpaque { opaque: ItemKey, alias: ItemKey },
    /// The defining body would be lowered with the build's prepared macros, and this caller
    /// has none (`Records::consts` is `None`).
    NoPrepares { opaque: ItemKey, owner: ItemKey, shown: Box<str> },
    /// The defining body did not check: its refusal, as printed.
    Defining { opaque: ItemKey, owner: ItemKey, shown: Box<str>, why: Box<str> },
    /// An auto trait of an `impl Trait` in the BOUNDS of a trait fn's `impl Trait` (`trait T {
    /// fn f(&self) -> impl Iterator<Item = impl Future>; }`), required or provided. The
    /// Reference, "Impl trait", "`impl Trait` in traits": each impl's fn gives its own type, so
    /// through a generic only the trait's bounds hold and at a concrete self type the impl's
    /// type leaks. An opaque at a PLACE of a trait fn's return type is answered before this
    /// table (front2 rpitit.rs: its hidden type is the synthetic projection, a provided fn's
    /// default its body's); one in another's bounds has no place in an impl's type, and this
    /// table, one hidden type per opaque, cannot reveal it per impl (a provided fn's body's type
    /// for it would be wrong through an impl that writes the fn). The gap's opaque may have been
    /// named only for its bounds (blaze-solve `Ask::first_hidden_unknown` is a superset).
    TraitFnInBounds { opaque: ItemKey, owner: ItemKey, shown: Box<str> },
    /// The defining body checked and its record names no hidden type for the opaque.
    NoneGiven { opaque: ItemKey, owner: ItemKey, shown: Box<str> },
    /// An auto trait of `opaque` asked while checking `asked_by`, which is its defining body or
    /// is checked to define an opaque the defining body is under on the work list: E0391's class.
    Cycle { opaque: ItemKey, asked_by: ItemKey, shown: Box<str> },
    /// A clean record's type could not be filed (a hole or an error type in it, an index past
    /// its table, a full table).
    Copy { owner: ItemKey, why: &'static str },
    /// A local coroutine's expression or closure signature did not yield its checked facts.
    Coroutine(tc::CheckError),
    /// The work list passed [`MOST_DEFINES`] checks.
    TooMany { opaque: ItemKey, most: u32 },
}

impl core::fmt::Display for HiddenFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HiddenFail::NoHead { opaque } => write!(f, "the `impl Trait` type {opaque:?}: no fn head of the program made it"),
            HiddenFail::AliasOpaque { opaque, alias } => write!(f, "the `impl Trait` type {opaque:?} of type alias {alias:?}: {}", super::tait::HIDDEN_OWED),
            HiddenFail::NoPrepares { opaque, owner, shown } => {
                write!(f, "the hidden type of the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}): this caller has no prepared macros to lower its defining body with")
            }
            HiddenFail::Defining { opaque, owner, shown, why } => {
                write!(f, "the hidden type of the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}), for an auto trait: its defining body did not check: {why}")
            }
            HiddenFail::TraitFnInBounds { opaque, owner, shown } => write!(
                f,
                "the `impl Trait` type {opaque:?} in the bounds of another `impl Trait` of the trait fn `{shown}` ({owner:?}), for an auto trait: each impl's fn gives its own (the Reference, \"Impl trait\", \"`impl Trait` in traits\"), and this front end reveals one in another's bounds per impl not yet"
            ),
            HiddenFail::NoneGiven { opaque, owner, shown } => {
                write!(f, "the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}): its defining body's record gives no hidden type for it")
            }
            HiddenFail::Cycle { opaque, asked_by, shown } => write!(
                f,
                "cycle (E0391): an auto trait of the `impl Trait` type {opaque:?} is asked while checking fn `{shown}` ({asked_by:?}), which is that type's defining body or is checked to define it (the Reference, \"Impl trait\", \"Auto trait leakage\")"
            ),
            HiddenFail::Copy { owner, why } => write!(f, "the hidden types of fn {owner:?}'s clean check could not be filed: {why}"),
            HiddenFail::Coroutine(e) => core::fmt::Display::fmt(e, f),
            HiddenFail::TooMany { opaque, most } => {
                write!(f, "the hidden type of the `impl Trait` type {opaque:?}: its defining bodies need more than {most} checks (rule 18)")
            }
        }
    }
}

fn gap(owner: ItemKey, fail: HiddenFail) -> CheckFail {
    let mut gaps = std::vec::Vec::new();
    gaps.push(Gap::Hidden(fail));
    CheckFail::Gap { owner, gaps }
}

fn shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str> {
    item_shown(r, k).into_boxed_str()
}

/// Files `body`'s clean check `done` in `Store::hidden` when it defines an opaque or a coroutine. Called where
/// a clean check's read list is filed (front2 body_reads.rs), so every clean check is filed
/// once, the driver's and the work list's alike. Its async blocks' exact interiors are made
/// here, only for a body that is filed (rule 77: exact_interior.rs).
pub(crate) fn file<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, body: &Body<'_>, done: &Checked<'_>) -> Result<(), CheckFail> {
    let owner = body.typeck.owner;
    if !r.store.hidden.wants(owner, &body.typeck.exprs, done) {
        return Ok(());
    }
    let alloc = r.store.tys.alloc();
    // Only suspension liveness can refine NeedsSavedLocals. Complete witnesses
    // (including a direct await) already carry both the captures and pending
    // future. Flow uses the same binding rows and cannot refine unknown places.
    let needs_flow = body.typeck.exprs.iter().enumerate().any(|(i, e)| {
        matches!(e, tc::Expr::AsyncBlock { .. })
            && done.coroutine_witness(tc::ExprId(i as u32)) == Some(tc::CoroutineWitness::NeedsSavedLocals)
    });
    let exacts = if needs_flow {
        exact_interiors(r, body, done, alloc).map_err(|error| CheckFail::Check { owner, error })?
    } else {
        None
    };
    let head = r.store.fns.get(&owner).ok_or_else(|| gap(owner, HiddenFail::Copy {
        owner, why: "missing function head for the coroutine owner's generic arguments",
    }))?;
    r.store.hidden.file_row(&body.typeck.view(tc::PTys::EMPTY), head, done, exacts.as_ref()).map_err(|f| gap(owner, f))
}

/// After a round of `owner`'s check stopped with `error`: whether the round is to run again
/// with `leaks` grown. Called by the check loop (front2 check.rs `check_rounds`) with the
/// round's world dropped.
pub(crate) fn after_auto_gap<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    owner: ItemKey,
    error: &tc::CheckError,
    leaks: &mut Vec<ItemKey, Alloc<'static>>,
) -> Result<bool, CheckFail> {
    let tc::CheckError::OpaqueHiddenUnknown { opaque, .. } = *error else { return Ok(false) };
    // Already read and still unknown: the error stands (the world answered its gap instead).
    if leaks.binary_search(&opaque).is_ok() {
        return Ok(false);
    }
    // The default of a provided trait fn's `impl Trait` is defined by that fn's body
    // (front2 rpitit.rs `Twin`): its source's row.
    let filed = super::rpitit::filed_as(r.store, opaque);
    let Some(&(def, _)) = r.store.opaques.get(&filed) else { return Err(gap(owner, HiddenFail::NoHead { opaque })) };
    if def == owner || r.store.hidden.defining.contains(&def) {
        // Inside the work list: the cycle is the list's answer for the body that asked it
        // first (`run`), typed, not the refusal of one body on it as text.
        if !r.store.hidden.defining.is_empty() {
            r.store.hidden.cycle = Some((opaque, owner));
        }
        return Err(gap(owner, HiddenFail::Cycle { opaque, asked_by: owner, shown: shown(r, owner) }));
    }
    if !r.store.hidden.filed(def) {
        if !r.store.hidden.defining.is_empty() {
            // Inside the work list: it defines this one first, then checks this body again.
            r.store.hidden.wanted = Some(opaque);
            return Ok(false);
        }
        define(r, opaque).map_err(|f| gap(owner, f))?;
    }
    if let Err(at) = leaks.binary_search(&opaque) {
        leaks.insert(at, opaque);
    }
    Ok(true)
}

/// Files the defining body of `opaque` (and of every opaque its check stops on first), by a
/// work list.
fn define<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, opaque: ItemKey) -> Result<(), HiddenFail> {
    let mut stack: Vec<ItemKey, Alloc<'static>> = r.store.hidden.new_leaks();
    stack.push(opaque);
    let got = run(r, &mut stack);
    r.store.hidden.defining.clear();
    r.store.hidden.wanted = None;
    r.store.hidden.cycle = None;
    got
}

fn run<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, stack: &mut Vec<ItemKey, Alloc<'static>>) -> Result<(), HiddenFail> {
    for _ in 0..MOST_DEFINES {
        let Some(&opaque) = stack.last() else { return Ok(()) };
        // A provided trait fn's default `impl Trait` is filed under its source (rpitit.rs).
        let filed = super::rpitit::filed_as(r.store, opaque);
        let Some(&(owner, index)) = r.store.opaques.get(&filed) else { return Err(HiddenFail::NoHead { opaque }) };
        if super::tait::of_alias(index) {
            return Err(HiddenFail::AliasOpaque { opaque, alias: owner });
        }
        if r.store.hidden.filed(owner) {
            if !r.store.hidden.gives(owner, filed) {
                return Err(HiddenFail::NoneGiven { opaque, owner, shown: shown(r, owner) });
            }
            stack.pop();
            continue;
        }
        // Every defining fn on the list: a body checked now that needs an auto trait of one of
        // their opaques is the cycle.
        r.store.hidden.defining.clear();
        for &o in stack.iter() {
            if let Some(&(w, _)) = r.store.opaques.get(&super::rpitit::filed_as(r.store, o)) {
                r.store.hidden.defining.push(w);
            }
        }
        match check_defining(r, opaque, owner)? {
            // Filed by the check itself (body_reads.rs); a body that defines nothing files
            // nothing, which the top of the loop names.
            Ok(()) if r.store.hidden.filed(owner) => {}
            Ok(()) => return Err(HiddenFail::NoneGiven { opaque, owner, shown: shown(r, owner) }),
            // Two async fns that each ask an auto trait of the other's future in their own
            // bodies (`async fn ping() { spawn(pong()) }`, `async fn pong() { spawn(ping()) }`):
            // each defining body waits on the other's. rustc stops there with a query cycle
            // (E0391: `type_of` of an opaque needs its defining body's `typeck`); the Reference
            // states no answer. A typed gap naming the cycle, never an error, never a pass.
            Err(why) => match (r.store.hidden.cycle.take(), r.store.hidden.wanted.take()) {
                (Some((o, asked_by)), _) => return Err(HiddenFail::Cycle { opaque: o, asked_by, shown: shown(r, asked_by) }),
                (None, Some(w)) => stack.push(w),
                (None, None) => return Err(HiddenFail::Defining { opaque, owner, shown: shown(r, owner), why }),
            },
        }
    }
    let first = stack.first().copied().unwrap_or(ItemKey(0));
    Err(HiddenFail::TooMany { opaque: first, most: MOST_DEFINES })
}

/// `owner`'s body lowered and checked (a clean check files itself: body_reads.rs), or its
/// refusal as printed (`Ok(Err)`, read by the work list with `wanted`).
fn check_defining<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, opaque: ItemKey, owner: ItemKey) -> Result<Result<(), Box<str>>, HiddenFail> {
    let Some(prepared) = r.consts.as_ref().map(|c| c.1) else {
        return Err(HiddenFail::NoPrepares { opaque, owner, shown: shown(r, owner) });
    };
    // A head or a constant the lowering needs is made and the body lowered again (front2
    // lower_made.rs, the one home of that loop: rule 30). A raw lowering here printed the need
    // as the defining body's refusal: five of tempo's async fns (`zone_info`, `set_leader`,
    // `published_payload`, `supervise_sequencer_tasks`, `authenticate_token`, c0410wave90)
    // were "not lowered: to_typeck refused a const argument (NeedConst ..)", and every body
    // that asked an auto trait of their futures stopped on it.
    let target = match r.keys.library_in(r.names, owner) {
        Some(lib) => LowerTarget::Free(lib),
        None => LowerTarget::Member(owner),
    };
    let lowered = match lower_made(r, prepared, target) {
        Ok(l) => l,
        // A required trait fn's `impl Trait` that has no synthetic row (one in another's
        // bounds: front2 rpitit.rs; one at a place reveals its projection and never comes
        // here): no body defines it, so no check of one can reveal it.
        Err(BodyError::NoBody(_)) => return Err(HiddenFail::TraitFnInBounds { opaque, owner, shown: shown(r, owner) }),
        Err(e) => return Ok(Err(format!("its body was not lowered: {e}").into_boxed_str())),
    };
    let view = lowered.lowered.view();
    let spell = match Spelling::of(&view) {
        Ok(s) => s,
        Err(why) => return Ok(Err(format!("its body's names were not spelled: {why:?}").into_boxed_str())),
    };
    let scope = lowered.scope(&spell);
    // The check the command's own check of this body makes (`ProjectWorld::check`, which
    // `blaze check` runs: `Stalls::Decide`), so the row filed is the same whichever path checks
    // the body first (rule 21; the module doc, "the same answers in any order"). A goal on one
    // of its async blocks' futures that fails only on the check's superset interior is a
    // STALLED ROW there, decided on the exact interior by the body's own check (borrows.rs
    // `witness_and_decide`); the hidden type this check files does not depend on it (the
    // Reference, "Impl trait": the defining body's type check chooses it). The `Stalls::Gap`
    // entry this took refused the whole body instead (`INTERIOR_GAP`): every async fn that
    // `tokio::spawn`s a local `async move` block (tempo zones/rpc ws.rs `handle_ws_session`,
    // run c0545wave91) hid its future's auto traits from every caller.
    // Its read list filed as `check_body_asked` files it (body_reads.rs `check_body_keyed`): a
    // later check of the same body that reads other answers is `ReadsDiffer` (rule 21). With no
    // stalled row the two entries ask the same questions; with one, the `Gap` entry stops and
    // files nothing to compare.
    let checked = super::body_reads::check_body_filed_as(r, &lowered, &scope, None, None, super::stalled_goals::Stalls::Decide)
        .and_then(|(_, filed)| {
            let scratch = r.store.tys.alloc();
            r.store.reads.file(lowered.typeck.owner, &filed.reads, scratch)
        });
    Ok(match checked {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("{e}").into_boxed_str()),
    })
}

/// `roots` of `from` interned into `out`, each reached node after its children (a table holds
/// a node after its children: `PortableTys::intern` takes their handles), by a work list (no
/// recursion). Parameters stay the defining fn's own. A nested future names its owner's
/// coroutine row and arguments so a reader can retrieve its facts. A hole or an error type is refused:
/// a clean record holds none (`CheckError::UnknownInCleanBody`).
fn copy_reached<'a>(
    from: PTys<'_>, roots: &[PTy], out: &mut PortableTys<'a>, alloc: Alloc<'a>,
    body: &tc::BodyView<'_>, head: &super::heads::FnHead<'_>,
) -> Result<Vec<PTy, Alloc<'a>>, &'static str> {
    const PAST: &str = "a type past its record's table";
    const FULL: &str = "the hidden types' table is full";
    let n = from.len();
    let mut seen: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        seen.push(false);
    }
    let mut work: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(roots.len(), alloc);
    for &t in roots {
        work.push(t);
    }
    while let Some(t) = work.pop() {
        let at = t.0 as usize;
        match seen.get(at) {
            None => return Err(PAST),
            Some(true) => continue,
            Some(false) => {}
        }
        seen[at] = true;
        let node = from.node(t).map_err(|_| PAST)?;
        // A nested future becomes a coroutine carrying `owner_args`, never its own output/interior.
        if !matches!(node, PNode::Future(..)) {
            from.children_of(node, &mut work).map_err(|_| PAST)?;
        }
    }
    let mut mapped: Vec<Option<PTy>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        mapped.push(None);
    }
    let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    let generics = head.parent + head.own;
    let mut params = Vec::with_capacity_in(generics as usize, alloc);
    for i in 0..generics {
        let is_const = head.consts.iter().any(|&(index, _)| index == i);
        let param = if is_const { PNode::ConstParam(i) } else { PNode::Param(i) };
        params.push(out.intern(param).map_err(|_| FULL)?);
    }
    let owner_args = out.intern_list(&params).map_err(|_| FULL)?;
    for i in 0..n {
        if !seen[i] {
            continue;
        }
        let node = from.node(PTy(i as u32)).map_err(|_| PAST)?;
        let m = |c: PTy, mapped: &[Option<PTy>]| -> Result<PTy, &'static str> {
            mapped.get(c.0 as usize).copied().flatten().ok_or("a type whose child is after it in its record's table")
        };
        let mut l = |l: List, mapped: &[Option<PTy>], out: &mut PortableTys<'a>| -> Result<List, &'static str> {
            kids.clear();
            for &c in from.list(l).map_err(|_| PAST)? {
                kids.push(m(c, mapped)?);
            }
            out.intern_list(&kids).map_err(|_| FULL)
        };
        let copied = match node {
            PNode::Infer | PNode::Error => return Err("a hole or an error type in a clean record's hidden type"),
            n @ (PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_)) => n,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty, &mapped)?, bits },
            PNode::Tuple(a) => PNode::Tuple(l(a, &mapped, out)?),
            PNode::Record(s, a) => PNode::Record(s, l(a, &mapped, out)?),
            PNode::Union(a) => PNode::Union(l(a, &mapped, out)?),
            PNode::Ref(mu, a) => PNode::Ref(mu, m(a, &mapped)?),
            PNode::Ptr(mu, a) => PNode::Ptr(mu, m(a, &mapped)?),
            PNode::Array(a, len) => PNode::Array(m(a, &mapped)?, m(len, &mapped)?),
            PNode::Slice(a) => PNode::Slice(m(a, &mapped)?),
            PNode::Pat(a, r) => PNode::Pat(m(a, &mapped)?, l(r, &mapped, out)?),
            PNode::Adt(k, a) => PNode::Adt(k, l(a, &mapped, out)?),
            PNode::FnDef(k, a) => PNode::FnDef(k, l(a, &mapped, out)?),
            PNode::FnPtr(a, h) => PNode::FnPtr(l(a, &mapped, out)?, h),
            PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: l(args, &mapped, out)? },
            PNode::Future(e, ..) => PNode::Coroutine {
                owner: body.owner,
                ordinal: tc::coroutine_id::coroutine_ordinal(body, e)
                    .ok_or("a nested future has no coroutine ordinal in its defining body")?,
                args: owner_args,
            },
            PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: l(args, &mapped, out)? },
            PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: l(args, &mapped, out)? },
            PNode::Opaque(k, a) => PNode::Opaque(k, l(a, &mapped, out)?),
            PNode::Dynamic(a) => PNode::Dynamic(l(a, &mapped, out)?),
            PNode::ExTrait(k, a) => PNode::ExTrait(k, l(a, &mapped, out)?),
            PNode::ExProj { trait_, name, args, term } => PNode::ExProj { trait_, name, args: l(args, &mapped, out)?, term: m(term, &mapped)? },
        };
        mapped[i] = Some(out.intern(copied).map_err(|_| FULL)?);
    }
    let mut got = Vec::with_capacity_in(roots.len(), alloc);
    for &t in roots {
        got.push(mapped.get(t.0 as usize).copied().flatten().ok_or(PAST)?);
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::HiddenTable;
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::front2::stalled_goals::Stalls;
    use crate::refusal::SourceFile;

    /// A defining body checked FOR a caller's auto trait takes the command's own check
    /// (`check_defining`, `Stalls::Decide`). Reduced from tempo zones/rpc ws.rs
    /// `handle_ws_session` (run c0545wave91: 2 of the 9 interior-gap lines): an async fn whose
    /// body hands a local `async` block to a `Send` bound (`tokio::spawn(async move { .. })`)
    /// that holds only on the block's exact interior. rustc accepts every fn here but
    /// `spawns_local_bad` (E0277: its block holds an `Rc` across its await) and `reads_send_bad`
    /// (E0277: `spawns_local_holds_rc`'s future holds an `Rc` across its own await); the
    /// Reference, "Impl trait", "Auto trait leakage", and "Async blocks" (a future stores what
    /// it needs to resume). Before, the caller checked first refused with `INTERIOR_GAP`
    /// ("its defining body did not check"), while the same defining body checked first was
    /// clean: an answer that depended on the order (rule 21).
    #[test]
    fn a_defining_body_with_a_stalled_send_reveals_its_future_to_a_caller() {
        const SOURCE: &str = r#"
use std::rc::Rc;
fn require_send<T: Send>(_: T) {}
async fn spawns_local() {
    require_send(async {
        let r = Rc::new(1u8);
        drop(r);
        async {}.await;
    });
    async {}.await;
}
async fn spawns_local_holds_rc() {
    require_send(async {
        let r = Rc::new(1u8);
        drop(r);
        async {}.await;
    });
    let r = Rc::new(1u8);
    async {}.await;
    drop(r);
}
async fn spawns_local_bad() {
    require_send(async {
        let r = Rc::new(1u8);
        async {}.await;
        drop(r);
    });
    async {}.await;
}
fn reads_send() { require_send(spawns_local()); }
fn reads_send_bad() { require_send(spawns_local_holds_rc()); }
fn reads_send_of_bad() { require_send(spawns_local_bad()); }
fn main() {}
"#;
        let alloc = blaze_own::testing::heap().alloc();
        let home = std::env::var("HOME").expect("HOME");
        let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("fixture toolchain");
        let files = [("/spawned/src/main.rs".to_owned(), SOURCE.to_owned())];
        let user = UserCrate {
            name: "spawned", dir: "/spawned/", root: "/spawned/src/main.rs", files: &files,
            edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
        };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture crate");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("fixture world");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies.iter().find_map(|&(body, _)| match body {
                BodyRef::Free(k) if names.item_text(k)?.name == Some(name) => Some(body),
                _ => None,
            }).unwrap_or_else(|| panic!("missing fixture function `{name}`"))
        };

        // The callers FIRST: each defining body is checked for them (`check_defining`).
        let pass = world.check(body("reads_send"), alloc);
        assert!(pass.is_clean() && pass.undecided.is_empty() && pass.reads.is_some(), "reads_send: {pass:?}");
        let of_bad = world.check(body("reads_send_of_bad"), alloc);
        assert!(of_bad.is_clean() && of_bad.undecided.is_empty(), "reads_send_of_bad: {of_bad:?}");
        let control = world.check(body("reads_send_bad"), alloc);
        assert!(control.refused.is_none() && control.gaps.is_empty() && control.undecided.is_empty(), "reads_send_bad: {control:?}");
        let [d] = control.diagnostics.as_slice() else { panic!("one error for the Rc across the await: {control:?}") };
        assert_eq!(d.code, "E0277", "{control:?}");

        // Then the defining bodies themselves: the stalled row decided on the exact interior.
        let defining = world.check(body("spawns_local"), alloc);
        assert!(defining.is_clean() && defining.undecided.is_empty(), "spawns_local: {defining:?}");
        let bad = world.check(body("spawns_local_bad"), alloc);
        assert!(bad.refused.is_none() && bad.gaps.is_empty(), "spawns_local_bad: {bad:?}");
        let [d] = bad.diagnostics.as_slice() else { panic!("one error for the inner block's Rc across its await: {bad:?}") };
        assert_eq!(d.code, "E0277", "{bad:?}");
    }

    /// The same `Rc<u8>` occurs in both async bodies. Only a value that still
    /// belongs to the future at suspension can prevent moving it to another thread. Before
    /// flow, both a sendable candidate tuple and one containing `Rc` leave the obligation open.
    #[test]
    fn an_async_fns_send_bound_waits_for_its_exact_interior() {
        const SOURCE: &str = r#"
use std::rc::Rc;
fn require_send<T: Send>(_: T) {}
fn require_unit_output<F: std::future::Future<Output = ()>>(_: F) {}
async fn dropped_before() {
    let r = Rc::new(1u8);
    drop(r);
    async {}.await;
}
async fn held_across() {
    let r = Rc::new(1u8);
    async {}.await;
    drop(r);
}
async fn bytes_only() {
    let b = 1u8;
    async {}.await;
    drop(b);
}
fn pass() { require_send(dropped_before()); }
fn control() { require_send(held_across()); }
fn output_only() { require_unit_output(dropped_before()); }
fn superset_send() { require_send(bytes_only()); }
fn local_send() {
    require_send(async {
        let b = 1u8;
        async {}.await;
        drop(b);
    });
}
fn local_send_rc() {
    require_send(async {
        let r = Rc::new(1u8);
        async {}.await;
        drop(r);
    });
}
fn pinned() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> { Box::pin(bytes_only()) }
fn boxed() -> Box<dyn std::future::Future<Output = ()> + Send> { Box::new(bytes_only()) }
fn main() {}
"#;
        let alloc = blaze_own::testing::heap().alloc();
        let home = std::env::var("HOME").expect("HOME");
        let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("fixture toolchain");
        let files = [("/witness/src/main.rs".to_owned(), SOURCE.to_owned())];
        let user = UserCrate {
            name: "witness", dir: "/witness/", root: "/witness/src/main.rs", files: &files,
            edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
        };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture crate");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("fixture world");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies.iter().find_map(|&(body, _)| match body {
                BodyRef::Free(k) if names.item_text(k)?.name == Some(name) => Some(body),
                _ => None,
            }).unwrap_or_else(|| panic!("missing fixture function `{name}`"))
        };

        // File the same defining check without its flow facts, then with them.
        // This holds the caller, the body and the type table contents of the
        // defining check constant across the two reads.
        let mut provisional = HiddenTable::new_in(alloc);
        let mut root_coroutine = None;
        let producer = world.check_then(body("dropped_before"), Stalls::Gap, alloc, |r, lowered, done| {
            let view = lowered.typeck.view(blaze_typeck::PTys::EMPTY);
            let head = r.store.fns.get(&view.owner).expect("the defining fn's signature");
            provisional.file_row(&view, head, done, None).expect("file without flow facts");
            let (ordinal, ..) = done.coroutine_at(&view, view.root).expect("the root block's facts").expect("an async fn");
            root_coroutine = Some((view.owner, ordinal));
        });
        assert!(producer.is_clean() && producer.reads.is_some(), "{producer:?}");
        let (owner, ordinal) = root_coroutine.expect("the defining check ran");
        let mut bytes_coroutine = None;
        let bytes = world.check_then(body("bytes_only"), Stalls::Gap, alloc, |r, lowered, done| {
            let view = lowered.typeck.view(blaze_typeck::PTys::EMPTY);
            let head = r.store.fns.get(&view.owner).expect("the defining fn's signature");
            provisional.file_row(&view, head, done, None).expect("file without flow facts");
            let (ordinal, ..) = done.coroutine_at(&view, view.root).expect("the root block's facts").expect("an async fn");
            bytes_coroutine = Some((view.owner, ordinal));
        });
        assert!(bytes.is_clean(), "{bytes:?}");
        let bytes_root = bytes_coroutine.expect("the defining check ran");
        world.store.hidden = provisional;
        // Both rows are supersets now.
        assert!(matches!(world.store.hidden.coroutine(owner, ordinal),
            super::CoroutineAnswer::Facts(f) if f.is == blaze_typeck::InteriorIs::Superset));
        let output_only = world.check(body("output_only"), alloc);
        assert!(output_only.is_clean() && output_only.undecided.is_empty() && output_only.reads.is_some(), "{output_only:?}");
        // A LOCAL async block's interior is never another fn's record: `ProjectWorld::check`
        // (`Stalls::Decide`) lowers the block's own witness (borrows.rs `witness_and_decide`)
        // and decides the stalled `Send` on that exact interior, whatever the hidden table
        // holds. rustc accepts `local_send` (it stores a `u8` and the awaited `async {}`: the
        // Reference, "Async blocks", "Auto traits") and rejects `local_send_rc` with E0277 (an
        // `Rc<u8>` held across the `.await`: alloc's `impl !Send for Rc`). The gap this list
        // expected for `local_send` was the `Stalls::Gap` entry's, which lowers no witness; the
        // decision with its witness removed stays a gap (stalled_goals.rs
        // `a_refused_witness_never_decides_a_local_send`).
        let local = world.check(body("local_send"), alloc);
        assert!(local.is_clean() && local.undecided.is_empty() && local.reads.is_some(), "local_send: {local:?}");
        let local_rc = world.check(body("local_send_rc"), alloc);
        assert!(local_rc.refused.is_none() && local_rc.gaps.is_empty() && local_rc.undecided.is_empty(), "local_send_rc: {local_rc:?}");
        let [d] = local_rc.diagnostics.as_slice() else { panic!("one error for the local block's Rc across its await: {local_rc:?}") };
        assert_eq!(d.code, "E0277", "{local_rc:?}");
        // The witness gap names the record it lacks (rule 19; tempo bin/tempo/src/tempo_cmd.rs
        // `execute`, where the gap named no fn): the producer's root coroutine, by owner and
        // ordinal, the first superset record reached. CONTROL (rule 23): `superset_send` and
        // `pass` reach DIFFERENT producers, so each must name its own, not a fixed one.
        for (caller, reason, names) in [
            ("superset_send", blaze_typeck::EXTERNAL_WITNESS_GAP, Some(bytes_root)),
            ("pass", blaze_typeck::EXTERNAL_WITNESS_GAP, Some((owner, ordinal))),
            ("pinned", blaze_typeck::INTERIOR_GAP, None),
            ("boxed", blaze_typeck::INTERIOR_GAP, None),
        ] {
            let waiting = world.check(body(caller), alloc);
            assert!(waiting.diagnostics.is_empty() && waiting.reads.is_none() && waiting.undecided.is_empty(), "{caller}: {waiting:?}");
            let shown = format!("{:?} {:?}", waiting.refused, waiting.gaps);
            assert!(shown.contains(reason) && !waiting.is_clean(), "{caller}: {waiting:?}");
            if let Some((item, nth)) = names {
                let named = format!("ForeignWitness {{ gap: {reason:?}, item: {item:?}, ordinal: Some({nth})");
                assert!(shown.contains(&named), "{caller}: the gap names the producer's record {item:?} #{nth}: {waiting:?}");
            }
        }
        assert_ne!(bytes_root.0, owner, "CONTROL: the two producers are different fns");

        let exact_bytes = world.check(body("bytes_only"), alloc);
        assert!(exact_bytes.is_clean() && exact_bytes.reads.is_some(), "{exact_bytes:?}");
        for caller in ["superset_send", "pinned", "boxed"] {
            let v = world.check(body(caller), alloc);
            assert!(v.is_clean() && v.undecided.is_empty() && v.reads.is_some(), "{caller}: {v:?}");
        }

        let exact = world.check(body("dropped_before"), alloc);
        assert!(exact.is_clean() && exact.reads.is_some(), "{exact:?}");
        let pass = world.check(body("pass"), alloc);
        assert!(pass.is_clean() && pass.undecided.is_empty() && pass.reads.is_some(), "{pass:?}");

        let control = world.check(body("control"), alloc);
        assert!(control.refused.is_none() && control.gaps.is_empty() && control.undecided.is_empty(), "{control:?}");
        let [d] = control.diagnostics.as_slice() else { panic!("one error for the Rc across the await: {control:?}") };
        assert_eq!(d.code, "E0277", "{control:?}");
        assert!(d.text.contains("Send") && d.start < d.end, "{control:?}");
    }
}
