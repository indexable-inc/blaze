//! `World`, the only way the driver learns anything outside the body, and `Reads`, the sealed
//! recorder that is the only way to ask it (typeck.md, "`World`: the only way out of the body").
//!
//! Every method of `World` takes a [`Seal`]. A `Seal` is made only in this module, by `Reads`,
//! which records the question and the content hash of the answer before it hands the answer
//! on. An implementation of `World` can write its methods and cannot call them; the rule
//! modules hold `&mut Reads` and neither the world nor a way to make a `Seal`. A read that is
//! not in the read list cannot be written outside this file.

#![blaze::sans_io]

use blaze_own::{Alloc, Soa, Vec};

use crate::columns::columns_eq;
use crate::error::Stop;
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{FloatTy, IntTy, ItemKey, LangItem, List, Mutbl, Name, PathIdx, ScopeId, UintTy};
use crate::portable::{PTy, PTys};

/// The proof that a call to a `World` method is made by the recorder. Its field is private to
/// this module, so nothing else can make one.
pub struct Seal {
    _private: (),
}

/// The world has no answer for this question yet: the run stops with `Outcome::Need`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missing;

/// An answer, or `Missing`.
pub type Fetch<T> = Result<T, Missing>;

/// The shape of a self type an impl lookup is bucketed by (rustc's `SimplifiedType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(u32),
    Ref(Mutbl),
    Ptr(Mutbl),
    Array,
    Slice,
    Adt(ItemKey),
    FnDef(ItemKey),
    FnPtr(u32),
    Closure,
    Param,
    Opaque(ItemKey),
}

/// A question to the world: what a read list holds and a `Need` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Question {
    FnSig(ItemKey),
    Predicates(ItemKey),
    Adt(ItemKey),
    ImplsOf(ItemKey, Shape),
    ImplHeader(ItemKey),
    Inherent(Shape, Name),
    AssocItem(ItemKey, Name),
    TraitsInScope(ScopeId),
    Path(ScopeId, PathIdx),
    Lang(LangItem),
    Const(ItemKey),
    Opaque(ItemKey),
    Attrs(ItemKey),
}

/// A fn item's signature (a method's too), in its own portable table.
#[derive(Clone, Copy, Debug)]
pub struct FnSig<'w> {
    /// Generic parameters of the parent (impl or trait, `Self` included), which come first.
    pub parent_count: u32,
    /// The fn's own generic parameters.
    pub own_count: u32,
    /// Whether the first input is a `self` parameter (a method).
    pub self_param: bool,
    pub inputs: &'w [PTy],
    pub output: PTy,
    pub types: PTys<'w>,
}

/// A where-clause, its types in the answer's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PClause {
    Trait { trait_: ItemKey, args: List },
    Projection { trait_: ItemKey, args: List, name: Name, term: PTy },
}

/// An item's where-clauses, own and inherited, its own generics' implied `Sized` included.
#[derive(Clone, Copy, Debug)]
pub struct Predicates<'w> {
    pub clauses: &'w [PClause],
    pub types: PTys<'w>,
}

/// What kind of ADT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdtKind {
    Struct,
    Enum,
    Union,
}

/// How a variant is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CtorKind {
    /// `S { .. }` only.
    Braces,
    /// `S(..)`: the ctor is a fn item.
    Tuple,
    /// `S`: the ctor is a value.
    Unit,
}

/// One variant of an ADT (a struct has one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Variant {
    pub key: ItemKey,
    pub name: Name,
    pub ctor: CtorKind,
    /// Its fields in `Adt::fields`.
    pub fields: List,
}

/// One field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    pub name: Name,
    pub ty: PTy,
}

/// An ADT's definition, as typing needs it.
#[derive(Clone, Copy, Debug)]
pub struct Adt<'w> {
    pub kind: AdtKind,
    pub generics: u32,
    pub variants: &'w [Variant],
    pub fields: &'w [Field],
    pub types: PTys<'w>,
}

/// An impl's header.
#[derive(Clone, Copy, Debug)]
pub struct ImplHeader<'w> {
    pub generics: u32,
    pub self_ty: PTy,
    /// The trait, for a trait impl; `None` for an inherent impl.
    pub trait_: Option<ItemKey>,
    /// The trait's arguments after `Self`.
    pub trait_args: List,
    /// Associated types the impl defines.
    pub assoc_types: &'w [(Name, PTy)],
    pub types: PTys<'w>,
}

/// What kind of associated item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssocKind {
    Fn { self_param: bool },
    Const,
    Type,
}

/// An associated item of a trait or an impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocItem {
    /// The impl or trait it is in.
    pub container: ItemKey,
    pub item: ItemKey,
    pub kind: AssocKind,
}

