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
//! THE STORE ([`HiddenTable`], `Store::hidden`): every body that checks clean in this command
//! and defines an opaque files its hidden types and its async blocks' facts here, copied into
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

use super::body::{Body, lower_fn, lower_member};
use super::check::{CheckFail, Records, check_body, item_shown};
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
    /// Each async block of the body by its ordinal (blaze-typeck coroutine_id.rs
    /// `async_ordinal`: the count of `Expr::AsyncBlock`s before it): its output and interior,
    /// from its `PNode::Future` expression type.
    coroutines: Vec<(u32, PTy, PTy), Alloc<'a>>,
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
}

/// What the world answers for an opaque.
pub enum HiddenAnswer<'t> {
    /// Its defining body's hidden type.
    Known(Hidden<'t>),
    /// Opaque to this check: not in its leak list.
    Opaque,
    /// In the leak list, and its defining body's row gives no hidden type for it.
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
        }
    }

    /// An empty leak list for one check, in the table's allocator.
    pub fn new_leaks(&self) -> Vec<ItemKey, Alloc<'a>> {
        Vec::new_in(self.alloc)
    }

    /// The running check's leak list (the check loop sets it before each round's world).
    pub fn set_leaking(&mut self, leaks: &[ItemKey]) {
        self.leaking.clear();
        for &k in leaks {
            self.leaking.push(k);
        }
    }

    /// The hidden type of `opaque`, defined by fn `owner`, to the running check.
    pub fn answer(&self, opaque: ItemKey, owner: ItemKey) -> HiddenAnswer<'_> {
        if self.leaking.binary_search(&opaque).is_err() {
            return HiddenAnswer::Opaque;
        }
        match self.rows.get(&owner).and_then(|row| row.hidden.iter().find(|h| h.0 == opaque)) {
            Some(&(_, ty)) => HiddenAnswer::Known(Hidden { ty, types: self.tys.view() }),
            None => HiddenAnswer::NoneGiven,
        }
    }

    /// The facts of async block `ordinal` of `owner`, from its filed row; `None` while that
    /// body is not filed or holds no such block typed as a future.
    pub fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
        let row = self.rows.get(&owner)?;
        let &(_, output, interior) = row.coroutines.iter().find(|c| c.0 == ordinal)?;
        Some(CoroutineFacts { output, interior, types: self.tys.view() })
    }

    fn filed(&self, owner: ItemKey) -> bool {
        self.rows.contains_key(&owner)
    }

    fn gives(&self, owner: ItemKey, opaque: ItemKey) -> bool {
        self.rows.get(&owner).is_some_and(|row| row.hidden.iter().any(|h| h.0 == opaque))
    }

    /// Whether `owner`'s clean check is to be filed: it defines an opaque and is not filed yet
    /// (a second clean check of one body reads the same answers: rule 21, `Digests::file`).
    fn wants(&self, owner: ItemKey, done: &Checked<'_>) -> bool {
        !done.hidden.is_empty() && !self.filed(owner)
    }

    /// `owner`'s clean check `done` (its body's expressions `exprs`) filed, when [`Self::wants`]
    /// it. Each async block's interior is its EXACT interior where `exacts` gives one
    /// (exact_interior.rs: what the state stores), else the check's superset (a block whose
    /// flow was refused: exact_interior.rs's module doc).
    fn file_row(&mut self, owner: ItemKey, exprs: &[tc::Expr], done: &Checked<'_>, exacts: Option<&Exacts<'_>>) -> Result<(), HiddenFail> {
        if !self.wants(owner, done) {
            return Ok(());
        }
        let from = done.types.view();
        // The roots: each hidden type, then each async block's output and its interior (the
        // superset, one root, or the exact interior's parts, one root each).
        let mut roots: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(done.hidden.len(), self.alloc);
        for &(_, t) in done.hidden.iter() {
            roots.push(t);
        }
        // Per block: its ordinal, its output's index in `roots`, and its exact parts if any.
        let mut blocks: Vec<(u32, usize, Option<&[InteriorPart]>), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut ordinal = 0u32;
        for (i, e) in exprs.iter().enumerate() {
            if !matches!(e, tc::Expr::AsyncBlock { .. }) {
                continue;
            }
            let at = ordinal;
            ordinal += 1;
            let Some(&t) = done.expr_ty.get(i) else {
                return Err(HiddenFail::Copy { owner, why: "its record has no type for one of its async blocks" });
            };
            // A block not typed as its own future gets no row: the world's gap by name
            // (`Question::Coroutine`) if it is ever asked, never a guess.
            if let Ok(PNode::Future(_, output, interior)) = from.node(t) {
                let exact = match exacts.and_then(|x| x.of(tc::ExprId(i as u32))) {
                    Some(BlockInterior::Exact(parts)) => Some(&parts[..]),
                    // Its flow was refused, or no exact interior was made: the superset stands.
                    Some(BlockInterior::Refused(_)) | None => None,
                };
                blocks.push((at, roots.len(), exact));
                roots.push(output);
                match exact {
                    Some(parts) => {
                        for p in parts {
                            roots.push(p.ty);
                        }
                    }
                    None => roots.push(interior),
                }
            }
        }
        let copied = copy_reached(from, &roots, &mut self.tys, self.alloc).map_err(|why| HiddenFail::Copy { owner, why })?;
        let mut hidden = Vec::with_capacity_in(done.hidden.len(), self.alloc);
        for (i, &(k, _)) in done.hidden.iter().enumerate() {
            hidden.push((k, copied[i]));
        }
        let mut coroutines = Vec::with_capacity_in(blocks.len(), self.alloc);
        for &(at, first, exact) in blocks.iter() {
            let interior = match exact {
                None => copied[first + 1],
                Some(parts) => self.exact_tuple(parts, &copied[first + 1..first + 1 + parts.len()]).map_err(|why| HiddenFail::Copy { owner, why })?,
            };
            coroutines.push((at, copied[first], interior));
        }
        self.rows.insert(owner, Row { hidden, coroutines });
        Ok(())
    }

    /// An exact interior as the tuple the type check's interior is (blaze-flow-lower
    /// coroutine.rs `interior`: its elements sorted and deduplicated), in the table's own types:
    /// `tys` are `parts`' types already copied, each wrapped in its reference here.
    fn exact_tuple(&mut self, parts: &[InteriorPart], tys: &[PTy]) -> Result<PTy, &'static str> {
        const FULL: &str = "the hidden types' table is full";
        let mut elems: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(parts.len(), self.alloc);
        for (p, &t) in parts.iter().zip(tys) {
            elems.push(match p.wrap {
                Some(m) => self.tys.intern(PNode::Ref(m, t)).map_err(|_| FULL)?,
                None => t,
            });
        }
        elems.sort_unstable();
        elems.dedup();
        let list = self.tys.intern_list(&elems).map_err(|_| FULL)?;
        self.tys.intern(PNode::Tuple(list)).map_err(|_| FULL)
    }
}

