//! The checker's `World` over the [`Store`] for one body (step 3): every answer is read from
//! the store, the names library or the program's [`Index`]; a record the store does not hold
//! yet is `Missing`, and the [`Fill`] that makes it is noted for the check loop
//! ([`super::check`]), which makes it and checks the body again. A question this front end
//! cannot answer yet is a [`Gap`] naming the item: noted, answered `Missing`, and the loop
//! stops with it (never answered wrongly).
//!
//! Paths and traits in scope are in [`super::world_paths`]; this file is the item records.

use core::cell::RefCell;
use std::alloc::Global;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_scope::resolve::{At, Names};
use blaze_surface::ItemDetail;
use blaze_typeck as tc;
use blaze_typeck::{
    AssocItem, AssocKind, AssocParent, Def, Fetch, FnSig, Hidden, ImplHeader, ItemKey, LangItem, Missing, Name, PTy, PTys, PathIdx,
    Predicates, ScopeId, Seal, Shape, TraitDef, TypeAttrs, World,
};

use super::buckets::{Buckets, Seen, Unseen};
use super::fill::FillError;
use super::heads::{Seeded, Store, Want};
use super::index::Index;
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::langs::{lang_name, trait_lang};
use super::spell::{Spelling, Unspelled};
use super::types::SelfTy;
use super::source::NamesSource;
use crate::resolve::{Paths, ResolveError};

/// How many heads one question of a seeded body's world imports at most before its answer
/// ([`BodyWorld::again`]; rule 18).
pub const MOST_IMPORTS_A_QUESTION: usize = 256;

/// What the check loop makes before it checks the body again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Store(Want),
    /// The headers of the impls that can answer a trait at a shape, then the answer: of the
    /// impls the asker sees (buckets.rs [`Seen`]: a checked body's crate's link closure).
    Impls(ItemKey, Shape, Seen),
    /// The headers of the inherent impls of a shape that declare an item of the name, then
    /// the shape's items of that name. The name filters first, as rustc finds an inherent item
    /// by name before it unifies self types: `impl Uint<{ <U1792>::LIMBS / 2 }> { fn concat }`
    /// is no candidate of `LIMBS`, so evaluating its header's constant never needs its own header.
    Inherent(Shape, Name),
    /// A callee whose return signature contains `_` (`-> _`, `-> Option<_>`, `-> Result<T, _>`)
    /// and whose body must be checked first to infer its concrete return type.
    InferredOutput(ItemKey),
}

