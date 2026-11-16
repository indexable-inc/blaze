//! `World`, the only way the driver learns anything outside the body, and `Reads`, the sealed
//! recorder that is the only way to ask it (typeck.md, "`World`: the only way out of the body").
//!
//! Every method of `World` takes a [`Seal`]. A `Seal` is made only in this module, by `Reads`,
//! which records the question and the content hash of the answer before it hands the answer
//! on. An implementation of `World` can write its methods and cannot call them; the rule
//! modules hold `&mut Reads` and neither the world nor a way to make a `Seal`. A read that is
//! not in the read list cannot be written outside this file.

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_own::{Alloc, Soa, Vec};

use crate::asks::{Ask, AskLog, Clock, Cursor, Tally, Tallying, question_kind};
use crate::body::BodyView;
use crate::columns::columns_eq;
use crate::error::{CheckError, Malformed, Stop};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{FloatTy, FnPtrHeader, IntTy, ItemKey, LangItem, List, Mutbl, Name, PathIdx, ScopeId, UintTy};
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
    /// A pattern type (typeck.md "Pattern types"): its impls are its own (core's pat.rs).
    Pat,
    Adt(ItemKey),
    FnDef(ItemKey),
    FnPtr(u32),
    Closure,
    Param,
    Opaque(ItemKey),
    /// An extern type.
    Foreign(ItemKey),
    /// A trait object, by its principal trait (`None`: auto traits only).
    Dynamic(Option<ItemKey>),
    /// An integer of a type not yet known (`{integer}`): the impls for every integer type,
    /// blanket impls included.
    AnyInt,
    /// A float of a type not yet known (`{float}`).
    AnyFloat,
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
    /// The definition a desugaring's path names (`BodyPath::lang`).
    LangPath(LangItem),
    Const(ItemKey),
    Opaque(ItemKey),
    Attrs(ItemKey),
    TraitDef(ItemKey),
    AssocParent(ItemKey),
    /// A `where const` condition (its anonymous constant) at ground arguments, named by the
    /// content digest of their portable table ([`ground_args_digest`]): the record is keyed by
    /// the condition's code identity and the arguments, so each pair is evaluated once.
    ConstHolds(ItemKey, u64),
    /// The const generic parameters of an item's generics ([`ConstParams`]).
    ConstParams(ItemKey),
    /// The facts of closure `ordinal` of `owner`'s body ([`ClosureFacts`]): a body record, the
    /// owner's `Checked::closures` row (typeck.md, "A closure's identity").
    Closure(ItemKey, u32),
}

/// Whether a question's record is made from items alone or from another body's check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordKind {
    /// Signatures, generics, where-clauses, ADT definitions, impl headers and buckets, trait
    /// definitions, paths, lang items, attributes: settled before any body of their scope is
    /// queued (the kind-order gate), so a body never waits on one once the gate exists.
    Item,
    /// A fact only another body's check makes: an opaque type's hidden type, a const's value.
    Body,
}

impl Question {
    /// The kind of record the question reads (typeck.md, "Item records and body records").
    pub const fn record_kind(self) -> RecordKind {
        match self {
            Question::Opaque(_) | Question::Const(_) | Question::ConstHolds(..) | Question::Closure(..) => {
                RecordKind::Body
            }
            Question::FnSig(_)
            | Question::Predicates(_)
            | Question::Adt(_)
            | Question::ImplsOf(..)
            | Question::ImplHeader(_)
            | Question::Inherent(..)
            | Question::AssocItem(..)
            | Question::TraitsInScope(_)
            | Question::Path(..)
            | Question::Lang(_)
            | Question::LangPath(_)
            | Question::Attrs(_)
            | Question::TraitDef(_)
            | Question::AssocParent(_)
            | Question::ConstParams(_) => RecordKind::Item,
        }
    }
}

/// A trait's definition: what the solver reads of a trait (its `TraitDef`).
#[derive(Clone, Copy, Debug)]
pub struct TraitDef<'w> {
    /// Generic parameters, `Self` (index 0) included.
    pub generics: u32,
    pub flags: TraitFlags,
    /// The lang item the trait is, if any (`#[lang = ".."]`).
    pub lang: Option<LangItem>,
    /// The supertrait bounds and the trait's where-clauses on `Self`, `Self` as `Param(0)`:
    /// what elaborating a `T: Trait` bound adds.
    pub supertraits: &'w [PClause],
    pub assoc_types: &'w [AssocTyDef],
    pub types: PTys<'w>,
    /// Its members' facts for dyn compatibility (`crate::dyn_compat`, the rule's one home).
    pub members: TraitMembers<'w>,
}

/// A trait's members as the world read them, or a typed "not known yet": a `dyn` of a trait
/// whose members are not known is a named gap (`CheckError::DynUndecided`), never compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraitMembers<'w> {
    Known(&'w [crate::dyn_compat::MemberFacts]),
    NotKnown,
}

/// A trait's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct TraitFlags(pub u32);

impl TraitFlags {
    /// `auto trait`.
    pub const AUTO: u32 = 1;
    /// `#[marker]`: impls may overlap.
    pub const MARKER: u32 = 1 << 1;
    pub const UNSAFE: u32 = 1 << 2;
    /// `#[fundamental]`.
    pub const FUNDAMENTAL: u32 = 1 << 3;
    /// `#[rustc_coinductive]` (and every auto trait).
    pub const COINDUCTIVE: u32 = 1 << 4;
    /// `#[const_trait]`.
    pub const CONST: u32 = 1 << 5;

