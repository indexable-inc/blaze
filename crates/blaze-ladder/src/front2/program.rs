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

use blaze_build::data::DataRecord;
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
use super::reach::{First, Reached, Start, fn_shown, reach_all};
use super::test_entry::{EntryShape, TestEntryRefusal};
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

mod float_text;
pub mod suite;
mod tabled;
pub mod walk_report;

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
    /// Constant item `constant`, read by a checked body, was refused by front2's evaluator.
    Const { constant: Box<str>, why: Box<str> },
    /// Static item `static_`, named by a checked body or pointed at by another static's
    /// value, was not made: front2's evaluator refused its initializer, or its head or
    /// attributes did not read (statics.rs).
    Static { static_: Box<str>, why: Box<str> },
    /// The walk from `entry` (the program's start, named) met more fns than
    /// [`super::reach::MOST_FNS`]; `queued` were still waiting when it stopped.
    Capped { entry: Box<str>, queued: usize },
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
    /// A fn or item of the program's OWN crate that `main` reaches was left out by the survey
    /// front ([`Front2Program::own_left_out`]): the program's own finding, so a run stops
    /// before it starts (a library's left-out fn only stops a program that calls it).
    OwnLeftOut { item: Box<str>, why: Box<str> },
    /// A Rust program with std starts through the library's `#[lang = "start"]` (start.rs),
    /// and no crate declares one: `shelf` names the crates looked in, each with its root file.
    NoLangStart { shelf: Box<str> },
    /// The Rust start refuses `item` (the root's `main`, or two `#[lang = "start"]` items):
    /// `what` says which rule (start.rs [`super::start::rust_start`]).
    Start { item: Box<str>, what: &'static str },
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
            Front2Refusal::Const { constant, why } => write!(f, "front2: the constant `{constant}`, read by a body of the program, was not evaluated: {why}"),
            Front2Refusal::Static { static_, why } => write!(f, "front2: the static `{static_}`, reached by the program, was not made: {why}"),
            Front2Refusal::Capped { entry, queued } => write!(
                f,
                "front2: `{entry}` reaches more than {} fns (the cap above the whole library's fn count; {queued} still queued): a walk that does not end",
                super::reach::MOST_FNS
            ),
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
            Front2Refusal::OwnLeftOut { item, why } => write!(f, "front2: `{item}`, of the program's own crate and reached from `main`, is not checked: {why}"),
            Front2Refusal::TwoEntries { symbol, first, second } => {
                write!(f, "front2: `{first}) and `{second}) are both exported as `{symbol}`, the process's entry")
            }
            Front2Refusal::NoLangStart { shelf } => write!(f, "the shelf has no `#[lang = \"start\"]` item: {shelf}"),
            Front2Refusal::Start { item, what } => write!(f, "front2: the Rust start: `{item}` {what}"),
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
    /// Each float literal: its text rounded to the nearest `f64` and `f32` ([`float_bits`]).
    floats: Vec<(ExprId, (u64, u32)), Global>,
    /// Each float literal pattern's magnitude (the `-` of a `NegLit` dropped), as `floats`.
    float_pats: Vec<(PatId, (u64, u32)), Global>,
    /// Each string literal pattern's text, unescaped.
    str_pats: Vec<(PatId, Box<str, Global>), Global>,
    /// Each `format_args!` template the builder reads (fmt_template.rs), by expression, sorted.
    templates: Vec<(ExprId, super::fmt_template::Template), Global>,
}

/// A float literal's text (`1.5`, `2e-3_f32`, `1_000.0`) rounded once to the nearest `f64`
/// and once to the nearest `f32` (the Reference, "Floating-point literal expressions"): the
/// underscores and a suffix dropped, then core's correctly rounded parse; `None` for a text
/// the parse refuses.
fn float_bits(text: &str) -> Option<(u64, u32)> {
    let t = text.strip_suffix("f32").or_else(|| text.strip_suffix("f64")).unwrap_or(text);
    let mut digits = blaze_own::String::new_in(Alloc::global());
    for ch in t.chars().filter(|&ch| ch != '_') {
        digits.push(ch);
    }
    let wide: f64 = digits.parse().ok()?;
    let narrow: f32 = digits.parse().ok()?;
    Some((wide.to_bits(), narrow.to_bits()))
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
    langs: [(LangItem, Option<ItemKey>); SEAM_LANGS.len()],
    /// In a survey, each fn or ADT left out of the program, by key, with the cause filed in
    /// `left_out` (the seam's `left_out`); empty in a run.
    left: HashMap<ItemKey, Box<str>, Global>,
    /// Each constant a checked body reads, by key (sorted), as front2's evaluator made it
    /// ([`const_values`]): the seam's `const_record`.
    const_values: Vec<(ItemKey, DataRecord<Global>), Global>,
    /// Every static the program reaches, each evaluated once ([`super::statics`]): the
    /// seam's `static_item` and `static_records`.
    statics: super::statics::Statics,
    /// The cause of the last `normalize` with no answer (the seam's `last_projection`).
    projection_why: core::cell::RefCell<Option<Box<str>>>,
    /// The walk's steps ([`super::reach::Census::edges`]), sorted, once each: a root's static
    /// reach is read from them (program/suite.rs `Suite::reach_of`).
    edges: Vec<(ItemKey, ItemKey), Global>,
    /// front2's constant evaluator, KEPT after the program is made (its checked constant
    /// bodies, its records, its unit's tables) with the build's prepared macros: the seam's
    /// `const_at` evaluates a constant or a const block at an instance's ground arguments
    /// with it, checking a body or making an impl row it has not made yet (const_at.rs).
    consts: Consts<'n>,
    prepared: &'n Prepares<'n>,
    /// Where the process starts (start.rs); `None` for a test unit's suite.
    start: Option<super::start::Entry2>,
}

