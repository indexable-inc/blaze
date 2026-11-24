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
    Adt, Checked, ExprId, FnSig, ItemKey, LangItem, List, PTy, PTys, PatId, PortableTys, ResolveError, resolve_instance,
};

use super::Front2;
use super::body::{Body, BodyError, lower_again};
use super::lower_made::{LowerTarget, lower_made};
use super::buckets::Buckets;
use super::check::{BuildOrder, CheckFail, Records, check_body};
use super::consts::Consts;
use super::heads::{CrateCfg, Store};
use super::keys::{MemberKind, checker_of};
use super::reach::{First, Reached, Start, fn_shown, reach_all};
use super::test_entry::{EntryShape, TestEntryRefusal};
use super::spell::Spelling;
use super::types::Why;
use super::world_items::BodyWorld;
use crate::front::{At, FnKind, FnLang, HOSTED_START, LitTexts, int_value, string_bytes};
use blaze_hash::HashMap;
use blaze_lower::head;
use std::boxed::Box;
use std::format;
use std::string::String;
use crate::refusal::SourceFile;
use crate::seam::{FnFacts, FrontEnd};

mod float_text;
mod head_abi;
pub mod suite;
pub mod run_entry;
mod tabled;
mod left_out;
/// `symbols_of` and the link table on ring's in-body `prefixed_extern!`: one symbol.
#[cfg(test)]
mod ring_in_body_tests;
pub use left_out::{LeftAt, LeftOut};
pub mod lazy;
pub mod walk_report;