/// A question this front end cannot answer yet, by the item it is about.
#[derive(Clone, Debug)]
pub enum Gap {
    /// The item has a `where const` condition: conditions are collected, not yet clauses.
    WhereConst(ItemKey),
    /// A constructor fn's key that no variant of its type's made head carries (or a variant in
    /// no enum): the head and the key disagree.
    CtorFn(ItemKey),
    /// A body path through a built-in type (`u32::MAX`): the checker's `Def` has no primitive
    /// type for a type-relative path to start from (the type checker's lane).
    BuiltinTypePath(Box<str>),
    /// A lang item's path the world cannot answer, and why.
    Lang(tc::LangItem, &'static str),
    /// A const's value, an opaque type's hidden type, a `where const` condition's value.
    BodyRecord(tc::Question),
    /// A type alias named in a body path whose aliased type is no path `Def` (an ADT with
    /// arguments other than the alias's own parameters in order: `type A<T> = B<T, X>`): the
    /// checker's `Def` has no form for a type with its arguments fixed.
    TypeAlias(LibKey),
    /// A type alias named in a body path whose head was refused: the alias and why.
    AliasHead(LibKey, Box<str>),
    /// An associated type of an impl, asked as an associated item (its key is not made yet).
    ImplAssocType(ItemKey, Name),
    /// A const term of the body `owner` (a const generic argument, a repeat length) that its
    /// conversion left a hole (blaze-lower `TypeckBody::const_gaps`): why, and where in the
    /// item's text the constant is written. The body was checked with a const variable
    /// there, which unifies with any value: its check is no answer.
    BodyConst { owner: ItemKey, gap: blaze_lower::typeck::ConstGap },
    /// A name the body's lowering holds no text for.
    Unspelled(Unspelled),
    /// The names library named an item with no text (a gap in the library, not the program).
    Resolve(ResolveError),
    /// The key map holds no row for the key.
    Fill(FillError),
    /// A lang variant in no enum of its crate.
    VariantOfNoEnum(LibKey),
    /// An auto trait of an opaque whose hidden type could not be given
    /// ([`super::opaque_auto::HiddenFail`]: its defining body's refusal, or the E0391 cycle).
    Hidden(super::opaque_auto::HiddenFail),
    /// A reveal requested the opaque's row, but the producer has supplied no hidden type.
    HiddenRow { opaque: ItemKey, owner: ItemKey, ordinal: u32, path: Box<str> },
    /// The producer's filed row gives no facts of this coroutine to this reader: `why`.
    CoroutineRow { reader: ItemKey, owner: ItemKey, ordinal: u32, path: Box<str>, why: &'static str },
    /// An impl whose trait path the names do not resolve as written (so it was a candidate of
    /// every question) and whose header could not be made: the header's error.
    UnreadImpl(LibKey, Box<str>),
    /// A candidate impl of the question asked (its trait and self type's shape were read: it
    /// may answer it) whose header was refused: the impl, its place (`file:line`) and why. The
    /// answer without it could be wrong, so the question is this gap.
    RefusedImpl { impl_: LibKey, place: Box<str>, why: Box<str> },
    /// A member fn whose signature could not be made (its container's head is made without
    /// it): the signature's error. Only a body that uses it stops on it.
    FnHead(ItemKey, Box<str>),
    /// A body names an item declared in a block whose form has no record here yet.
    Local(super::locals::LocalGap),
    /// A closure of ANOTHER body with no answer from its owner's checked record
    /// ([`super::foreign_closures`]): the owner, the ordinal, and the text naming the owner
    /// by its path and why.
    ForeignClosure { owner: ItemKey, ordinal: u32, text: Box<str> },
    /// A question refused by what its key is ([`super::by_origin::KindGap`]): a made want with
    /// no answer, a variant index past its ADT's, an unfiled anonymous constant, ..
    Kind(super::by_origin::KindGap),
    /// The body path at `path` (its index in the checked body): `why` its mark gives it no
    /// site (blaze-lower `mark_site::SiteRefused`), or `None`: the world holds no mark row
    /// for it (made without the body's `path_marks`).
    Site { path: u32, why: Option<blaze_lower::mark_site::SiteRefused> },
    /// A base-less struct expression (`S { a: 1, .. }`) of an ADT whose head row was filed
    /// without its field defaults (every maker files both: heads.rs `Store::adt`, and a head
    /// record carries them since form 8, head_record.rs).
    FieldDefaults(ItemKey),
    /// The crates whose impls a checked body sees were not told (buckets.rs [`Unseen`]).
    Sight(Unseen),
    /// Whether the body may name member `name` of inherent impl `impl_` could not be told
    /// (`World::member_visible`): why (a `pub(in path)` in the impl's crate, a module chain
    /// the names do not hold, no frame for the impl).
    MemberVisibility { impl_: ItemKey, name: Name, why: &'static str },
    /// Whether member `name` of inherent impl `impl_` is unstable under a feature the body's
    /// crate does not turn on could not be told (`World::member_unstable`): why.
    MemberStability { impl_: ItemKey, name: Name, why: &'static str },
    /// An import through the one ask (`AsksMet::Once`, front2 asks.rs) was refused: the need
    /// and why.
    Ask(super::asks::AskRefused),
    /// Mutual recursion cycle while inferring a return-type `_` placeholder (`-> _`, `Result<T, _>`):
    /// add an explicit return/error type to break the cycle.
    InferredOutputCycle { owner: ItemKey, callee: ItemKey },
    /// A callee's inferred return/error type `_` could not be resolved from its body.
    InferredOutputFailed { item: ItemKey, why: &'static str },
}

impl core::fmt::Display for Gap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Gap::WhereConst(k) => write!(f, "item {k:?} has a `where const` condition, not yet a checker clause"),
            Gap::CtorFn(k) => write!(f, "the constructor fn {k:?} is in no variant of its type's head"),
            Gap::Lang(item, why) => write!(f, "the path of lang item {item:?}: {why}"),
            Gap::BuiltinTypePath(p) => write!(f, "the body path `{p}` starts at a built-in type; the checker's `Def` has no primitive type to resolve it on"),
            Gap::BodyRecord(q) => write!(f, "the body record {q:?} (a const's value, a hidden type, a condition) is not made yet"),
            Gap::TypeAlias(k) => write!(f, "the type alias {k:x} named in a body path aliases a type with fixed arguments; the checker's `Def` has no form for it"),
            Gap::AliasHead(k, why) => write!(f, "the type alias {k:x} named in a body path has a refused head: {why}"),
            Gap::ImplAssocType(k, n) => write!(f, "the associated type {n:?} of impl {k:?} has no key yet"),
            Gap::BodyConst { owner, gap } => {
                use blaze_lower::typeck::{ConstGapWhy, ConstSite};
                let site = match gap.site {
                    ConstSite::Arg => "const argument",
                    ConstSite::RepeatLen => "array repeat length",
                    ConstSite::LegacyArg => "legacy const generic argument",
                };
                let why = match gap.why {
                    ConstGapWhy::NoEvaluator => "its converter evaluates no constant",
                    ConstGapWhy::OverGeneric => "a constant expression over a generic parameter (the checker's type form has no node for an unevaluated expression)",
                    ConstGapWhy::AnonInBody(_) => "an anonymous constant whose type is unknown in this body",
                    ConstGapWhy::NegativeNoSuffix => "a negated literal with no suffix (its bits wait for the parameter's type)",
                };
                write!(f, "the {site} at {}..{} of the body {owner:?} has no value yet: {why}", gap.range.start, gap.range.end)?;
                match gap.why {
                    ConstGapWhy::AnonInBody(text) if !text.text().is_empty() => write!(f, ": `{text}`"),
                    _ => Ok(()),
                }
            }
            Gap::Unspelled(u) => write!(f, "the body names {:?}, whose text its lowering does not hold", u.0),
            Gap::Resolve(e) => write!(f, "{e}"),
            Gap::Fill(e) => write!(f, "{e}"),
            Gap::VariantOfNoEnum(k) => write!(f, "the lang variant {k:x} is in no enum of its crate"),
            Gap::Hidden(super::opaque_auto::HiddenFail::NoHead { opaque }) => write!(f, "the opaque type {opaque:?} has no defining-owner row in the loaded heads"),
            Gap::Hidden(h) => write!(f, "{h}"),
            Gap::HiddenRow { opaque, owner, ordinal, path } => write!(f, "the hidden-type row for return opaque #{ordinal} ({opaque:?}) of {path} ({owner:?}) is unavailable"),
            Gap::CoroutineRow { reader, owner, ordinal, path, why } => write!(
                f, "item {reader:?} needs the checked output and stored types of coroutine #{ordinal} of {path} ({owner:?}): {why}: {}", tc::EXTERNAL_WITNESS_GAP
            ),
            Gap::FnHead(k, why) => write!(f, "the member fn {k:?} has no signature: {why}"),
            Gap::Local(g) => write!(f, "{g}"),
            Gap::UnreadImpl(k, why) => write!(f, "the impl {k:x}, a candidate filed without its trait or self type (unread as written), has no header: {why}"),
            Gap::RefusedImpl { impl_, place, why } => write!(f, "the impl {impl_:x} at {place}, a candidate of this question, has a refused header: {why}"),
            Gap::ForeignClosure { text, .. } => write!(f, "{text}"),
            Gap::Kind(g) => write!(f, "{g}"),
            Gap::Site { path, why: Some(why) } => write!(f, "the body path {path}: {why}"),
            Gap::Site { path, why: None } => write!(f, "the body path {path} has no mark row in the world (made without the body's `path_marks`)"),
            Gap::FieldDefaults(k) => write!(f, "the field defaults of ADT {k:?}, read by a base-less `S {{ a: 1, .. }}`: its head row was filed without them"),
            Gap::Sight(why) => write!(f, "{why}"),
            Gap::MemberVisibility { impl_, name, why } => {
                write!(f, "whether the body may name the member {name:?} of inherent impl {impl_:?} (the Reference, \"Visibility and privacy\"): {why}")
            }
            Gap::MemberStability { impl_, name, why } => {
                write!(f, "whether the member {name:?} of inherent impl {impl_:?} is unstable under a feature this crate does not turn on (rustc's `eval_stability`): {why}")
            }
            Gap::Ask(refused) => write!(f, "{refused}"),
            Gap::InferredOutputCycle { owner, callee } => write!(
                f,
                "cycle (E0391) inferring return/error type `_` between {owner:?} and {callee:?}; add an explicit return or error type to break the cycle"
            ),
            Gap::InferredOutputFailed { item, why } => write!(
                f,
                "could not infer return/error type `_` for {item:?}: {why}"
            ),
        }
    }
}

/// Where item `key` is written, as `file:line`: an item a macro made is placed at its call (at
/// most 16 expansions out); "(no file place)" where the names library holds no place.
pub fn item_place(names: &Names<'_>, key: LibKey) -> Box<str> {
    let Some(krate) = names.crate_of(key) else { return "(no crate)".into() };
    let cn = names.krate(krate);
    let mut k = key;
    let mut text = names.item_text(k).map_or(0, |t| t.text.as_ptr() as usize);
    for _ in 0..16 {
        let Some(p) = names.place(k) else { break };
        let Some((base, _)) = cn.outline_at(p.at) else { break };
        let b = base.as_ptr() as usize;
        let off = if text >= b && text <= b + base.len() { text - b } else { 0 };
        match p.at {
            At::File(f) => {
                let line = base.as_bytes()[..off].iter().filter(|c| **c == b'\n').count() + 1;
                let path = cn.files.get(f as usize).map_or("?", |s| s.path);
                return format!("{path}:{line}").into_boxed_str();
            }
            At::Expansion(call) => {
                k = call;
                text = names.item_text(call).map_or(0, |t| t.text.as_ptr() as usize);
            }
        }
    }
    "(no file place)".into()
}

/// The world one body is checked in, its names from the source `S` ([`NamesSource`]: the
/// solved names by default, or each crate's records).
pub struct BodyWorld<'w, 'n, S = &'n Names<'n>> {
    pub names: S,
    pub keys: &'w Keys,
    pub index: &'w Index<'n>,
    pub store: &'w Store<'n>,
    /// `None` until the loop made them.
    pub buckets: Option<&'w Buckets>,
    /// The body's checker key: whose local items its blocks hold ([`super::locals`]).
    pub owner: ItemKey,
    /// The body's module, its generic parameters by name and its `Self`.
    pub module: LibKey,
    pub params: &'w [(Name, u32)],
    pub self_: &'w SelfTy,
    pub spell: &'w Spelling<'w>,
    /// The traits in scope at the body's module, sorted (checker keys).
    pub traits: &'w [ItemKey],
    /// The traits in scope at the body's blocks whose list is not `traits` (a block's `use`).
    pub blocks: &'w super::block_traits::BlockTraits,
    /// Parallel to the body's paths (blaze-lower `TypeckBody::path_marks`): where each is
    /// read ([`Self::path`], by `blaze_lower::mark_site::site_of`). Empty for a world that
    /// resolves no path (a projection's, a selection's): a path asked there is a gap.
    pub path_marks: &'w [Option<blaze_lower::mark_site::TextMark>],
    /// Parallel to the same paths: the namespace each is resolved in (blaze-lower
    /// `TypeckBody::path_ns`; rustc `PathSource::namespace`). Empty where `path_marks` is.
    pub path_ns: &'w [blaze_lower::typeck::PathNs],
    pub fills: RefCell<Vec<Fill, Global>>,
    pub gaps: RefCell<Vec<Gap, Global>>,
    /// Which opaques this world answers the hidden type of: a body's check sees only its leak
    /// list's, a resolution at ground arguments after checking sees every one.
    pub reveal: super::opaque_auto::Reveal,
}

/// `f` over the world of the ctor `ctor`, a fn row with no body: its types are its ADT's, so
/// the questions the back half asks while building it (the ADT's `Drop` impl and layout facts,
/// a selection or a projection at ground arguments, a callee's instance) are asked in the ADT's
/// own scope. No path is resolved there (no body has paths), no parameter is in scope (every
/// question is at ground arguments) and every opaque is its hidden type, as in a built body's
/// world after its check. A key that is no ctor, or whose ADT has no library key, is the
/// world's own refusal naming the fn: no world is made for it.
///
/// What the questions found unmade (`fills`) or refused (`gaps`) is handed to the program's
/// `asked` and `asked_gaps`, as a body's world hands its own (program.rs `in_world`). Before,
/// this world was dropped with them: a ctor's `Missing` (an ADT's `Drop` bucket no check made)
/// reached `make_asked` with nothing to make, and the instance was refused by the bare "the
/// world has no answer to ..", which no round could fill (rule 3: nothing quietly dropped).
#[allow(clippy::too_many_arguments)]
pub(super) fn ctor_world<'n, S: NamesSource<'n>, T>(
    names: S,
    keys: &Keys,
    index: &Index<'n>,
    store: &Store<'n>,
    buckets: Option<&Buckets>,
    ctor: ItemKey,
    asked: &RefCell<Vec<Fill, Global>>,
    asked_gaps: &RefCell<Vec<Gap, Global>>,
    f: impl FnOnce(&BodyWorld<'_, 'n, S>) -> T,
) -> Result<T, tc::ResolveError> {
    let refused = tc::ResolveError::Missing(tc::Question::FnSig(ctor));
    // A ctor's world is its ADT's. The other bodiless row that builds calls is a trait's method
    // tabled for a function the language gives (the lang `Clone::clone` at a tuple or a
    // closure, clone_shim.rs `language_clone`: each field's `clone` is resolved here): its
    // world is its trait's. Every question it asks is at ground arguments (`Reveal::All`, the
    // whole program's impls: coherence makes the answer the same in every world).
    // A fn row with a head and no live body: the world of its own module, for the same reason
    // (every question at ground arguments). Asked only by a found code entry's read list asked
    // again before any body is checked (run/make_one.rs `verify_instance`: its `Resolve`,
    // `DropImpl` and `Normalize` rows); a make checks its fn first, so its world is the body's.
    let at = match store.ctors.get(&ctor) {
        Some(c) => c.adt,
        None => match keys.origin(ctor) {
            Some(super::keys::Origin::Member { container, .. }) if store.trait_defs.contains_key(container) => *container,
            // An impl's fn: the impl's module, as its library item.
            Some(super::keys::Origin::Member { container, .. }) if store.fns.contains_key(&ctor) => *container,
            _ if store.fns.contains_key(&ctor) => ctor,
            _ => return Err(refused),
        },
    };
    let Some(module) = keys.library_in(names, at) else { return Err(refused) };
    let spell = Spelling::fixed();
    let blocks = super::block_traits::BlockTraits::module(store.tys.alloc());
    let self_ = SelfTy::None;
    let world = BodyWorld {
        names,
        keys,
        index,
        store,
        buckets,
        owner: ctor,
        module,
        params: &[],
        self_: &self_,
        spell: &spell,
        traits: &[],
        blocks: &blocks,
        path_marks: &[],
        path_ns: &[],
        fills: RefCell::new(Vec::new_in(Global)),
        gaps: RefCell::new(Vec::new_in(Global)),
        reveal: super::opaque_auto::Reveal::All,
    };
    let out = f(&world);
    asked.borrow_mut().extend(world.fills.into_inner());
    asked_gaps.borrow_mut().extend(world.gaps.into_inner());
    Ok(out)
}

impl<'w, 'n, S: NamesSource<'n>> BodyWorld<'w, 'n, S> {
    pub(super) fn resolver(&self) -> Paths<S> {
        Paths { names: self.names }
    }

    /// Where the impl `k` is, for a gap's text: over the solved names its file and line
    /// ([`item_place`]); over records its module's path (no text is opened to count lines).
    fn place_shown(&self, k: LibKey) -> Box<str> {
        match self.names.solved() {
            Some(solved) => item_place(solved, k),
            None => match self.names.home_of(k).and_then(|m| self.names.module_path(m)) {
                Some(path) => format!("in {path}").into_boxed_str(),
                None => "(no place in the records)".into(),
            },
        }
    }

    pub(super) fn fill<T>(&self, f: Fill) -> Fetch<T> {
        self.fills.borrow_mut().push(f);
        Err(Missing)
    }

    pub(super) fn gap<T>(&self, g: Gap) -> Fetch<T> {
        self.gaps.borrow_mut().push(g);
        Err(Missing)
    }

    /// Want `w` made, the one seam between the two ways a body's check meets a head it does
    /// not hold (rule 78; the cost test runs both and asserts equal answers and read lists):
    /// - IMPORT NOW: over a seeded store (`Store::seeds`, a body task's frozen heads) `w`'s
    ///   cell is filed from its frozen record through shared borrows, the same `HeadSeed::seed`
    ///   `Store::ensure` calls between rounds, and the answer is `Missing` with no fill:
    ///   [`Self::again`] asks the question again at once, and the round goes on;
    /// - NEXT ROUND: over a lowering store (no `seeds`), the fill `Store::ensure` makes
    ///   between rounds (`check.rs` `make_round`). A seed that files nothing, or refuses, is
    ///   left to that same fill: `Store::ensure` asks the seed again and stops the body with
    ///   its typed refusal naming the head (`HeadError::Seed`), as before (rule 3).
    /// - ONE ASK (`AsksMet::Once`, front2 asks.rs, D161 step 3): over a seeded store the import
    ///   is `w`'s need asked once on the store's board ([`super::asks::ask_seed`]); made, the
    ///   question is asked again at once; failed, the round's fill; refused, a gap naming it.
    pub(super) fn store_want<T>(&self, w: Want) -> Fetch<T> {
        if let Some(seeds) = self.store.seeds
            && self.store.asks.once()
        {
            use super::asks::Imported;
            return match super::asks::ask_seed(self.store, self.keys, seeds, w) {
                Ok(Imported::Made) => Err(Missing),
                Ok(Imported::Read | Imported::Failed) => self.fill(Fill::Store(w)),
                Err(refused) => {
                    self.store.asks.take_refused();
                    self.gap(Gap::Ask(refused))
                }
            };
        }
        if let Some(seeds) = self.store.seeds {
            let _cost = super::tables::cost::Cost::enter_want(self.store.cost, w);
            let filed = seeds.seed(self.store, self.keys, w, self.store.tys.alloc());
            if matches!(filed, Ok(Seeded::Filed)) && self.store.made(w) {
                self.store.imported.set(self.store.imported.get() + 1);
                return Err(Missing);
            }
        }
        self.fill(Fill::Store(w))
    }

    /// An impl question (`fill`, `Fill::Impls` or `Fill::Inherent`) the body's buckets hold no
    /// answer to: the one seam between the two ways a body's check meets it (rule 78, chosen by
    /// `Store::seeds` as [`Self::store_want`]'s is; the cost test runs both and asserts equal
    /// answers and read lists):
    /// - ON THE SPOT: over a seeded store with the body's buckets made (check.rs
    ///   `check_rounds_as` makes them before round 0), the question is answered now through
    ///   shared borrows, by the same body `make_once` runs between rounds (check.rs
    ///   `trait_question` / `inherent_question` over `Importing`: the trait's head and the
    ///   candidates' headers imported from their frozen records, the impls filed through the
    ///   buckets' one index borrow, which records the bucket cells read, `names.impls_read`, as
    ///   between rounds; the answer filed as a new row). `Store::imported` counts it, so
    ///   [`Self::again`] asks at once and reads the filed answer.
    /// - NEXT ROUND: over a lowering store, or where the on-the-spot path leaves it (a head its
    ///   seed does not file through a shared borrow, `check::NotHere`; or a refusal: a table
    ///   read, a candidate without its header), the fill `make_once` makes between rounds,
    ///   which meets the same seed or refusal and names it there (rule 3: nothing is dropped;
    ///   the fill repeats the work through `&mut`, where the refusal is kept).
    fn impl_question<T>(&self, fill: Fill) -> Fetch<T> {
        let (Some(seeds), Some(b)) = (self.store.seeds, self.buckets) else { return self.fill(fill) };
        use super::check::{BuildOrder, Importing, Made, inherent_question, trait_question};
        let mut heads = Importing { keys: self.keys, store: self.store, seeds };
        // The order a question's candidate headers are imported in: an answer depends on none
        // (each is a function of its own record), and the check's `BuildOrder` is the round
        // loop's, which a body answered in one round never reaches.
        let made = match fill {
            Fill::Impls(trait_, shape, seen) => trait_question(self.names, &mut heads, b, trait_, shape, seen, BuildOrder::AsAsked),
            Fill::Inherent(shape, name) => inherent_question(self.names, &mut heads, b, shape, name, BuildOrder::AsAsked),
            Fill::Store(_) | Fill::InferredOutput(_) => return self.fill(fill),
        };
        // Under the one ask, an import its question met refused is the question's gap.
        if let Some(refused) = self.store.asks.take_refused() {
            return self.gap(Gap::Ask(refused));
        }
        match made {
            Ok(Made::Made) if self.store.asks.once() => {
                let fact = if matches!(fill, Fill::Inherent(..)) { blaze_ask::Fact::Inherent } else { blaze_ask::Fact::ImplsOf };
                self.store.answered(fact);
                Err(Missing)
            }
            Ok(Made::Made) => {
                self.store.imported.set(self.store.imported.get() + 1);
                Err(Missing)
            }
            Ok(Made::Later(_)) | Err(_) => self.fill(fill),
        }
    }

    /// `ask` answered, asked again after each head it imported ([`Self::store_want`]). Each
    /// import files a cell not filed before (`want_unmade` asks only an unmade want), so the
    /// asks are bounded by the cells the question reads; past [`MOST_IMPORTS_A_QUESTION`] the
    /// last `Missing` stands and the round ends (check.rs `check_rounds_as` goes on).
    ///
    /// Under the one ask (`AsksMet::Once`) the count watched is what the store's asks made
    /// (`Store::imports`), and the bound the board's slots: each pass past the first made a
    /// fact never made before in the store. (The question itself is read again: a read of
    /// published rows, no work made again.)
    fn again<T>(&self, ask: impl Fn() -> Fetch<T>) -> Fetch<T> {
        for _ in 0..self.store.asks.most_asks_again() {
            let before = self.store.imports();
            let got = ask();
            if got.is_ok() || self.store.imports() == before {
                return got;
            }
        }
        ask()
    }

    /// `Missing`, noting the want that makes `key`'s record, by what `key` is
    /// ([`super::by_origin::want_unmade`]): a kind no want makes is its named gap, and a want
    /// already made (whose answer is still not in the store) is `KindGap::MadeWithoutRow`,
    /// never asked again.
    pub(super) fn want<T>(&self, key: ItemKey) -> Fetch<T> {
        self.unmade(super::by_origin::want_unmade(self.names, self.keys, self.store, key))
    }

    /// A member fn's signature row: made, a typed gap where it could not be made, else asked
    /// of the store.
    fn fn_row(&self, f: ItemKey) -> Fetch<&super::heads::FnHead<'_>> {
        if let Some(row) = self.store.fns.get(&f) {
            return Ok(row);
        }
        match self.store.fn_gaps.get(&f) {
            Some(why) => self.gap(Gap::FnHead(f, format!("{why:?}").into_boxed_str())),
            None => self.want(f),
        }
    }