    pub const fn has(self, bits: u32) -> bool {
        self.0 & bits != 0
    }
}

/// An associated type of a trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocTyDef {
    pub name: Name,
    pub item: ItemKey,
    /// The default (`type A = T;` in the trait), in the trait's table.
    pub default: Option<PTy>,
}

/// Where an associated item is: its trait or impl, and its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocParent {
    pub container: ItemKey,
    pub name: Name,
    pub kind: AssocKind,
}

/// The const generic parameters of an item (the Reference, "Generic parameters": `const N:
/// usize`): over all its generics, the parent's (impl or trait) first, each by its index in
/// that full list (the index `PNode::Param` and `PNode::ConstParam` use) with its declared
/// type in `types`. A parameter not listed is a type parameter. The declared type is an
/// integer, `bool` or `char` (the only types stable Rust admits), so it names no parameter.
#[derive(Clone, Copy, Debug)]
pub struct ConstParams<'w> {
    /// Sorted by index.
    pub params: &'w [(u32, PTy)],
    pub types: PTys<'w>,
}

impl ConstParams<'_> {
    /// The declared type of parameter `index`, when it is a const parameter.
    pub fn type_of(&self, index: u32) -> Option<PTy> {
        self.params.iter().find(|(i, _)| *i == index).map(|&(_, t)| t)
    }
}

/// A fn item's signature (a method's too), in its own portable table.
#[derive(Clone, Copy, Debug)]
pub struct FnSig<'w> {
    /// Generic parameters of the parent (impl or trait, `Self` included), which come first.
    pub parent_count: u32,
    /// The fn's own generic parameters.
    pub own_count: u32,
    /// How many of the own parameters are declared in `<..>` (the first ones); the rest, to
    /// `own_count`, are the anonymous parameters of its `impl Trait` arguments, which explicit
    /// generic arguments never give (the Reference, "Impl trait": "explicit generic arguments
    /// cannot be given for an anonymous type parameter"). At most `own_count`.
    pub declared_count: u32,
    /// Whether the first input is a `self` parameter (a method).
    pub self_param: bool,
    /// The fn's ABI and safety (`unsafe fn`, `extern "C" fn`): the header of the fn pointer
    /// it reifies to.
    pub header: FnPtrHeader,
    pub inputs: &'w [PTy],
    pub output: PTy,
    pub types: PTys<'w>,
}

/// A where-clause, its types in the answer's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PClause {
    Trait { trait_: ItemKey, args: List },
    Projection { trait_: ItemKey, args: List, name: Name, term: PTy },
    /// `where const { COND }`: `cond` is the condition's anonymous constant, whose generics are
    /// its item's; `args` are those generics as this item's table writes them (the identity
    /// for the item's own clause), substituted as a trait clause's are.
    Const { cond: ItemKey, args: List },
}

/// The value of a `where const` condition at ground arguments: blaze-solve's answer type,
/// one home (rule 30).
pub use blaze_solve::facts::ConstHolds;

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
    /// The trait's arguments after `Self`: a type of its own, so a self-first list cannot be
    /// handed in by mistake (it once was, and every trait goal silently matched no impl).
    pub trait_args: ArgsAfterSelf,
    /// Associated types the impl defines.
    pub assoc_types: &'w [(Name, PTy)],
    pub types: PTys<'w>,
    /// `impl Trait for T` is positive, `impl !Trait for T` negative (core's
    /// `impl<T: PointeeSized> !Send for *const T`): it says the trait does NOT hold, which also
    /// keeps an auto trait from being proven by the type's parts. `#[rustc_reservation_impl]`
    /// is a reservation. The solver's own type: one home for the concept.
    pub polarity: ImplPolarity,
}

pub use blaze_solve::facts::ImplPolarity;

/// A trait's arguments AFTER `Self` (`[A, B]` of `impl Trait<A, B> for T`), as a list of the
/// answer's table. Built only by [`ArgsAfterSelf::split`], which takes a trait reference's
/// whole list (`Self` first, the way a where-clause writes it) apart against the self type,
/// or as [`ArgsAfterSelf::NONE`]. Where a world's answer enters the checker
/// ([`Reads::impl_header`]), the count is checked against the trait's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct ArgsAfterSelf(List);

/// Why a trait reference's list could not be taken apart into `Self` and the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitSelf {
    /// The list is not in the table.
    OutsideTable,
    /// The list is empty: no `Self`.
    NoSelf,
    /// The list's first argument is not the self type it was split against.
    OtherSelf { found: PTy, self_ty: PTy },
}

impl ArgsAfterSelf {
    /// A trait with no parameters besides `Self`.
    pub const NONE: ArgsAfterSelf = ArgsAfterSelf(List::EMPTY);