/// What the new front end refuses of a program, by name: each a stage it stopped at or a
/// question of the back half it cannot answer yet.
#[derive(Debug)]
pub enum Front2Refusal {
    /// The front end over the program's names could not start.
    Front(super::FrontError),
    /// The crate root names no `main` in the value namespace.
    NoMain,
    /// The root's `fn main` is the input of an attribute call that wrote no `main` (Class P: a
    /// written `#[bstd::main]` whose path does not resolve, a refused call, or one that expanded
    /// and declares no `main`; [`blaze_scope::main_call::MainCall`]) (rule 19).
    MainTakenBy { call: LibKey, path: Box<str>, implicit: bool, why: blaze_scope::main_call::MainCallWhy },
    /// The root's `main` is missing because the macro call that would have written it failed:
    /// under D104 the implicit `#[::std::main]` whose input is the written `fn main` (or a
    /// written attribute macro on it). The parts of blaze-scope's
    /// [`blaze_scope::failed_call::FailedCall`]: the call and its module by key, the macro's
    /// path, whether it is the implicit call, and the matcher's typed reason (`diagnostic`, its
    /// texts ranges of `notes`; [`blaze_expand::Failed::from_parts`]) (rule 19).
    MainNotWritten { call: LibKey, module: LibKey, path: Box<str>, implicit: bool, diagnostic: blaze_expand::Diagnostic, notes: Box<[u8]> },
    /// `main`'s body did not lower.
    Lower(BodyError),
    /// `main`'s names could not be spelled back from its lowering.
    Spell(Why),
    /// `main` did not check clean: diagnostics, gaps or a typed failure.
    Check(CheckFail),
    /// What an instance of `function` asked of its world (the seam's `make_asked`) could not
    /// be made.
    Asked { function: Box<str>, error: CheckFail },
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
    /// `cap` is the walk's ([`super::reach::Census::cap`]): sized by the `items` the names
    /// held, unless `forced` (the caller's); `report` is what the walk met
    /// ([`super::reach::capped_report`]: by crate, keys tabled twice, the fns still queued).
    Capped { entry: Box<str>, queued: usize, cap: usize, items: usize, forced: bool, report: Box<str> },
    /// A tabled fn has no head in front2's store.
    NoHead { function: Box<str> },
    /// A fn's attributes could not be read from its signature's lowering.
    Attrs { function: Box<str>, why: Box<str> },
    /// The names source did not list crate `krate`'s items (over records: its items record
    /// did not read; the error is in the source's log).
    Unlisted { krate: usize },
    /// The names source did not answer whether an `extern` block declares the fn (its error
    /// is in the source's log). Never over the solved names, which answer every key.
    Unplaced { function: Box<str> },
    /// A fn's ABI is neither Rust nor C.
    Abi { function: Box<str>, abi: tc::Abi },
    /// A literal whose value front2 cannot read (an escape, a form), at its range in the fn.
    Literal { function: Box<str>, what: &'static str, range: tc::Range },
    /// A `#[rustc_intrinsic]` fn the build's intrinsic table does not name and that its
    /// declaration gives no fallback body to build from (front2/intrinsic_rule.rs): which of
    /// the two reasons, and where the declaration stands (`file:line`, made when refused).
    Intrinsic { function: Box<str>, unbuilt: super::intrinsic_rule::Unbuilt, at: Box<str> },
    /// A declaration of an `extern "llvm-intrinsic"` block naming an LLVM intrinsic `symbol`
    /// (front2/llvm_intrinsic.rs) outside blaze-build's set (intrinsics/target.rs `named`).
    TargetIntrinsic { function: Box<str>, symbol: Box<str> },
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
    /// A blaze-edition program crate exports no hosted entry (run_entry.rs
    /// [`run_entry::HostedStartMissing`], the record walk's `EntryRefusal::NoHostedStart`):
    /// never started at its `main` directly.
    NoHostedStart,
    /// The names source did not answer a fact the start rule reads (over records: the error is
    /// in the source's log): `what` names the question, `krate` the crate it was asked of.
    StartFact { krate: usize, what: &'static str },
    /// The lazy program (program/lazy) refused one body, by name.
    Lazy(lazy::LazyRefusal),
    /// A reached fn's row path could not be built from the names the program declares (front2
    /// row_path.rs): refused by name, never a key's `Debug` in its place.
    NoRowPath(super::row_path::NoRowPath),
}

impl core::fmt::Display for Front2Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Front2Refusal::Front(e) => write!(f, "front2 could not start: {e}"),
            Front2Refusal::Lazy(e) => write!(f, "{e}"),
            Front2Refusal::NoRowPath(e) => write!(f, "{e}"),
            Front2Refusal::NoMain => write!(f, "front2: the crate root has no `main`"),
            Front2Refusal::MainTakenBy { call, path, implicit, why } => {
                let taken = blaze_scope::main_call::MainCall { call: *call, path: &**path, implicit: *implicit, at: None, why: *why };
                write!(f, "front2: the crate root's {taken}")
            }
            Front2Refusal::MainNotWritten { call, module, path, implicit, diagnostic, notes } => {
                let failed = blaze_scope::failed_call::FailedCall {
                    name: "main",
                    call: *call,
                    module: *module,
                    path: &**path,
                    implicit: *implicit,
                    at: None,
                    failed: blaze_expand::Failed::from_parts(*diagnostic, &**notes),
                };
                write!(f, "front2: the crate root's {failed}")
            }
            Front2Refusal::Lower(e) => write!(f, "front2: `main` did not lower: {e:?}"),
            Front2Refusal::Spell(e) => write!(f, "front2: `main`'s names: {e:?}"),
            Front2Refusal::Check(e) => write!(f, "front2: `main` did not check: {e:?}"),
            Front2Refusal::Asked { function, error } => write!(f, "front2: an answer an instance of `{function}` asked was not made: {error:?}"),
            Front2Refusal::Unchecked { function, class, first } => {
                write!(f, "front2: `{function}`, reached from `main`, is not checked ({class}): {first}")
            }
            Front2Refusal::NoBody { function } => write!(f, "front2: `{function}`, reached from `main`, has no body (front2's)"),
            Front2Refusal::Const { constant, why } => write!(f, "front2: the constant `{constant}`, read by a body of the program, was not evaluated: {why}"),
            Front2Refusal::Static { static_, why } => write!(f, "front2: the static `{static_}`, reached by the program, was not made: {why}"),
            Front2Refusal::Capped { entry, queued, cap, items, forced: false, report } => write!(
                f,
                "front2: `{entry}` reaches more than {cap} fns, the cap: {} x the {items} items, members and variants the program's names hold (under {}); {queued} still queued: the walk tabled more fns than exist, so it does not end\n{report}",
                super::reach::NEED_FACTOR,
                super::reach::MOST_FNS_CEILING
            ),
            Front2Refusal::Capped { entry, queued, cap, items, forced: true, report } => {
                write!(f, "front2: `{entry}` reaches more than {cap} fns, the cap its caller set (the names hold {items} items, members and variants); {queued} still queued\n{report}")
            }
            Front2Refusal::NoHead { function } => write!(f, "front2: `{function}` has no head in the store (front2's)"),
            Front2Refusal::Attrs { function, why } => write!(f, "front2: `{function}`'s attributes: {why}"),
            Front2Refusal::Unlisted { krate } => write!(f, "front2: the items of crate {krate} were not listed by the names source"),
            Front2Refusal::Unplaced { function } => write!(f, "front2: whether an `extern` block declares `{function}` was not answered by the names source"),
            Front2Refusal::Abi { function, abi } => write!(f, "front2: `{function}`'s ABI {abi:?} is neither Rust nor C"),
            Front2Refusal::Literal { function, what, range } => {
                write!(f, "front2: `{function}` at {}..{}: {what} (front2's)", range.start, range.end)
            }
            Front2Refusal::Intrinsic { function, unbuilt, at } => write!(
                f,
                "front2: `{function}` is an intrinsic the build's table does not name (blaze-build intrinsics), and its declaration at {at} cannot be built from: {unbuilt} (front2/intrinsic_rule.rs)"
            ),
            Front2Refusal::TargetIntrinsic { function, symbol } => {
                write!(f, "front2: `{function}` is a target intrinsic (`{symbol}`) outside the set the builder builds (blaze-build intrinsics/target.rs)")
            }
            Front2Refusal::AdtRow { adt, why } => write!(f, "front2: the ADT `{adt}`'s row: {why}"),
            Front2Refusal::ObjectTrait { trait_, why } => write!(f, "front2: the trait `{trait_}` as a trait object sees it: {why}"),
            Front2Refusal::Lang { item, why } => write!(f, "front2: the lang item {item:?}: {why}"),
            Front2Refusal::OwnLeftOut { item, why } => write!(f, "front2: `{item}`, of the program's own crate and reached from `main`, is not checked: {why}"),
            Front2Refusal::TwoEntries { symbol, first, second } => {
                write!(f, "front2: `{first}) and `{second}) are both exported as `{symbol}`, the process's entry")
            }
            Front2Refusal::NoLangStart { shelf } => write!(f, "the shelf has no `#[lang = \"start\"]` item: {shelf}"),
            Front2Refusal::Start { item, what } => write!(f, "front2: the Rust start: `{item}` {what}"),
            Front2Refusal::NoHostedStart => write!(f, "front2: the program's start: {}", run_entry::HostedStartMissing),
            Front2Refusal::StartFact { krate, what } => {
                write!(f, "front2: the program's start: the names source did not answer {what} of crate {krate} (the start rule reads it: program.rs `start_of`)")
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
    /// Whether the fn is nounwind ([`FrontEnd::nounwind`]; program/head_abi.rs `BuildAbi`):
    /// `abi` is one calling convention for "C" and "C-unwind", this keeps which.
    nounwind: bool,
    /// Whether the fn's own signature says `#[track_caller]` ([`FrontEnd::track_caller`] adds
    /// what a trait impl's method inherits from the trait's declaration).
    track_caller: bool,
    /// Function code-generation attributes (`#[inline]`, `#[inline(always)]`, `#[inline(never)]`, `#[cold]`).
    fn_attrs: blaze_lir::func::FnAttrs,
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
    /// Each string or byte string literal pattern's bytes, unescaped.
    str_pats: Vec<(PatId, Box<[u8], Global>), Global>,
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
    /// What the instance questions' worlds asked and found unmade (the seam's `make_asked`).
    asked: RefCell<Vec<super::world_items::Fill, Global>>,
    /// What the instance questions' worlds could not answer and no fill makes (a refused
    /// candidate impl, by name): `make_asked`'s refusal when no fill is left (rule 19).
    asked_gaps: RefCell<Vec<super::world_items::Gap, Global>>,
    /// Each crate's `#[define_opaque]` definers, read once per crate (tait.rs `Definers`).
    definers: super::tait::Definers<'static>,
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
    /// Every checked body's async blocks' state recipes, sorted by (owner, ordinal)
    /// (coroutine_recipes.rs): the seam's `coroutine_recipe`.
    recipes: Vec<super::coroutine_recipes::RecipeRow, Global>,
    /// The walk's fns by crate (crate index, fns tabled), most first ([`super::reach::by_crate`]).
    reached_by_crate: Vec<(usize, usize), Global>,
    /// Monomorphized specializations of generic `inline for` functions (`FrontEnd::specialized_row`).
    inline_for_specs: blaze_own::Vec<super::inline_for::SpecRow, Alloc<'static>>,
}

/// Where a program's walk starts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Starts<'a> {
    /// The process's entry: `main`, or the fn exported as the entry ([`exported_entry`]).
    Main,
    /// Every test fn of a test unit, by its tests record's row order (program/suite.rs).
    Tests(&'a [LibKey]),
    /// A survey's roots, the walk limited by `within` ([`Front2Program::survey_within`]): a
    /// root that does not lower is a left-out row, not the survey's stop.
    Within { roots: &'a [LibKey], within: super::reach::Within<'a> },
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
/// name there, so those answers never meet a lookup that fails. `Clone`: its `clone`, the
/// builtin clone's (blaze-ladder clone_shim.rs).
const SEAM_LANGS: [LangItem; 10] = [
    LangItem::Drop,
    LangItem::Deref,
    LangItem::DerefMut,
    LangItem::Fn,
    LangItem::FnMut,
    LangItem::FnOnce,
    LangItem::Index,
    LangItem::IndexMut,
    LangItem::Clone,
    LangItem::From,
];

fn lang_of(front: &Front2<'_>, item: LangItem) -> Result<Option<ItemKey>, crate::seam::LangRefusal> {
    match super::langs::find(&front.index, front.resolver, item) {
        // A `#[lang]` member (`Future::poll`) is asked by its member key, as the lazy program asks
        // it (keys.rs `asked_key`).
        Ok(found) => Ok(found.map(|lib| super::keys::asked_key(front.names, lib))),
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
    supers: Vec<blaze_build::vtable::ObjectSuper, Global>,
    /// The portable type table holding `supers`' argument trees (`None` when no supertrait
    /// carries generic arguments).
    super_tys: Option<PortableTys<'static>>,
    methods: Vec<blaze_build::vtable::ObjectMethod, Global>,
    shape: ContentId,
}

/// Whether a trait's method has a slot (the Reference, "Trait objects": a method callable on
/// the object): dyn-compatible by the checker's one rule ([`tc::dyn_compat::member_verdict`]),
/// and not one of the explicitly non-dispatchable (`where Self: Sized`, a `self` receiver).
/// Whether a trait's method has a slot in its vtables (the builder's `ObjectMethod::dispatch`):
/// also the rule the reach walk reaches each table's slots by (reach/instances/slots.rs).
pub(super) fn dispatchable(m: &tc::dyn_compat::MemberFacts, f: &tc::dyn_compat::FnFacts) -> bool {
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
    mut left_out: Option<&mut Vec<(Box<str>, LeftOut), Global>>,
    left: &mut HashMap<ItemKey, Box<str>, Global>,
) -> Result<super::statics::Statics, Front2Refusal> {
    let (statics, refused) = super::statics::static_values(r, bodies);
    for (k, why) in refused {
        // A static declared in a body (bstd `main!`'s `MANIFEST`) has no path: its written
        // head and the item declaring it name it (front2/static_shown.rs). The class carries
        // the name too: a run's own-left-out line shows the left map's text, not `static_`.
        let static_ = super::static_shown::static_shown(r, k).into_boxed_str();
        match left_out.as_deref_mut() {
            Some(out) => {
                let class = format!("the static {static_}: {why}").into_boxed_str();
                left.insert(k, class.clone());
                out.push((static_.clone(), LeftOut::named(class, Front2Refusal::Static { static_, why })));
            }
            None => return Err(Front2Refusal::Static { static_, why }),
        }
    }
    Ok(statics)
}

/// Each constant item `bodies` read by value, evaluated once by front2's evaluator (consts.rs
/// `Consts::value`, the one that fills array lengths), in key order: a constant an EXPRESSION
/// names (`Def::Const` in a checked body's `expr_defs`: what the builder's `Adts::const_value`
/// asks for a path) and one a PATTERN names (`Def::Const` in `pat_defs`: what the builder's
/// `const_bits` asks to compare a scrutinee with; the Reference, "Patterns", "Constant
/// patterns": the value is compared, so it must exist before the body is built). One list,
/// one evaluator, one refusal for both. Returns the records, sorted by key, and each constant
/// the evaluator refused with its words (a nested evaluation included: `ConstFail` names the
/// constant it stopped on). A path the check recorded generic arguments for
/// (`Checked::const_args`: a trait's constant, an inherent impl's over its generics) is not
/// asked here: it has a value only at an instance's ground arguments, pulled by the build
/// before it walks that instance (const_at.rs, the seam's `const_at`). Its body is checked
/// then, by this same evaluator, which the program keeps. A pattern's constant alike: one
/// with recorded arguments (`Checked::pat_const_args`: an inherent impl's, `U256::ZERO`) is
/// pulled at them by const_at.rs; one without is asked here.
fn const_values<'c, 'n, S: super::source::NamesSource<'n>>(r: &mut Records<'_, 'n, S>, bodies: impl Iterator<Item = &'c Checked<'static>>) -> (Vec<(ItemKey, DataRecord<Global>), Global>, Vec<(ItemKey, Box<str>), Global>) {
    let mut asked: Vec<ItemKey, Global> = Vec::new_in(Global);
    for c in bodies {
        let at_args = c.const_args.column::<0>();
        asked.extend(c.expr_defs.column::<0>().iter().zip(c.expr_defs.column::<1>().iter()).filter_map(|(e, d)| match d {
            tc::Def::Const(k) if at_args.binary_search(e).is_err() => Some(*k),
            _ => None,
        }));
        let pat_at_args = c.pat_const_args.column::<0>();
        asked.extend(c.pat_defs.column::<0>().iter().zip(c.pat_defs.column::<1>().iter()).filter_map(|(p, d)| match d {
            tc::Def::Const(k) if pat_at_args.binary_search(p).is_err() => Some(*k),
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
                // Whole, on one line (rule 19; see statics.rs `one_static`).
                let said = format!("{e}");
                refused.push((k, said.chars().map(|c| if c == '\n' { ' ' } else { c }).collect::<Box<str>>()));
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

/// Each trait of `asked` not yet in `tables`, with every supertrait under it, made into
/// `tables` (the whole program passes an empty map once; the lazy program its own map, per
/// body, so a trait is made once per command).
fn object_tables<'n, S: super::source::NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    mut asked: Vec<ItemKey, Global>,
    tables: &mut HashMap<ItemKey, ObjectTable, Global>,
    mut left_out: Option<&mut Vec<(Box<str>, LeftOut), Global>>,
) -> Result<(), Front2Refusal> {
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
            let lib = r.keys.library_in(r.names, k).ok_or_else(|| String::from("no library key"))?;
            r.store.ensure(r.names, r.keys, super::heads::Want::Head(lib)).map_err(|e| format!("its head: {e:?}"))?;
            r.store.ensure(r.names, r.keys, super::heads::Want::Members(lib)).map_err(|e| format!("its members: {e:?}"))?;
            let head = r.store.maps.traits.get(&lib).ok_or_else(|| String::from("made, but no head filed under its key"))?;
            let supers_written = r.store.maps.supers.get(&lib).ok_or_else(|| String::from("made, but no supertraits filed under its key"))?;
            let view = r.store.tys.view();
            let err_super = |sk: ItemKey, detail: core::fmt::Arguments<'_>| format!("its supertrait {}: {detail}", fn_shown(r, sk));
            let mut supers = Vec::with_capacity_in(supers_written.len(), Global);
            let mut super_tys: Option<PortableTys<'static>> = None;
            let mut copied: Vec<PTy, Global> = Vec::new_in(Global);
            let mut waiting = false;
            for &(s, args) in supers_written.iter() {
                let sk = checker_of(s);
                let written = view.list(args).map_err(|e| err_super(sk, format_args!("arguments: {e:?}")))?;
                // `Self` is the first argument (always the trait's own `Self`); each one after
                // it is copied as a portable type tree parameterized over this trait's own
                // parameters (`Self` 0, its parameters from 1): both bare parameters
                // (`FnMut<Args>: FnOnce<Args>`) and concrete/compound types
                // (`Buffer: AsRef<[u8]>`, `Sub<T>: Super<Option<T>>`). The builder composes
                // them down the tree (blaze-build vtable.rs `Shape::of`).
                let span = if written.len() > 1 {
                    let tys = super_tys.get_or_insert_with(|| PortableTys::new_in(r.store.tys.alloc()));
                    copied.clear();
                    for &a in written.iter().skip(1) {
                        let c = tys.copy_from(view, a, None).map_err(|e| err_super(sk, format_args!("argument: {e:?}")))?;
                        copied.push(c);
                    }
                    if tys.view().nodes().any(|n| matches!(n, tc::PNode::Infer | tc::PNode::Error | tc::PNode::Future(..))) {
                        return Err(err_super(sk, format_args!("non-portable argument type")));
                    }
                    tys.intern_list(&copied).map_err(|e| err_super(sk, format_args!("arguments: {e:?}")))?
                } else {
                    List::EMPTY
                };
                if refused.contains_key(&sk) {
                    return Err(err_super(sk, format_args!("has no table")));
                }
                if !tables.contains_key(&sk) {
                    asked.push(sk);
                    waiting = true;
                }
                supers.push(blaze_build::vtable::ObjectSuper { trait_: sk, args: span });
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
            let mut h = blaze_ids::Hasher::new("blaze-ladder front2 object trait 3");
            h.update(&k.0.to_le_bytes());
            for m in methods.iter() {
                h.update(&m.item.0.to_le_bytes()).update(&[u8::from(m.dispatch)]);
            }
            let super_view = super_tys.as_ref().map_or(PTys::EMPTY, PortableTys::view);
            for s in supers.iter() {
                let below = tables.get(&s.trait_).map_or([0; 32], |t| t.shape.0);
                h.update(&s.trait_.0.to_le_bytes()).update(&below).update(&s.args.len.to_le_bytes());
                let list = super_view.list(s.args).map_err(|e| err_super(s.trait_, format_args!("arguments: {e:?}")))?;
                for &a in list {
                    crate::code_records::reads::term(&mut h, a, super_view)
                        .map_err(|e| err_super(s.trait_, format_args!("argument digest: {e:?}")))?;
                }
            }
            Ok(Some(ObjectTable { supers, super_tys, methods, shape: h.finish() }))
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
                    Some(out) => {
                        let class = format!("object trait: {why}").into_boxed_str();
                        out.push((trait_, LeftOut::named(class, Front2Refusal::ObjectTrait { trait_: super::item_shown::refused(r, k), why })));
                    }
                    None => return Err(Front2Refusal::ObjectTrait { trait_: super::item_shown::refused(r, k), why }),
                }
            }
        }
    }
    Ok(())
}

impl<'n> Front2Program<'n> {
    /// `main` of the program's crate (index 0 of `names`), lowered and checked; refused by name
    /// where the back half would ask what front2 cannot answer yet.
    /// `frame` is the caller's, alive for the command (`Front2::new`); `alloc` is the check's,
    /// handed down from the program's entry (`'static`, as the head store's tables are).
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], alloc: Alloc<'static>) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, frame, user, None, alloc)
    }

    /// Where the process starts ([`super::start::Entry2`]): what the builder makes the C entry
    /// from (LADDER.md L1b); `None` for a test unit's suite.
    pub fn start(&self) -> Option<super::start::Entry2> {
        self.start
    }
    /// The program with every fn that checks clean tabled and every other one LEFT OUT (each
    /// named in `left_out` with its cause), for a survey of what the back half builds
    /// ([`crate::run::build_survey`]); never run: a left-out fn's caller cannot be built.
    pub fn survey(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], left_out: &mut Vec<(Box<str>, LeftOut), Global>, alloc: Alloc<'static>) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make(names, frame, user, Some(left_out), alloc)
    }

    /// [`Front2Program::survey`] from given ROOTS (each a fn's library key, of any crate): one
    /// walk from all of them, every fn reached that checks clean tabled and every other one left
    /// out. For a census of what the back half builds of a program whose `main` is not crate
    /// 0's (blaze-real's `tempo_build` example: a workspace's binary). Never run: the program has
    /// no entry, so [`crate::run::build_survey`] builds every clean non-generic fn and each
    /// generic one at the instances the built bodies call it at.
    /// `body_sources` is what a body's macro calls expand with (the crates' environment, texts
    /// and proc-macro runner: [`super::body_host::BodySources`], the item-level expansion's), as
    /// the command's program has it (`set_body_sources`); `None` refuses each body `env!`,
    /// include and proc macro by name.
    pub fn survey_from(
        names: &'n Names<'n>,
        frame: &'n blaze_mem::Frame,
        user: &[SourceFile<'_>],
        roots: &[LibKey],
        left_out: &mut Vec<(Box<str>, LeftOut), Global>,
        body_sources: Option<&'n dyn super::body_host::BodySources>,
        alloc: Alloc<'static>,
    ) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make_from(names, frame, user, Some(left_out), Starts::Tests(roots), body_sources, alloc).map(|made| made.program)
    }

    /// [`Front2Program::survey_from`] limited by `within` (reach.rs [`super::reach::Within`]):
    /// only fns of its crates are lowered, checked and walked (another crate's fn is a left-out
    /// row of class [`super::reach::OUTSIDE`], so the builds survey builds none of it), and a
    /// root that does not lower is a left-out row (`lower: root`), not the survey's stop.
    #[expect(clippy::too_many_arguments, reason = "survey_from's inputs and the limit, each read once")]
    pub fn survey_within(
        names: &'n Names<'n>,
        frame: &'n blaze_mem::Frame,
        user: &[SourceFile<'_>],
        roots: &[LibKey],
        within: super::reach::Within<'_>,
        left_out: &mut Vec<(Box<str>, LeftOut), Global>,
        body_sources: Option<&'n dyn super::body_host::BodySources>,
        alloc: Alloc<'static>,
    ) -> Result<Front2Program<'n>, Front2Refusal> {
        Self::make_from(names, frame, user, Some(left_out), Starts::Within { roots, within }, body_sources, alloc).map(|made| made.program)
    }

    /// The walk's fns by crate: (crate index, fns tabled), most first, then by crate
    /// (`usize::MAX`: a key no crate holds).
    pub fn reached_by_crate(&self) -> &[(usize, usize)] {
        &self.reached_by_crate
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

    fn make(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], left_out: Option<&mut Vec<(Box<str>, LeftOut), Global>>, alloc: Alloc<'static>) -> Result<Front2Program<'n>, Front2Refusal> {
        // A program of given files (a hello world, a fixture): no package, no proc-macro
        // runner, so no body sources.
        Self::make_from(names, frame, user, left_out, Starts::Main, None, alloc).map(|made| made.program)
    }

    /// The program from its ROOTS ([`Starts`]): `main` (the process's entry), or every test fn
    /// of a test unit (program/suite.rs). One walk from all roots ([`super::reach::reach_all`]),
    /// so a fn two roots reach is checked and tabled once; each root keeps its row.
    /// `body_sources` is the store's `sources` (what a body's `env!`, includes and proc macros
    /// are answered from), set before any body is lowered.
    fn make_from(
        names: &'n Names<'n>,
        frame: &'n blaze_mem::Frame,
        user: &[SourceFile<'_>],
        mut left_out: Option<&mut Vec<(Box<str>, LeftOut), Global>>,
        starts: Starts<'_>,
        body_sources: Option<&'n dyn super::body_host::BodySources>,
        alloc: Alloc<'static>,
    ) -> Result<Made<'n>, Front2Refusal> {
        // The program's lang rows are its link closure's (linked.rs): `main`'s program is
        // crate 0's; given roots (a workspace's binary in `survey_from`, a test unit's tests,
        // a walk's roots) are the program of their crates.
        let linked = match starts {
            Starts::Main => super::linked::Linked::of(names, &[0], alloc),
            Starts::Tests(keys) | Starts::Within { roots: keys, .. } => super::linked::Linked::of_keys(names, keys, alloc),
        }
        .map_err(|e| Front2Refusal::Front(super::FrontError::Roots(e)))?;
        let mut front = Front2::for_program(names, frame, user, &linked).map_err(Front2Refusal::Front)?;
        let mut roots: Vec<LibKey, Global> = Vec::new_in(Global);
        // Made before the roots: a Rust program's start reads `main`'s head (start.rs).
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        store.sources = body_sources;
        let mut start2: Option<super::start::Entry2> = None;
        let tests = match starts {
            Starts::Main => {
                let written = Written { kind: PathKind::Plain, segments: &["main"] };
                // A miss behind a failed call (D104's implicit `::std::main` that took no arm)
                // names that call and why (blaze-scope failed_call.rs), never "no `main`"; one
                // behind an attribute call that wrote no `main` names that call (main_call.rs).
                let found = names::path(names, names.root(0), Namespace::Values, written, alloc).map_err(|error| {
                    match blaze_scope::failed_call::FailedCall::of(names, &error, "main") {
                        Some(failed) => Front2Refusal::MainNotWritten {
                            call: failed.call,
                            module: failed.module,
                            path: failed.path.into(),
                            implicit: failed.implicit,
                            diagnostic: failed.failed.diagnostic,
                            notes: failed.failed.notes().into(),
                        },
                        None => match blaze_scope::main_call::MainCall::of(names, 0) {
                            Some(taken) => Front2Refusal::MainTakenBy { call: taken.call, path: taken.path.into(), implicit: taken.implicit, why: taken.why },
                            None => Front2Refusal::NoMain,
                        },
                    }
                })?;
                let rust_main: LibKey = found.whole(1).ok_or(Front2Refusal::NoMain)?.key;
                // The process's entry: an exported one ([`exported_entry`]); else the root's
                // start by THE rule the record walk takes too (run_entry.rs `RootStart`, decided
                // once from crate 0's `Implicit::std` and root attributes): a blaze-edition root
                // that exports nothing is refused by name, never started at its `main`
                // directly; a Rust root starts through the `#[lang = "start"]` the program links
                // (start.rs `linked_lang_start`, Rust's rule), else at its `fn main` directly (a
                // program that links no std). The walk and `entry()` start at its root.
                // The rule is [`start_of`], which `blaze run` asks too before anything is built
                // (run.rs `entry_start`): one home.
                let entry = match start_of(names, alloc)? {
                    StartOf::Exported(e) => e,
                    StartOf::LangStart(lang_start) => super::start::Entry2::Rust(super::start::rust_start(names, lang_start, rust_main, &mut store, &mut front.keys)?),
                    StartOf::Direct => super::start::Entry2::Direct(rust_main),
                };
                roots.push(entry.root());
                start2 = Some(entry);
                false
            }
            Starts::Tests(list) | Starts::Within { roots: list, .. } => {
                roots.extend_from_slice(list);
                true
            }
        };
        let mut buckets = None;
        let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
        let prepared = Prepares::new_in(bump);
        let mut mains: Vec<Body<'n>, Global> = Vec::with_capacity_in(roots.len(), Global);
        let mut kept: Vec<LibKey, Global> = Vec::with_capacity_in(roots.len(), Global);
        // The records, with the program's one constant evaluator, are made BEFORE the roots
        // are lowered: a root's written type may name a constant (`size_of::<[u32;
        // BUFFER_SIZE]>()` in `main`: the Reference, "Array types": the length is a constant
        // expression, a const context), which its lowering asks for as `Why::NeedConst`. A
        // root is lowered by the one loop that evaluates what a lowering needs and lowers
        // again (front2/lower_made.rs), as every reached fn is (reach.rs); a bare `lower_fn`
        // here returned the need as `main`'s refusal.
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
        for &lib in roots.iter() {
            // An entry declared in a block (super::nested_exports): the bodies around it are
            // lowered first, outermost first, so each keeps its local items (locals.rs
            // `Locals::keep`, which runs before the body's check) and the entry's head and text
            // are known. An enclosing body's refusal stops the run, named (no quiet pass;
            // coordinator's ruling 19:1x).
            let mut lowered = Ok(());
            for &outer in super::nested_exports::enclosing_bodies(names, alloc, lib)?.iter() {
                if let Err(e) = lower_made(&mut records, prepared, LowerTarget::Free(outer)) {
                    lowered = Err(e);
                    break;
                }
            }
            let lowered = lowered.and_then(|()| lower_made(&mut records, prepared, LowerTarget::Free(lib)));
            match (lowered, left_out.as_deref_mut()) {
                (Ok(m), _) => {
                    mains.push(m);
                    kept.push(lib);
                }
                // A survey limited by `Within` (its roots are every fn of some crates, no suite's
                // rows): a root that does not lower is filed and left out, as a reached fn is.
                (Err(e), Some(out)) if matches!(starts, Starts::Within { .. }) => {
                    // By its path, as every reached fn's row is (`fn_shown`), so a census files
                    // it by its crate; its key only when the names hold no path.
                    let item = super::item_shown::ItemShown::of_lib(names, lib);
                    let shown: Box<str> = item.path().map_or_else(|| item.boxed(), Box::from);
                    out.push((shown, LeftOut::named(Box::from("lower: root"), Front2Refusal::Lower(e))));
                }
                (Err(e), _) => return Err(Front2Refusal::Lower(e)),
            }
        }
        // The roots that lowered, in the order given: what `mains` is the bodies of.
        roots = kept;
        // In a survey a root that does not check is filed and left out, and the walk goes on
        // from what it calls by name: what is reachable is surveyed whatever the root is.
        let mut checked: Vec<Option<Checked<'static>>, Global> = Vec::with_capacity_in(mains.len(), Global);
        // Each checked body's async blocks' state recipes, made with its check (coroutine_recipes.rs).
        let mut recipes: Vec<super::coroutine_recipes::RecipeRow, Global> = Vec::new_in(Global);
        for m in mains.iter_mut() {
            let checked_res = if m.typeck.inline_fors.is_empty() {
                let v = m.lowered.view();
                let s = Spelling::of(&v).map_err(Front2Refusal::Spell)?;
                let scope = m.scope(&s);
                check_body(&mut records, m, &scope)
            } else {
                super::inline_for::unroll_concrete_body(&mut records, prepared, m)
            };
            let done = checked_res.and_then(|c| {
                super::coroutine_recipes::recipes_of(&mut records, m, &c, &mut recipes, alloc)
                    .map_err(|error| CheckFail::Check { owner: m.owner, error })?;
                Ok(c)
            });
            checked.push(match done {
                Ok(c) => Some(c),
                Err(e) => match left_out.as_deref_mut() {
                    Some(out) => {
                        let d = format!("{e:?}");
                        let class = format!("check: {}", &d[..d.find(['(', ' ', '{']).unwrap_or(d.len())]);
                        let whole = format!("{e}").into_boxed_str();
                        let row = LeftOut::reached(&records, class.into_boxed_str(), whole, Front2Refusal::Check(e));
                        out.push((fn_shown(&records, m.owner).into_boxed_str(), row));
                        None
                    }
                    None => return Err(Front2Refusal::Check(e)),
                },
            });
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
        // The program's compiler_builtins soft-float routines (builtins_shim.rs): an `f128`
        // operation is a call of one, made by the emitter's legalize, so no body names them;
        // each found one is reached beside the roots, so its row and body are the build's
        // (run.rs `build_from` binds them as the record walk does, run/make_one.rs).
        let builtins = super::builtins_shim::find(names, linked.roots(), alloc);
        for r in blaze_lir::legalize::SoftFloat::ALL {
            let Some(lib) = builtins.lib(r) else { continue };
            let k = records.keys.item(lib).map_err(|clash| Front2Refusal::Unchecked {
                function: Box::from(r.symbol()),
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
        let within = match starts {
            Starts::Within { within, .. } => within,
            Starts::Main | Starts::Tests(_) => super::reach::Within::ALL,
        };
        let mut census = reach_all(&mut records, prepared, walk_from, &also, within);
        if census.capped {
            let entry = mains.first().map_or_else(|| String::from("the suite's tests"), |m| fn_shown(&records, m.owner));
            let report = super::reach::capped_report(&records, &census).into_boxed_str();
            return Err(Front2Refusal::Capped { entry: entry.into_boxed_str(), queued: census.queued, cap: census.cap, items: census.items, forced: census.forced, report });
        }
        // Specialize any reached generic `inline for` functions at every concrete call site and
        // merge newly discovered callees into `census`.
        let inline_specs = super::inline_for::specialize_generic_inline_fors(&mut records, prepared, &mains, &checked, &mut census)
            .map_err(Front2Refusal::Check)?;
        let reached_by_crate = super::reach::by_crate(&records, &census.fns);
        // Every fn the walk met is checked clean, or the program is refused by its first one
        // that is not: a fn with any diagnostic or gap is never built.
        // Each row's path, or why none is built: decided per fn by `tabled::left_out_or_stop`
        // in the table loop below (a survey files it and goes on), never returned from here.
        let mut bodiless: Vec<(ItemKey, Result<super::row_path::RowPath, super::row_path::NoRowPath>), Global> = Vec::new_in(Global);
        let mut ctors: Vec<(ItemKey, Result<super::row_path::RowPath, super::row_path::NoRowPath>), Global> = Vec::new_in(Global);
        let mut declared: Vec<(ItemKey, Result<super::row_path::RowPath, super::row_path::NoRowPath>), Global> = Vec::new_in(Global);
        // Every key a survey leaves out, with its cause: the seam's `left_out`.
        let mut left: HashMap<ItemKey, Box<str>, Global> = HashMap::new_in(Global);
        for (k, outcome) in census.fns.iter_mut() {
            match outcome {
                Reached::Failed { class, detail, refusal } if let Some(out) = left_out.as_deref_mut() => {
                    // The cause a caller's build names ([`RunError::LeftOut`]'s `why`): the class
                    // and its WHOLE detail, on one line (rule 19: an error names the thing; a cut
                    // at 160 characters hid the failing goal of Class O, 2026-11-16).
                    let mut why = String::with_capacity(class.len() + 2 + detail.len());
                    why.push_str(class);
                    why.push_str(": ");
                    why.extend(detail.chars().map(|c| if c == '\n' { ' ' } else { c }));
                    left.insert(*k, why.into_boxed_str());
                    // The row keeps the walk's typed refusal (program/left_out.rs); a site that
                    // named its cause by text only keeps the refusal a run stops with.
                    let refusal = match refusal.take() {
                        Some(r) => *r,
                        None => Front2Refusal::Unchecked { function: super::item_shown::refused(&records, *k), class: class.clone(), first: detail.clone() },
                    };
                    let row = LeftOut::reached(&records, core::mem::take(class), core::mem::take(detail), refusal);
                    out.push((fn_shown(&records, *k).into_boxed_str(), row));
                }
                Reached::Failed { class, detail, .. } => {
                    return Err(Front2Refusal::Unchecked {
                        function: super::item_shown::refused(&records, *k),
                        class: core::mem::take(class),
                        first: core::mem::take(detail),
                    });
                }
                Reached::NoBody => bodiless.push((*k, super::row_path::row_path_of(&records, *k))),
                // A ctor is the language's: tabled as one, the build makes its value.
                Reached::Ctor => ctors.push((*k, super::row_path::row_path_of(&records, *k))),
                // A call on a parameter or `dyn` is resolved by the back half at the instance,
                // to an impl fn the walk tabled (reach.rs `Instances`); the trait's fn itself is
                // tabled as declared: the build reads its ABI at the call before resolving it.
                Reached::AtInstance | Reached::AtObject => declared.push((*k, super::row_path::row_path_of(&records, *k))),
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
        for c in checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)).chain(inline_specs.iter().map(|s| &s.checked)) {
            for &item in c.callees.column::<1>() {
                let of_trait = matches!(records.keys.origin(item), Some(super::keys::Origin::Member { container, kind: MemberKind::Fn, .. }) if !records.store.impls.contains_key(container));
                if of_trait && met.insert(item, ()).is_none() {
                    declared.push((item, super::row_path::row_path_of(&records, item)));
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
            let views = core::iter::once(records.store.tys.view())
                .chain(checked.iter().flatten().map(|c| c.types.view()))
                .chain(census.bodies.iter().map(|(_, c)| c.types.view()))
                .chain(inline_specs.iter().map(|s| s.checked.types.view()));
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
            if let Some(lib) = records.index.lang("panic_location") {
                let k = super::keys::asked_key(names, lib);
                if !records.store.adts.contains_key(&k) && !given_up.contains_key(&k) {
                    missing.push(k);
                }
            }
            missing.sort_unstable();
            missing.dedup();
            let Some(&first) = missing.first() else { break };
            let refuse = |r: &Records<'_, '_>, k: ItemKey, why: String| Front2Refusal::AdtRow { adt: super::item_shown::refused(r, k), why: why.into_boxed_str() };
            if round == super::reach::MOST_FNS {
                return Err(refuse(&records, first, String::from("rows still missing after the most rounds")));
            }
            for k in missing {
                let why = match records.keys.library_in(names, k) {
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
                        out.push((adt, LeftOut::named(class, Front2Refusal::AdtRow { adt: super::item_shown::refused(&records, k), why })));
                        given_up.insert(k, ());
                    }
                    None => return Err(Front2Refusal::AdtRow { adt: super::item_shown::refused(&records, k), why }),
                }
            }
        }
        // Every constant a checked body reads by value, evaluated once now (the builder's
        // `const_value` reads the record); one the evaluator refuses is named: in a survey filed
        // and left out (each reader's build then names it), in a run the program's refusal.
        let (values, refused) = const_values(
            &mut records,
            checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)).chain(inline_specs.iter().map(|s| &s.checked)),
        );
        for (k, why) in refused {
            let constant = fn_shown(&records, k).into_boxed_str();
            match left_out.as_deref_mut() {
                Some(out) => {
                    let class = format!("constant: {why}").into_boxed_str();
                    left.insert(k, class.clone());
                    out.push((constant, LeftOut::named(class, Front2Refusal::Const { constant: super::item_shown::refused(&records, k), why })));
                }
                None => return Err(Front2Refusal::Const { constant: super::item_shown::refused(&records, k), why }),
            }
        }
        let statics = made_statics(
            &mut records,
            checked.iter().flatten().chain(census.bodies.iter().map(|(_, c)| c)).chain(inline_specs.iter().map(|s| &s.checked)),
            left_out.as_deref_mut(),
            &mut left,
        )?;
        let views = core::iter::once(records.store.tys.view())
            .chain(checked.iter().flatten().map(|c| c.types.view()))
            .chain(census.bodies.iter().map(|(_, c)| c.types.view()))
            .chain(inline_specs.iter().map(|s| s.checked.types.view()));
        let asked = dyn_principals(views);
        let mut objects: HashMap<ItemKey, ObjectTable, Global> = HashMap::new_in(Global);
        object_tables(&mut records, asked, &mut objects, left_out.as_deref_mut())?;
        // Each checked root's and each reached body's row path, or why none is built, in the
        // order they are lowered again below; `None`: a root that did not check.
        let mut main_paths: Vec<Option<Result<super::row_path::RowPath, super::row_path::NoRowPath>>, Global> = Vec::with_capacity_in(mains.len(), Global);
        for (m, c) in mains.iter().zip(checked.iter()) {
            main_paths.push(c.as_ref().map(|_| super::row_path::row_path_of(&records, m.owner)));
        }
        let mut body_paths: Vec<Result<super::row_path::RowPath, super::row_path::NoRowPath>, Global> = Vec::with_capacity_in(census.bodies.len(), Global);
        for (b, _) in census.bodies.iter() {
            body_paths.push(super::row_path::row_path_of(&records, b.owner));
        }
        let mut spec_paths: blaze_own::Vec<Result<super::row_path::RowPath, super::row_path::NoRowPath>, Alloc<'static>> = blaze_own::Vec::with_capacity_in(inline_specs.len(), alloc);
        for s in inline_specs.iter() {
            spec_paths.push(super::row_path::row_path_of(&records, s.orig));
        }
        drop(scopes);
        drop(spells);
        drop(lowered);
        // One table for the back half, which reads every body and record from one: the
        // records' table, then each body lowered again into it in the order tabled. (A checked
        // body's own table is a copy of the records' at its lowering, so two bodies' own types
        // would share indices; the records' table never holds a body's type.)
        // A table error names the first root (the crate's root module when a suite has none).
        let table_item = roots.first().copied().unwrap_or_else(|| names.root(0));
        let mut tys = store.tys.fork_in(alloc).map_err(|error| Front2Refusal::Lower(BodyError::Table { item: table_item, error }))?;
        // A bodied fn is lowered again only when its row path is built; no path, or a body that
        // does not lower again, is that ONE fn's refusal (tabled.rs `lowered_row`), decided
        // where every fn's is (`left_out_or_stop`): a survey files it and goes on, a run stops.
        let mut main = Vec::with_capacity_in(mains.len(), Global);
        for ((m, c), path) in mains.iter().zip(checked).zip(main_paths) {
            let (Some(c), Some(path)) = (c, path) else { continue };
            match tabled::lowered_row(names, prepared, &mut front.keys, &mut store, m, &mut tys, path) {
                Ok((body, path)) => main.push((body, c, path)),
                Err((shown, refusal)) => tabled::left_out_or_stop(m.owner, shown, refusal, &mut left, left_out.as_deref_mut())?,
            }
        }
        // `main`'s program enters at row 0 (none when a survey left `main` out above); a
        // suite's rows are entered by the launcher, each by its test's row (program/suite.rs),
        // so it has no one entry.
        let entry = if tests { None } else { main.first().map(|_| 0) };
        recipes.extend(census.recipes);
        super::coroutine_recipes::sort(&mut recipes);
        let mut bodies = Vec::with_capacity_in(census.bodies.len(), Global);
        for ((b, c), path) in census.bodies.into_iter().zip(body_paths) {
            match tabled::lowered_row(names, prepared, &mut front.keys, &mut store, &b, &mut tys, path) {
                Ok((body, path)) => bodies.push((body, c, path)),
                Err((shown, refusal)) => tabled::left_out_or_stop(b.owner, shown, refusal, &mut left, left_out.as_deref_mut())?,
            }
        }
        let mut spec_lowered: blaze_own::Vec<(ItemKey, blaze_own::Vec<PTy, Alloc<'static>>, Body<'n>, Checked<'static>, super::row_path::RowPath), Alloc<'static>> = blaze_own::Vec::with_capacity_in(inline_specs.len(), alloc);
        for (s, path) in inline_specs.into_iter().zip(spec_paths) {
            match tabled::lowered_row(names, prepared, &mut front.keys, &mut store, &s.body, &mut tys, path) {
                Ok((body, path)) => spec_lowered.push((s.orig, s.args, body, s.checked, path)),
                Err((shown, refusal)) => tabled::left_out_or_stop(s.spec_key, shown, refusal, &mut left, left_out.as_deref_mut())?,
            }
        }
        // The walk's steps, sorted and once each: what a test's reach is read from.
        let mut edges = census.edges;
        edges.sort_unstable();
        edges.dedup();
        let mut fns = Vec::with_capacity_in(main.len() + bodies.len() + bodiless.len(), Global);
        let mut by_key = HashMap::new_in(Global);
        let all = main.into_iter().chain(bodies);
        let bodied = all.map(|(body, checked, path)| (body.owner, Some((body, checked)), Ok::<_, super::row_path::NoRowPath>(path), false));
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
            let path = match path {
                Ok(path) => path,
                Err(no) => {
                    tabled::left_out_or_stop(key, no.item.boxed(), Front2Refusal::NoRowPath(no), &mut left, left_out.as_deref_mut())?;
                    if of_main {
                        entry = None;
                    }
                    continue;
                }
            };
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
        let mut inline_for_specs: blaze_own::Vec<super::inline_for::SpecRow, Alloc<'static>> = blaze_own::Vec::with_capacity_in(spec_lowered.len(), alloc);
        for (orig, args, body, checked, path) in spec_lowered {
            let key = body.owner;
            match tabled::table_one(names, &store, key, Some((body, checked)), path, false) {
                tabled::Verdict::Tabled(row) => {
                    let row_idx = fns.len();
                    by_key.insert(key, row_idx);
                    fns.push(row);
                    inline_for_specs.push(super::inline_for::SpecRow { orig, args, row: row_idx });
                }
                tabled::Verdict::LeftOut { path, refusal } => {
                    tabled::left_out_or_stop(key, path, refusal, &mut left, left_out.as_deref_mut())?;
                }
            }
        }
        for (key, path) in ctors {
            let path = match path {
                Ok(path) => path,
                Err(no) => {
                    tabled::left_out_or_stop(key, no.item.boxed(), Front2Refusal::NoRowPath(no), &mut left, left_out.as_deref_mut())?;
                    continue;
                }
            };
            // A ctor with no row in the store is that ONE ctor's refusal: a survey files it and
            // goes on (a call of it is then a refused callee), a run stops, named.
            if !store.ctors.contains_key(&key) {
                let function = path.into_text();
                tabled::left_out_or_stop(key, function.clone(), Front2Refusal::NoHead { function }, &mut left, left_out.as_deref_mut())?;
                continue;
            }
            by_key.insert(key, fns.len());
            let at = head_abi::placed_ctor(names, front.keys.library_in(names, key));
            let none = Box::<str>::from("");
            fns.push(Tabled { key, body: None, checked: None, kind: FnKind::Ctor, path: path.into_text(), at, abi: Abi::Rust, nounwind: false, track_caller: false, fn_attrs: blaze_lir::func::FnAttrs::default(), symbol: none, export: None, lits: Literals::default(), intrinsic: None });
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
        let program = Front2Program { front, store, tys, buckets, fns, by_key, entry, objects, langs, left, const_values: values, statics, projection_why: core::cell::RefCell::new(None), asked: RefCell::new(Vec::new_in(Global)), asked_gaps: RefCell::new(Vec::new_in(Global)), definers: super::tait::Definers::new_in(alloc), edges, consts, prepared, start: start2, recipes, reached_by_crate, inline_for_specs };
        Ok(Made { program, roots: rows, shapes })
    }

    /// `f` over the checker's world of fn `i`: its body's, or for a ctor (a fn row with no body,
    /// built as a fn by blaze-build's `build_ctor_fn`) its ADT's (`ctor_world`). Any other row
    /// with no body, or a body whose spelling cannot be made, has no world: the question is the
    /// world's own refusal naming the fn, and only the instance that asked it is refused.
    fn in_world<T>(&self, i: usize, f: impl FnOnce(&BodyWorld<'_, 'n>) -> T) -> Result<T, ResolveError> {
        let key = self.fns[i].key;
        let Some(body) = self.fns[i].body.as_ref() else {
            return super::world_items::ctor_world(self.front.names, &self.front.keys, &self.front.index, &self.store, self.buckets.as_ref(), key, &self.asked, &self.asked_gaps, f);
        };
        let view = body.lowered.view();
        // A function of the lowering alone, made once when the body was checked.
        let Ok(spell) = Spelling::of(&view) else {
            return Err(ResolveError::Missing(tc::Question::FnSig(key)));
        };
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
            path_marks: scope.path_marks,
            path_ns: scope.path_ns,
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
            // Every question here is at an instance's ground arguments, after the body checked
            // (rustc's post-analysis mode): an opaque is its hidden type, so `Iterator::next`
            // on a `FlatMap` whose fn returns `impl Iterator` finds the impl.
            reveal: super::opaque_auto::Reveal::All,
        };
        let out = f(&world);
        // What the question found unmade, for the build's `make_asked`; what it found refused
        // (a candidate impl's header, by name), for `make_asked`'s refusal when nothing is
        // left to make: never the bare `Missing` (rule 19).
        self.asked.borrow_mut().extend(world.fills.into_inner());
        self.asked_gaps.borrow_mut().extend(world.gaps.into_inner());
        Ok(out)
    }
}

/// How the process of the program in `names` starts, decided from its names before any head or
/// body is read: what [`start_of`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartOf {
    /// An entry crate 0 exports ([`exported_entry`]): `Entry2::Hosted` or `Entry2::CMain`.
    Exported(super::start::Entry2),
    /// A Rust program with std: its root's `fn main` through this `#[lang = "start"]` item
    /// (start.rs [`super::start::rust_start`] then reads `main`'s head).
    LangStart(LibKey),
    /// The root's `fn main`, called directly: a program that links no `#[lang = "start"]`.
    Direct,
}

/// THE START RULE (one home): an exported entry ([`exported_entry`]); else the root's start
/// ([`run_entry::RootStart`], from crate 0's `Implicit::std` and root attributes: a
/// blaze-edition root with no export is refused, [`Front2Refusal::NoHostedStart`]), refined by
/// what is linked (start.rs `linked_lang_start`: a `#![no_std]` root's `extern crate std`, a
/// shelf-less program), which `RootStart` does not read. Reads crate 0's items and the linked
/// crates' lang rows; no head, no body. [`Front2Program`]'s walk and `blaze run`'s choice of
/// process (run.rs `entry_start`) both take it, so the two cannot disagree. `alloc` holds the
/// link walk's lists for the call.
///
/// Generic over the names source (D124): over the solved names as the solved front asks it,
/// and over the names RECORDS as `blaze run` asks it before anything is solved. The two give
/// one answer by construction (tests/run_special.rs asserts it). A fact the source does not
/// answer is [`Front2Refusal::StartFact`] naming it.
pub fn start_of<'n, S: super::source::NamesSource<'n>>(names: S, alloc: Alloc<'_>) -> Result<StartOf, Front2Refusal> {
    if let Some(e) = exported_entry(names)? {
        return Ok(StartOf::Exported(e));
    }
    if names.crate_count() == 0 {
        return Err(Front2Refusal::NoMain);
    }
    let start = run_entry::RootStart::of_names(names).ok_or(Front2Refusal::StartFact { krate: 0, what: "its `Implicit::std` and its root's `#![no_core]`/`#![no_std]`" })?;
    match start.unexported().map_err(|run_entry::HostedStartMissing| Front2Refusal::NoHostedStart)? {
        run_entry::Unhosted::LangStart | run_entry::Unhosted::Direct => match super::start::linked_lang_start(names, alloc)? {
            Some(lang_start) => Ok(StartOf::LangStart(lang_start)),
            None => Ok(StartOf::Direct),
        },
    }
}

/// THE ENTRY RULE (one place): the process starts at the free fn of the program's crate
/// (crate 0) exported as [`HOSTED_START`] (bstd's hosted entry, given the host's table), else
/// the one exported as `main` (the C entry the platform calls), else at the crate root's
/// `fn main` (`None` here). Exported means `#[no_mangle]` or `#[export_name = ".."]`, read from
/// each fn's attribute rows through its signature's lowering ([`symbols_of`]), the fns listed by
/// the names library ([`Names::items`]); two fns under one symbol is [`Front2Refusal::TwoEntries`].
/// The answer is a function of the program: candidates are ordered by key, never by the map.
fn exported_entry<'n, S: super::source::NamesSource<'n>>(names: S) -> Result<Option<super::start::Entry2>, Front2Refusal> {
    // Crate 0's fn rows, in key order (both sources list by key).
    let mut fns: Vec<LibKey, Global> = Vec::new_in(Global);
    let listed = names.each_item(0, &mut |key, _module| {
        if names.kind_of_item(key) == Some(blaze_surface::ItemKind::Function) {
            fns.push(key);
        }
    });
    if !listed {
        return Err(Front2Refusal::Unlisted { krate: 0 });
    }
    let mut found: Vec<(u8, LibKey, Box<str>), Global> = Vec::new_in(Global);
    for &key in fns.iter() {
        // A free fn: never a member of a trait or an impl.
        if names.member_owner(key).is_some() {
            continue;
        }
        let Some(it) = names.text_of(key) else { continue };
        let name = it.name.unwrap_or("_");
        // `symbols_of` reads a `#[no_mangle]` fn's export from the NAME it is given: the bare
        // name here, never `shown` (which appends the place for the TwoEntries message).
        // A module-level free fn: its own key holds its value rows.
        let export = symbols_of(names, key, key, it.text, name)?.export;
        let rank = match export.as_deref() {
            Some(HOSTED_START) => 0,
            Some("main") => 1,
            _ => continue,
        };
        // Never a foreign fn (an `extern` block's): asked only of an exported candidate, so
        // the program's other fns ask nothing more of the record.
        match names.extern_abi(key) {
            Some(None) => {}
            Some(Some(_)) => continue,
            None => return Err(Front2Refusal::Unplaced { function: Box::from(name) }),
        }
        let at = item_at(names, key);
        let shown = format!("{name}` (file {}, byte {}", at.file, at.place);
        found.push((rank, key, shown.into_boxed_str()));
    }
    // Exports declared inside bodies (super::nested_exports; the Reference, "Items": "Items
    // may be declared in a block", and `no_mangle` exports one wherever it is written): D104's
    // `bstd::main` expansion declares `bstd_hosted_start` in the process's `main` body.
    super::nested_exports::nested_entries(names, Alloc::global(), &mut found)?;
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
    /// `#[rustc_intrinsic]`: built by the rule of front2/intrinsic_rule.rs (the table's row,
    /// else its fallback body, else refused).
    intrinsic: bool,
    /// What the declaration says for that rule: a body, and `#[rustc_intrinsic_must_be_overridden]`.
    intrinsic_declared: super::intrinsic_rule::Declared,
    /// `#[rustc_nounwind]`: the fn never unwinds whatever its ABI (program/head_abi.rs).
    nounwind: bool,
    /// `#[track_caller]`: the fn takes its caller's `Location` as a hidden last parameter
    /// (the Reference, "The `track_caller` attribute").
    track_caller: bool,
    /// `#[inline]`, `#[inline(always)]`, `#[inline(never)]`, `#[cold]`.
    fn_attrs: blaze_lir::func::FnAttrs,
}

/// The identifier a signature's lowering declares, `r#` dropped: the symbol `#[no_mangle]`
/// gives a fn (the Reference, [abi.no_mangle]: "the symbol name for the function ... will be
/// the same as the item's name"), wherever the fn is declared, in a block too.
fn declared_identifier<'b>(v: &blaze_lower::View<'b>) -> Option<&'b str> {
    let n = v.head_opt(head::NAME)?;
    let name = v.name_text(blaze_lower::NameIdx(n)).ok()?;
    Some(name.strip_prefix("r#").unwrap_or(name))
}

/// `values_at`: the key whose value rows a `link_name`/`export_name` written as a macro call
/// reads (links.rs `values_key`: `lib` itself, or a body's foreign declaration's library twin).
fn symbols_of<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey, values_at: LibKey, text: &str, path: &str) -> Result<Symbols, Front2Refusal> {
    let refuse = |why: String| Front2Refusal::Attrs { function: path.into(), why: why.into_boxed_str() };
    let krate = names.crate_of(lib).ok_or_else(|| refuse(format!("no crate holds {:x}", lib)))?;
    let atoms = names.cfg_atoms(krate).ok_or_else(|| refuse(format!("crate {krate}'s cfg atoms are not read")))?;
    let edition = names.edition_of(krate).ok_or_else(|| refuse(format!("crate {krate}'s edition is not read")))?;
    let extensions = names.blaze_extensions(krate);
    let sig = blaze_lower::lower_signature_with_extensions(text, edition, extensions, &mut CrateCfg(atoms), Alloc::global())
        .map_err(|_| refuse(String::from("its signature needs a `cfg` the crate does not answer")))?;
    let v = sig.view();
    // A body (`{ .. }`, not `;`) is the signature lowering's own flag (blaze-lower machine.rs
    // `sig::HAS_BODY`), as the frozen fn head's qualifiers read by const_fn_fact.rs.
    let has_body = v.head[head::FLAGS] & blaze_lower::machine::sig::HAS_BODY != 0;
    let intrinsic_declared = super::intrinsic_rule::Declared { has_body, must_be_overridden: false };
    let mut out = Symbols { export: None, link_name: None, intrinsic: false, intrinsic_declared, nounwind: false, track_caller: false, fn_attrs: blaze_lir::func::FnAttrs::default() };
    for &n in v.rows(v.list(head::ATTRS)).map_err(|e| refuse(format!("{e:?}")))? {
        let a = v.attr(n).map_err(|e| refuse(format!("{e:?}")))?;
        // The cooked string, as the native image reads it (links.rs `symbol_of`): escapes
        // applied, a leading `\u{1}` kept as the exact-symbol mark, and a value written as a
        // macro call (ring's `concat!(prefix!(), stringify!(f))`) the names' row of the item,
        // expanded once by its crate's solve: the link table's symbol, over records too.
        let value = |attr: &'static str| {
            let t = v.text_at(a.args).map_err(|e| refuse(format!("{e:?}")))?;
            super::links::with_symbol_of(t, Some(values_at), |item| names.attr_value(item, attr), edition, |res| {
                res.map(Box::<str>::from).map_err(|why| refuse(format!("`#[{attr} = {t}]`: {why:?}")))
            })
        };
        match v.text_at(a.path).map_err(|e| refuse(format!("{e:?}")))?.trim() {
            // The fn's own identifier, from its signature; never the shown `path`, which for a
            // fn declared in a block is no name (check.rs `item_shown` prints its key):
            // hello-entry-shape, runner 9's "`ItemKey(..)`: it takes parameters".
            "no_mangle" if out.export.is_none() => {
                let name = declared_identifier(&v).ok_or_else(|| refuse(String::from("a `#[no_mangle]` fn whose signature's lowering holds no name")))?;
                out.export = Some(name.into());
            }
            "export_name" => out.export = Some(value("export_name")?),
            "link_name" => out.link_name = Some(value("link_name")?),
            "rustc_intrinsic" => out.intrinsic = true,
            super::intrinsic_rule::MUST_BE_OVERRIDDEN => out.intrinsic_declared.must_be_overridden = true,
            "rustc_nounwind" => out.nounwind = true,
            "track_caller" => out.track_caller = true,
            "cold" => out.fn_attrs.0 |= blaze_lir::func::FnAttrs::COLD,
            "inline" => {
                let args = v.text_at(a.args).map_or("", str::trim);
                let inner = args.strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or(args).trim();
                let mode = match inner {
                    "always" => blaze_lir::func::Inline::Always,
                    "never" => blaze_lir::func::Inline::Never,
                    _ => blaze_lir::func::Inline::Hint,
                };
                out.fn_attrs = out.fn_attrs.with_inline(mode);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Where `lib`'s text begins, as a row's `At`: an item with no text is at `{0, 0}` (owed, as
/// a struct's ctor is), and an expansion's text has no file of its own: its call's file is
/// owed (file 0).
pub(super) fn item_at<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey) -> At {
    match names.item_start(lib) {
        Some((blaze_scope::resolve::At::File(file), place)) => At { file, place },
        Some((blaze_scope::resolve::At::Expansion(_), place)) => At { file: 0, place },
        None => At { file: 0, place: 0 },
    }
}

/// Static `k`'s C symbol when an `extern` block declares it, the ONE rule the eager program
/// and the lazy one name a foreign static by (statics.rs `static_values`): a module-level
/// block's by its names row's `extern_abi` ([`head_abi::placed_fn`]), a block in a body's by
/// `Locals::foreign`; the symbol its `#[link_name]`, else its name, as a foreign fn's row takes
/// it (lazy/fill.rs `row`). The Reference, "External blocks", "Statics": it is declared with
/// no initializer, so it has no value to evaluate; its storage is the foreign object's.
/// `Ok(None)`: no block declares it. `Err`: why it could not be told, in words.
pub(super) fn foreign_static_symbol<'n, S: super::source::NamesSource<'n>>(names: S, keys: &super::keys::Keys, store: &Store<'n>, k: ItemKey) -> Result<Option<Box<str>>, Box<str>> {
    let said = |e: Front2Refusal| Box::<str>::from(format!("{e}"));
    // Whether an `extern` block declares it is asked BEFORE its head is read: a Rust static's
    // head is made by its evaluation (statics.rs `one_static`: "made by the path's resolution
    // or by the evaluation's check"), which runs after this, so a program that reads its
    // bodies (make_one.rs `FromRecord`) holds no head yet. Before (d106_records
    // `a_run_from_typed_records_makes_the_entries_a_second_check_makes`, 19:41): every Rust
    // static was refused here, "no static head in the store". A body's own block is its local
    // head's (`Locals::foreign`); any other is the names' answer of the item's library key.
    let foreign = match store.locals.foreign(k) {
        Some(_) => true,
        None if store.locals.head(k).is_some() => false,
        None => {
            let lib = keys.library_in(names, k).ok_or_else(|| Box::<str>::from("no library key: the static is no item the names hold"))?;
            let (_, block) = placed(names, lib).map_err(|_| Box::<str>::from("the names source did not answer whether an `extern` block declares it (`extern_abi`)"))?;
            block.is_some()
        }
    };
    if !foreign {
        return Ok(None);
    }
    let Some(head) = store.fns.get(&k) else {
        return Err(Box::from("no head of the foreign static in the store (heads.rs `Store::fns`)"));
    };
    let (lib, text) = (head.lib, head.text);
    let row_path = super::row_path::RowPath::of(names, keys, store, k).map_err(|no| said(Front2Refusal::NoRowPath(no)))?;
    let values_at = super::links::values_key(names, store, lib);
    let symbols = symbols_of(names, lib, values_at, text, row_path.text()).map_err(said)?;
    let declared = super::panic_entry::local_name(store, k).map_err(|why| Box::<str>::from(why.form()))?;
    Ok(Some(Box::from(symbols.link_name.as_deref().or(declared).unwrap_or(row_path.ident()))))
}

/// Where `lib`'s text begins ([`item_at`]) and the abi of the `extern` block declaring it
/// (`None`: none does). `Err`: the source did not answer whether a block declares it (its
/// error is in the source's log): a fn is never taken for a Rust fn by a missing answer.
pub(super) fn placed<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey) -> Result<(At, Option<&'n str>), Unplaced> {
    let abi = names.extern_abi(lib).ok_or(Unplaced { key: lib })?;
    Ok((item_at(names, lib), abi))
}

/// Why `normalize` of `trait_`'s associated type `name` at `args` gave no type (`out`), for the
/// seam's `last_projection`, the one printer of the eager and the lazy program: the trait,
/// `Self`'s form (an ADT by its path, else the node's kind: `FnDef`, `Opaque`, `Coroutine`..),
/// and the solver's whole error (at most 300 bytes), or that its step gave none.
pub(super) fn projection_why<P: crate::seam::FrontEnd>(program: &P, trait_: ItemKey, name: tc::Name, args: &[PTy], ground: &PortableTys<'_>, out: &Result<Option<PTy>, ResolveError>) -> Box<str> {
    let trait_shown = program.item_named(trait_).unwrap_or_else(|| format!("{:#x}", trait_.0).into_boxed_str());
    let self_shown = match args.first().map(|&a| ground.view().node(a)) {
        Some(Ok(tc::PNode::Adt(k, _))) => program.item_named(k).map_or_else(|| format!("ADT {:#x}", k.0), |p| format!("`{p}`")),
        Some(Ok(n)) => {
            let d = format!("{n:?}");
            String::from(&d[..d.find(['(', ' ', '{']).unwrap_or(d.len())])
        }
        Some(Err(_)) | None => String::from("(no Self)"),
    };
    let why = match out {
        Err(e) => {
            let mut d = format!("{e:?}");
            if d.len() > 300 {
                let mut end = 300;
                while !d.is_char_boundary(end) {
                    end -= 1;
                }
                d.truncate(end);
                d.push_str("..");
            }
            d
        }
        Ok(_) => String::from("the solver's step gave no type"),
    };
    format!("`{trait_shown}`'s associated type {:#x} at Self = {self_shown}: {why}", name.0).into_boxed_str()
}

/// The names source did not answer whether an `extern` block declares the item `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unplaced {
    pub key: LibKey,
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
            // `".."` and `b".."` alike (`string_bytes` reads both forms, as for expressions).
            tc::Lit::Str | tc::Lit::ByteStr(_) => {
                let bytes = string_bytes(text).ok_or_else(|| refuse("this escape in a string literal pattern"))?;
                out.str_pats.push((id, bytes.into_boxed_slice()));
            }
            // The builder asks no other literal pattern's value (a C string's, a `bool`'s).
            tc::Lit::CStr | tc::Lit::Bool(_) => {}
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

    fn legacy_const_generics(&self, key: ItemKey) -> &[u32] {
        self.store.fns.get(&key).map(|head| &head.legacy_const_generics[..]).unwrap_or(&[])
    }

    /// A tabled fn's row holds its ABI; any other fn's is read from its head (head_abi.rs):
    /// a fn left out for its body keeps its signature's ABI. `None` where it has no head or an
    /// ABI the builder spreads no arguments of: the builder's `NoAbi` names the fn, and a
    /// survey's left-out cause is printed beside it (run.rs `callee_refusal`).
    fn fn_abi(&self, key: ItemKey) -> Option<Abi> {
        // A call of an allocator declaration (`__rust_alloc`, "Rust", an `Alignment` argument)
        // is made with the ABI of the fn it runs (std's `__rdl_alloc`, "C", a `usize`):
        // blaze-build `Adts::abi`. Read from the declaration, the caller passed the address of
        // its `Alignment` where the default reads the value (std hello's trap, 2026-11-17).
        let key = super::alloc_shim::bound(self.front.names, &self.front.index, &self.store, key, self.store.tys.alloc()).unwrap_or(key);
        let tabled = self.by_key.get(&key).map(|&i| self.fns[i].abi);
        lazy::seam_rules::fn_abi(tabled, self.front.names, &self.store, key)
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
        let alloc = self.store.tys.alloc();
        if let Some(handler) = super::panic_entry::bound(self.front.names, &self.front.index, &self.store, item, alloc) {
            return Ok((handler, List::EMPTY));
        }
        // A call of one of alloc's `extern "Rust"` allocator declarations (`__rust_alloc` ..)
        // runs its default, std's `__rdl_*` fn, which takes no generic arguments
        // (front2/alloc_shim.rs).
        if let Some(default) = super::alloc_shim::bound(self.front.names, &self.front.index, &self.store, item, alloc) {
            return Ok((default, List::EMPTY));
        }
        // A call of any other `extern "Rust"` `#[rustc_std_internal_symbol]` declaration
        // (`__rust_start_panic`, `__rust_drop_panic` ..) runs its one definition in the graph,
        // the panic runtime's by the strategy (front2/internal_symbol.rs); no generic arguments.
        // The program's strategy, read from its inputs (crate 0's cfg, derived from
        // `UserCrate::panic`: crate::panic_strategy). An unreadable one was refused by name by
        // the reach walk at the declaration, so no call of it is built to come here.
        if let Ok(Some(def)) = lazy::seam_rules::internal_binding(self.front.names, &self.front.keys, &self.store, item, || Ok(())) {
            return Ok((def, List::EMPTY));
        }
        let r = self.in_world(i, |w| resolve_instance(w, item, args, ground, alloc))??;
        Ok((r.item, r.args))
    }

    fn holds(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        let alloc = self.store.tys.alloc();
        self.in_world(i, |w| blaze_typeck::holds(w, trait_, &[ty], ground, alloc))?
    }

    /// As the lazy program's: the graph's compiler_builtins exports (their rows were reached
    /// beside the roots, `make_from`).
    fn soft_float_routines(&self, alloc: Alloc<'_>) -> super::builtins_shim::Builtins {
        super::builtins_shim::find(self.front.names, self.front.index.scope(), alloc)
    }

    fn adt_drop_impl(&self, i: usize, drop_trait: ItemKey, adt: ItemKey, alloc: Alloc<'_>) -> Result<Option<ItemKey>, ResolveError> {
        self.in_world(i, |w| blaze_typeck::adt_drop_impl(w, drop_trait, adt, alloc))?
    }

    /// The fills the instance questions' worlds noted, made as the body check makes its own
    /// (check.rs `make_all`).
    fn make_asked(&mut self, i: usize) -> Result<bool, crate::run::RunError> {
        let fills = core::mem::replace(self.asked.get_mut(), Vec::new_in(Global));
        let mut gaps = core::mem::replace(self.asked_gaps.get_mut(), Vec::new_in(Global));
        let owner = self.fns[i].key;
        if fills.is_empty() {
            // As the lazy program's: a refused answer is the instance's refusal by name.
            if gaps.is_empty() {
                return Ok(false);
            }
            gaps.sort_unstable_by_key(|g| format!("{g:?}"));
            gaps.dedup_by_key(|g| format!("{g:?}"));
            return Err(crate::run::RunError::Front2(Front2Refusal::Asked { function: self.fns[i].path.clone(), error: CheckFail::Gap { owner, gaps } }));
        }
        // With the evaluator the program keeps, as the lazy program's `make_asked` (its
        // comment says why: a head these fills make may name a constant).
        let mut records = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: Some((&mut self.consts, self.prepared)),
        };
        super::check::make_all(&mut records, fills, owner)
            .map_err(|error| crate::run::RunError::Front2(Front2Refusal::Asked { function: self.fns[i].path.clone(), error }))?;
        Ok(true)
    }

    /// Every tabled fn's head is in the store (`new` refuses one without).
    fn sig(&self, i: usize) -> FnSig<'_> {
        let key = self.fns[i].key;
        // A ctor: a safe Rust fn from its fields to its type, generic over the type's params.
        if let Some(c) = self.store.ctors.get(&key) {
            let header = tc::FnPtrHeader::RUST;
            let (inputs, output, types) = (&c.inputs[..], c.output, self.tys.view());
            return FnSig { parent_count: 0, own_count: c.generics, declared_count: c.generics, self_param: false, header, c_variadic: false, legacy_const_generics: &[], inputs, output, types };
        }
        let f = &self.store.fns[&key];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            declared_count: f.declared,
            self_param: f.self_param,
            header: f.header,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs: &f.inputs,
            output: self.store.fn_output(key).unwrap_or(f.output),
            types: self.tys.view(),
        }
    }

    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.by_key.get(&key).copied()
    }

    fn specialized_row(&self, i: usize, args: &[PTy], ground: PTys<'_>) -> Option<usize> {
        if self.inline_for_specs.is_empty() {
            return None;
        }
        let orig = self.fns.get(i)?.key;
        let alloc = self.tys.alloc();
        for s in self.inline_for_specs.iter() {
            if s.orig == orig && super::inline_for::args_equal(ground, args, self.tys.view(), &s.args, alloc) {
                return Some(s.row);
            }
        }
        None
    }

    fn specialize_inline_for(&mut self, i: usize, args: &[PTy], ground: PTys<'_>) -> Result<Option<(usize, Option<Checked<'static>>)>, crate::run::RunError> {
        let Some(row) = self.specialized_row(i, args, ground) else {
            return Ok(None);
        };
        Ok(Some((row, self.fns.get_mut(row).and_then(|f| f.checked.take()))))
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

    /// D106: fn `key`'s body, checked in `new` (the whole reach, today), handed over once: the
    /// record walk's maker asks each key once a run. A stage: the lazy program (piece Y) checks
    /// only this body here, and this table goes when it is tested equal.
    fn check_one(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, crate::run::RunError> {
        let Some(i) = self.by_key.get(&key).copied() else { return Ok(None) };
        Ok(self.fns[i].checked.take().map(|done| (i, done)))
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
        self.front.index.lang(name).map(|lib| super::keys::asked_key(self.front.names, lib))
    }

    /// The Rust start the program was made with (start.rs [`super::start::Entry2::Rust`]);
    /// `None` for every other start, and for a test unit's suite (no start).
    fn rust_start(&self) -> Option<&super::start::RustStart> {
        match &self.start {
            Some(super::start::Entry2::Rust(s)) => Some(s),
            _ => None,
        }
    }

    /// From the rows made with each body's check (coroutine_recipes.rs).
    fn coroutine_recipe(
        &self,
        owner: ItemKey,
        ordinal: u32,
    ) -> Option<Result<&blaze_flow_lower::state_recipe::StateRecipe<Global>, blaze_flow_lower::Refusal>> {
        super::coroutine_recipes::find(&self.recipes, owner, ordinal)
    }

    /// From the head store's `opaques` ([`super::heads::Store::opaques`]).
    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.store.opaques.get(&opaque).copied()
    }

    /// [`super::tait::defining_fn`] of the alias's library key.
    fn alias_definer(&self, alias: ItemKey) -> Result<ItemKey, super::tait::DefinerMiss> {
        let names = self.front.names;
        let lib = self.front.keys.library_in(names, alias).ok_or(super::tait::DefinerMiss::NoLibraryKey)?;
        super::tait::defining_fn(&self.definers, names, lib).map(super::keys::checker_of)
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let o = self.objects.get(&t)?;
        let tys = o.super_tys.as_ref().map_or(PTys::EMPTY, PortableTys::view);
        Some(blaze_build::vtable::ObjectTrait { supers: &o.supers, tys, methods: &o.methods, shape: o.shape })
    }

    /// The fn `name` of the lang trait `trait_` (`Deref::deref`), filed with its trait's head,
    /// falling back to the library's member row when the trait's head has not been made yet
    /// (matching `LazyProgram::lang_trait_fn`).
    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        let tr = self.made_lang(trait_)?;
        if let Some(filed) = self.front.keys.filed_member(tr, MemberKind::Fn, name) {
            return Some(filed);
        }
        let names = self.front.names;
        let lib = self.front.keys.library_in(names, tr)?;
        let mut declared = false;
        super::source::NamesSource::each_member(names, lib, &mut |m| declared |= m.kind == blaze_surface::ItemKind::Function && m.name == Some(name));
        declared.then(|| super::keys::member_key_of(tr, MemberKind::Fn, name))
    }

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let a = self.store.adts.get(&key)?;
        Some(Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.tys.view() })
    }

    fn left_out(&self, key: ItemKey) -> Option<&str> {
        self.left.get(&key).map(|why| &**why)
    }

    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        Some(self)
    }

    /// The item's path in the names library (`crate::module::Name`).
    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
        // A member (a trait's fn: what a resolve refusal names) by its container's path.
        let mut cur = key;
        let mut members = [""; 64];
        let mut count = 0usize;
        while let Some(super::keys::Origin::Member { container, name, .. }) = self.front.keys.origin(cur) {
            let Some(slot) = members.get_mut(count) else {
                return None;
            };
            *slot = name;
            count += 1;
            cur = *container;
        }
        let (names, lib) = (self.front.names, self.front.keys.library_in(self.front.names, cur)?);
        let name = names.item_text(lib)?.name?;
        let module = names.krate(names.crate_of(lib)?).path_of(names.module_of(lib)?)?;
        let mut out = format!("{module}::{name}");
        if let Some(slice) = members.get(..count) {
            for &member in slice.iter().rev() {
                out.push_str("::");
                out.push_str(member);
            }
        }
        Some(out.into_boxed_str())
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

    /// The constructor's answer the walk's ask left in the impl index (fmt_template.rs
    /// `argument_new_answered`, the one reader both programs share); none asked, or not one
    /// fn, is no answer (the builder refuses naming the constructor).
    fn fmt_argument_new(&self, trait_: tc::FmtTrait) -> Option<ItemKey> {
        super::fmt_template::argument_new_answered(&self.front.index, self.buckets.as_ref(), trait_)
    }

    fn float_lit(&self, owner: ItemKey, e: ExprId) -> Option<(u64, u32)> {
        let s = &self.fns[self.fn_index(owner)?].lits.floats;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn float_pat(&self, owner: ItemKey, p: PatId) -> Option<(u64, u32)> {
        let s = &self.fns[self.fn_index(owner)?].lits.float_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&[u8]> {
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
        let alloc = self.store.tys.alloc();
        let out = self.in_world(i, |w| blaze_typeck::normalize_projection(w, trait_, name, args, ground, alloc)).and_then(|o| o);
        if !matches!(out, Ok(Some(_))) {
            *self.projection_why.borrow_mut() = Some(projection_why(self, trait_, name, args, ground, &out));
        }
        out
    }

    fn const_record(&self, item: ItemKey) -> Option<&DataRecord<Global>> {
        let at = self.const_values.binary_search_by_key(&item, |(k, _)| *k).ok()?;
        Some(&self.const_values[at].1)
    }

    /// `need` at ground `args` (types of `from`) by the evaluator the program kept: a constant
    /// read by path (`Consts::value_with_args`: a trait's constant selected at `args[0]`), or
    /// a const block or a promoted borrow of a body (`Consts::body_value`, under its key over
    /// the owner's text: `ConstNeed::in_body`). The evaluator checks any body and makes any
    /// impl row the value needs.
    fn const_at(&mut self, need: super::const_at::ConstNeed, from: tc::PTys<'_>, args: &[tc::PTy], closures: &crate::run::ClosureTable) -> Result<DataRecord<Global>, super::const_at::ConstAtWhy> {
        let alloc = self.store.tys.alloc();
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
            super::const_at::ConstNeed::Item(item) => self.consts.value_with_args(&mut records, self.prepared, item, from, args, Some(closures), alloc),
            super::const_at::ConstNeed::Block { .. } | super::const_at::ConstNeed::Promoted { .. } => {
                let owner = need.item();
                let in_body = records.store.fns.get(&owner).and_then(|h| need.in_body(h.text));
                let Some((owner, root, key)) = in_body else {
                    return Err(super::const_at::ConstAtWhy::Eval(super::consts::ConstFail::Body { item: owner, error: super::body::BodyError::NoHead(owner) }));
                };
                self.consts.body_value(&mut records, self.prepared, owner, root, key, from, args, Some(closures), alloc)
            }
        };
        made.map(super::const_at::data_record).map_err(super::const_at::ConstAtWhy::Eval)
    }

    /// From the evaluator the program kept: the record that holds the fn word names its
    /// instance (consts.rs `Consts::fn_instance`).
    fn const_fn_instance(&self, key: blaze_build::InstanceKey, ground: &mut tc::PortableTys<'_>, scratch: Alloc<'_>) -> Option<blaze_build::Instance> {
        self.consts.fn_instance(key, ground, scratch)
    }

    fn evaluates_at_args(&self) -> bool {
        true
    }

    fn last_projection(&self) -> Option<Box<str>> {
        self.projection_why.borrow_mut().take()
    }

    /// The row's `#[repr(..)]`; no row (as `adt`'s `None`): `Missing`, never `repr(Rust)`.
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, blaze_build::layout::Missing> {
        self.store.adts.get(&key).map(|a| a.repr).ok_or(blaze_build::layout::Missing { item: key })
    }

    /// The row's `rustc_layout_scalar_valid_range_*` bounds (heads.rs `AdtRow::valid`); no row:
    /// `Missing`, as [`Self::repr`].
    fn valid_range(&self, key: ItemKey) -> Result<blaze_build::layout::ValidRange, blaze_build::layout::Missing> {
        self.store.adts.get(&key).map(|a| a.valid).ok_or(blaze_build::layout::Missing { item: key })
    }

    /// A `#[rustc_intrinsic]` fn's table entry, read from its name when it was tabled.
    fn intrinsic(&self, key: ItemKey) -> Option<Intrinsic> {
        self.fns.get(*self.by_key.get(&key)?)?.intrinsic
    }

    /// From the fn's row: its written ABI's unwind category or `#[rustc_nounwind]`
    /// (program/head_abi.rs); `None` for a fn not tabled (its attributes are not read, so no
    /// answer is guessed from its ABI alone).
    fn nounwind(&self, key: ItemKey) -> Option<bool> {
        Some(self.fns.get(*self.by_key.get(&key)?)?.nounwind)
    }

    fn fn_attrs(&self, key: ItemKey) -> blaze_lir::func::FnAttrs {
        self.by_key.get(&key).and_then(|&i| self.fns.get(i)).map_or_else(blaze_lir::func::FnAttrs::default, |r| r.fn_attrs)
    }

    /// From the fn's row; a trait impl's method that does not say it itself answers with the
    /// row of its trait's declaration of the method of its name (`None` when that row is not
    /// tabled: never guessed `false`).
    fn track_caller(&self, key: ItemKey) -> Option<bool> {
        lazy::seam_rules::track_caller_from(self.front.names, &self.front.keys, &self.store, key, |k| Some(self.fns.get(*self.by_key.get(&k)?)?.track_caller))
    }

    /// From `owner`'s lowered body (`View::site`: item-relative, or the outermost macro call's
    /// range) and the solved file the item is written in, by its shown name (seam_rules.rs).
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        let not_live = blaze_build::SiteGap::NotLive { owner };
        let row = self.by_key.get(&owner).and_then(|&i| self.fns.get(i)).ok_or(not_live)?;
        let body = row.body.as_ref().ok_or(not_live)?;
        let anchor_owner = body.param_subst.as_ref().map_or(owner, |(orig, _)| *orig);
        lazy::seam_rules::caller_site(self.front.names, &self.front.keys, &self.store, anchor_owner, body, expr)
    }

    /// By seam_rules.rs `def_site`, from the item's head and the names source.
    fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        let anchor = self
            .inline_for_specs
            .iter()
            .find(|s| self.fns.get(s.row).is_some_and(|r| r.key == item))
            .map_or(item, |s| s.orig);
        lazy::seam_rules::def_site(self.front.names, &self.front.keys, &self.store, anchor)
    }

    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        let row = self.fns.get(*self.by_key.get(&owner)?)?;
        lazy::seam_rules::expr_offset(row.body.as_ref()?, expr)
    }

    /// From the item's present first byte in today's text of its file (`NamesSource::item_start`,
    /// item_grain.rs) and that text's line starts: an edit above the item moves the answer and
    /// no record (seam_rules.rs).
    fn source_position(&self, item: ItemKey, site: u32) -> Result<(u32, u32), crate::seam::PositionGap> {
        lazy::seam_rules::source_position(self.front.names, &self.front.keys, &self.store, item, site)
    }

    /// A static the program reaches: its head's mutability and `#[thread_local]` (statics.rs).
    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        self.statics.kind(key)
    }

    fn foreign_static(&self, key: ItemKey) -> Option<&str> {
        self.statics.foreign(key)
    }

    fn static_records(&self) -> &[(ItemKey, DataRecord<Global>)] {
        &self.statics.values
    }

    /// A tuple struct's or variant's ctor: its ADT and the variant's index there.
    /// From the ADT's row: every variant's value, explicit or implicit (heads.rs `discrs`).
    /// No row: `Missing`, never `None` (which numbers the variants by index).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        let row = self.store.adts.get(&adt).ok_or(blaze_build::layout::Missing { item: adt })?;
        Ok(row.discrs.get(variant as usize).copied())
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

impl blaze_build::type_name::TypeNames for Front2Program<'_> {
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool {
        super::type_name::item_path(self.front.names, &self.front.keys, &self.store, item, out)
    }
}