    /// The gap of a candidate impl whose header failed: unread as written, else refused.
    /// `asked` says the question (which trait at which shape, or the inherent items of which
    /// shape): it follows the place, so a gap names what was asked beside who could not answer.
    fn failed_impl(&self, k: LibKey, why: &str, asked: &str) -> Gap {
        let place = format!("{} [asked: {asked}]", self.place_shown(k));
        match self.buckets.is_some_and(|b| b.is_unread(k)) {
            // The place leads the why: a census cuts the text, and the impl is to be found.
            true => Gap::UnreadImpl(k, format!("at {place}: {why}").into_boxed_str()),
            false => Gap::RefusedImpl { impl_: k, place: place.into_boxed_str(), why: why.into() },
        }
    }

    /// The library's outline entry of a checker key's item (`None` for a member or a variant).
    pub(super) fn detail(&self, key: ItemKey) -> Option<ItemDetail> {
        self.resolver().detail(self.keys.library_in(self.names, key)?)
    }

    fn types(&self) -> PTys<'_> {
        self.store.tys.view()
    }

    /// The enum `variant` is in and its index there.
    pub(super) fn enum_of(&self, variant: LibKey) -> Option<(LibKey, u32)> {
        self.resolver().variant_of(variant)
    }

    /// A ctor fn's signature not yet made: its ADT's head makes it, so want that; once the head
    /// is made and still has no ctor row, the variant is unit or braced: no fn (`None`).
    fn ctor_owner<'s>(&'s self, ctor: ItemKey, adt: ItemKey) -> Fetch<Option<FnSig<'s>>> {
        match self.store.adts.get(&adt) {
            Some(a) if a.variants.iter().any(|v| v.key == ctor) => Ok(None),
            Some(_) => self.gap(Gap::CtorFn(ctor)),
            None => self.want(adt),
        }
    }

    /// A variant's (or a struct's, index 0) definition as the body names it.
    /// An index past the made ADT's variants is `KindGap::NoVariant` naming the ADT and the
    /// index ([`super::by_origin::ctor_def`]), never "unknown key".
    pub(super) fn ctor(&self, adt: LibKey, variant: u32) -> Fetch<Def> {
        match super::by_origin::ctor_def(self.names, self.keys, self.store, adt, variant) {
            Ok(def) => Ok(def),
            Err(u) => self.unmade(u),
        }
    }
}

