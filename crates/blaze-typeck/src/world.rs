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
use crate::regions::Region;

#[cfg(test)]
mod read_digest;

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
    Record(u64, u32),
    Union(u32),
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
    /// The facts of closure `ordinal` of `owner`'s body ([`ClosureFacts`]): its owner's check.
    Closure(ItemKey, u32),
    /// The facts of async block `ordinal` of `owner`'s body ([`CoroutineFacts`]): its owner's
    /// check (`PNode::Coroutine`).
    Coroutine(ItemKey, u32),
    /// The generics of a trait's associated type, by its trait and name (as a projection
    /// names it): the trait's count and its own ([`AssocGenerics`]).
    AssocGenerics(ItemKey, Name),
    /// A type alias's definition, as a body path through it reads it ([`AliasDef`]).
    TypeAlias(ItemKey),
    /// The default values of an ADT's fields (`struct S { a: u8 = 1 }`, RFC 3681), as a
    /// base-less struct expression `S { .. }` reads them ([`World::field_defaults`]).
    FieldDefaults(ItemKey),
    /// Whether the body may name the member fn `name` of inherent impl `container` (its
    /// visibility against the body's module, [`World::member_visible`]): what method lookup
    /// reads before it takes an inherent candidate.
    MemberVisible(ItemKey, Name),
    /// Whether the member `name` (a fn or constant) of inherent impl `container` is unstable
    /// under a library feature the body's crate does not turn on
    /// ([`World::member_unstable`]): what method and path lookup read before they take an
    /// inherent candidate.
    MemberUnstable(ItemKey, Name),
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
            Question::Opaque(_)
            | Question::Const(_)
            | Question::ConstHolds(..)
            | Question::Closure(..)
            | Question::Coroutine(..) => RecordKind::Body,
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
            | Question::ConstParams(_)
            | Question::AssocGenerics(..)
            | Question::TypeAlias(_)
            | Question::FieldDefaults(_)
            | Question::MemberVisible(..)
            | Question::MemberUnstable(..) => RecordKind::Item,
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
    /// The trait's implied clauses, `Self` as `Param(0)`: what elaborating a `T: Trait` bound
    /// adds (the reference's `explicit_implied_clauses_of`): the supertrait bounds, the
    /// where-clauses on `Self`, and the associated item constraints' bounds written in them
    /// (`trait Tr: Super<A: B>` gives `<Self as Super>::A: B`, whose self type is no
    /// `Param(0)`). A walk of the supertraits proper keeps the clauses whose first argument is
    /// `Param(0)` (`dyn_compat`, `lit_path::elaborate_bound_traits`). A where-clause on another
    /// type is not here (the Reference, "Traits", "Supertraits").
    pub supertraits: &'w [PClause],
    pub assoc_types: &'w [AssocTyDef],
    pub types: PTys<'w>,
    /// Its members' facts for dyn compatibility (`crate::dyn_compat`, the rule's one home).
    pub members: TraitMembers<'w>,
    /// Its parameters' defaults after `Self` (`trait PartialEq<Rhs = Self>`), in `types`,
    /// `Self` as `Param(0)`: what a qualified path `<T as Tr>::x` that writes no arguments
    /// takes (rules/qpath_defaults.rs).
    pub defaults: TraitDefaults<'w>,
}

/// A trait's parameter defaults as the world read them: one entry per parameter after `Self`
/// (`None`: no default), or a typed "not known": a qualified path that needs them is then a
/// named refusal (`CheckError::TraitDefaultsNotKnown`), never inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraitDefaults<'w> {
    Known(&'w [Option<PTy>]),
    NotKnown,
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
    /// The trait has early-bound lifetime arguments erased from its portable trait reference.
    pub const LIFETIME_PARAMS: u32 = 1 << 6;
    /// The trait declares a `Self: 'static` supertrait bound (e.g. `pub trait Any: 'static`):
    /// a trait object `dyn Trait` with no written lifetime defaults to `'static` even behind `&`.
    pub const SELF_STATIC: u32 = 1 << 7;

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

/// The generics of a trait's associated type (the Reference, "Associated Items", "Associated
/// Types": "Associated types may include generic parameters and where clauses; these are often
/// referred to as generic associated types, or GATs"). A projection's argument list is the
/// trait's arguments, `Self` first, then the associated type's own in declaration order:
/// `<T as Tr<A>>::Out<B>` is `Alias { args: [T, A, B] }`, and an impl's `type Out<X> = ..;` is
/// a type over the impl's generics then `X`. Lifetime parameters are not counted: portable
/// types hold no regions (`PNode::Ref` has none either), so `type Iter<'a>` has `own: 0` and
/// `Iter<'a>` no argument, exactly as `&'a T` is `Ref(T)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocGenerics {
    /// The trait's generic parameters, `Self` (index 0) included ([`TraitDef::generics`]).
    pub parent: u32,
    /// The associated type's own type and const parameters.
    pub own: u32,
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
    /// A C-variadic foreign fn (`fn printf(format: *const c_char, ...) -> c_int;`, the
    /// Reference, "External blocks", [items.extern.variadic]): `inputs` are the declared
    /// parameters, and a call may pass more (rules/c_variadic.rs).
    pub c_variadic: bool,
    /// `#[rustc_legacy_const_generics(i, ..)]` (stdarch's `vget_lane_u64<const IMM5: i32>(v)`):
    /// the argument positions of a call at which the fn's own const parameters may be passed as
    /// value arguments, as the attribute lists them; empty for every other fn
    /// (rules/legacy_const.rs). `inputs` hold only the real parameters.
    pub legacy_const_generics: &'w [u32],
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
    /// `ty: 'region` (`T: 'a`, `where &'b U: 'static`; D148). The type checker treats it as
    /// true: lifetimes are erased here, and the borrow check is what proves it (as rustc's
    /// typeck leaves region obligations to region resolution). Every place that turns a clause
    /// into a goal or an assumption passes it over (machine.rs `register_predicates`,
    /// infer/facts.rs `clauses`, rules/control.rs, rules/opaque_def.rs, rules/await_impl.rs,
    /// specialize.rs, stalled.rs, instance.rs); the region is in the head's region numbering
    /// (`regions::Region`), the clause's place in its head's list keyed by `nth` in the head's
    /// `HeadRegions::outlives`.
    TypeOutlives { ty: PTy, region: Region },
    /// `'long: 'short` (`'a: 'b`, `where 'a: 'static`; D148): true for the type checker, as
    /// [`PClause::TypeOutlives`] is.
    RegionOutlives { long: Region, short: Region },
    /// `where for<F in fields(container)> F: trait_<args...>`: every field type `F` of
    /// `container` implements `trait_` with `args` (the trait's arguments after `Self`).
    ForFields { container: PTy, trait_: ItemKey, args: List },
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
    /// `Self` in an impl whose self type is no struct, enum or union (a parameter, an array, a
    /// primitive, a tuple, a reference): the self type of the impl the body is a member of (the
    /// Reference, "Paths", "`Self`": "In an implementation, it refers to the type being
    /// implemented."). The type itself is the impl header's ([`World::impl_header`]), a read
    /// of its own, so this answer holds no type. An ADT's `Self` stays [`Def::Adt`], which
    /// constructor and struct paths take.
    SelfTy,
    /// A type alias whose aliased type no [`Def::Adt`] or [`Def::Prim`] names as written: an
    /// ADT with arguments the alias fixes (`type InnerSized<T, const N: usize> =
    /// PolymorphicIter<[MaybeUninit<T>; N]>`), a projection (`type Scalar<C> = <C as
    /// CurveArithmetic>::Scalar`), any other type. The Reference, "Type aliases": "A type alias
    /// defines a new name for an existing type", so a path through it means the aliased type
    /// with the alias's parameters replaced by the arguments written on the alias's segment
    /// (fresh variables where none are; defaults where some are left out): `A::new()`,
    /// `A::CONST`, `A::Variant`, `A { .. }` (rules/alias_path.rs). The type is the world's
    /// [`World::type_alias`] answer, a read of its own.
    Alias(ItemKey),
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
    /// `#[rustc_promotable]` on a fn: a call of it with promotable arguments may be promoted
    /// to a `'static` slot (rules/promote.rs; core/src/task/wake.rs `RawWakerVTable::new`).
    pub const PROMOTABLE: u32 = 4;
    /// `#[repr(simd)]` on a struct: a vector, its one field an array (an `asm!` operand's
    /// vector types, asm.rs). The world sets it from the type's repr.
    pub const SIMD: u32 = 8;
    /// `#[rustc_intrinsic]` on a fn: a compiler intrinsic, never reified to a fn pointer
    /// (rules/reify.rs; core/src/intrinsics/mod.rs). Bit 16: 8 is taken by `SIMD` (intr-P9b).
    pub const INTRINSIC: u32 = 16;
    /// `#[target_feature(..)]` on a fn (rules/target_feature.rs): a SAFE such fn is a safe fn
    /// pointer only in a body that enables its features. Bit 32.
    pub const TARGET_FEATURE: u32 = 32;
    /// `#[blaze::args]` on a struct (rule 83, decision D126): an options struct, a call's named
    /// arguments and nothing else. Typing reads nothing of it; the body check's `args-inline`
    /// (rules/args_inline.rs) does. Bit 64.
    pub const ARGS: u32 = 64;
    /// `#[repr(packed)]`, `#[repr(packed(N))]` or `#[repr(C, packed)]` on a struct or union: a
    /// field may be unaligned, so a closure or `async` block capturing a field BY REFERENCE
    /// captures the struct instead, whatever the field's alignment (capture_places.rs `borrow`;
    /// rustc_hir_typeck upvar.rs `restrict_repr_packed_field_ref_capture`). The world sets it
    /// from the type's repr, the one fact layout reads too. Bit 128.
    pub const PACKED: u32 = 128;
    /// Bits 8..12: an enum's discriminant type ([`DiscrRepr`]); 0 is `Unread`.
    const DISCR: u32 = 0xf << 8;
    /// Blaze edition: transparent all-`pub` struct eligible for automatic `Copy`/`Clone`
    /// (`blaze_solve::facts::AdtFlags::AUTO_COPY_PUB`). Bit 12.
    pub const AUTO_COPY_PUB: u32 = 1 << 12;

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

/// A type alias's definition (`type A<T, const N: usize = 4> = B<[T; N]>;`): what a body path
/// through it names ([`Def::Alias`]). The Reference, "Type aliases": "A type alias defines a
/// new name for an existing type". `ty` is the aliased type over the alias's own parameters
/// (`Param(i)`, `ConstParam(i)`, `i < generics`; type and const parameters together, in
/// declaration order, lifetimes not counted), every alias in it already expanded; `defaults`
/// holds one entry per parameter (`None` where none is written), over the parameters before it.
#[derive(Clone, Copy, Debug)]
pub struct AliasDef<'w> {
    pub generics: u32,
    pub defaults: &'w [Option<PTy>],
    pub ty: PTy,
    pub types: PTys<'w>,
}