    /// Takes `with_self` (`[Self, A, B]`, a trait reference's whole list) apart: its first
    /// argument must be `self_ty`; the rest is the list after `Self`, the same table's slice.
    pub fn split(types: PTys<'_>, with_self: List, self_ty: PTy) -> Result<ArgsAfterSelf, SplitSelf> {
        let all = types.list(with_self).map_err(|_| SplitSelf::OutsideTable)?;
        match all.first() {
            None => Err(SplitSelf::NoSelf),
            Some(&found) if found != self_ty => Err(SplitSelf::OtherSelf { found, self_ty }),
            Some(_) => Ok(ArgsAfterSelf(List { start: with_self.start + 1, len: with_self.len - 1 })),
        }
    }

    /// A list the checker's own tests write as is, so a test can hand in a wrong one.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const fn unchecked(l: List) -> ArgsAfterSelf {
        ArgsAfterSelf(l)
    }

    /// The list after `Self`.
    pub const fn list(self) -> List {
        self.0
    }
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
    /// A built-in type named by its path (the Reference, "Primitive types"; "Paths": `u32` in
    /// `u32::MAX` is a type, and the segment after it is its associated item).
    Prim(Prim),
}

/// A built-in type a path can name: the first segment of `u32::MAX`, `str::len`,
/// `char::from_u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prim {
    Bool,
    Char,
    Str,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
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
    /// Bits 8..12: an enum's discriminant type ([`DiscrRepr`]); 0 is `Unread`.
    const DISCR: u32 = 0xf << 8;

    /// These attributes with an enum's discriminant type set.
    pub const fn with_discr(self, d: DiscrRepr) -> Self {
        let code = match d {
            DiscrRepr::Unread => 0,
            DiscrRepr::Int(i) => 1 + i as u32,
            DiscrRepr::Uint(u) => 7 + u as u32,
        };
        TypeAttrs((self.0 & !Self::DISCR) | (code << 8))
    }

    /// An enum's discriminant type, as the world read it.
    pub const fn discr(self) -> DiscrRepr {
        const I: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
        const U: [UintTy; 6] = [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
        match (self.0 & Self::DISCR) >> 8 {
            c @ 1..=6 => DiscrRepr::Int(I[c as usize - 1]),
            c @ 7..=12 => DiscrRepr::Uint(U[c as usize - 7]),
            _ => DiscrRepr::Unread,
        }
    }
}

/// An enum's discriminant type, answered with its [`TypeAttrs`] (`TypeAttrs::with_discr`).
/// Reference, "Enumerations", "Discriminants": "Under the default representation, the
/// discriminant is interpreted as an `isize` value" (write `Int(Isize)`); under a primitive
/// representation, `#[repr(u8)]` and the rest ("Type Layout", "Primitive representations"), it
/// is that integer. `Unread`: the world does not read `repr` yet; the checker then stops on
/// `<E as DiscriminantKind>::Discriminant` naming the enum, never guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiscrRepr {
    Unread,
    Int(IntTy),
    Uint(UintTy),
}

/// A closure's facts, from its owner's check (`Checked::closures`, the row at its ordinal): its
/// kind, the strongest `Fn*` it implements, and its signature, a `FnPtr` in the owner's generics
/// (`Param(i)` the owner's parameter `i`). Its captures stay in the owner's `Checked::captures`.
#[derive(Clone, Copy, Debug)]
pub struct ClosureFacts<'w> {
    pub kind: crate::ty::ClosureKind,
    pub sig: PTy,
    pub types: PTys<'w>,
}

impl Tabled for ClosureFacts<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.sig));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        ClosureFacts { types: PTys::EMPTY, ..*self }
    }
}

impl ContentHash for ClosureFacts<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(self.kind as u8);
        h.u32(self.sig.0);
        self.types.hash_into(h);
    }
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
    /// The definition a desugaring's path to `item` names (`BodyPath::lang`): `Def::Fn` for a
    /// fn or a trait's fn (`Try::branch`), `Def::Ctor` for a variant (`ControlFlow::Continue`),
    /// `Def::Adt` for a struct (`Range`); `None` when the crate graph declares no such item.
    /// A variant's ctor key answers `fn_sig` as rustc's ctor fn: the ADT's generics as its own,
    /// the fields as inputs, the ADT as output.
    fn lang_path(&self, seal: Seal, item: LangItem) -> Fetch<Option<Def>>;
    fn const_value(&self, seal: Seal, item: ItemKey) -> Fetch<Option<u64>>;
    fn opaque_hidden(&self, seal: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>>;
    /// Closure `ordinal` of `owner`'s body (`None`: the checked owner has no such closure;
    /// `Missing` until the owner is checked).
    fn closure(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<ClosureFacts<'_>>>;
    fn attrs(&self, seal: Seal, item: ItemKey) -> Fetch<TypeAttrs>;
    /// The definition of trait `trait_` (`None`: not a trait).
    fn trait_def(&self, seal: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>>;
    /// The container and name of an associated item (`None`: not an associated item).
    fn assoc_parent(&self, seal: Seal, item: ItemKey) -> Fetch<Option<AssocParent>>;
    /// The `where const` condition `cond` (an anonymous constant) at `args`, ground types of
    /// `types` (no parameter, no inference variable): `True` or `False` from its value, `Error`
    /// when evaluating it failed (a panic included: never taken as false). `Missing` is the
    /// evaluator's own need.
    fn const_holds(&self, seal: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<ConstHolds>;
    /// The const generic parameters of `item`'s generics, the parent's first ([`ConstParams`]).
    fn const_params(&self, seal: Seal, item: ItemKey) -> Fetch<ConstParams<'_>>;
    /// The questions the last check of this body asked, before the walk starts: fetch them in
    /// one batch. Answers nothing.
    fn prefetch(&self, seal: Seal, questions: &[Question]) {
        let _ = (seal, questions);
    }
}

/// The read list: each question asked and the content hash of its answer, sorted by question
/// and deduplicated at the end of the run. A lookup scans the question column only.
#[derive(Clone)]
pub struct ReadList<'a> {
    pub rows: Soa<(Question, u64), Alloc<'a>>,
}

columns_eq!(Needs { questions });

impl<'a> PartialEq for ReadList<'a> {
    fn eq(&self, other: &ReadList<'a>) -> bool {
        self.rows.column::<0>() == other.rows.column::<0>() && self.rows.column::<1>() == other.rows.column::<1>()
    }
}

impl<'a> Eq for ReadList<'a> {}

impl<'a> core::fmt::Debug for ReadList<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadList").field("rows", &self.rows).finish()
    }
}