/// The world's answers, each asked again after an import on the spot ([`BodyWorld::again`]).
impl<'n, S: NamesSource<'n>> World for BodyWorld<'_, 'n, S> {
    fn goal_share(&self) -> Option<&dyn blaze_solve::share::GoalShare> {
        self.store.goals
    }
    /// Under the one ask over frozen heads (`AsksMet::Once` with `Store::seeds`: D161, front2
    /// asks.rs) every head and impl answer that can be made is made on the spot, so a hole is a
    /// fact this world cannot make inline (a body-local item's head, an impl candidate left
    /// `NotHere`, another body's record): the run stops at it, never reading a stand-in past
    /// it, and the round's fill is exactly that fact. Elsewhere the stand-in path, as before.
    fn stops_at_holes(&self) -> bool {
        self.store.stops_at_holes()
    }
    /// The syntactic prefetch of a body with no record: the questions its text names, before
    /// the walk enters it. Not answered here (no read enters the list); collected as the
    /// body's gate set while round 0 collects (front2 asks.rs `Gate`).
    fn prefetch(&self, _: Seal, questions: &[tc::Question]) {
        self.store.gate.borrow_mut().note(questions);
    }
    /// The checker's run memo, handed to the recorder the names are read through (tables/
    /// recorder.rs `Repeats`): a question this run answered already offers none of its reads.
    fn asking(&self, _: Seal, ask: Option<tc::Asking>) {
        self.names.asking(ask);
    }
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        self.again(|| self.fn_sig_once(item))
    }
    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        self.again(|| self.predicates_once(item))
    }
    fn field_defaults(&self, _: Seal, adt: ItemKey) -> Fetch<&[Option<ItemKey>]> {
        self.again(|| self.field_defaults_once(adt))
    }
    fn member_visible(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<bool> {
        self.again(|| self.member_visible_once(container, name))
    }
    fn member_unstable(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<bool> {
        self.again(|| self.member_unstable_once(container, name))
    }
    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<tc::Adt<'_>>> {
        self.again(|| self.adt_once(item))
    }
    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        self.again(|| self.impls_of_once(trait_, shape))
    }
    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        self.again(|| self.impl_header_once(item))
    }
    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        self.again(|| self.inherent_once(shape, name))
    }
    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        self.again(|| self.assoc_item_once(container, name))
    }
    fn traits_in_scope(&self, _: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        self.traits_in_scope_once(scope)
    }
    fn resolve_path(&self, _: Seal, scope: ScopeId, p: PathIdx, segments: &[Name]) -> Fetch<tc::PathAnswer> {
        self.again(|| self.path(scope, p, segments))
    }
    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        self.lang_item_once(item)
    }
    fn lang_path(&self, _: Seal, item: LangItem) -> Fetch<Option<Def>> {
        self.again(|| self.lang_path_once(item))
    }
    fn const_value(&self, _: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.gap(Gap::BodyRecord(tc::Question::Const(item)))
    }
    fn opaque_hidden(&self, _: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        self.opaque_hidden_once(item)
    }
    fn coroutine(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<tc::CoroutineFacts<'_>>> {
        self.coroutine_once(owner, ordinal)
    }
    fn closure(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<tc::ClosureFacts<'_>>> {
        self.closure_once(owner, ordinal).map(|row| row.map(|(call, _)| call))
    }
    fn closure_with_captures(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<(tc::ClosureFacts<'_>, Option<tc::PTy>)>> {
        self.closure_once(owner, ordinal)
    }
    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        self.again(|| self.attrs_once(item))
    }
    fn trait_def(&self, _: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        self.again(|| self.trait_def_once(trait_))
    }
    fn assoc_parent(&self, _: Seal, item: ItemKey) -> Fetch<Option<AssocParent>> {
        self.again(|| self.assoc_parent_once(item))
    }
    fn const_holds(&self, _: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<tc::ConstHolds> {
        self.const_holds_once(cond, args, types)
    }
    fn const_params(&self, _: Seal, item: ItemKey) -> Fetch<tc::ConstParams<'_>> {
        self.again(|| self.const_params_once(item))
    }
    fn assoc_generics(&self, _: Seal, trait_: ItemKey, name: Name) -> Fetch<Option<tc::AssocGenerics>> {
        self.again(|| self.assoc_generics_once(trait_, name))
    }
    fn type_alias(&self, _: Seal, item: ItemKey) -> Fetch<Option<tc::AliasDef<'_>>> {
        self.again(|| self.type_alias_once(item))
    }
}