/// An opaque type's hidden type.
#[derive(Clone, Copy, Debug)]
pub struct Hidden<'w> {
    pub ty: PTy,
    pub types: PTys<'w>,
}

/// A closure's facts, from its owner's check (`Checked::closures[ordinal]`): its kind and its
/// signature (a `FnPtr` of `types`, in the owner's parameters: `Param(i)` is the closure
/// type's `args[i]`). Its captures stay in the owner's `Checked::captures`.
#[derive(Clone, Copy, Debug)]
pub struct ClosureFacts<'w> {
    pub kind: crate::ty::ClosureKind,
    pub sig: PTy,
    pub types: PTys<'w>,
}

/// An async block's facts, from its owner's check (`PNode::Coroutine`): its output and its
/// interior (the tuple of what it holds), both of `types` in the owner's parameters
/// (`Param(i)` is the coroutine type's `args[i]`), and what that interior is ([`InteriorIs`]).
/// A producer whose captured places are not known cannot answer this question yet.
#[derive(Clone, Copy, Debug)]
pub struct CoroutineFacts<'w> {
    pub output: PTy,
    pub interior: PTy,
    pub types: PTys<'w>,
    pub is: InteriorIs,
}

/// How the producer determined the coroutine's stored state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteriorIs {
    /// The stored state: the flow's saved locals and the captures (or a block that never
    /// suspends, whose state is its captures).
    Exact,
    /// The candidate types collected by the body checker. Flow can discard a candidate
    /// or store a reference to it. For example, proving `T: Send` leaves `&T: Send`
    /// open until `T: Sync` is established. Only an exact row supplies a solver witness.
    Superset,
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
    /// The resolution of a path written at `scope`, by its segments' names; when it names
    /// nothing, the segment that failed, why, and what the segments before it named
    /// ([`crate::PathMiss`]: E0425 names that segment).
    fn resolve_path(
        &self,
        seal: Seal,
        scope: ScopeId,
        path: PathIdx,
        segments: &[Name],
    ) -> Fetch<crate::path_miss::PathAnswer>;
    fn lang_item(&self, seal: Seal, item: LangItem) -> Fetch<Option<ItemKey>>;
    /// The definition a desugaring's path to `item` names (`BodyPath::lang`): `Def::Fn` for a
    /// fn or a trait's fn (`Try::branch`), `Def::Ctor` for a variant (`ControlFlow::Continue`),
    /// `Def::Adt` for a struct (`Range`); `None` when the crate graph declares no such item.
    /// A variant's ctor key answers `fn_sig` as rustc's ctor fn: the ADT's generics as its own,
    /// the fields as inputs, the ADT as output.
    fn lang_path(&self, seal: Seal, item: LangItem) -> Fetch<Option<Def>>;
    fn const_value(&self, seal: Seal, item: ItemKey) -> Fetch<Option<u64>>;
    fn opaque_hidden(&self, seal: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>>;
    /// The facts of closure `ordinal` of `owner`'s body (`Question::Closure`): `Missing` while
    /// the owner's body is not checked; `None` when its record has no such closure.
    fn closure(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<ClosureFacts<'_>>>;
    /// The exact environment in the signature's type table. Signature-only
    /// providers can answer callable goals, but cannot establish a capture trait.
    fn closure_with_captures(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<(ClosureFacts<'_>, Option<PTy>)>> {
        self.closure(seal, owner, ordinal).map(|row| row.map(|call| (call, None)))
    }
    /// The facts of async block `ordinal` of `owner`'s body (`Question::Coroutine`): `Missing`
    /// while the owner's body or the block's exact interior is unavailable; `None` when its
    /// record has no such block. The default is the gap (`Missing`): a world that holds no
    /// body records cannot answer it, and
    /// a `PNode::Coroutine` it meets stays unlowered (never guessed). DEFAULTED so no world
    /// breaks; a world that holds body records answers it (blaze-ladder front2, the fixture).
    fn coroutine(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<CoroutineFacts<'_>>> {
        let (_, _, _) = (seal, owner, ordinal);
        Err(Missing)
    }
    /// The request's table of trait goals shared by every body (blaze-solve share.rs), when
    /// the world's request shares them: the solver looks each root goal up there before it
    /// solves it and puts what it solved. Not a fact (nothing of it is in a read list: an
    /// entry is taken only when its own read list answers the same in this body's facts).
    /// DEFAULTED to none, so every world that does not share is unchanged.
    fn goal_share(&self) -> Option<&dyn blaze_solve::share::GoalShare> {
        None
    }
    /// How a run meets a HOLE ([`Hole`], D161 "a task that lacks a fact waits on it"): `false`
    /// reads the answer's stand-in ([`Holed::hole`]) and goes on, so one round finds many holes
    /// (some speculative: asked only because an earlier answer was a stand-in); `true` stops the
    /// run at the first hole with `Stop::Need`, the hole recorded, so nothing runs past a
    /// missing fact and every question asked was asked with real answers before it. A world
    /// that makes every fact it can on the spot (blaze-ladder front2 `BodyWorld` under the one
    /// ask over frozen heads) says `true`: its holes are only the facts it cannot make inline,
    /// each one real. DEFAULTED to `false`: every other world is unchanged. Either answer gives
    /// the same `Outcome::Done` (the oracle: blaze-ladder front2 tables/task.rs `compare_asks`).
    fn stops_at_holes(&self) -> bool {
        false
    }
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
    /// The generics of the associated type `name` of trait `trait_`: the trait's count and
    /// the type's own ([`AssocGenerics`]). `None`: this world keeps no such count, which is
    /// what the default answers: right only for a world none of whose traits declares a
    /// generic associated type (the checker then files no own parameter and checks no
    /// count, as it did before this question). A world that reads real traits answers it
    /// (blaze-ladder front2 does); the method becomes required with the next batch over
    /// the worlds.
    fn assoc_generics(&self, seal: Seal, trait_: ItemKey, name: Name) -> Fetch<Option<AssocGenerics>> {
        let (_, _, _) = (seal, trait_, name);
        Ok(None)
    }
    /// The definition of type alias `item` ([`AliasDef`], `Question::TypeAlias`): `None` when
    /// `item` is no type alias. The default is the gap (`Missing`): only a world that answers a
    /// path with [`Def::Alias`] is asked it, and that world answers it (blaze-ladder front2).
    /// DEFAULTED so no other world breaks.
    fn type_alias(&self, seal: Seal, item: ItemKey) -> Fetch<Option<AliasDef<'_>>> {
        let (_, _) = (seal, item);
        Err(Missing)
    }
    /// The default value of each field of ADT `adt` (`Question::FieldDefaults`), in the order
    /// of [`Adt::fields`] (every variant's fields, concatenated): the key of the field's
    /// default, an anonymous constant of the field's type over the ADT's generics, or `None`
    /// for a field with no default. An empty slice: no field of `adt` has one. The default is
    /// the gap (`Missing`): only a base-less struct expression (`S { a: 1, .. }`) asks it, and
    /// the world that files ADT heads answers it (blaze-ladder front2). DEFAULTED so no other
    /// world breaks.
    fn field_defaults(&self, seal: Seal, adt: ItemKey) -> Fetch<&[Option<ItemKey>]> {
        let (_, _) = (seal, adt);
        Err(Missing)
    }
    /// Whether the checked body may name the member fn `name` of the inherent impl
    /// `container` (`Question::MemberVisible`): the Reference, "Visibility and privacy": a
    /// private item (or `pub(self)`) is visible in its module and the modules under it,
    /// `pub(super)` in its parent's, `pub(in path)` in the module the path names, `pub(crate)`
    /// in its crate, `pub` everywhere. Method lookup asks it of each inherent candidate
    /// before taking it (rustc's probe.rs `push_candidate`: an inaccessible candidate is set
    /// aside as a private candidate, so a visible trait method of that name is found instead:
    /// reth's private `Tx::<RW>::put(kind, key, value)` beside `DbTxMut::put(key, value)`).
    /// DEFAULTED to `true`: right only for a world whose inherent members are all visible from
    /// the bodies it checks, which is every world but the one over real crates (blaze-ladder
    /// front2 answers it); the method becomes required with the next batch over the worlds.
    fn member_visible(&self, seal: Seal, container: ItemKey, name: Name) -> Fetch<bool> {
        let (_, _, _) = (seal, container, name);
        Ok(true)
    }
    /// Whether the member `name` (a fn or constant) of inherent impl `container` is unstable
    /// under a library feature the checked body's crate does not turn on
    /// (`Question::MemberUnstable`): rustc_middle stability.rs `eval_stability_allow_unstable`
    /// gives `Deny` for an item of another crate whose stability is `#[unstable(feature = f,
    /// implied_by = g)]` unless the crate turns on `#![feature(f)]` or `#![feature(g)]`; an
    /// item of the body's own crate is always allowed ("Only the cross-crate scenario matters
    /// when checking unstable APIs"). Method and path lookup drop such a candidate before the
    /// pick, and search again with it only when nothing else is picked (rustc_hir_typeck
    /// method/probe.rs `pick_core` and `consider_candidates`): std's unstable inherent
    /// `HashMap::try_insert` beside a user trait's `try_insert` (turso's `TursoHashMapExt`).
    /// DEFAULTED to `false` (allowed): right for every world whose crates write no stability
    /// attributes, which is every world but the one over real crates (blaze-ladder front2
    /// answers it).
    fn member_unstable(&self, seal: Seal, container: ItemKey, name: Name) -> Fetch<bool> {
        let (_, _, _) = (seal, container, name);
        Ok(false)
    }
    /// The questions the last check of this body asked, before the walk starts: fetch them in
    /// one batch. Answers nothing.
    fn prefetch(&self, seal: Seal, questions: &[Question]) {
        let _ = (seal, questions);
    }
    /// Which question the run is about to ask of this world (`Some`), or that the answer is in
    /// (`None`): [`Asking::repeat`] says this run had an answer to it already, so every read
    /// the world makes answering it again is one it recorded then (a world that records its
    /// own reads mutes them: blaze-ladder front2 tables/recorder.rs `Repeats::Quiet`), and
    /// [`Asking::kind`] is the question's [`QUESTION_KINDS`](crate::QUESTION_KINDS) index, what
    /// a world counts its reads under. Answers nothing. Nothing by default.
    fn asking(&self, seal: Seal, ask: Option<Asking>) {
        let _ = (seal, ask);
    }
}

/// One ask as [`World::asking`] is told it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asking {
    /// The question's kind, its index in [`QUESTION_KINDS`](crate::QUESTION_KINDS).
    pub kind: usize,
    /// This run had an answer (`Ok`) to the same question already: the world's reads of it
    /// are listed (the run's memo, `Reads::heard`).
    pub repeat: bool,
}

