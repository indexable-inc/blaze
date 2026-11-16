//! The new front end as the second implementation of the runner's seam ([`crate::seam`]):
//! the program's `main` found by the names library, lowered and checked by front2, and the
//! back half's questions answered from front2's store.
//!
//! What front2 cannot answer the back half yet is a [`Front2Refusal`] naming it, raised before
//! the back half runs, so no answer below is a guess: the list of variants after `Check` is
//! the work list for the switch (crates/blaze-cli/tests/ladder/README.md, "The seam").

use core::cell::RefCell;
use std::alloc::Global;
use std::vec::Vec;

use blaze_build::layout::ReprAttrs;
use blaze_build::{Abi, Intrinsic, StaticItem};
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{Namespace, PathKind};
use blaze_typeck as tc;
use blaze_typeck::{
    Adt, Checked, ExprId, FnSig, ItemKey, LangItem, List, PTy, PTys, PatId, PortableTys, ResolveError, resolve_instance, select_impl,
};

use super::Front2;
use super::body::{Body, BodyError, lower_again, lower_fn};
use super::buckets::Buckets;
use super::check::{BuildOrder, CheckFail, Records, check_body};
use super::consts::Consts;
use super::heads::{CrateCfg, FnHead, Store};
use super::keys::{MemberKind, checker_of};
use super::reach::{First, Reached, fn_shown, reach};
use super::spell::Spelling;
use super::types::Why;
use super::world_items::BodyWorld;
use crate::front::{At, FnKind, FnLang, HOSTED_START, LitTexts, int_value, string_bytes, unquote};
use blaze_hash::HashMap;
use blaze_lower::head;
use std::boxed::Box;
use std::format;
use std::string::String;
use crate::refusal::SourceFile;
use crate::seam::{FnFacts, FrontEnd};

/// What the new front end refuses of a program, by name: each a stage it stopped at or a
/// question of the back half it cannot answer yet.
#[derive(Debug)]
pub enum Front2Refusal {
    /// The front end over the program's names could not start.
    Front(super::FrontError),
    /// The crate root names no `main` in the value namespace.
    NoMain,
    /// `main`'s body did not lower.
    Lower(BodyError),
    /// `main`'s names could not be spelled back from its lowering.
    Spell(Why),
    /// `main` did not check clean: diagnostics, gaps or a typed failure.
    Check(CheckFail),
    /// A fn `main` reaches did not lower or check clean: never tabled, never built. `class`
    /// groups the cause; `first` is its first finding.
    Unchecked { function: Box<str>, class: Box<str>, first: Box<str> },
    /// A fn `main` reaches has no body and is no fn the back half calls by symbol.
    NoBody { function: Box<str> },
    /// The walk from `main` met more fns than [`super::reach::MOST_FNS`].
    Capped,
    /// A tabled fn has no head in front2's store.
    NoHead { function: Box<str> },
    /// A fn's attributes could not be read from its signature's lowering.
    Attrs { function: Box<str>, why: Box<str> },
    /// A fn's ABI is neither Rust nor C.
    Abi { function: Box<str>, abi: tc::Abi },
    /// A literal whose value front2 cannot read (an escape, a form), at its range in the fn.
    Literal { function: Box<str>, what: &'static str, range: tc::Range },
    /// An ADT the program made narrows its valid range (`rustc_layout_scalar_valid_range_*`),
    /// which the back half's `valid_range` does not read yet.
    Layout { adt: Box<str> },
    /// A `#[rustc_intrinsic]` fn the build's intrinsic table does not name.
    Intrinsic { function: Box<str> },
    /// Two fns of the program's crate are exported under the process entry's symbol.
    TwoEntries { symbol: &'static str, first: Box<str>, second: Box<str> },
    /// An ADT a type of the program names whose row the store could not make.
    AdtRow { adt: Box<str>, why: Box<str> },
    /// A trait a type of the program names as `dyn` (or a supertrait under one) whose table as
    /// a trait object sees it (supertraits, methods, dispatchability) could not be made.
    ObjectTrait { trait_: Box<str>, why: Box<str> },
    /// A lang item the back half asks for that core marks and front2 finds nothing under (or
    /// cannot read): the item and why.
    Lang { item: LangItem, why: &'static str },
}

impl core::fmt::Display for Front2Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Front2Refusal::Front(e) => write!(f, "front2 could not start: {e}"),
            Front2Refusal::NoMain => write!(f, "front2: the crate root has no `main`"),
            Front2Refusal::Lower(e) => write!(f, "front2: `main` did not lower: {e:?}"),
            Front2Refusal::Spell(e) => write!(f, "front2: `main`'s names: {e:?}"),
            Front2Refusal::Check(e) => write!(f, "front2: `main` did not check: {e:?}"),
            Front2Refusal::Unchecked { function, class, first } => {
                write!(f, "front2: `{function}`, reached from `main`, is not checked ({class}): {first}")
            }
            Front2Refusal::NoBody { function } => write!(f, "front2: `{function}`, reached from `main`, has no body (front2's)"),
            Front2Refusal::Capped => write!(f, "front2: `main` reaches more than {} fns", super::reach::MOST_FNS),
            Front2Refusal::NoHead { function } => write!(f, "front2: `{function}` has no head in the store (front2's)"),
            Front2Refusal::Attrs { function, why } => write!(f, "front2: `{function}`'s attributes: {why}"),
            Front2Refusal::Abi { function, abi } => write!(f, "front2: `{function}`'s ABI {abi:?} is neither Rust nor C"),
            Front2Refusal::Literal { function, what, range } => {
                write!(f, "front2: `{function}` at {}..{}: {what} (front2's)", range.start, range.end)
            }
            Front2Refusal::Layout { adt } => {
                write!(f, "front2: `{adt}` narrows its valid range and the back half reads none (run.rs `valid_range`)")
            }
            Front2Refusal::Intrinsic { function } => write!(f, "front2: `{function}` is an intrinsic the build's table does not name (blaze-build intrinsics)"),
            Front2Refusal::AdtRow { adt, why } => write!(f, "front2: the ADT `{adt}`'s row: {why}"),
            Front2Refusal::ObjectTrait { trait_, why } => write!(f, "front2: the trait `{trait_}` as a trait object sees it: {why}"),
            Front2Refusal::Lang { item, why } => write!(f, "front2: the lang item {item:?}: {why}"),
            Front2Refusal::TwoEntries { symbol, first, second } => {
                write!(f, "front2: `{first}) and `{second}) are both exported as `{symbol}`, the process's entry")
            }
        }
    }
}