impl<'a> ReadList<'a> {
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
pub struct Needs<'a> {
    pub questions: Vec<Question, Alloc<'a>>,
}

impl<'a> Needs<'a> {
    /// No needs.
    pub fn new_in(alloc: Alloc<'a>) -> Needs<'a> {
        Needs { questions: Vec::new_in(alloc) }
    }
}

/// The recorder: owns the world's borrow and the read list. The only caller of `World`.
pub struct Reads<'w, 'a, W: World> {
    world: &'w W,
    entries: Vec<(Question, u64), Alloc<'a>>,
    /// The ask log, when the run measures re-entry (asks.rs); `None` otherwise.
    log: Option<AskRecorder<'w, 'a>>,
    /// The tally, when the run counts (asks.rs `Tally`); `None` otherwise.
    tally: Option<Tallying<'w>>,
    /// The digests of the record tables' terms this run has hashed (`rooted_hash`).
    hashes: Hashes<'a>,
}

/// A run's memo of term digests: a record table's node (the table's number, its index) to
/// the digest of the terms under it. It lives for one run (one `Reads`), so nothing in it
/// outlives the answers it was read from.
pub(crate) struct Hashes<'a> {
    alloc: Alloc<'a>,
    seen: HashMap<(usize, usize), u32, Alloc<'a>>,
    memo: HashMap<(u32, u32), u64, Alloc<'a>>,
    roots: Vec<Root, Alloc<'a>>,
    work: Vec<(u32, bool), Alloc<'a>>,
    kids: Vec<u32, Alloc<'a>>,
}

impl<'a> Hashes<'a> {
    fn new_in(alloc: Alloc<'a>) -> Hashes<'a> {
        Hashes {
            alloc,
            seen: HashMap::new_in(alloc),
            memo: HashMap::new_in(alloc),
            roots: Vec::new_in(alloc),
            work: Vec::new_in(alloc),
            kids: Vec::new_in(alloc),
        }
    }

    /// The digest of `root` of table number `tab` (`t`): its node, then its children's
    /// digests in order; each node hashed once per run, children first, by a work list. A
    /// child must come before its parent in the table, so the walk ends.
    fn term(&mut self, tab: u32, t: PTys<'_>, root: u32) -> Result<u64, CheckError> {
        if let Some(&d) = self.memo.get(&(tab, root)) {
            return Ok(d);
        }
        self.work.clear();
        self.work.push((root, false));
        while let Some((n, expanded)) = self.work.pop() {
            if self.memo.contains_key(&(tab, n)) {
                continue;
            }
            let node = t.node(PTy(n))?;
            self.kids.clear();
            crate::portable::node_children(t, node, &mut self.kids)?;
            if !expanded {
                self.work.push((n, true));
                for &c in &self.kids {
                    if c >= n {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: n, child: c }));
                    }
                    self.work.push((c, false));
                }
                continue;
            }
            let mut h = StableHasher::new("term");
            crate::portable::hash_node(&node, &mut h);
            for c in &self.kids {
                let d = self.memo.get(&(tab, *c)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: n, child: *c }))?;
                h.u64(d);
            }
            self.memo.insert((tab, n), h.finish());
        }
        self.memo.get(&(tab, root)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root, child: root }))
    }

    /// The read-list digest of a record with a table: the record without its table, then the
    /// digest of each term its handles reach (a list: its length, then its terms').
    fn rooted<T: Tabled>(&mut self, v: &T) -> Result<u64, CheckError> {
        let mut h = StableHasher::new("answer");
        v.untabled().hash_into(&mut h);
        self.roots.clear();
        let mut roots = core::mem::replace(&mut self.roots, Vec::new_in(self.alloc));
        let t = v.parts(&mut roots);
        if let Some(t) = t {
            let id = (t.nodes.as_ptr() as usize, t.lists.as_ptr() as usize);
            let next = self.seen.len() as u32;
            let tab = *self.seen.entry(id).or_insert(next);
            for r in &roots {
                match *r {
                    Root::Ty(p) => h.u64(self.term(tab, t, p.0)?),
                    Root::List(l) => {
                        h.u32(l.len);
                        for i in l.range() {
                            let p = t.lists.get(i).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: i as u32, child: i as u32 }))?;
                            h.u64(self.term(tab, t, p.0)?);
                        }
                    }
                }
            }
        }
        self.roots = roots;
        Ok(h.finish())
    }
}