/// The read list: each question asked and the content hash of its answer, sorted by question
/// and deduplicated at the end of the run. A lookup scans the question column only.
#[derive(Clone)]
pub struct ReadList<'a> {
    pub rows: Soa<(Question, u64), Alloc<'a>>,
}

columns_eq!(Needs { questions, missing, first });

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
    /// Every question the run asked, answered or not, sorted, without duplicates: the hints.
    pub questions: Vec<Question, Alloc<'a>>,
    /// The holes: each question the world had no answer to, sorted, without duplicates.
    pub missing: Vec<Question, Alloc<'a>>,
    /// The first hole the run met: the one need that is not speculative (`Hole`).
    pub first: Option<Question>,
}

impl<'a> Needs<'a> {
    /// No needs.
    pub fn new_in(alloc: Alloc<'a>) -> Needs<'a> {
        Needs { questions: Vec::new_in(alloc), missing: Vec::new_in(alloc), first: None }
    }
}

/// A HOLE: a record that is not here YET, this round (typeck.md, "Speed on real code").
///
/// The world answered `Missing` to `question`. The run records it, reads the answer's stand-in
/// ([`Holed::hole`]) and goes on, so one round finds every record the body needs, not one.
/// A run that met any hole ends `Outcome::Need` with all of them and makes NO result (lib.rs
/// `run`): nothing computed past a hole (a diagnostic, a type, an error) ever leaves the run,
/// so the stand-in can never change an answer. Questions asked after the first hole are
/// speculative: the real run might not ask them.
///
/// Not a GAP: a gap is a question the front end cannot answer at all (the world's gap list,
/// a `CheckError`): it is reported and the body is not checked. A hole only waits for a fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hole {
    pub question: Question,
}