/// One fn of the program's table: lowered, checked clean, its literals read.
struct Tabled<'n> {
    key: ItemKey,
    /// `None` for a foreign fn: called by `symbol`.
    body: Option<Body<'n>>,
    /// Taken by [`FrontEnd::check_all`].
    checked: Option<Checked<'static>>,
    kind: FnKind,
    path: Box<str>,
    at: At,
    abi: Abi,
    symbol: Box<str>,
    export: Option<Box<str>>,
    lits: Literals,
    /// A `#[rustc_intrinsic]` fn's entry in the build's intrinsic table: built by the build.
    intrinsic: Option<Intrinsic>,
}

/// A body's literals as the back half reads them: each value by its id, sorted, and their
/// texts (as the old front's `BodyRow`).
#[derive(Default)]
struct Literals {
    texts: LitTexts,
    ints: Vec<(ExprId, u128), Global>,
    int_pats: Vec<(PatId, u128), Global>,
    strings: Vec<(ExprId, Box<[u8], Global>), Global>,
}

/// The program as front2 made it for the back half: `main` at index 0, then every fn the walk
/// from it checked, in the order checked.
pub struct Front2Program<'n> {
    front: Front2<'n>,
    store: Store<'n>,
    /// The records' table, then every tabled body's own types: what the back half reads.
    tys: PortableTys<'static>,
    buckets: Option<Buckets>,
    fns: Vec<Tabled<'n>, Global>,
    by_key: HashMap<ItemKey, usize, Global>,
    /// `Some(0)`, or `None` in a survey whose `main` did not check (never tabled then).
    entry: Option<usize>,
    /// Each trait a type of the program names as a `dyn` principal, and every supertrait under
    /// it, as a trait object sees it ([`object_tables`]): what the seam's `object_trait` lends.
    objects: HashMap<ItemKey, ObjectTable, Global>,
    /// [`SEAM_LANGS`]' answers.
    langs: [(LangItem, Option<ItemKey>); 3],
}

/// The lang traits the seam's own `Option` answers name (`drop_trait`, `lang_trait_fn`):
/// each looked up once in [`Front2Program::make`], a refusal stopping the program by the item's
/// name there, so those answers never meet a lookup that fails.
const SEAM_LANGS: [LangItem; 3] = [LangItem::Drop, LangItem::Deref, LangItem::DerefMut];

fn lang_of(front: &Front2<'_>, item: LangItem) -> Result<Option<ItemKey>, crate::seam::LangRefusal> {
    match super::langs::find(&front.index, front.resolver, item) {
        Ok(found) => Ok(found.map(checker_of)),
        Err(why) => Err(crate::seam::LangRefusal { item, why }),
    }
}

impl Front2Program<'_> {
    /// A lang item of [`SEAM_LANGS`], as looked up at the start.
    fn made_lang(&self, item: LangItem) -> Option<ItemKey> {
        self.langs.iter().find(|(i, _)| *i == item).and_then(|(_, k)| *k)
    }
}

/// One trait as a trait object sees it (blaze-build's `vtable::ObjectTrait`, owned).
struct ObjectTable {
    supers: Vec<ItemKey, Global>,
    methods: Vec<blaze_build::vtable::ObjectMethod, Global>,
    shape: ContentId,
}

/// Whether a trait's method has a slot (the Reference, "Trait objects": a method callable on
/// the object): dyn-compatible by the checker's one rule ([`tc::dyn_compat::member_verdict`]),
/// and not one of the explicitly non-dispatchable (`where Self: Sized`, a `self` receiver).
fn dispatchable(m: &tc::dyn_compat::MemberFacts, f: &tc::dyn_compat::FnFacts) -> bool {
    !(f.where_self_sized || f.receiver == tc::dyn_compat::Receiver::Value) && tc::dyn_compat::member_verdict(m) == tc::dyn_compat::MemberVerdict::Compatible
}

/// The most traits one program's objects reach, supertraits included (rule 18).
const MOST_OBJECT_TRAITS: usize = 1024;

/// The table of every trait a type in `views` names as a `dyn` principal, and of every
/// supertrait under it (each after its supertraits, by a work list: its shape hashes theirs).
/// A trait whose table cannot be made is refused by name: in a survey filed in `left_out`
/// (a vtable of it is then the builder's `NoTrait`), in a run the program's refusal.
fn dyn_principals<'v>(views: impl Iterator<Item = PTys<'v>>) -> Vec<ItemKey, Global> {
    let mut asked: Vec<ItemKey, Global> = Vec::new_in(Global);
    for v in views {
        for n in v.nodes.iter() {
            if let tc::PNode::Dynamic(l) = *n
                && let Some(k) = tc::object_principal(v, l)
            {
                asked.push(k);
            }
        }
    }
    asked.sort_unstable();
    asked.dedup();
    // Reversed so the least key is made first (deterministic, rule 25).
    asked.reverse();
    asked
}