/// What a value or type path written in the body resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Def {
    Fn(ItemKey),
    Const(ItemKey),
    Static(ItemKey, Mutbl),
    /// A struct's or variant's ctor or name; `variant` is its index in the ADT.
    Ctor { adt: ItemKey, variant: u32, kind: CtorKind },
    /// A type: an ADT's name (for a struct literal or a type-relative path).
    Adt(ItemKey),
    Trait(ItemKey),
    /// A generic parameter of the body's item.
    TyParam(u32),
}

/// A path's resolution: the definition and how many segments it consumed. Segments after
/// `consumed` are type-relative, resolved by the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PathRes {
    pub def: Def,
    pub consumed: u32,
}

/// Attributes that change typing, as bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct TypeAttrs(pub u32);

impl TypeAttrs {
    pub const NON_EXHAUSTIVE: u32 = 1;
    pub const FUNDAMENTAL: u32 = 2;
}

/// An opaque type's hidden type.
#[derive(Clone, Copy, Debug)]
pub struct Hidden<'w> {
    pub ty: PTy,
    pub types: PTys<'w>,
}

/// Everything the driver may learn from outside the body. Implemented by the item layer over
/// its records (`blaze-surface`'s `Items`), and by the tests' toy world. Each method answers
/// one [`Question`], or `Missing`. It must be a function of the records: two calls with equal
/// arguments give equal answers.
///
/// The `Seal` argument means only `Reads` can call these; an implementation never calls its
/// own methods for the driver.
pub trait World {
    fn fn_sig(&self, seal: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>>;
    fn predicates(&self, seal: Seal, item: ItemKey) -> Fetch<Predicates<'_>>;
    fn adt(&self, seal: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>>;
    /// The impls of `trait_` whose self type may have `shape`, blanket impls included, in an
    /// order of the records (by item key).
    fn impls_of(&self, seal: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]>;
    fn impl_header(&self, seal: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>>;
    /// Associated items named `name` of inherent impls whose self type may have `shape`.
    fn inherent(&self, seal: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]>;
    fn assoc_item(&self, seal: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>>;
    /// The traits in scope at a block of the body, sorted by key.
    fn traits_in_scope(&self, seal: Seal, scope: ScopeId) -> Fetch<&[ItemKey]>;
    /// The resolution of a path written at `scope`, by its segments' names.
    fn resolve_path(
        &self,
        seal: Seal,
        scope: ScopeId,
        path: PathIdx,
        segments: &[Name],
    ) -> Fetch<Option<PathRes>>;
    fn lang_item(&self, seal: Seal, item: LangItem) -> Fetch<Option<ItemKey>>;
    fn const_value(&self, seal: Seal, item: ItemKey) -> Fetch<Option<u64>>;
    fn opaque_hidden(&self, seal: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>>;
    fn attrs(&self, seal: Seal, item: ItemKey) -> Fetch<TypeAttrs>;
    /// The questions the last check of this body asked, before the walk starts: fetch them in
    /// one batch. Answers nothing.
    fn prefetch(&self, seal: Seal, questions: &[Question]) {
        let _ = (seal, questions);
    }
}

/// The read list: each question asked and the content hash of its answer, sorted by question
/// and deduplicated at the end of the run. A lookup scans the question column only.
#[derive(Clone)]
pub struct ReadList<A: Allocator + Clone> {
    pub rows: Soa<(Question, u64), A>,
}

columns_eq!(Needs { questions });

impl<A: Allocator + Clone> PartialEq for ReadList<A> {
    fn eq(&self, other: &ReadList<A>) -> bool {
        self.rows.column::<0>() == other.rows.column::<0>() && self.rows.column::<1>() == other.rows.column::<1>()
    }
}

impl<A: Allocator + Clone> Eq for ReadList<A> {}

impl<A: Allocator + Clone> core::fmt::Debug for ReadList<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadList").field("rows", &self.rows).finish()
    }
}

impl<A: Allocator + Clone> ReadList<A> {
    /// The questions read, sorted.
    pub fn questions(&self) -> &[Question] {
        self.rows.column::<0>()
    }

    /// The hash recorded for `q`, if it was read.
    pub fn hash_of(&self, q: &Question) -> Option<u64> {
        match self.rows.column::<0>().binary_search(q) {
            Ok(i) => Some(self.rows.column::<1>()[i]),
            Err(_) => None,
        }
    }

    /// Whether `q` was read.
    pub fn holds(&self, q: &Question) -> bool {
        self.hash_of(q).is_some()
    }
}

/// The questions a check needed: what the next check of the body fetches up front.
#[derive(Clone)]
pub struct Needs<A: Allocator + Clone> {
    pub questions: Vec<Question, A>,
}

impl<A: Allocator + Clone> Needs<A> {
    /// No needs.
    pub fn new_in(alloc: Alloc<'a>) -> Needs<'a> {
        Needs { questions: Vec::new_in(alloc) }
    }
}

/// The recorder: owns the world's borrow and the read list. The only caller of `World`.
pub struct Reads<'w, 'a, W: World> {
    world: &'w W,
    entries: Vec<(Question, u64), A>,
}

fn hash_of(domain: &str, v: &impl ContentHash) -> u64 {
    let mut h = StableHasher::new(domain);
    v.hash_into(&mut h);
    h.finish()
}

impl<'w, 'a, W: World> Reads<'w, 'a, W> {
    /// The recorder for one run.
    pub(crate) fn new_in(world: &'w W, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        Reads { world, entries: Vec::new_in(alloc) }
    }

    fn seal() -> Seal {
        Seal { _private: () }
    }

    fn record<T: ContentHash>(&mut self, q: Question, got: Fetch<T>) -> Result<T, Stop> {
        match got {
            Ok(v) => {
                self.entries.push((q, hash_of("answer", &v)));
                Ok(v)
            }
            Err(Missing) => Err(Stop::Need(q)),
        }
    }

    /// Hands the last run's questions to the world.
    pub(crate) fn prefetch(&mut self, questions: &[Question]) {
        self.world.prefetch(Self::seal(), questions);
    }

    pub fn fn_sig(&mut self, item: ItemKey) -> Result<Option<FnSig<'w>>, Stop> {
        let got = self.world.fn_sig(Self::seal(), item);
        self.record(Question::FnSig(item), got)
    }

    pub fn predicates(&mut self, item: ItemKey) -> Result<Predicates<'w>, Stop> {
        let got = self.world.predicates(Self::seal(), item);
        self.record(Question::Predicates(item), got)
    }

    pub fn adt(&mut self, item: ItemKey) -> Result<Option<Adt<'w>>, Stop> {
        let got = self.world.adt(Self::seal(), item);
        self.record(Question::Adt(item), got)
    }

    pub fn impls_of(&mut self, trait_: ItemKey, shape: Shape) -> Result<&'w [ItemKey], Stop> {
        let got = self.world.impls_of(Self::seal(), trait_, shape);
        self.record(Question::ImplsOf(trait_, shape), got)
    }