/// An answer type's stand-in for a hole: the value a rule reads in its place.
pub trait Holed {
    fn hole() -> Self;
}

impl<T> Holed for Option<T> {
    fn hole() -> Self {
        None
    }
}

impl<T> Holed for &[T] {
    fn hole() -> Self {
        &[]
    }
}

impl Holed for Predicates<'_> {
    fn hole() -> Self {
        Predicates { clauses: &[], types: PTys::EMPTY }
    }
}

impl Holed for ConstParams<'_> {
    fn hole() -> Self {
        ConstParams { params: &[], types: PTys::EMPTY }
    }
}

impl Holed for TypeAttrs {
    fn hole() -> Self {
        TypeAttrs(0)
    }
}

impl Holed for ConstHolds {
    fn hole() -> Self {
        ConstHolds::True
    }
}

/// A visibility answer's stand-in ([`World::member_visible`]): visible, so the round goes on
/// and asks what the candidate's check needs; nothing past a hole leaves the run.
impl Holed for bool {
    fn hole() -> Self {
        true
    }
}

/// A planted bug of the hole tests (`tests::a_hole_*`); `None` outside them.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HolePlant {
    None,
    /// A run that met a hole still makes a result from the stand-ins.
    AnswerThroughHoles,
    /// The hints (an earlier round's questions) enter the read list.
    KeepEarlierReads,
}

/// The recorder: owns the world's borrow and the read list. The only caller of `World`.
pub struct Reads<'w, 'a, W: World> {
    world: &'w W,
    entries: Vec<(Question, u64), Alloc<'a>>,
    /// The holes this run met, in the order met (`Hole`).
    holes: Vec<Hole, Alloc<'a>>,
    #[cfg(test)]
    pub(crate) hole_plant: HolePlant,
    /// The ask log, when the run measures re-entry (asks.rs); `None` otherwise.
    log: Option<AskRecorder<'w, 'a>>,
    /// The tally, when the run counts (asks.rs `Tally`); `None` otherwise.
    tally: Option<Tallying<'w>>,
    /// The digests of the record tables' terms this run has hashed (`rooted_hash`).
    hashes: Hashes<'a>,
    /// THE RUN'S MEMO OF ASKS (rule 77): every question the world answered (`Ok`) in this run,
    /// its syntactic prefetch's answers too. A question in it is asked again as a repeat
    /// ([`Asking::repeat`]): the world's reads of it were recorded at its first answer, so a
    /// world that records them mutes the second. A hole is not in it (the next ask may be
    /// answered, by reads the hole's did not make).
    heard: blaze_hash::HashSet<Question, Alloc<'a>>,
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
            // A future's output and interior are terms too, though conversion does not walk
            // them (a head never holds a future: infer/facts.rs refuses one): its digest covers
            // them, by content like every other child.
            if let crate::portable::PNode::Future(_, o, i) = node {
                self.kids.push(o.0);
                self.kids.push(i.0);
            }
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
            let mut h = StableHasher::new("term shape");
            crate::portable::hash_node_shape(&node, &mut h);
            for c in &self.kids {
                let d = self.memo.get(&(tab, *c)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: n, child: *c }))?;
                h.u64(d);
            }
            self.memo.insert((tab, n), h.finish());
        }
        self.memo.get(&(tab, root)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root, child: root }))
    }

    /// The read-list digest of a record with a table: the record without its table and without
    /// its handles' numbers (`Tabled::hash_shape`), then the digest of each term its handles
    /// reach (a list: its length, then its terms'). Content only: two stores holding one head
    /// give one digest whatever order each interned its terms in.
    fn rooted<T: Tabled>(&mut self, v: &T) -> Result<u64, CheckError> {
        let mut h = StableHasher::new(T::DOMAIN);
        v.hash_shape(&mut h);
        self.roots.clear();
        let mut roots = core::mem::replace(&mut self.roots, Vec::new_in(self.alloc));
        let t = v.parts(&mut roots);
        if let Some(t) = t {
            let id = t.identity();
            let next = self.seen.len() as u32;
            let tab = *self.seen.entry(id).or_insert(next);
            for r in &roots {
                match *r {
                    Root::Ty(p) => h.u64(self.term(tab, t, p.0)?),
                    Root::List(l) => {
                        h.u32(l.len);
                        for i in l.range() {
                            let p = t.list_slot(i).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: i as u32, child: i as u32 }))?;
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
    const DOMAIN: &'static str = "answer shape";
    /// Pushes the record's handles into its table and returns the table (`None`: no record).
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>>;
    /// Hashes what is hashed besides the reached terms: the record without its table and
    /// without its handles' numbers (a handle's place in the table is not content; the term
    /// it reaches is hashed after, in `parts`' order), with every count that fixes how many
    /// roots `parts` pushes.
    fn hash_shape(&self, h: &mut StableHasher);
}

impl<T: Tabled> Tabled for Option<T> {
    const DOMAIN: &'static str = T::DOMAIN;
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        self.as_ref().and_then(|v| v.parts(roots))
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        match self {
            Some(v) => {
                h.tag(1);
                v.hash_shape(h);
            }
            None => h.tag(0),
        }
    }
}

/// A clause's shape: its kind and keys, never its handles (`Tabled::hash_shape`).
fn clauses_shape(cs: &[PClause], h: &mut StableHasher) {
    h.u64(cs.len() as u64);
    for c in cs {
        match *c {
            PClause::Trait { trait_, .. } => {
                h.tag(0);
                h.u128(trait_.0);
            }
            PClause::Projection { trait_, name, .. } => {
                h.tag(1);
                h.u128(trait_.0);
                h.u64(name.0);
            }
            PClause::Const { cond, .. } => {
                h.tag(2);
                h.u128(cond.0);
            }
            // The kind only: the region is no input of a type check (the clause is true here),
            // and a lifetime-only edit must not move what type checks read (D148).
            PClause::TypeOutlives { .. } => h.tag(3),
            PClause::RegionOutlives { .. } => h.tag(4),
            PClause::ForFields { trait_, .. } => {
                h.tag(5);
                h.u128(trait_.0);
            }
        }
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
            PClause::ForFields { container, args, .. } => {
                roots.push(Root::Ty(container));
                roots.push(Root::List(args));
            }
            PClause::TypeOutlives { ty, .. } => roots.push(Root::Ty(ty)),
            PClause::RegionOutlives { .. } => {}
        }
    }
}