fn object_tables(
    r: &mut Records<'_, '_>,
    mut asked: Vec<ItemKey, Global>,
    mut left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>,
) -> Result<HashMap<ItemKey, ObjectTable, Global>, Front2Refusal> {
    let mut tables: HashMap<ItemKey, ObjectTable, Global> = HashMap::new_in(Global);
    let mut refused: HashMap<ItemKey, (), Global> = HashMap::new_in(Global);
    let mut steps = 0;
    while let Some(&k) = asked.last() {
        steps += 1;
        if tables.contains_key(&k) || refused.contains_key(&k) {
            asked.pop();
            continue;
        }
        let why: Result<Option<ObjectTable>, String> = (|| {
            if steps > 4 * MOST_OBJECT_TRAITS {
                return Err(String::from("more traits under its objects than the most one program's objects reach"));
            }
            let lib = r.keys.library(k).ok_or_else(|| String::from("no library key"))?;
            r.store.ensure(r.names, r.keys, super::heads::Want::Head(lib)).map_err(|e| format!("its head: {e:?}"))?;
            r.store.ensure(r.names, r.keys, super::heads::Want::Members(lib)).map_err(|e| format!("its members: {e:?}"))?;
            let head = r.store.maps.traits.get(&lib).ok_or_else(|| String::from("made, but no head filed under its key"))?;
            let supers_written = r.store.maps.supers.get(&lib).ok_or_else(|| String::from("made, but no supertraits filed under its key"))?;
            let mut supers = Vec::with_capacity_in(supers_written.len(), Global);
            let mut waiting = false;
            for &(s, args) in supers_written.iter() {
                // `Self` is the first argument; one past it is a generic supertrait.
                if args.len > 1 {
                    return Err(format!("its supertrait {} has generic arguments, which a table does not carry yet", fn_shown(r, checker_of(s))));
                }
                let sk = checker_of(s);
                if refused.contains_key(&sk) {
                    return Err(format!("its supertrait {} has no table", fn_shown(r, sk)));
                }
                if !tables.contains_key(&sk) {
                    asked.push(sk);
                    waiting = true;
                }
                supers.push(sk);
            }
            if waiting {
                return Ok(None);
            }
            let mut methods = Vec::with_capacity_in(head.members.len(), Global);
            for m in head.members.iter() {
                let dispatch = match &m.shape {
                    tc::dyn_compat::MemberShape::Fn(f) => dispatchable(m, f),
                    tc::dyn_compat::MemberShape::Unread(u) => return Err(format!("a member the front end could not read ({u:?})")),
                    _ => continue,
                };
                let item = head.fns.iter().find(|(n, _)| *n == m.name).map(|(_, f)| *f).ok_or_else(|| String::from("a method with no fn filed under its name"))?;
                methods.push(blaze_build::vtable::ObjectMethod { item, dispatch });
            }
            let mut h = blaze_ids::Hasher::new("blaze-ladder front2 object trait 1");
            h.update(&k.0.to_le_bytes());
            for m in methods.iter() {
                h.update(&m.item.0.to_le_bytes()).update(&[u8::from(m.dispatch)]);
            }
            for s in supers.iter() {
                let below = tables.get(s).map_or([0; 32], |t| t.shape.0);
                h.update(&s.0.to_le_bytes()).update(&below);
            }
            Ok(Some(ObjectTable { supers, methods, shape: h.finish() }))
        })();
        match why {
            Ok(None) => {}
            Ok(Some(t)) => {
                asked.pop();
                tables.insert(k, t);
            }
            Err(why) => {
                asked.retain(|a| *a != k);
                refused.insert(k, ());
                let (trait_, why) = (fn_shown(r, k).into_boxed_str(), why.into_boxed_str());
                match left_out.as_deref_mut() {
                    Some(out) => out.push((trait_, format!("object trait: {why}").into_boxed_str())),
                    None => return Err(Front2Refusal::ObjectTrait { trait_, why }),
                }
            }
        }
    }
    Ok(tables)
}