/// Each question answered once: `Missing` with a fill or a gap noted, or with a head imported
/// on the spot ([`BodyWorld::store_want`]), which the trait's method above asks again.
impl<'n, S: NamesSource<'n>> BodyWorld<'_, 'n, S> {
    fn fn_sig_once(&self, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        if let Some(f) = self.store.fns.get(&item) {
            if item != self.owner && self.store.has_unresolved_inferred_output(item) {
                if self.store.inferring_output_stack.scan_for(&item).is_some() {
                    return self.gap(Gap::InferredOutputCycle { owner: self.owner, callee: item });
                }
                return self.fill(Fill::InferredOutput(item));
            }
            return Ok(Some(FnSig {
                parent_count: f.parent,
                own_count: f.own,
                declared_count: f.declared,
                self_param: f.self_param,
                header: f.header,
                c_variadic: f.c_variadic,
                legacy_const_generics: &f.legacy_const_generics,
                inputs: &f.inputs,
                output: self.store.fn_output(item).unwrap_or(f.output),
                types: self.types(),
            }));
        }
        if let Some(c) = self.store.ctors.get(&item) {
            return Ok(Some(FnSig {
                parent_count: 0,
                own_count: c.generics,
                declared_count: c.generics,
                self_param: false,
                header: tc::FnPtrHeader::RUST,
                c_variadic: false,
                legacy_const_generics: &[],
                inputs: &c.inputs,
                output: c.output,
                types: self.types(),
            }));
        }
        if let Some(why) = self.store.fn_gaps.get(&item) {
            return self.gap(Gap::FnHead(item, format!("{why:?}").into_boxed_str()));
        }
        match self.keys.origin(item) {
            Some(Origin::Member { kind: MemberKind::Fn, .. }) => self.want(item),
            Some(Origin::Member { kind: MemberKind::Ctor, container, .. }) => self.ctor_owner(item, *container),
            Some(Origin::Member { .. }) => Ok(None),
            // What the key is, from the one classifier (super::by_origin::kind_of_key): the
            // library's outline, or the kind a body's lowering filed for an item declared in a
            // body (whose key the library holds no entry for: hello-main-ctorfn). A key in no
            // row of the map is asked of the source (`Keys::library_in`); a key of nothing has
            // no signature. A key the library holds no entry for is refused by name, never
            // read as a variant (a path to a fn is never asked as a ctor).
            Some(Origin::Item(_)) | None => {
                use super::by_origin::{ItemKind, KeyKind};
                match super::by_origin::kind_of_key(self.names, self.keys, item) {
                    Ok(KeyKind::Item(ItemKind::Fn(_) | ItemKind::Const(_) | ItemKind::Static(_))) => self.want(item),
                    // A variant: its enum's head makes its ctor fn.
                    Ok(KeyKind::Item(ItemKind::Variant { enum_, .. })) => self.ctor_owner(item, checker_of(enum_)),
                    // A member keyed by the library: its container's head makes it (by kind;
                    // a made head still without it is `KindGap::MadeWithoutRow`).
                    Ok(KeyKind::Item(ItemKind::LibMember { .. })) => self.want(item),
                    Ok(KeyKind::Item(
                        ItemKind::Adt(_) | ItemKind::Trait(_) | ItemKind::TraitAlias(_) | ItemKind::Impl(_) | ItemKind::TypeAlias(_) | ItemKind::Other { .. },
                    )) => Ok(None),
                    // A member's key (filed under `Origin::Member`, answered above).
                    Ok(KeyKind::MemberFn { .. } | KeyKind::MemberConst { .. } | KeyKind::MemberType { .. } | KeyKind::Ctor { .. } | KeyKind::Anon { .. }) => Ok(None),
                    Err(FillError::UnknownKey(_)) => Ok(None),
                    Err(e) => self.gap(Gap::Fill(e)),
                }
            }
        }
    }