impl Tabled for TraitDef<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        clause_roots(self.supertraits, roots);
        roots.extend(self.assoc_types.iter().filter_map(|a| a.default).map(Root::Ty));
        if let TraitDefaults::Known(ds) = self.defaults {
            roots.extend(ds.iter().filter_map(|d| *d).map(Root::Ty));
        }
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        h.u32(self.flags.0);
        match self.lang {
            Some(l) => {
                h.tag(1);
                l.hash_into(h);
            }
            None => h.tag(0),
        }
        clauses_shape(self.supertraits, h);
        h.u64(self.assoc_types.len() as u64);
        for a in self.assoc_types {
            h.u64(a.name.0);
            h.u128(a.item.0);
            h.tag(u8::from(a.default.is_some()));
        }
        self.members.hash_into(h);
        match self.defaults {
            TraitDefaults::NotKnown => h.tag(0),
            TraitDefaults::Known(ds) => {
                h.tag(1);
                h.u64(ds.len() as u64);
                for d in ds {
                    h.tag(u8::from(d.is_some()));
                }
            }
        }
    }
}

impl Tabled for ConstParams<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.params.iter().map(|&(_, p)| Root::Ty(p)));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.params.len() as u32);
        for &(i, _) in self.params {
            h.u32(i);
        }
    }
}

impl Tabled for FnSig<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.inputs.iter().map(|&p| Root::Ty(p)));
        roots.push(Root::Ty(self.output));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.parent_count);
        h.u32(self.own_count);
        h.u32(self.declared_count);
        h.tag(u8::from(self.self_param));
        h.tag(self.header.bits() | if self.c_variadic { 0x40 } else { 0 });
        h.u64(self.legacy_const_generics.len() as u64);
        for &i in self.legacy_const_generics {
            h.u32(i);
        }
        h.u64(self.inputs.len() as u64);
    }
}

impl Tabled for Predicates<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        clause_roots(self.clauses, roots);
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        clauses_shape(self.clauses, h);
    }
}

impl Tabled for Adt<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.extend(self.fields.iter().map(|f| Root::Ty(f.ty)));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.tag(self.kind as u8);
        h.u32(self.generics);
        self.variants.hash_into(h);
        h.u64(self.fields.len() as u64);
        for f in self.fields {
            h.u64(f.name.0);
        }
    }
}

impl Tabled for ImplHeader<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.self_ty));
        roots.push(Root::List(self.trait_args.list()));
        roots.extend(self.assoc_types.iter().map(|&(_, p)| Root::Ty(p)));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        self.trait_.hash_into(h);
        h.u64(self.assoc_types.len() as u64);
        for (n, _) in self.assoc_types {
            h.u64(n.0);
        }
        h.u32(match self.polarity {
            ImplPolarity::Positive => 0,
            ImplPolarity::Negative => 1,
            ImplPolarity::Reservation => 2,
        });
    }
}

impl Tabled for ClosureFacts<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.sig));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.kind as u32);
    }
}

impl ContentHash for ClosureFacts<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.kind as u32);
        h.u32(self.sig.0);
        self.types.hash_into(h);
    }
}

impl Tabled for (ClosureFacts<'_>, Option<PTy>) {
    const DOMAIN: &'static str = "blaze.closure-facts.v2.captures";

    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.0.sig));
        roots.extend(self.1.map(Root::Ty));
        Some(self.0.types)
    }

    fn hash_shape(&self, h: &mut StableHasher) {
        self.0.hash_shape(h);
        h.u32(u32::from(self.1.is_some()));
    }
}

impl ContentHash for (ClosureFacts<'_>, Option<PTy>) {
    fn hash_into(&self, h: &mut StableHasher) {
        *h = h.bytes(Self::DOMAIN.as_bytes());
        self.0.hash_into(h);
        self.1.map(|p| p.0).hash_into(h);
    }
}

impl Tabled for CoroutineFacts<'_> {
    const DOMAIN: &'static str = "blaze.coroutine-facts.v2.exact";

    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.output));
        roots.push(Root::Ty(self.interior));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        // Two handles and their table: all its content is the terms `parts` reaches, and what
        // the interior is. An exact record hashes as it did before `InteriorIs` (no byte), so
        // every exact read's digest stands; a superset record adds one tag, so a reader that
        // checked against a superset is entered again when its producer's flow is repaired.
        match self.is {
            InteriorIs::Exact => {}
            InteriorIs::Superset => h.tag(1),
        }
    }
}

impl ContentHash for CoroutineFacts<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        *h = h.bytes(Self::DOMAIN.as_bytes());
        h.u32(self.output.0);
        h.u32(self.interior.0);
        if self.is == InteriorIs::Superset {
            h.tag(1);
        }
        self.types.hash_into(h);
    }
}

#[cfg(test)]
#[test]
fn a_coroutine_read_requires_an_exact_witness_in_its_key() {
    let nodes = [crate::portable::PNode::Bool, crate::portable::PNode::Tuple(List::EMPTY)];
    let types = PTys::flat(&nodes, &[]);
    let record = CoroutineFacts { output: PTy(0), interior: PTy(1), types, is: InteriorIs::Exact };
    let mut hashes = Hashes::new_in(Alloc::global());
    let exact = hashes.rooted(&Some(record)).expect("a checked coroutine");
    // A superset record of the same types is another answer: a reader checked against it is
    // entered again when the producer's flow gives the exact one.
    let superset = hashes.rooted(&Some(CoroutineFacts { is: InteriorIs::Superset, ..record })).expect("a superset coroutine");
    assert_ne!(exact, superset, "a superset interior read the same as an exact one");
    let mut unqualified = StableHasher::new("answer shape");
    unqualified.tag(1);
    for t in [record.output, record.interior] {
        unqualified.u64(hashes.term(0, types, t.0).expect("a term in the record"));
    }
    assert_ne!(exact, unqualified.finish(), "the old key did not certify an exact interior");
    assert_ne!(exact, hashes.rooted(&None::<CoroutineFacts<'_>>).expect("an absent block"));
}

impl Tabled for AliasDef<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.ty));
        roots.extend(self.defaults.iter().filter_map(|d| *d).map(Root::Ty));
        Some(self.types)
    }
    fn hash_shape(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        h.u64(self.defaults.len() as u64);
        for d in self.defaults {
            h.tag(u8::from(d.is_some()));
        }
    }
}

impl ContentHash for AliasDef<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.generics);
        h.u32(self.ty.0);
        h.u64(self.defaults.len() as u64);
        for d in self.defaults {
            match d {
                Some(t) => {
                    h.tag(1);
                    h.u32(t.0);
                }
                None => h.tag(0),
            }
        }
        self.types.hash_into(h);
    }
}

impl Tabled for Hidden<'_> {
    fn parts(&self, roots: &mut Vec<Root, Alloc<'_>>) -> Option<PTys<'_>> {
        roots.push(Root::Ty(self.ty));
        Some(self.types)
    }
    fn hash_shape(&self, _h: &mut StableHasher) {
        // One handle and its table: all its content is the term `parts` reaches.
    }
}

impl Hidden<'_> {
    /// The digest a caller records after revealing this opaque's hidden type.
    pub fn answer_digest(self, alloc: Alloc<'_>) -> Result<u64, CheckError> {
        Hashes::new_in(alloc).rooted(&Some(self))
    }
}