impl<'n> Front2Program<'n> {
    /// `main` of the program's crate (index 0 of `names`), lowered and checked; refused by name
    /// where the back half would ask what front2 cannot answer yet.
    pub fn new(names: &'n Names<'n>, user: &[SourceFile<'_>]) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, user, None)
    }

    /// The program with every fn that checks clean tabled and every other one LEFT OUT (each
    /// named in `left_out` with its cause), for a survey of what the back half builds
    /// ([`crate::run::build_survey`]); never run: a left-out fn's caller cannot be built.
    pub fn survey(names: &'n Names<'n>, user: &[SourceFile<'_>], left_out: &mut Vec<(Box<str>, Box<str>), Global>) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, user, Some(left_out))
    }

    fn make(names: &'n Names<'n>, user: &[SourceFile<'_>], mut left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>) -> Result<Front2Program<'n>, Front2Refusal> {
        let mut front = Front2::new(names, user).map_err(Front2Refusal::Front)?;
        let written = Written { kind: PathKind::Plain, segments: &["main"] };
        let found = names::path(names, names.root(0), Namespace::Values, written, Alloc::global()).map_err(|_| Front2Refusal::NoMain)?;
        let rust_main: LibKey = found.whole(1).ok_or(Front2Refusal::NoMain)?.key;
        // The walk and `entry()` start at the process's entry (the rule: [`exported_entry`]).
        let main_lib = exported_entry(names)?.unwrap_or(rust_main);
        let mut store = Store::new(front.index.lang("sized"));
        let mut buckets = None;
        let prepared = Prepares::new_in(Alloc::global());
        let main = lower_fn(names, prepared, &mut front.keys, &mut store, main_lib).map_err(Front2Refusal::Lower)?;
        let view = main.lowered.view();
        let spell = Spelling::of(&view).map_err(Front2Refusal::Spell)?;
        let scope = main.scope(&spell);
        let mut consts = Consts::default();
        let mut records = Records {
            names,
            keys: &mut front.keys,
            index: &front.index,
            store: &mut store,
            buckets: &mut buckets,
            order: BuildOrder::AsAsked,
            consts: Some((&mut consts, prepared)),
        };
        // In a survey a `main` that does not check is filed and left out, and the walk goes on
        // from what it calls by name: what is reachable is surveyed whatever `main` is.
        let mut checked = match check_body(&mut records, &main, &scope) {
            Ok(c) => Some(c),
            Err(e) => match left_out.as_deref_mut() {
                Some(out) => {
                    let d = format!("{e:?}");
                    let class = format!("check: {}", &d[..d.find(['(', ' ', '{']).unwrap_or(d.len())]);
                    out.push((fn_shown(&records, main.owner).into_boxed_str(), class.into_boxed_str()));
                    None
                }
                None => return Err(Front2Refusal::Check(e)),
            },
        };
        let first = match &mut checked {
            Some(c) => First::Checked(c),
            None => First::Unchecked(&main),
        };
        let census = reach(&mut records, prepared, main.owner, first, &scope);
        if census.capped {
            return Err(Front2Refusal::Capped);
        }
        // Every fn the walk met is checked clean, or the program is refused by its first one
        // that is not: a fn with any diagnostic or gap is never built.
        let mut bodiless: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        let mut ctors: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        let mut declared: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        for (k, outcome) in census.fns.iter() {
            match outcome {
                Reached::Failed { class, .. } if let Some(out) = left_out.as_deref_mut() => {
                    out.push((fn_shown(&records, *k).into_boxed_str(), class.clone()));
                }
                Reached::Failed { class, detail } => {
                    return Err(Front2Refusal::Unchecked { function: fn_shown(&records, *k).into_boxed_str(), class: class.clone(), first: detail.clone() });
                }
                Reached::NoBody => bodiless.push((*k, fn_shown(&records, *k).into_boxed_str())),
                // A ctor is the language's: tabled as one, the build makes its value.
                Reached::Ctor => ctors.push((*k, fn_shown(&records, *k).into_boxed_str())),
                // A call on a parameter or `dyn` is resolved by the back half at the instance,
                // to an impl fn the walk tabled (reach.rs `Instances`); the trait's fn itself is
                // tabled as declared: the build reads its ABI at the call before resolving it.
                Reached::AtInstance | Reached::AtObject => declared.push((*k, fn_shown(&records, *k).into_boxed_str())),
                Reached::Checked => {}
            }
        }
        // A trait's fn called on a concrete type (`?`'s `Try::branch` on a `Result`, a path
        // call `Tr::f(&x)`) is named by the call as the trait's: the back half reads its ABI at
        // the call, then resolves it to the impl's fn the walk reached. Tabled as declared.
        let mut met: HashMap<ItemKey, (), Global> = HashMap::new_in(Global);
        met.extend(census.fns.iter().map(|(k, _)| (*k, ())));
        met.insert(main.owner, ());
        for c in checked.iter().chain(census.bodies.iter().map(|(_, c)| c)) {
            for &item in c.callees.column::<1>() {
                let of_trait = matches!(records.keys.origin(item), Some(super::keys::Origin::Member { container, kind: MemberKind::Fn, .. }) if !records.store.impls.contains_key(container));
                if of_trait && met.insert(item, ()).is_none() {
                    declared.push((item, fn_shown(&records, item).into_boxed_str()));
                }
            }
        }
        // Every ADT a type of the program names has its row before the build: the back half
        // lays out each type it meets (an impl's signature's `ControlFlow<..>` too, which no
        // body's check needed the row of). Rounds, as a row's fields name more ADTs.
        // In a survey a row that cannot be made is filed in `left_out` and given up on: the
        // builder then refuses each type naming it, by name.
        let mut given_up: HashMap<ItemKey, (), Global> = HashMap::new_in(Global);
        for round in 0.. {
            let mut missing: Vec<ItemKey, Global> = Vec::new_in(Global);
            let views = core::iter::once(records.store.tys.view()).chain(checked.iter().map(|c| c.types.view())).chain(census.bodies.iter().map(|(_, c)| c.types.view()));
            for v in views {
                for n in v.nodes.iter() {
                    if let tc::PNode::Adt(k, _) = n
                        && !records.store.adts.contains_key(k)
                        && !given_up.contains_key(k)
                    {
                        missing.push(*k);
                    }
                }
            }
            missing.sort_unstable();
            missing.dedup();
            let Some(&first) = missing.first() else { break };
            let refuse = |r: &Records<'_, '_>, k: ItemKey, why: String| Front2Refusal::AdtRow { adt: fn_shown(r, k).into_boxed_str(), why: why.into_boxed_str() };
            if round == super::reach::MOST_FNS {
                return Err(refuse(&records, first, String::from("rows still missing after the most rounds")));
            }
            for k in missing {
                let why = match records.keys.library(k) {
                    None => Some(String::from("no library key")),
                    Some(lib) => match records.store.ensure(names, records.keys, super::heads::Want::Adt(lib)) {
                        Err(e) => Some(format!("{e:?}")),
                        Ok(()) if !records.store.adts.contains_key(&k) => Some(String::from("made, but no row filed under its key")),
                        Ok(()) => None,
                    },
                };
                let Some(why) = why else { continue };
                let (adt, why) = (fn_shown(&records, k).into_boxed_str(), why.into_boxed_str());
                match left_out.as_deref_mut() {
                    Some(out) => {
                        let cut = why.char_indices().nth(160).map_or(&*why, |(i, _)| &why[..i]);
                        out.push((adt, format!("ADT row: {cut}").into_boxed_str()));
                        given_up.insert(k, ());
                    }
                    None => return Err(Front2Refusal::AdtRow { adt, why }),
                }
            }
        }
        let views = core::iter::once(records.store.tys.view()).chain(checked.iter().map(|c| c.types.view())).chain(census.bodies.iter().map(|(_, c)| c.types.view()));
        let asked = dyn_principals(views);
        let objects = object_tables(&mut records, asked, left_out.as_deref_mut())?;
        let mut paths: Vec<Box<str>, Global> = Vec::with_capacity_in(census.bodies.len() + 1, Global);
        if checked.is_some() {
            paths.push(fn_shown(&records, main.owner).into_boxed_str());
        }
        paths.extend(census.bodies.iter().map(|(b, _)| fn_shown(&records, b.owner).into_boxed_str()));
        drop(scope);
        drop(spell);
        drop(view);
        // A narrowed valid range is not read by the back half yet: refused, never a wrong niche.
        let narrowed = records.store.adts.iter().filter(|(_, a)| a.narrowed).map(|(k, _)| *k).min();
        if let Some(k) = narrowed {
            return Err(Front2Refusal::Layout { adt: fn_shown(&records, k).into_boxed_str() });
        }
        // One table for the back half, which reads every body and record from one: the
        // records' table, then each body lowered again into it in the order tabled. (A checked
        // body's own table is a copy of the records' at its lowering, so two bodies' own types
        // would share indices; the records' table never holds a body's type.)
        let mut tys = store.tys.fork_in(Alloc::global()).map_err(|error| Front2Refusal::Lower(BodyError::Table { item: main.item, error }))?;
        let main = match checked {
            Some(c) => Some((lower_again(names, prepared, &mut front.keys, &mut store, &main, &mut tys).map_err(Front2Refusal::Lower)?, c)),
            None => None,
        };
        let entry = main.as_ref().map(|_| 0);
        let mut bodies = Vec::with_capacity_in(census.bodies.len(), Global);
        for (b, c) in census.bodies {
            bodies.push((lower_again(names, prepared, &mut front.keys, &mut store, &b, &mut tys).map_err(Front2Refusal::Lower)?, c));
        }
        let mut fns = Vec::with_capacity_in(paths.len() + bodiless.len(), Global);
        let mut by_key = HashMap::new_in(Global);
        let all = main.into_iter().chain(bodies);
        let bodied = all.zip(paths).map(|((body, checked), path)| (body.owner, Some((body, checked)), path, false));
        let bodiless = bodiless.into_iter().map(|(k, path)| (k, None, path, false));
        let declared = declared.into_iter().map(|(k, path)| (k, None, path, true));
        for (key, body, path, of_trait) in bodied.chain(bodiless).chain(declared) {
            let Some(head) = store.fns.get(&key) else {
                // A survey files it and goes on (a call of it is then a refused callee).
                match left_out.as_deref_mut() {
                    Some(out) => {
                        out.push((path, Box::from("no head in the store (front2's)")));
                        continue;
                    }
                    None => return Err(Front2Refusal::NoHead { function: path }),
                }
            };
            let abi = match head.header.abi {
                tc::Abi::Rust => Abi::Rust,
                tc::Abi::C => Abi::C,
                tc::Abi::RustCall => Abi::RustCall,
                // A survey files it and goes on (a call of it is then a refused callee); a run
                // stops: the builder spreads no "rust-call" tuple yet.
                abi => match left_out.as_deref_mut() {
                    Some(out) => {
                        out.push((path, format!("ABI {abi:?}: neither Rust nor C").into_boxed_str()));
                        continue;
                    }
                    None => return Err(Front2Refusal::Abi { function: path, abi }),
                },
            };
            let symbols = symbols_of(names, head.lib, head.text, &path)?;
            let (at, foreign) = placed(names, head.lib);
            let (body, checked, kind, symbol, lits) = match body {
                Some((body, checked)) => {
                    let lits = literals(&body, &path)?;
                    (Some(body), Some(checked), FnKind::Free, Box::<str>::from(""), lits)
                }
                // A declaration in an `extern` block is called by its symbol: its
                // `#[link_name]`, else its name.
                None if foreign.is_some() && head.container.is_none() => {
                    let symbol = symbols.link_name.unwrap_or_else(|| path.rsplit("::").next().unwrap_or(&path).into());
                    (None, None, FnKind::Foreign, symbol, Literals::default())
                }
                // A trait's fn called on a parameter or `dyn`: never built (each instance
                // resolves to an impl's fn); its container index is the old front's and front2
                // numbers no traits, so it reads 0, which nothing asks.
                None if of_trait => (None, None, FnKind::Trait(0), Box::<str>::from(""), Literals::default()),
                // An intrinsic: the build makes each call from its table, by the fn's name.
                None if symbols.intrinsic => (None, None, FnKind::Free, Box::<str>::from(""), Literals::default()),
                None => match left_out.as_deref_mut() {
                    Some(out) => {
                        out.push((path, Box::from("no body (front2's)")));
                        continue;
                    }
                    None => return Err(Front2Refusal::NoBody { function: path }),
                },
            };
            let intrinsic = match (symbols.intrinsic, &body) {
                (true, None) => {
                    let name = path.rsplit("::").next().unwrap_or(&path);
                    match (blaze_build::intrinsics::Which::named(name), left_out.as_deref_mut()) {
                        (Ok(which), _) => Some(Intrinsic::Table(which)),
                        (Err(_), Some(out)) => {
                            out.push((path, Box::from("an intrinsic the build's table does not name")));
                            continue;
                        }
                        (Err(_), None) => return Err(Front2Refusal::Intrinsic { function: path }),
                    }
                }
                _ => None,
            };
            // A foreign fn's own text carries no ABI: its block's does.
            let abi = match foreign {
                None => abi,
                Some("C" | "C-unwind") => Abi::C,
                Some("Rust") => Abi::Rust,
                Some("rust-call") => Abi::RustCall,
                Some(_) => match left_out.as_deref_mut() {
                    Some(out) => {
                        out.push((path, format!("ABI {:?}: neither Rust nor C", head.header.abi).into_boxed_str()));
                        continue;
                    }
                    None => return Err(Front2Refusal::Abi { function: path, abi: head.header.abi }),
                },
            };
            by_key.insert(key, fns.len());
            fns.push(Tabled { key, body, checked, kind, path, at, abi, symbol, export: symbols.export, lits, intrinsic });
        }
        for (key, path) in ctors {
            if !store.ctors.contains_key(&key) {
                return Err(Front2Refusal::NoHead { function: path });
            }
            by_key.insert(key, fns.len());
            // A variant's place; a struct's ctor is a member key, placed with its struct (owed).
            let at = front.keys.library(key).map_or(At { file: 0, place: 0 }, |lib| placed(names, lib).0);
            let none = Box::<str>::from("");
            fns.push(Tabled { key, body: None, checked: None, kind: FnKind::Ctor, path, at, abi: Abi::Rust, symbol: none, export: None, lits: Literals::default(), intrinsic: None });
        }
        let mut langs = [(LangItem::Drop, None); 3];
        for (slot, item) in langs.iter_mut().zip(SEAM_LANGS) {
            *slot = (item, lang_of(&front, item).map_err(|l| Front2Refusal::Lang { item: l.item, why: l.why })?);
        }
        Ok(Front2Program { front, store, tys, buckets, fns, by_key, entry, objects, langs })
    }

    /// `f` over the checker's world of fn `i`'s body.
    fn in_world<T>(&self, i: usize, f: impl FnOnce(&BodyWorld<'_, 'n>) -> T) -> T {
        // The back half resolves and selects only in a body it builds: a foreign fn has none.
        let body = self.fns[i].body.as_ref().expect("the back half asks only in a fn it builds, and every one has its body");
        let view = body.lowered.view();
        // A function of the lowering alone, made once when the body was checked.
        let spell = Spelling::of(&view).expect("a tabled body's spelling was made when it was checked");
        let scope = body.scope(&spell);
        let world = BodyWorld {
            names: self.front.names,
            keys: &self.front.keys,
            index: &self.front.index,
            store: &self.store,
            buckets: self.buckets.as_ref(),
            module: scope.module,
            params: scope.params,
            self_: scope.self_,
            spell: scope.spell,
            traits: scope.traits,
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
        };
        f(&world)
    }
}

/// THE ENTRY RULE (one place): the process starts at the free fn of the program's crate
/// (crate 0) exported as [`HOSTED_START`] (bstd's hosted entry, given the host's table), else
/// the one exported as `main` (the C entry the platform calls), else at the crate root's
/// `fn main` (`None` here). Exported means `#[no_mangle]` or `#[export_name = ".."]`, read from
/// each fn's attribute rows through its signature's lowering ([`symbols_of`]), the fns listed by
/// the names library ([`Names::items`]); two fns under one symbol is [`Front2Refusal::TwoEntries`].
/// The answer is a function of the program: candidates are ordered by key, never by the map.
fn exported_entry(names: &Names<'_>) -> Result<Option<LibKey>, Front2Refusal> {
    use blaze_surface::ItemDetail;
    let mut found: Vec<(u8, LibKey, Box<str>), Global> = Vec::new_in(Global);
    for (key, _module, place) in names.items(0) {
        let Some((_, outline)) = names.outline_at(place.krate, place.at) else { continue };
        let item = outline.item(place.item);
        if !matches!(item.detail, ItemDetail::Fn { .. }) {
            continue;
        }
        // A free fn: at a file's top or in an inline module, never a member or a foreign fn.
        if item.parent.is_some_and(|p| !matches!(outline.item(p).detail, ItemDetail::Mod { .. })) {
            continue;
        }
        let Some(it) = names.item_text(key) else { continue };
        let at = placed(names, key).0;
        let shown = format!("{}` (file {}, byte {}", it.name.unwrap_or("_"), at.file, at.place);
        let export = symbols_of(names, key, it.text, &shown)?.export;
        let rank = match export.as_deref() {
            Some(HOSTED_START) => 0,
            Some("main") => 1,
            _ => continue,
        };
        found.push((rank, key, shown.into_boxed_str()));
    }
    found.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    match found.as_slice() {
        [] => Ok(None),
        [(r, _, first), (s, _, second), ..] if r == s => Err(Front2Refusal::TwoEntries {
            symbol: if *r == 0 { HOSTED_START } else { "main" },
            first: first.clone(),
            second: second.clone(),
        }),
        [(_, key, _), ..] => Ok(Some(*key)),
    }
}

/// The symbols of a fn, from its signature's attribute rows (as the old front's `read_attrs`).
struct Symbols {
    /// `#[export_name = ".."]`, or `#[no_mangle]` (its own name).
    export: Option<Box<str>>,
    /// A foreign fn's `#[link_name = ".."]`.
    link_name: Option<Box<str>>,
    /// `#[rustc_intrinsic]`: the build makes its body from the intrinsic table.
    intrinsic: bool,
}

fn symbols_of(names: &Names<'_>, lib: LibKey, text: &str, path: &str) -> Result<Symbols, Front2Refusal> {
    let refuse = |why: String| Front2Refusal::Attrs { function: path.into(), why: why.into_boxed_str() };
    let krate = names.crate_of(lib).ok_or_else(|| refuse(format!("no crate holds {:x}", lib)))?;
    let atoms = names.krate(krate).cfg.atoms();
    let sig = blaze_lower::lower_signature(text, names.edition(krate), &mut CrateCfg(atoms), Alloc::global())
        .map_err(|_| refuse(String::from("its signature needs a `cfg` the crate does not answer")))?;
    let v = sig.view();
    let mut out = Symbols { export: None, link_name: None, intrinsic: false };
    for &n in v.rows(v.list(head::ATTRS)).map_err(|e| refuse(format!("{e:?}")))? {
        let a = v.attr(n).map_err(|e| refuse(format!("{e:?}")))?;
        let value = || v.text_at(a.args).map(|t| Box::<str>::from(unquote(t))).map_err(|e| refuse(format!("{e:?}")));
        match v.text_at(a.path).map_err(|e| refuse(format!("{e:?}")))?.trim() {
            "no_mangle" if out.export.is_none() => out.export = Some(path.rsplit("::").next().unwrap_or(path).into()),
            "export_name" => out.export = Some(value()?),
            "link_name" => out.link_name = Some(value()?),
            "rustc_intrinsic" => out.intrinsic = true,
            _ => {}
        }
    }
    Ok(out)
}

/// Where `lib` is written (its file and first byte, as the old front's [`At`]), and, for an
/// entry of an `extern` block, the block's ABI (Rust: `extern { .. }` is `"C"`): read from the
/// names library's outline of its text.
fn placed<'n>(names: &Names<'n>, lib: LibKey) -> (At, Option<&'n str>) {
    use blaze_surface::ItemDetail;
    let Some(p) = names.place(lib) else { return (At { file: 0, place: 0 }, None) };
    let file = match p.at {
        blaze_scope::resolve::At::File(n) => n,
        // An expansion's text has no file of its own: its call's file is owed.
        blaze_scope::resolve::At::Expansion(_) => 0,
    };
    let Some((text, outline)) = names.outline_at(p.krate, p.at) else { return (At { file, place: 0 }, None) };
    let item = outline.item(p.item);
    let foreign = item.parent.and_then(|q| {
        let block = outline.item(q);
        let ItemDetail::ExternBlock { abi } = block.detail else { return None };
        let Some(r) = abi else { return Some("C") };
        let written = text.get((block.range.start + r.start) as usize..(block.range.start + r.end) as usize)?;
        Some(unquote(written.trim().strip_prefix("extern").unwrap_or(written)))
    });
    (At { file, place: item.range.start }, foreign)
}

/// Every literal's text and value of `body`, read through its lowering's view (the checker's
/// ids are the view's), as the old front's `bodies` reads them.
fn literals(body: &Body<'_>, path: &str) -> Result<Literals, Front2Refusal> {
    let view = body.lowered.view();
    let t = &body.typeck;
    let mut out = Literals::default();
    for (e, x) in t.exprs.iter().enumerate() {
        let tc::Expr::Lit(lit) = x else { continue };
        let id = ExprId(e as u32);
        let range = t.expr_ranges[e];
        let refuse = |what| Front2Refusal::Literal { function: path.into(), what, range };
        let text = view.expr_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or_else(|| refuse("a literal whose text the lowering does not hold"))?;
        let start = out.texts.text.len() as u32;
        out.texts.text.push_str(text);
        out.texts.exprs.push(id);
        out.texts.expr_ranges.push(tc::Range { start, end: out.texts.text.len() as u32 });
        match lit {
            tc::Lit::Str | tc::Lit::ByteStr(_) => {
                let bytes = string_bytes(text).ok_or_else(|| refuse("this escape in a string literal"))?;
                out.strings.push((id, bytes.into_boxed_slice()));
            }
            tc::Lit::Int(_) | tc::Lit::Char => out.ints.push((id, int_value(text).ok_or_else(|| refuse("this integer or char literal"))?)),
            _ => {}
        }
    }
    for (p, x) in t.pats.iter().enumerate() {
        let (tc::Pat::Lit(lit) | tc::Pat::NegLit(lit)) = x else { continue };
        let id = PatId(p as u32);
        let range = t.pat_ranges[p];
        let refuse = |what| Front2Refusal::Literal { function: path.into(), what, range };
        let text = view.pat_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or_else(|| refuse("a literal pattern whose text the lowering does not hold"))?;
        let start = out.texts.text.len() as u32;
        out.texts.text.push_str(text);
        out.texts.pats.push(id);
        out.texts.pat_ranges.push(tc::Range { start, end: out.texts.text.len() as u32 });
        if let tc::Lit::Int(_) | tc::Lit::Char = lit {
            out.int_pats.push((id, int_value(text).ok_or_else(|| refuse("this literal pattern"))?));
        }
    }
    Ok(out)
}

impl FrontEnd for Front2Program<'_> {
    fn fn_count(&self) -> usize {
        self.fns.len()
    }

    /// A fn with its body is `Free` (the back half tells apart only ctors, foreign fns, consts
    /// and statics; a member's container index is the old front's and asked by nothing), one
    /// without is `Foreign`.
    fn function(&self, i: usize) -> FnFacts<'_> {
        let f = &self.fns[i];
        FnFacts {
            key: f.key,
            path: &f.path,
            at: f.at,
            text: self.store.fns.get(&f.key).map_or("", |h| h.text),
            kind: f.kind,
            abi: f.abi,
            symbol: &f.symbol,
            export: f.export.as_deref(),
            body: f.body.as_ref().map(|b| (&b.typeck, &f.lits.texts)),
            const_fn: false,
            condition: false,
        }
    }

    fn entry(&self) -> Option<usize> {
        self.entry
    }

    fn types(&self) -> PTys<'_> {
        self.tys.view()
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        let r = self.in_world(i, |w| resolve_instance(w, item, args, ground, Alloc::global()))?;
        Ok((r.item, r.args))
    }

