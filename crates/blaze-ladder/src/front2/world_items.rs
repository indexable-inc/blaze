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
    AssocItem, AssocKind, AssocParent, Def, Fetch, FnSig, Hidden, ImplHeader, ItemKey, LangItem, Missing, Name, PTy, PTys, PathIdx, PathRes,
    Predicates, ScopeId, Seal, Shape, TraitDef, TypeAttrs, World,
};

use super::buckets::Buckets;
use super::fill::{FillError, want_of_key};
use super::heads::{Store, Want};
use super::index::Index;
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::langs::{lang_name, trait_lang};
use super::spell::{Spelling, Unspelled};
use super::types::SelfTy;
use super::source::NamesSource;
use crate::resolve::{Paths, ResolveError};

/// What the check loop makes before it checks the body again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Store(Want),
    /// The headers of the impls that can answer a trait at a shape, then the answer.
    Impls(ItemKey, Shape),
    /// The headers of the inherent impls that can answer a shape, then its fns.
    Inherent(Shape),
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
                };
                let why = match gap.why {
                    ConstGapWhy::NoEvaluator => "its converter evaluates no constant",
                    ConstGapWhy::OverGeneric => "a constant expression over a generic parameter (the checker's type form has no node for an unevaluated expression)",
                    ConstGapWhy::AnonInBody => "an anonymous constant in a body (its type is the checker's to find, or the evaluator refused it)",
                    ConstGapWhy::NegativeNoSuffix => "a negated literal with no suffix (its bits wait for the parameter's type)",
                };
                write!(f, "the {site} at {}..{} of the body {owner:?} has no value yet: {why}", gap.range.start, gap.range.end)
            }
            Gap::Unspelled(u) => write!(f, "the body names {:?}, whose text its lowering does not hold", u.0),
            Gap::Resolve(e) => write!(f, "{e}"),
            Gap::Fill(e) => write!(f, "{e}"),
            Gap::VariantOfNoEnum(k) => write!(f, "the lang variant {k:x} is in no enum of its crate"),
            Gap::FnHead(k, why) => write!(f, "the member fn {k:?} has no signature: {why}"),
            Gap::Local(g) => write!(f, "{g}"),
            Gap::UnreadImpl(k, why) => write!(f, "the impl {k:x}, a candidate filed without its trait or self type (unread as written), has no header: {why}"),
            Gap::RefusedImpl { impl_, place, why } => write!(f, "the impl {impl_:x} at {place}, a candidate of this question, has a refused header: {why}"),
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
    pub fills: RefCell<Vec<Fill, Global>>,
    pub gaps: RefCell<Vec<Gap, Global>>,
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

    /// `Missing`, noting the want that makes `key`'s record.
    pub(super) fn want<T>(&self, key: ItemKey) -> Fetch<T> {
        match want_of_key(self.names, self.keys, key) {
            Ok(w) => self.fill(Fill::Store(w)),
            Err(e) => self.gap(Gap::Fill(e)),
        }
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
    pub(super) fn ctor(&self, adt: LibKey, variant: u32) -> Fetch<Def> {
        let k = checker_of(adt);
        let Some(a) = self.store.adts.get(&k) else { return self.fill(Fill::Store(Want::Adt(adt))) };
        match a.variants.get(variant as usize) {
            Some(v) => Ok(Def::Ctor { adt: k, variant, kind: v.ctor }),
            None => self.gap(Gap::Fill(FillError::UnknownKey(k))),
        }
    }
}