impl CoroutineFacts<'_> {
    /// The digest a caller records for this exact output and interior.
    pub fn answer_digest(self, alloc: Alloc<'_>) -> Result<u64, CheckError> {
        Hashes::new_in(alloc).rooted(&Some(self))
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
        Self::made(world, None, None, alloc)
    }

    /// The allocator this run's recorder lives in (D102): a walk inside the run takes its
    /// scratch here rather than fetching an allocator of its own.
    pub(crate) fn alloc(&self) -> Alloc<'a> {
        *self.entries.allocator()
    }

    /// The recorder with its log and tally, each optional: the one place a `Reads` is built.
    fn made(world: &'w W, log: Option<AskRecorder<'w, 'a>>, tally: Option<Tallying<'w>>, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        Reads {
            world,
            entries: Vec::new_in(alloc),
            holes: Vec::new_in(alloc),
            #[cfg(test)]
            hole_plant: HolePlant::None,
            log,
            tally,
            hashes: Hashes::new_in(alloc),
            heard: blaze_hash::HashSet::new_in(alloc),
        }
    }

    /// The start of one ask of `q`: the world told what it answers next and whether this run
    /// answered it already ([`World::asking`]); the stamp the tally times the answer from.
    /// [`Reads::record_with`] tells the world the answer is in.
    fn ask(&self, q: Question) -> u64 {
        self.world.asking(Self::seal(), Some(Asking { kind: question_kind(q), repeat: self.heard.contains(&q) }));
        self.stamp()
    }

    /// One ask of the syntactic prefetch (not a read of the run: nothing enters the read list),
    /// framed as [`Reads::ask`] frames a read, so the world counts and mutes it alike; an `Ok`
    /// answer enters [`Reads::heard`] (the world recorded its reads).
    fn ask_ahead<T>(&mut self, q: Question, answer: impl FnOnce(&'w W) -> Fetch<T>) -> Fetch<T> {
        self.world.asking(Self::seal(), Some(Asking { kind: question_kind(q), repeat: self.heard.contains(&q) }));
        let got = answer(self.world);
        self.world.asking(Self::seal(), None);
        if got.is_ok() {
            self.heard.insert(q);
        }
        got
    }

    /// The recorder for one run that also keeps `tally`, reading `clock`.
    pub(crate) fn counted_in(world: &'w W, tally: &'w mut Tally, clock: &'w dyn Clock, alloc: Alloc<'a>) -> Reads<'w, 'a, W> {
        Self::made(world, None, Some(Tallying { tally, clock }), alloc)
    }

    /// Counts one solver call that began at `at` (the tally's clock), when the run counts.
    pub(crate) fn count_solve(&mut self, at: u64) {
        let end = self.stamp();
        if let Some(t) = self.tally() {
            t.solve.calls += 1;
            t.solve.ns += end.saturating_sub(at);
        }
    }

    /// Whether this run met a hole in a world that stops at holes ([`World::stops_at_holes`]):
    /// then nothing past it runs, a site that met the stop and would go on (writeback's
    /// selected rows, rules/writeback/selected_rows.rs) included.
    pub(crate) fn stopped(&self) -> bool {
        self.holed() && self.world.stops_at_holes()
    }

    /// Whether this run met a hole: then it makes no result (`Hole`).
    pub(crate) fn holed(&self) -> bool {
        #[cfg(test)]
        if self.hole_plant == HolePlant::AnswerThroughHoles {
            return false;
        }
        !self.holes.is_empty()
    }

    /// The first hole this run met: a one-shot question's answer read through it is void
    /// (instance.rs `unholed`).
    pub(crate) fn first_hole(&self) -> Option<Question> {
        self.holes.first().map(|h| h.question)
    }

    /// How many holes this run met so far: a fill compares it before and after.
    pub(crate) fn hole_count(&self) -> usize {
        self.holes.len()
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
        Self::made(world, Some(log), None, alloc)
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
    fn record<T: ContentHash + Holed>(&mut self, q: Question, got: Fetch<T>, asked_at: u64) -> Result<T, Stop> {
        self.record_with(q, got, asked_at, |_, v| Ok(Some(hash_of("answer", v))))
    }

    /// The read-list digest of this run's latest answer to `q` (a record already read).
    pub(crate) fn digest(&self, q: Question) -> Result<u64, CheckError> {
        self.recorded(q).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))
    }

    /// The read-list digest of this run's latest answer to `q`, `None` when no answer to it
    /// entered the list (never asked, a hole, or an opaque seen as itself: [`Reads::opaque_record`]).
    pub(crate) fn recorded(&self, q: Question) -> Option<u64> {
        self.entries.iter().rev().find(|e| e.0 == q).map(|e| e.1)
    }

    /// `record` for an answer with a table: its digest covers only the terms it reaches.
    fn record_tabled<T: Tabled + Holed>(&mut self, q: Question, got: Fetch<T>, asked_at: u64) -> Result<T, Stop> {
        self.record_with(q, got, asked_at, |h, v| h.rooted(v).map(Some))
    }

    /// The one seam's body. `digest` gives the answer's read-list digest, or `None` for an
    /// answer that is no read of the program (it is asked, counted and logged as any answer,
    /// and enters no read list).
    fn record_with<T: Holed>(
        &mut self,
        q: Question,
        got: Fetch<T>,
        asked_at: u64,
        digest: impl FnOnce(&mut Hashes<'a>, &T) -> Result<Option<u64>, CheckError>,
    ) -> Result<T, Stop> {
        let answered_at = self.stamp();
        // The answer is in ([`Reads::ask`] began it): the world's reads are its own again.
        self.world.asking(Self::seal(), None);
        if got.is_ok() {
            self.heard.insert(q);
        }
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
                if let Some(d) = digest(&mut self.hashes, &v)? {
                    self.entries.push((q, d));
                }
                self.count(q, false, asked_at, answered_at);
                Ok(v)
            }
            Err(Missing) => {
                // A hole: record the need; then, as the world says ([`World::stops_at_holes`]),
                // stop the run at it (nothing past a missing fact) or read the stand-in and go on
                // (`Hole`). Either way the run is holed and makes no result.
                self.count(q, true, asked_at, answered_at);
                self.holes.push(Hole { question: q });
                if self.world.stops_at_holes() {
                    return Err(Stop::Need(q));
                }
                Ok(T::hole())
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
                Some(l) => self.ask_ahead(Question::LangPath(l), |w| w.lang_path(Self::seal(), l)),
                None => {
                    names.clear();
                    let Ok(segs) = body.segment_list(bp.segments) else { continue };
                    names.extend(segs.iter().map(|s| s.name));
                    let path = PathIdx(i as u32);
                    self.ask_ahead(Question::Path(bp.scope, path), |w| w.resolve_path(Self::seal(), bp.scope, path, &names)).map(|r| r.ok().map(|r| r.def))
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
                Def::Alias(k) => {
                    second.push(Question::TypeAlias(k));
                    second.push(Question::ConstParams(k));
                }
                Def::TyParam(_) | Def::Prim(_) | Def::SelfTy => {}
            }
        }
        for e in body.exprs {
            let crate::body::Expr::MethodCall { scope, name, .. } = *e else { continue };
            let Ok(traits) = self.ask_ahead(Question::TraitsInScope(scope), |w| w.traits_in_scope(Self::seal(), scope)) else { continue };
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
        #[cfg(test)]
        if self.hole_plant == HolePlant::KeepEarlierReads {
            self.entries.extend(questions.iter().map(|&q| (q, 0)));
        }
        self.world.prefetch(Self::seal(), questions);
    }

    pub fn fn_sig(&mut self, item: ItemKey) -> Result<Option<FnSig<'w>>, Stop> {
        let asked_at = self.ask(Question::FnSig(item));
                let got = self.world.fn_sig(Self::seal(), item);
        self.record_tabled(Question::FnSig(item), got, asked_at)
    }

    pub fn predicates(&mut self, item: ItemKey) -> Result<Predicates<'w>, Stop> {
        let asked_at = self.ask(Question::Predicates(item));
                let got = self.world.predicates(Self::seal(), item);
        self.record_tabled(Question::Predicates(item), got, asked_at)
    }

    pub fn adt(&mut self, item: ItemKey) -> Result<Option<Adt<'w>>, Stop> {
        let asked_at = self.ask(Question::Adt(item));
                let got = self.world.adt(Self::seal(), item);
        self.record_tabled(Question::Adt(item), got, asked_at)
    }

    pub fn impls_of(&mut self, trait_: ItemKey, shape: Shape) -> Result<&'w [ItemKey], Stop> {
        let asked_at = self.ask(Question::ImplsOf(trait_, shape));
                let got = self.world.impls_of(Self::seal(), trait_, shape);
        self.record(Question::ImplsOf(trait_, shape), got, asked_at)
    }

    /// An impl's header, checked where it enters: a trait impl's arguments after `Self` must
    /// number the trait's parameters besides `Self` (`CheckError::ImplTraitArity` names it).
    pub fn impl_header(&mut self, item: ItemKey) -> Result<Option<ImplHeader<'w>>, Stop> {
        let asked_at = self.ask(Question::ImplHeader(item));
                let got = self.world.impl_header(Self::seal(), item);
        let h = self.record_tabled(Question::ImplHeader(item), got, asked_at)?;
        if let Some(ImplHeader { trait_: Some(trait_), trait_args, .. }) = h {
            let holes = self.holes.len();
            let Some(def) = self.trait_def(trait_)? else {
                if self.holes.len() > holes {
                    // The trait's record is a hole: no arity to compare yet (`Hole`).
                    return Ok(h);
                }
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
        let asked_at = self.ask(Question::Inherent(shape, name));
                let got = self.world.inherent(Self::seal(), shape, name);
        self.record(Question::Inherent(shape, name), got, asked_at)
    }

    pub fn assoc_item(&mut self, container: ItemKey, name: Name) -> Result<Option<AssocItem>, Stop> {
        let asked_at = self.ask(Question::AssocItem(container, name));
                let got = self.world.assoc_item(Self::seal(), container, name);
        self.record(Question::AssocItem(container, name), got, asked_at)
    }

    pub fn traits_in_scope(&mut self, scope: ScopeId) -> Result<&'w [ItemKey], Stop> {
        let asked_at = self.ask(Question::TraitsInScope(scope));
                let got = self.world.traits_in_scope(Self::seal(), scope);
        self.record(Question::TraitsInScope(scope), got, asked_at)
    }

    pub fn resolve_path(
        &mut self,
        scope: ScopeId,
        path: PathIdx,
        segments: &[Name],
    ) -> Result<crate::path_miss::PathAnswer, Stop> {
        let asked_at = self.ask(Question::Path(scope, path));
                let got = self.world.resolve_path(Self::seal(), scope, path, segments);
        self.record(Question::Path(scope, path), got, asked_at)
    }

    pub fn lang_item(&mut self, item: LangItem) -> Result<Option<ItemKey>, Stop> {
        let asked_at = self.ask(Question::Lang(item));
                let got = self.world.lang_item(Self::seal(), item);
        self.record(Question::Lang(item), got, asked_at)
    }

    pub fn lang_path(&mut self, item: LangItem) -> Result<Option<Def>, Stop> {
        let asked_at = self.ask(Question::LangPath(item));
                let got = self.world.lang_path(Self::seal(), item);
        self.record(Question::LangPath(item), got, asked_at)
    }

    pub fn const_value(&mut self, item: ItemKey) -> Result<Option<u64>, Stop> {
        let asked_at = self.ask(Question::Const(item));
                let got = self.world.const_value(Self::seal(), item);
        self.record(Question::Const(item), got, asked_at)
    }

    pub fn opaque_hidden(&mut self, item: ItemKey) -> Result<Option<Hidden<'w>>, Stop> {
        use crate::rules::async_fn::{SupersetFrom, superset_record};
        let hidden = self.opaque_record(item)?;
        if let Some(h) = hidden {
            let alloc = self.alloc();
            if superset_record(self, SupersetFrom::Import(h), alloc)?.is_some() {
                // The opaque's declared bounds still apply. Structural auto-trait leakage
                // waits until the producer supplies the stored state.
                return Ok(None);
            }
        }
        Ok(hidden)
    }

    /// The hidden type of `item` as this run's world reveals it. A hidden type given is a read
    /// (its digest enters the list). `None`, the opaque seen as itself (not in the running
    /// check's leak list: front2 opaque_auto.rs `Reveal::InBody`), is NOT: it reads nothing of
    /// the program (the opaque's bounds are their own read, `Question::Predicates`), only the
    /// state of this check, which a later reveal changes while the program stays as it was.
    /// Recorded, it put two answers to one fact in one body's list whenever two parts of a
    /// check saw the opaque at two leak lists (the check's last round, then the stalled-goal
    /// decision that revealed it: front2 stalled_goals.rs), and the list was refused
    /// (`TwoAnswers`, rule 21). A check that sees the opaque as itself decides nothing by its
    /// hidden type (an auto trait on it is undecided until it is revealed: blaze-solve
    /// `Ask::first_hidden_unknown`), so its answers depend only on the reads it keeps.
    pub(crate) fn opaque_record(&mut self, item: ItemKey) -> Result<Option<Hidden<'w>>, Stop> {
        let asked_at = self.ask(Question::Opaque(item));
        let got = self.world.opaque_hidden(Self::seal(), item);
        self.record_with(Question::Opaque(item), got, asked_at, |h, v| match v {
            Some(_) => h.rooted(v).map(Some),
            None => Ok(None),
        })
    }

    pub fn closure(&mut self, owner: ItemKey, ordinal: u32) -> Result<Option<(ClosureFacts<'w>, Option<PTy>)>, Stop> {
        let asked_at = self.ask(Question::Closure(owner, ordinal));
        let got = self.world.closure_with_captures(Self::seal(), owner, ordinal);
        self.record_tabled(Question::Closure(owner, ordinal), got, asked_at)
    }

    pub fn coroutine(&mut self, owner: ItemKey, ordinal: u32) -> Result<Option<CoroutineFacts<'w>>, Stop> {
        Ok(self.coroutine_record(owner, ordinal)?.filter(|facts| facts.is == InteriorIs::Exact))
    }

    /// Read the precision and hash the producer's actual answer, including provisional
    /// rows. Only `coroutine` supplies facts for importing a solver term.
    pub(crate) fn coroutine_record(&mut self, owner: ItemKey, ordinal: u32) -> Result<Option<CoroutineFacts<'w>>, Stop> {
        let asked_at = self.ask(Question::Coroutine(owner, ordinal));
        let got = self.world.coroutine(Self::seal(), owner, ordinal);
        self.record_tabled(Question::Coroutine(owner, ordinal), got, asked_at)
    }

    /// The world's table of shared goals ([`World::goal_share`]), for the solver's source.
    pub(crate) fn goal_share(&self) -> Option<&'w dyn blaze_solve::share::GoalShare> {
        self.world.goal_share()
    }
    pub fn attrs(&mut self, item: ItemKey) -> Result<TypeAttrs, Stop> {
        let asked_at = self.ask(Question::Attrs(item));
                let got = self.world.attrs(Self::seal(), item);
        self.record(Question::Attrs(item), got, asked_at)
    }

    pub fn trait_def(&mut self, trait_: ItemKey) -> Result<Option<TraitDef<'w>>, Stop> {
        let asked_at = self.ask(Question::TraitDef(trait_));
                let got = self.world.trait_def(Self::seal(), trait_);
        self.record_tabled(Question::TraitDef(trait_), got, asked_at)
    }

    pub fn assoc_parent(&mut self, item: ItemKey) -> Result<Option<AssocParent>, Stop> {
        let asked_at = self.ask(Question::AssocParent(item));
                let got = self.world.assoc_parent(Self::seal(), item);
        self.record(Question::AssocParent(item), got, asked_at)
    }

    pub fn const_holds(&mut self, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Result<ConstHolds, Stop> {
        let q = Question::ConstHolds(cond, ground_args_digest(args, types));
        let asked_at = self.ask(q);
                let got = self.world.const_holds(Self::seal(), cond, args, types);
        self.record(q, got, asked_at)
    }

    pub fn const_params(&mut self, item: ItemKey) -> Result<ConstParams<'w>, Stop> {
        let asked_at = self.ask(Question::ConstParams(item));
                let got = self.world.const_params(Self::seal(), item);
        self.record_tabled(Question::ConstParams(item), got, asked_at)
    }

    /// The generics of a trait's associated type ([`World::assoc_generics`]).
    pub fn assoc_generics(&mut self, trait_: ItemKey, name: Name) -> Result<Option<AssocGenerics>, Stop> {
        let asked_at = self.ask(Question::AssocGenerics(trait_, name));
        let got = self.world.assoc_generics(Self::seal(), trait_, name);
        self.record(Question::AssocGenerics(trait_, name), got, asked_at)
    }

    /// A type alias's definition ([`World::type_alias`]).
    pub fn type_alias(&mut self, item: ItemKey) -> Result<Option<AliasDef<'w>>, Stop> {
        let asked_at = self.ask(Question::TypeAlias(item));
        let got = self.world.type_alias(Self::seal(), item);
        self.record_tabled(Question::TypeAlias(item), got, asked_at)
    }

    /// The default value of each field of ADT `adt` ([`World::field_defaults`]).
    pub fn field_defaults(&mut self, adt: ItemKey) -> Result<&'w [Option<ItemKey>], Stop> {
        let asked_at = self.ask(Question::FieldDefaults(adt));
        let got = self.world.field_defaults(Self::seal(), adt);
        self.record(Question::FieldDefaults(adt), got, asked_at)
    }

    /// Whether the body may name inherent member `name` of `container`
    /// ([`World::member_visible`]).
    pub fn member_visible(&mut self, container: ItemKey, name: Name) -> Result<bool, Stop> {
        let asked_at = self.ask(Question::MemberVisible(container, name));
        let got = self.world.member_visible(Self::seal(), container, name);
        self.record(Question::MemberVisible(container, name), got, asked_at)
    }

    /// Whether inherent member `name` of `container` is unstable under a feature the body's
    /// crate does not turn on ([`World::member_unstable`]).
    pub fn member_unstable(&mut self, container: ItemKey, name: Name) -> Result<bool, Stop> {
        let asked_at = self.ask(Question::MemberUnstable(container, name));
        let got = self.world.member_unstable(Self::seal(), container, name);
        self.record(Question::MemberUnstable(container, name), got, asked_at)
    }

    /// The test build's one backdoor: an ADT read that is not recorded. Only the planted bug
    /// of the read-list property calls it, to show the oracle catches what the seal prevents.
    #[cfg(test)]
    pub(crate) fn peek_unrecorded_adt(&self, item: ItemKey) -> Fetch<Option<Adt<'w>>> {
        self.world.adt(Self::seal(), item)
    }

    /// The read list and the needs of the run, sorted and deduplicated (rule 25).
    pub(crate) fn finish(mut self, alloc: Alloc<'a>) -> (ReadList<'a>, Needs<'a>) {
        let recorded = self.entries.len() as u64;
        self.entries.sort_unstable();
        self.entries.dedup();
        let kept = self.entries.len() as u64;
        if let Some(t) = self.tally() {
            t.solve.entries_recorded += recorded;
            t.solve.entries_kept += kept;
        }
        let mut rows = Soa::with_capacity_in(self.entries.len(), alloc);
        let mut needs = Vec::with_capacity_in(self.entries.len(), alloc);
        for &(q, h) in self.entries.iter() {
            rows.push((q, h));
            if needs.last() != Some(&q) {
                needs.push(q);
            }
        }
        (ReadList { rows }, Needs { questions: needs, missing: Vec::new_in(alloc), first: None })
    }

    /// The needs of a run that met a hole: every hole, sorted and without duplicates (rule
    /// 25), the first one met, and every question asked as the next run's hints. `stopped` is a
    /// need that ended the walk instead of becoming a hole (a `Stop::Need` made by a rule or
    /// the solver, not by this seam): a hole like the others, the last one met.
    pub(crate) fn finish_need(self, stopped: Option<Question>, alloc: Alloc<'a>) -> Needs<'a> {
        let first = self.holes.first().map(|h| h.question).or(stopped);
        let mut missing = Vec::with_capacity_in(self.holes.len() + 1, alloc);
        missing.extend(self.holes.iter().map(|h| h.question));
        missing.extend(stopped);
        missing.sort_unstable();
        missing.dedup();
        let mut questions = Vec::with_capacity_in(self.entries.len() + missing.len(), alloc);
        questions.extend(self.entries.iter().map(|&(q, _)| q));
        questions.extend_from_slice(&missing);
        questions.sort_unstable();
        questions.dedup();
        Needs { questions, missing, first }
    }
}