    fn predicates_once(&self, item: ItemKey) -> Fetch<Predicates<'_>> {
        // A ctor fn's where-clauses are its type's.
        let item = self.store.ctors.get(&item).map_or(item, |c| c.adt);
        // The default of a provided trait fn's `impl Trait` (super::rpitit `Twin`): its
        // source's bounds with itself as `Self`, filed with its trait's rows.
        if let Some(twin) = self.store.rpitit_defaults.get(&item) {
            return Ok(Predicates { clauses: &twin.clauses, types: self.types() });
        }
        match self.store.preds.get(&item) {
            Some(p) if !p.conds.is_empty() => self.gap(Gap::WhereConst(item)),
            Some(p) => Ok(Predicates { clauses: &p.clauses, types: self.types() }),
            // A trait's associated type whose bounds were refused (`Store::trait_members`
            // files the refusal under the type's own key): the gap names it and why.
            None if self.store.fn_gaps.contains_key(&item) => {
                let why = self.store.fn_gaps.get(&item).map(|w| format!("{w:?}")).unwrap_or_default();
                self.gap(Gap::FnHead(item, why.into_boxed_str()))
            }
            // By what the key is: an opaque's bounds come with its fn's head, an anonymous
            // constant's with its own filing; any other kind's row with its own want.
            None => self.unmade(super::by_origin::predicates_unmade(self.names, self.keys, self.store, item)),
        }
    }

    fn field_defaults_once(&self, adt: ItemKey) -> Fetch<&[Option<ItemKey>]> {
        if let Some(d) = self.store.field_defaults.get(&adt) {
            return Ok(&d[..]);
        }
        // A row filed with no defaults entry: the gap, never "no defaults".
        if self.store.adts.contains_key(&adt) {
            return self.gap(Gap::FieldDefaults(adt));
        }
        match self.detail(adt) {
            Some(ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => self.want(adt),
            _ => Ok(&[]),
        }
    }

    /// The Reference, "Visibility and privacy", for member fn `name` of inherent impl
    /// `container` against the body's module ([`World::member_visible`]). The member's
    /// visibility is its impl head's (`ImplHead::fn_vis`, as written); the module it is
    /// relative to is the impl's (its members' frame). As rustc's `Visibility::Restricted`, a
    /// restriction names the nearest NORMAL module (a block's module is passed over:
    /// `written_module(m, 0)`), and a module sees it when that module is on its chain
    /// (`written_module(body, n)` for n = 0, 1, ..). What cannot be told is a named gap.
    /// Whether member `name` of inherent impl `container` is unstable under a library feature
    /// the body's crate does not turn on ([`World::member_unstable`]): rustc_middle
    /// stability.rs `eval_stability_allow_unstable`. The member's stability is its impl head's
    /// (`ImplHead::unstable`, read from its `#[unstable(..)]`); an item of the body's own crate
    /// is allowed ("Only the cross-crate scenario matters"), and so is one whose `feature` or
    /// `implied_by` the body's crate root turns on (`NamesSource::has_feature`, the crate
    /// root's `#![feature]` list: a read of the root over records). Not read: a macro's
    /// `#[allow_internal_unstable(..)]` at the call (rustc's `span.allows_unstable`) and a
    /// module's own `#[unstable]` over an unmarked impl.
    fn member_unstable_once(&self, container: ItemKey, name: Name) -> Fetch<bool> {
        let Some(i) = self.store.impls.get(&container) else {
            return match self.detail(container) {
                Some(ItemDetail::Impl) => self.want(container),
                // No inherent impl: the question was asked of no member, nothing marks it.
                _ => Ok(false),
            };
        };
        let Some((_, u)) = i.unstable.iter().find(|(n, _)| *n == name) else { return Ok(false) };
        let gap = |why: &'static str| self.gap(Gap::MemberStability { impl_: container, name, why });
        let home = self.keys.library_in(self.names, container).and_then(|k| self.names.crate_of(k));
        let (Some(home_crate), Some(body_crate)) = (home, self.names.crate_of(self.module)) else {
            return gap("the names hold no crate for the impl or the body's module");
        };
        if home_crate == body_crate {
            return Ok(false);
        }
        let Some(on) = self.names.has_feature(body_crate, &u.feature) else {
            return gap("the body's crate root features are not answered");
        };
        if on {
            return Ok(false);
        }
        match u.implied_by.as_deref() {
            None => Ok(true),
            Some(by) => match self.names.has_feature(body_crate, by) {
                Some(on) => Ok(!on),
                None => gap("the body's crate root features are not answered"),
            },
        }
    }

    fn member_visible_once(&self, container: ItemKey, name: Name) -> Fetch<bool> {
        let Some(i) = self.store.impls.get(&container) else {
            return match self.detail(container) {
                Some(ItemDetail::Impl) => self.want(container),
                // No inherent impl: the question was asked of no member, nothing hides it.
                _ => Ok(true),
            };
        };
        let Some(at) = i.fns.iter().position(|(n, _)| *n == name) else { return Ok(true) };
        let Some(&vis) = i.fn_vis.get(at) else {
            return self.gap(Gap::MemberVisibility { impl_: container, name, why: "its impl's head holds no visibility for it" });
        };
        use super::heads::MemberVis;
        let gap = |why: &'static str| self.gap(Gap::MemberVisibility { impl_: container, name, why });
        if vis == MemberVis::Public {
            return Ok(true);
        }
        let Some(frame) = self.store.member_frame(container) else { return gap("its impl's head holds no members' frame (the impl's module)") };
        let home = frame.module;
        let (Some(home_crate), Some(body_crate)) = (self.names.crate_of(home), self.names.crate_of(self.module)) else {
            return gap("the names hold no crate for the impl's module or the body's");
        };
        // `pub(crate)`, `pub(super)`, `pub(in ..)` and private never reach past the crate.
        if home_crate != body_crate {
            return Ok(false);
        }
        let up = match vis {
            MemberVis::Public | MemberVis::Crate => return Ok(true),
            MemberVis::SelfMod => 0,
            MemberVis::Super => 1,
            MemberVis::Up { up } => up,
            // The ancestor `depth` modules below the root: the impl's module's own depth, less
            // `depth`, steps up (the path names an ancestor: heads.rs `MemberVis::InCrate`).
            MemberVis::InCrate { depth } => {
                let mut deepest = None;
                for n in 0..=u8::MAX {
                    match self.names.written_module(home, n) {
                        Ok(_) => deepest = Some(n),
                        Err(e) if matches!(e.why, blaze_scope::names::Why::SuperPastRoot) => break,
                        Err(_) => return gap("the impl's module chain is not held"),
                    }
                }
                let Some(at_root) = deepest else { return gap("the impl's module chain is not held") };
                match at_root.checked_sub(depth) {
                    Some(up) => up,
                    None => return gap("its `pub(in crate::..)` path is deeper than the impl's module: no ancestor (rustc E0742)"),
                }
            }
            MemberVis::InPath => return gap("it is `pub(in path)` in a form other than `crate::..` or `super::..`, and the body is in the impl's crate"),
        };
        let Ok(scope) = self.names.written_module(home, up) else {
            return gap("the impl's module chain is not held (or `pub(super)` at the crate's root)");
        };
        for n in 0..=u8::MAX {
            match self.names.written_module(self.module, n) {
                Ok(m) if m == scope => return Ok(true),
                Ok(_) => {}
                Err(e) if matches!(e.why, blaze_scope::names::Why::SuperPastRoot) => return Ok(false),
                Err(_) => return gap("the body's module chain is not held"),
            }
        }
        gap("the body's module is deeper than 256 modules (rule 18)")
    }

    fn adt_once(&self, item: ItemKey) -> Fetch<Option<tc::Adt<'_>>> {
        if let Some(a) = self.store.adts.get(&item) {
            return Ok(Some(tc::Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.types() }));
        }
        match self.detail(item) {
            Some(ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => self.want(item),
            _ => Ok(None),
        }
    }

    /// Whose impls this world's trait questions see (buckets.rs [`Seen`]): checking a body
    /// (`Reveal::InBody`), its crate's and the crates it links; at ground arguments after
    /// checking (`Reveal::All`), the whole program's.
    fn seen(&self) -> Result<Seen, Unseen> {
        match self.reveal {
            super::opaque_auto::Reveal::All => Ok(Seen::Program),
            super::opaque_auto::Reveal::InBody => Seen::of_checked_body(self.names, self.module),
        }
    }

    fn impls_of_once(&self, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        let seen = match self.seen() {
            Ok(seen) => seen,
            Err(why) => return self.gap(Gap::Sight(why)),
        };
        if let Some((k, why)) = self.buckets.and_then(|b| b.trait_gap_seen(trait_, shape, seen)) {
            let asked = match self.keys.library_in(self.names, trait_) {
                Some(lib) => format!("the impls of the trait {lib:x} at {shape:?}"),
                None => format!("the impls of the trait {trait_:?} at {shape:?}"),
            };
            return self.gap(self.failed_impl(k, &why, &asked));
        }
        match self.buckets.and_then(|b| b.impls_seen(trait_, shape, seen)) {
            Some(v) => Ok(v),
            None => self.impl_question(Fill::Impls(trait_, shape, seen)),
        }
    }

    fn impl_header_once(&self, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        if let Some((i, provisional)) = self.store.impl_or_provisional(item) {
            if provisional {
                self.store.note_provisional_read(item);
            }
            // An impl of a trait with a fn returning `impl Trait`: that type's value
            // here is a synthetic associated type's row (super::rpitit). Skip rpitit
            // caching while provisional so incomplete assoc types are never cached.
            let assoc_types = if provisional {
                &i.assoc[..]
            } else {
                match i.trait_ {
                    Some(trait_) => match self.rpitit_rows(trait_)? {
                        Some(rows) => self.rpitit_retry(item, || super::rpitit::impl_assoc(self.store, item, i, rows))?,
                        None => &i.assoc[..],
                    },
                    None => &i.assoc[..],
                }
            };
            return Ok(Some(ImplHeader {
                generics: i.generics,
                self_ty: i.self_ty,
                trait_: i.trait_,
                trait_args: i.trait_args,
                assoc_types,
                types: self.types(),
                polarity: i.polarity,
            }));
        }
        match self.detail(item) {
            Some(ItemDetail::Impl) => self.want(item),
            _ => Ok(None),
        }
    }

    fn inherent_once(&self, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        if let Some((k, why)) = self.buckets.and_then(|b| b.inherent_gap(shape, name)) {
            return self.gap(self.failed_impl(k, &why, &format!("the inherent items of {shape:?} named {name:?}")));
        }
        match self.buckets.and_then(|b| b.inherent(shape, name)) {
            Some(v) => Ok(v),
            None => self.impl_question(Fill::Inherent(shape, name)),
        }
    }

    fn assoc_item_once(&self, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        if let Some((i, provisional)) = self.store.impl_or_provisional(container) {
            if provisional {
                self.store.note_provisional_read(container);
            }
            // The fn's `self` from its text (`ImplHead::fn_self`): naming a member makes no
            // signature; `fn_sig` asks for it (`Want::MemberFn`) when the fn is used.
            if let Some(at) = i.fns.iter().position(|(n, _)| *n == name) {
                let f = i.fns[at].1;
                let Some(&self_param) = i.fn_self.get(at) else {
                    return self.gap(Gap::FnHead(f, "its impl's head holds no `self` flag for it".into()));
                };
                return Ok(Some(AssocItem { container, item: f, kind: AssocKind::Fn { self_param } }));
            }
            if let Some(&(_, c)) = i.consts.iter().find(|(n, _)| *n == name) {
                return Ok(Some(AssocItem { container, item: c, kind: AssocKind::Const }));
            }
            if i.assoc.iter().any(|(n, _)| *n == name) {
                return self.gap(Gap::ImplAssocType(container, name));
            }
            return Ok(None);
        }
        let Some(lib) = self.keys.library_in(self.names, container) else { return Ok(None) };
        let (Some(t), Some(extra)) = (self.store.maps.traits.get(&lib), self.store.trait_defs.get(&container)) else {
            return match self.resolver().detail(lib) {
                // Its head is made only when one of its members has this name: method
                // resolution asks every trait in scope, and a trait that declares no such item
                // is no candidate, so its head (refused or not) is never read for this name
                // (member_names.rs; the made head answers from the same member list).
                Some(ItemDetail::Trait | ItemDetail::Impl) if !super::member_names::may_declare(self.names, lib, name) => Ok(None),
                Some(ItemDetail::Trait | ItemDetail::Impl) => self.want(container),
                _ => Ok(None),
            };
        };
        if let Some(&(_, f)) = t.fns.iter().find(|(n, _)| *n == name) {
            let row = self.fn_row(f)?;
            return Ok(Some(AssocItem { container, item: f, kind: AssocKind::Fn { self_param: row.self_param } }));
        }
        // The trait's associated constant (`<B4 as Specifier>::BITS`, `P::WRITE`), declared or
        // defaulted: its row's output is the type the checker gives the path; its value is
        // the constant evaluator's (consts.rs `impl_const`) and only at a concrete `Self`.
        if let Some(&(_, c)) = extra.consts.iter().find(|(n, _)| *n == name) {
            return Ok(Some(AssocItem { container, item: c, kind: AssocKind::Const }));
        }
        Ok(extra.assoc.iter().find(|a| a.name == name).map(|a| AssocItem { container, item: a.item, kind: AssocKind::Type }))
    }

    /// The module's traits, or the block's when a `use` of the block or of one around it brings
    /// in more ([`super::block_traits`]); a block whose imports are not known is the gap
    /// naming the import.
    fn traits_in_scope_once(&self, scope: ScopeId) -> Fetch<&[ItemKey]> {
        match self.blocks.at(scope.0) {
            super::block_traits::InScope::Module => Ok(self.traits),
            super::block_traits::InScope::Traits(t) => Ok(t),
            super::block_traits::InScope::Refused(gap) => self.gap(Gap::Local(gap)),
        }
    }

    fn lang_item_once(&self, item: LangItem) -> Fetch<Option<ItemKey>> {
        match super::langs::find(self.index, self.resolver(), item) {
            Ok(found) => Ok(found.map(checker_of)),
            Err(why) => self.gap(Gap::Lang(item, why)),
        }
    }

    fn lang_path_once(&self, item: LangItem) -> Fetch<Option<Def>> {
        let Some(name) = lang_name(item) else { return self.gap(Gap::Lang(item, "no `#[lang]` name is known for it")) };
        let Some(lib) = self.index.lang(name) else { return self.gap(Gap::Lang(item, "no item of the program's crates is `#[lang]` under its name")) };
        match self.resolver().detail(lib) {
            Some(ItemDetail::Fn { .. }) => Ok(Some(Def::Fn(checker_of(lib)))),
            Some(ItemDetail::Struct) => Ok(Some(Def::Adt(checker_of(lib)))),
            Some(_) => self.gap(Gap::Lang(item, "its item is of a kind a path to it is not answered for")),
            // A lang item that is a member: the pinned library marks fns inside a trait
            // (`Try::branch`, which `?` calls; `Iterator::next`) and fns inside an inherent impl
            // (`RangeInclusive::new`, which `a..=b` calls, in `impl<Idx> RangeInclusive<Idx>`;
            // `Pin::new_unchecked`, in `impl<Ptr: Deref> Pin<Ptr>`). Each is the fn its owner's
            // record files under the member's name: a trait's head, an impl's header.
            None => {
                if let Some((owner, name)) = self.resolver().member_of(lib) {
                    let name = Name::of(name);
                    return match self.resolver().detail(owner) {
                        Some(ItemDetail::Trait) => {
                            let Some(t) = self.store.maps.traits.get(&owner) else { return self.store_want(Want::Head(owner)) };
                            match t.fns.iter().find(|(n, _)| *n == name) {
                                Some(&(_, f)) => Ok(Some(Def::Fn(f))),
                                None => self.gap(Gap::Lang(item, "the trait's head holds no fn of its `#[lang]` name")),
                            }
                        }
                        Some(ItemDetail::Impl) => {
                            let Some(i) = self.store.impls.get(&checker_of(owner)) else { return self.store_want(Want::Impl(owner)) };
                            match i.fns.iter().find(|(n, _)| *n == name) {
                                Some(&(_, f)) => Ok(Some(Def::Fn(f))),
                                None => self.gap(Gap::Lang(item, "the impl holds no fn of its `#[lang]` name")),
                            }
                        }
                        _ => self.gap(Gap::Lang(item, "its item is a member of neither a trait nor an impl")),
                    };
                }
                match self.enum_of(lib) {
                    Some((e, i)) => self.ctor(e, i).map(Some),
                    None => self.gap(Gap::VariantOfNoEnum(lib)),
                }
            }
        }
    }

    /// An opaque type a fn's head made: `None` (a body sees only its bounds: the Reference,
    /// "Impl trait", "Abstract return types") unless the running check's own leak list holds it
    /// (an auto trait of it was asked: "Auto trait leakage") or the world is a ground
    /// resolution's ([`super::opaque_auto::Reveal::All`]), then its hidden type from its
    /// defining body's filed check ([`super::opaque_auto`]). A filed record with no hidden type
    /// for it, and any key no head made, is a gap.
    fn opaque_hidden_once(&self, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        // The default of a provided trait fn's `impl Trait` (super::rpitit `Twin`) is answered
        // by that fn's body's row for its source opaque.
        let filed = super::rpitit::filed_as(self.store, item);
        let Some(&(owner, ordinal)) = self.store.opaques.get(&filed) else {
            return self.gap(Gap::Hidden(super::opaque_auto::HiddenFail::NoHead { opaque: item }));
        };
        // A trait fn's `impl Trait` at a place of its return type reveals its synthetic
        // projection, per impl (super::rpitit), required or provided: the answer is the same in
        // every body. An impl's fn's container has no rows (`rpitit_rows` of an impl is `None`).
        let rows = match self.store.fns.get(&owner).and_then(|f| f.container) {
            Some(container) if filed == item => self.rpitit_rows(container)?,
            _ => None,
        };
        if let Some(alias) = super::rpitit::hidden_of(rows, item) {
            return Ok(Some(Hidden { ty: alias, types: self.types() }));
        }
        match self.store.hidden.answer(item, filed, owner, self.reveal) {
            // A trait fn's `impl Trait` with no row stands in another's bounds: its body's type
            // (a provided fn's) is not the type through an impl that writes the fn.
            super::opaque_auto::HiddenAnswer::Known(_) if rows.is_some() => {
                let shown = super::check::key_shown_in(self.names, self.keys, self.store, owner);
                self.gap(Gap::Hidden(super::opaque_auto::HiddenFail::TraitFnInBounds { opaque: item, owner, shown }))
            }
            super::opaque_auto::HiddenAnswer::Known(h) => Ok(Some(h)),
            super::opaque_auto::HiddenAnswer::Opaque => Ok(None),
            super::opaque_auto::HiddenAnswer::NoneGiven => {
                if super::tait::of_alias(ordinal) {
                    return self.gap(Gap::Hidden(super::opaque_auto::HiddenFail::AliasOpaque { opaque: item, alias: owner }));
                }
                let path = super::check::key_shown_in(self.names, self.keys, self.store, owner);
                self.gap(Gap::HiddenRow { opaque: item, owner, ordinal, path })
            }
        }
    }
    /// Async block `ordinal` of `owner` (an `async fn`'s hidden type is its root block by
    /// identity, `PNode::Coroutine`): its output and interior from the owner's filed check
    /// (`Store::hidden`); a gap, saying why, while that body is not filed or its row cannot
    /// answer. A row whose flow was refused answers with the check's superset interior
    /// ([`tc::InteriorIs::Superset`]) to a body check only ([`super::opaque_auto::Reveal::InBody`]),
    /// where a failure over it is the checker's typed gap (blaze-typeck rules/async_fn.rs
    /// `Witness::ForeignSuperset`); a resolution after checking decides on exact facts only.
    fn coroutine_once(&self, owner: ItemKey, ordinal: u32) -> Fetch<Option<tc::CoroutineFacts<'_>>> {
        use super::opaque_auto::{CoroutineAnswer, Reveal};
        let why = match self.store.hidden.coroutine(owner, ordinal) {
            CoroutineAnswer::Facts(f) if f.is == tc::InteriorIs::Exact || matches!(self.reveal, Reveal::InBody) => return Ok(Some(f)),
            CoroutineAnswer::Facts(_) => "its flow was refused, so only its check's superset interior is filed, which decides no auto trait outside a body check",
            other => other.why().unwrap_or("no facts were given"),
        };
        let path = super::check::key_shown_in(self.names, self.keys, self.store, owner);
        self.gap(Gap::CoroutineRow { reader: self.owner, owner, ordinal, path, why })
    }
    /// A closure of another body: its kind and signature from its owner's checked record, filed
    /// in the store where that check finished clean ([`super::foreign_closures`]; typeck.md "A
    /// closure's identity"). The checker records the question with its answer's digest, so it
    /// is in this body's read list (rule 21). An owner with no clean record, or an ordinal it
    /// does not hold, is a gap naming the owner by its path and the ordinal (rule 19).
    fn closure_once(&self, owner: ItemKey, ordinal: u32) -> Fetch<Option<(tc::ClosureFacts<'_>, Option<tc::PTy>)>> {
        match self.store.closures.with_captures(owner, ordinal) {
            Ok((kind, sig, env)) => Ok(Some((tc::ClosureFacts { kind, sig, types: self.store.tys.view() }, env))),
            Err(why) => {
                let path = super::check::key_shown_in(self.names, self.keys, self.store, owner);
                let text = format!("{}", super::foreign_closures::gap_text(owner, &path, ordinal, why)).into_boxed_str();
                self.gap(Gap::ForeignClosure { owner, ordinal, text })
            }
        }
    }
    /// No ADT attribute is read into the store yet (`#[non_exhaustive]`, `#[fundamental]`):
    /// the open item "attrs" in the README; answered empty as the old front end did.
    /// An enum's discriminant type ([`super::heads::discr_ty`]: `isize` with no primitive
    /// repr, else the repr's integer), from its made head; a struct or union has none to give.
    /// A `#[repr(simd)]` struct is [`TypeAttrs::SIMD`] (intr-P9b: an `asm!` operand's vector
    /// type, blaze-typeck asm.rs), from the same made repr (`ReprAttrs::simd`).
    /// A fn item's (the callee of a borrowed call, blaze-typeck rules/promote.rs): read from
    /// its made head's text ([`super::fn_attrs::fn_type_attrs`], `#[rustc_promotable]`,
    /// `#[rustc_intrinsic]`, `#[target_feature]`); a ctor fn carries none. A fn's key is never
    /// asked for as an ADT (rung x0-drop-11-zst-temporary: "a fn (asked for as an ADT)").
    fn attrs_once(&self, item: ItemKey) -> Fetch<TypeAttrs> {
        if let Some(a) = self.store.adts.get(&item) {
            // `#[blaze::args]` (rule 83): the options-struct mark, from the made head or its record.
            let args = if a.args { TypeAttrs::ARGS } else { 0 };
            // `repr(packed)` in any form (`packed(N)`, `C, packed`): the layout's own fact
            // (`ReprAttrs::packed`), answered for the capture analysis (TypeAttrs::PACKED).
            let packed = if a.repr.packed.is_some() { TypeAttrs::PACKED } else { 0 };
            let auto_copy = if a.kind == tc::AdtKind::Struct && self.store.is_auto_copy_pub(self.names, self.keys, item) {
                TypeAttrs::AUTO_COPY_PUB
            } else {
                0
            };
            return Ok(match a.kind {
                tc::AdtKind::Enum => TypeAttrs(args).with_discr(super::heads::discr_ty(a.repr)),
                tc::AdtKind::Struct if a.repr.simd => TypeAttrs(TypeAttrs::SIMD | args | packed | auto_copy),
                _ => TypeAttrs(args | packed | auto_copy),
            });
        }
        if let Some(f) = self.store.fns.get(&item) {
            return Ok(super::fn_attrs::fn_type_attrs(f));
        }
        if self.store.ctors.contains_key(&item) {
            return Ok(TypeAttrs(0));
        }
        if let Some(why) = self.store.fn_gaps.get(&item) {
            return self.gap(Gap::FnHead(item, format!("{why:?}").into_boxed_str()));
        }
        match self.keys.origin(item) {
            Some(Origin::Member { kind: MemberKind::Fn, .. }) => self.want(item),
            // A ctor fn's row is made with its ADT's head.
            Some(Origin::Member { kind: MemberKind::Ctor, container, .. }) => self.want(*container),
            Some(Origin::Member { .. }) => self.gap(Gap::Fill(FillError::NotAHead(tc::Question::Attrs(item)))),
            Some(Origin::Item(_)) | None => match self.keys.library_in(self.names, item) {
                None => self.gap(Gap::Fill(FillError::UnknownKey(item))),
                Some(lib) => match self.resolver().detail(lib) {
                    Some(ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => self.store_want(Want::Adt(lib)),
                    Some(ItemDetail::Fn { .. }) => self.want(item),
                    // A variant's ctor: its enum's head makes its row.
                    None => match self.enum_of(lib) {
                        Some((e, _)) => self.store_want(Want::Adt(e)),
                        None => self.gap(Gap::Fill(FillError::UnknownKey(item))),
                    },
                    Some(_) => self.gap(Gap::Fill(FillError::NotAHead(tc::Question::Attrs(item)))),
                },
            },
        }
    }

    fn trait_def_once(&self, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        let Some(lib) = self.keys.library_in(self.names, trait_) else { return Ok(None) };
        let (Some(t), Some(extra)) = (self.store.maps.traits.get(&lib), self.store.trait_defs.get(&trait_)) else {
            return match self.resolver().detail(lib) {
                Some(ItemDetail::Trait) => self.want(trait_),
                _ => Ok(None),
            };
        };
        Ok(Some(TraitDef {
            generics: t.generics,
            flags: t.flags,
            lang: self.index.lang_name_of(lib).and_then(trait_lang).or_else(|| {
                if t.generics != 2 {
                    return None;
                }
                let from_key = match self.store.from_trait.get() {
                    Some(cached) => cached,
                    None => {
                        let found = super::langs::find(self.index, self.resolver(), LangItem::From).ok().flatten();
                        self.store.from_trait.set(Some(found));
                        found
                    }
                };
                (from_key == Some(lib)).then_some(LangItem::From)
            }),
            supertraits: &extra.supertraits,
            // With a synthetic associated type per `impl Trait` at a place of a fn's return type
            // (super::rpitit), after the written ones.
            assoc_types: match self.rpitit_rows(trait_)? {
                Some(rows) => &rows.assoc[..],
                None => &extra.assoc[..],
            },
            types: self.types(),
            members: blaze_typeck::TraitMembers::Known(&t.members),
            // heads.rs `TraitHead::defaults`: lowered into `store.tys` (as buckets.rs
            // `trait_home` reads them), one per parameter after `Self`.
            defaults: blaze_typeck::TraitDefaults::Known(&t.defaults),
        }))
    }

    fn assoc_parent_once(&self, item: ItemKey) -> Fetch<Option<AssocParent>> {
        let Some(Origin::Member { container, kind, name }) = self.keys.origin(item) else { return Ok(None) };
        let kind = match kind {
            MemberKind::Fn => AssocKind::Fn { self_param: self.fn_row(item)?.self_param },
            MemberKind::Const => AssocKind::Const,
            MemberKind::Type => AssocKind::Type,
            // An anonymous constant is a member of its module: no trait or impl is its parent.
            MemberKind::Ctor | MemberKind::Anon => return Ok(None),
        };
        Ok(Some(AssocParent { container: *container, name: Name::of(name), kind }))
    }

    fn const_holds_once(&self, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<tc::ConstHolds> {
        self.gap(Gap::BodyRecord(tc::Question::ConstHolds(cond, tc::ground_args_digest(args, types))))
    }

    /// The item's const generic parameters as its head made them (`Maps::const_params`: a
    /// fn's with its container's first), by what the item is
    /// ([`super::by_origin::const_params_of`]): a head not made is wanted, never answered "no
    /// const parameters"; a kind with none to give is refused by name.
    fn const_params_once(&self, item: ItemKey) -> Fetch<tc::ConstParams<'_>> {
        match super::by_origin::const_params_of(self.names, self.keys, self.store, item) {
            Ok(params) => Ok(tc::ConstParams { params, types: self.types() }),
            Err(u) => self.unmade(u),
        }
    }

    /// The generics of the trait's associated type `name` as the trait's head counted them
    /// (`Maps::assoc_generics`, filled by `Store::trait_head`: type and const parameters; a
    /// lifetime is none, as a portable type holds no region): the Reference, "Associated
    /// Types": "Associated types may include generic parameters and where clauses". A trait
    /// whose head is not made is asked for; a trait that declares no such type has no count.
    fn assoc_generics_once(&self, trait_: ItemKey, name: Name) -> Fetch<Option<tc::AssocGenerics>> {
        let Some(lib) = self.keys.library_in(self.names, trait_) else { return Ok(None) };
        let Some(t) = self.store.maps.traits.get(&lib) else {
            return match self.resolver().detail(lib) {
                Some(ItemDetail::Trait) => self.want(trait_),
                _ => Ok(None),
            };
        };
        if let Some(&own) = self.store.maps.assoc_generics.get(&(lib, name)) {
            return Ok(Some(tc::AssocGenerics { parent: t.generics, own }));
        }
        // A synthetic associated type (super::rpitit): its trait fn's own parameters.
        let rows = self.rpitit_rows(trait_)?;
        Ok(rows.and_then(|r| r.rows.iter().find(|s| s.name == name)).map(|s| tc::AssocGenerics { parent: t.generics, own: s.own }))
    }

    /// Trait `trait_`'s synthetic associated types ([`super::rpitit::trait_rows`]): `None` for
    /// a key that is no trait, or a trait with no fn returning `impl Trait` (read from
    /// its head's member facts, so such a trait's members are never asked for here). A trait
    /// whose head or members are not made is asked for.
    fn rpitit_rows(&self, trait_: ItemKey) -> Fetch<Option<&super::rpitit::TraitRows>> {
        let Some(lib) = self.keys.library_in(self.names, trait_) else { return Ok(None) };
        let Some(t) = self.store.maps.traits.get(&lib) else {
            return match self.resolver().detail(lib) {
                Some(ItemDetail::Trait) => self.want(trait_),
                _ => Ok(None),
            };
        };
        if !super::rpitit::may_have_rows(t) {
            return Ok(None);
        }
        let Some(extra) = self.store.trait_defs.get(&trait_) else { return self.want(trait_) };
        self.rpitit_retry(trait_, || super::rpitit::trait_rows(self.store, trait_, t, &extra.assoc))
    }

    /// `make` again after each fn signature it needs ([`super::rpitit::Need::Fn`], made or
    /// asked for through [`Self::fn_row`]); a refusal is its gap naming the item (rule 19). A
    /// round makes one more signature, so the cap is never met by a whole program: met, it is
    /// the gap naming `about`.
    fn rpitit_retry<T>(&self, about: ItemKey, make: impl Fn() -> Result<T, super::rpitit::Need>) -> Fetch<T> {
        const ROUNDS: u32 = 4096;
        for _ in 0..ROUNDS {
            match make() {
                Ok(v) => return Ok(v),
                Err(super::rpitit::Need::Fn(f)) => {
                    self.fn_row(f)?;
                }
                Err(super::rpitit::Need::Refused(k, why)) => return self.gap(Gap::FnHead(k, why.into())),
            }
        }
        self.gap(Gap::FnHead(about, "its synthetic associated types (an `impl Trait` of a trait fn) needed more signatures than any trait or impl has".into()))
    }

    /// A type alias's definition from its head (`Maps::aliases`, `Maps::alias_defaults`, made
    /// by `Store::alias`): asked for while its head is not made, the gap naming it when its head
    /// was refused (as `BodyWorld::path` names it), `None` for an item that is no alias.
    fn type_alias_once(&self, item: ItemKey) -> Fetch<Option<tc::AliasDef<'_>>> {
        let Some(lib) = self.keys.library_in(self.names, item) else { return Ok(None) };
        match self.store.maps.aliases.get(&lib) {
            Some(&Some((generics, ty))) => {
                let defaults = self.store.maps.alias_defaults.get(&lib).map_or(&[][..], |d| &d[..]);
                Ok(Some(tc::AliasDef { generics, defaults, ty, types: self.types() }))
            }
            Some(None) => {
                let why = self.store.maps.alias_gaps.get(&lib).map_or_else(|| "(no why recorded)".into(), |w| format!("{w:?}").into_boxed_str());
                self.gap(Gap::AliasHead(lib, why))
            }
            None => match self.resolver().detail(lib) {
                Some(ItemDetail::TypeAlias) => self.store_want(Want::Head(lib)),
                _ => Ok(None),
            },
        }
    }
}