impl<'n, S: NamesSource<'n>> World for BodyWorld<'_, 'n, S> {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        if let Some(f) = self.store.fns.get(&item) {
            return Ok(Some(FnSig {
                parent_count: f.parent,
                own_count: f.own,
                declared_count: f.declared,
                self_param: f.self_param,
                header: f.header,
                c_variadic: f.c_variadic,
                inputs: &f.inputs,
                output: f.output,
                types: self.types(),
            }));
        }
        if let Some(c) = self.store.ctors.get(&item) {
            return Ok(Some(FnSig {
                parent_count: 0,
                own_count: c.generics,
                declared_count: c.generics,
                self_param: false,
                header: tc::FnPtrHeader { abi: tc::Abi::Rust, safety: tc::Safety::Safe },
                c_variadic: false,
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
            // A key in no row of the map is asked of the source (`Keys::library_in`: over
            // records no item is filed up front); a key of nothing has no signature.
            Some(Origin::Item(_)) | None => match self.keys.library_in(self.names, item) {
                None => Ok(None),
                Some(lib) => match self.resolver().detail(lib) {
                    Some(ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. }) => self.want(item),
                    Some(_) => Ok(None),
                    // A variant: its enum's head makes its ctor fn.
                    None => match self.enum_of(lib) {
                        Some((e, _)) => self.ctor_owner(item, checker_of(e)),
                        None => self.gap(Gap::CtorFn(item)),
                    },
                },
            },
        }
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        // A ctor fn's where-clauses are its type's.
        let item = self.store.ctors.get(&item).map_or(item, |c| c.adt);
        match self.store.preds.get(&item) {
            Some(p) if !p.conds.is_empty() => self.gap(Gap::WhereConst(item)),
            Some(p) => Ok(Predicates { clauses: &p.clauses, types: self.types() }),
            // A trait's associated type whose bounds were refused (`Store::trait_members`
            // files the refusal under the type's own key): the gap names it and why.
            None if self.store.fn_gaps.contains_key(&item) => {
                let why = self.store.fn_gaps.get(&item).map(|w| format!("{w:?}")).unwrap_or_default();
                self.gap(Gap::FnHead(item, why.into_boxed_str()))
            }
            None => self.want(item),
        }
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<tc::Adt<'_>>> {
        if let Some(a) = self.store.adts.get(&item) {
            return Ok(Some(tc::Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.types() }));
        }
        match self.detail(item) {
            Some(ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => self.want(item),
            _ => Ok(None),
        }
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        if let Some((k, why)) = self.buckets.and_then(|b| b.trait_gap(trait_, shape)) {
            let asked = match self.keys.library_in(self.names, trait_) {
                Some(lib) => format!("the impls of the trait {lib:x} at {shape:?}"),
                None => format!("the impls of the trait {trait_:?} at {shape:?}"),
            };
            return self.gap(self.failed_impl(k, why, &asked));
        }
        match self.buckets.and_then(|b| b.impls_of(trait_, shape)) {
            Some(v) => Ok(v),
            None => self.fill(Fill::Impls(trait_, shape)),
        }
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        if let Some(i) = self.store.impls.get(&item) {
            return Ok(Some(ImplHeader {
                generics: i.generics,
                self_ty: i.self_ty,
                trait_: i.trait_,
                trait_args: i.trait_args,
                assoc_types: &i.assoc,
                types: self.types(),
                polarity: i.polarity,
            }));
        }
        match self.detail(item) {
            Some(ItemDetail::Impl) => self.want(item),
            _ => Ok(None),
        }
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        if let Some((k, why)) = self.buckets.and_then(|b| b.inherent_gap(shape)) {
            return self.gap(self.failed_impl(k, why, &format!("the inherent items of {shape:?}")));
        }
        match self.buckets.and_then(|b| b.inherent(shape, name)) {
            Some(v) => Ok(v),
            None => self.fill(Fill::Inherent(shape)),
        }
    }

    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        if let Some(i) = self.store.impls.get(&container) {
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
    fn traits_in_scope(&self, _: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        match self.blocks.at(scope.0) {
            super::block_traits::InScope::Module => Ok(self.traits),
            super::block_traits::InScope::Traits(t) => Ok(t),
            super::block_traits::InScope::Refused(gap) => self.gap(Gap::Local(gap)),
        }
    }

    fn resolve_path(&self, _: Seal, scope: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<Option<PathRes>> {
        self.path(scope, segments)
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        match super::langs::find(self.index, self.resolver(), item) {
            Ok(found) => Ok(found.map(checker_of)),
            Err(why) => self.gap(Gap::Lang(item, why)),
        }
    }

    fn lang_path(&self, _: Seal, item: LangItem) -> Fetch<Option<Def>> {
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
                            let Some(t) = self.store.maps.traits.get(&owner) else { return self.fill(Fill::Store(Want::Head(owner))) };
                            match t.fns.iter().find(|(n, _)| *n == name) {
                                Some(&(_, f)) => Ok(Some(Def::Fn(f))),
                                None => self.gap(Gap::Fill(FillError::UnknownKey(checker_of(lib)))),
                            }
                        }
                        Some(ItemDetail::Impl) => {
                            let Some(i) = self.store.impls.get(&checker_of(owner)) else { return self.fill(Fill::Store(Want::Impl(owner))) };
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

    fn const_value(&self, _: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.gap(Gap::BodyRecord(tc::Question::Const(item)))
    }

    /// An opaque type a fn's head made: its hidden type is not told (`None`): a body sees
    /// only its bounds (the Reference, "Impl trait", "Abstract return types"; auto traits that
    /// leak from the hidden type need the defining body's record: not made yet). Any other key
    /// is a gap.
    fn opaque_hidden(&self, _: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        match self.store.opaques.get(&item) {
            Some(_) => Ok(None),
            None => self.gap(Gap::BodyRecord(tc::Question::Opaque(item))),
        }
    }
    /// A closure of another body: its facts are in the owner's checked record, which this
    /// world does not hold yet (the RUN path's, typeck.md "A closure's identity"): a gap.
    fn closure(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<tc::ClosureFacts<'_>>> {
        self.gap(Gap::BodyRecord(tc::Question::Closure(owner, ordinal)))
    }
    /// No attribute is read into the store yet (`#[non_exhaustive]`, `#[fundamental]`): the
    /// open item "attrs" in the README; answered empty as the old front end did.
    /// An enum's discriminant type ([`super::heads::discr_ty`]: `isize` with no primitive
    /// repr, else the repr's integer), from its made head; a struct or union has none to give.
    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        if let Some(a) = self.store.adts.get(&item) {
            return Ok(match a.kind {
                tc::AdtKind::Enum => TypeAttrs(0).with_discr(super::heads::discr_ty(a.repr)),
                _ => TypeAttrs(0),
            });
        }
        match self.keys.library_in(self.names, item) {
            Some(lib) => self.fill(Fill::Store(Want::Adt(lib))),
            None => self.gap(Gap::Fill(FillError::UnknownKey(item))),
        }
    }

    fn trait_def(&self, _: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
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
            lang: self.index.lang_name_of(lib).and_then(trait_lang),
            supertraits: &extra.supertraits,
            assoc_types: &extra.assoc,
            types: self.types(),
            members: blaze_typeck::TraitMembers::Known(&t.members),
        }))
    }