// The content of every answer: what a read list hashes.

impl ContentHash for FnSig<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.parent_count);
        h.u32(self.own_count);
        h.tag(u8::from(self.self_param));
        h.tag(self.header.bits() | if self.c_variadic { 0x40 } else { 0 });
        h.u64(self.legacy_const_generics.len() as u64);
        for &i in self.legacy_const_generics {
            h.u32(i);
        }
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
            // The type, not the region: the clause is true for every type check, so its region
            // is no input of one; the head's regions digest holds it (D148: a lifetime-only
            // edit moves that digest and not this one). The tag stays: the clause takes a place
            // in the list, and `nth` counts it.
            PClause::TypeOutlives { ty, region: _ } => {
                h.tag(3);
                h.u32(ty.0);
            }
            PClause::RegionOutlives { long: _, short: _ } => h.tag(4),
            PClause::ForFields { container, trait_, args } => {
                h.tag(5);
                h.u32(container.0);
                h.u128(trait_.0);
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

impl ContentHash for &[Option<ItemKey>] {
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
            Def::SelfTy => h.tag(8),
            Def::Alias(k) => {
                h.tag(9);
                h.u128(k.0);
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
        match self.defaults {
            TraitDefaults::NotKnown => h.tag(0),
            TraitDefaults::Known(ds) => {
                h.tag(1);
                h.u64(ds.len() as u64);
                for d in ds {
                    match d {
                        Some(t) => {
                            h.tag(1);
                            h.u32(t.0);
                        }
                        None => h.tag(0),
                    }
                }
            }
        }
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

impl ContentHash for AssocGenerics {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(u64::from(self.parent));
        h.u64(u64::from(self.own));
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