/// Where a program's walk starts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Starts<'a> {
    /// The process's entry: `main`, or the fn exported as the entry ([`exported_entry`]).
    Main,
    /// Every test fn of a test unit, by its tests record's row order (program/suite.rs).
    Tests(&'a [LibKey]),
}

/// What [`Front2Program::make_from`] made beside the program.
struct Made<'n> {
    program: Front2Program<'n>,
    /// Each root in the order given: its checker key and its row of the table (`None` for one
    /// left out in a survey, or one that did not check).
    roots: Vec<(ItemKey, Option<usize>), Global>,
    /// A suite's roots' entries, in the same order (test_entry.rs); empty for `main`.
    shapes: Vec<Result<EntryShape, TestEntryRefusal>, Global>,
}

/// The lang traits the seam's own `Option` answers name (`drop_trait`, `lang_trait_fn`):
/// each looked up once in [`Front2Program::make`], a refusal stopping the program by the item's
/// name there, so those answers never meet a lookup that fails.
const SEAM_LANGS: [LangItem; 8] = [LangItem::Drop, LangItem::Deref, LangItem::DerefMut, LangItem::Fn, LangItem::FnMut, LangItem::FnOnce, LangItem::Index, LangItem::IndexMut];

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

/// The constants one ADT head may name before its row is given up on (reach.rs's
/// `MOST_LENS`): a pattern type's ends, an array length.
const MOST_ADT_LENS: u32 = 64;

/// Every static `bodies` name, evaluated once ([`super::statics::static_values`]); one
/// refused is named: in a survey filed in `left_out` and left out (each reader's build then
/// names it: the builder's `NoStatic`), in a run the program's refusal.
fn made_statics<'c>(
    r: &mut Records<'_, '_>,
    bodies: impl Iterator<Item = &'c Checked<'static>>,
    mut left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>,
    left: &mut HashMap<ItemKey, Box<str>, Global>,
) -> Result<super::statics::Statics, Front2Refusal> {
    let (statics, refused) = super::statics::static_values(r, bodies);
    for (k, why) in refused {
        let static_ = fn_shown(r, k).into_boxed_str();
        match left_out.as_deref_mut() {
            Some(out) => {
                let class = format!("static: {why}").into_boxed_str();
                left.insert(k, class.clone());
                out.push((static_, class));
            }
            None => return Err(Front2Refusal::Static { static_, why }),
        }
    }
    Ok(statics)
}

/// Each constant item `bodies` read by value (`Def::Const` in a checked body's `expr_defs`:
/// what the builder's `Adts::const_value` asks), evaluated once by front2's evaluator
/// (consts.rs `Consts::value`, the one that fills array lengths), in key order. Returns the
/// records, sorted by key, and each constant the evaluator refused with its words (a nested
/// evaluation included: `ConstFail` names the constant it stopped on). A path the check
/// recorded generic arguments for (`Checked::const_args`: a trait's constant, an inherent
/// impl's over its generics) is not asked here: it has a value only at an instance's ground
/// arguments, pulled by the build before it walks that instance (const_at.rs, the seam's
/// `const_at`). Its body is checked then, by this same evaluator, which the program keeps.
fn const_values<'c>(r: &mut Records<'_, '_>, bodies: impl Iterator<Item = &'c Checked<'static>>) -> (Vec<(ItemKey, DataRecord<Global>), Global>, Vec<(ItemKey, Box<str>), Global>) {
    let mut asked: Vec<ItemKey, Global> = Vec::new_in(Global);
    for c in bodies {
        let at_args = c.const_args.column::<0>();
        asked.extend(c.expr_defs.column::<0>().iter().zip(c.expr_defs.column::<1>().iter()).filter_map(|(e, d)| match d {
            tc::Def::Const(k) if at_args.binary_search(e).is_err() => Some(*k),
            _ => None,
        }));
    }
    asked.sort_unstable();
    asked.dedup();
    let mut values = Vec::with_capacity_in(asked.len(), Global);
    let mut refused = Vec::new_in(Global);
    for k in asked {
        let Some((consts, prepared)) = r.consts.take() else {
            refused.push((k, Box::from("no evaluator: the program's records hold none")));
            continue;
        };
        let made = consts.value(r, prepared, k).map(super::const_at::data_record);
        r.consts = Some((consts, prepared));
        match made {
            Ok(record) => values.push((k, record)),
            Err(e) => {
                let said = format!("{e}");
                let cut = said.char_indices().nth(200).map_or(&*said, |(i, _)| &said[..i]);
                refused.push((k, Box::from(cut)));
            }
        }
    }
    (values, refused)
}