    pub fn impl_header(&mut self, item: ItemKey) -> Result<Option<ImplHeader<'w>>, Stop> {
        let got = self.world.impl_header(Self::seal(), item);
        self.record(Question::ImplHeader(item), got)
    }

    pub fn inherent(&mut self, shape: Shape, name: Name) -> Result<&'w [AssocItem], Stop> {
        let got = self.world.inherent(Self::seal(), shape, name);
        self.record(Question::Inherent(shape, name), got)
    }

    pub fn assoc_item(&mut self, container: ItemKey, name: Name) -> Result<Option<AssocItem>, Stop> {
        let got = self.world.assoc_item(Self::seal(), container, name);
        self.record(Question::AssocItem(container, name), got)
    }

    pub fn traits_in_scope(&mut self, scope: ScopeId) -> Result<&'w [ItemKey], Stop> {
        let got = self.world.traits_in_scope(Self::seal(), scope);
        self.record(Question::TraitsInScope(scope), got)
    }

    pub fn resolve_path(
        &mut self,
        scope: ScopeId,
        path: PathIdx,
        segments: &[Name],
    ) -> Result<Option<PathRes>, Stop> {
        let got = self.world.resolve_path(Self::seal(), scope, path, segments);
        self.record(Question::Path(scope, path), got)
    }

    pub fn lang_item(&mut self, item: LangItem) -> Result<Option<ItemKey>, Stop> {
        let got = self.world.lang_item(Self::seal(), item);
        self.record(Question::Lang(item), got)
    }

    pub fn const_value(&mut self, item: ItemKey) -> Result<Option<u64>, Stop> {
        let got = self.world.const_value(Self::seal(), item);
        self.record(Question::Const(item), got)
    }

    pub fn opaque_hidden(&mut self, item: ItemKey) -> Result<Option<Hidden<'w>>, Stop> {
        let got = self.world.opaque_hidden(Self::seal(), item);
        self.record(Question::Opaque(item), got)
    }

    pub fn attrs(&mut self, item: ItemKey) -> Result<TypeAttrs, Stop> {
        let got = self.world.attrs(Self::seal(), item);
        self.record(Question::Attrs(item), got)
    }

    /// The test build's one backdoor: an ADT read that is not recorded. Only the planted bug
    /// of the read-list property calls it, to show the oracle catches what the seal prevents.
    #[cfg(test)]
    pub(crate) fn peek_unrecorded_adt(&self, item: ItemKey) -> Fetch<Option<Adt<'w>>> {
        self.world.adt(Self::seal(), item)
    }

    /// The read list and the needs of the run, sorted and deduplicated (rule 25).
    pub(crate) fn finish(mut self, alloc: Alloc<'a>) -> (ReadList<'a>, Needs<'a>) {
        self.entries.sort_unstable();
        self.entries.dedup();
        let mut rows = Soa::with_capacity_in(self.entries.len(), alloc);
        let mut needs = Vec::with_capacity_in(self.entries.len(), alloc);
        for &(q, h) in self.entries.iter() {
            rows.push((q, h));
            if needs.last() != Some(&q) {
                needs.push(q);
            }
        }
        (ReadList { rows }, Needs { questions: needs })
    }

    /// The needs of a run stopped by the missing answer to `missing`: every question asked
    /// before it and `missing` itself, sorted, so the next run fetches them in one batch.
    pub(crate) fn finish_need(self, missing: Question, alloc: Alloc<'a>) -> Needs<'a> {
        let mut questions = Vec::with_capacity_in(self.entries.len() + 1, alloc);
        for &(q, _) in self.entries.iter() {
            questions.push(q);
        }
        questions.push(missing);
        questions.sort_unstable();
        questions.dedup();
        Needs { questions }
    }
}