/// The ask log being written, with the clock it reads and where the walk is.
pub(crate) struct AskRecorder<'w, 'a> {
    pub asks: Vec<Ask, Alloc<'a>>,
    pub clock: &'w dyn Clock,
    pub start_ns: u64,
    pub cursor: Cursor,
}

fn hash_of(domain: &str, v: &impl ContentHash) -> u64 {
    let mut h = StableHasher::new(domain);
    v.hash_into(&mut h);
    h.finish()
}

/// A handle of a record into its table: a term or a list of terms.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Root {
    Ty(PTy),
    List(List),
}

/// A record that comes with a table of terms (`types`), which may be a store's whole table:
/// its read-list digest hashes the record without the table, then the terms its own handles
/// reach (`Reads::rooted_hash`), never the rest of the table.
pub(crate) trait Tabled: ContentHash {
    /// Pushes the record's handles into its table and returns the table (`None`: no record).
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>>;
    /// The record with an empty table: what is hashed besides the reached terms.
    fn untabled(&self) -> Self;
}

impl<T: Tabled> Tabled for Option<T> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        self.as_ref().and_then(|v| v.parts(roots))
    }
    fn untabled(&self) -> Self {
        self.as_ref().map(|v| v.untabled())
    }
}

fn clause_roots(cs: &[PClause], roots: &mut Vec<Root, Alloc<'_>>) {
    for c in cs {
        match *c {
            PClause::Trait { args, .. } | PClause::Const { args, .. } => roots.push(Root::List(args)),
            PClause::Projection { args, term, .. } => {
                roots.push(Root::List(args));
                roots.push(Root::Ty(term));
            }
        }
    }
}

impl Tabled for TraitDef<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        clause_roots(self.supertraits, roots);
        roots.extend(self.assoc_types.iter().filter_map(|a| a.default).map(Root::Ty));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        TraitDef { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for ConstParams<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.params.iter().map(|&(_, p)| Root::Ty(p)));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        ConstParams { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for FnSig<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.inputs.iter().map(|&p| Root::Ty(p)));
        roots.push(Root::Ty(self.output));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        FnSig { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for Predicates<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        clause_roots(self.clauses, roots);
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        Predicates { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for Adt<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.fields.iter().map(|f| Root::Ty(f.ty)));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        Adt { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for ImplHeader<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.self_ty));
        roots.push(Root::List(self.trait_args.list()));
        roots.extend(self.assoc_types.iter().map(|&(_, p)| Root::Ty(p)));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        ImplHeader { types: PTys::EMPTY, ..*self }
    }
}

impl Tabled for Hidden<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.ty));
        Some(self.types)
    }
    fn untabled(&self) -> Self {
        Hidden { types: PTys::EMPTY, ..*self }
    }
}

/// The content digest of ground arguments `args` of a table `types` holding only them (as
/// exported, hash-consed: its bytes are a function of the arguments' structure), which names a
/// `where const` condition's record with the condition's key ([`Question::ConstHolds`]).
pub fn ground_args_digest(args: &[PTy], types: PTys<'_>) -> u64 {
    let mut h = StableHasher::new("where const args");
    h.u32(args.len() as u32);
    for a in args {
        h.u32(a.0);
    }
    types.hash_into(&mut h);
    h.finish()
}

impl<'w, 'a, W: World> Reads<'w, 'a, W> {
    /// The recorder for one run.
    pub(crate) fn new_in(world: &'w W, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        Reads { world, entries: Vec::new_in(alloc), log: None, tally: None, hashes: Hashes::new_in(alloc) }
    }

    /// The recorder for one run that also keeps `tally`, reading `clock`.
    pub(crate) fn counted_in(world: &'w W, tally: &'w mut Tally, clock: &'w dyn Clock, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        Reads { world, entries: Vec::new_in(alloc), log: None, tally: Some(Tallying { tally, clock }), hashes: Hashes::new_in(alloc) }
    }

    /// The tally's clock now, when the run counts (0 otherwise).
    pub(crate) fn stamp(&self) -> u64 {
        self.tally.as_ref().map_or(0, |t| t.clock.now_ns())
    }

    /// The tally, when the run counts.
    pub(crate) fn tally(&mut self) -> Option<&mut Tally> {
        self.tally.as_mut().map(|t| &mut *t.tally)
    }

    /// The recorder for one run that also writes the ask log, reading `clock`.
    pub(crate) fn logged_in(world: &'w W, clock: &'w dyn Clock, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        let start_ns = clock.now_ns();
        let log = AskRecorder { asks: Vec::new_in(alloc), clock, start_ns, cursor: Cursor::default() };
        Reads { world, entries: Vec::new_in(alloc), log: Some(log), tally: None, hashes: Hashes::new_in(alloc) }
    }

    /// Whether the run writes the ask log (the machine then reports where it is).
    pub(crate) fn logging(&self) -> bool {
        self.log.is_some()
    }

    /// Where the walk is: the step and the checker's state in bytes.
    pub(crate) fn at(&mut self, step: u32, state_bytes: u64) {
        if let Some(l) = &mut self.log {
            l.cursor.step = step;
            l.cursor.state_bytes = state_bytes;
        }
    }