/// Why an opaque's hidden type could not be given.
#[derive(Clone, Debug)]
pub enum HiddenFail {
    /// No fn head of the program made this opaque (`Store::opaques` holds no row).
    NoHead { opaque: ItemKey },
    /// The defining body would be lowered with the build's prepared macros, and this caller
    /// has none (`Records::consts` is `None`).
    NoPrepares { opaque: ItemKey, owner: ItemKey, shown: Box<str> },
    /// The defining body did not check: its refusal, as printed.
    Defining { opaque: ItemKey, owner: ItemKey, shown: Box<str>, why: Box<str> },
    /// The defining body checked and its record names no hidden type for the opaque.
    NoneGiven { opaque: ItemKey, owner: ItemKey, shown: Box<str> },
    /// An auto trait of `opaque` asked while checking `asked_by`, which is its defining body or
    /// is checked to define an opaque the defining body is under on the work list: E0391's class.
    Cycle { opaque: ItemKey, asked_by: ItemKey, shown: Box<str> },
    /// A clean record's type could not be filed (a hole or an error type in it, an index past
    /// its table, a full table).
    Copy { owner: ItemKey, why: &'static str },
    /// The work list passed [`MOST_DEFINES`] checks.
    TooMany { opaque: ItemKey, most: u32 },
}

impl core::fmt::Display for HiddenFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HiddenFail::NoHead { opaque } => write!(f, "the `impl Trait` type {opaque:?}: no fn head of the program made it"),
            HiddenFail::NoPrepares { opaque, owner, shown } => {
                write!(f, "the hidden type of the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}): this caller has no prepared macros to lower its defining body with")
            }
            HiddenFail::Defining { opaque, owner, shown, why } => {
                write!(f, "the hidden type of the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}), for an auto trait: its defining body did not check: {why}")
            }
            HiddenFail::NoneGiven { opaque, owner, shown } => {
                write!(f, "the `impl Trait` type {opaque:?} of fn `{shown}` ({owner:?}): its defining body's record gives no hidden type for it")
            }
            HiddenFail::Cycle { opaque, asked_by, shown } => write!(
                f,
                "cycle (E0391): an auto trait of the `impl Trait` type {opaque:?} is asked while checking fn `{shown}` ({asked_by:?}), which is that type's defining body or is checked to define it (the Reference, \"Impl trait\", \"Auto trait leakage\")"
            ),
            HiddenFail::Copy { owner, why } => write!(f, "the hidden types of fn {owner:?}'s clean check could not be filed: {why}"),
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

/// Files `body`'s clean check `done` in `Store::hidden` when it defines an opaque. Called where
/// a clean check's read list is filed (front2 body_reads.rs), so every clean check is filed
/// once, the driver's and the work list's alike. Its async blocks' exact interiors are made
/// here, only for a body that is filed (rule 77: exact_interior.rs).
pub(crate) fn file<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, body: &Body<'_>, done: &Checked<'_>) -> Result<(), CheckFail> {
    let owner = body.typeck.owner;
    if !r.store.hidden.wants(owner, done) {
        return Ok(());
    }
    let exacts = exact_interiors(r, body, done, Alloc::global()).map_err(|error| CheckFail::Check { owner, error })?;
    r.store.hidden.file_row(owner, &body.typeck.exprs, done, exacts.as_ref()).map_err(|f| gap(owner, f))
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
    let Some(&(def, _)) = r.store.opaques.get(&opaque) else { return Err(gap(owner, HiddenFail::NoHead { opaque })) };
    if def == owner || r.store.hidden.defining.contains(&def) {
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
    got
}

fn run<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, stack: &mut Vec<ItemKey, Alloc<'static>>) -> Result<(), HiddenFail> {
    for _ in 0..MOST_DEFINES {
        let Some(&opaque) = stack.last() else { return Ok(()) };
        let Some(&(owner, _)) = r.store.opaques.get(&opaque) else { return Err(HiddenFail::NoHead { opaque }) };
        if r.store.hidden.filed(owner) {
            if !r.store.hidden.gives(owner, opaque) {
                return Err(HiddenFail::NoneGiven { opaque, owner, shown: shown(r, owner) });
            }
            stack.pop();
            continue;
        }
        // Every defining fn on the list: a body checked now that needs an auto trait of one of
        // their opaques is the cycle.
        r.store.hidden.defining.clear();
        for o in stack.iter() {
            if let Some(&(w, _)) = r.store.opaques.get(o) {
                r.store.hidden.defining.push(w);
            }
        }
        match check_defining(r, opaque, owner)? {
            // Filed by the check itself (body_reads.rs); a body that defines nothing files
            // nothing, which the top of the loop names.
            Ok(()) if r.store.hidden.filed(owner) => {}
            Ok(()) => return Err(HiddenFail::NoneGiven { opaque, owner, shown: shown(r, owner) }),
            Err(why) => match r.store.hidden.wanted.take() {
                Some(w) => stack.push(w),
                None => return Err(HiddenFail::Defining { opaque, owner, shown: shown(r, owner), why }),
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
    let lowered = match r.keys.library_in(r.names, owner) {
        Some(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
        None => lower_member(r.names, prepared, r.keys, r.store, owner),
    };
    let lowered = match lowered {
        Ok(l) => l,
        Err(e) => return Ok(Err(format!("its body was not lowered: {e}").into_boxed_str())),
    };
    let view = lowered.lowered.view();
    let spell = match Spelling::of(&view) {
        Ok(s) => s,
        Err(why) => return Ok(Err(format!("its body's names were not spelled: {why:?}").into_boxed_str())),
    };
    let scope = lowered.scope(&spell);
    Ok(match check_body(r, &lowered, &scope) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{e}").into_boxed_str()),
    })
}

/// `roots` of `from` interned into `out`, each reached node after its children (a table holds
/// a node after its children: `PortableTys::intern` takes their handles), by a work list (no
/// recursion). Every form is kept as it is (parameters stay the defining fn's own; a nested
/// future keeps its expression id, which is its owner's). A hole or an error type is refused:
/// a clean record holds none (`CheckError::UnknownInCleanBody`).
fn copy_reached<'a>(from: PTys<'_>, roots: &[PTy], out: &mut PortableTys<'a>, alloc: Alloc<'a>) -> Result<Vec<PTy, Alloc<'a>>, &'static str> {
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
        let (one, two, list) = children(node);
        for c in [one, two].into_iter().flatten() {
            work.push(c);
        }
        if let Some(l) = list {
            for &c in from.list(l).map_err(|_| PAST)? {
                work.push(c);
            }
        }
    }
    let mut mapped: Vec<Option<PTy>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        mapped.push(None);
    }
    let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
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
            | PNode::ExAuto(_)) => n,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty, &mapped)?, bits },
            PNode::Tuple(a) => PNode::Tuple(l(a, &mapped, out)?),
            PNode::Ref(mu, a) => PNode::Ref(mu, m(a, &mapped)?),
            PNode::Ptr(mu, a) => PNode::Ptr(mu, m(a, &mapped)?),
            PNode::Array(a, len) => PNode::Array(m(a, &mapped)?, m(len, &mapped)?),
            PNode::Slice(a) => PNode::Slice(m(a, &mapped)?),
            PNode::Pat(a, r) => PNode::Pat(m(a, &mapped)?, l(r, &mapped, out)?),
            PNode::Adt(k, a) => PNode::Adt(k, l(a, &mapped, out)?),
            PNode::FnDef(k, a) => PNode::FnDef(k, l(a, &mapped, out)?),
            PNode::FnPtr(a, h) => PNode::FnPtr(l(a, &mapped, out)?, h),
            PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: l(args, &mapped, out)? },
            PNode::Future(e, o, i2) => PNode::Future(e, m(o, &mapped)?, m(i2, &mapped)?),
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

/// A node's children: up to two single types and one list.
fn children(n: PNode) -> (Option<PTy>, Option<PTy>, Option<List>) {
    match n {
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::ExAuto(_)
        | PNode::Infer
        | PNode::Error => (None, None, None),
        PNode::ConstValue { ty, .. } => (Some(ty), None, None),
        PNode::Ref(_, a) | PNode::Ptr(_, a) | PNode::Slice(a) => (Some(a), None, None),
        PNode::Array(a, len) => (Some(a), Some(len), None),
        PNode::Future(_, o, i) => (Some(o), Some(i), None),
        PNode::Pat(a, r) => (Some(a), None, Some(r)),
        PNode::ExProj { args, term, .. } => (Some(term), None, Some(args)),
        PNode::Tuple(a)
        | PNode::Adt(_, a)
        | PNode::FnDef(_, a)
        | PNode::FnPtr(a, _)
        | PNode::Closure { args: a, .. }
        | PNode::Coroutine { args: a, .. }
        | PNode::Alias { args: a, .. }
        | PNode::Opaque(_, a)
        | PNode::Dynamic(a)
        | PNode::ExTrait(_, a) => (None, None, Some(a)),
    }
}