// The content of every answer: what a read list hashes.

impl ContentHash for FnSig<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.parent_count);
        h.u32(self.own_count);
        h.tag(u8::from(self.self_param));
        self.inputs.hash_into(h);
        h.u32(self.output.0);
        self.types.hash_into(h);
    }
}

impl ContentHash for PClause {
    fn hash_into(&self, h: &mut StableHasher) {
        match *self {
            PClause::Trait { trait_, args } => {
                h.tag(0);
                h.u128(trait_.0);
                h.u32(args.start);
                h.u32(args.len);
            }
            PClause::Projection { trait_, args, name, term } => {
                h.tag(1);
                h.u128(trait_.0);
                h.u32(args.start);
                h.u32(args.len);
                h.u64(name.0);
                h.u32(term.0);
            }
        }
    }
}

impl ContentHash for Predicates<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        self.clauses.hash_into(h);
        self.types.hash_into(h);
    }
}

impl ContentHash for Variant {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u128(self.key.0);
        h.u64(self.name.0);
        h.tag(self.ctor as u8);
        h.u32(self.fields.start);
        h.u32(self.fields.len);
    }
}

impl ContentHash for Field {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.name.0);
        h.u32(self.ty.0);
    }
}

impl ContentHash for Adt<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(self.kind as u8);
        h.u32(self.generics);
        self.variants.hash_into(h);
        self.fields.hash_into(h);
        self.types.hash_into(h);
    }
}

impl ContentHash for &[ItemKey] {
    fn hash_into(&self, h: &mut StableHasher) {
        (**self).hash_into(h);
    }
}

impl ContentHash for ImplHeader<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        h.u32(self.self_ty.0);
        self.trait_.hash_into(h);
        h.u32(self.trait_args.start);
        h.u32(self.trait_args.len);
        h.u64(self.assoc_types.len() as u64);
        for (n, t) in self.assoc_types {
            h.u64(n.0);
            h.u32(t.0);
        }
        self.types.hash_into(h);
    }
}

impl ContentHash for AssocItem {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u128(self.container.0);
        h.u128(self.item.0);
        match self.kind {
            AssocKind::Fn { self_param } => {
                h.tag(0);
                h.tag(u8::from(self_param));
            }
            AssocKind::Const => h.tag(1),
            AssocKind::Type => h.tag(2),
        }
    }
}

impl ContentHash for &[AssocItem] {
    fn hash_into(&self, h: &mut StableHasher) {
        (**self).hash_into(h);
    }
}

impl ContentHash for PathRes {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.consumed);
        match self.def {
            Def::Fn(k) => {
                h.tag(0);
                h.u128(k.0);
            }
            Def::Const(k) => {
                h.tag(1);
                h.u128(k.0);
            }
            Def::Static(k, m) => {
                h.tag(2);
                h.u128(k.0);
                h.tag(m as u8);
            }
            Def::Ctor { adt, variant, kind } => {
                h.tag(3);
                h.u128(adt.0);
                h.u32(variant);
                h.tag(kind as u8);
            }
            Def::Adt(k) => {
                h.tag(4);
                h.u128(k.0);
            }
            Def::Trait(k) => {
                h.tag(5);
                h.u128(k.0);
            }
            Def::TyParam(i) => {
                h.tag(6);
                h.u32(i);
            }
        }
    }
}

impl ContentHash for TypeAttrs {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.0);
    }
}

impl ContentHash for Hidden<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.ty.0);
        self.types.hash_into(h);
    }
}