    /// Marks the asks that follow as a ground solver goal's (`true`) or not.
    pub(crate) fn in_ground_goal(&mut self, ground: bool) {
        if let Some(l) = &mut self.log {
            l.cursor.ground_goal = ground;
        }
    }

    /// The ask log of the run, with its end.
    pub(crate) fn take_log(&mut self) -> Option<AskLog<'a>> {
        let l = self.log.take()?;
        let end_ns = l.clock.now_ns().saturating_sub(l.start_ns);
        Some(AskLog { asks: l.asks, end_step: l.cursor.step, end_ns })
    }

    fn seal() -> Seal {
        Seal { _private: () }
    }

    /// The one seam every read of another item's record goes through: asks the world, logs
    /// the ask when measuring, and enters the answer's hash into the read list.
    fn record<T: ContentHash>(&mut self, q: Question, got: Fetch<T>, asked_at: u64) -> Result<T, Stop> {
        self.record_with(q, got, asked_at, |_, v| Ok(hash_of("answer", v)))
    }

    /// The read-list digest of this run's latest answer to `q` (a record already read).
    pub(crate) fn digest(&self, q: Question) -> Result<u64, CheckError> {
        self.entries.iter().rev().find(|e| e.0 == q).map(|e| e.1).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))
    }

    /// `record` for an answer with a table: its digest covers only the terms it reaches.
    fn record_tabled<T: Tabled>(&mut self, q: Question, got: Fetch<T>, asked_at: u64) -> Result<T, Stop> {
        self.record_with(q, got, asked_at, |h, v| h.rooted(v))
    }

    fn record_with<T>(
        &mut self,
        q: Question,
        got: Fetch<T>,
        asked_at: u64,
        digest: impl FnOnce(&mut Hashes<'a>, &T) -> Result<u64, CheckError>,
    ) -> Result<T, Stop> {
        let answered_at = self.stamp();
        if let Some(l) = &mut self.log {
            l.asks.push(Ask {
                question: q,
                kind: q.record_kind(),
                step: l.cursor.step,
                at_ns: l.clock.now_ns().saturating_sub(l.start_ns),
                state_bytes: l.cursor.state_bytes,
                missing: got.is_err(),
                ground_goal: l.cursor.ground_goal,
            });
        }
        match got {
            Ok(v) => {
                let d = digest(&mut self.hashes, &v)?;
                self.entries.push((q, d));
                self.count(q, false, asked_at, answered_at);
                Ok(v)
            }
            Err(Missing) => {
                self.count(q, true, asked_at, answered_at);
                Err(Stop::Need(q))
            }
        }
    }

    /// Counts one ask into the tally, when the run counts.
    fn count(&mut self, q: Question, missing: bool, asked_at: u64, answered_at: u64) {
        if let Some(t) = &mut self.tally {
            let hashed_at = t.clock.now_ns();
            t.tally.asks[question_kind(q)] += 1;
            t.tally.missing += missing as u64;
            t.tally.world_ns += answered_at.saturating_sub(asked_at);
            t.tally.hash_ns += hashed_at.saturating_sub(answered_at);
        }
    }

    /// The syntactic prefetch of a body with no record (typeck.md, "Re-entry"): every path its
    /// text names, resolved where the world already can, and what each resolution's use will
    /// read; every method name, as the associated item of each trait in scope. Asked at once
    /// as prefetches, so a cold body stops once per round of fetches, not once per signature.
    /// Not reads: nothing enters the read list, and no answer here changes the run.
    pub(crate) fn prefetch_syntactic(&mut self, body: &BodyView<'_>, alloc: Alloc<'a>) {
        let mut first: Vec<Question, Alloc<'a>> = Vec::new_in(alloc);
        let mut names: Vec<Name, Alloc<'a>> = Vec::new_in(alloc);
        for (i, bp) in body.paths.iter().enumerate() {
            first.push(match bp.lang {
                Some(l) => Question::LangPath(l),
                None => Question::Path(bp.scope, PathIdx(i as u32)),
            });
        }
        let mut scopes: Vec<ScopeId, Alloc<'a>> = Vec::new_in(alloc);
        for e in body.exprs {
            if let crate::body::Expr::MethodCall { scope, .. } = *e
                && !scopes.contains(&scope)
            {
                scopes.push(scope);
                first.push(Question::TraitsInScope(scope));
            }
        }
        self.world.prefetch(Self::seal(), &first);
        // The second round: what the answers already there name.
        let mut second: Vec<Question, Alloc<'a>> = Vec::new_in(alloc);
        for (i, bp) in body.paths.iter().enumerate() {
            let def = match bp.lang {
                Some(l) => self.world.lang_path(Self::seal(), l),
                None => {
                    names.clear();
                    let Ok(segs) = body.segment_list(bp.segments) else { continue };
                    names.extend(segs.iter().map(|s| s.name));
                    self.world.resolve_path(Self::seal(), bp.scope, PathIdx(i as u32), &names).map(|r| r.map(|r| r.def))
                }
            };
            let Ok(Some(def)) = def else { continue };
            match def {
                Def::Fn(k) => {
                    second.push(Question::FnSig(k));
                    second.push(Question::Predicates(k));
                }
                Def::Ctor { adt: k, .. } | Def::Adt(k) => {
                    second.push(Question::Adt(k));
                    second.push(Question::Predicates(k));
                }
                Def::Trait(k) => second.push(Question::TraitDef(k)),
                Def::Const(k) | Def::Static(k, _) => second.push(Question::FnSig(k)),
                Def::TyParam(_) | Def::Prim(_) => {}
            }
        }
        for e in body.exprs {
            let crate::body::Expr::MethodCall { scope, name, .. } = *e else { continue };
            let Ok(traits) = self.world.traits_in_scope(Self::seal(), scope) else { continue };
            for &t in traits {
                second.push(Question::AssocItem(t, name));
                second.push(Question::TraitDef(t));
            }
        }
        second.sort_unstable();
        second.dedup();
        self.world.prefetch(Self::seal(), &second);
    }

    /// Hands the last run's questions to the world.
    pub(crate) fn prefetch(&mut self, questions: &[Question]) {
        self.world.prefetch(Self::seal(), questions);
    }

    pub fn fn_sig(&mut self, item: ItemKey) -> Result<Option<FnSig<'w>>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.fn_sig(Self::seal(), item);
        self.record_tabled(Question::FnSig(item), got, asked_at)
    }

    pub fn predicates(&mut self, item: ItemKey) -> Result<Predicates<'w>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.predicates(Self::seal(), item);
        self.record_tabled(Question::Predicates(item), got, asked_at)
    }

    pub fn adt(&mut self, item: ItemKey) -> Result<Option<Adt<'w>>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.adt(Self::seal(), item);
        self.record_tabled(Question::Adt(item), got, asked_at)
    }

    pub fn impls_of(&mut self, trait_: ItemKey, shape: Shape) -> Result<&'w [ItemKey], Stop> {
        let asked_at = self.stamp();
                let got = self.world.impls_of(Self::seal(), trait_, shape);
        self.record(Question::ImplsOf(trait_, shape), got, asked_at)
    }

    /// An impl's header, checked where it enters: a trait impl's arguments after `Self` must
    /// number the trait's parameters besides `Self` (`CheckError::ImplTraitArity` names it).
    pub fn impl_header(&mut self, item: ItemKey) -> Result<Option<ImplHeader<'w>>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.impl_header(Self::seal(), item);
        let h = self.record_tabled(Question::ImplHeader(item), got, asked_at)?;
        if let Some(ImplHeader { trait_: Some(trait_), trait_args, .. }) = h {
            let Some(def) = self.trait_def(trait_)? else {
                return Err(CheckError::Malformed(Malformed::NoRecord(Question::TraitDef(trait_))).into());
            };
            let expected = def.generics.saturating_sub(1);
            if trait_args.list().len != expected {
                return Err(CheckError::ImplTraitArity { impl_: item, trait_, expected, got: trait_args.list().len }.into());
            }
        }
        Ok(h)
    }

    pub fn inherent(&mut self, shape: Shape, name: Name) -> Result<&'w [AssocItem], Stop> {
        let asked_at = self.stamp();
                let got = self.world.inherent(Self::seal(), shape, name);
        self.record(Question::Inherent(shape, name), got, asked_at)
    }

    pub fn assoc_item(&mut self, container: ItemKey, name: Name) -> Result<Option<AssocItem>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.assoc_item(Self::seal(), container, name);
        self.record(Question::AssocItem(container, name), got, asked_at)
    }

    pub fn traits_in_scope(&mut self, scope: ScopeId) -> Result<&'w [ItemKey], Stop> {
        let asked_at = self.stamp();
                let got = self.world.traits_in_scope(Self::seal(), scope);
        self.record(Question::TraitsInScope(scope), got, asked_at)
    }

    pub fn resolve_path(
        &mut self,
        scope: ScopeId,
        path: PathIdx,
        segments: &[Name],
    ) -> Result<Option<PathRes>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.resolve_path(Self::seal(), scope, path, segments);
        self.record(Question::Path(scope, path), got, asked_at)
    }

    pub fn lang_item(&mut self, item: LangItem) -> Result<Option<ItemKey>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.lang_item(Self::seal(), item);
        self.record(Question::Lang(item), got, asked_at)
    }

    pub fn lang_path(&mut self, item: LangItem) -> Result<Option<Def>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.lang_path(Self::seal(), item);
        self.record(Question::LangPath(item), got, asked_at)
    }

    pub fn const_value(&mut self, item: ItemKey) -> Result<Option<u64>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.const_value(Self::seal(), item);
        self.record(Question::Const(item), got, asked_at)
    }

    pub fn opaque_hidden(&mut self, item: ItemKey) -> Result<Option<Hidden<'w>>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.opaque_hidden(Self::seal(), item);
        self.record_tabled(Question::Opaque(item), got, asked_at)
    }

    pub fn closure(&mut self, owner: ItemKey, ordinal: u32) -> Result<Option<ClosureFacts<'w>>, Stop> {
        let asked_at = self.stamp();
        let got = self.world.closure(Self::seal(), owner, ordinal);
        self.record_tabled(Question::Closure(owner, ordinal), got, asked_at)
    }

    pub fn attrs(&mut self, item: ItemKey) -> Result<TypeAttrs, Stop> {
        let asked_at = self.stamp();
                let got = self.world.attrs(Self::seal(), item);
        self.record(Question::Attrs(item), got, asked_at)
    }

    pub fn trait_def(&mut self, trait_: ItemKey) -> Result<Option<TraitDef<'w>>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.trait_def(Self::seal(), trait_);
        self.record_tabled(Question::TraitDef(trait_), got, asked_at)
    }

    pub fn assoc_parent(&mut self, item: ItemKey) -> Result<Option<AssocParent>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.assoc_parent(Self::seal(), item);
        self.record(Question::AssocParent(item), got, asked_at)
    }

    pub fn const_holds(&mut self, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Result<ConstHolds, Stop> {
        let asked_at = self.stamp();
                let got = self.world.const_holds(Self::seal(), cond, args, types);
        self.record(Question::ConstHolds(cond, ground_args_digest(args, types)), got, asked_at)
    }

    pub fn const_params(&mut self, item: ItemKey) -> Result<ConstParams<'w>, Stop> {
        let asked_at = self.stamp();
                let got = self.world.const_params(Self::seal(), item);
        self.record_tabled(Question::ConstParams(item), got, asked_at)
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
        h.tag(self.header.bits());
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
            PClause::Const { cond, args } => {
                h.tag(2);
                h.u128(cond.0);
                h.u32(args.start);
                h.u32(args.len);
            }
        }
    }
}