/// The table of every trait a type in `views` names as a `dyn` principal, and of every
/// supertrait under it (each after its supertraits, by a work list: its shape hashes theirs).
/// A trait whose table cannot be made is refused by name: in a survey filed in `left_out`
/// (a vtable of it is then the builder's `NoTrait`), in a run the program's refusal.
fn dyn_principals<'v>(views: impl Iterator<Item = PTys<'v>>) -> Vec<ItemKey, Global> {
    let mut asked: Vec<ItemKey, Global> = Vec::new_in(Global);
    for v in views {
        for n in v.nodes() {
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
    /// `frame` is the caller's, alive for the command (`Front2::new`).
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>]) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, frame, user, None)
    }

    /// Where the process starts ([`super::start::Entry2`]): what the builder makes the C entry
    /// from (LADDER.md L1b); `None` for a test unit's suite.
    pub fn start(&self) -> Option<super::start::Entry2> {
        self.start
    }

    /// The program with every fn that checks clean tabled and every other one LEFT OUT (each
    /// named in `left_out` with its cause), for a survey of what the back half builds
    /// ([`crate::run::build_survey`]); never run: a left-out fn's caller cannot be built.
    pub fn survey(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], left_out: &mut Vec<(Box<str>, Box<str>), Global>) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, frame, user, Some(left_out))
    }

    /// Of a program made by [`Front2Program::survey`]: the left-out fn or item of the
    /// program's OWN crate (index 0 of the names) with the smallest key, named, and its cause;
    /// `None` when every left-out key is a library's. A run refuses such a program before it
    /// starts ([`crate::run::run_program`]): a finding in the program's own text is its
    /// writer's to fix, while a library fn blaze cannot check yet stops only the program that
    /// calls it. A member's crate is its container's (an impl's or a trait's).
    pub fn own_left_out(&self) -> Option<(Box<str>, Box<str>)> {
        use super::keys::Origin;
        let names = self.front.names;
        let mut found: Option<(ItemKey, &str)> = None;
        for (&key, why) in self.left.iter() {
            // A member's container is an item: two steps at most; a longer chain is no item's.
            let mut at = key;
            let mut lib = None;
            for _ in 0..4 {
                match self.front.keys.origin(at) {
                    Some(Origin::Item(k)) => {
                        lib = Some(*k);
                        break;
                    }
                    Some(Origin::Member { container, .. }) => at = *container,
                    None => break,
                }
            }
            let own = lib.and_then(|lib| names.crate_of(lib)) == Some(0);
            if own && found.is_none_or(|(least, _)| key < least) {
                found = Some((key, &**why));
            }
        }
        let (key, why) = found?;
        let shown = match self.front.keys.origin(key) {
            Some(Origin::Member { container, name, .. }) => match self.item_named(*container) {
                Some(of) => format!("{of}::{name}").into_boxed_str(),
                None => Box::from(&**name),
            },
            _ => self.item_named(key).unwrap_or_else(|| format!("{:#x}", key.0).into_boxed_str()),
        };
        Some((shown, Box::from(why)))
    }

    fn make(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make_from(names, frame, user, left_out, Starts::Main).map(|made| made.program)
    }

    /// The program from its ROOTS ([`Starts`]): `main` (the process's entry), or every test fn
    /// of a test unit (program/suite.rs). One walk from all roots ([`super::reach::reach_all`]),
    /// so a fn two roots reach is checked and tabled once; each root keeps its row.
    fn make_from(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], mut left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>, starts: Starts<'_>) -> Result<Made<'n>, Front2Refusal> {
        let mut front = Front2::new(names, frame, user).map_err(Front2Refusal::Front)?;
        let mut roots: Vec<LibKey, Global> = Vec::new_in(Global);
        // Made before the roots: a Rust program's start reads `main`'s head (start.rs).
        let mut store = Store::new(front.index.lang("sized"), front.frame);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        let mut start2: Option<super::start::Entry2> = None;
        let tests = match starts {
            Starts::Main => {
                let written = Written { kind: PathKind::Plain, segments: &["main"] };
                let found = names::path(names, names.root(0), Namespace::Values, written, Alloc::global()).map_err(|_| Front2Refusal::NoMain)?;
                let rust_main: LibKey = found.whole(1).ok_or(Front2Refusal::NoMain)?.key;
                // The process's entry (the rule: [`exported_entry`], then start.rs): an exported
                // entry, else a Rust program with std through `#[lang = "start"]`, else the
                // root's `fn main` called directly. The walk and `entry()` start at its root.
                let entry = match exported_entry(names)? {
                    Some(e) => e,
                    None if super::start::through_lang_start(names) => {
                        super::start::Entry2::Rust(super::start::rust_start(names, rust_main, &mut store, &mut front.keys)?)
                    }
                    None => super::start::Entry2::Direct(rust_main),
                };
                roots.push(entry.root());
                start2 = Some(entry);
                false
            }
            Starts::Tests(list) => {
                roots.extend_from_slice(list);
                true
            }
        };
        let mut buckets = None;
        let prepared = Prepares::new_in(Alloc::global());
        let mut mains: Vec<Body<'n>, Global> = Vec::with_capacity_in(roots.len(), Global);
        for &lib in roots.iter() {
            mains.push(lower_fn(names, prepared, &mut front.keys, &mut store, lib).map_err(Front2Refusal::Lower)?);
        }
        let mut lowered = Vec::with_capacity_in(mains.len(), Global);
        for m in mains.iter() {
            lowered.push(m.lowered.view());
        }
        let mut spells = Vec::with_capacity_in(mains.len(), Global);
        for v in lowered.iter() {
            spells.push(Spelling::of(v).map_err(Front2Refusal::Spell)?);
        }
        let mut scopes = Vec::with_capacity_in(mains.len(), Global);
        for (m, s) in mains.iter().zip(spells.iter()) {
            scopes.push(m.scope(s));
        }
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
        // In a survey a root that does not check is filed and left out, and the walk goes on
        // from what it calls by name: what is reachable is surveyed whatever the root is.
        let mut checked: Vec<Option<Checked<'static>>, Global> = Vec::with_capacity_in(mains.len(), Global);
        for (m, scope) in mains.iter().zip(scopes.iter()) {
            checked.push(match check_body(&mut records, m, scope) {
                Ok(c) => Some(c),
                Err(e) => match left_out.as_deref_mut() {
                    Some(out) => {
                        let d = format!("{e:?}");
                        let class = format!("check: {}", &d[..d.find(['(', ' ', '{']).unwrap_or(d.len())]);
                        out.push((fn_shown(&records, m.owner).into_boxed_str(), class.into_boxed_str()));
                        None
                    }
                    None => return Err(Front2Refusal::Check(e)),
                },
            });
        }
        // A test's entry, from its head alone (test_entry.rs): a refusal is that test's own (the
        // runner fails it by name; the rest of the suite goes on). A `Report` entry's `report`
        // fn is a root of the walk, so it is tabled and in the test's reach.
        let mut shapes: Vec<Result<EntryShape, TestEntryRefusal>, Global> = Vec::new_in(Global);
        let mut also: Vec<ItemKey, Global> = Vec::new_in(Global);
        if tests {
            for (m, &lib) in mains.iter().zip(roots.iter()) {
                let shape = super::test_entry::entry_shape(&mut records, lib, m.owner);
                if let Ok(EntryShape::Report { report, .. }) = shape {
                    also.push(report);
                }
                shapes.push(shape);
            }
            also.sort_unstable();
            also.dedup();
        }
        // A Rust program's start: `lang_start` is reached beside `main` like any fn it calls;
        // its instance at `T` is the builder's (LADDER.md L1b).
        if let Some(super::start::Entry2::Rust(s)) = start2 {
            let k = records.keys.item(s.lang_start).map_err(|clash| Front2Refusal::Unchecked {
                function: Box::from("lang_start"),
                class: Box::from("key clash"),
                first: format!("{clash}").into_boxed_str(),
            })?;
            also.push(k);
        }
        let mut walk_from = Vec::with_capacity_in(mains.len(), Global);
        for ((m, c), scope) in mains.iter().zip(checked.iter_mut()).zip(scopes.iter()) {
            let first = match c {
                Some(c) => First::CheckedBody(m, c),
                None => First::Unchecked(m),
            };
            walk_from.push(Start { owner: m.owner, first, scope });
        }
        let census = reach_all(&mut records, prepared, walk_from, &also);
        if census.capped {
            let entry = mains.first().map_or_else(|| String::from("the suite's tests"), |m| fn_shown(&records, m.owner));
            return Err(Front2Refusal::Capped { entry: entry.into_boxed_str(), queued: census.queued });
        }
        // Every fn the walk met is checked clean, or the program is refused by its first one
        // that is not: a fn with any diagnostic or gap is never built.
        let mut bodiless: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        let mut ctors: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        let mut declared: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        // Every key a survey leaves out, with its cause: the seam's `left_out`.
        let mut left: HashMap<ItemKey, Box<str>, Global> = HashMap::new_in(Global);
        for (k, outcome) in census.fns.iter() {
            match outcome {
                Reached::Failed { class, detail } if let Some(out) = left_out.as_deref_mut() => {
                    out.push((fn_shown(&records, *k).into_boxed_str(), class.clone()));
                    // The cause a caller's build names ([`RunError::LeftOut`]'s `why`): the class
                    // and its WHOLE detail, on one line (rule 19: an error names the thing; a cut
                    // at 160 characters hid the failing goal of Class O, 2026-11-16).
                    let mut why = String::with_capacity(class.len() + 2 + detail.len());
                    why.push_str(class);
                    why.push_str(": ");
                    why.extend(detail.chars().map(|c| if c == '\n' { ' ' } else { c }));
                    left.insert(*k, why.into_boxed_str());
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
        for m in mains.iter() {
            met.insert(m.owner, ());
        }
        for c in checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)) {
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
            let views = core::iter::once(records.store.tys.view()).chain(checked.iter().flatten().map(|c| c.types.view())).chain(census.bodies.iter().map(|(_, c)| c.types.view()));
            for v in views {
                for n in v.nodes() {
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
                    // A constant a head names (a pattern type's range end, `UsizeNoHighBit`'s
                    // `HALF_USIZE`) is evaluated as met and the head made again, at most
                    // `MOST_ADT_LENS` times, as a body's lowering does.
                    Some(lib) => {
                        let mut lens = 0;
                        loop {
                            match records.store.ensure(names, records.keys, super::heads::Want::Adt(lib)) {
                                Err(super::heads::HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_ADT_LENS => {
                                    lens += 1;
                                    if let Err(e) = super::check::eval_len(&mut records, c) {
                                        break Some(format!("the constant {c:x} its head names: {e:?}"));
                                    }
                                }
                                Err(e) => break Some(format!("{e:?}")),
                                Ok(()) if !records.store.adts.contains_key(&k) => break Some(String::from("made, but no row filed under its key")),
                                Ok(()) => break None,
                            }
                        }
                    }
                };
                let Some(why) = why else { continue };
                let (adt, why) = (fn_shown(&records, k).into_boxed_str(), why.into_boxed_str());
                match left_out.as_deref_mut() {
                    Some(out) => {
                        let cut = why.char_indices().nth(160).map_or(&*why, |(i, _)| &why[..i]);
                        let class = format!("ADT row: {cut}").into_boxed_str();
                        left.insert(k, class.clone());
                        out.push((adt, class));
                        given_up.insert(k, ());
                    }
                    None => return Err(Front2Refusal::AdtRow { adt, why }),
                }
            }
        }
        // Every constant a checked body reads by value, evaluated once now (the builder's
        // `const_value` reads the record); one the evaluator refuses is named: in a survey filed
        // and left out (each reader's build then names it), in a run the program's refusal.
        let (values, refused) = const_values(&mut records, checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)));
        for (k, why) in refused {
            let constant = fn_shown(&records, k).into_boxed_str();
            match left_out.as_deref_mut() {
                Some(out) => {
                    let class = format!("constant: {why}").into_boxed_str();
                    left.insert(k, class.clone());
                    out.push((constant, class));
                }
                None => return Err(Front2Refusal::Const { constant, why }),
            }
        }
        let statics = made_statics(&mut records, checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)), left_out.as_deref_mut(), &mut left)?;
        let views = core::iter::once(records.store.tys.view()).chain(checked.iter().flatten().map(|c| c.types.view())).chain(census.bodies.iter().map(|(_, c)| c.types.view()));
        let asked = dyn_principals(views);
        let objects = object_tables(&mut records, asked, left_out.as_deref_mut())?;
        let mut paths: Vec<Box<str>, Global> = Vec::with_capacity_in(census.bodies.len() + mains.len(), Global);
        for (m, c) in mains.iter().zip(checked.iter()) {
            if c.is_some() {
                paths.push(fn_shown(&records, m.owner).into_boxed_str());
            }
        }
        paths.extend(census.bodies.iter().map(|(b, _)| fn_shown(&records, b.owner).into_boxed_str()));
        drop(scopes);
        drop(spells);
        drop(lowered);
        // A narrowed valid range is not read by the back half yet: refused, never a wrong niche.
        let narrowed = records.store.adts.iter().filter(|(_, a)| a.narrowed).map(|(k, _)| *k).min();
        if let Some(k) = narrowed {
            return Err(Front2Refusal::Layout { adt: fn_shown(&records, k).into_boxed_str() });
        }
        // One table for the back half, which reads every body and record from one: the
        // records' table, then each body lowered again into it in the order tabled. (A checked
        // body's own table is a copy of the records' at its lowering, so two bodies' own types
        // would share indices; the records' table never holds a body's type.)
        // A table error names the first root (the crate's root module when a suite has none).
        let table_item = roots.first().copied().unwrap_or_else(|| names.root(0));
        let mut tys = store.tys.fork_in(Alloc::global()).map_err(|error| Front2Refusal::Lower(BodyError::Table { item: table_item, error }))?;
        let mut main = Vec::with_capacity_in(mains.len(), Global);
        for (m, c) in mains.iter().zip(checked) {
            if let Some(c) = c {
                main.push((lower_again(names, prepared, &mut front.keys, &mut store, m, &mut tys).map_err(Front2Refusal::Lower)?, c));
            }
        }
        // `main`'s program enters at row 0; a suite's rows are entered by the launcher, each by
        // its test's row (program/suite.rs), so it has no one entry.
        let entry = if tests { None } else { main.first().map(|_| 0) };
        let mut bodies = Vec::with_capacity_in(census.bodies.len(), Global);
        for (b, c) in census.bodies {
            bodies.push((lower_again(names, prepared, &mut front.keys, &mut store, &b, &mut tys).map_err(Front2Refusal::Lower)?, c));
        }
        // The walk's steps, sorted and once each: what a test's reach is read from.
        let mut edges = census.edges;
        edges.sort_unstable();
        edges.dedup();
        let mut fns = Vec::with_capacity_in(paths.len() + bodiless.len(), Global);
        let mut by_key = HashMap::new_in(Global);
        let all = main.into_iter().chain(bodies);
        let bodied = all.zip(paths).map(|((body, checked), path)| (body.owner, Some((body, checked)), path, false));
        let bodiless = bodiless.into_iter().map(|(k, path)| (k, None, path, false));
        let declared = declared.into_iter().map(|(k, path)| (k, None, path, true));
        // Each fn's row is made with a verdict (program/tabled.rs): nothing that goes wrong
        // for ONE fn can return from here except through `left_out_or_stop`, the one place
        // that says what a fn's refusal means: a survey files it and goes on (a call of the
        // fn is then a refused callee), a run stops with it.
        let mut entry = entry;
        let mut first = true;
        for (key, body, path, of_trait) in bodied.chain(bodiless).chain(declared) {
            let of_main = core::mem::replace(&mut first, false) && entry.is_some();
            match tabled::table_one(names, &store, key, body, path, of_trait) {
                tabled::Verdict::Tabled(row) => {
                    by_key.insert(key, fns.len());
                    fns.push(row);
                }
                tabled::Verdict::LeftOut { path, refusal } => {
                    tabled::left_out_or_stop(key, path, refusal, &mut left, left_out.as_deref_mut())?;
                    // A survey whose `main` is left out has no entry (index 0 is another fn).
                    if of_main {
                        entry = None;
                    }
                }
            }
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
        let mut langs = [(LangItem::Drop, None); SEAM_LANGS.len()];
        for (slot, item) in langs.iter_mut().zip(SEAM_LANGS) {
            *slot = (item, lang_of(&front, item).map_err(|l| Front2Refusal::Lang { item: l.item, why: l.why })?);
        }
        // Each root's row: `None` for one left out (a survey) or that did not check.
        let mut rows: Vec<(ItemKey, Option<usize>), Global> = Vec::with_capacity_in(mains.len(), Global);
        rows.extend(mains.iter().map(|m| (m.owner, by_key.get(&m.owner).copied())));
        // The evaluator is kept, not dropped: the build pulls constants at instances' ground
        // arguments from it (`FrontEnd::const_at`, const_at.rs).
        let program = Front2Program { front, store, tys, buckets, fns, by_key, entry, objects, langs, left, const_values: values, statics, projection_why: core::cell::RefCell::new(None), edges, consts, prepared, start: start2 };
        Ok(Made { program, roots: rows, shapes })
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
            owner: scope.owner,
            module: scope.module,
            params: scope.params,
            self_: scope.self_,
            spell: scope.spell,
            traits: scope.traits,
            blocks: scope.blocks,
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
fn exported_entry(names: &Names<'_>) -> Result<Option<super::start::Entry2>, Front2Refusal> {
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
        // `symbols_of` reads a `#[no_mangle]` fn's export from the NAME it is given: the bare
        // name here, never `shown` (which appends the place for the TwoEntries message).
        let export = symbols_of(names, key, it.text, it.name.unwrap_or("_"))?.export;
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
        [(0, key, _), ..] => Ok(Some(super::start::Entry2::Hosted(*key))),
        [(_, key, _), ..] => Ok(Some(super::start::Entry2::CMain(*key))),
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
pub(super) fn placed<'n>(names: &Names<'n>, lib: LibKey) -> (At, Option<&'n str>) {
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
    let mut out = Literals { templates: super::fmt_template::templates(body), ..Literals::default() };
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
            // `c".."` / `cr".."`: the bytes without the nul the builder adds (its `byte_str`).
            tc::Lit::CStr => {
                let bytes = text.strip_prefix('c').and_then(string_bytes).ok_or_else(|| refuse("this escape in a C string literal"))?;
                out.strings.push((id, bytes.into_boxed_slice()));
            }
            tc::Lit::Int(_) | tc::Lit::Char => out.ints.push((id, int_value(text).ok_or_else(|| refuse("this integer or char literal"))?)),
            tc::Lit::Float(_) => match float_text::float_text(text) {
                float_text::FloatText::Bits { wide, narrow } => out.floats.push((id, (wide, narrow))),
                // An `f16` or `f128` literal: the seam carries no value for it and the builder
                // refuses its type by name before asking one (walk/numbers.rs `float_repr`).
                float_text::FloatText::NoSeamForm(_) => {}
                float_text::FloatText::Unread => return Err(refuse("this float literal")),
            },
            tc::Lit::Bool(_) => {}
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
        match lit {
            tc::Lit::Int(_) | tc::Lit::Char => {
                out.int_pats.push((id, int_value(text).ok_or_else(|| refuse("this literal pattern"))?));
            }
            // The magnitude: a `NegLit`'s text may hold its `-`; the build flips the sign.
            tc::Lit::Float(_) => {
                let magnitude = text.trim().trim_start_matches('-').trim();
                match float_text::float_text(magnitude) {
                    float_text::FloatText::Bits { wide, narrow } => out.float_pats.push((id, (wide, narrow))),
                    // As a literal expression's: the builder refuses the pattern's type by name.
                    float_text::FloatText::NoSeamForm(_) => {}
                    float_text::FloatText::Unread => return Err(refuse("this float literal pattern")),
                }
            }
            tc::Lit::Str => {
                let bytes = string_bytes(text).ok_or_else(|| refuse("this escape in a string literal pattern"))?;
                let text = core::str::from_utf8(&bytes).map_err(|_| refuse("a string literal pattern that is not UTF-8"))?;
                out.str_pats.push((id, Box::from(text)));
            }
            // The builder asks no other literal pattern's value (a byte string's, a `bool`'s).
            tc::Lit::ByteStr(_) | tc::Lit::CStr | tc::Lit::Bool(_) => {}
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

    fn c_variadic(&self, key: ItemKey) -> Option<u32> {
        let head = self.store.fns.get(&key)?;
        head.c_variadic.then_some(head.inputs.len() as u32)
    }

    fn entry(&self) -> Option<usize> {
        self.entry
    }

    fn types(&self) -> PTys<'_> {
        self.tys.view()
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        // A call of core's `#[lang = "panic_impl"]` declaration runs the program's one
        // `#[panic_handler]` fn, which takes no generic arguments (front2/panic_entry.rs).
        if let Some(handler) = super::panic_entry::bound(self.front.names, &self.front.index, &self.store, item, Alloc::global()) {
            return Ok((handler, List::EMPTY));
        }
        // A call of one of alloc's `extern "Rust"` allocator declarations (`__rust_alloc` ..)
        // runs its default, std's `__rdl_*` fn, which takes no generic arguments
        // (front2/alloc_shim.rs).
        if let Some(default) = super::alloc_shim::bound(self.front.names, &self.front.index, &self.store, item, Alloc::global()) {
            return Ok((default, List::EMPTY));
        }
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
            return FnSig { parent_count: 0, own_count: c.generics, declared_count: c.generics, self_param: false, header, c_variadic: false, inputs, output, types };
        }
        let f = &self.store.fns[&key];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            declared_count: f.declared,
            self_param: f.self_param,
            header: f.header,
            c_variadic: false,
            inputs: &f.inputs,
            output: f.output,
            types: self.tys.view(),
        }
    }

    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.by_key.get(&key).copied()
    }

    /// The digest filed in the store when fn `i`'s body was checked: its `Checked` holds no
    /// list (front2 body_reads.rs).
    fn body_reads(&self, i: usize, _checked: &Checked<'static>) -> Option<super::body_reads::ReadsDigest> {
        self.store.reads.get(self.fns[i].key)
    }

    /// Each body was checked once, in `new` (rule 77); this hands the results over.
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, crate::run::RunError> {
        let mut out = Vec::with_capacity_in(self.fns.len(), Global);
        out.extend(self.fns.iter_mut().map(|f| f.checked.take()));
        Ok(out)
    }

    fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        self.front.index.fn_lang(l).map(checker_of)
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

    /// By the library's `#[lang]` table, as [`super::langs::find`] reads a marked item.
    fn lang_named(&self, name: &str) -> Option<ItemKey> {
        self.front.index.lang(name).map(checker_of)
    }

    /// From the head store's `opaques` ([`super::heads::Store::opaques`]).
    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.store.opaques.get(&opaque).copied()
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

    fn left_out(&self, key: ItemKey) -> Option<&str> {
        self.left.get(&key).map(|why| &**why)
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

    fn format_template(&self, owner: ItemKey, e: ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> {
        let s = &self.fns[self.fn_index(owner)?].lits.templates;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1.pieces())
    }

    /// Core's `#[lang = "format_argument"] pub struct Argument<'a>` (library/core/src/fmt/rt.rs).
    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        self.front.index.lang("format_argument").map(checker_of)
    }

    /// The constructor by name (blaze-build walk/fmt.rs `argument_new_name`) in rt.rs's one
    /// inherent `impl Argument<'_>`, found among the impls of `Argument`'s own module whose
    /// header is inherent on it, as its member key once the impl's head was made. Two such
    /// impls, or none made, is no answer (the builder refuses naming the constructor).
    fn fmt_argument_new(&self, trait_: tc::FmtTrait) -> Option<ItemKey> {
        let name = super::fmt_template::argument_new_name(trait_);
        let adt = self.front.index.lang("format_argument")?;
        let module = self.front.names.module_of(adt)?;
        let mut found = self
            .front
            .index
            .impls
            .iter()
            .filter(|i| i.module == module && super::fmt_template::inherent_on(i.header, "Argument"))
            .filter_map(|i| self.front.keys.filed_member(checker_of(i.key), MemberKind::Fn, name));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    fn float_lit(&self, owner: ItemKey, e: ExprId) -> Option<(u64, u32)> {
        let s = &self.fns[self.fn_index(owner)?].lits.floats;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn float_pat(&self, owner: ItemKey, p: PatId) -> Option<(u64, u32)> {
        let s = &self.fns[self.fn_index(owner)?].lits.float_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&str> {
        let s = &self.fns[self.fn_index(owner)?].lits.str_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| &*s[i].1)
    }

    /// In the first tabled body's world: selection and the projection step read the impls and
    /// the traits, never a body's own scope, so any body's world answers alike.
    fn normalize(&self, trait_: ItemKey, name: tc::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<Option<PTy>, ResolveError> {
        let Some(i) = self.fns.iter().position(|f| f.body.is_some()) else {
            *self.projection_why.borrow_mut() = Some(Box::from("the program tables no body to ask in"));
            return Ok(None);
        };
        let out = self.in_world(i, |w| blaze_typeck::normalize_projection(w, trait_, name, args, ground, Alloc::global()));
        if !matches!(out, Ok(Some(_))) {
            // Named for the refusal: the trait, `Self`'s form (an ADT by its path, else the
            // node's kind: `FnDef`, `Closure`, `Slice`..), and the solver's error or none.
            let trait_shown = self.item_named(trait_).unwrap_or_else(|| format!("{:#x}", trait_.0).into_boxed_str());
            let self_shown = match args.first().map(|&a| ground.view().node(a)) {
                Some(Ok(tc::PNode::Adt(k, _))) => self.item_named(k).map_or_else(|| format!("ADT {:#x}", k.0), |p| format!("`{p}`")),
                Some(Ok(n)) => {
                    let d = format!("{n:?}");
                    String::from(&d[..d.find(['(', ' ', '{']).unwrap_or(d.len())])
                }
                Some(Err(_)) | None => String::from("(no Self)"),
            };
            let why = match &out {
                Err(e) => {
                    let d = format!("{e:?}");
                    String::from(&d[..d.find(['(', ' ', '{']).unwrap_or(d.len())])
                }
                Ok(_) => String::from("the solver's step gave no type"),
            };
            *self.projection_why.borrow_mut() = Some(format!("`{trait_shown}`'s associated type {:#x} at Self = {self_shown}: {why}", name.0).into_boxed_str());
        }
        out
    }

    fn const_record(&self, item: ItemKey) -> Option<&DataRecord<Global>> {
        let at = self.const_values.binary_search_by_key(&item, |(k, _)| *k).ok()?;
        Some(&self.const_values[at].1)
    }

    /// `need` at ground `args` (types of `from`) by the evaluator the program kept: a constant
    /// read by path (`Consts::value_with_args`: a trait's constant selected at `args[0]`), or
    /// a const block of a body (`Consts::block_value`, under the block's key over the owner's
    /// text). The evaluator checks any body and makes any impl row the value needs.
    fn const_at(&mut self, need: super::const_at::ConstNeed, from: tc::PTys<'_>, args: &[tc::PTy]) -> Result<DataRecord<Global>, super::const_at::ConstAtWhy> {
        let mut records = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            // As while `const_values` evaluates: a head's array length met inside this
            // evaluation is `ConstFail::Nested`, never a second evaluation inside the first.
            consts: None,
        };
        let made = match need {
            super::const_at::ConstNeed::Item(item) => self.consts.value_with_args(&mut records, self.prepared, item, from, args, Alloc::global()),
            super::const_at::ConstNeed::Block { owner, block } => {
                let Some(text) = records.store.fns.get(&owner).map(|h| h.text) else {
                    return Err(super::const_at::ConstAtWhy::Eval(super::consts::ConstFail::Body { item: owner, error: super::body::BodyError::NoHead(owner) }));
                };
                let key = super::const_at::block_key(owner, block, text);
                self.consts.block_value(&mut records, self.prepared, owner, block, key, from, args, Alloc::global())
            }
        };
        made.map(super::const_at::data_record).map_err(super::const_at::ConstAtWhy::Eval)
    }

    fn evaluates_at_args(&self) -> bool {
        true
    }

    fn last_projection(&self) -> Option<Box<str>> {
        self.projection_why.borrow_mut().take()
    }

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.store.adts.get(&key).map_or(ReprAttrs::RUST, |a| a.repr)
    }

    /// A `#[rustc_intrinsic]` fn's table entry, read from its name when it was tabled.
    fn intrinsic(&self, key: ItemKey) -> Option<Intrinsic> {
        self.fns.get(*self.by_key.get(&key)?)?.intrinsic
    }

    /// A static the program reaches: its head's mutability and `#[thread_local]` (statics.rs).
    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        self.statics.kind(key)
    }

    fn static_records(&self) -> &[(ItemKey, DataRecord<Global>)] {
        &self.statics.values
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
        // The evaluator's identity (consts.rs `item_code_id`): a static's fn pointer keys its
        // instance by it, and the run finds that fn's slot by the same key.
        // Its text as the evaluator reads it: a local item's kept text, a library item's own.
        let text = match (self.store.locals.head(owner), self.front.keys.library_in(self.front.names, owner)) {
            (Some(local), _) => Some(local.text),
            // `NamesSource::text_of`'s answer: the names library's item text.
            (None, Some(lib)) => self.front.names.item_text(lib).map(|t| t.text),
            (None, None) => self.store.fns.get(&owner).map(|h| h.text),
        };
        super::consts::item_code_id(owner, text)
    }
}