    fn assoc_parent(&self, _: Seal, item: ItemKey) -> Fetch<Option<AssocParent>> {
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

    fn const_holds(&self, _: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<tc::ConstHolds> {
        self.gap(Gap::BodyRecord(tc::Question::ConstHolds(cond, tc::ground_args_digest(args, types))))
    }

    /// The item's const generic parameters as its head made them (`Maps::const_params`: a
    /// fn's with its container's first); an item whose head is not made has none to give, and
    /// the checker asks for its head (`fn_sig`, `adt`, `impl_header`) before its parameters.
    fn const_params(&self, _: Seal, item: ItemKey) -> Fetch<tc::ConstParams<'_>> {
        let made = self.keys.library_in(self.names, item).and_then(|lib| self.store.maps.const_params.get(&lib)).map(|c| &c[..]);
        let params = made.or_else(|| self.store.fns.get(&item).map(|f| &f.consts[..])).unwrap_or(&[]);
        Ok(tc::ConstParams { params, types: self.types() })
    }

    /// The generics of the trait's associated type `name` as the trait's head counted them
    /// (`Maps::assoc_generics`, filled by `Store::trait_head`: type and const parameters; a
    /// lifetime is none, as a portable type holds no region): the Reference, "Associated
    /// Types": "Associated types may include generic parameters and where clauses". A trait
    /// whose head is not made is asked for; a trait that declares no such type has no count.
    fn assoc_generics(&self, _: Seal, trait_: ItemKey, name: Name) -> Fetch<Option<tc::AssocGenerics>> {
        let Some(lib) = self.keys.library_in(self.names, trait_) else { return Ok(None) };
        let Some(t) = self.store.maps.traits.get(&lib) else {
            return match self.resolver().detail(lib) {
                Some(ItemDetail::Trait) => self.want(trait_),
                _ => Ok(None),
            };
        };
        Ok(self.store.maps.assoc_generics.get(&(lib, name)).map(|&own| tc::AssocGenerics { parent: t.generics, own }))
    }
}