impl ContentHash for ConstHolds {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(*self as u8);
    }
}

impl ContentHash for Predicates<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        self.clauses.hash_into(h);
        self.types.hash_into(h);
    }
}

impl ContentHash for ConstParams<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.params.len() as u32);
        for &(i, t) in self.params {
            h.u32(i);
            h.u32(t.0);
        }
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
        h.u32(self.trait_args.list().start);
        h.u32(self.trait_args.list().len);
        h.u64(self.assoc_types.len() as u64);
        for (n, t) in self.assoc_types {
            h.u64(n.0);
            h.u32(t.0);
        }
        self.types.hash_into(h);
        h.u32(match self.polarity {
            ImplPolarity::Positive => 0,
            ImplPolarity::Negative => 1,
            ImplPolarity::Reservation => 2,
        });
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
        self.def.hash_into(h);
    }
}

impl ContentHash for Def {
    fn hash_into(&self, h: &mut StableHasher) {
        match *self {
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
            Def::Prim(p) => {
                h.tag(7);
                match p {
                    Prim::Bool => h.tag(0),
                    Prim::Char => h.tag(1),
                    Prim::Str => h.tag(2),
                    Prim::Int(i) => {
                        h.tag(3);
                        h.tag(i as u8);
                    }
                    Prim::Uint(u) => {
                        h.tag(4);
                        h.tag(u as u8);
                    }
                    Prim::Float(f) => {
                        h.tag(5);
                        h.tag(f as u8);
                    }
                }
            }
        }
    }
}