    fn selects(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        Ok(self.in_world(i, |w| select_impl(w, trait_, &[ty], ground, Alloc::global()))?.is_some())
    }

    /// Every tabled fn's head is in the store (`new` refuses one without).
    fn sig(&self, i: usize) -> FnSig<'_> {
        let key = self.fns[i].key;
        // A ctor: a safe Rust fn from its fields to its type, generic over the type's params.
        if let Some(c) = self.store.ctors.get(&key) {
            let header = tc::FnPtrHeader { abi: tc::Abi::Rust, safety: tc::Safety::Safe };
            let (inputs, output, types) = (&c.inputs[..], c.output, self.tys.view());
            return FnSig { parent_count: 0, own_count: c.generics, declared_count: c.generics, self_param: false, header, inputs, output, types };
        }
        let f = &self.store.fns[&key];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            declared_count: f.declared,
            self_param: f.self_param,
            header: f.header,
            inputs: &f.inputs,
            output: f.output,
            types: self.tys.view(),
        }
    }

    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.by_key.get(&key).copied()
    }

    /// Each body was checked once, in `new` (rule 77); this hands the results over.
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, crate::run::RunError> {
        let mut out = Vec::with_capacity_in(self.fns.len(), Global);
        out.extend(self.fns.iter_mut().map(|f| f.checked.take()));
        Ok(out)
    }

    fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        self.front.index.lang(l.lang()).map(checker_of)
    }

    /// The `drop` lang trait and its `drop` fn (filed once the trait's head was made).
    fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)> {
        let tr = self.made_lang(LangItem::Drop)?;
        Some((tr, self.front.keys.filed_member(tr, MemberKind::Fn, "drop")))
    }

    /// The library item for `item`, by the one home the checker's world asks too
    /// ([`super::langs::find`]: the `#[lang]` table, or the item's path in core for those the
    /// library marks otherwise, `Display`, `Debug`, `Send`). A core that holds no answer for a
    /// marked item is refused by the item's name.
    fn lang_item(&self, item: LangItem) -> Result<Option<ItemKey>, crate::seam::LangRefusal> {
        lang_of(&self.front, item)
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let o = self.objects.get(&t)?;
        Some(blaze_build::vtable::ObjectTrait { supers: &o.supers, methods: &o.methods, shape: o.shape })
    }

    /// The fn `name` of the lang trait `trait_` (`Deref::deref`), filed with its trait's head.
    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        self.front.keys.filed_member(self.made_lang(trait_)?, MemberKind::Fn, name)
    }

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let a = self.store.adts.get(&key)?;
        Some(Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.tys.view() })
    }

    /// The item's path in the names library (`crate::module::Name`).
    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
        let (names, lib) = (self.front.names, self.front.keys.library(key)?);
        let name = names.item_text(lib)?.name?;
        let module = names.krate(names.crate_of(lib)?).path_of(names.module_of(lib)?)?;
        Some(format!("{module}::{name}").into_boxed_str())
    }

    fn int_lit(&self, owner: ItemKey, e: ExprId) -> Option<u128> {
        let s = &self.fns[self.fn_index(owner)?].lits.ints;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn int_pat(&self, owner: ItemKey, p: PatId) -> Option<u128> {
        let s = &self.fns[self.fn_index(owner)?].lits.int_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn string(&self, owner: ItemKey, e: ExprId) -> Option<&[u8]> {
        let s = &self.fns[self.fn_index(owner)?].lits.strings;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| &*s[i].1)
    }

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.store.adts.get(&key).map_or(ReprAttrs::RUST, |a| a.repr)
    }

    /// A `#[rustc_intrinsic]` fn's table entry, read from its name when it was tabled.
    fn intrinsic(&self, key: ItemKey) -> Option<Intrinsic> {
        self.fns.get(*self.by_key.get(&key)?)?.intrinsic
    }

    fn static_item(&self, _: ItemKey) -> Option<StaticItem> {
        None
    }

    /// A tuple struct's or variant's ctor: its ADT and the variant's index there.
    /// From the ADT's row: every variant's value, explicit or implicit (heads.rs `discrs`).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Option<i128> {
        self.store.adts.get(&adt)?.discrs.get(variant as usize).copied()
    }

    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        let adt = self.store.ctors.get(&key)?.adt;
        let variant = self.store.adts.get(&adt)?.variants.iter().position(|v| v.key == key)?;
        Some((adt, variant as u32))
    }

    /// The item's key and text, as the old front's `code_of`.
    fn code_of(&self, i: usize) -> ContentId {
        let owner = self.fns[i].key;
        let text = self.store.fns.get(&owner).map_or("", |h| h.text);
        blaze_ids::Hasher::new("blaze-ladder front2 code 1").part(&owner.0.to_le_bytes()).part(text.as_bytes()).finish()
    }
}