impl ContentHash for TypeAttrs {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.0);
    }
}

impl ContentHash for TraitDef<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        h.u32(self.flags.0);
        match self.lang {
            Some(l) => {
                h.tag(1);
                l.hash_into(h);
            }
            None => h.tag(0),
        }
        self.supertraits.hash_into(h);
        h.u64(self.assoc_types.len() as u64);
        for a in self.assoc_types {
            h.u64(a.name.0);
            h.u128(a.item.0);
            match a.default {
                Some(t) => {
                    h.tag(1);
                    h.u32(t.0);
                }
                None => h.tag(0),
            }
        }
        self.types.hash_into(h);
        self.members.hash_into(h);
    }
}

impl ContentHash for TraitMembers<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        use crate::dyn_compat::{MemberShape, Receiver, Unread};
        let ms = match self {
            TraitMembers::NotKnown => return h.tag(0),
            TraitMembers::Known(ms) => ms,
        };
        h.tag(1);
        h.u64(ms.len() as u64);
        for m in *ms {
            h.u64(m.name.0);
            match m.shape {
                MemberShape::Const => h.tag(0),
                MemberShape::Type { generic } => {
                    h.tag(1);
                    h.tag(u8::from(generic));
                }
                MemberShape::Fn(f) => {
                    h.tag(2);
                    h.tag(match f.receiver {
                        Receiver::Absent => 0,
                        Receiver::Value => 1,
                        Receiver::Ref => 2,
                        Receiver::RefMut => 3,
                        Receiver::Box => 4,
                        Receiver::Rc => 5,
                        Receiver::Arc => 6,
                        Receiver::PinOfPointer => 7,
                        Receiver::Other => 8,
                    });
                    h.u32(f.own_non_lifetime_params);
                    for b in [f.where_self_sized, f.is_async, f.opaque_return, f.self_outside_receiver] {
                        h.tag(u8::from(b));
                    }
                }
                MemberShape::Unread(u) => {
                    h.tag(3);
                    h.tag(match u {
                        Unread::TypedReceiver => 0,
                        Unread::SignatureType => 1,
                    });
                }
            }
        }
    }
}

impl ContentHash for AssocParent {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u128(self.container.0);
        h.u64(self.name.0);
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

impl ContentHash for Hidden<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.ty.0);
        self.types.hash_into(h);
    }
}
