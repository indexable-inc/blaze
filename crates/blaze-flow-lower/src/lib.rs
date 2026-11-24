//! The lowering from a type-checked body to its flow (docs/v2/plan/design/flow.md, "The lowering";
//! decision D61): the body the type checker walked (`BodyView`) and its record (`Checked`) in, a
//! [`blaze_flow::Flow`] out, or a typed [`Refusal`] naming the form it does not take yet and its
//! item-relative range. A body is lowered whole or not at all.
//!
//! The rules, from the written sources (flow.md "Sources, quoted"): place and value expression
//! contexts (the Reference, "Expressions"); moves by the type's `Copy` fact ("Moved and copied
//! types"); temporaries dropped at the end of their statement, extended by `let` ("Destructors",
//! "Temporary lifetime extension"), the tail's temporaries before the block's locals from edition
//! 2024; locals dropped in reverse order of declaration; two-phase borrows for an autoref'd
//! receiver (RFC 2025); a fake shallow borrow of the scrutinee across a guard; origins from types
//! and the elision rules for a call (the Reference, "Lifetime elision").
//!
//! No recursion: one task stack and one value stack, every loop bounded by the body's size.

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api)]

#[cfg(test)]
extern crate std;

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_flow::{Block, BorrowKind, Flow, FlowBuilder, Loan, Local, Operand, Origin, Place, Proj, Span, StmtKind};
use crate::identity::{Identity, Role};
use blaze_typeck::{
    AdjustKind, BinOp, BodyView, Checked, Def, Edition, Expr, ExprId, ItemKey, List, Mutbl, Name, PNode, PTy, PTys, Pat,
    PatId, Range, Stmt as TStmt, StmtId, UnOp,
};

pub mod sig;
/// Calls of known generic heads, by their signature instantiated with the call's type
/// arguments and their declared bounds (pieces BR4 and BR3: census 3's false findings in
/// derived `eq`/`cmp`/`partial_cmp`, and the `CallWritesLifetimes` refusals), and the
/// `format_args!` flow (`write!`).
mod generic_call;
/// Closure captures as places (RFC 2229 disjoint fields and `&*self`).
mod capture_places;
/// Step 1 of an `.await` whose future is not its operand: `into_future` by its recorded head.
mod await_call;
#[cfg(test)]
mod hidden_call_tests;
/// A const argument in a declared type holds no position (tempo 1120's two largest groups).
#[cfg(test)]
mod const_arg_tests;
/// A local's storage end invalidates its loans whatever its type (storage-end-loans).
#[cfg(test)]
mod storage_end_tests;
#[cfg(test)]
mod temp_scope_tests;
/// A closure's late-bound region that must outlive a region its owner names: E0521.
#[cfg(test)]
mod escapes_tests;
#[cfg(test)]
mod sig_lay_over_tests;
#[cfg(test)]
mod impl_call_tests;
pub use generic_call::{CallBounds, InputProjectionWhy, Mark, Misfit, OutlivesType, Shape, ShapeBuf, ShapeRefusal, Skel, read_shape};
/// A callee's trait lifetimes tied to the caller's regions at a call (piece RB8).
mod trait_ties;
pub use trait_ties::{Tie, TieMiss, Ties};
#[cfg(test)]
mod trait_ties_tests;
/// A bound over a concrete type answered by the impl the type check selected (lane B12).
#[cfg(test)]
mod impl_ties_tests;
/// A projection over a bare type parameter whose trait declares lifetimes (piece open-projection).
#[cfg(test)]
mod open_projection_tests;
/// A caller's own type parameters under a callee's `T: 'a` (static-param-gap-1).
mod param_outlives;
pub use param_outlives::ParamOrigin;
/// A call's type-outlives bound and an opaque's captures read from the region facts (L4).
#[cfg(test)]
mod outlives_call_tests;
mod super_let;
#[cfg(test)]
mod tests;
/// The fixtures' region facts declared by hand and laid in the checked record's table (D148).
#[cfg(test)]
mod region_fixture;
/// A subpattern that peels references under an ADT field (piece RB7).
#[cfg(test)]
mod peel_tests;
/// An ADT field is the positions its declared type names (lane RB10).
#[cfg(test)]
mod field_regions_tests;
/// A `Copy` impl bounds only the arguments it names (lane RB10).
#[cfg(test)]
mod copy_args_tests;
/// A reference argument's loan through `unify_call`'s hub (lane RB10's repro for RB8).
#[cfg(test)]
mod hub_call_tests;

/// A fn's signature regions as the lowering takes them: the head record's column (D148), read
/// through [`Facts::regions`]. `Region` is the one region of a column (blaze-typeck regions.rs).
pub use blaze_typeck::region_facts::{RegionFacts, RegionsUnread, SigRegions, UnreadWhy};
pub use blaze_typeck::regions::Region;
pub use sig::life::{BodyOut, CallValue, NoRegions, SigLife};
/// `Self::Name` read through the impl's own text (piece sig-assoc-projection).
#[cfg(test)]
mod sig_assoc_tests;
/// A written path resolved before its lifetimes are read: aliases expanded (piece sig-paths).
#[cfg(test)]
mod sig_alias_tests;
/// The ban of the text reader (L6): no source file of this crate names it.
#[cfg(test)]
mod text_reader_ban_tests;

pub mod coroutine;
pub mod identity;
pub mod state_recipe;
#[cfg(test)]
mod coroutine_tests;

pub use coroutine::{AsyncWitness, InteriorPart, LoweredAsync, interior, interior_parts, lower_async, lower_async_witness};

pub mod closure;

pub use closure::{ClosureFlow, External, Need, Requirement, SigSlot, escapes_closure, lower_async_fn, lower_closure, lower_closure_external, requirement};

// Guards that move, or-pattern bindings, slice patterns (lane B8).
mod pattern_forms;
use pattern_forms::{BindItem, BindPass};
#[cfg(test)]
mod pattern_forms_tests;

// Calls through a value of a closure made in this body (lane B8).
mod callee_value;
#[cfg(test)]
mod callee_value_tests;
#[cfg(test)]
mod closure_requirements_tests;

mod variance;
pub use variance::Variance;
#[cfg(test)]
mod variance_tests;
/// THE GRAIN of a place's origins, a type parameter through the call rule (D156, D190 step
/// 3): the flow's positions under the old paths, the type's region slots under the one rule.
mod grain;
#[cfg(test)]
mod grain_tests;

/// The call rule as a type parameter at one seam (D156, rule 78): `OneRule` (production) and
/// `Erased` (region-erased async witnesses).
pub(crate) mod one_call;
pub use one_call::{CountAt, MalformedWhy, OneRuleWhy, Unselected};
/// blaze-flow's builder wrapper.
mod builder;
/// Fixture lowering helpers (production's `OneRule` + async witness cross-check).
#[cfg(test)]
pub(crate) mod oracle;

/// What the lowering needs to know about an ADT (flow.md section 7, question (e)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdtFacts {
    /// A `Copy` impl exists; the type is `Copy` when its type arguments are (the derive's rule).
    pub copy: bool,
    /// The type implements `Drop` (its fields cannot be moved out).
    pub drop_impl: bool,
    /// Dropping it may run code (a `Drop` impl, or a field that does).
    pub needs_drop: bool,
    /// How many lifetime parameters its head declares: one origin position each, in declared
    /// order, each invariant (blaze-ladder front2 lifetimes.rs `Grain::PerParam`).
    pub lifetimes: u32,
    pub union: bool,
    pub is_enum: bool,
    pub is_box: bool,
}

/// How a field's declared type begins, for a pattern that peels references under the field
/// ([`Facts::field_lead`]). The Reference, "Binding modes": a non-reference pattern (a struct,
/// tuple struct, tuple or slice pattern) met by a reference derefs it and makes the default
/// binding mode `ref` (`ref mut` for a `&mut` met under a move mode). The type the subpattern
/// meets is the field's declared type at the ADT's type arguments, which the type checker's
/// record does not hold (it records the subpattern's type AFTER its peels, rules/pat.rs
/// `step_pat`, not `pat_adjustments`): this is what lets the lowering rebuild it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldLead {
    /// Declared as the ADT's type parameter `i` (`Some(T)`'s field): the field's type is the
    /// `i`th type argument, whatever references it holds.
    Param(u32),
    /// Declared `&'a T` or `&'a mut T` (`mutbl`), whose referent is the ADT's type parameter
    /// `param` (`Some`), or a type whose head is no reference, type parameter, projection,
    /// opaque type or trait object (`None`: after this one peel the subpattern meets its own
    /// type).
    Ref { mutbl: Mutbl, param: Option<u32> },
    /// Declared as a type whose head is no reference, type parameter, projection, opaque type
    /// or trait object (an ADT, a tuple, a scalar, an array): nothing to peel there.
    Plain,
    /// Any other declared type (a projection or an opaque type that may normalize to a
    /// reference, two references over a non-parameter): its peels are not read.
    Unread,
}

/// What stands at one stretch of origin positions of a field's DECLARED type, in the order the
/// lowering counts positions (`npos`: pre-order, a reference's own before its referent's, an
/// ADT's lifetimes before its arguments'; one for a trait object, an opaque type or an alias),
/// for [`Facts::field_regions`]. The field's type is its declared type at the ADT's arguments
/// (the Reference, "Generic parameters": a generic item's fields are its declared field types
/// with every parameter substituted), so each of its positions IS one of the value's: a
/// field's region is no fresh region the borrow check may choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldRegion {
    /// One position: the ADT's lifetime parameter `i`, in declared order.
    Lifetime(u32),
    /// One position: `'static` (written, or a trait object's default with no bound: the
    /// Reference, "Default trait object lifetimes").
    Static,
    /// Every position of the ADT's type argument `i` (its index in the record's argument
    /// list, as [`Facts::field_param`] counts it), in that argument's own order: a field
    /// declared as the parameter `T`, or `T` standing anywhere in it.
    Arg(u32),
    /// One position: a lifetime bound by a fn-pointer type of the field's written type
    /// (`f: fn(&u8)`, sig.rs `Region::Bound`, numbered within the field), no region of the
    /// value's: an existential origin of the field's place, one per number (lane B15).
    Bound(u32),
}

pub use blaze_borrow::SelfShape;

/// The enclosing trait impl of the body owner, when the body is a method of a trait impl
/// whose `Self` type has a matchable outer constructor (`Adt`, `Ref`, or `Tuple`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerTraitImpl {
    pub imp: ItemKey,
    pub trait_: ItemKey,
    pub generics: u32,
    pub self_shape: SelfShape,
}

/// What the lowering needs to know about a callee's signature (flow.md section 7, heads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalleeFacts {
    /// The signature writes a lifetime (`'a`, `'static`): its relations need the heads' lifetime
    /// part, not built; such a call is refused.
    pub writes_lifetimes: bool,
    /// The first input is `&self` or `&mut self`: elided outputs take its lifetime.
    pub self_ref: bool,
}

/// The facts the type checker's record does not hold (flow.md section 7: the named questions).
///
/// NO DEFAULT ANSWER (rules 54, 73): every method that states a fact is required, so an
/// implementor, and above all a wrapper (front2 borrows_nested.rs `NestedFacts`, task.rs
/// `KeepFacts`), writes each answer: a wrapper forwards what it wraps, and a fixture writes
/// its "not known" answer (`None`, `false`, `&[]`, `&NoRegions`). A forgotten method does not
/// compile. Three times a wrapper silently took a default here (`implied_bounds`,
/// `constant_initializer`, then `trait_lifetimes` and `alias_lifetimes`: census wave-1119's
/// 30,061 `Count` and 9,512 `HeadUnread`). Only [`Facts::sig`] is provided: it is DERIVED
/// from [`Facts::regions`], not a fact of its own.
pub trait Facts {
    fn adt(&self, adt: ItemKey) -> Option<AdtFacts>;
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts>;
    /// The head records' regions (D148, flow.md "Lifetimes are produced once and carried beside
    /// the types"): every lifetime of a declared type the lowering reads comes from here, never
    /// from a text. Not known: [`NoRegions`], every item's regions unread, so a call that
    /// carries a lifetime is refused by name ([`Form::RegionsUnread`]), never guessed.
    fn regions(&self) -> &dyn RegionFacts;
    /// The type table the columns of [`Facts::regions`] are laid over (each `ColView::ty`, its
    /// declared types): the head records' (front2 `Store::tys`), never the checked record's,
    /// whose `PTy`s number other types. `None`: the columns were laid over the checked record's
    /// own table (the fixtures import their declared heads into it, region_fixture.rs).
    /// Every declared type of a column is read in this table and only here
    /// ([`Lower::head_node`]); a count of the body's own types is compared with it, never a
    /// `PTy` of one table read in the other.
    fn region_tys(&self) -> Option<PTys<'_>>;
    /// The lifetime part of `item`'s signature (flow.md, "What the signature must say (H1)"),
    /// read from [`Facts::regions`]: the body's own gives its placeholders and what is known
    /// among them; a callee's is instantiated at each call, each `Param` and each `Late` one
    /// fresh origin a call. `Err`: not read, and why (the one refusal, [`Form::RegionsUnread`]).
    /// Not to be answered another way: there is one source of a fn's regions.
    fn sig(&self, item: ItemKey) -> Result<SigLife<'_>, RegionsUnread> {
        self.regions().sig(item).map(SigLife::new)
    }
    /// The variance of each generic parameter of ADT `adt` (the Reference, "Subtyping and
    /// Variance": an ADT is variant in a parameter as its fields are), its lifetime parameters
    /// first, then its type parameters, in head order (variance.rs). A slice shorter than the
    /// parameters leaves the rest invariant, as `None` leaves every one (the sound answer).
    /// Whoever answers files the read of what the variances were computed from (rule 21).
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]>;
    /// The bounds the body owner's signature types imply, and its impl's header types (RFC
    /// 1214: a fn assumes its inputs and output well-formed, an impl its header), whole: every
    /// reference's `T: 'r` and every ADT's requirements, declared and inferred (RFC 2093),
    /// split componentwise, in the owner's numbering: `(type parameter, region)` and
    /// `(long, short)`. `None`: not computed (nothing beyond what `signature` reads itself).
    /// Whoever answers files the reads it was computed from (rule 21).
    fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])>;
    /// Whether the body is a constant initializer: a `const` or `static` item's, or an
    /// associated const's (the Reference, "Destructors", "Temporary lifetime extension": "The
    /// temporary scopes for expressions in `const` and `static` items and in `const` blocks are
    /// extended to the end of the program"; rustc region.rs `resolve_body`). Its extending
    /// borrows' temporaries are `'static` (lib.rs `STATIC_EXTENDED`). Asked because such an item's
    /// head is a fn of no inputs (blaze-ladder heads.rs), so the head alone cannot tell it from
    /// `fn f() -> &'static S`, whose `&mk()` is E0515. `false`: a fn's body (every fixture's
    /// owner); whoever lowers item bodies answers it from the owner's kind.
    fn constant_initializer(&self) -> bool;
    /// Whether the regions a fn-pointer type's binder binds ([`Region::Late`]) are read as
    /// bound: the body's own an existential origin each, a call
    /// through a fn-pointer parameter instantiating them afresh. `false` is the control of
    /// lane B15 (rule 23, front2 `FactsPlant::FnBindersFree`): every one read as FREE, a
    /// placeholder of the body's like a named lifetime, taken at a call from the parameter's
    /// own position, as a reading that knew no binder would. Every implementor but that
    /// control answers `true`.
    fn fn_binders(&self) -> bool;
    /// Whether the caller checks each closure's BODY on its own (its captures as inputs of
    /// their modes). The lowering makes a closure expression the borrows and moves of its
    /// captures and does not descend into its body; while nothing checks that body, a body
    /// holding a closure is refused (`Form::Closure`: not checked, never accepted), which is
    /// what `false` says.
    fn closure_bodies_checked(&self) -> bool;
    /// The requirements closure `closure`'s own check put on the flow that makes it
    /// ([`Requirement`]: relations among the closure value's regions its body needs; closure.rs
    /// [`requirement`]): each is emitted as a constraint at the closure expression, so one the
    /// owner cannot prove is the owner's finding at the closure's range. The caller checks a
    /// closure's flow BEFORE the flow that holds its expression and answers with what it found;
    /// empty: nothing required (or the closure not checked yet: then its outlives results must
    /// still be refused, never dropped). An async block's requirements are asked the same way,
    /// by the block's expression (coroutine.rs `LoweredAsync::externals`; `closure_value` makes
    /// both values and emits both).
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement];
    /// Whether the caller checks each async block's BODY on its own ([`lower_async`]). The
    /// lowering makes an async block expression the borrows and moves of its captures (as a
    /// closure's) and does not descend into its body; while nothing checks that body, a body
    /// holding an async block is refused (`Form::AsyncBlock`), which is what `false` says.
    fn async_bodies_checked(&self) -> bool;
    /// Whether `ty` (a type of the body's record, `Checked::types`) implements `Future`. An
    /// `.await`'s operand must: the future it holds is then the operand itself (the library's
    /// `impl<F: Future> IntoFuture for F`, core/src/future/into_future.rs). `None`: not known,
    /// and such an `.await` is refused (`Form::AwaitOperand`). A future of this body
    /// (`PNode::Future`) or a coroutine (`PNode::Coroutine`) is not asked.
    fn is_future(&self, ty: PTy) -> Option<bool>;
    /// The generic shape of callee `item`'s head ([`read_shape`]): where its generic
    /// parameters stand among its positions. With [`Facts::sig`], a call whose type arguments
    /// carry origins is related by the instantiated signature (generic_call.rs); `None`: such
    /// a call keeps the one origin of `unify_call`.
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>>;
    /// Whether trait `trait_`, the principal of a trait object of the body's record, declares
    /// a lifetime parameter. `Some(false)` lets an unsized coercion to that object relate the
    /// source's positions into the object lifetime one way ([`Lower::unsize_to_object`]);
    /// `None` (not known) or `Some(true)` keeps the coarse relation, every position equal.
    /// Whoever answers files the read of the trait's text in the body's read list (rule 21).
    fn object_trait_lifetimes(&self, trait_: ItemKey) -> Option<bool>;
    /// The type-outlives bounds callee `item` declares (`T: 'a`; generic_call.rs
    /// [`CallBounds`]). With [`Facts::sig`] and [`Facts::shape`], a call whose type arguments
    /// carry origins, or whose head writes lifetimes, is related by the instantiated
    /// signature. `None`: not known, and such a call keeps `unify_call`'s one origin, or is
    /// refused [`Form::CallWritesLifetimes`] when the head writes lifetimes.
    fn call_bounds(&self, item: ItemKey) -> Option<CallBounds<'_>>;
    /// How many lifetime parameters trait `trait_` declares: the own slots of a clause's trait
    /// reference and of a trait object's principal, as the one region counter counts them
    /// (blaze-typeck regions.rs `Arity::trait_lifetimes`). Read by the one call rule
    /// (one_call.rs) only. `None`: not known, and a call whose types hold the trait is refused
    /// by the one rule with the count named (`Refused::Count`), never counted as 0.
    fn trait_lifetimes(&self, trait_: ItemKey) -> Option<u32>;
    /// For `<_ as trait_>::name`: the trait's lifetime arguments plus a GAT's own lifetimes
    /// (`Arity::alias_lifetimes`). As [`Facts::trait_lifetimes`].
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Option<u32>;
    /// Whether EVERY proof of trait `trait_` relates no region, whatever its instance: no impl
    /// may be written for it, and its builtin rule reads no lifetime (`Sized` and its kin,
    /// `Destruct`; front2 borrows.rs `BorrowLangs::relates_nothing` quotes core). Read by the
    /// one call rule only (one_call.rs `CallSel`, blaze-borrow relate.rs `tie_of`): such a
    /// clause asks no selection. `false`, the provided answer: not known, and the clause is
    /// tied through what selected it, or refused by name (stricter, never looser). Whoever
    /// answers `true` files the read of the trait (rule 21; a lang item of the toolchain).
    fn trait_relates_nothing(&self, trait_: ItemKey) -> bool {
        let _ = trait_;
        false
    }
    /// Whether `trait_` is one of the callable traits (`Fn`, `FnMut`, `FnOnce`: lang items `fn`,
    /// `fn_mut`, `fn_once`). Used by `call_through_value` to locate a generic parameter's or
    /// trait object's callable signature in the owner's clause or signature columns.
    fn is_fn_trait(&self, trait_: ItemKey) -> bool {
        let _ = trait_;
        false
    }
    /// The `AssocTyDef::item` key of associated type `trait_::name` (whose head record holds its
    /// item bounds such as `type Searcher<'a>: Searcher<'a>` or `type Structural: UniqueSet`).
    fn assoc_item(&self, trait_: ItemKey, name: Name) -> Option<ItemKey> {
        let _ = (trait_, name);
        None
    }
    /// If the body owner is a method of a trait impl `impl<..> Trait for SelfTy` (where `SelfTy`
    /// is an `Adt`, `Ref`, or `Tuple`), returns its [`OwnerTraitImpl`] so `blaze-borrow` can
    /// normalize projections on the enclosing impl's own `Self` (e.g. `Cycle<I>::Item`,
    /// `Zip<A, B>::Item`, `(&T0, &T1)::Item`) even when the call site has no explicit selection row.
    fn owner_trait_impl(&self) -> Option<OwnerTraitImpl> {
        None
    }
    /// The unique `Deref` impl for ADT `adt`, when known (`answer_impls(Deref, Adt(adt))` has
    /// exactly one impl). Used by overloaded deref lowering to map `Deref::Target` positions
    /// precisely from `Self` without unifying unrelated generic parameters (e.g. `Vec<T, A>`'s
    /// allocator lifetime).
    fn deref_impl(&self, adt: ItemKey) -> Option<ItemKey> {
        let _ = adt;
        None
    }
    /// Whether `<T as trait_>::name` is a compiler-builtin zero-slot associated type
    /// (`DiscriminantKind::Discriminant`, `Pointee::Metadata`).
    fn is_zero_slot_builtin_assoc(&self, trait_: ItemKey, name: Name) -> bool {
        let _ = (trait_, name);
        false
    }
    /// The trait bounds with lifetime arguments of fn `item` ([`Ties`]): a callee's tie its
    /// trait lifetimes to what the call's arguments answer them with, the body's own say what
    /// its type parameters answer them with (trait_ties.rs). Whoever answers files the read of
    /// the fn's text and each trait's header in the body's read list (rule 21). `None`: not
    /// known, and a callee's trait lifetimes are fresh per call as before piece RB8.
    fn ties(&self, item: ItemKey) -> Option<Ties<'_>>;
    /// The index of field `name` of `variant`.
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32>;
    /// The type parameter field `field` of `variant` is declared as, when its declared type is
    /// a bare one (`Some(T)`'s field is `T`): its index among the ADT's type arguments. `None`:
    /// not known or another type, and a constructor call filling it keeps `unify_call`'s one
    /// origin.
    fn field_param(&self, adt: ItemKey, variant: u32, field: u32) -> Option<u32>;
    /// How field `field` of `variant`'s DECLARED type begins ([`FieldLead`]): what a pattern
    /// that peels references under that field (`Some(Cmd::Hello { who })` against
    /// `Option<&Cmd>`, a field declared `&'a E` matched by `E::V(x)`) peels. `None`: not known,
    /// and only a field [`Facts::field_param`] names is taken; a peeling pattern under any
    /// other field is refused (`Form::PeelUnderAdtField`). Whoever answers files the read of
    /// the ADT's field types in the body's read list (rule 21).
    fn field_lead(&self, adt: ItemKey, variant: u32, field: u32) -> Option<FieldLead>;
    /// Where each origin position of field `field` of `variant`'s DECLARED type stands among
    /// the ADT's own ([`FieldRegion`], in the field type's position order). With it, a field
    /// of an ADT place is a place of exactly those positions (`Lower::field_slot`), and a
    /// struct literal relates each field's value into its own positions only, as a tuple's
    /// element goes into its own; without it (`None`), a field stands for all of the ADT's
    /// positions, and a literal goes through `unify_call`'s one origin, which ties distinct
    /// lifetime parameters together (`Buffered<'f, 'a>`'s `held: Bytes<'a>` took `'f`).
    /// Whoever answers files the read of the ADT's field types in the body's read list
    /// (rule 21), and gives no row for a field whose declared type holds a projection, an
    /// opaque type or an alias (its positions after normalizing are not the declared ones).
    fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]>;
    /// For each type argument of `adt` (the record's argument list, as
    /// [`Facts::field_param`] counts it), whether its `Copy` impl needs that argument to be
    /// `Copy`: `[false]` for `impl<T: ?Sized> Copy for NonNull<T>`, every `true` for a derived
    /// `Copy` (the derive bounds every type parameter by `Copy`). `None`: not read, and every
    /// argument must be `Copy` (the derive's rule, [`AdtFacts::copy`]). Only an impl whose
    /// bounds are nothing but `Copy` on parameters (or none) may be answered: a bound the
    /// lowering does not check (`T: Tr`) gives `None`. Whoever answers files the read of the
    /// impl's header in the body's read list (rule 21).
    fn copy_args(&self, adt: ItemKey) -> Option<&[bool]>;
    /// For each generic argument of `adt` (the record's argument list, as
    /// [`Facts::copy_args`] counts it), whether `adt`'s drop glue drops a value of that
    /// argument: `[false]` for `slice::Iter<'a, T>` (holds `NonNull<T>`, `*const T`,
    /// `PhantomData<&'a T>`), `[true]` for `Option<T>`. Read only when
    /// [`AdtFacts::needs_drop`] is `false`. `None`: every argument may be dropped.
    fn drop_args(&self, _adt: ItemKey) -> Option<&[bool]> {
        None
    }
    /// The number of fields of `variant`.
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32>;
    /// Whether the body's generic parameter `index` is bounded by `Copy`.
    fn param_copy(&self, index: u32) -> bool;
}

/// Which check of a signature's laying refused it ([`Form::SigPositionsUnaligned`]). `input`:
/// the parameter's index, `None` for the return type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SigSite {
    /// The column's input count, or its well-formedness, is not the body's parameters'.
    Arity,
    /// The column has no child for this input or output (`SigLife::input`/`output`).
    NoChild { input: Option<u32> },
    /// The column's root has fewer children than the child asked (`Lower::sig_declared`).
    ColumnShort,
    /// A future returned (an `async fn`'s, or an `impl Future` whose body is an async block)
    /// whose declared output is not an opaque type in the column.
    FutureNotOpaque,
    /// The body's type of the parameter carries an adjustment wrap.
    Wrapped { input: Option<u32> },
    /// `Lower::lay_positions` could not lay the declared type over the slots (two opaque or
    /// projection subtrees, a slot count that differs from the type's).
    Unlaid { input: Option<u32> },
    /// Laid, but to another count of positions than the body's type has.
    Count { input: Option<u32> },
    /// A closure's signature slots (closure.rs `sig_slots`) against its positions.
    ClosureSlots,
    /// An `async fn` root block's written output (coroutine.rs `async_fn_root`).
    AsyncOutput,
}

/// Which check refused a call's type-outlives bound ([`Form::CallOutlivesUnread`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutlivesUnreadWhy {
    /// The parameter is below the bounds' `unread_below` (a bound of the container's header
    /// not resolved), and its argument has positions (`positions`), holds a type the lowering
    /// does not count (`unplaced`: an alias, opaque, closure, future, trait object), or holds
    /// a type parameter of the body (`body_param`). generic_call.rs `call_by_generic_signature`.
    BoundsUnread { positions: u32, unplaced: bool, body_param: bool },
    /// A declared `T: 'r` whose argument's outlives components are not all read: `Param` an
    /// unplaced type in the argument (a closure without its capture witness, an async fn's
    /// future, an opaque, an alias); `UnreadProjection` a bound over a projection;
    /// `FutureOutput` a future's output that has positions or is not a future.
    Components { subject: OutlivesSubject },
    /// The call owes a bound over a caller's type parameter (param_outlives.rs
    /// `owes_type_outlives`), and the generic signature was not taken, for `lacks`.
    Owed { lacks: CallLacks },
}

/// The kind of a callee's type-outlives bound's subject ([`OutlivesType`], without its index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutlivesSubject {
    Param,
    UnreadProjection,
    FutureOutput,
}

/// Which step of closure.rs `lay_sig_typed` refused a clause part ([`Form::ClosureSigUnaligned`]).
/// `declared`/`closure`: the two nodes that did not walk in step, by kind
/// (`closure::node_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClosureSigWhy {
    /// The clause's declared part was not read (no `Args` tuple of the closure's arity, no
    /// projection term).
    NoDeclared,
    /// A node or its slot count of the declared part is not in the heads' table.
    ColumnUnread,
    /// A binder's region stands under a type parameter, projection or opaque type, or among a
    /// trait object's predicates: no position of the closure's is it.
    BinderHidden,
    /// The declared part's slots ran out before its positions.
    SlotsShort,
    /// Two ADTs of different keys, list lengths or lifetime counts.
    AdtDiffers,
    /// A scalar of the declared part where the closure's type has positions.
    ScalarHolds,
    /// Two nodes that do not walk in step.
    Shape { declared: &'static str, closure: &'static str },
    /// Walked, but the slots or positions laid are not the part's counts.
    CountsDiffer,
}

/// A form the lowering does not take yet, or a fact it lacks: named, never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Form {
    Closure,
    AsyncBlock,
    Await,
    Coroutine,
    /// `asm!`: its operands' places and borrows are not lowered yet.
    InlineAsm,
    /// An `async ||` closure: its call future may borrow the closure's environment.
    AsyncClosure,
    /// An `.await` whose held future, `<T as IntoFuture>::IntoFuture`, has no row in the
    /// record (`Checked::awaited_of`: not wholly known), or whose operand is a reference the
    /// record does not name.
    AwaitOperand,
    /// An `.await` whose value carries a lifetime and that nothing relates precisely
    /// (coroutine.rs `await_done`). Its value is the future's `Output`. For an `async fn`'s
    /// call, that value is made from the written return type (await_call.rs `awaited_shadow`).
    /// For an async block of this body, it comes from the block's captures (coroutine.rs
    /// `run_async`). Anything else is refused here: the future's positions hold every borrow it
    /// captured (an opaque type is ONE position, `npos`), so relating the value to them gave
    /// the value the arguments' borrows (tempo tip20_channel_reserve.rs:229). `why` names the
    /// case (rule 19), first so the census groups by it; `callee` is the awaited call's head
    /// when there is one.
    AwaitValueUnrelated { why: AwaitUnrelated, callee: Option<ItemKey> },
    /// The table given to `interior` does not hold the record's types at their indices.
    InteriorTable,
    Union,
    StaticMut,
    DestructuringAssign,
    /// An or-pattern that binds, in a form not lowered (pattern_forms.rs, "Or-patterns"): in a
    /// guarded arm whose guard sees a moving binding (the guard would run once per
    /// alternative), or a binding whose type is not its name's first binding's.
    OrPatternBinding,
    /// A slice pattern's binding not lowered (pattern_forms.rs, "Slice patterns"). Each
    /// element is a constant projection now; kept for a form the walk does not place.
    SlicePatternBinding,
    /// A peeling pattern under an ADT's field binds, and the field's declared type is not
    /// known well enough to say what it peels: [`Facts::field_lead`] gave `None` or
    /// [`FieldLead::Unread`] and the field is no bare type parameter ([`Facts::field_param`]).
    /// The record holds the subpattern's type after its peels only (typeck's
    /// `pat_adjustments`, omitted there), so the lowering rebuilds the field's type from these.
    PeelUnderAdtField,
    /// The binding's recorded type is not the place's type or a reference to it.
    BindingMode,
    /// [`Facts::field_regions`] gave a row for a field of `adt` that does not line up with
    /// the record: a lifetime or argument index past the ADT's, or a count of positions other
    /// than the field type's. A malformed fact, never related another way.
    FieldRegionsUnaligned { adt: ItemKey },
    /// [`Facts::copy_args`] gave a row for `adt` whose length is not its argument count in the
    /// record.
    CopyArgsUnaligned { adt: ItemKey },
    /// A guard over a by-value binding of a non-`Copy` type that the lowering could not bind by
    /// reference for the guard. Such guards are lowered now (pattern_forms.rs, "Match
    /// guards": shared references for the guard, the moves after it); kept for a form that
    /// cannot be split so.
    GuardMovesBinding,
    /// A call of `callee`, whose signature writes lifetimes, that no row of [`Facts`] let the
    /// lowering relate: `lacks` names the row that was missing, or that the rows given did not
    /// line up with the call (rule 19: the refusal names the callee and what it needs).
    CallWritesLifetimes { callee: ItemKey, lacks: CallLacks },
    /// A call of a KNOWN head `callee` that writes no lifetimes, where something carries a
    /// lifetime, that neither its signature nor its generic signature related (`lacks`: the
    /// row missing, or where the instantiation did not line up). It goes through
    /// `unify_call`'s one origin, a stricter relation that can carry a `&mut self` receiver's
    /// loan into the receiver's own type's regions: `c.step(); c.step();` with `C<'x>` made the
    /// first loan live as long as `c` (false E0499 at the second call, and "dropped while
    /// borrowed" at `c`'s drop: tempo's `for x in it` loops, `cursor.seek(..)` pairs,
    /// hub_call_tests.rs). Made only as [`Lowered::coarse`]: the body is refused by it only if
    /// it has a finding. The fix for each `lacks` is the row or the alignment.
    CallGenericUnaligned { callee: ItemKey, lacks: CallLacks },
    /// The callee's signature is not known (no head, or a lifetime part that does not line
    /// up) and an argument or the result carries a lifetime: one origin for all cannot say
    /// "this argument must be `'static`", so the body is not checked rather than accepted.
    CalleeUnknown,
    /// A call of `item`, or the body of `item` itself, whose head regions were not read
    /// ([`Facts::sig`] gave `Err`: `why` says no head holds them, the head was refused, or its
    /// record is of another form), with an argument or the result carrying a lifetime. The one
    /// refusal of a fn's regions (D148): never related under a guess, never a fresh origin in
    /// place of a declared region (rule 3).
    RegionsUnread { item: ItemKey, why: UnreadWhy },
    /// A call through a value (a fn pointer, a closure, a `dyn Fn`, an `F: Fn(..)`) whose own
    /// signature's lifetimes are not known, with an argument or result that carries one. Known
    /// are: a body parameter of fn-pointer type (`call_through_value`: its regions the body's
    /// own signature column's, D148), and a closure made in this body, whose result may hold
    /// anything the closure holds (callee_value.rs). Not known yet: a `dyn Fn` or `F: Fn(..)`
    /// parameter (its signature is a clause's column, pieces L4 and L5), a field holding a
    /// closure (`(self.f)(x)`).
    CalleeValueUnknown,
    /// A closure's captured place that `capture_places` cannot lower yet (a nested closure
    /// capturing a place of a place-captured variable, an enum variant field, or partial cover).
    CapturePlace,
    /// The callee's head names its generic parameter `param` (`PNode::Param`, parents first),
    /// but the call's argument list from the type checker has only `args`: a malformed record,
    /// never related another way (generic_call.rs `call_by_generic_signature`).
    CalleeParamPastArgs { callee: ItemKey, param: u32, args: u32 },
    /// A call of `callee` whose type argument for its parameter `param` is under a
    /// type-outlives bound this does not relate: the head declares `T: 'a` over an argument
    /// whose outlives components are unavailable (an opaque type, a projection, or a
    /// closure or future without known captures), or the argument carries a lifetime
    /// under a bound the reader did not resolve (the container's header declares one). Not
    /// checked rather than accepted (generic_call.rs `call_by_generic_signature`). `why`: which
    /// check refused (rule 19).
    CallOutlivesUnread { callee: ItemKey, param: u32, why: OutlivesUnreadWhy },
    /// A call of `callee` whose signature's regions include a trait lifetime of a bound on its
    /// parameter `param` (a trait method's `Self: Visitor<'de>`, a `V: Visitor<'de>`), which
    /// is the region the call's argument for `param` answers that bound with, not one the call
    /// may choose; and `why` says that answer is not known here (trait_ties.rs). Not checked
    /// rather than accepted with a fresh region.
    CallTieUnplaced { callee: ItemKey, param: u32, why: TieMiss },
    /// The body's type parameter `param` is required to outlive a lifetime its read bounds do
    /// not prove, and some bound on it was not read (a trait bound, its container's header, a
    /// type in its signature whose implied bounds are not read): not checked rather than
    /// reported (param_outlives.rs). Made by the borrow check's caller from
    /// [`Lowered::params`], not by the lowering.
    ParamOutlivesUnread { param: u32 },
    /// The signature's lifetime part does not have one region per origin position of the
    /// body's parameters and return type.
    SigUnaligned,
    /// The signature's region slice and the body's position count disagree. Both counts are
    /// retained so the refusal names the malformed record rather than indexing past the slice;
    /// `site` names which check refused (rule 19: nine checks shared this form, and tempo's
    /// `{ positions: 1, regions: 1 }` named none of them).
    SigPositionsUnaligned { positions: usize, regions: usize, site: SigSite },
    /// A fn's signature column is not laid over a fn pointer (`FnPtr(inputs.., output)`, D148):
    /// its root type in the columns' table ([`Facts::region_tys`]) is another node.
    SigColumnRoot,
    /// A call's result position `slot` holds a region of the callee's column that an input
    /// passed as a place also holds, but no instance of it was made by that input
    /// (`call_laid`'s invariant): the result would be related to no argument.
    CallOutputUntied { slot: u32 },
    /// A declared type of a head record's column is not in the columns' table
    /// ([`Facts::region_tys`]): read in the wrong table, never taken as another type.
    SigColumnType,
    /// The body of an `async fn` whose written return type's regions were not read
    /// (`BodyOut::AsyncUnread`): its value cannot be checked against them, so it is not checked.
    AsyncOutputUnread,
    /// A capture of an `async fn`'s root block that is not one of the fn's parameters by value
    /// (coroutine.rs `async_fn_root`): its regions are not the signature's to lay.
    AsyncFnCapture,
    /// A pre-2024 return-position opaque future has captures selected by its bounds (and any
    /// `use<..>` precise-capture clause), but the signature record does not carry that set.
    RpitCapturesUnread,
    ///The output has elided lifetimes but no single input lifetime.
    ElisionAmbiguous,
    /// The body's own signature writes lifetimes the record cannot read.
    LetOutsideCondition,
    Missing,
    UnknownAdt,
    UnknownLocal,
    UnknownField,
    UnknownVariant,
    BreakTarget,
    /// A type node the lowering does not place (a closure's, a future's).
    TypeNode,
    /// The record does not line up with the body: a table entry missing, positions that do not
    /// add up. Never a form of the program. (A closure capturing a by-reference binding once
    /// counted its capture by the pattern's peeled type, one position short: `npos` counts the
    /// variable's type, `Checked::local_ty`, as `closure_value` does.)
    Malformed,
    Bound,
    Builder,
    /// Two locals saved across a suspension with one identity (identity.rs): the builder could
    /// not tell which one a slot holds. The fix is a role of its own for one, in the lowering.
    IdentityTwice,
    /// A `super let` form not placed: with an `else` block, or in a block no temporary scope
    /// holds (super_let.rs).
    SuperLet,
    /// A closure whose signature a clause expected (`SigFrom::Clause`) and whose clause column
    /// is not in the head's regions: the use names no item, or the item has no clause `nth`
    /// (closure.rs `sig_slots`). A malformed record, never guessed.
    ClosureClauseUnread { nth: u32 },
    /// The expecting clause binds a region (`for<..>`, `Fn(&T)`'s elided input) and a type
    /// parameter of it holds positions of the closure's signature, so which position the
    /// binder binds is not known from the column alone (closure.rs `sig_slots`).
    /// `why`: which step of the walk refused (rule 19; closure.rs `lay_sig_typed`).
    ClosureSigUnaligned { part: u32, why: ClosureSigWhy },
    /// A direct call of a closure value whose argument `arg` (`args.len()` itself: the call's
    /// argument count differs from the signature's; past the last: the result) does not lay
    /// one exact position per signature position it binds (closure.rs `closure_call_sig`).
    ClosureCallUnaligned { arg: u32 },
    /// A closure whose body needs a relation on a signature position its owner binds, passed
    /// to `callee`, a clause of which binds a region (`for<..>`, `Fn(&T)`): the callee may call
    /// it with a region no owner region stands for (closure.rs `tie_closure_sigs`).
    ClosureSigHigherRanked { callee: ItemKey },
    /// The same closure made into another type (a coercion to a trait object or a fn pointer,
    /// an opaque return type), whose calls may bind a region per call (closure.rs
    /// `closure_sig_kept`).
    ClosureSigErased,
    /// The one call rule (one_call.rs, D156) did not relate the call of `callee`: why, by name.
    /// Made only under `one_call::OneRule`, never in production until the flip.
    OneRule { callee: ItemKey, why: one_call::OneRuleWhy },
    /// A projection `<S as trait_>::Name` of the body's own signature that the body's type
    /// has normalized, and that is not `Selected::Generic`, could not be laid through the impl
    /// selected for it ([`Lower::lay_through_impl`]): `why` names the step. Its positions are
    /// then placeholders, a stricter reading that reads regions the impl ties as unrelated (a
    /// false "a lifetime must outlive another": `get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8`):
    /// made only as [`Lowered::coarse`], the body refused by it only if it has a finding.
    ProjectionUnbound { trait_: ItemKey, why: ProjectionUnbound },
    /// A relation STRICTER than Rust's made outside a call's signature: three or more positions
    /// (or two of one value) made equal, or each made to outlive the others, where Rust
    /// relates them one way or through one of them only. `why` says which relation. Made only
    /// as [`Lowered::coarse`]: the body is refused by it only if it has a finding.
    Unified { why: Unified },
}

/// Which relation of the lowering made positions equal that Rust's rule does not
/// ([`Form::Unified`]). Each is a superset of the exact relation, recorded so that a finding it
/// may have made is never reported (the coarse-relations rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unified {
    /// `dst = src` where either place is inexact (its positions its base's) or the counts
    /// differ (a coercion that changes the type's shape): lib.rs `relate`'s `unify`.
    ValueInexact,
    /// `&p` where `p` is inexact or its count is not the reference's referent's: lib.rs
    /// `borrow`'s hub.
    BorrowInexact,
    /// A value into some positions of an aggregate (a field, an element, a constructor's
    /// argument) where the value is inexact or its count is not the slot's: lib.rs `relate_at`.
    SlotInexact,
    /// A non-comparison binary operator: its operands and result made equal (lib.rs
    /// `binary`): `impl<'a, 'b> Add<&'b U> for &'a U` relates `'a` and `'b` to nothing.
    BinaryOperator,
    /// An overloaded deref's or index's `Target`/`Output` positions, each related to every
    /// position of the receiver's referent and the index operand (lib.rs `target_flow`): the
    /// true region is ONE of them (E0207).
    DerefTarget,
    /// An async block's future awaited where it is made: its value made equal to every
    /// capture (coroutine.rs `await_unrelated`).
    AsyncBlockOutput,
    /// A call of a closure value: every position of the value made equal to every position of
    /// the result (callee_value.rs).
    ClosureValueCall,
    /// A call of a closure value: two or more argument or result positions tied both ways to
    /// one binder variable of its signature (closure.rs `closure_call_sig`).
    ClosureCallSig,
    /// A closure passed to a generic call: its owner signature positions tied both ways to two
    /// or more positions of the call (closure.rs `tie_closure_sigs`).
    ClosureSigTies,
    /// An ADT whose declared variances are not known ([`Facts::adt_variances`] gave `None`, or
    /// a list shorter than its parameters): each such parameter taken as invariant where the
    /// path to it is not invariant already (variance.rs `position_variances`).
    VarianceUnknown { adt: ItemKey },
    /// An inexact place (its positions its base's) related at a call by variance: every
    /// position taken invariant (variance.rs `variances_for`).
    VarianceInexact,
    /// A struct or variant aggregate whose fields' declared regions are not known: its
    /// operands and its value made equal through one hub (lib.rs `aggregate`).
    AggregateHub,
}

/// Which step of laying a declared projection through its selected impl failed
/// ([`Form::ProjectionUnbound`]; lib.rs `lay_over_in`, `sig_impl`, `lay_through_impl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProjectionUnbound {
    /// No row at the body's root (`Checked::selected_at(root)`) names this trait and name.
    NoRow,
    /// A row says `Selected::NotImpl`: a builtin rule or a where-clause, no impl text.
    RowNotImpl,
    /// A row says `Selected::Unanswered`: the selection needed a fact not read.
    RowUnanswered,
    /// The rows onto this trait and name name two impls, or an impl and `Generic`: which one
    /// is this projection's is not told (the rows are per name, not per projection).
    RowsDisagree,
    /// The column's slots before the projection, or its own, were not counted (`why`: which
    /// count, rule 19), so its own slots are not found.
    SlotsUncounted { why: Uncounted },
    /// The projection is inside an impl's `type` value being laid: the signature's rows do
    /// not answer it.
    InsideImplValue,
    /// The impl's `impl_header` (its self type's column) was not read.
    ImplHeaderUnread,
    /// The impl's `type Name = ..;` column (`impl_assoc`) was not read.
    ImplAssocUnread,
    /// The impl's lifetime parameters were not read.
    ImplParamsUnread,
    /// The alias's own slots were not counted, or it has its own (a GAT's lifetimes, not
    /// numbered in the impl's value here).
    GatLifetimes,
    /// The alias node's children are not a list with a self type first, or its count is not
    /// known.
    SelfTypeUnread,
    /// The impl's self type column and the alias's self type slots differ in count
    /// (`impl<T> Tr for Vec<T>` with `Vec<&'a u8>`: `T`'s regions are not the impl's).
    SelfSlotsDiffer { imp: u32, declared: u32 },
    /// The impl's self type names a lifetime past its parameters, or a bound (`Late`) one.
    SelfRegionUnnumbered,
    /// One impl lifetime is bound to two different regions of the alias's self type.
    BoundTwice,
    /// The impl's value is not of the body's normalized type's shape.
    ValueUnaligned,
}

/// Which count `lay_over_in` could not make on the way to a declared projection's slots
/// ([`ProjectionUnbound::SlotsUncounted`]): blaze-borrow `Columns`' `Unknown`, by name, or the
/// input's column shorter than the counted range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Uncounted {
    /// `Facts::adt(adt)` answered nothing: the body's facts hold no row for an ADT its own
    /// signature names (one the type check normalized away, as `Wrap` in
    /// `<Wrap<'a> as Tr>::A`, is a candidate: UNSURE which one tempo misses).
    AdtLifetimes(ItemKey),
    OpaqueCaptures(ItemKey),
    /// `Facts::alias_lifetimes(trait_, name)` answered nothing.
    AliasLifetimes { trait_: ItemKey, name: Name },
    TraitLifetimes(ItemKey),
    /// A body's type (closure, future, coroutine) in a declared column.
    BodyType,
    Hole,
    /// The region table could not be read at a node.
    Table,
    Bound,
    Overflow,
    /// The counted range `lo..hi` is past the input's column of `len` slots.
    ColumnShort { lo: u32, hi: u32, len: u32 },
}

impl Uncounted {
    fn of(u: blaze_borrow::Unknown) -> Uncounted {
        use blaze_borrow::Why;
        use blaze_typeck::regions::Unknown as Fact;
        match u.why {
            Why::Fact(Fact::AdtLifetimes(k)) => Uncounted::AdtLifetimes(k),
            Why::Fact(Fact::OpaqueCaptures(k)) => Uncounted::OpaqueCaptures(k),
            Why::Fact(Fact::AliasLifetimes { trait_, name }) => Uncounted::AliasLifetimes { trait_, name },
            Why::Fact(Fact::TraitLifetimes(k)) => Uncounted::TraitLifetimes(k),
            Why::BodyType => Uncounted::BodyType,
            Why::Hole => Uncounted::Hole,
            Why::Table => Uncounted::Table,
            Why::Bound => Uncounted::Bound,
            Why::Overflow => Uncounted::Overflow,
        }
    }
}

/// What [`Lower::lay_through_impl`] did with a declared projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Through {
    /// Laid through its selected impl: its positions pushed.
    Laid,
    /// Its normalization's positions are placeholders: exact for `Selected::Generic` (`<T as
    /// Tr>::A`, `T` the body's parameter: no impl is the projection's), stricter and noted
    /// ([`Lower::unbound`]) where it could not be laid. Nothing pushed.
    Placeholders,
}

/// Which fact a call of a head that writes lifetimes lacked ([`Form::CallWritesLifetimes`]):
/// relating it takes the head's lifetime part, its generic shape and its declared bounds. A
/// head whose lifetime part was not read never gets here: it is [`Form::RegionsUnread`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallLacks {
    /// No generic shape for the callee ([`Facts::shape`] gave `None`).
    Shape,
    /// No declared type-outlives bounds for the callee ([`Facts::call_bounds`] gave `None`).
    Bounds,
    /// The call was related, but a normalized projection in its output was tied both ways to
    /// two or more positions of its joined arguments (generic_call.rs `Slot::Joined`). That is
    /// a superset of the one true input, and it makes those inputs equal. Made only as
    /// [`Lowered::coarse`] (`Form::CallGenericUnaligned`).
    JoinedSuperset,
    /// Every row was given, and they did not line up with the call: the [`Misfit`] says
    /// where (another count of inputs than the call has operands, a part's positions that do
    /// not match its place, a hidden region, a projection in an input).
    Unaligned(Misfit),
}

/// Why an `.await`'s value was not related through a written return type
/// ([`Form::AwaitValueUnrelated`]). Each case is one exit of await_call.rs `awaited_shadow` or
/// `relate_awaited`, or of coroutine.rs `await_done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AwaitUnrelated {
    /// The operand is not a call lowered as a call of a head: a local, a field, a method
    /// chain's value held in a place, or a future of another body.
    NotACall,
    /// The operand is a call with no head (a callee value): no written return type to read.
    NoHead,
    /// The operand's `IntoFuture` is not the identity: the future is an impl's
    /// `into_future(self)` value, and its `Output` is that impl's to say.
    NotIdentity,
    /// The head's regions were not read ([`Facts::sig`] gave `Err`).
    HeadUnread,
    /// The head has no written return type column (`SigRegions::written_output`, an `async
    /// fn`'s) and its output is no opaque type (await_call.rs `opaque_output`): a future struct
    /// (`tokio::time::Timeout<F>`), a boxed `dyn Future`, a type parameter.
    NoWrittenReturn,
    /// The head's output is opaque `opaque` (`fn f(..) -> impl Future<Output = T>`, in a trait
    /// too) whose `Output = T` was not read: its clauses unread, or not exactly one `Output`
    /// binding with the opaque as `Self` among them.
    OpaqueOutputUnread { opaque: ItemKey },
    /// The `Output = T` clause of opaque `opaque` is not in the signature's numbering: its
    /// `Self` child's slots are not the signature's slots of the opaque, or it binds regions of
    /// its own (`for<'x>`).
    OpaqueOutputUnaligned { opaque: ItemKey },
    /// No declared bounds row for the head ([`Facts::call_bounds`] gave `None`), and a type
    /// argument carries a lifetime.
    NoBounds,
    /// The call's type argument for `param` carries a lifetime and is under a bound the reader
    /// did not resolve ([`CallBounds::unread_below`]).
    BoundUnread { param: u32 },
    /// The written return type's shape over the head's inputs was not read
    /// (generic_call.rs `awaited_shape`).
    ShapeUnread,
    /// The head's lifetime part did not line up with the call, with no type argument carrying
    /// a lifetime (`call_by_signature_to` related nothing).
    SignatureUnaligned,
    /// The generic signature did not line up with the call; the [`Misfit`] says where.
    GenericUnaligned(Misfit),
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// Which row a call of `item`, whose head writes lifetimes, lacked to be related by its
    /// signature (its [`Facts::sig`] read): the first of [`Facts::shape`],
    /// [`Facts::call_bounds`] that is `None`, else [`CallLacks::Unaligned`] with why the last
    /// instantiation tried at this call did not line up (`misfit`, reset at each try).
    fn call_lacks(&self, item: ItemKey) -> CallLacks {
        if self.facts.shape(item).is_none() {
            CallLacks::Shape
        } else if self.facts.call_bounds(item).is_none() {
            CallLacks::Bounds
        } else {
            CallLacks::Unaligned(self.misfit)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub form: Form,
    pub range: Range,
}

/// A callee's signature laid as the flow's positions (`Lower::call_laid`): one region per
/// position, each input's then the output's; input `i`'s end at `ends[i]`; the declared clauses
/// `(longer, shorter)`. Made from a head's column (`Lower::call_by_signature`) or from a
/// fn-pointer parameter's slots in the body's own column (`Lower::call_through_value`).
#[derive(Clone, Copy, Debug)]
struct Laid<'r> {
    regions: &'r [Region],
    ends: &'r [u32],
    bounds: &'r [(Region, Region)],
}

/// Counts the census reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Calls related by elision.
    pub calls_elided: u32,
    /// Calls whose instantiation holds an origin, or whose callee is a value: every origin of
    /// their operands and destination made equal (a superset of any signature's relations).
    pub calls_unified: u32,
    /// Calls related by instantiating the callee's lifetime part ([`Facts::sig`]).
    pub calls_by_signature: u32,
    /// Calls of a known head whose lifetime part holds a hidden position (`Lower::sig_laid`
    /// false: an opaque type, or a projection over the head's own type parameter, `<T as
    /// Tr>::X`, whose impl is chosen per call), so not instantiated from the column: related
    /// per call (hidden_call.rs), or by elision or one origin, a superset for the output.
    pub calls_hidden: u32,
    /// Calls of a head with a hidden input whose impl the call fixes (no origin in its type
    /// arguments): each hidden input's argument positions demanded `'static` (E0207;
    /// hidden_call.rs).
    pub calls_hidden_static: u32,
    /// The body's own placeholders and known relations came from its lifetime part.
    pub sig_read: bool,
    /// The body's signature writes lifetimes: its placeholders are taken as all related.
    pub sig_writes_lifetimes: bool,
}

pub struct Lowered<A: Allocator + Copy> {
    pub flow: Flow<A>,
    pub stats: Stats,
    /// The placeholders standing for the body's type parameters (param_outlives.rs): an
    /// `UnprovenOutlives` from one is E0310 when `certain`, else `Form::ParamOutlivesUnread`.
    pub params: Vec<ParamOrigin, A>,
    /// STRICT RELATIONS REFUSE ONLY ON A FINDING. `Some`: the first place this flow related by
    /// a STRICTER relation than Rust's, because the precise one could not be made, named as
    /// the refusal it would be (`CallGenericUnaligned`, `AwaitValueUnrelated`,
    /// `ProjectionUnbound`): `unify_call`'s one origin, an awaited value made equal to its
    /// future, a projection's normalization as placeholders. Each only ADDS constraints (or
    /// knows less of a placeholder), so a body CLEAN under it is clean under Rust's rules and
    /// stands checked; a finding may be the relation's artifact, so a body with ANY finding
    /// and a `Some` here (in any of its flows) is refused by this name, its findings dropped
    /// (the checker's caller does that: blaze-ladder borrows.rs, region_fixture.rs `lower`).
    /// Per body, not per finding: coarser, sound, and no clean body is lost.
    pub coarse: Option<Refusal>,
}

/// A planted bug for the fixtures' controls; `None` outside them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// Every place is read by copy: no move is ever lowered.
    AllCopy,
    /// An assignment relates no origins: a loan stays in the temporary it was made into.
    NoRelate,
    /// A call of unknown signature makes a reference operand's own lifetime equal to every
    /// other position (the form before the first position flowed one way).
    HubBothWays,
    /// A tuple is built with every position of its operands made equal (the form before each
    /// element was related to its own field).
    TupleUnified,
    /// The signature's implied and declared bounds are not given to the flow as known.
    NoKnown,
    /// A callee's `'static` is instantiated afresh at each call like any other region: a fn
    /// pointer's `fn(&'static T)` then takes a local's reference.
    StaticIgnored,
    /// A call of unknown head is related through one origin instead of refused (the form
    /// before `Form::CalleeUnknown`): census 1's "invalidated in a derived `fmt`" comes back.
    UnknownUnified,
    /// A closure's captures are not lowered: the closure value holds no loan and moves nothing.
    NoCaptures,
    /// A call of a head with a hidden input takes the coarse path as before sig-assoc-per-call
    /// (hidden_call.rs): `take::<Foo>(&local)` with `type Out = &'static S` is accepted.
    HiddenCoarse,
    /// A local's storage ends only where a drop runs (`needs_drop`): a parameter of a type with
    /// no drop glue is never ended, and `fn f(s: S) { keep(&s) }` with `keep(&'static S)` is
    /// accepted (the form before storage-end-loans).
    StorageEndTiedToDrop,
    /// A callee's trait lifetimes are instantiated afresh at each call, as every other region
    /// (the form before piece RB8, trait_ties.rs): `t.conv(x)` through `T: Conv<'b, 'a>` with
    /// `fn conv(self, x: &'a u8) -> &'b u8` is accepted in `fn g<'a, 'b, ..>(..) -> &'b u8`.
    TiesFresh,
    /// A closure's requirements ([`Facts::closure_requirements`]) are not emitted where it is
    /// made: `|| put(v, r)` needing `'b: 'a` in `fn f<'a, 'b>(v: &mut &'a u8, r: &'b u8)` is
    /// accepted (closure.rs `requirement`).
    NoRequirements,
    /// A call of a closure of this body relates nothing into its result (callee_value.rs):
    /// `let r = c();` with `c = || &s` holds no loan of `s`.
    ClosureCallUnrelated,
    /// A guarded arm's by-value bindings are moved BEFORE its guard, as if it had none
    /// (pattern_forms.rs): `Some(x) if take(x)` moves `x` in the guard unseen.
    GuardBindsByValue,
    /// A slice pattern's elements are projected as any index (`Proj::Index`), each overlapping
    /// every other (pattern_forms.rs): `let [x, y] = a;` with `a: &mut [S; 2]` then holds two
    /// conflicting mutable loans.
    SliceElementsOverlap,
    /// Every position below the top of a value related both ways, as before piece B13
    /// (variance.rs): the top reference one way, all else invariant, ADTs' variances unasked.
    NoVariance,
    /// A place evaluated and discarded is lowered as nothing, as before the flow had
    /// `StmtKind::Mention` (`let _ = place;`, `_ = place;`) and an expression statement's place
    /// was read (`place;`): `let r; { let a = 2; r = &a; } let _ = *r;` is accepted.
    MentionNothing,
    /// A constant argument at a hidden input expands to no position, as in the wave's L3 head
    /// (hidden_call.rs): `out.line(cx, "Hello")` at `what: impl Show` is refused
    /// (`HiddenWhy::OperandUnaligned`).
    HiddenConstantZero,
    /// A type holding two opaque or projection subtrees is not laid even when no slot lies
    /// under them, as in the wave's L3 head (lib.rs `lay_positions`): every `?`'s
    /// `Try::branch(e)` (`-> ControlFlow<Self::Residual, Self::Output>`) is refused
    /// (`CallHiddenInput { input: u32::MAX, why: NotLaid }`), the regression a real hello world
    /// met.
    HiddenSubtreesApart,
    /// A const argument in a declared type (`Fixed<32>`) is a node `lay_positions` does not
    /// know, as from D148 until tempo 1120: every head naming one is not laid (a call
    /// `CallHiddenInput { why: NotLaid }`, a body's own signature `SigPositionsUnaligned`).
    ConstArgUnlaid,
    /// A block's, `if`'s, `match`'s or `loop`'s result temporary is dropped where `dest_for`
    /// made it, below the temporaries its evaluation left (the form before `settle_dest`):
    /// `look_w(&{ vs(&mk()) })` before 2024 is a false E0597 at `mk()`'s storage end.
    DestDroppedLast,
    /// An `if`'s or `match`'s arm is no temporary scope: its temporaries live to the enclosing
    /// statement's end (the form before `close_arm_temps`): `look(match 0 { _ => make(&mk())
    /// })` is accepted.
    ArmTempsToStatement,
}

/// Lowers one checked body.
pub fn lower<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_planted(body, ck, facts, Plant::None, alloc)
}

#[doc(hidden)]
pub fn lower_planted<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    plant: Plant,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_with::<A, F, one_call::Production>(body, ck, facts, plant, false, alloc).0
}

/// [`lower_planted`] under the call rule `R`. With `logged` (the oracle's comparing lowering,
/// oracle.rs), the second answer is every call's record (builder.rs [`builder::Log`]: its
/// endpoints and the subsets it emitted), and a refusal of the call rule is per call: the walk
/// goes on past it so the body's later calls are compared too, and the first refusal is the
/// answer (`Lower::finish_held`), the same answer as a walk that stops there.
pub(crate) fn lower_with<A: Allocator + Copy, F: Facts, R: one_call::CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    plant: Plant,
    logged: bool,
    alloc: A,
) -> (Result<Lowered<A>, Refusal>, Option<builder::Log<A>>) {
    if body.coroutine.is_some() {
        return (Err(Refusal { form: Form::Coroutine, range: range_of(body, body.root) }), None);
    }
    let mut l: blaze_own::Box<Lower<'_, '_, F, A, R>, A> = Lower::new(body, ck, facts, alloc);
    l.plant = plant;
    if logged {
        l.b.start_log(alloc);
        l.per_call = true;
    }
    let ran = l.run();
    let ran = l.finish_held(ran);
    let log = l.b.take_log();
    if let Err(r) = ran {
        return (Err(r), log);
    }
    let stats = l.stats;
    let b = core::mem::replace(&mut l.b, builder::Builder::new(FlowBuilder::new_in(1, 0, alloc)));
    let flow = b.finish().map_err(|_| Refusal { form: Form::Builder, range: range_of(body, body.root) });
    let coarse = l.coarse.get();
    let params = core::mem::replace(&mut l.param_origs, Vec::new_in(alloc));
    (flow.map(|flow| Lowered { flow, stats, params, coarse }), log)
}

fn range_of(body: &BodyView<'_>, e: ExprId) -> Range {
    body.expr_ranges.get(e.ix()).copied().unwrap_or(Range { start: 0, end: 0 })
}

/// A type as the lowering holds it: a checked type, or a reference to one (a temporary the
/// lowering makes for an overloaded deref or index, whose type the record does not name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TyD {
    wrap: Option<Mutbl>,
    ty: PTy,
}

impl TyD {
    fn of(ty: PTy) -> TyD {
        TyD { wrap: None, ty }
    }
}

#[derive(Clone, Copy, Debug)]
enum At {
    Place(Place),
    Const,
}

#[derive(Clone, Copy, Debug)]
struct Val {
    at: At,
    ty: TyD,
}

/// A place's origins: `len` of the local's origins from `start` (an index into `origs`); exact
/// when they are the place's own type's positions, else the whole of an ADT's. An ADT field's
/// exact origins are a stretch of `origs` laid for it (`Lower::field_slot`): copies of the
/// value's origins in the field type's order, not a slice of the value's own stretch.
#[derive(Clone, Copy, Debug)]
struct PInfo {
    start: u32,
    len: u32,
    exact: bool,
    /// 0: not a deref; 1: deref of a shared reference; 2: of a mutable one; 3: of a raw pointer.
    deref: u8,
    parent: u32,
    local: u32,
}

/// Which aggregate `Lower::aggregate` builds: a constructor call (`Some(x)`, `S(a, b)`), a
/// literal (a tuple, a struct, an array), or `format_args!`'s arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agg {
    Ctor,
    Other,
    Fmt,
}

#[derive(Clone, Copy, Debug)]
enum Task {
    Eval(ExprId, bool),
    Adjust(ExprId, bool),
    Stmt(StmtId),
    OpenTemps,
    CloseTemps(ExprId),
    /// After an `if`'s non-`let` condition has branched: the temporaries above the top mark
    /// (the condition's) move from `temps` to `held`, the mark popped.
    HoldTemps(ExprId),
    /// On entry to an arm of that `if`: the held run dropped; `last` (the else side) ends it.
    DropHeld(ExprId, bool),
    /// A 2024 `if let`'s then-arm end: the value into `dest`, the temporaries above the top
    /// mark (the scrutinee's) dropped without popping it (the else side's `CloseTemps` ends
    /// it), then to `join`.
    ArmEndTemps(ExprId, Option<Local>, Block),
    OpenScope,
    CloseScope(ExprId),
    Start(Block),
    Goto(Block, ExprId),
    Unreachable(Block, ExprId),
    PushVal(Val),
    /// An `if`'s or `match`'s value complete in its result temporary: its drop moves after the
    /// temporaries its evaluation left ([`Lower::settle_dest`]).
    SettleDest(Local, ExprId),
    Discard,
    /// The value of place expression `e` discarded unread: `_ = e` ([`Lower::mention`]).
    Mention(ExprId),
    /// An expression statement's value discarded: a place expression's is read
    /// ([`Lower::discard_stmt`]).
    DiscardStmt(ExprId),
    Branch(ExprId, Block, Block),
    BranchOn(ExprId, Block, Block),
    LetBranch(PatId, ExprId, Block, Block),
    TailStore(Option<Local>, ExprId),
    BlockEnd(ExprId, Option<Local>, bool),
    ArmEnd(ExprId, Option<Local>, Block, bool),
    AssignConst(Local, ExprId),
    MatchDispatch(ExprId, Option<Local>),
    FakeBorrow(ExprId, Val),
    FakeUse(Local, ExprId),
    BindPat(PatId, Val),
    /// A guarded arm's bindings before its guard: by-value ones as shared references to their
    /// places (pattern_forms.rs, "Match guards").
    GuardRefs(PatId, Val, ExprId),
    /// The same arm's by-value bindings once its guard held: moved or copied then.
    GuardMoves(PatId, Val),
    LoopBack(ExprId),
    BreakDone(ExprId, bool),
    ReturnDone(ExprId, bool),
    RootEnd(ExprId),
    CallDone(ExprId, u32, bool, u32),
    CtorDone(ExprId, u32),
    AggDone(ExprId, u32),
    OpDone(ExprId),
    BinDone(ExprId, BinOp),
    AssignDone(ExprId),
    AssignOpDone(ExprId),
    RefDone(ExprId, Mutbl),
    RawDone(ExprId),
    DerefDone(ExprId, bool),
    FieldDone(ExprId),
    /// A struct update's base, its value on the stack: replaced by the fields it gives
    /// (`struct_base`).
    StructBase(ExprId),
    /// A base-less struct expression's defaulted fields (`S { a: 1, .. }`): one constant
    /// operand per field it takes from its defaults (`field_defaults`).
    FieldDefaults(ExprId),
    IndexDone(ExprId, bool),
    FmtDone(ExprId, u32),
    LetInit(StmtId),
    /// An `.await`, its operand's value on the stack (coroutine.rs `await_done`).
    AwaitDone(ExprId),
    /// An operand of a call, a tuple, an array or a struct expression just evaluated, another
    /// after it: its value read now ([`Lower::settle_operand`]).
    Settle(ExprId),
}

/// [`Lower::extended`]'s row of an expression whose temporary no `let` extends.
const NOT_EXTENDED: u32 = u32::MAX;

/// [`Lower::extended`]'s row of an expression whose temporary a constant initializer extends
/// to `'static` (rustc region.rs `resolve_body`: "temporaries and `super let` bindings in
/// constant initializers may be extended to have 'static lifetimes, using the same syntactical
/// rules used for `let` initializers"; `const X: ... = &f();` has "the result of `f()` live
/// for `'static`"): never dropped, its storage never ended.
const STATIC_EXTENDED: u32 = u32::MAX - 1;

/// What a live value of an opaque type keeps live, by the opaque's item bounds
/// ([`Lower::opaque_live`], rust-lang/rust#116733). Its TYPE still captures every region;
/// only liveness (and the outlives it is proven by: `'r: 'x` proves `Opaque: 'x`) follows this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpaqueLive {
    /// A `+ 'static` bound: no region.
    Nothing,
    /// Every `+ 'r` bound names this one region (the opaque's fn's numbering): only it.
    Only(Region),
    /// No bound, two different ones, or the head not read: every captured region.
    Captures,
}

/// Per-slot liveness mode for a local's origins ([`Lower::origin_live_mask`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OriginLive {
    /// Live on both value use and drop.
    Full,
    /// Live on value use, not on drop (RFC 1327 / dropck).
    NoDrop,
    /// Present in `self.origs` for subtyping/calls, but neither use nor drop keeps it live
    /// (opaque captured lifetimes excluded by the opaque's item outlives bound, rust-lang/rust#116733).
    None,
}

#[derive(Clone, Copy, Debug)]
struct Target {
    e: ExprId,
    head: Option<Block>,
    exit: Block,
    dest: Option<Local>,
    scopes: u32,
    temps: u32,
}

/// Inline work stack for type walks (`npos_positions`, `positions_all_opaque`, `is_copy`,
/// `needs_drop`): zero heap allocation up to depth 16, spilling to an explicit-allocator `Vec`
/// only past 16.
pub struct TyStack<A: Allocator> {
    inline: [PTy; 16],
    len: usize,
    spill: Vec<PTy, A>,
}

impl<A: Allocator> TyStack<A> {
    #[inline]
    pub fn new(root: PTy, alloc: A) -> Self {
        let mut inline = [PTy(0); 16];
        inline[0] = root;
        Self { inline, len: 1, spill: Vec::new_in(alloc) }
    }

    #[inline]
    pub fn push(&mut self, t: PTy) {
        if self.spill.is_empty() && self.len < self.inline.len() {
            self.inline[self.len] = t;
            self.len += 1;
        } else {
            self.spill.push(t);
        }
    }

    #[inline]
    pub fn extend_from_slice(&mut self, slice: &[PTy]) {
        if self.spill.is_empty() && self.len + slice.len() <= self.inline.len() {
            self.inline[self.len..self.len + slice.len()].copy_from_slice(slice);
            self.len += slice.len();
        } else {
            self.spill.extend_from_slice(slice);
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Option<PTy> {
        if let Some(t) = self.spill.pop() {
            return Some(t);
        }
        if self.len > 0 {
            self.len -= 1;
            return Some(self.inline[self.len]);
        }
        None
    }
}

/// Builds a dense O(1) first-match index table (`u32::MAX` sentinel) from a column of row ids.
pub fn index_rows<T: Copy, A: Allocator>(col: &[T], len: usize, ix: impl Fn(T) -> usize, alloc: A) -> Vec<u32, A> {
    if col.is_empty() {
        return Vec::new_in(alloc);
    }
    let mut table = Vec::from_elem_in(u32::MAX, len, alloc);
    for (i, &k) in col.iter().enumerate() {
        if let Some(slot) = table.get_mut(ix(k))
            && *slot == u32::MAX
        {
            *slot = i as u32;
        }
    }
    table
}

/// Why a type walk in [`pty_is_copy`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyWalkError {
    /// A type id, list id, or closure capture record was out of range.
    Malformed,
    /// An ADT's [`AdtFacts`] were missing from [`Facts::adt`].
    UnknownAdt,
    /// The type walk passed `MOST_POSITIONS`.
    Bound,
    /// [`Facts::copy_args`] returned a mask whose length did not match the ADT's type arguments.
    CopyArgsUnaligned { adt: ItemKey },
}

/// How [`pty_is_copy`] checks an ADT's generic arguments once [`AdtFacts::copy`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdtCopyArgs {
    /// Require every type argument of the ADT to be `Copy`.
    All,
    /// Consult [`Facts::copy_args`] to check only the arguments bounded by the ADT's `Copy` impl.
    Facts,
}

/// How [`pty_is_copy`] treats `PNode::Closure` nodes.
#[derive(Clone, Copy)]
pub enum ClosureCopy<'a, 'c> {
    /// Closures are not `Copy` (body lowering).
    Never,
    /// A closure of `owner` is `Copy` if it captures nothing by mutable reference and every
    /// by-value capture's type is `Copy`, using the precomputed CSR index over `ck.captures`
    /// (flow lowering).
    ByValueCaptures {
        owner: ItemKey,
        ck: &'a Checked<'c>,
        cap_offsets: &'a [u32],
        cap_rows: &'a [u32],
    },
}

/// Memo table, facts, and allocator for [`pty_is_copy`].
pub struct CopyWalk<'a, 'c, F: Facts, A: Allocator + Copy> {
    pub facts: &'a F,
    pub copy_memo: &'a [core::cell::Cell<u8>],
    pub adt_args: AdtCopyArgs,
    pub closures: ClosureCopy<'a, 'c>,
    pub alloc: A,
}

/// Whether a value of `root` is `Copy`, memoized in `walk.copy_memo`
/// (`0` = unknown, `1` = false, `2` = true).
pub fn pty_is_copy<'a, 'c, F: Facts, A: Allocator + Copy, E>(
    walk: CopyWalk<'a, 'c, F, A>,
    root: PTy,
    node: impl Fn(PTy) -> Option<PNode>,
    list: impl Fn(List) -> Option<&'a [PTy]>,
    on_err: impl Fn(CopyWalkError) -> E,
) -> Result<bool, E> {
    let root_ix = root.0 as usize;
    if let Some(cell) = walk.copy_memo.get(root_ix) {
        let v = cell.get();
        if v != 0 {
            return Ok(v == 2);
        }
    }
    let quick = match node(root).ok_or_else(|| on_err(CopyWalkError::Malformed))? {
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Never
        | PNode::Ptr(..)
        | PNode::FnDef(..)
        | PNode::FnPtr(..)
        | PNode::ConstValue { .. }
        | PNode::ConstParam(_)
        | PNode::Ref(Mutbl::Not, _) => Some(true),
        PNode::Ref(Mutbl::Mut, _) => Some(false),
        PNode::Param(i) => Some(walk.facts.param_copy(i)),
        PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) if l.len == 0 => Some(true),
        PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Array(..) | PNode::Pat(..) | PNode::Adt(..) => None,
        PNode::Closure { owner, .. } => match walk.closures {
            ClosureCopy::ByValueCaptures { owner: body_owner, .. } if owner == body_owner => None,
            _ => Some(false),
        },
        _ => Some(false),
    };
    if let Some(ans) = quick {
        if let Some(cell) = walk.copy_memo.get(root_ix) {
            cell.set(if ans { 2 } else { 1 });
        }
        return Ok(ans);
    }
    let set_false = || -> Result<bool, E> {
        if let Some(rc) = walk.copy_memo.get(root_ix) {
            rc.set(1);
        }
        Ok(false)
    };
    let mut stack = TyStack::new(root, walk.alloc);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_POSITIONS {
            return Err(on_err(CopyWalkError::Bound));
        }
        if let Some(cell) = walk.copy_memo.get(t.0 as usize) {
            let v = cell.get();
            if v == 1 {
                return set_false();
            }
            if v == 2 {
                continue;
            }
        }
        match node(t).ok_or_else(|| on_err(CopyWalkError::Malformed))? {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Never
            | PNode::Ptr(..)
            | PNode::FnDef(..)
            | PNode::FnPtr(..)
            | PNode::ConstValue { .. }
            | PNode::ConstParam(_)
            | PNode::Ref(Mutbl::Not, _) => {}
            PNode::Ref(Mutbl::Mut, _) => return set_false(),
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                let s = list(l).ok_or_else(|| on_err(CopyWalkError::Malformed))?;
                stack.extend_from_slice(s);
            }
            PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
            PNode::Adt(k, l) => {
                let f = walk.facts.adt(k).ok_or_else(|| on_err(CopyWalkError::UnknownAdt))?;
                if !f.copy {
                    return set_false();
                }
                // The type is `Copy` when the arguments its `Copy` impl bounds are (the
                // Reference, "Copy": an impl's bounds decide; `impl<T: ?Sized> Copy for
                // NonNull<T>` bounds none, so `NonNull<Header>` is `Copy` and
                // `(head.release)(job.0)` after `job.0.as_ref()` reads a copy, task/threads.rs
                // `run_lent`). Unread: every argument, the derive's rule.
                let args = list(l).ok_or_else(|| on_err(CopyWalkError::Malformed))?;
                match walk.adt_args {
                    AdtCopyArgs::All => stack.extend_from_slice(args),
                    AdtCopyArgs::Facts => match walk.facts.copy_args(k) {
                        None => stack.extend_from_slice(args),
                        Some(need) if need.len() == args.len() => {
                            for (&a, &req) in args.iter().zip(need.iter()) {
                                if req {
                                    stack.push(a);
                                }
                            }
                        }
                        Some(_) => return Err(on_err(CopyWalkError::CopyArgsUnaligned { adt: k })),
                    },
                }
            }
            PNode::Param(i) => {
                if !walk.facts.param_copy(i) {
                    return set_false();
                }
            }
            // A closure of this body (the Reference, "Closure types", "Other traits": a
            // closure is `Copy` when it captures nothing by mutable reference and every value
            // it captures by value is `Copy`). A shared capture is a `&T`, `Copy`. tempo
            // prover connection.rs:123 `let invalid = |..| ..; a.ok_or_else(invalid)?;
            // b.map_err(invalid)?`: the second use read a moved closure (E0382).
            PNode::Closure { owner, ordinal, .. } => match walk.closures {
                ClosureCopy::ByValueCaptures { owner: body_owner, ck, cap_offsets, cap_rows } if owner == body_owner => {
                    let Some(&ce) = ck.closures.column::<0>().get(ordinal as usize) else {
                        return Err(on_err(CopyWalkError::Malformed));
                    };
                    let _ = (cap_offsets, cap_rows);
                    for f in ck.env_fields_by::<blaze_typeck::Places>(ce) {
                        match f.how {
                            blaze_typeck::CaptureKind::ByRef => {}
                            blaze_typeck::CaptureKind::ByMutRef => return set_false(),
                            blaze_typeck::CaptureKind::ByValue => match f.place_ty(ck) {
                                Some(ct) => stack.push(ct),
                                None => return Err(on_err(CopyWalkError::Malformed)),
                            },
                        }
                    }
                }
                _ => return set_false(),
            },
            _ => return set_false(),
        }
    }
    if let Some(rc) = walk.copy_memo.get(root_ix) {
        rc.set(2);
    }
    Ok(true)
}

/// Why a type walk in [`pty_needs_drop`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropWalkError {
    /// A type id, list id, or closure capture record was out of range.
    Malformed,
    /// An ADT's [`AdtFacts`] were missing from [`Facts::adt`].
    UnknownAdt,
    /// The type walk passed `MOST_POSITIONS`.
    Bound,
}

/// How [`pty_needs_drop`] treats `PNode::Closure` nodes.
#[derive(Clone, Copy)]
pub enum ClosureDrop<'a, 'c> {
    /// Closures do not need drop (flow lowering).
    Ignore,
    /// A closure of `owner` needs drop if any of its by-value captures in `ck` does; a
    /// closure of another body conservatively needs drop (body lowering).
    ByValueCaptures { owner: ItemKey, ck: &'a Checked<'c> },
}

/// Memo tables, facts, and allocator for [`pty_needs_drop`].
pub struct DropWalk<'a, 'c, F: Facts, A: Allocator + Copy> {
    pub facts: &'a F,
    pub copy_memo: &'a [core::cell::Cell<u8>],
    pub drop_memo: &'a [core::cell::Cell<u8>],
    pub closures: ClosureDrop<'a, 'c>,
    pub alloc: A,
}

/// Whether dropping a value of `root` may run code, memoized in `walk.drop_memo`
/// (`0` = unknown, `1` = false, `2` = true).
pub fn pty_needs_drop<'a, 'c, F: Facts, A: Allocator + Copy, E>(
    walk: DropWalk<'a, 'c, F, A>,
    root: PTy,
    is_copy: impl FnOnce() -> Result<bool, E>,
    node: impl Fn(PTy) -> Option<PNode>,
    list: impl Fn(List) -> Option<&'a [PTy]>,
    on_err: impl Fn(DropWalkError) -> E,
) -> Result<bool, E> {
    let root_ix = root.0 as usize;
    if let Some(cell) = walk.drop_memo.get(root_ix) {
        let v = cell.get();
        if v != 0 {
            return Ok(v == 2);
        }
    }
    if is_copy()? {
        if let Some(cell) = walk.drop_memo.get(root_ix) {
            cell.set(1);
        }
        return Ok(false);
    }
    let quick = match node(root).ok_or_else(|| on_err(DropWalkError::Malformed))? {
        PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) if l.len == 0 => Some(false),
        PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Array(..) | PNode::Pat(..) | PNode::Adt(..) => None,
        PNode::Param(_)
        | PNode::Alias { .. }
        | PNode::Opaque(..)
        | PNode::Dynamic(_)
        | PNode::Future(..)
        | PNode::Coroutine { .. } => Some(true),
        PNode::Closure { owner, .. } => match walk.closures {
            ClosureDrop::Ignore => Some(false),
            ClosureDrop::ByValueCaptures { owner: body_owner, .. } => {
                if owner != body_owner {
                    Some(true)
                } else {
                    None
                }
            }
        },
        _ => Some(false),
    };
    if let Some(ans) = quick {
        if let Some(cell) = walk.drop_memo.get(root_ix) {
            cell.set(if ans { 2 } else { 1 });
        }
        return Ok(ans);
    }
    let mut stack = TyStack::new(root, walk.alloc);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_POSITIONS {
            return Err(on_err(DropWalkError::Bound));
        }
        let t_ix = t.0 as usize;
        if let Some(cell) = walk.drop_memo.get(t_ix) {
            let v = cell.get();
            if v == 2 {
                if let Some(rc) = walk.drop_memo.get(root_ix) {
                    rc.set(2);
                }
                return Ok(true);
            }
            if v == 1
                && walk.copy_memo.get(t_ix).is_some_and(|c| c.get() == 1)
            {
                continue;
            }
        }
        match node(t).ok_or_else(|| on_err(DropWalkError::Malformed))? {
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                let s = list(l).ok_or_else(|| on_err(DropWalkError::Malformed))?;
                stack.extend_from_slice(s);
            }
            PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
            PNode::Adt(k, l) => {
                let f = walk.facts.adt(k).ok_or_else(|| on_err(DropWalkError::UnknownAdt))?;
                if f.needs_drop {
                    if let Some(rc) = walk.drop_memo.get(root_ix) {
                        rc.set(2);
                    }
                    return Ok(true);
                }
                let s = list(l).ok_or_else(|| on_err(DropWalkError::Malformed))?;
                match walk.facts.drop_args(k) {
                    Some(need) if need.len() == s.len() => {
                        for (&a, &drops) in s.iter().zip(need.iter()) {
                            if drops {
                                stack.push(a);
                            }
                        }
                    }
                    _ => stack.extend_from_slice(s),
                }
            }
            // A future holds its captures and its saved values: dropping it may run code.
            PNode::Param(_)
            | PNode::Alias { .. }
            | PNode::Opaque(..)
            | PNode::Dynamic(_)
            | PNode::Future(..)
            | PNode::Coroutine { .. } => {
                if let Some(rc) = walk.drop_memo.get(root_ix) {
                    rc.set(2);
                }
                return Ok(true);
            }
            // A closure of this body holds its by-value captures; one of another body is
            // not known here, and its `Drop` row stays for elaboration to judge.
            PNode::Closure { owner, ordinal, .. } => {
                if let ClosureDrop::ByValueCaptures { owner: body_owner, ck } = walk.closures {
                    if owner != body_owner {
                        if let Some(rc) = walk.drop_memo.get(root_ix) {
                            rc.set(2);
                        }
                        return Ok(true);
                    }
                    let Some(&c) = ck.closures.column::<0>().get(ordinal as usize) else {
                        return Err(on_err(DropWalkError::Malformed));
                    };
                    // Its by-value environment fields (blaze-typeck env_fields.rs): a whole
                    // binding's type, or a captured place's (`x.a` by value holds `x.a` only).
                    for f in ck.env_fields_by::<blaze_typeck::Places>(c) {
                        if f.how == blaze_typeck::CaptureKind::ByValue {
                            match f.place_ty(ck) {
                                Some(ct) => stack.push(ct),
                                None => return Err(on_err(DropWalkError::Malformed)),
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(rc) = walk.drop_memo.get(root_ix) {
        rc.set(1);
    }
    Ok(false)
}

/// Builds a CSR index over `ck.captures.column::<0>()` keyed by `ExprId`, preserving exact row order.
fn build_capture_csr<A: Allocator + Copy>(col: &[ExprId], n_exprs: usize, alloc: A) -> (Vec<u32, A>, Vec<u32, A>) {
    if col.is_empty() || n_exprs == 0 {
        return (Vec::new_in(alloc), Vec::new_in(alloc));
    }
    let mut offsets = Vec::from_elem_in(0u32, n_exprs + 1, alloc);
    for &ce in col {
        let i = ce.ix();
        if i < n_exprs {
            offsets[i + 1] += 1;
        }
    }
    for i in 1..=n_exprs {
        offsets[i] += offsets[i - 1];
    }
    let total = offsets[n_exprs] as usize;
    let mut rows = Vec::from_elem_in(0u32, total, alloc);
    for (row, &ce) in col.iter().enumerate() {
        let i = ce.ix();
        if i < n_exprs {
            let pos = offsets[i] as usize;
            offsets[i] = (pos + 1) as u32;
            rows[pos] = row as u32;
        }
    }
    offsets.copy_within(0..n_exprs, 1);
    offsets[0] = 0;
    (offsets, rows)
}

/// One body's lowering. `R` is the call rule (rule 78, D156: one_call.rs); a lowering named
/// without it is production's ([`one_call::Production`]).
struct Lower<'b, 'c, F: Facts, A: Allocator + Copy, R: one_call::CallRule = one_call::Production> {
    body: &'b BodyView<'b>,
    ck: &'c Checked<'c>,
    tys: PTys<'c>,
    facts: &'b F,
    /// The flow being built (builder.rs: blaze-flow's builder, its subsets counted per call
    /// for the call-rule oracle when asked).
    b: builder::Builder<A>,
    rule: core::marker::PhantomData<R>,
    alloc: A,
    /// Dense O(1) first-match lookup tables into `ck` columns (`u32::MAX` = absent).
    expr_def_row: Vec<u32, A>,
    pat_def_row: Vec<u32, A>,
    callee_row: Vec<u32, A>,
    field_row: Vec<u32, A>,
    closure_ordinal: Vec<u32, A>,
    /// CSR index over `ck.captures` keyed by `ExprId`, preserving row order.
    cap_offsets: Vec<u32, A>,
    cap_rows: Vec<u32, A>,
    /// Per-`PTy` memo tables (0 = unknown, 1 = false, 2 = true; `u32::MAX` = unknown for `npos`).
    copy_memo: Vec<core::cell::Cell<u8>, A>,
    drop_memo: Vec<core::cell::Cell<u8>, A>,
    npos_memo: Vec<core::cell::Cell<u32>, A>,
    /// Per pattern: the flow local of a binding, `u32::MAX` before it is bound.
    pat_local: Vec<u32, A>,
    local_ty: Vec<TyD, A>,
    /// Per local: its origins' range in `origs`.
    local_origs: Vec<(u32, u32), A>,
    origs: Vec<Origin, A>,
    pinfo: Vec<PInfo, A>,
    vals: Vec<Val, A>,
    tasks: Vec<Task, A>,
    vars: Vec<Local, A>,
    scope_marks: Vec<u32, A>,
    /// Per open variable scope, beside `scope_marks`: the count of `temps` and of `temp_marks`
    /// when it opened. The temporaries above the first were made inside the scope (its current
    /// statement's and deeper), so an exit drops them before its variables; those under it are
    /// of a statement around the scope, dropped after (`exit_drops`).
    scope_temps: Vec<(u32, u32), A>,
    temps: Vec<Local, A>,
    temp_marks: Vec<u32, A>,
    /// The temporaries of `if` conditions that are not `let`s, taken out of `temps` once the
    /// condition has branched (`Task::HoldTemps`) and dropped on entry to each arm
    /// (`Task::DropHeld`); `held_marks` the start of each open `if`'s run. (Named apart from
    /// `held`, the refusal a walk keeps: two fields of one name did not compile.)
    cond_temps: Vec<Local, A>,
    held_marks: Vec<u32, A>,
    targets: Vec<Target, A>,
    two_phase: Vec<Loan, A>,
    /// Per expression: [`NOT_EXTENDED`], or, for the operand of an extending borrow of a `let`'s
    /// initializer (`mark_extended`), the count of drop scopes open when that `let` was reached
    /// (its block is the innermost of them): where the operand's temporary goes
    /// ([`Lower::extend_temp`]).
    extended: Vec<u32, A>,
    /// Per block met as an extending expression of a `let` (`mark_extended`): the block and the
    /// drop scopes open then; where its `super let` bindings go (super_let.rs).
    super_ext: Vec<(u32, u32), A>,
    is_target: Vec<bool, A>,
    /// The signature's regions and their placeholder origins (`'static` among them once met).
    uni: Vec<(Region, Origin), A>,
    /// The placeholders of the body's type parameters met so far (param_outlives.rs).
    param_origs: Vec<ParamOrigin, A>,
    /// `(param, reference placeholder)`: the signature's `&'x U` implies `U: 'x`.
    param_implied: Vec<(u32, Origin), A>,
    /// Parameters met in the signature where its implied bounds are not read.
    param_unsure: Vec<u32, A>,
    /// No parameter's bounds are fully read: true until `signature` lays them, and set again
    /// when it meets a type whose implied bounds are not read.
    param_all_unsure: bool,
    /// `Facts::implied_bounds` answered: an ADT's and a trait object's or fn pointer's
    /// implied bounds on a parameter are then read (`sig_param_bounds` no longer marks them).
    implied_given: bool,
    ret_block: Block,
    stats: Stats,
    plant: Plant,
    /// The oracle's comparing lowerings only (oracle.rs): a refusal of the call rule is per
    /// call, so the walk goes on past it to compare the body's later calls, the first refusal
    /// held in `held` and returned when the walk ends ([`Lower::finish_held`]).
    per_call: bool,
    held: Option<Refusal>,
    /// The flow's arguments: the body's parameters, or an async block's captures.
    nargs: u32,
    /// The async block whose body is lowered (`lower_async`); `None` for a fn body.
    async_root: Option<ExprId>,
    /// True when lowering a nested closure or async block body (not a top-level fn/async fn).
    in_nested: bool,
    /// For a non-async RPIT fn (`-> impl Trait`), the placeholder representing the opaque's
    /// captured lifetimes (`future_region(regions)`). Every concrete return position gets a
    /// distinct existential constrained one-way into `rpit_out` at each return.
    rpit_out: Option<Origin>,
    /// True when the outer body is an `async fn` whose return place `Local(0)` holds the single
    /// opaque future region (`future_region`), so the root async block's capture origins flow one
    /// way into it without equating independent captures (`relate`).
    async_body: bool,
    /// Per capture by reference of the async block: the pattern, the place `*arg` an
    /// `Expr::Local` of it reads, and its type.
    upvar_places: Vec<(PatId, Place, TyD), A>,
    /// Environment fields of the closure currently being lowered (capture_places.rs).
    env_now: Vec<blaze_typeck::EnvField, A>,
    /// Upvar places of projected environment fields, indexed by field order (capture_places.rs).
    field_places: Vec<Option<(Place, TyD, Option<Mutbl>)>, A>,
    /// Per suspension, in statement order, its `Expr::Await`.
    points: Vec<ExprId, A>,
    /// Per local: what it stands for (identity.rs): the one table of identities, emitted in
    /// `LoweredAsync::identity`.
    identity: Vec<Identity, A>,
    /// Why the last `call_by_generic_signature` related nothing (generic_call.rs `Misfit`):
    /// what a `CallWritesLifetimes { lacks: Unaligned(..) }` refusal says.
    misfit: Misfit,
    /// Per `.await` being lowered, innermost last: `(operand, await)`, so the call that is the
    /// operand knows it is awaited (await_call.rs `awaited_shadow`).
    awaiting: Vec<(ExprId, ExprId), A>,
    /// Per awaited call lowered and not yet awaited: the call's destination, and either the
    /// local whose positions are its awaited value's or why there is none, with the call's head
    /// when it has one (await_call.rs `awaited_shadow`, coroutine.rs `await_done`).
    awaited: Vec<(Place, Result<Local, (AwaitUnrelated, Option<ItemKey>)>), A>,
    /// The first STRICTER relation this flow used where the precise one could not be made
    /// ([`Lowered::coarse`]): a `Cell` because the type walks that meet one take `&self`.
    coarse: core::cell::Cell<Option<Refusal>>,
    /// Pattern-bound locals extracted directly from an ADT variant field (`(pat, adt, variant, field)`),
    /// so `call_through_value` can read the field's declared `dyn Fn*` regions (e.g. `Provider::Handler(handler)`).
    local_field_source: Vec<(PatId, ItemKey, u32, u32), A>,
    /// Places projected from an ADT field (`(place, adt, variant, field)`), so pattern bindings
    /// extracting a fn pointer (e.g. `let Some(dup) = self.duplicator`) can trace back to the
    /// field's declared region column.
    place_field_source: Vec<(Place, ItemKey, u32, u32), A>,
    /// Pattern-bound `FnPtr` locals traced back to their source ADT field (`(pat, adt, variant, field)`).
    local_fn_field: Vec<(PatId, ItemKey, u32, u32), A>,
}

/// Every type walk visits each node at most once per position: bounded by the table's size.
const MOST_POSITIONS: u32 = 4096;

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: one_call::CallRule> Lower<'b, 'c, F, A, R> {
    #[inline(never)]
    fn new(body: &'b BodyView<'b>, ck: &'c Checked<'c>, facts: &'b F, alloc: A) -> blaze_own::Box<Self, A> {
        let n = body.params.len() as u32;
        let b = builder::Builder::new(FlowBuilder::new_in(1 + n, n, alloc));
        let mut pat_local = Vec::new_in(alloc);
        pat_local.resize(body.pats.len(), u32::MAX);
        let mut extended = Vec::new_in(alloc);
        extended.resize(body.exprs.len(), NOT_EXTENDED);
        let mut is_target = Vec::new_in(alloc);
        is_target.resize(body.exprs.len(), false);
        for e in body.exprs {
            if let Expr::Break { target, .. } = *e
                && let Some(t) = is_target.get_mut(target.ix())
            {
                *t = true;
            }
        }
        let tys = ck.types.view();
        let n_exprs = body.exprs.len();
        let n_pats = body.pats.len();
        let n_tys = tys.len();
        let expr_def_row = index_rows(ck.expr_defs.column::<0>(), n_exprs, ExprId::ix, alloc);
        let pat_def_row = index_rows(ck.pat_defs.column::<0>(), n_pats, PatId::ix, alloc);
        let callee_row = index_rows(ck.callees.column::<0>(), n_exprs, ExprId::ix, alloc);
        let field_row = index_rows(ck.fields.column::<0>(), n_exprs, ExprId::ix, alloc);
        let closure_ordinal = index_rows(ck.closures.column::<0>(), n_exprs, ExprId::ix, alloc);
        let (cap_offsets, cap_rows) = build_capture_csr(ck.captures.column::<0>(), n_exprs, alloc);
        let l = Lower {
            body,
            ck,
            tys,
            facts,
            b,
            rule: core::marker::PhantomData,
            alloc: alloc,
            expr_def_row,
            pat_def_row,
            callee_row,
            field_row,
            closure_ordinal,
            cap_offsets,
            cap_rows,
            copy_memo: Vec::from_elem_in(core::cell::Cell::new(0u8), n_tys, alloc),
            drop_memo: Vec::from_elem_in(core::cell::Cell::new(0u8), n_tys, alloc),
            npos_memo: Vec::from_elem_in(core::cell::Cell::new(u32::MAX), n_tys, alloc),
            pat_local,
            local_ty: Vec::new_in(alloc),
            local_origs: Vec::new_in(alloc),
            origs: Vec::new_in(alloc),
            pinfo: Vec::new_in(alloc),
            vals: Vec::new_in(alloc),
            tasks: Vec::new_in(alloc),
            vars: Vec::new_in(alloc),
            scope_marks: Vec::new_in(alloc),
            scope_temps: Vec::new_in(alloc),
            temps: Vec::new_in(alloc),
            temp_marks: Vec::new_in(alloc),
            cond_temps: Vec::new_in(alloc),
            held_marks: Vec::new_in(alloc),
            targets: Vec::new_in(alloc),
            two_phase: Vec::new_in(alloc),
            extended,
            super_ext: Vec::new_in(alloc),
            is_target,
            uni: Vec::new_in(alloc),
            param_origs: Vec::new_in(alloc),
            param_implied: Vec::new_in(alloc),
            param_unsure: Vec::new_in(alloc),
            param_all_unsure: true,
            implied_given: false,
            ret_block: Block(0),
            stats: Stats::default(),
            plant: Plant::None,
            per_call: false,
            held: None,
            nargs: n,
            async_root: None,
            in_nested: false,
            rpit_out: None,
            async_body: false,
            upvar_places: Vec::new_in(alloc),
            env_now: Vec::new_in(alloc),
            field_places: Vec::new_in(alloc),
            points: Vec::new_in(alloc),
            identity: Vec::new_in(alloc),
            misfit: Misfit::NotTried,
            awaiting: Vec::new_in(alloc),
            awaited: Vec::new_in(alloc),
            coarse: core::cell::Cell::new(None),
            local_field_source: Vec::new_in(alloc),
            place_field_source: Vec::new_in(alloc),
            local_fn_field: Vec::new_in(alloc),
        };
        blaze_own::Box::new_in(l, alloc)
    }

    // ---- refusals and small reads ----

    fn refuse<T>(&self, form: Form, e: ExprId) -> Result<T, Refusal> {
        Err(Refusal { form, range: range_of(self.body, e) })
    }

    /// Notes that a STRICTER relation than Rust's stands at `e` where the precise one could not
    /// be made, named as `form` (the first one only: [`Lowered::coarse`]). The caller then
    /// relates by the stricter relation; the body is refused by this name only if it has a
    /// finding.
    fn coarse_step(&self, form: Form, e: ExprId) {
        if self.coarse.get().is_none() {
            self.coarse.set(Some(Refusal { form, range: range_of(self.body, e) }));
        }
    }

    fn refuse_pat<T>(&self, form: Form, p: PatId) -> Result<T, Refusal> {
        let range = self.body.pat_ranges.get(p.ix()).copied().unwrap_or(Range { start: 0, end: 0 });
        Err(Refusal { form, range })
    }

    fn span(&self, e: ExprId) -> Span {
        let r = range_of(self.body, e);
        Span { lo: r.start, hi: r.end }
    }

    fn node(&self, t: PTy, e: ExprId) -> Result<PNode, Refusal> {
        match self.tys.node(t) {
            Ok(n) => Ok(n),
            Err(_) => self.refuse(Form::Malformed, e),
        }
    }

    fn list(&self, l: List, e: ExprId) -> Result<&'c [PTy], Refusal> {
        match self.tys.list(l) {
            Ok(s) => Ok(s),
            Err(_) => self.refuse(Form::Malformed, e),
        }
    }

    fn expr(&self, e: ExprId) -> Result<Expr, Refusal> {
        match self.body.exprs.get(e.ix()) {
            Some(x) => Ok(*x),
            None => self.refuse(Form::Malformed, self.body.root),
        }
    }

    fn expr_ty(&self, e: ExprId) -> Result<PTy, Refusal> {
        match self.ck.expr_ty.get(e.ix()) {
            Some(t) => Ok(*t),
            None => self.refuse(Form::Malformed, e),
        }
    }

    /// The type after the expression's adjustments.
    fn adjusted_ty(&self, e: ExprId) -> Result<PTy, Refusal> {
        let (lo, hi) = self.adjust_rows(e);
        if hi > lo {
            let tys = self.ck.adjusts.column::<2>();
            return Ok(tys[hi - 1]);
        }
        self.expr_ty(e)
    }

    fn adjust_rows(&self, e: ExprId) -> (usize, usize) {
        let col = self.ck.adjusts.column::<0>();
        let lo = col.partition_point(|x| *x < e);
        let hi = col.partition_point(|x| *x <= e);
        (lo, hi)
    }

    #[inline]
    fn capture_range(&self, e: ExprId) -> (usize, usize) {
        let i = e.ix();
        if i < self.cap_offsets.len().saturating_sub(1) {
            (self.cap_offsets[i] as usize, self.cap_offsets[i + 1] as usize)
        } else {
            (0, 0)
        }
    }

    #[inline]
    pub(crate) fn captures_of(&self, e: ExprId) -> &[u32] {
        let (lo, hi) = self.capture_range(e);
        &self.cap_rows[lo..hi]
    }

    #[inline]
    fn closure_row(&self, e: ExprId) -> Option<usize> {
        let row = *self.closure_ordinal.get(e.ix())?;
        if row == u32::MAX { None } else { Some(row as usize) }
    }

    fn expr_def(&self, e: ExprId) -> Option<Def> {
        let row = *self.expr_def_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        Some(self.ck.expr_defs.column::<1>()[row as usize])
    }

    fn pat_def(&self, p: PatId) -> Option<Def> {
        let row = *self.pat_def_row.get(p.ix())?;
        if row == u32::MAX {
            return None;
        }
        Some(self.ck.pat_defs.column::<1>()[row as usize])
    }

    fn callee_of(&self, e: ExprId) -> Option<(ItemKey, List)> {
        let row = *self.callee_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        let i = row as usize;
        Some((self.ck.callees.column::<1>()[i], self.ck.callees.column::<2>()[i]))
    }

    fn field_of(&self, e: ExprId) -> Option<u32> {
        let row = *self.field_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        Some(self.ck.fields.column::<2>()[row as usize])
    }

    fn adt_facts(&self, k: ItemKey, e: ExprId) -> Result<AdtFacts, Refusal> {
        match self.facts.adt(k) {
            Some(f) => Ok(f),
            None => self.refuse(Form::UnknownAdt, e),
        }
    }

    // ---- type facts, each a bounded work list ----

    /// The number of origin positions of `t`: one per reference, one per lifetime parameter of an ADT, one
    /// per opaque, alias or trait object; in pre-order, a node's own before its children's. A fn
    /// pointer's are its inputs' then its output's, as a tuple's: the layout is a function of the
    /// erased type (rule 21), and which of them the pointer's binder binds is a region of the
    /// head's column that holds the type (`Region::Late`, D148: the erased table cannot hold it).
    ///
    /// A node's OWN count is `blaze_typeck::regions::region_arity`, the one counter the head
    /// lowering's region column is laid by (RT1 piece P0); the walk below (which children, the
    /// closure and async-block captures) is still this lowering's.
    ///
    /// In the rule's grain (grain.rs): [`Lower::npos_positions`] under the positions, the
    /// type's region slots (grain.rs `region_slots`) under the one rule. Every layout walk
    /// counts by this one.
    fn npos(&self, t: PTy, e: ExprId) -> Result<u32, Refusal> {
        <R::Grain as grain::Grain>::count(self, t, e)
    }

    /// [`Lower::npos`] in the positions' grain (the old paths', unchanged).
    pub(crate) fn npos_positions(&self, root: PTy, e: ExprId) -> Result<u32, Refusal> {
        use blaze_typeck::regions::{Arity, Count, NotKnown, Unknown, region_arity};
        if let Some(cell) = self.npos_memo.get(root.0 as usize) {
            let cached = cell.get();
            if cached != u32::MAX {
                return Ok(cached);
            }
        }
        let root_node = self.node(root, e)?;
        let quick = match root_node {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::FnDef(..)
            | PNode::Param(_)
            | PNode::Infer
            | PNode::Error
            | PNode::ConstValue { .. }
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_) => Some(0),
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) if l.len == 0 => Some(0),
            PNode::Opaque(_, _) | PNode::Alias { .. } | PNode::Dynamic(_) => Some(1),
            _ => None,
        };
        if let Some(ans) = quick {
            if let Some(cell) = self.npos_memo.get(root.0 as usize) {
                cell.set(ans);
            }
            return Ok(ans);
        }
        /// The facts `region_arity` reads, from this lowering's `Facts`: an ADT's lifetime
        /// parameters as its head declares them. An opaque and an alias count ONE position as
        /// before; they take their true arity (an opaque's captured lifetimes, an alias's trait
        /// and associated-type lifetime arguments) when the head record carries it (RT1 piece
        /// P2), and this walk then descends into their arguments as `regions::slots` does.
        struct ByFacts<'f, G>(&'f G);
        impl<G: Facts> Arity for ByFacts<'_, G> {
            fn adt_lifetimes(&self, adt: ItemKey) -> Count {
                match self.0.adt(adt) {
                    Some(f) => Count::Known(f.lifetimes),
                    None => Count::NotKnown,
                }
            }
            fn opaque_captures(&self, _: ItemKey) -> Count {
                Count::Known(1)
            }
            fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
                Count::Known(1)
            }
            fn trait_lifetimes(&self, _: ItemKey) -> Count {
                Count::NotKnown
            }
        }
        let arity = ByFacts(self.facts);
        let mut stack = TyStack::new(root, self.alloc);
        let mut n = 0u32;
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            if let Some(cell) = self.npos_memo.get(t.0 as usize) {
                let cached = cell.get();
                if cached != u32::MAX {
                    n += cached;
                    continue;
                }
            }
            let node = self.node(t, e)?;
            match node {
                // A body's own types: their positions are their captures', counted below (no
                // declared type has them, so the one counter gives them none of their own).
                PNode::Future(..) | PNode::Closure { .. } | PNode::Coroutine { .. } => {}
                _ => match region_arity(t, node, &arity) {
                    Ok(own) => n += own,
                    Err(NotKnown { what: Unknown::AdtLifetimes(_), .. }) => return self.refuse(Form::UnknownAdt, e),
                    // `ByFacts` knows every opaque and alias (one each, above).
                    Err(NotKnown { what: Unknown::OpaqueCaptures(_) | Unknown::AliasLifetimes { .. } | Unknown::TraitLifetimes(_), .. }) => {
                        return self.refuse(Form::TypeNode, e);
                    }
                },
            }
            match node {
                PNode::Ref(_, i) => stack.push(i),
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) => stack.push(i),
                PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Adt(_, l) => stack.extend_from_slice(self.list(l, e)?),
                // Not descended into (as before P0): one position each, counted above.
                PNode::Opaque(_, _) | PNode::Alias { .. } | PNode::Dynamic(_) => {}
                // A closure of this body is its captures (the Reference, "Closure types": "A
                // closure expression produces a closure value with a unique, anonymous type",
                // "roughly" a struct of the captured variables): one position per capture by
                // reference, then the captured type's own.
                PNode::Closure { owner, ordinal, .. } if owner == self.body.owner => {
                    let ck: &'c Checked<'c> = self.ck;
                    let Some(&ce) = ck.closures.column::<0>().get(ordinal as usize) else {
                        return self.refuse(Form::Malformed, e);
                    };
                    // After the captures', the signature positions its owner binds (D148:
                    // closure.rs `sig_slots`).
                    n += self.owner_sig_count(ordinal as usize, e)?;
                    for f in ck.env_fields_by::<blaze_typeck::Places>(ce) {
                        if f.how != blaze_typeck::CaptureKind::ByValue {
                            n += 1;
                        }
                        match f.place_ty(ck) {
                            Some(t) => stack.push(t),
                            None => return self.refuse(Form::Malformed, e),
                        }
                    }
                }
                // An async block of this body is its captures, as a closure is (the Reference,
                // "Async blocks": "Async blocks capture variables from their environment using
                // the same capture modes as closures"): one position per capture by
                // reference, then the captured type's own.
                PNode::Future(fe, _, _) => {
                    let ck: &'c Checked<'c> = self.ck;
                    for &row in self.captures_of(fe) {
                        let i = row as usize;
                        if ck.captures.column::<2>()[i] != blaze_typeck::CaptureKind::ByValue {
                            n += 1;
                        }
                        match ck.local_ty.get(ck.captures.column::<1>()[i].ix()) {
                            Some(&t) => stack.push(t),
                            None => return self.refuse(Form::Malformed, e),
                        }
                    }
                }
                PNode::Closure { .. } | PNode::Coroutine { .. } => return self.refuse(Form::TypeNode, e),
                _ => {}
            }
        }
        if n != u32::MAX
            && let Some(cell) = self.npos_memo.get(root.0 as usize)
        {
            cell.set(n);
        }
        Ok(n)
    }

    /// What a value of opaque type `k` keeps live by its item bounds, the ONE home of the rule
    /// (rustc_borrowck liveness for an alias or opaque, rust-lang/rust#116733), read by
    /// [`Lower::opaques_live`] (the elision arm) and the generic signature path (generic_call.rs
    /// `Proj::live`): the opaque's `+ 'r` bounds (its head's `Outlives::Type`, preds.rs, in its
    /// fn's numbering). Any `'static`: nothing. Else all one region `'r`: only `'r`. Else (no
    /// bound, two different ones, the head not read): every captured region.
    fn opaque_live(&self, k: ItemKey) -> OpaqueLive {
        use blaze_typeck::region_facts::Outlives;
        let Ok(os) = self.facts.regions().outlives(k) else { return OpaqueLive::Captures };
        let mut one: Option<Region> = None;
        let mut some = false;
        for o in os.iter() {
            let Outlives::Type { region, .. } = *o else { continue };
            if region == Region::Static {
                return OpaqueLive::Nothing;
            }
            match one {
                None if !some => one = Some(region),
                Some(r) if r == region => {}
                _ => one = None,
            }
            some = true;
        }
        one.map_or(OpaqueLive::Captures, OpaqueLive::Only)
    }

    /// A closure expression: an `Aggregate` of its captures, each a borrow or a move of the
    /// captured local at this point, as the type checker decided (the Reference, "Closure
    /// types", "Capture modes": "The compiler prefers to capture a closed-over variable by
    /// immutable borrow, followed by unique immutable borrow, by mutable borrow, and finally by
    /// move"; with `move`, "all captures are by move or, for `Copy` types, by copy"). The loans
    /// go into the closure value's own positions, so they are live as long as it is. The
    /// closure's BODY is not lowered here (see [`Facts::closure_bodies_checked`]).
    fn closure_value(&mut self, e: ExprId) -> Result<(), Refusal> {
        let ty = TyD::of(self.expr_ty(e)?);
        let c = self.temp(ty, e)?;
        let cp = self.place_of(c);
        let ci = self.info(cp);
        let ck: &'c Checked<'c> = self.ck;
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        // What each capture relates once the aggregate is made: its place, where its positions
        // start in the closure's, how many, and its type.
        let mut rel: Vec<(Place, u32, u32, TyD), A> = Vec::new_in(self.alloc);
        let mut at = ci.start;
        if matches!(self.expr(e)?, Expr::Closure { .. }) {
            let mut fields: Vec<blaze_typeck::EnvField, A> = Vec::new_in(self.alloc);
            for f in ck.env_fields_by::<blaze_typeck::Places>(e) {
                fields.push(f);
            }
            for f in fields {
                let (pl, lt) = self.capture_source(f, e)?;
                let (src, st) = match f.how {
                    blaze_typeck::CaptureKind::ByValue => (pl, lt),
                    kind => {
                        let (m, bk) = if kind == blaze_typeck::CaptureKind::ByRef { (Mutbl::Not, BorrowKind::Shared) } else { (Mutbl::Mut, BorrowKind::Mut) };
                        if lt.wrap.is_some() {
                            return self.refuse(Form::Closure, e);
                        }
                        let rt = TyD { ty: lt.ty, wrap: Some(m) };
                        let t = self.temp(rt, e)?;
                        if self.plant != Plant::NoCaptures {
                            self.borrow(pl, lt, bk, t, e)?;
                        }
                        (self.place_of(t), rt)
                    }
                };
                let n = self.npos_d(st, e)?;
                if self.plant != Plant::NoCaptures {
                    ops.push(self.operand(Val { at: At::Place(src), ty: st }, e)?);
                    rel.push((src, at, n, st));
                }
                at += n;
            }
        } else {
            let (cap_lo, cap_hi) = self.capture_range(e);
            for idx in cap_lo..cap_hi {
                let i = self.cap_rows[idx] as usize;
                let p = ck.captures.column::<1>()[i];
                // In an async block's body, a variable the block captures by reference is `*arg`.
                let (pl, lt) = self.binding_place(p, e)?;
                let (src, st) = match ck.captures.column::<2>()[i] {
                    blaze_typeck::CaptureKind::ByValue => (pl, lt),
                    kind => {
                        let (m, bk) = if kind == blaze_typeck::CaptureKind::ByRef { (Mutbl::Not, BorrowKind::Shared) } else { (Mutbl::Mut, BorrowKind::Mut) };
                        if lt.wrap.is_some() {
                            return self.refuse(Form::Closure, e);
                        }
                        let rt = TyD { ty: lt.ty, wrap: Some(m) };
                        let t = self.temp(rt, e)?;
                        if self.plant != Plant::NoCaptures {
                            self.borrow(pl, lt, bk, t, e)?;
                        }
                        (self.place_of(t), rt)
                    }
                };
                let n = self.npos_d(st, e)?;
                if self.plant != Plant::NoCaptures {
                    ops.push(self.operand(Val { at: At::Place(src), ty: st }, e)?);
                    rel.push((src, at, n, st));
                }
                at += n;
            }
        }
        // After the captures', the signature positions the owner binds (closure.rs
        // `sig_slots`): no operand fills them; the calls and the requirements relate them.
        let caps = at - ci.start;
        let nsig = match self.closure_row(e) {
            Some(row) => self.owner_sig_count(row, e)?,
            None => 0,
        };
        if at + nsig != ci.start + ci.len {
            return self.refuse(Form::Malformed, e);
        }
        self.b.aggregate(cp, &ops, self.span(e));
        for j in 0..rel.len() {
            let (src, start, n, st) = rel[j];
            self.relate_at(src, start, n, st, e)?;
        }
        // What the closure's body needs of its captures and does not prove itself (closure.rs
        // `Requirement`, the NLL RFC's closure requirements): each a subset among the value's
        // positions here, at the aggregate's statement, so one the owner cannot prove is the
        // owner's finding at the closure's range.
        if self.plant != Plant::NoRequirements && R::RELATES {
            let facts: &'b F = self.facts;
            for r in facts.closure_requirements(e) {
                let from = self.need_origin(r.from, ci, caps, e)?;
                let into = self.need_origin(r.into, ci, caps, e)?;
                self.b.subset(from, into);
            }
        }
        self.push_place(cp, ty);
        Ok(())
    }

    /// The origin a requirement's end names in the closure value laid at `ci`, whose first
    /// `caps` positions are its captures' and the rest its owner signature positions.
    fn need_origin(&mut self, n: Need, ci: PInfo, caps: u32, e: ExprId) -> Result<Origin, Refusal> {
        match n {
            Need::Static => Ok(self.universal(Region::Static)),
            Need::Capture(k) if ci.exact && k < caps && caps <= ci.len => Ok(self.origs[(ci.start + k) as usize]),
            Need::Sig(k) if ci.exact && caps + k < ci.len => Ok(self.origs[(ci.start + caps + k) as usize]),
            Need::OwnerParam(i) => Ok(self.universal(Region::Param(i))),
            Need::Param(u) => Ok(self.param_origin(u)),
            Need::Capture(_) | Need::Sig(_) => self.refuse(Form::Malformed, e),
        }
    }

    fn npos_d(&self, t: TyD, e: ExprId) -> Result<u32, Refusal> {
        Ok(self.npos(t.ty, e)? + u32::from(t.wrap.is_some()))
    }

    /// The Reference, "Moved and copied types": a value whose type implements `Copy` is copied.
    fn is_copy(&self, t: TyD, e: ExprId) -> Result<bool, Refusal> {
        if let Some(m) = t.wrap {
            return Ok(m == Mutbl::Not);
        }
        self.is_copy_pty(t.ty, e)
    }

    fn is_copy_pty(&self, root: PTy, e: ExprId) -> Result<bool, Refusal> {
        pty_is_copy(
            CopyWalk {
                facts: self.facts,
                copy_memo: &self.copy_memo,
                adt_args: AdtCopyArgs::Facts,
                closures: ClosureCopy::ByValueCaptures {
                    owner: self.body.owner,
                    ck: self.ck,
                    cap_offsets: &self.cap_offsets,
                    cap_rows: &self.cap_rows,
                },
                alloc: self.alloc,
            },
            root,
            |t| self.tys.node(t).ok(),
            |l| self.tys.list(l).ok(),
            |err| match err {
                CopyWalkError::Malformed => Refusal { form: Form::Malformed, range: range_of(self.body, e) },
                CopyWalkError::UnknownAdt => Refusal { form: Form::UnknownAdt, range: range_of(self.body, e) },
                CopyWalkError::Bound => Refusal { form: Form::Bound, range: range_of(self.body, e) },
                CopyWalkError::CopyArgsUnaligned { adt } => {
                    Refusal { form: Form::CopyArgsUnaligned { adt }, range: range_of(self.body, e) }
                }
            },
        )
    }

    /// Whether dropping a value of `t` may run code (more is safe: a `Drop` of a value that needs
    /// none is a deep write the borrow check may refuse, never a missed one).
    fn needs_drop(&self, t: TyD, e: ExprId) -> Result<bool, Refusal> {
        if t.wrap.is_some() {
            return Ok(false);
        }
        self.needs_drop_pty(t.ty, e)
    }

    fn needs_drop_pty(&self, root: PTy, e: ExprId) -> Result<bool, Refusal> {
        pty_needs_drop(
            DropWalk {
                facts: self.facts,
                copy_memo: &self.copy_memo,
                drop_memo: &self.drop_memo,
                closures: ClosureDrop::Ignore,
                alloc: self.alloc,
            },
            root,
            || self.is_copy_pty(root, e),
            |t| self.tys.node(t).ok(),
            |l| self.tys.list(l).ok(),
            |err| match err {
                DropWalkError::Malformed => Refusal { form: Form::Malformed, range: range_of(self.body, e) },
                DropWalkError::UnknownAdt => Refusal { form: Form::UnknownAdt, range: range_of(self.body, e) },
                DropWalkError::Bound => Refusal { form: Form::Bound, range: range_of(self.body, e) },
            },
        )
    }

    fn is_unit_or_never(&self, t: PTy, e: ExprId) -> Result<bool, Refusal> {
        Ok(match self.node(t, e)? {
            PNode::Never => true,
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => l.len == 0,
            _ => false,
        })
    }

    // ---- locals, places, origins ----

    fn ensure_pinfo(&mut self, p: Place, info: PInfo) {
        let i = p.0 as usize;
        if self.pinfo.len() <= i {
            self.pinfo.resize(i + 1, PInfo { start: 0, len: 0, exact: true, deref: 0, parent: u32::MAX, local: 0 });
        }
        self.pinfo[i] = info;
    }

    /// Per position of `t` (in `npos_d` / `position_variances` pre-order), whether using or
    /// dropping a value of type `t` keeps that origin live:
    /// - `OriginLive::Full`: live on both use and drop.
    /// - `OriginLive::NoDrop`: live on use, not on drop (RFC 1327 / dropck: references, raw
    ///   pointers, fn pointers, unions, non-dropping fields/args of structural ADTs).
    /// - `OriginLive::None`: kept in `self.origs` for subtyping/calls, but neither use nor drop
    ///   keeps it live (captured lifetimes of an `Opaque` whose item outlives bound excludes them,
    ///   rust-lang/rust#116733).
    fn origin_live_mask(&self, t: TyD, out: &mut Vec<OriginLive, A>, e: ExprId) -> Result<(), Refusal> {
        out.clear();
        let n = self.npos_d(t, e)?;
        if n == 0 {
            return Ok(());
        }
        enum Step {
            Ty(PTy, bool),
            Pos(OriginLive),
        }
        let mut todo: Vec<Step, A> = Vec::new_in(self.alloc);
        if t.wrap.is_some() {
            // A reference itself never runs a destructor on its referent, while inner opaques
            // still respect their item outlives bounds.
            todo.push(Step::Ty(t.ty, false));
            todo.push(Step::Pos(OriginLive::NoDrop));
        } else {
            todo.push(Step::Ty(t.ty, true));
        }
        let mut steps = 0u32;
        while let Some(step) = todo.pop() {
            steps += 1;
            if steps > MOST_POSITIONS || out.len() as u32 > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let (ty, active) = match step {
                Step::Pos(m) => {
                    out.push(m);
                    continue;
                }
                Step::Ty(ty, active) => (ty, active),
            };
            let live = if active { OriginLive::Full } else { OriginLive::NoDrop };
            match self.node(ty, e)? {
                PNode::Ref(_, i) => {
                    todo.push(Step::Ty(i, false));
                    todo.push(Step::Pos(OriginLive::NoDrop));
                }
                PNode::Ptr(_, i) => todo.push(Step::Ty(i, false)),
                PNode::FnPtr(l, _) => {
                    let xs = self.list(l, e)?;
                    for &x in xs.iter().rev() {
                        todo.push(Step::Ty(x, false));
                    }
                }
                PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => todo.push(Step::Ty(i, active)),
                PNode::Tuple(l) | PNode::Record(_, l) => {
                    for &x in self.list(l, e)?.iter().rev() {
                        todo.push(Step::Ty(x, active));
                    }
                }
                PNode::Union(l) => {
                    for &x in self.list(l, e)?.iter().rev() {
                        todo.push(Step::Ty(x, false));
                    }
                }
                PNode::Adt(k, l) => {
                    let af = self.adt_facts(k, e)?;
                    let args = self.list(l, e)?;
                    if af.drop_impl || af.needs_drop {
                        for &x in args.iter().rev() {
                            todo.push(Step::Ty(x, active));
                        }
                        for _ in 0..af.lifetimes {
                            todo.push(Step::Pos(live));
                        }
                    } else if af.union {
                        for &x in args.iter().rev() {
                            todo.push(Step::Ty(x, false));
                        }
                        for _ in 0..af.lifetimes {
                            todo.push(Step::Pos(OriginLive::NoDrop));
                        }
                    } else {
                        let drop_args = self.facts.drop_args(k);
                        for (j, &x) in args.iter().enumerate().rev() {
                            let arg_drops = active && match drop_args {
                                Some(d) if d.len() == args.len() => d[j],
                                _ => true,
                            };
                            todo.push(Step::Ty(x, arg_drops));
                        }
                        for _ in 0..af.lifetimes {
                            todo.push(Step::Pos(OriginLive::NoDrop));
                        }
                    }
                }
                PNode::Opaque(k, l) if R::ONE => {
                    if let Ok(caps) = self.facts.regions().opaque_captures(k) {
                        let ol = self.opaque_live(k);
                        let args = self.list(l, e)?;
                        for &x in args.iter().rev() {
                            todo.push(Step::Ty(x, active));
                        }
                        for j in (0..caps).rev() {
                            let mode = match ol {
                                OpaqueLive::Nothing => OriginLive::None,
                                OpaqueLive::Only(Region::Param(p)) if p < caps => {
                                    if j == p { live } else { OriginLive::None }
                                }
                                _ => live,
                            };
                            todo.push(Step::Pos(mode));
                        }
                    } else {
                        let cnt = self.npos(ty, e)?;
                        for _ in 0..cnt {
                            out.push(live);
                        }
                    }
                }
                PNode::Dynamic(_) | PNode::Opaque(..) | PNode::Alias { .. } | PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } | PNode::FnDef(..) => {
                    let cnt = self.npos(ty, e)?;
                    for _ in 0..cnt {
                        out.push(live);
                    }
                }
                _ => {}
            }
        }
        if out.len() as u32 != n {
            out.clear();
            for _ in 0..n {
                out.push(OriginLive::Full);
            }
        }
        Ok(())
    }

    /// A new local of type `t`, each of its positions a fresh origin (a placeholder for the
    /// signature's), or the origins given (`reuse`, for the return place under elision).
    fn new_local_with(&mut self, t: TyD, placeholder: bool, at: Option<Local>, e: ExprId) -> Result<Local, Refusal> {
        let l = match at {
            Some(l) => l,
            None => self.b.add_local(),
        };
        let n = self.npos_d(t, e)?;
        let mut live_mask: Vec<OriginLive, A> = Vec::new_in(self.alloc);
        if n > 0 {
            self.origin_live_mask(t, &mut live_mask, e)?;
        }
        let start = self.origs.len() as u32;
        for k in 0..n as usize {
            let o = self.b.origin(placeholder);
            match live_mask.get(k).copied().unwrap_or(OriginLive::Full) {
                OriginLive::Full => self.b.mentions(l.0, o),
                OriginLive::NoDrop => self.b.mentions_no_drop(l.0, o),
                OriginLive::None => {}
            }
            self.origs.push(o);
        }
        self.set_local(l, t, start, n);
        // identity.rs: every local named; a site with a sharper key renames it (`name_local`).
        let identity = match at {
            Some(Local(0)) => Identity::Return,
            Some(a) => Identity::Arg(a.0),
            None => Identity::Temp(e, Role::Value),
        };
        self.name_local(l, identity);
        Ok(l)
    }

    /// Names local `l` in the table of identities (identity.rs; `LoweredAsync::identity`).
    /// Rows not yet named hold `Return` until their `new_local_with` names them.
    fn name_local(&mut self, l: Local, id: Identity) {
        let i = l.0 as usize;
        if self.identity.len() <= i {
            self.identity.resize(i + 1, Identity::Return);
        }
        self.identity[i] = id;
    }

    fn set_local(&mut self, l: Local, t: TyD, start: u32, n: u32) {
        let i = l.0 as usize;
        if self.local_ty.len() <= i {
            self.local_ty.resize(i + 1, t);
            self.local_origs.resize(i + 1, (0, 0));
        }
        self.local_ty[i] = t;
        self.local_origs[i] = (start, n);
        let p = self.b.local(l.0);
        self.ensure_pinfo(p, PInfo { start, len: n, exact: true, deref: 0, parent: u32::MAX, local: l.0 });
    }

    fn temp(&mut self, t: TyD, e: ExprId) -> Result<Local, Refusal> {
        let l = self.new_local_with(t, false, None, e)?;
        self.b.stmt(StmtKind::StorageLive(l), self.span(e));
        self.temps.push(l);
        Ok(l)
    }

    fn place_of(&self, l: Local) -> Place {
        self.b.local(l.0)
    }

    fn info(&self, p: Place) -> PInfo {
        self.pinfo[p.0 as usize]
    }

    /// `parent` projected by `proj`, whose type is `child`; `parent_ty` says which positions it has.
    fn project(&mut self, parent: Place, parent_ty: TyD, proj: Proj, child: TyD, drop_type: bool, e: ExprId) -> Result<Place, Refusal> {
        let mark = if drop_type { blaze_flow::PlaceMark::DropField } else { blaze_flow::PlaceMark::None };
        self.project_mark(parent, parent_ty, proj, child, mark, e)
    }

    /// As [`Self::project`], the new place marked `mark` (blaze-flow `PlaceMark`: a field of a
    /// `Drop` type, an element of an unsized `[T]`, a deref of `&T` / raw pointer).
    fn project_mark(&mut self, parent: Place, parent_ty: TyD, proj: Proj, child: TyD, mark: blaze_flow::PlaceMark, e: ExprId) -> Result<Place, Refusal> {
        let mark = if proj == Proj::Deref && mark == blaze_flow::PlaceMark::None {
            let shared_or_ptr = match parent_ty.wrap {
                Some(Mutbl::Not) => true,
                Some(Mutbl::Mut) => false,
                None => matches!(self.node(parent_ty.ty, e)?, PNode::Ref(Mutbl::Not, _) | PNode::Ptr(..)),
            };
            if shared_or_ptr { blaze_flow::PlaceMark::SharedDeref } else { mark }
        } else {
            mark
        };
        let p = self.b.project_in(parent, proj, mark);
        let pi = self.info(parent);
        let mut info = PInfo { start: pi.start, len: pi.len, exact: false, deref: 0, parent: parent.0, local: pi.local };
        if pi.exact {
            match (proj, parent_ty.wrap) {
                (Proj::Deref, Some(m)) => {
                    info = PInfo { start: pi.start + 1, len: pi.len.saturating_sub(1), exact: true, deref: if m == Mutbl::Mut { 2 } else { 1 }, ..info };
                }
                (_, Some(_)) => return self.refuse(Form::Malformed, e),
                (_, None) => match (proj, self.node(parent_ty.ty, e)?) {
                    (Proj::Deref, PNode::Ref(m, _)) => {
                        let len = self.npos_d(child, e)?;
                        info = PInfo { start: pi.start + 1, len, exact: true, deref: if m == Mutbl::Mut { 2 } else { 1 }, ..info };
                    }
                    (Proj::Deref, PNode::Ptr(..)) => {
                        info = PInfo { start: pi.start, len: 0, exact: true, deref: 3, ..info };
                    }
                    (Proj::DerefBox, _) | (Proj::Index, _) => {
                        let len = self.npos_d(child, e)?;
                        info = PInfo { start: pi.start, len, exact: true, ..info };
                    }
                    (Proj::Field(i), PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l)) => {
                        let elems = self.list(l, e)?;
                        let mut off = 0;
                        for &t in elems.iter().take(i as usize) {
                            off += self.npos(t, e)?;
                        }
                        let len = self.npos_d(child, e)?;
                        info = PInfo { start: pi.start + off, len, exact: true, ..info };
                    }
                    // A variant of the value is the value: the same type, the same positions.
                    (Proj::Downcast(_), PNode::Adt(..)) => {
                        info = PInfo { start: pi.start, len: pi.len, exact: true, ..info };
                    }
                    // A field of a struct, or of an enum's variant (its parent the downcast),
                    // is exactly the positions its declared type names (`Facts::field_regions`).
                    // Coarse (all of the ADT's), `self.held.push(..)` with `self: &mut
                    // Buffered<'f, 'a>` and `held: Bytes<'a>` tied the loan to `'f` too.
                    (Proj::Field(i), PNode::Adt(k, _)) => {
                        let variant = match self.b.proj_of(parent) {
                            Some(Proj::Downcast(v)) => Some(v),
                            _ if !self.adt_facts(k, e)?.is_enum => Some(0),
                            _ => None,
                        };
                        if let Some(variant) = variant
                            && let Some((start, len)) = self.field_slot(pi, parent_ty.ty, variant, i, child, e)?
                        {
                            info = PInfo { start, len, exact: true, ..info };
                        }
                    }
                    _ => {}
                },
            }
        } else if proj == Proj::Deref {
            // A deref below an ADT's field (a peeled pattern's `*(opt as Some).0`): a `&mut`
            // there is a mutable deref, so a reborrow through it walks on to the prefixes above
            // and takes their references' regions too (RFC 2094, "Reborrow constraints": the
            // walk stops after a deref of a shared reference only). Taken as shared, the walk
            // stopped here and dropped `'a: 'r` for `&'r mut *(*s).f` with `s: &'a mut W`.
            let mutbl = match (parent_ty.wrap, self.node(parent_ty.ty, e)?) {
                (Some(m), _) | (None, PNode::Ref(m, _)) => Some(m),
                _ => None,
            };
            info.deref = if mutbl == Some(Mutbl::Mut) { 2 } else { 1 };
        }
        self.ensure_pinfo(p, info);
        Ok(p)
    }

    /// The positions of field `field` of `variant` of a value whose positions are `of` (exact)
    /// and whose type is the ADT `adt_ty`, laid out as the field's own type `field_ty` counts
    /// them: a new stretch of `origs` holding, per [`FieldRegion`], the value's origin of that
    /// lifetime parameter, the body's `'static`, or each of that type argument's origins.
    /// `(start, len)` of the stretch; `None` when [`Facts::field_regions`] gives no row (the
    /// caller keeps its coarse form). The field's positions ARE the value's (the field's type
    /// is its declared type at the value's arguments), so a loan into one reaches exactly the
    /// value's positions that type names, and a write of the field relates into them.
    fn field_slot(&mut self, of: PInfo, adt_ty: PTy, variant: u32, field: u32, field_ty: TyD, e: ExprId) -> Result<Option<(u32, u32)>, Refusal> {
        if !of.exact {
            return Ok(None);
        }
        let PNode::Adt(k, l) = self.node(adt_ty, e)? else { return Ok(None) };
        let facts: &'b F = self.facts;
        let Some(regions) = facts.field_regions(k, variant, field) else { return Ok(None) };
        let lifetimes = self.adt_facts(k, e)?.lifetimes;
        let args = self.list(l, e)?;
        // Where each argument's positions begin among the value's: its lifetimes, then each
        // argument's, in order (`npos`).
        let mut at: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        let mut total = lifetimes;
        for &a in args {
            let n = self.npos(a, e)?;
            at.push((total, n));
            total += n;
        }
        let unaligned = Form::FieldRegionsUnaligned { adt: k };
        if total != of.len {
            return self.refuse(unaligned, e);
        }
        let want = self.npos_d(field_ty, e)?;
        let start = self.origs.len() as u32;
        let mut bound: Vec<(u32, Origin), A> = Vec::new_in(self.alloc);
        for &r in regions {
            if self.origs.len() as u32 - start > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match r {
                FieldRegion::Lifetime(i) if i < lifetimes => {
                    let o = self.origs[(of.start + i) as usize];
                    self.origs.push(o);
                }
                FieldRegion::Static => {
                    let o = self.universal(Region::Static);
                    self.origs.push(o);
                }
                FieldRegion::Arg(i) if (i as usize) < at.len() => {
                    let (off, n) = at[i as usize];
                    for j in 0..n {
                        let o = self.origs[(of.start + off + j) as usize];
                        self.origs.push(o);
                    }
                }
                // A fn pointer's bound lifetime: any instance of its binder (as the body's own
                // in `universal`), one origin per binder variable of this field.
                FieldRegion::Bound(v) => {
                    let o = match bound.iter().find(|(x, _)| *x == v) {
                        Some(&(_, o)) => o,
                        None => {
                            let o = self.b.origin(false);
                            bound.push((v, o));
                            o
                        }
                    };
                    self.origs.push(o);
                }
                FieldRegion::Lifetime(_) | FieldRegion::Arg(_) => return self.refuse(unaligned, e),
            }
        }
        if self.origs.len() as u32 - start != want {
            return self.refuse(unaligned, e);
        }
        Ok(Some((start, want)))
    }

    fn origins(&self, p: Place) -> &[Origin] {
        let i = self.info(p);
        &self.origs[i.start as usize..(i.start + i.len) as usize]
    }

    /// Every origin of `places` made equal through one new origin: a superset of any relation a
    /// signature or an aggregate's type could state among them.
    fn unify(&mut self, places: &[Place]) {
        let mut any = false;
        for &p in places {
            any |= self.info(p).len > 0;
        }
        if !any {
            return;
        }
        let hub = self.b.origin(false);
        for &p in places {
            let i = self.info(p);
            for k in i.start..i.start + i.len {
                let o = self.origs[k as usize];
                self.b.subset(o, hub);
                self.b.subset(hub, o);
            }
        }
    }

    /// A call whose signature is not known: every position of its operands and its destination
    /// made equal through one origin (a superset of any signature's relations among them),
    /// except the first position of an operand that is itself a reference, which only flows
    /// in. The callee holds that reference by value: its lifetime is covariant there, and
    /// nothing the callee writes can put a loan into it (RFC 2094, "Subtyping": `&'a T` is
    /// covariant in `'a`). What this cannot state is a callee that demands `'static`: a callee
    /// whose head is known and writes lifetimes is refused before it comes here.
    fn unify_call(&mut self, vs: &[Val], dest: Place, e: ExprId) -> Result<(), Refusal> {
        // The oracle's hub allowance (oracle.rs): the call being logged went through the hub.
        self.b.mark_hub();
        let mut any = self.info(dest).len > 0;
        for v in vs {
            if let At::Place(p) = v.at {
                any |= self.info(p).len > 0;
            }
        }
        if !any {
            return Ok(());
        }
        let hub = self.b.origin(false);
        for i in 0..=vs.len() {
            let (p, one_way) = if i < vs.len() {
                let v = vs[i];
                let At::Place(p) = v.at else { continue };
                let top_ref = v.ty.wrap.is_some() || matches!(self.node(v.ty.ty, e)?, PNode::Ref(..));
                (p, top_ref && self.info(p).exact && self.plant != Plant::HubBothWays)
            } else {
                (dest, false)
            };
            let s = self.info(p);
            for k in s.start..s.start + s.len {
                let o = self.origs[k as usize];
                self.b.subset(o, hub);
                if !(one_way && k == s.start) {
                    self.b.subset(hub, o);
                }
            }
        }
        Ok(())
    }

    /// `dst = src` relates `src`'s type to `dst`'s at the statement just added: position by
    /// position when both are exact, each by its variance in `dst`'s type (variance.rs: the
    /// Reference's "Subtyping and Variance"; an unknown one invariant: more relations refuse
    /// more, never less); else all made equal.
    fn relate(&mut self, src: Place, dst: Place, dst_ty: TyD, e: ExprId) -> Result<(), Refusal> {
        let (s, d) = (self.info(src), self.info(dst));
        if s.len == 0 || d.len == 0 || self.plant == Plant::NoRelate {
            return Ok(());
        }
        if s.exact && d.exact && s.len == d.len {
            let mut vs: Vec<Variance, A> = Vec::new_in(self.alloc);
            self.position_variances(dst_ty, &mut vs, e)?;
            if vs.len() as u32 != s.len {
                return self.refuse(Form::Malformed, e);
            }
            self.relate_by(s.start, d.start, &vs);
            return Ok(());
        }
        // An `async fn`'s outer body returns its root async block into `Local(0)`'s single
        // `future_region`: each captured lifetime outlives that future region (one way), without
        // equating independent captures or tripping `Unified::ValueInexact`.
        if self.async_body && dst == self.place_of(Local(0)) && s.exact && d.exact && d.len == 1 {
            if let (Ok(d_start), Ok(s_start), Ok(s_end)) = (usize::try_from(d.start), usize::try_from(s.start), usize::try_from(s.start + s.len))
                && let (Some(&o), Some(srcs)) = (self.origs.get(d_start), self.origs.get(s_start..s_end))
            {
                for &src_o in srcs {
                    self.b.subset(src_o, o);
                }
                return Ok(());
            }
        }
        if s.len + d.len > 2 {
            self.coarse_step(Form::Unified { why: Unified::ValueInexact }, e);
        }
        self.unify(&[src, dst]);
        Ok(())
    }

    fn operand(&self, v: Val, e: ExprId) -> Result<Operand, Refusal> {
        Ok(match v.at {
            At::Const => Operand::Const,
            At::Place(p) => {
                if self.plant == Plant::AllCopy || self.is_copy(v.ty, e)? {
                    Operand::Copy(p)
                } else {
                    Operand::Move(p)
                }
            }
        })
    }

    /// `dest = v`, with its origins related.
    fn assign_val(&mut self, dest: Place, dest_ty: TyD, v: Val, e: ExprId) -> Result<(), Refusal> {
        let op = self.operand(v, e)?;
        self.b.assign(dest, op, self.span(e));
        if let At::Place(p) = v.at {
            self.relate(p, dest, dest_ty, e)?;
        }
        Ok(())
    }

    fn as_place(&mut self, v: Val, e: ExprId) -> Result<Place, Refusal> {
        match v.at {
            At::Place(p) => Ok(p),
            At::Const => {
                let t = self.temp(v.ty, e)?;
                let p = self.place_of(t);
                self.b.assign(p, Operand::Const, self.span(e));
                Ok(p)
            }
        }
    }

    fn pop(&mut self, e: ExprId) -> Result<Val, Refusal> {
        match self.vals.pop() {
            Some(v) => Ok(v),
            None => self.refuse(Form::Malformed, e),
        }
    }

    fn push_place(&mut self, p: Place, ty: TyD) {
        self.vals.push(Val { at: At::Place(p), ty });
    }

    fn fresh(&mut self) {
        let b = self.b.block();
        self.b.start(b);
    }

    /// `t = &kind p`: a loan into `t`'s first origin, `p`'s type related to `t`'s referent, and
    /// for a reborrow `subset(r, t)` for each deref of a reference `r` in `p`'s supporting
    /// prefixes (RFC 2094: stop after the deref of a shared reference).
    fn borrow(&mut self, p: Place, pty: TyD, kind: BorrowKind, t: Local, e: ExprId) -> Result<Loan, Refusal> {
        let tp = self.place_of(t);
        let ti = self.info(tp);
        if ti.len == 0 {
            return self.refuse(Form::Malformed, e);
        }
        let o = self.origs[ti.start as usize];
        let loan = self.b.borrow(tp, kind, p, o, self.span(e));
        let pi = self.info(p);
        if pi.len > 0 && ti.len > 1 {
            if pi.exact && pi.len == ti.len - 1 {
                // A borrow of a trait object `dyn Tr + 'p` (its one position is the object
                // lifetime): the borrow may name any shorter `'q`, `&'r mut (dyn Tr + 'q)` with
                // `'p: 'q`, even when mutable (the Reference, "Type coercions", unsized
                // coercions: `T: Unsize<U>` holds for `dyn Tr + 'p` to `dyn Tr + 'q`, and
                // `&mut T` coerces to `&mut U`). Sound: an unsized place cannot be written as a
                // whole, so nothing of the shorter `'q` can be stored where `'p` is assumed.
                // Related both ways, every `out.put(..)` through a `&mut dyn Buf` tied its loan to
                // `out`'s own object lifetime, live for the whole body (tempo class B, E0499).
                let object = pty.wrap.is_none() && matches!(self.node(pty.ty, e)?, PNode::Dynamic(_));
                if object {
                    // In the slot grain (grain.rs) only the FIRST slot is the object lifetime;
                    // the rest are its predicates' (`dyn Tr<'a> + 'p`'s `'a`), which unsizing
                    // keeps equal (`dyn Tr<'a> + 'p: Unsize<dyn Tr<'a> + 'q>`, never another
                    // `'a`): related both ways. In the positions' grain the one position is
                    // the object lifetime (or stands for more and is related one way, as before).
                    for k in 0..pi.len {
                        let (a, b) = (self.origs[(pi.start + k) as usize], self.origs[(ti.start + 1 + k) as usize]);
                        self.b.subset(a, b);
                        if <R::Grain as grain::Grain>::SLOTS && k > 0 {
                            self.b.subset(b, a);
                        }
                    }
                } else {
                    // `&'r P` is covariant in `P` and `&'r mut P` invariant (the Reference,
                    // "Subtyping and Variance"): a shared borrow's referent positions are
                    // related by their variance in `P` (variance.rs), a mutable one's both ways.
                    let mut vs: Vec<Variance, A> = Vec::new_in(self.alloc);
                    self.position_variances(pty, &mut vs, e)?;
                    if vs.len() as u32 != pi.len {
                        return self.refuse(Form::Malformed, e);
                    }
                    if kind != BorrowKind::Shared {
                        for v in vs.iter_mut() {
                            *v = Variance::In;
                        }
                    }
                    self.relate_by(pi.start, ti.start + 1, &vs);
                }
            } else {
                if pi.len + ti.len > 3 {
                    self.coarse_step(Form::Unified { why: Unified::BorrowInexact }, e);
                }
                let hub = self.b.origin(false);
                for k in pi.start..pi.start + pi.len {
                    let x = self.origs[k as usize];
                    self.b.subset(x, hub);
                    self.b.subset(hub, x);
                }
                for k in ti.start + 1..ti.start + ti.len {
                    let x = self.origs[k as usize];
                    self.b.subset(x, hub);
                    self.b.subset(hub, x);
                }
            }
        }
        let mut q = pi;
        let mut steps = 0u32;
        while q.parent != u32::MAX {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            if q.deref == 3 {
                // Dereferencing a raw pointer breaks the borrow chain: any outer reference
                // holding the raw pointer itself does not bound the resulting reference.
                break;
            }
            let up = self.pinfo[q.parent as usize];
            if q.deref == 1 || q.deref == 2 {
                if up.len > 0 {
                    let r = self.origs[up.start as usize];
                    self.b.subset(r, o);
                    if !up.exact {
                        for k in up.start + 1..up.start + up.len {
                            let x = self.origs[k as usize];
                            self.b.subset(x, o);
                        }
                    }
                }
                if q.deref == 1 {
                    break;
                }
            }
            q = up;
        }
        Ok(loan)
    }

    /// The end of local `l`'s scope: its `Drop` when its type needs one, then its storage's end
    /// WHATEVER its type (the Reference, "Drop scopes"): the end invalidates every loan of it
    /// still live (blaze-flow loans.rs, `StorageDead` is a shallow write). `needs_drop` decides
    /// only the `Drop`.
    fn drop_local(&mut self, l: Local, e: ExprId) -> Result<(), Refusal> {
        let t = self.local_ty[l.0 as usize];
        let drops = self.needs_drop(t, e)?;
        if drops {
            let p = self.place_of(l);
            self.b.stmt(StmtKind::Drop(p), self.span(e));
        }
        // CONTROL (`Plant::StorageEndTiedToDrop`): no storage end without a drop.
        if drops || self.plant != Plant::StorageEndTiedToDrop {
            self.b.stmt(StmtKind::StorageDead(l), self.span(e));
        }
        Ok(())
    }

    /// The drops of leaving every scope above `scopes` and every temporary above `temps`
    /// (a `break`, `continue` or `return`): the Reference's reverse order, nothing truncated.
    /// Scopes unwind innermost first ("Destructors": "When control flow leaves a drop scope
    /// all variables associated to that scope are freed"; scopes nest, a statement's
    /// temporaries around a block inside it): per variable scope from the innermost, the
    /// temporaries made inside it, then its variables; last the temporaries around the
    /// outermost one left. Before, every temporary went first and every variable after: `for x
    /// in read()?.lines() { f(x)?; }` dropped `read()`'s `String` (the `for` statement's
    /// temporary) at the `?` before the iterator variable borrowing it, a false "goes out of
    /// scope while the borrow is live" (tempo corpus fuzz.rs:390, :391; precompiles
    /// storage_tests/vecs.rs:28, tip403_registry/mod.rs:1147).
    fn exit_drops(&mut self, scopes: u32, temps: u32, e: ExprId) -> Result<(), Refusal> {
        if self.scope_temps.len() != self.scope_marks.len() {
            return self.refuse(Form::Malformed, e);
        }
        let (mut tend, mut vend) = (self.temps.len(), self.vars.len());
        let tlo = temps as usize;
        for d in (scopes as usize..self.scope_marks.len()).rev() {
            let (vm, tm) = (self.scope_marks[d] as usize, self.scope_temps[d].0 as usize);
            let inside = tm.clamp(tlo, tend.max(tlo));
            for i in (inside..tend).rev() {
                let l = self.temps[i];
                self.drop_local(l, e)?;
            }
            tend = tend.min(inside);
            let vlo = vm.min(vend);
            for i in (vlo..vend).rev() {
                let l = self.vars[i];
                // A parameter (or a closure's or async block's argument) too: its drop when it
                // needs one, and its storage's end whatever its type (storage-end-loans:
                // before, a parameter of a type with no drop glue was never ended, and a loan
                // of it that must outlive the function was accepted).
                self.drop_local(l, e)?;
            }
            vend = vlo;
        }
        for i in (tlo..tend).rev() {
            let l = self.temps[i];
            self.drop_local(l, e)?;
        }
        Ok(())
    }

    // ---- the walk ----

    fn run(&mut self) -> Result<(), Refusal> {
        let body = self.body;
        let root = body.root;
        // Block 0 is the flow's entry; the return block follows it.
        let entry = self.b.block();
        self.ret_block = self.b.block();
        self.b.start(entry);
        // The parameters: locals 1..=n, the signature's placeholders.
        let nparams = body.params.len() as u32;
        self.scope_marks.push(0);
        self.scope_temps.push((self.temps.len() as u32, self.temp_marks.len() as u32));
        let facts: &'b F = self.facts;
        // A constant initializer (a `const`, `static` or associated const's): as the facts say
        // (its head is a fn of no inputs, `Facts::constant_initializer`), or a body with no
        // parameter and no signature in its owner's head (below).
        let mut const_init = nparams == 0 && facts.constant_initializer();
        match facts.sig(body.owner) {
            Ok(sig) => {
                self.signature(sig, nparams, root)?;
                // The open projections' knowns, in the rule's grain (grain.rs `own_known`: the
                // old paths' `own_alias_known`; none under the slot grain).
                <R::Grain as grain::Grain>::own_known(self, nparams, root)?;
            }
            // Regions not read. Laid without them only where no region could be guessed: a body
            // with no parameter whose owner's head holds no signature (a `const` or `static`
            // initializer: its return place's placeholders all related, `return_place`), or one
            // where no parameter and not the return type carries a lifetime. Any other is not
            // checked rather than checked under guessed placeholders (D148, rule 3).
            Err(unread) => {
                let no_fn = nparams == 0 && unread.why == UnreadWhy::NoHead;
                const_init |= no_fn;
                let mut carries = !no_fn && self.npos_d(TyD::of(self.adjusted_ty(root)?), root)? > 0;
                for &p in body.params.iter() {
                    let t = match self.ck.pat_ty.get(p.ix()) {
                        Some(&t) => TyD::of(t),
                        None => return self.refuse_pat(Form::Malformed, p),
                    };
                    carries |= self.npos_d(t, root)? > 0;
                }
                if carries {
                    return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, root);
                }
                for (i, &p) in body.params.iter().enumerate() {
                    let t = match self.ck.pat_ty.get(p.ix()) {
                        Some(&t) => TyD::of(t),
                        None => return self.refuse_pat(Form::Malformed, p),
                    };
                    let l = Local(1 + i as u32);
                    self.new_local_with(t, true, Some(l), root)?;
                    self.vars.push(l);
                }
                self.return_place(nparams, root)?;
            }
        }
        for (i, &p) in body.params.iter().enumerate() {
            let l = Local(1 + i as u32);
            let ty = self.local_ty[l.0 as usize];
            let pl = self.place_of(l);
            match body.pats.get(p.ix()) {
                Some(Pat::Bind { ann, sub: None, .. }) if ann.by_ref.is_none() => {
                    self.pat_local[p.ix()] = l.0;
                }
                _ => self.bind_pattern(p, Val { at: At::Place(pl), ty })?,
            }
        }
        // A constant initializer's extending borrows (the `let` rules, applied to the
        // initializer: rustc region.rs `resolve_body`) make `'static` temporaries: `const X:
        // &[T] = &[..];`, `static S: &C = &mk();` hold them past the body's end. Before, they
        // were dropped at its end under the returned borrow: a false "a reference to a local or
        // temporary value is returned" (tempo evm handler/config.rs:60, zones contract_creation.rs
        // :24, :113, nitro-attestation lib.rs:45, shadow_replay expectations.rs:208, precompiles
        // tip20/mod.rs:147, tip403_registry/mod.rs:42, xtask check_abi.rs:43,
        // zones/check_abi.rs:147). A borrow that is no extending one (`g(&f())`) still dies at
        // the body's end, as rustc's does.
        if const_init {
            self.mark_extended(root, STATIC_EXTENDED)?;
        }
        self.tasks.push(Task::RootEnd(root));
        self.tasks.push(Task::Eval(root, false));
        let fuel = 32 * (body.exprs.len() + body.pats.len() + body.stmts.len()) as u64 + 256;
        let mut steps = 0u64;
        while let Some(t) = self.tasks.pop() {
            steps += 1;
            if steps > fuel {
                return self.refuse(Form::Bound, root);
            }
            self.step(t)?;
        }
        Ok(())
    }

    /// Local 0, the return place: its positions are the signature's. Under elision (the
    /// Reference, "Lifetime elision": "If there is exactly one lifetime used in the parameters
    /// (elided or not), that lifetime is assigned to all elided output lifetimes"; "If the
    /// receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to `Self` is
    /// assigned to all elided output lifetime parameters") they are that input's origin; when the
    /// signature writes lifetimes they are fresh placeholders, every placeholder taken as related
    /// to every other (the heads' lifetime part is not built: counted, `Stats::sig_writes_lifetimes`).
    fn return_place(&mut self, nparams: u32, root: ExprId) -> Result<(), Refusal> {
        let rt = TyD::of(self.adjusted_ty(root)?);
        let n = self.npos_d(rt, root)?;
        let sig = self.facts.callee(self.body.owner);
        let self_ref = sig.is_some_and(|s| s.self_ref);
        let writes = sig.is_none_or(|s| s.writes_lifetimes);
        let inputs: u32 = (1..=nparams).map(|l| self.local_origs[l as usize].1).sum();
        let elided = if n == 0 || writes {
            None
        } else if self_ref && nparams > 0 && self.local_origs[1].1 > 0 {
            Some(self.origs[self.local_origs[1].0 as usize])
        } else if inputs == 1 {
            (1..=nparams).find(|&l| self.local_origs[l as usize].1 == 1).map(|l| self.origs[self.local_origs[l as usize].0 as usize])
        } else {
            None
        };
        match elided {
            Some(o) => {
                let start = self.origs.len() as u32;
                for _ in 0..n {
                    self.origs.push(o);
                }
                self.b.mentions(0, o);
                self.set_local(Local(0), rt, start, n);
            }
            None => {
                self.new_local_with(rt, true, Some(Local(0)), root)?;
                if n > 0 {
                    self.stats.sig_writes_lifetimes = true;
                    let all = self.origs.len();
                    if all > 64 {
                        return self.refuse(Form::Bound, root);
                    }
                    for i in 0..all {
                        for j in 0..all {
                            self.b.known(self.origs[i], self.origs[j]);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The placeholder origin of a region of the body's own signature. `'static` outlives every
    /// other (the Reference, "Lifetime bounds": `'static` "outlives all other lifetimes").
    fn universal(&mut self, r: Region) -> Origin {
        if let Some(&(_, o)) = self.uni.iter().find(|(x, _)| *x == r) {
            return o;
        }
        // A lifetime a fn-pointer type of the signature binds (`Region::Late`, D148): no
        // region of the fn, so no placeholder. The body may use such a parameter at any
        // instance of its binder (`for<'r> fn(&'r u8)` is a subtype of `fn(&'a u8)`: the
        // Reference, "Subtyping and Variance", higher-ranked fn pointers), which an existential
        // origin says: it takes whatever region its uses ask, one per binder variable. Under the
        // control (`Facts::fn_binders` false) it is a placeholder like a named lifetime.
        if matches!(r, Region::Late { .. }) && self.facts.fn_binders() {
            let o = self.b.origin(false);
            self.uni.push((r, o));
            return o;
        }
        let o = self.b.origin(true);
        if r == Region::Static {
            for i in 0..self.uni.len() {
                // An existential of a bound lifetime is not a placeholder: `'static` is known
                // to outlive the placeholders only.
                if matches!(self.uni[i].0, Region::Late { .. }) && self.facts.fn_binders() {
                    continue;
                }
                let x = self.uni[i].1;
                self.b.known(o, x);
            }
        } else if let Some(&(_, st)) = self.uni.iter().find(|(x, _)| *x == Region::Static) {
            self.b.known(st, o);
        }
        self.uni.push((r, o));
        o
    }

    /// Every reference of `t` and how many positions its referent has, positions counted from
    /// `t`'s first: `&'a T` is well formed only when `T: 'a` (the Reference, "Lifetime bounds"),
    /// so each position in the referent outlives the reference's. A signature's types are well
    /// formed by assumption in its body (an implied bound) and by proof at each call.
    fn ref_ranges(&self, t: TyD, e: ExprId, out: &mut Vec<(u32, u32), A>) -> Result<(), Refusal> {
        out.clear();
        let mut stack: Vec<(PTy, u32), A> = Vec::new_in(self.alloc);
        if t.wrap.is_some() {
            out.push((0, self.npos(t.ty, e)?));
            stack.push((t.ty, 1));
        } else {
            stack.push((t.ty, 0));
        }
        let mut steps = 0u32;
        while let Some((t, at)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Ref(_, i) => {
                    out.push((at, self.npos(i, e)?));
                    stack.push((i, at + 1));
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) => stack.push((i, at)),
                PNode::Array(i, _) => stack.push((i, at)),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    let mut off = at;
                    for &x in self.list(l, e)? {
                        stack.push((x, off));
                        off += self.npos(x, e)?;
                    }
                }
                PNode::Adt(k, l) => {
                    let mut off = at + self.adt_facts(k, e)?.lifetimes;
                    for &x in self.list(l, e)? {
                        stack.push((x, off));
                        off += self.npos(x, e)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// RFC 1214 implied bounds for type parameters under references in a closure's captures,
    /// parameters, and return place: `&'r T` implies `P: 'r` for every type parameter `P`
    /// occurring inside `T`.
    fn ref_param_implied(&mut self, t: TyD, start: u32, e: ExprId) -> Result<(), Refusal> {
        let mut stack: Vec<(PTy, u32, Option<u32>), A> = Vec::new_in(self.alloc);
        if t.wrap.is_some() {
            stack.push((t.ty, 1, Some(0)));
        } else {
            stack.push((t.ty, 0, None));
        }
        let mut steps = 0u32;
        while let Some((cur, at, ref_at)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(cur, e)? {
                PNode::Param(p) => {
                    if let Some(ra) = ref_at {
                        let outer = self.origs[(start + ra) as usize];
                        if !self.param_implied.contains(&(p, outer)) {
                            self.param_implied.push((p, outer));
                        }
                    }
                }
                PNode::Ref(_, i) => stack.push((i, at + 1, Some(at))),
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push((i, at, ref_at)),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    let mut off = at;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, ref_at));
                        off += self.npos(x, e)?;
                    }
                }
                PNode::Adt(k, l) => {
                    let mut off = at + self.adt_facts(k, e)?.lifetimes;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, ref_at));
                        off += self.npos(x, e)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The parameters and the return place from the signature's lifetime part: one placeholder
    /// per region, each position of each parameter and of the return type taking its region's;
    /// known among them exactly what the signature states: its declared bounds, `'static`, and
    /// the bounds implied by its types being well formed. Nothing else is known, so a `subset`
    /// the body derives between two placeholders beyond these is a finding (flow.md section 3).
    /// The column is the head record's (D148; [`Facts::sig`]): input `k`'s positions are laid
    /// from child `k`, the return place's from the last ([`Lower::lay_positions`] maps each
    /// position to its slot). A position the flow gives an opaque type or a projection of a
    /// parameter's type is a placeholder of its own, related to nothing but `'static`. A
    /// column that does not line up with the body's types is refused by name
    /// (`SigPositionsUnaligned`), never laid by a guess.
    ///
    /// The body's FUTURE is the exception (an `async fn`'s, or a return-position `impl Future`
    /// whose body is an async block): the declared output is an opaque type, its slots the
    /// lifetimes it captures (edition rules and `use<..>` applied by the definer). The future
    /// has one region of its own, a placeholder that each captured lifetime outlives (the
    /// Reference, "Async functions": the future "holds its arguments"; "Impl trait",
    /// "Capturing"), and no relation between two captured lifetimes.
    fn signature(&mut self, sig: SigLife<'_>, nparams: u32, root: ExprId) -> Result<(), Refusal> {
        if !sig.well_formed() || sig.inputs() != nparams || self.body.params.len() != nparams as usize {
            return self.refuse(Form::SigPositionsUnaligned { positions: nparams as usize, regions: sig.inputs() as usize, site: SigSite::Arity }, root);
        }
        // The signature is laid: its types' implied bounds on the type parameters are read
        // below (`sig_param_bounds` sets this again where it cannot read them).
        self.param_all_unsure = false;
        // The bounds the signature's and the impl header's types imply, whole (front2
        // adt_facts.rs): `'long: 'short` known, `T: 'r` taken by `param_origin`.
        let facts: &'b F = self.facts;
        self.implied_given = false;
        if let Some((params, regions)) = facts.implied_bounds() {
            for &(long, short) in regions {
                let (ol, os) = (self.universal(long), self.universal(short));
                if self.plant != Plant::NoKnown {
                    self.b.known(ol, os);
                }
            }
            for &(p, r) in params {
                let o = self.universal(r);
                self.param_implied.push((p, o));
            }
            self.implied_given = true;
        }
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        let mut laid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        for i in 0..=nparams {
            let is_out = i == nparams;
            let (l, t) = if is_out {
                (Local(0), TyD::of(self.adjusted_ty(root)?))
            } else {
                let p = self.body.params[i as usize];
                match self.ck.pat_ty.get(p.ix()) {
                    Some(&t) => (Local(1 + i), TyD::of(t)),
                    None => return self.refuse_pat(Form::Malformed, p),
                }
            };
            let Some(regions) = (if is_out { sig.output() } else { sig.input(i) }) else {
                let site = SigSite::NoChild { input: (!is_out).then_some(i) };
                return self.refuse(Form::SigPositionsUnaligned { positions: nparams as usize, regions: sig.inputs() as usize, site }, root);
            };
            let declared = self.sig_declared(sig, i, root)?;
            let async_body = is_out && sig.body() != BodyOut::Output;
            if async_body {
                self.async_body = true;
            }
            let rpit_future = is_out && !async_body && matches!(self.node(t.ty, root)?, PNode::Future(..));
            // A non-async RPIT (`fn each(&self) -> impl Iterator<Item = &str>`): the declared
            // output is `PNode::Opaque` whose column slots `regions` are its captured lifetimes.
            // Every position of the concrete return type sits inside that opaque, so it may hold
            // any lifetime the opaque captures (and `'static`), via `future_region(regions)`:
            // local borrows and uncaptured lifetimes are still refused.
            let rpit_opaque = is_out && !async_body && !rpit_future && matches!(self.head_node(declared, root)?, PNode::Opaque(..));
            // An `async fn`'s body returns its future, the root async block's value (the
            // Reference, "Async functions"; `BodyOut`): ONE region. What the future yields is
            // checked in the root block's own flow, against the written return type
            // (coroutine.rs `async_fn_root`).
            let n = if async_body { 1 } else { self.npos_d(t, root)? };
            if is_out && n == 0 {
                // The body's value has no origin position (it diverges, or the output has
                // none): nothing borrowed can reach the return place.
                self.set_local(l, t, self.origs.len() as u32, 0);
                break;
            }
            let future = async_body || rpit_future;
            let input = (!is_out).then_some(i);
            let unaligned = |site| Form::SigPositionsUnaligned { positions: n as usize, regions: regions.len(), site };
            if future {
                if !matches!(self.head_node(declared, root)?, PNode::Opaque(..)) {
                    return self.refuse(unaligned(SigSite::FutureNotOpaque), root);
                }
            } else if rpit_opaque {
                if t.wrap.is_some() {
                    return self.refuse(unaligned(SigSite::Wrapped { input }), root);
                }
            } else if t.wrap.is_some() {
                return self.refuse(unaligned(SigSite::Wrapped { input }), root);
            } else if !self.lay_over(declared, t.ty, regions, &mut laid, root)? {
                return self.refuse(unaligned(SigSite::Unlaid { input }), root);
            } else if laid.len() != n as usize {
                return self.refuse(unaligned(SigSite::Count { input }), root);
            }
            let start = self.origs.len() as u32;
            if rpit_opaque {
                self.rpit_out = Some(self.future_region(regions));
            }
            let mut live_mask: Vec<OriginLive, A> = Vec::new_in(self.alloc);
            if n > 0 && !async_body {
                self.origin_live_mask(t, &mut live_mask, root)?;
            }
            for k in 0..n as usize {
                let o = if async_body {
                    self.future_region(regions)
                } else if rpit_future || rpit_opaque {
                    // The concrete future's or RPIT opaque's positions: distinct existentials,
                    // never one shared placeholder (that would relate independent captures or
                    // conflict with invariant positions like `&'static` in a closure). For
                    // `rpit_opaque`, each return constrains every position one-way into
                    // `self.rpit_out` (`future_region(regions)`).
                    self.b.origin(false)
                } else {
                    match laid.get(k) {
                        Some(&Some(r)) => self.universal(r),
                        // An opaque type's or a projection's one position (`lay_positions`).
                        Some(&None) => self.hidden_placeholder(),
                        None => return self.refuse(Form::Malformed, root),
                    }
                };
                match live_mask.get(k).copied().unwrap_or(OriginLive::Full) {
                    OriginLive::Full => self.b.mentions(l.0, o),
                    OriginLive::NoDrop => self.b.mentions_no_drop(l.0, o),
                    OriginLive::None => {}
                }
                self.origs.push(o);
            }
            self.set_local(l, t, start, n);
            if !is_out {
                self.vars.push(l);
            }
            self.sig_param_bounds(t, start, root)?;
            self.ref_ranges(t, root, &mut ranges)?;
            for j in 0..ranges.len() {
                let Some(&(at, inner)) = ranges.get(j) else {
                    return self.refuse(Form::Malformed, root);
                };
                let Some(&outer) = self.origs.get((start + at) as usize) else {
                    return self.refuse(Form::Malformed, root);
                };
                for k in 1..=inner {
                    let Some(&o) = self.origs.get((start + at + k) as usize) else {
                        return self.refuse(Form::Malformed, root);
                    };
                    if o != outer && self.plant != Plant::NoKnown {
                        self.b.known(o, outer);
                    }
                }
            }
        }
        // The declared outlives clauses, `(longer, shorter)` (ParamsView::outlives): known, as
        // `known(a, b)` says `a` outlives `b`.
        for &(a, b) in sig.bounds() {
            let (oa, ob) = (self.universal(a), self.universal(b));
            if self.plant != Plant::NoKnown {
                self.b.known(oa, ob);
            }
        }
        self.stats.sig_read = true;
        Ok(())
    }

    /// The declared type of child `k` of `sig`'s column: input `k`, or the output at
    /// `sig.inputs()` (the column is laid over `FnPtr(inputs.., output)`, D148; the head
    /// record's types are the store's table, the lowering's `tys`).
    fn sig_declared(&self, sig: SigLife<'_>, k: u32, e: ExprId) -> Result<PTy, Refusal> {
        let PNode::FnPtr(l, _) = self.head_node(sig.regions().col.ty(), e)? else {
            return self.refuse(Form::SigColumnRoot, e);
        };
        let kids = self.head_list(l, e)?;
        match kids.get(k as usize) {
            Some(&t) => Ok(t),
            None => self.refuse(Form::SigPositionsUnaligned { positions: k as usize + 1, regions: kids.len(), site: SigSite::ColumnShort }, e),
        }
    }

    /// [`Self::sig_declared`] with the destination's child (`k == sig.inputs()`) the call's
    /// `value` ([`CallValue`]): an awaited `async fn`'s written return type, laid over its own
    /// column's type (the head records' table, as the signature's).
    fn value_declared(&self, sig: SigLife<'_>, value: CallValue<'_>, k: u32, e: ExprId) -> Result<PTy, Refusal> {
        match value {
            CallValue::Awaited { ty, .. } if k == sig.inputs() => Ok(ty),
            _ => self.sig_declared(sig, k, e),
        }
    }

    /// A declared type of a head record's column, read in the columns' table
    /// ([`Facts::region_tys`]; the checked record's when the facts give none). Never a type of
    /// the body's: its `PTy`s number another table.
    fn head_node(&self, t: PTy, e: ExprId) -> Result<PNode, Refusal> {
        let tys = self.facts.region_tys().unwrap_or(self.tys);
        match tys.node(t) {
            Ok(n) => Ok(n),
            Err(_) => self.refuse(Form::SigColumnType, e),
        }
    }

    /// [`Lower::head_node`]'s list.
    fn head_list(&self, l: List, e: ExprId) -> Result<&[PTy], Refusal> {
        match self.facts.region_tys() {
            Some(tys) => match tys.list(l) {
                Ok(s) => Ok(s),
                Err(_) => self.refuse(Form::SigColumnType, e),
            },
            None => match self.tys.list(l) {
                Ok(s) => Ok(s),
                Err(_) => self.refuse(Form::SigColumnType, e),
            },
        }
    }

    /// Which slot of a column stands at each of the flow's origin positions of declared type
    /// `t` ([`Lower::npos`]'s positions, in their order: pre-order, a node's own before its
    /// children's, list children in order), `slots` the column's slots of `t` (one child of a
    /// signature's column). `out[k]` is position `k`'s region, or `None` for the ONE position
    /// the flow gives an opaque type or a projection (the column lays its captures or its alias
    /// lifetimes there, then its arguments': no single region of the flow's position).
    ///
    /// The two walks differ where the flow does not descend and the column does: a trait
    /// object's predicates (its own slot, the object lifetime, is the first of its subtree),
    /// an opaque type's or a projection's arguments, a fn item's. The extent of ONE such subtree
    /// in `slots` is what the rest leaves. `false` when `t` holds two of them and the rest
    /// leaves any slot (their extents cannot be told apart; with none left, each has none), a
    /// body's own type or a node this walk does not know, or the counts
    /// do not add up: the caller refuses by name or takes another path, and never reads a slot
    /// as another position's (a projection's slot is its trait's lifetime argument, not the
    /// projection's own region: reading one as the other would take a `'static` an impl
    /// demands for a region the caller chose).
    fn lay_positions(&self, t: PTy, slots: &[Region], out: &mut Vec<Option<Region>, A>, e: ExprId) -> Result<bool, Refusal> {
        out.clear();
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        // Pass 1: the slots the flow's walk accounts for, and the subtrees it does not.
        let mut regular = 0u32;
        let mut apart = 0u32;
        stack.push(t);
        let mut steps = 0u32;
        while let Some(x) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.head_node(x, e)? {
                PNode::Ref(_, i) => {
                    regular += 1;
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) => stack.extend_from_slice(self.head_list(l, e)?),
                PNode::Adt(k, l) => {
                    regular += self.adt_facts(k, e)?.lifetimes;
                    stack.extend_from_slice(self.head_list(l, e)?);
                }
                PNode::Dynamic(_) => {
                    regular += 1;
                    apart += 1;
                }
                PNode::Opaque(..) | PNode::Alias { .. } | PNode::FnDef(..) => apart += 1,
                PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never | PNode::Param(_) => {}
                // A const argument (`FixedBytes<32>`, `Uint<256, 4>`: tempo's `B256`, `U256`,
                // reth's `PeerId`) is a value, no type: it holds no region, so no position and
                // no slot (blaze-typeck regions.rs `region_arity` gives it 0, `kids` none).
                // Refused here before, every head that named one in an input or the output was
                // `CallHiddenInput { why: NotLaid }` at each call, and every body whose own
                // signature named one `SigPositionsUnaligned` (tempo 1120: 3,645 and 1,004
                // bodies): a regression of D148, which made this walk the one that lays heads.
                PNode::ConstValue { .. } | PNode::ConstParam(_) if self.plant != Plant::ConstArgUnlaid => {}
                _ => return Ok(false),
            }
        }
        let Some(extent) = (slots.len() as u32).checked_sub(regular) else { return Ok(false) };
        // `extent` slots lie under the subtrees apart. One subtree takes them all; none takes
        // none. Several split them in a way the column does not say, unless there are none to
        // split: each subtree's count is at least zero, so a zero sum is zero in each
        // (`Try::branch`'s `ControlFlow<Self::Residual, Self::Output>`: two projections, `Try`
        // and its associated types without a lifetime, no slot). CONTROL
        // (`Plant::HiddenSubtreesApart`): several refused even with nothing to split, as the
        // wave's L3 head did, which refused every `?` (`CallHiddenInput { input: u32::MAX }`).
        let use_exact_counts = self.plant != Plant::HiddenSubtreesApart && apart > 1 && extent != 0;
        let several = self.plant == Plant::HiddenSubtreesApart && apart > 1;
        if several || (apart == 0 && extent != 0) {
            return Ok(false);
        }
        let arity = crate::one_call::FactsArity { facts: self.facts };
        let heads = blaze_borrow::Columns::new(self.facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        // Pass 2: each position's slot, in the flow's order.
        stack.clear();
        stack.push(t);
        let mut at = 0usize;
        steps = 0;
        while let Some(x) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.head_node(x, e)? {
                PNode::Ref(_, i) => {
                    let Some(&r) = slots.get(at) else { return Ok(false) };
                    out.push(Some(r));
                    at += 1;
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) => {
                    for &y in self.head_list(l, e)?.iter().rev() {
                        stack.push(y);
                    }
                }
                PNode::Adt(k, l) => {
                    for _ in 0..self.adt_facts(k, e)?.lifetimes {
                        let Some(&r) = slots.get(at) else { return Ok(false) };
                        out.push(Some(r));
                        at += 1;
                    }
                    for &y in self.head_list(l, e)?.iter().rev() {
                        stack.push(y);
                    }
                }
                PNode::Dynamic(_) => {
                    let Some(&r) = slots.get(at) else { return Ok(false) };
                    out.push(Some(r));
                    if use_exact_counts {
                        let Ok(c) = blaze_borrow::RegionTypes::count(&heads, x) else { return Ok(false) };
                        let Some(c) = c.get().checked_sub(1) else { return Ok(false) };
                        at += 1 + c as usize;
                    } else {
                        at += 1 + extent as usize;
                    }
                }
                PNode::Opaque(..) | PNode::Alias { .. } => {
                    out.push(None);
                    if use_exact_counts {
                        let Ok(c) = blaze_borrow::RegionTypes::count(&heads, x) else { return Ok(false) };
                        at += c.get() as usize;
                    } else {
                        at += extent as usize;
                    }
                }
                PNode::FnDef(..) => {
                    if use_exact_counts {
                        let Ok(c) = blaze_borrow::RegionTypes::count(&heads, x) else { return Ok(false) };
                        at += c.get() as usize;
                    } else {
                        at += extent as usize;
                    }
                }
                _ => {}
            }
        }
        // `out` has one entry per position of `t` as the flow counts them (by construction:
        // each node above pushes what `npos` counts for it); each caller compares that count
        // with its own type's positions in the body's table, never `t` read there.
        Ok(at == slots.len())
    }

    /// [`Lower::lay_positions`] carried over to the BODY's type `body_t` of the same value (a
    /// parameter's, the return place's), position by position in the flow's order. The two
    /// walks agree everywhere but under a declared projection or opaque type: the head keeps
    /// `<TempoNode as Node<A>>::ComponentsBuilder` (ONE position, its slots apart), the checked
    /// body has its normalization (an ADT of any count of positions, often none). Each of the
    /// normalized subtree's positions is then a hidden position (`None`: a placeholder of its
    /// own, related to nothing but `'static`, as the projection's one was): the signature says
    /// nothing of a region inside a normalization, and knowing less of a placeholder is never
    /// unsound. Laid by the declared walk alone, the counts differed and the body was refused
    /// `SigPositionsUnaligned` (tempo node-launcher `launch(builder: TempoLaunchBuilder)`).
    /// `false` where the two types differ in shape anywhere else (the caller refuses by name).
    ///
    /// A declared projection whose impl the type checker selected (design B, head VERSION 26:
    /// the column lays `<T as Tr>::Name` as the alias node, its slots the trait's lifetimes,
    /// the GAT's own, then each argument's, the self type first) is laid THROUGH that impl
    /// ([`Lower::lay_through_impl`]): `get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }` with
    /// `impl<'i> Tr for Wrap<'i> { type A = Inner<'i>; }` lays `x`'s `Inner` position as `'a`,
    /// as rustc's normalized signature has it. `Selected::Generic` (`<T as Tr>::A`, `T` a
    /// parameter): the placeholders above, exactly the signature's word. Anything else that
    /// cannot be laid (no row, an unanswered or disagreeing row, a head not read, a binding
    /// that does not map) is placeholders noted as stricter ([`Form::ProjectionUnbound`] in
    /// [`Lowered::coarse`]): they read regions the impl ties as unrelated, so a body with a
    /// finding is refused by that name, never given the finding (rule 3).
    ///
    /// In the rule's grain (grain.rs `Grain::lay`): this walk under the positions; under the
    /// slot grain grain.rs `lay_slots`, the same rule over every slot.
    fn lay_over(&self, declared: PTy, body_t: PTy, slots: &[Region], out: &mut Vec<Option<Region>, A>, e: ExprId) -> Result<bool, Refusal> {
        <R::Grain as grain::Grain>::lay(self, declared, body_t, slots, out, e)
    }

    /// [`Lower::lay_over`]; `through_impls` false inside an impl's value (its projections are
    /// not the signature's, so the signature's rows do not answer them: a normalized one there
    /// is refused, `ProjectionUnbound::InsideImplValue`).
    fn lay_over_in(&self, declared: PTy, body_t: PTy, slots: &[Region], out: &mut Vec<Option<Region>, A>, through_impls: bool, e: ExprId) -> Result<bool, Refusal> {
        use blaze_borrow::RegionTypes;
        let mut laid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        if !self.lay_positions(declared, slots, &mut laid, e)? {
            return Ok(false);
        }
        out.clear();
        let facts: &'b F = self.facts;
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        // The cursor into `slots` (the column's pre-order: a node's own slots, then its
        // children's in order, as this walk visits them). `Err(why)` once a count is not known:
        // then no projection after it is laid through its impl (placeholders noted
        // `SlotsUncounted { why }`).
        let mut sc: Result<usize, Uncounted> = Ok(0);
        let mut stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        stack.push((declared, body_t));
        let mut at = 0usize;
        let mut steps = 0u32;
        while let Some((d, b)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let (dn, bn) = (self.head_node(d, e)?, self.node(b, e)?);
            // The slots of `d` this walk does not descend into (all of them for a node it
            // does not open), and its own (for one it opens).
            let here = sc;
            let opens = matches!(
                (dn, bn),
                (PNode::Ref(..), PNode::Ref(..))
                    | (PNode::Ptr(..), PNode::Ptr(..))
                    | (PNode::Slice(_), PNode::Slice(_))
                    | (PNode::Pat(..), PNode::Pat(..))
                    | (PNode::Array(..), PNode::Array(..))
                    | (PNode::Tuple(_), PNode::Tuple(_))
                    | (PNode::Union(_), PNode::Union(_))
                    | (PNode::FnPtr(..), PNode::FnPtr(..))
                    | (PNode::Adt(..), PNode::Adt(..))
            ) || matches!((dn, bn), (PNode::Record(s1, _), PNode::Record(s2, _)) if s1 == s2);
            let step = if opens { heads.own(d) } else { heads.count(d) };
            sc = match (sc, step) {
                (Ok(s), Ok(n)) => s.checked_add(n.get() as usize).ok_or(Uncounted::Overflow),
                (Err(why), _) => Err(why),
                (Ok(_), Err(unknown)) => Err(Uncounted::of(unknown)),
            };
            match (dn, bn) {
                (PNode::Opaque(..) | PNode::Alias { .. }, PNode::Opaque(..) | PNode::Alias { .. }) => {
                    let Some(&r) = laid.get(at) else { return Ok(false) };
                    out.push(r);
                    at += 1;
                }
                (PNode::Opaque(..) | PNode::Alias { .. }, _) => {
                    if laid.get(at).is_none() {
                        return Ok(false);
                    }
                    at += 1;
                    let before = out.len();
                    let mine = match (here, sc) {
                        (Ok(lo), Ok(hi)) => slots.get(lo..hi).ok_or(Uncounted::ColumnShort { lo: lo as u32, hi: hi as u32, len: slots.len() as u32 }),
                        (Err(why), _) | (_, Err(why)) => Err(why),
                    };
                    // An opaque type keeps its placeholders (only its bounds are known); a
                    // projection is laid through its selected impl, placeholders where it is
                    // `Selected::Generic` (exact), and placeholders NOTED as stricter where it
                    // could not be laid (`Lower::unbound`: refused only on a finding).
                    let through = match dn {
                        PNode::Alias { trait_, name, args } => match mine {
                            _ if !through_impls => self.unbound(trait_, ProjectionUnbound::InsideImplValue, e),
                            Err(why) => self.unbound(trait_, ProjectionUnbound::SlotsUncounted { why }, e),
                            Ok(mine) => self.lay_through_impl(&heads, (trait_, name, args), d, b, mine, out, e)?,
                        },
                        _ => Through::Placeholders,
                    };
                    if through == Through::Placeholders {
                        out.truncate(before);
                        for _ in 0..self.npos(b, e)? {
                            out.push(None);
                        }
                    }
                }
                // A fn item has no position in the flow, nor a type parameter; what the body has
                // in their place (a normalization, an impl's `Self` substituted) is hidden as
                // above: the signature names no region inside it.
                (PNode::FnDef(..), _) | (PNode::Param(_), _) if dn != bn => {
                    for _ in 0..self.npos(b, e)? {
                        out.push(None);
                    }
                }
                (PNode::Ref(_, di), PNode::Ref(_, bi)) => {
                    let Some(&r) = laid.get(at) else { return Ok(false) };
                    out.push(r);
                    at += 1;
                    stack.push((di, bi));
                }
                (PNode::Ptr(_, di), PNode::Ptr(_, bi))
                | (PNode::Slice(di), PNode::Slice(bi))
                | (PNode::Pat(di, _), PNode::Pat(bi, _))
                | (PNode::Array(di, _), PNode::Array(bi, _)) => stack.push((di, bi)),
                (PNode::Tuple(dl), PNode::Tuple(bl))
                | (PNode::Union(dl), PNode::Union(bl))
                | (PNode::FnPtr(dl, _), PNode::FnPtr(bl, _)) => {
                    let (ds, bs) = (self.head_list(dl, e)?, self.list(bl, e)?);
                    if ds.len() != bs.len() {
                        return Ok(false);
                    }
                    for (&x, &y) in ds.iter().zip(bs.iter()).rev() {
                        stack.push((x, y));
                    }
                }
                (PNode::Record(ds_schema, dl), PNode::Record(bs_schema, bl)) if ds_schema == bs_schema => {
                    let (ds, bs) = (self.head_list(dl, e)?, self.list(bl, e)?);
                    if ds.len() != bs.len() {
                        return Ok(false);
                    }
                    for (&x, &y) in ds.iter().zip(bs.iter()).rev() {
                        stack.push((x, y));
                    }
                }
                (PNode::Adt(dk, dl), PNode::Adt(bk, bl)) if dk == bk => {
                    for _ in 0..self.adt_facts(dk, e)?.lifetimes {
                        let Some(&r) = laid.get(at) else { return Ok(false) };
                        out.push(r);
                        at += 1;
                    }
                    let (ds, bs) = (self.head_list(dl, e)?, self.list(bl, e)?);
                    if ds.len() != bs.len() {
                        return Ok(false);
                    }
                    for (&x, &y) in ds.iter().zip(bs.iter()).rev() {
                        stack.push((x, y));
                    }
                }
                (PNode::Dynamic(_), PNode::Dynamic(_)) => {
                    let Some(&r) = laid.get(at) else { return Ok(false) };
                    out.push(r);
                    at += 1;
                }
                (
                    PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never | PNode::Param(_),
                    _,
                ) if dn == bn => {}
                (PNode::ConstValue { .. } | PNode::ConstParam(_), PNode::ConstValue { .. } | PNode::ConstParam(_)) => {}
                _ => return Ok(false),
            }
        }
        Ok(at == laid.len())
    }

    /// The impl the type checker selected for the body's own signature's projections onto
    /// `trait_`'s `name`: its rows at the body's root (`Checked::selected_at(root)`, filed by
    /// the type check for the signature it normalized, D-sig-rows). Order-free: EVERY such row
    /// must name the same impl (`Ok(Some(imp))`), or every one be `Selected::Generic`
    /// (`Ok(None)`: placeholders). Anything else, why by name: no row, a `NotImpl` or
    /// `Unanswered` row, rows naming two impls or an impl and `Generic`.
    fn sig_impl(&self, trait_: ItemKey, name: Name) -> Result<Option<ItemKey>, ProjectionUnbound> {
        use blaze_typeck::Selected;
        let mut found: Option<ItemKey> = None;
        let (mut any, mut generic) = (false, false);
        for r in self.ck.selected_at(self.body.root).iter().filter(|r| r.trait_ == trait_ && r.name == Some(name)) {
            any = true;
            match r.selected {
                Selected::Impl { imp, .. } if found.is_none_or(|f| f == imp) => found = Some(imp),
                Selected::Impl { .. } => return Err(ProjectionUnbound::RowsDisagree),
                // FROZEN path (D190): the sources the record now names read as before they
                // were named (`Generic`, `NotImpl`).
                Selected::Generic | Selected::ParamEnv { .. } | Selected::OpaqueBound | Selected::ObjectBound => generic = true,
                Selected::NotImpl | Selected::Builtin => return Err(ProjectionUnbound::RowNotImpl),
                Selected::Unanswered => return Err(ProjectionUnbound::RowUnanswered),
            }
        }
        match (any, generic, found) {
            (true, true, Some(_)) => Err(ProjectionUnbound::RowsDisagree),
            (true, true, None) => Ok(None),
            (true, false, Some(imp)) => Ok(Some(imp)),
            // No row (an `Impl` row always sets `found`).
            (false, ..) | (true, false, None) => Err(ProjectionUnbound::NoRow),
        }
    }

    /// One declared projection `<S as trait_>::name` (`alias`: its trait, name and arguments;
    /// `d` its node, `mine` its slots in the signature's column) laid over the body's
    /// normalization `b` through the selected impl's head record (D148, design B):
    ///
    /// 1. The impl's lifetimes are bound from the alias's self-type slots against the impl's
    ///    self type column (`RegionFacts::impl_header`: `Wrap<'i>` is `[Param(0)]`, so `'i` is
    ///    the alias's `Wrap<'a>` slot, `'a`). The Reference, "Generic implementations" (E0207):
    ///    an impl's lifetime appears in its self type or trait reference; one only in the
    ///    trait's arguments stays unbound (its positions placeholders).
    /// 2. The `type name = U;` value's column (`RegionFacts::impl_assoc`, the impl's numbering)
    ///    is laid over `b` as the flow's positions, each `Param(i)` replaced by its binding,
    ///    `'static` kept, anything unbound a placeholder (`None`).
    ///
    /// [`Through::Placeholders`] exactly where every row is `Selected::Generic`. Placeholders
    /// NOTED as stricter ([`Lower::unbound`]: [`Form::ProjectionUnbound`], each step its
    /// [`ProjectionUnbound`], the body refused by it only if it has a finding): no row or no
    /// impl selected; the impl's head not read; a GAT's own lifetimes (not numbered here); a
    /// self type of another slot count (`impl<T> Tr for Vec<T>` with `Vec<&'a u8>`: `T`'s
    /// regions are not the impl's); one impl lifetime bound to two regions; the value not of
    /// `b`'s shape. Placeholders are sound (they know less) but make false findings there
    /// (`get` above: its `'a` and `x`'s position unrelated, "a lifetime must outlive another"):
    /// those are now the refusal, never a finding.
    ///
    /// UNSURE (not run): which of these steps names front2_impl_reads.rs `get`.
    ///
    /// Read list (rule 21): the impl's `impl_header` and `impl_assoc` are its head record's,
    /// so the impl's regions digest must be in the body's reads (blaze-ladder borrows.rs files
    /// a head read for every impl a `selected` row names: an edit to `type A = ..;` rechecks
    /// the body).
    #[expect(clippy::too_many_arguments, reason = "one projection, every part named")]
    fn lay_through_impl(
        &self,
        heads: &blaze_borrow::Columns<'_>,
        alias: (ItemKey, Name, List),
        d: PTy,
        b: PTy,
        mine: &[Region],
        out: &mut Vec<Option<Region>, A>,
        e: ExprId,
    ) -> Result<Through, Refusal> {
        use blaze_borrow::{Kids, RegionTypes};
        let (trait_, name, _) = alias;
        let imp = match self.sig_impl(trait_, name) {
            Ok(Some(imp)) => imp,
            Ok(None) => return Ok(Through::Placeholders),
            Err(why) => return Ok(self.unbound(trait_, why, e)),
        };
        let regions = self.facts.regions();
        let Ok((self_col, _)) = regions.impl_header(imp) else { return Ok(self.unbound(trait_, ProjectionUnbound::ImplHeaderUnread, e)) };
        let Ok(value) = regions.impl_assoc(imp, name) else { return Ok(self.unbound(trait_, ProjectionUnbound::ImplAssocUnread, e)) };
        let Ok(params) = regions.lifetime_params(imp) else { return Ok(self.unbound(trait_, ProjectionUnbound::ImplParamsUnread, e)) };
        // The alias's own slots (the trait's lifetimes, always none; a GAT's own): a GAT's are
        // not numbered in the impl's value here.
        match heads.own(d) {
            Ok(own) if own.get() == 0 => {}
            _ => return Ok(self.unbound(trait_, ProjectionUnbound::GatLifetimes, e)),
        }
        let Ok(Kids::List(args)) = heads.kids(d) else { return Ok(self.unbound(trait_, ProjectionUnbound::SelfTypeUnread, e)) };
        let Some(&self_ty) = args.first() else { return Ok(self.unbound(trait_, ProjectionUnbound::SelfTypeUnread, e)) };
        let Ok(self_n) = heads.count(self_ty) else { return Ok(self.unbound(trait_, ProjectionUnbound::SelfTypeUnread, e)) };
        let differ = ProjectionUnbound::SelfSlotsDiffer { imp: self_col.as_slice().len() as u32, declared: self_n.get() };
        let Some(decl_self) = mine.get(..self_n.get() as usize) else { return Ok(self.unbound(trait_, differ, e)) };
        if self_col.as_slice().len() != decl_self.len() {
            return Ok(self.unbound(trait_, differ, e));
        }
        let mut bind: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        bind.resize(params.count() as usize, None);
        for (&ir, &dr) in self_col.as_slice().iter().zip(decl_self.iter()) {
            match ir {
                Region::Param(i) => {
                    let Some(slot) = bind.get_mut(i as usize) else { return Ok(self.unbound(trait_, ProjectionUnbound::SelfRegionUnnumbered, e)) };
                    match *slot {
                        Some(x) if x != dr => return Ok(self.unbound(trait_, ProjectionUnbound::BoundTwice, e)),
                        _ => *slot = Some(dr),
                    }
                }
                // `impl Tr for Wrap<'static>`: the alias's region there is shown `'static` by
                // the selection itself; nothing is bound.
                Region::Static => {}
                Region::Late { .. } => return Ok(self.unbound(trait_, ProjectionUnbound::SelfRegionUnnumbered, e)),
            }
        }
        let mut vlaid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        if !self.lay_over_in(value.ty(), b, value.as_slice(), &mut vlaid, false, e)? {
            return Ok(self.unbound(trait_, ProjectionUnbound::ValueUnaligned, e));
        }
        for &v in vlaid.iter() {
            out.push(match v {
                Some(Region::Param(i)) => bind.get(i as usize).copied().flatten(),
                Some(Region::Static) => Some(Region::Static),
                Some(Region::Late { .. }) | None => None,
            });
        }
        Ok(Through::Laid)
    }

    /// A projection of `trait_` that could not be laid through its impl (`why`): its
    /// normalization's positions are placeholders, a STRICTER reading than Rust's (a
    /// placeholder is known to outlive nothing but what outlives `'static`, so knowing less of
    /// it only adds findings), noted as `Form::ProjectionUnbound` ([`Lowered::coarse`]): the
    /// body is refused by that name only if it has a finding.
    fn unbound(&self, trait_: ItemKey, why: ProjectionUnbound, e: ExprId) -> Through {
        self.coarse_step(Form::ProjectionUnbound { trait_, why }, e);
        Through::Placeholders
    }

    /// A placeholder of the body's own signature standing for an opaque type's or a
    /// projection's position ([`Lower::lay_positions`]'s `None`): related to nothing but
    /// `'static`, which outlives it. Knowing less of a placeholder is never unsound.
    fn hidden_placeholder(&mut self) -> Origin {
        let st = self.universal(Region::Static);
        let o = self.b.origin(true);
        self.b.known(st, o);
        o
    }

    /// The one region of the body's future (an `async fn`'s): a placeholder that each lifetime
    /// the declared opaque output captures (`captured`, the output child's slots) outlives,
    /// and `'static` too. No relation between two captured lifetimes is made.
    fn future_region(&mut self, captured: &[Region]) -> Origin {
        let o = self.hidden_placeholder();
        if self.plant == Plant::NoKnown {
            return o;
        }
        for &r in captured {
            // `'static` is known to outlive it already; a `Late` is bound inside the output (a
            // fn pointer's), no lifetime of the fn's.
            if matches!(r, Region::Static | Region::Late { .. }) {
                continue;
            }
            let c = self.universal(r);
            self.b.known(c, o);
        }
        o
    }

    /// The origin a callee's region takes at one call: `'static` is the body's; every other is
    /// fresh for the call.
    fn instance(&mut self, inst: &mut Vec<(Region, Origin), A>, r: Region) -> Origin {
        if r == Region::Static && self.plant != Plant::StaticIgnored {
            return self.universal(r);
        }
        if let Some(&(_, o)) = inst.iter().find(|(x, _)| *x == r) {
            return o;
        }
        let o = self.b.origin(false);
        inst.push((r, o));
        o
    }

    /// Relates a call's operands and destination through the callee's lifetime part, at the
    /// call statement just added: each operand's positions flow into the regions of its
    /// parameter, the output's regions into the destination's positions (the first position of
    /// a reference one way, every other both ways, as [`Self::relate`] does); the declared
    /// bounds and the bounds implied by the parameter and output types hold among the regions.
    /// `false`, relating nothing, when the positions do not line up one for one or a region is
    /// hidden: the caller takes its older, coarser path.
    /// `tie`: the callee and its type arguments, whose trait lifetimes are tied to the caller's
    /// regions (trait_ties.rs); `None` for a value callee.
    fn call_by_signature(&mut self, life: SigLife<'_>, tie: Option<(ItemKey, List)>, vs: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        self.call_by_signature_to(life, CallValue::Returned, tie, vs, dest, dest_ty, e)
    }

    /// [`Self::call_by_signature`] with the destination holding the call's `value`
    /// ([`CallValue`]): the call's own output, or what an `.await` of an `async fn`'s call
    /// yields (await_call.rs `relate_awaited`), the written return type's column taking the
    /// output's place.
    #[expect(clippy::too_many_arguments, reason = "call_by_signature's inputs and the value")]
    fn call_by_signature_to<'s>(&mut self, life: SigLife<'s>, value: CallValue<'s>, tie: Option<(ItemKey, List)>, vs: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        // The column's positions, laid as the flow lays the call's: each input's then the
        // output's, one region per position (`regions`), input `i`'s ending at `ends[i]`. A
        // child the flow does not lay as the column does (`lay_positions`: an opaque type or a
        // projection, two subtrees it does not descend into) is related per call or refused by
        // the caller, never here by a guess.
        if !life.well_formed() || life.inputs() as usize != vs.len() {
            return Ok(false);
        }
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut laid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        for k in 0..=life.inputs() {
            let slots = if k < life.inputs() { life.input(k) } else { life.value(value) };
            let Some(slots) = slots else { return Ok(false) };
            let declared = self.value_declared(life, value, k, e)?;
            if !self.lay_positions(declared, slots, &mut laid, e)? {
                return Ok(false);
            }
            for &r in laid.iter() {
                let Some(r) = r else { return Ok(false) };
                regions.push(r);
            }
            if k < life.inputs() {
                ends.push(regions.len() as u32);
            }
        }
        let tie = tie.map(|(item, g)| (item, g, life));
        self.call_laid(Laid { regions: &regions, ends: &ends, bounds: life.bounds() }, tie, &[], vs, dest, dest_ty, e)
    }

    /// Relates a call through a signature already laid as the flow's positions ([`Laid`]),
    /// with some of the callee's regions already the caller's origins (`seed`): a call through
    /// a fn-pointer parameter, whose free regions are that parameter's own positions
    /// (`call_through_value`); every other `Param` and each `Late` is one fresh origin a call
    /// ([`Lower::instance`]), `'static` the body's. `tie`: the callee, its type arguments and
    /// its signature, for its trait lifetimes (trait_ties.rs).
    #[expect(clippy::too_many_arguments, reason = "call_by_signature's inputs and the seed")]
    fn call_laid(&mut self, laid: Laid<'_>, tie: Option<(ItemKey, List, SigLife<'_>)>, seed: &[(Region, Origin)], vs: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        let Laid { regions, ends, bounds } = laid;
        if ends.len() != vs.len() {
            return Ok(false);
        }
        let mut lo = 0usize;
        for (i, v) in vs.iter().enumerate() {
            let hi = ends[i] as usize;
            // Each operand's positions, counted in the body's own table, are the column's
            // child's (laid in the head records' table, `lay_positions`): a place's by its
            // info, a constant's by its type.
            let n = match v.at {
                At::Place(p) => {
                    let s = self.info(p);
                    if !s.exact {
                        return Ok(false);
                    }
                    s.len as usize
                }
                At::Const => self.npos_d(v.ty, e)? as usize,
            };
            if hi < lo || n != hi - lo {
                return Ok(false);
            }
            lo = hi;
        }
        let d = self.info(dest);
        if !d.exact || d.len as usize != regions.len() - lo {
            return Ok(false);
        }
        let out_at = lo;
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        inst.extend_from_slice(seed);
        if let Some((item, g, life)) = tie {
            let args: &'c [PTy] = self.list(g, e)?;
            let mut impl_ties = trait_ties::ImplTies::new_in(self.alloc);
            self.tie_bounds(item, life, args, &mut inst, &mut impl_ties, e)?;
            // No type argument here holds a position (the caller's condition for this path),
            // so an impl's answer names none: a row that does is malformed.
            self.tie_impl_relations(&impl_ties, &mut inst, |_, _, _| None, e)?;
        }
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        let mut vars: Vec<Variance, A> = Vec::new_in(self.alloc);
        lo = 0;
        for i in 0..=vs.len() {
            // The operands, then the destination.
            let (place, ty, into_callee) = if i < vs.len() {
                let v = vs[i];
                match v.at {
                    At::Place(p) => (p, v.ty, true),
                    At::Const => {
                        lo = ends[i] as usize;
                        continue;
                    }
                }
            } else {
                // INVARIANT (D148): an output region that the column also gives an input
                // passed as a place was instanced by that input, so the result is tied to
                // what the argument holds. A result position of such a region with no instance
                // yet would be a fresh origin related to no argument: refused by name, never
                // a silent pass. (A region of no input is the caller's choice, free: `fn
                // f<'a>() -> &'a u8`.)
                for k in out_at..regions.len() {
                    let r = regions[k];
                    if !matches!(r, Region::Param(_)) || inst.iter().any(|(x, _)| *x == r) {
                        continue;
                    }
                    let mut from = 0usize;
                    let mut held = false;
                    for (j, v) in vs.iter().enumerate() {
                        let to = ends[j] as usize;
                        held |= matches!(v.at, At::Place(_)) && regions.get(from..to).is_some_and(|s| s.contains(&r));
                        from = to;
                    }
                    if held {
                        return self.refuse(Form::CallOutputUntied { slot: (k - out_at) as u32 }, e);
                    }
                }
                lo = out_at;
                (dest, dest_ty, false)
            };
            let s = self.info(place);
            // An argument's type is a subtype of the callee's input there, the callee's output
            // of the destination's: each position by its variance (variance.rs).
            self.variances_for(ty, s.len, &mut vars, e)?;
            for k in 0..s.len {
                let here = self.origs[(s.start + k) as usize];
                let there = self.instance(&mut inst, regions[lo + k as usize]);
                let (from, into) = if into_callee { (here, there) } else { (there, here) };
                self.relate_one(from, into, vars[k as usize]);
            }
            self.ref_ranges(ty, e, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner) = ranges[j];
                let outer = self.instance(&mut inst, regions[lo + at as usize]);
                for k in 1..=inner {
                    let o = self.instance(&mut inst, regions[lo + (at + k) as usize]);
                    if o != outer {
                        self.b.subset(o, outer);
                    }
                }
            }
            if i < vs.len() {
                lo = ends[i] as usize;
            }
        }
        // The declared clauses `(longer, shorter)`, demanded of the call's instances.
        for &(a, b) in bounds {
            let (oa, ob) = (self.instance(&mut inst, a), self.instance(&mut inst, b));
            self.b.subset(oa, ob);
        }
        Ok(true)
    }

    /// Whether every child of `life`'s column is laid by the flow as the column lays it with
    /// no opaque or projection position ([`Lower::lay_positions`] all `Some`): a head that is
    /// not holds a hidden position, related per call (hidden_call.rs) or refused, never by its
    /// column read as positions.
    fn sig_laid(&self, life: SigLife<'_>, e: ExprId) -> Result<bool, Refusal> {
        if !life.well_formed() {
            return Ok(false);
        }
        let mut laid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        for k in 0..=life.inputs() {
            let slots = if k < life.inputs() { life.input(k) } else { life.output() };
            let Some(slots) = slots else { return Ok(false) };
            let declared = self.sig_declared(life, k, e)?;
            if !self.lay_positions(declared, slots, &mut laid, e)? || laid.iter().any(Option::is_none) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn step(&mut self, t: Task) -> Result<(), Refusal> {
        match t {
            Task::Eval(e, m) => self.eval(e, m),
            Task::Adjust(e, m) => self.adjust(e, m),
            Task::Stmt(s) => self.stmt(s),
            Task::OpenTemps => {
                self.temp_marks.push(self.temps.len() as u32);
                Ok(())
            }
            Task::CloseTemps(e) => {
                let mark = self.temp_marks.pop().unwrap_or(0) as usize;
                for i in (mark..self.temps.len()).rev() {
                    let l = self.temps[i];
                    self.drop_local(l, e)?;
                }
                self.temps.truncate(mark);
                Ok(())
            }
            Task::HoldTemps(e) => {
                let Some(mark) = self.temp_marks.pop() else { return self.refuse(Form::Malformed, e) };
                let mark = mark as usize;
                if mark > self.temps.len() {
                    return self.refuse(Form::Malformed, e);
                }
                self.held_marks.push(self.cond_temps.len() as u32);
                for i in mark..self.temps.len() {
                    let l = self.temps[i];
                    self.cond_temps.push(l);
                }
                self.temps.truncate(mark);
                Ok(())
            }
            Task::DropHeld(e, last) => {
                let Some(&mark) = self.held_marks.last() else { return self.refuse(Form::Malformed, e) };
                let mark = mark as usize;
                for i in (mark..self.cond_temps.len()).rev() {
                    let l = self.cond_temps[i];
                    self.drop_local(l, e)?;
                }
                if last {
                    self.cond_temps.truncate(mark);
                    self.held_marks.pop();
                }
                Ok(())
            }
            Task::ArmEndTemps(e, dest, join) => {
                let v = self.pop(e)?;
                if let Some(d) = dest {
                    let dp = self.place_of(d);
                    let dt = self.local_ty[d.0 as usize];
                    self.assign_val(dp, dt, v, e)?;
                }
                // The arm's own temporaries first (`close_arm_temps`), then the scrutinee's.
                self.close_arm_temps(e)?;
                let Some(&mark) = self.temp_marks.last() else { return self.refuse(Form::Malformed, e) };
                for i in (mark as usize..self.temps.len()).rev() {
                    let l = self.temps[i];
                    self.drop_local(l, e)?;
                }
                self.b.goto(join, self.span(e));
                Ok(())
            }
            Task::OpenScope => {
                self.open_scope();
                Ok(())
            }
            Task::CloseScope(e) => self.close_scope(e),
            Task::Start(b) => {
                self.b.start(b);
                Ok(())
            }
            Task::Goto(b, e) => {
                self.b.goto(b, self.span(e));
                Ok(())
            }
            Task::Unreachable(b, e) => {
                self.b.start(b);
                self.b.stmt(StmtKind::Unreachable, self.span(e));
                Ok(())
            }
            Task::PushVal(v) => {
                self.vals.push(v);
                Ok(())
            }
            Task::SettleDest(d, e) => self.settle_dest(d, e),
            Task::Discard => {
                self.vals.pop();
                Ok(())
            }
            // As `Discard`, an absent value (none was pushed) is nothing to mention.
            Task::Mention(e) => match self.vals.pop() {
                Some(v) => self.mention(v, e),
                None => Ok(()),
            },
            Task::DiscardStmt(e) => self.discard_stmt(e),
            Task::Branch(e, t, f) => self.branch(e, t, f),
            Task::BranchOn(e, t, f) => {
                let v = self.pop(e)?;
                let p = self.as_place(v, e)?;
                self.b.switch(p, &[t, f], self.span(e));
                Ok(())
            }
            Task::LetBranch(pat, e, t, f) => {
                let v = self.pop(e)?;
                let p = self.as_place(v, e)?;
                let bind = self.b.block();
                self.b.switch(p, &[bind, f], self.span(e));
                self.b.start(bind);
                self.bind_pattern(pat, Val { at: At::Place(p), ty: v.ty })?;
                self.b.goto(t, self.span(e));
                Ok(())
            }
            Task::TailStore(dest, e) => {
                let v = self.pop(e)?;
                if let Some(d) = dest {
                    let dp = self.place_of(d);
                    let dt = self.local_ty[d.0 as usize];
                    self.assign_val(dp, dt, v, e)?;
                }
                Ok(())
            }
            Task::BlockEnd(e, dest, target) => {
                self.close_scope(e)?;
                if target {
                    let tg = self.targets.pop();
                    if let Some(tg) = tg {
                        self.b.goto(tg.exit, self.span(e));
                        self.b.start(tg.exit);
                    }
                }
                if let Some(d) = dest {
                    self.settle_dest(d, e)?;
                }
                self.push_dest(dest, e)
            }
            Task::ArmEnd(e, dest, join, close) => {
                let v = self.pop(e)?;
                if let Some(d) = dest {
                    let dp = self.place_of(d);
                    let dt = self.local_ty[d.0 as usize];
                    self.assign_val(dp, dt, v, e)?;
                }
                self.close_arm_temps(e)?;
                if close {
                    self.close_scope(e)?;
                }
                self.b.goto(join, self.span(e));
                Ok(())
            }
            Task::AssignConst(d, e) => {
                let p = self.place_of(d);
                self.b.assign(p, Operand::Const, self.span(e));
                Ok(())
            }
            Task::MatchDispatch(e, dest) => self.match_dispatch(e, dest),
            Task::FakeBorrow(e, v) => {
                // A shallow borrow of the scrutinee across the guard, so the guard cannot
                // overwrite what was matched (flow.md section 1).
                let (pty, p) = match v.at {
                    At::Place(p) => (v.ty, p),
                    At::Const => return self.refuse(Form::Malformed, e),
                };
                let t = self.new_local_with(TyD { wrap: Some(Mutbl::Not), ty: pty.ty }, false, None, e)?;
                self.name_local(t, Identity::Temp(e, Role::Borrow));
                self.b.stmt(StmtKind::StorageLive(t), self.span(e));
                self.vars.push(t);
                self.borrow(p, pty, BorrowKind::Shallow, t, e)?;
                self.vals.push(Val { at: At::Place(self.place_of(t)), ty: self.local_ty[t.0 as usize] });
                Ok(())
            }
            Task::FakeUse(_, e) => {
                // The guard's fake borrow is used once the guard is done.
                let v = self.pop(e)?;
                if let At::Place(p) = v.at {
                    self.b.assign(p, Operand::Copy(p), self.span(e));
                }
                Ok(())
            }
            Task::BindPat(p, v) => self.bind_pattern(p, v),
            Task::GuardRefs(p, v, g) => self.bind_walk(p, v, None, BindPass::GuardRefs(g)),
            Task::GuardMoves(p, v) => self.bind_walk(p, v, None, BindPass::GuardMoves),
            Task::LoopBack(e) => {
                self.vals.pop();
                let tg = match self.targets.pop() {
                    Some(t) => t,
                    None => return self.refuse(Form::BreakTarget, e),
                };
                if let Some(h) = tg.head {
                    self.b.goto(h, self.span(e));
                }
                self.b.start(tg.exit);
                if let Some(d) = tg.dest {
                    self.settle_dest(d, e)?;
                }
                self.push_dest(tg.dest, e)
            }
            Task::BreakDone(e, has) => self.break_done(e, has),
            Task::ReturnDone(e, has) => {
                let v = if has { self.pop(e)? } else { Val { at: At::Const, ty: self.local_ty[0] } };
                let rp = self.place_of(Local(0));
                let rt = self.local_ty[0];
                if v.ty != rt {
                    self.closure_sig_kept(v.ty.ty, e)?;
                }
                self.assign_val(rp, rt, v, e)?;
                if let Some(out_o) = self.rpit_out {
                    let ri = self.info(rp);
                    for k in ri.start..ri.start + ri.len {
                        self.b.subset(self.origs[k as usize], out_o);
                    }
                }
                self.exit_drops(0, 0, e)?;
                self.b.goto(self.ret_block, self.span(e));
                self.fresh();
                let ty = TyD::of(self.expr_ty(e)?);
                self.vals.push(Val { at: At::Const, ty });
                Ok(())
            }
            Task::RootEnd(e) => {
                let v = self.pop(e)?;
                let rp = self.place_of(Local(0));
                let rt = self.local_ty[0];
                // A closure returned as another type (an opaque return type): closure.rs
                // `closure_sig_kept`.
                if v.ty != rt {
                    self.closure_sig_kept(v.ty.ty, e)?;
                }
                self.assign_val(rp, rt, v, e)?;
                if let Some(out_o) = self.rpit_out {
                    let ri = self.info(rp);
                    for k in ri.start..ri.start + ri.len {
                        self.b.subset(self.origs[k as usize], out_o);
                    }
                }
                self.exit_drops(0, 0, e)?;
                self.b.goto(self.ret_block, self.span(e));
                self.b.start(self.ret_block);
                self.b.ret(self.span(e));
                Ok(())
            }
            Task::CallDone(e, n, value_callee, mark) => self.call_done(e, n, value_callee, mark),
            Task::Settle(e) => self.settle_operand(e),
            Task::CtorDone(e, n) => self.aggregate(e, n, Agg::Ctor),
            Task::AggDone(e, n) => self.aggregate(e, n, Agg::Other),
            Task::FmtDone(e, n) => self.aggregate(e, n, Agg::Fmt),
            Task::OpDone(e) => self.aggregate(e, 1, Agg::Other),
            Task::BinDone(e, op) => self.bin_done(e, op),
            Task::AssignDone(e) => {
                let lhs = self.pop(e)?;
                let rhs = self.pop(e)?;
                let lp = self.as_place(lhs, e)?;
                if self.needs_drop(lhs.ty, e)? {
                    // The Reference: "Assignment also runs the destructor of its left-hand
                    // operand, if it's initialized."
                    self.b.stmt(StmtKind::Drop(lp), self.span(e));
                }
                self.assign_val(lp, lhs.ty, rhs, e)?;
                self.vals.push(Val { at: At::Const, ty: TyD::of(self.expr_ty(e)?) });
                Ok(())
            }
            Task::AssignOpDone(e) => {
                let rhs = self.pop(e)?;
                let lhs = self.pop(e)?;
                let lp = self.as_place(lhs, e)?;
                let r = self.operand(rhs, e)?;
                self.b.aggregate(lp, &[Operand::Copy(lp), r], self.span(e));
                if let At::Place(rp) = rhs.at {
                    self.relate(rp, lp, lhs.ty, e)?;
                }
                self.vals.push(Val { at: At::Const, ty: TyD::of(self.expr_ty(e)?) });
                Ok(())
            }
            Task::RefDone(e, m) => self.ref_done(e, m),
            Task::RawDone(e) => {
                let v = self.pop(e)?;
                let p = self.as_place(v, e)?;
                let t = self.temp(TyD::of(self.expr_ty(e)?), e)?;
                let tp = self.place_of(t);
                self.b.aggregate(tp, &[Operand::Copy(p)], self.span(e));
                self.push_place(tp, self.local_ty[t.0 as usize]);
                Ok(())
            }
            Task::DerefDone(e, m) => {
                let v = self.pop(e)?;
                let ty = TyD::of(self.expr_ty(e)?);
                // An overloaded `*x` (`Box`, `Rc`, any `Deref`) is the type check's adjustment of
                // the OPERAND (`Deref { overloaded: Some(_) }`, typeck rules/op.rs): the operand's
                // value is already the place `*Deref::deref(&x)` (or `*x` of a box, `deref`),
                // of the expression's type. Derefed again here, `*Box::from_raw(p)` was a deref
                // of the `T` and a move out of it E0507 (lane B11's class H; blaze-build's
                // walk.rs `deref_on_operand` reads the same row).
                if let Expr::Unary { operand, .. } = self.expr(e)? {
                    let (lo, hi) = self.adjust_rows(operand);
                    let ck: &'c Checked<'c> = self.ck;
                    if matches!(ck.adjusts.column::<1>()[lo..hi].last(), Some(AdjustKind::Deref { overloaded: Some(_) })) {
                        let p = self.as_place(v, e)?;
                        self.push_place(p, ty);
                        return Ok(());
                    }
                }
                let p = self.deref(v, ty, m, None, e)?;
                self.push_place(p, ty);
                Ok(())
            }
            Task::FieldDone(e) => self.field_done(e),
            Task::StructBase(e) => self.struct_base(e),
            Task::FieldDefaults(e) => {
                self.field_defaults(e);
                Ok(())
            }
            Task::IndexDone(e, m) => self.index_done(e, m),
            Task::LetInit(s) => self.let_init(s),
            Task::AwaitDone(e) => self.await_done(e),
        }
    }

    fn push_dest(&mut self, dest: Option<Local>, e: ExprId) -> Result<(), Refusal> {
        let ty = TyD::of(self.expr_ty(e)?);
        match dest {
            Some(d) => {
                let p = self.place_of(d);
                self.push_place(p, ty);
            }
            None => self.vals.push(Val { at: At::Const, ty }),
        }
        Ok(())
    }

    /// The result temporary `d` of a block, `if`, `match` or `loop` (`dest_for`), its value
    /// now complete: its drop moves after every temporary its evaluation left in the enclosing
    /// scope. rustc `as_temp` (rustc_mir_build builder/expr/as_temp.rs) schedules the temporary's
    /// storage end before it evaluates the expression into it and its VALUE drop after, so at
    /// the scope's end the value is dropped before the temporaries it was made from: before
    /// 2024 a block's tail temporaries (the Reference, "Temporary scopes"), an `if let`'s or a
    /// `match`'s scrutinee temporaries, a `super let`'s binding. `dest_for` creates `d` before
    /// the operands (its `StorageLive` must come first), so in `temps` it sat below them and was
    /// dropped last: with a `Drop` (every non-`Copy` ADT is taken to need one) its loans of
    /// them were drop-live past their storage end, a false E0505 and E0597 on tracing's
    /// `dispatch(&valueset!(..))`, a 2018 macro's block borrowed for a call. The storage end
    /// moves with the drop: nothing can borrow `d` before its value is complete, and a loan
    /// of it taken after is of a later temporary, dropped before it.
    fn settle_dest(&mut self, d: Local, e: ExprId) -> Result<(), Refusal> {
        if self.plant == Plant::DestDroppedLast {
            return Ok(());
        }
        let Some(at) = self.temps.iter().rposition(|l| l.0 == d.0) else {
            return self.refuse(Form::Malformed, e);
        };
        let l = self.temps.remove(at);
        self.temp_left(at);
        self.temps.push(l);
        Ok(())
    }

    /// The end of an arm's temporary scope (its `OpenTemps` pushed before the arm's `Eval`),
    /// once its value is in the destination: its temporaries dropped, newest first, before the
    /// arm's bindings (`close_scope` after). The Reference, "Temporary scopes": "the body
    /// expression for a match arm", "the body of an `if`", "the `else` block of an `if`" are
    /// temporary scopes in every edition (rustc region.rs: the arm and both `if` branches are
    /// terminating scopes). Before, an arm's temporaries lived to the enclosing statement's
    /// end: `look(match 0 { _ => make(&mk()) })`, E0716 in rustc, was accepted.
    /// CONTROL: `Plant::ArmTempsToStatement` keeps them to the statement's end (the mark is
    /// popped, its temporaries left to the scope below).
    fn close_arm_temps(&mut self, e: ExprId) -> Result<(), Refusal> {
        let Some(mark) = self.temp_marks.pop() else { return self.refuse(Form::Malformed, e) };
        let mark = mark as usize;
        if mark > self.temps.len() {
            return self.refuse(Form::Malformed, e);
        }
        if self.plant == Plant::ArmTempsToStatement {
            return Ok(());
        }
        for i in (mark..self.temps.len()).rev() {
            let l = self.temps[i];
            self.drop_local(l, e)?;
        }
        self.temps.truncate(mark);
        Ok(())
    }

    /// A variable scope opens: its variables start at `vars`' end, and what `temps` and
    /// `temp_marks` hold now is around it (`scope_temps`).
    pub(crate) fn open_scope(&mut self) {
        self.scope_marks.push(self.vars.len() as u32);
        self.scope_temps.push((self.temps.len() as u32, self.temp_marks.len() as u32));
    }

    /// `temps[at]` leaves its place (removed, or moved to the end once its value is whole): a
    /// scope opened after it was made has one fewer temporary around it.
    fn temp_left(&mut self, at: usize) {
        for s in self.scope_temps.iter_mut() {
            if s.0 as usize > at {
                s.0 -= 1;
            }
        }
    }

    fn close_scope(&mut self, e: ExprId) -> Result<(), Refusal> {
        self.scope_temps.pop();
        let mark = self.scope_marks.pop().unwrap_or(0) as usize;
        for i in (mark..self.vars.len()).rev() {
            let l = self.vars[i];
            self.drop_local(l, e)?;
        }
        self.vars.truncate(mark);
        Ok(())
    }

    fn dest_for(&mut self, e: ExprId) -> Result<Option<Local>, Refusal> {
        let ty = self.expr_ty(e)?;
        if self.is_unit_or_never(ty, e)? {
            return Ok(None);
        }
        Ok(Some(self.temp(TyD::of(ty), e)?))
    }

    /// A binding in this flow, including a capture reached through the environment.
    fn binding_place(&self, p: PatId, e: ExprId) -> Result<(Place, TyD), Refusal> {
        if let Some(at) = self.upvar_place(p) {
            return Ok(at);
        }
        match self.pat_local.get(p.ix()) {
            Some(&l) if l != u32::MAX => Ok((self.place_of(Local(l)), self.local_ty[l as usize])),
            _ => self.refuse(Form::UnknownLocal, e),
        }
    }

    fn eval(&mut self, e: ExprId, m: bool) -> Result<(), Refusal> {
        self.tasks.push(Task::Adjust(e, m));
        if self.place_capture_use(e)? {
            return Ok(());
        }
        let ty = TyD::of(self.expr_ty(e)?);
        let body = self.body;
        match self.expr(e)? {
            Expr::Missing => return self.refuse(Form::Missing, e),
            Expr::Lit(_) | Expr::OffsetOf { .. } | Expr::ConstBlock { .. } => self.vals.push(Val { at: At::Const, ty }),
            Expr::Local(p) => {
                let (pl, lt) = self.binding_place(p, e)?;
                self.push_place(pl, lt);
            }
            Expr::Path(_) => {
                if let Some(Def::Static(_, Mutbl::Mut)) = self.expr_def(e) {
                    return self.refuse(Form::StaticMut, e);
                }
                self.vals.push(Val { at: At::Const, ty });
            }
            Expr::Paren(i) => self.tasks.push(Task::Eval(i, m)),
            Expr::Block { stmts, tail, .. } => {
                let target = self.is_target.get(e.ix()).copied().unwrap_or(false);
                let dest = match tail {
                    Some(_) => self.dest_for(e)?,
                    None => None,
                };
                if target {
                    let exit = self.b.block();
                    self.targets.push(Target { e, head: None, exit, dest, scopes: self.scope_marks.len() as u32, temps: self.temps.len() as u32 });
                }
                self.tasks.push(Task::BlockEnd(e, dest, target));
                if let Some(t) = tail {
                    // The edition of the block's own `{` (rustc `resolve_block`: the block
                    // span's edition): a macro's block is in its defining crate's edition,
                    // whatever the body's (tracing's `valueset!`, 2018, in a 2024 body).
                    let own_temps = body.expr_edition(e) >= Edition::E2024;
                    if own_temps {
                        self.tasks.push(Task::CloseTemps(t));
                    }
                    self.tasks.push(Task::TailStore(dest, t));
                    self.tasks.push(Task::Eval(t, false));
                    if own_temps {
                        self.tasks.push(Task::OpenTemps);
                    }
                }
                let ss = match body.stmt_lists.get(stmts.range()) {
                    Some(s) => s,
                    None => return self.refuse(Form::Malformed, e),
                };
                for &s in ss.iter().rev() {
                    self.tasks.push(Task::Stmt(s));
                }
                self.tasks.push(Task::OpenScope);
            }
            Expr::If { cond, then, else_ } => {
                let dest = self.dest_for(e)?;
                let (tb, eb, join) = (self.b.block(), self.b.block(), self.b.block());
                // Exec order: open scope (the condition's bindings), branch, then-arm, else-arm,
                // join, close scope, the value.
                let value = match dest {
                    Some(d) => Val { at: At::Place(self.place_of(d)), ty },
                    None => Val { at: At::Const, ty },
                };
                // The condition's temporaries (the Reference, "Destructors", "Temporary
                // scopes"; rustc region.rs `resolve_expr`):
                // - a condition with no `let` is a temporary scope of its own in every edition
                //   (rustc's `DropTemps`): its temporaries are dropped before either arm runs
                //   (`HoldTemps`, then `DropHeld` at the start of each arm);
                // - from 2024, "the pattern-matching condition(s) and consequent body of if"
                //   are one (`ScopeData::IfThenRescope`): the scrutinee's temporaries live
                //   through the then-arm and are dropped at its end (`ArmEndTemps`) and before
                //   the else-arm runs (`CloseTemps` at its start);
                // - before 2024 an `if let`'s scrutinee temporaries are the enclosing scope's
                //   (`IfThen`): nothing here.
                // The edition is the `if`'s own (its span's: a macro's `if` is its crate's).
                let has_let = self.cond_has_let(cond)?;
                let rescope = has_let && body.expr_edition(e) >= Edition::E2024;
                self.tasks.push(Task::PushVal(value));
                if let Some(d) = dest {
                    self.tasks.push(Task::SettleDest(d, e));
                }
                self.tasks.push(Task::CloseScope(e));
                self.tasks.push(Task::Start(join));
                match else_ {
                    // Each arm a temporary scope of its own, closed by its `ArmEnd`
                    // (`close_arm_temps`).
                    Some(x) => {
                        self.tasks.push(Task::ArmEnd(x, dest, join, false));
                        self.tasks.push(Task::Eval(x, false));
                        self.tasks.push(Task::OpenTemps);
                    }
                    None => self.tasks.push(Task::Goto(join, e)),
                }
                if rescope {
                    self.tasks.push(Task::CloseTemps(e));
                } else if !has_let {
                    self.tasks.push(Task::DropHeld(e, true));
                }
                self.tasks.push(Task::Start(eb));
                if rescope {
                    self.tasks.push(Task::ArmEndTemps(then, dest, join));
                } else {
                    self.tasks.push(Task::ArmEnd(then, dest, join, false));
                }
                self.tasks.push(Task::Eval(then, false));
                self.tasks.push(Task::OpenTemps);
                if !has_let {
                    self.tasks.push(Task::DropHeld(e, false));
                }
                self.tasks.push(Task::Start(tb));
                if !has_let {
                    self.tasks.push(Task::HoldTemps(e));
                }
                self.tasks.push(Task::Branch(cond, tb, eb));
                if rescope || !has_let {
                    self.tasks.push(Task::OpenTemps);
                }
                self.tasks.push(Task::OpenScope);
            }
            Expr::Let { .. } => return self.refuse(Form::LetOutsideCondition, e),
            Expr::Match { scrutinee, .. } => {
                let dest = self.dest_for(e)?;
                self.tasks.push(Task::MatchDispatch(e, dest));
                self.tasks.push(Task::Eval(scrutinee, false));
            }
            Expr::Loop { body: lb } => {
                let dest = self.dest_for(e)?;
                let (head, exit) = (self.b.block(), self.b.block());
                self.b.goto(head, self.span(e));
                self.b.start(head);
                self.targets.push(Target { e, head: Some(head), exit, dest, scopes: self.scope_marks.len() as u32, temps: self.temps.len() as u32 });
                self.tasks.push(Task::LoopBack(e));
                self.tasks.push(Task::Eval(lb, false));
            }
            Expr::Break { value, .. } => {
                self.tasks.push(Task::BreakDone(e, value.is_some()));
                if let Some(v) = value {
                    self.tasks.push(Task::Eval(v, false));
                }
            }
            Expr::Continue { target } => {
                let tg = match self.targets.iter().rev().find(|t| t.e == target) {
                    Some(t) => *t,
                    None => return self.refuse(Form::BreakTarget, e),
                };
                self.exit_drops(tg.scopes, tg.temps, e)?;
                match tg.head {
                    Some(h) => self.b.goto(h, self.span(e)),
                    None => return self.refuse(Form::BreakTarget, e),
                };
                self.fresh();
                self.vals.push(Val { at: At::Const, ty });
            }
            Expr::Return { value } => {
                self.tasks.push(Task::ReturnDone(e, value.is_some()));
                if let Some(v) = value {
                    self.tasks.push(Task::Eval(v, false));
                }
            }
            Expr::Call { callee, args } => {
                let args = match body.expr_lists.get(args.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                let def = match self.expr(callee)? {
                    Expr::Path(_) => self.expr_def(callee),
                    _ => None,
                };
                let mark = self.two_phase.len() as u32;
                match def {
                    Some(Def::Ctor { .. }) => self.tasks.push(Task::CtorDone(e, args.len() as u32)),
                    Some(Def::Fn(_)) => self.tasks.push(Task::CallDone(e, args.len() as u32, false, mark)),
                    _ => self.tasks.push(Task::CallDone(e, args.len() as u32, true, mark)),
                }
                // Each operand but the last read where it is evaluated ([`Lower::settle_operand`]).
                for (j, &a) in args.iter().enumerate().rev() {
                    if j + 1 < args.len() {
                        self.tasks.push(Task::Settle(a));
                    }
                    self.tasks.push(Task::Eval(a, false));
                }
                if !matches!(def, Some(Def::Ctor { .. } | Def::Fn(_))) {
                    self.tasks.push(Task::Eval(callee, false));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                let args = match body.expr_lists.get(args.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                let mark = self.two_phase.len() as u32;
                self.tasks.push(Task::CallDone(e, 1 + args.len() as u32, false, mark));
                // The receiver, then each operand but the last, read where it is evaluated
                // ([`Lower::settle_operand`]).
                for (j, &a) in args.iter().enumerate().rev() {
                    if j + 1 < args.len() {
                        self.tasks.push(Task::Settle(a));
                    }
                    self.tasks.push(Task::Eval(a, false));
                }
                if !args.is_empty() {
                    self.tasks.push(Task::Settle(receiver));
                }
                let (lo, hi) = self.adjust_rows(receiver);
                let recv_mut = self.ck.adjusts.column::<1>()[lo..hi]
                    .iter()
                    .any(|k| matches!(k, AdjustKind::Borrow { mutbl: Mutbl::Mut, .. } | AdjustKind::Deref { overloaded: Some(Mutbl::Mut) }));
                self.tasks.push(Task::Eval(receiver, recv_mut));
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                self.tasks.push(Task::FieldDone(e));
                self.tasks.push(Task::Eval(base, m));
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                self.tasks.push(Task::DerefDone(e, m));
                self.tasks.push(Task::Eval(operand, m));
            }
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. } => {
                self.tasks.push(Task::OpDone(e));
                self.tasks.push(Task::Eval(operand, false));
            }
            Expr::Binary { op: BinOp::And | BinOp::Or, .. } => {
                let d = self.temp(ty, e)?;
                let (tb, fb, join) = (self.b.block(), self.b.block(), self.b.block());
                self.tasks.push(Task::PushVal(Val { at: At::Place(self.place_of(d)), ty }));
                self.tasks.push(Task::Start(join));
                self.tasks.push(Task::Goto(join, e));
                self.tasks.push(Task::AssignConst(d, e));
                self.tasks.push(Task::Start(fb));
                self.tasks.push(Task::Goto(join, e));
                self.tasks.push(Task::AssignConst(d, e));
                self.tasks.push(Task::Start(tb));
                self.tasks.push(Task::Branch(e, tb, fb));
            }
            Expr::Binary { op, lhs, rhs } => {
                self.tasks.push(Task::BinDone(e, op));
                self.tasks.push(Task::Eval(rhs, false));
                self.tasks.push(Task::Eval(lhs, false));
            }
            Expr::Assign { lhs, rhs } => {
                if matches!(self.expr(lhs)?, Expr::Underscore) {
                    // The Reference, "Underscore expressions": `_ = rhs` "ignores" the value,
                    // as `let _ = rhs;` does: the place is mentioned, not read.
                    self.tasks.push(Task::PushVal(Val { at: At::Const, ty }));
                    self.tasks.push(Task::Mention(rhs));
                    self.tasks.push(Task::Eval(rhs, false));
                } else {
                    if !self.is_place_expr(lhs)? {
                        return self.refuse(Form::DestructuringAssign, e);
                    }
                    // The Reference, "Assignment expressions": "evaluates its right hand operand
                    // first, then the left hand place". Materialize `rhs` before evaluating `lhs`
                    // so `funcs[kept] = funcs[at]` ends the shared borrow of `funcs` before
                    // `IndexMut` borrows `funcs` mutably.
                    self.tasks.push(Task::AssignDone(e));
                    self.tasks.push(Task::Eval(lhs, true));
                    self.tasks.push(Task::Settle(rhs));
                    self.tasks.push(Task::Eval(rhs, false));
                }
            }
            Expr::AssignOp { lhs, rhs, .. } => {
                self.tasks.push(Task::AssignOpDone(e));
                self.tasks.push(Task::Eval(rhs, false));
                self.tasks.push(Task::Eval(lhs, true));
            }
            // The Reference, "Constant promotion": a borrow the checker promoted
            // (`Checked::promoted`) is of a `'static` slot: no loan of the body's, and the
            // operand's code does not run here (the builder reads the evaluator's value).
            Expr::Ref { mutbl: Mutbl::Not, .. } if self.ck.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok() => {
                let ty = TyD::of(self.expr_ty(e)?);
                let t = self.temp(ty, e)?;
                let tp = self.place_of(t);
                self.b.assign(tp, Operand::Const, self.span(e));
                self.push_place(tp, ty);
            }
            // The Reference, "Constant promotion": a borrow the checker promoted
            // (`Checked::promoted`) is of a `'static` slot: no loan of the body's, and the
            // operand's code does not run here (the builder reads the evaluator's value).
            Expr::Ref { mutbl: Mutbl::Not, .. } if self.ck.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok() => {
                let ty = TyD::of(self.expr_ty(e)?);
                let t = self.temp(ty, e)?;
                let tp = self.place_of(t);
                self.b.assign(tp, Operand::Const, self.span(e));
                self.push_place(tp, ty);
            }
            Expr::Ref { mutbl, operand } => {
                self.tasks.push(Task::RefDone(e, mutbl));
                self.tasks.push(Task::Eval(operand, mutbl == Mutbl::Mut));
            }
            Expr::RawRef { mutbl, operand } => {
                self.tasks.push(Task::RawDone(e));
                self.tasks.push(Task::Eval(operand, mutbl == Mutbl::Mut));
            }
            Expr::Tuple { elems } | Expr::Array { elems } => {
                let es = match body.expr_lists.get(elems.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                self.tasks.push(Task::AggDone(e, es.len() as u32));
                // Each operand but the last read where it is evaluated ([`Lower::settle_operand`]).
                for (j, &a) in es.iter().enumerate().rev() {
                    if j + 1 < es.len() {
                        self.tasks.push(Task::Settle(a));
                    }
                    self.tasks.push(Task::Eval(a, false));
                }
            }
            Expr::Repeat { elem, .. } => {
                self.tasks.push(Task::AggDone(e, 1));
                self.tasks.push(Task::Eval(elem, false));
            }
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                let fs = match body.field_inits.get(fields.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                // A struct update (`S { a: x, ..base }`, the Reference, "Functional update
                // syntax"): the written fields in source order, then the base as a place, then
                // one operand per field the update takes from it (`Checked::fru_fields`), each
                // `base.f` moved or copied by its own type as `let x = base.f;` is: a partial
                // move, so the written fields of a place base stay usable. A temporary base is
                // taken the same way: its fields not taken are still dropped with it.
                // A base-less `S { a: 1, .. }` (RFC 3681): each unwritten field is its default,
                // a constant (`Checked::default_fields`): a value of no place, no borrow and no
                // move; its operand is a constant of the field's type (`fru_fields` row).
                let n = match base {
                    blaze_typeck::StructBase::Expr(_) => fs.len() as u32 + self.ck.fru_fields_of(e).0.len() as u32,
                    blaze_typeck::StructBase::Defaults => fs.len() as u32 + self.ck.default_fields_of(e).0.len() as u32,
                    blaze_typeck::StructBase::None => fs.len() as u32,
                };
                self.tasks.push(Task::AggDone(e, n));
                if base == blaze_typeck::StructBase::Defaults {
                    self.tasks.push(Task::FieldDefaults(e));
                }
                if let Some(b) = base.expr() {
                    self.tasks.push(Task::StructBase(e));
                    self.tasks.push(Task::Eval(b, false));
                }
                // Each written field's value but the last read where it is evaluated
                // ([`Lower::settle_operand`]).
                for (j, f) in fs.iter().enumerate().rev() {
                    if j + 1 < fs.len() {
                        self.tasks.push(Task::Settle(f.expr));
                    }
                    self.tasks.push(Task::Eval(f.expr, false));
                }
            }
            Expr::Index { base, index } => {
                self.tasks.push(Task::IndexDone(e, m));
                self.tasks.push(Task::Eval(index, false));
                self.tasks.push(Task::Eval(base, m));
            }
            // An async closure is a closure VALUE (the Reference, "Closure expressions", "Async
            // closures", RFC 3668): its captures are taken as a closure's (the type check's
            // capture rows for it), and calling it returns a future of its body, a coroutine
            // body of its own. The witness lowering (`R::RELATES` false: which locals an
            // enclosing future stores across a suspension, no region related) needs only that
            // value: laid as any closure's, its captures alone (closure.rs `owner_sig_count` is
            // 0 there). The borrow lowering needs what the body requires of its captures, which
            // no lowering of an async closure's body gives yet (closure.rs
            // `lower_closure_with`): refused there by name, never related as if it needed
            // nothing. tempo bin/tempo/src/tempo_cmd.rs `Info::run`, `ClaimFees::run` (an
            // EXTERNAL_WITNESS_GAP at `execute`), crates/node/src/rpc/simulate.rs.
            Expr::Closure { is_async: true, .. } if !R::RELATES => self.closure_value(e)?,
            Expr::Closure { is_async: true, .. } => return self.refuse(Form::AsyncClosure, e),
            Expr::Closure { .. } => {
                if !self.facts.closure_bodies_checked() {
                    return self.refuse(Form::Closure, e);
                }
                self.closure_value(e)?;
            }
            // An async block is the aggregate of its captures, as a closure is (closure_value);
            // its body is checked on its own (`lower_async`), or the block is refused.
            Expr::AsyncBlock { .. } => {
                if !self.facts.async_bodies_checked() {
                    return self.refuse(Form::AsyncBlock, e);
                }
                self.closure_value(e)?;
            }
            // `.await` only inside the body `lower_async` lowers (the Reference, "Await
            // expressions": "only allowed within an async context").
            Expr::Await(op) => {
                if self.async_root.is_none() {
                    return self.refuse(Form::Await, e);
                }
                // The operand under its parentheses: the call `awaited_shadow` looks for.
                let mut call = op;
                for _ in 0..body.exprs.len() {
                    let Expr::Paren(i) = self.expr(call)? else { break };
                    call = i;
                }
                self.awaiting.push((call, e));
                self.tasks.push(Task::AwaitDone(e));
                self.tasks.push(Task::Eval(op, false));
            }
            Expr::Underscore => return self.refuse(Form::Malformed, e),
            Expr::Asm { .. } => return self.refuse(Form::InlineAsm, e),
            Expr::FormatArgs { args, .. } => {
                let es = match body.expr_lists.get(args.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                self.tasks.push(Task::FmtDone(e, es.len() as u32));
                for &a in es.iter().rev() {
                    self.tasks.push(Task::Eval(a, false));
                }
            }
        }
        Ok(())
    }

    fn is_place_expr(&self, e: ExprId) -> Result<bool, Refusal> {
        let mut e = e;
        for _ in 0..self.body.exprs.len() + 1 {
            match self.expr(e)? {
                Expr::Paren(i) => e = i,
                Expr::Local(_) | Expr::Field { .. } | Expr::TupleField { .. } | Expr::Index { .. } | Expr::Unary { op: UnOp::Deref, .. } | Expr::Path(_) => return Ok(true),
                _ => return Ok(false),
            }
        }
        self.refuse(Form::Bound, e)
    }

    /// `v`, the value of `e`, evaluated and discarded unread (`let _ = e;`, `_ = e;`): when `e`
    /// is a place expression, the place is mentioned (blaze-flow `StmtKind::Mention`: no access,
    /// no move; a liveness use of its local when it passes through a `Deref`). The Reference,
    /// "Wildcard pattern": it "does not copy, move or borrow the value it matches"; "Place
    /// expressions and value expressions": the place expression is still evaluated, and
    /// evaluating `*r` dereferences `r`. A value expression's temporary is left to its scope.
    fn mention(&mut self, v: Val, e: ExprId) -> Result<(), Refusal> {
        if self.plant == Plant::MentionNothing || !self.is_place_expr(e)? {
            return Ok(());
        }
        if let At::Place(p) = v.at {
            self.b.mention(p, self.span(e));
        }
        Ok(())
    }

    /// An expression statement's value discarded (`e;`). The Reference, "Expression
    /// statements": it "evaluates an expression and ignores its result"; "Place expressions and
    /// value expressions": a place expression in a value context is read, "copied" or "moved"
    /// (`s;` moves a `String`, `*r;` of a `&String` is a move out of a borrow). So a place
    /// expression's value is read into a temporary, closed with the statement's temporaries;
    /// any other value was already made in its own temporary.
    fn discard_stmt(&mut self, e: ExprId) -> Result<(), Refusal> {
        let Some(v) = self.vals.pop() else {
            return Ok(());
        };
        if self.plant == Plant::MentionNothing || !self.is_place_expr(e)? {
            return Ok(());
        }
        if let At::Place(_) = v.at {
            let t = self.temp(v.ty, e)?;
            let tp = self.place_of(t);
            self.assign_val(tp, v.ty, v, e)?;
        }
        Ok(())
    }

    /// Whether pattern `p` binds nothing (`_`, `(_, _)`, `S { .. }`): then `let p = place;`
    /// reads nothing of the place, which is only mentioned ([`Self::mention`]).
    /// Whether `p` is an EXTENDING pattern (the Reference, "Temporary lifetime extension",
    /// "Extending based on patterns"): "an identifier pattern that binds by reference or mutable
    /// reference", or a struct, tuple, tuple struct or slice pattern "where at least one of the
    /// direct subpatterns is an extending pattern"; a binding's `@` subpattern counts as its own
    /// (rustc region.rs `is_binding_pat`). The `ref` must be written: a binding by reference that
    /// match ergonomics chose binds a part of a reference, not of a temporary. An `|` pattern, a
    /// `&` pattern and every other form do not extend.
    fn pat_extends(&self, p: PatId) -> Result<bool, Refusal> {
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc);
        stack.push(p);
        let mut steps = 0usize;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, p);
            }
            let pat = match self.body.pats.get(p.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            match pat {
                Pat::Bind { ann, sub, .. } => {
                    if ann.by_ref.is_some() {
                        return Ok(true);
                    }
                    if let Some(s) = sub {
                        stack.push(s);
                    }
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => match self.body.pat_lists.get(elems.range()) {
                    Some(l) => stack.extend_from_slice(l),
                    None => return self.refuse_pat(Form::Malformed, p),
                },
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => match self.body.field_pats.get(fields.range()) {
                    Some(fs) => {
                        for f in fs {
                            stack.push(f.pat);
                        }
                    }
                    None => return self.refuse_pat(Form::Malformed, p),
                },
                Pat::Missing
                | Pat::Wild
                | Pat::Ref { .. }
                | Pat::Lit(_)
                | Pat::NegLit(_)
                | Pat::Path(_)
                | Pat::Or { .. }
                | Pat::Range { .. } => {}
            }
        }
        Ok(false)
    }

    fn binds_nothing(&self, p: PatId) -> Result<bool, Refusal> {
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc);
        stack.push(p);
        let mut steps = 0usize;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, p);
            }
            let pat = match self.body.pats.get(p.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            if let Pat::Bind { .. } = pat {
                return Ok(false);
            }
            self.sub_pats(pat, &mut stack)?;
        }
        Ok(true)
    }

    /// The expression's adjustments, in order, on its value.
    fn adjust(&mut self, e: ExprId, m: bool) -> Result<(), Refusal> {
        let (lo, hi) = self.adjust_rows(e);
        if lo == hi {
            return Ok(());
        }
        let mut v = self.pop(e)?;
        for i in lo..hi {
            let kind = self.ck.adjusts.column::<1>()[i];
            let out = TyD::of(self.ck.adjusts.column::<2>()[i]);
            v = match kind {
                AdjustKind::NeverToAny => Val { at: v.at, ty: out },
                AdjustKind::Deref { overloaded } => {
                    let p = self.deref(v, out, overloaded == Some(Mutbl::Mut), overloaded, e)?;
                    Val { at: At::Place(p), ty: out }
                }
                AdjustKind::Borrow { raw: true, .. } => {
                    let p = self.as_place(v, e)?;
                    let t = self.temp(out, e)?;
                    self.name_local(t, Identity::Temp(e, Role::Borrow));
                    let tp = self.place_of(t);
                    self.b.aggregate(tp, &[Operand::Copy(p)], self.span(e));
                    Val { at: At::Place(tp), ty: out }
                }
                // A shared autoref of a constant is a borrow of a `'static` slot, as `&C` is
                // (`ref_done`, the Reference, "Constant promotion"): no loan of the body's.
                // Through a temporary, `RNG.with_borrow_mut(..)` (a `thread_local!` is a `const`
                // `LocalKey` whose methods take `&'static self`) tied the temporary's loan to
                // `'static` (tempo test_utils.rs:32, a false E0505 and E0597).
                AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. }
                    if matches!(v.at, At::Const)
                        || (i == lo
                            && !matches!(self.body.exprs.get(e.ix()), Some(Expr::Ref { .. }))
                            && self.ck.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok()) =>
                {
                    let t = self.temp(out, e)?;
                    let tp = self.place_of(t);
                    self.b.assign(tp, Operand::Const, self.span(e));
                    Val { at: At::Place(tp), ty: out }
                }
                AdjustKind::Borrow { mutbl, two_phase, raw: false } => {
                    let p = self.as_place(v, e)?;
                    let t = self.temp(out, e)?;
                    self.name_local(t, Identity::Temp(e, Role::Borrow));
                    let kind = match (mutbl, two_phase) {
                        (Mutbl::Not, _) => BorrowKind::Shared,
                        (Mutbl::Mut, true) => BorrowKind::TwoPhase,
                        (Mutbl::Mut, false) => BorrowKind::Mut,
                    };
                    let loan = self.borrow(p, v.ty, kind, t, e)?;
                    if kind == BorrowKind::TwoPhase {
                        self.two_phase.push(loan);
                    }
                    Val { at: At::Place(self.place_of(t)), ty: out }
                }
                AdjustKind::ReifyFnPointer
                | AdjustKind::ClosureFnPointer
                | AdjustKind::UnsafeFnPointer
                | AdjustKind::MutToConstPointer
                | AdjustKind::Unsize => {
                    if matches!(kind, AdjustKind::ClosureFnPointer | AdjustKind::Unsize) {
                        self.closure_sig_kept(v.ty.ty, e)?;
                    }
                    let op = self.operand(v, e)?;
                    let t = self.temp(out, e)?;
                    let tp = self.place_of(t);
                    self.b.aggregate(tp, &[op], self.span(e));
                    if let At::Place(p) = v.at
                        && !(kind == AdjustKind::Unsize && self.unsize_to_object(p, v.ty, tp, out, e)?)
                    {
                        self.relate(p, tp, out, e)?;
                    }
                    Val { at: At::Place(tp), ty: out }
                }
                AdjustKind::UnionInject { .. } | AdjustKind::UnionWiden | AdjustKind::RecordProject => {
                    let op = self.operand(v, e)?;
                    let t = self.temp(out, e)?;
                    let tp = self.place_of(t);
                    self.b.aggregate(tp, &[op], self.span(e));
                    if let At::Place(p) = v.at {
                        self.relate(p, tp, out, e)?;
                    }
                    Val { at: At::Place(tp), ty: out }
                }
            };
        }
        self.vals.push(v);
        Ok(())
    }

    /// `*v` as a place of type `out`: a builtin deref of a reference or pointer, `DerefBox` of a
    /// `Box`, or `Deref::deref(&v)` / `DerefMut::deref_mut(&mut v)` then the builtin deref of
    /// its result (whose lifetime is the receiver's, by elision).
    fn deref(&mut self, v: Val, out: TyD, m: bool, overloaded: Option<Mutbl>, e: ExprId) -> Result<Place, Refusal> {
        let p = self.as_place(v, e)?;
        // `*b` of a `Box` is builtin whatever the type check recorded (it records the operand's
        // deref as overloaded, `Deref for Box`): the Reference, "Dereference expressions":
        // "On non-pointer types `*x` is equivalent to `*std::ops::Deref::deref(&x)`" except for
        // `Box`, whose deref is a place that may be moved out of ("Moved and copied types").
        if overloaded.is_some()
            && v.ty.wrap.is_none()
            && let PNode::Adt(k, _) = self.node(v.ty.ty, e)?
            && self.adt_facts(k, e)?.is_box
        {
            return self.project(p, v.ty, Proj::DerefBox, out, false, e);
        }
        if overloaded.is_none() {
            if v.ty.wrap.is_some() {
                return self.project(p, v.ty, Proj::Deref, out, false, e);
            }
            match self.node(v.ty.ty, e)? {
                PNode::Ref(..) | PNode::Ptr(..) => return self.project(p, v.ty, Proj::Deref, out, false, e),
                PNode::Adt(k, _) if self.adt_facts(k, e)?.is_box => return self.project(p, v.ty, Proj::DerefBox, out, false, e),
                _ => {}
            }
        }
        // `*ET` of an extending borrow's operand: the overloaded deref borrows the temporary
        // (`&mut *cell.borrow_mut()`'s `RefMut`), so it lives as the borrow's result does.
        self.extend_root(p, e)?;
        let mutbl = if m { Mutbl::Mut } else { Mutbl::Not };
        let bt = self.temp(TyD { wrap: Some(mutbl), ty: v.ty.ty }, e)?;
        self.name_local(bt, Identity::Temp(e, Role::Borrow));
        let kind = if m { BorrowKind::Mut } else { BorrowKind::Shared };
        self.borrow(p, v.ty, kind, bt, e)?;
        let rt = self.temp(TyD { wrap: Some(mutbl), ty: out.ty }, e)?;
        self.name_local(rt, Identity::Temp(e, Role::Result));
        let (bp, rp) = (self.place_of(bt), self.place_of(rt));
        let op = if m { Operand::Move(bp) } else { Operand::Copy(bp) };
        self.b.call(&[op], rp, self.span(e));
        let (bi, ri) = (self.info(bp), self.info(rp));
        let (ob, or) = (self.origs[bi.start as usize], self.origs[ri.start as usize]);
        self.b.subset(ob, or);
        let mapped_deref = if ri.len > 1
            && bi.len > 1
            && let PNode::Adt(adt, _) = self.node(v.ty.ty, e)?
            && let Some(imp) = self.facts.deref_impl(adt)
            && let Some(slots) = self.map_impl_assoc_slots(imp, Name::of("Target"), v.ty.ty, bi.len - 1, ri.len - 1, e)?
        {
            for (k, slot) in slots.iter().enumerate() {
                let to = self.origs[(ri.start + 1 + k as u32) as usize];
                let b = match *slot {
                    generic_call::ImplAssocSlot::SelfPos(pos) => self.origs[(bi.start + 1 + pos) as usize],
                    generic_call::ImplAssocSlot::Static => self.universal(Region::Static),
                };
                self.b.subset(b, to);
                if m {
                    self.b.subset(to, b);
                }
            }
            true
        } else {
            false
        };
        if !mapped_deref {
            self.target_flow(bi, 1, ri, m);
            if bi.len > 2 && ri.len > 1 {
                self.coarse_step(Form::Unified { why: Unified::DerefTarget }, e);
            }
        }
        self.project(rp, self.local_ty[rt.0 as usize], Proj::Deref, out, false, e)
    }

    /// The referent positions of an overloaded deref's or index's result `r` (`&Target`,
    /// `&Output`), related to the positions of `from` past its first `skip` (the receiver's
    /// referent, or the index operand). SOUNDNESS: they were related to nothing, so a `&local`
    /// stored through `v.iter_mut()`'s item or into `v[0]` never reached `v: Vec<&u8>`'s
    /// elements. By E0207 (generic_call.rs, "PROJECTIONS": `Deref` and `Index` declare no
    /// lifetime, and `Target` and `Output` have no generics) every lifetime of the projection
    /// is `'static` or one of `Self`'s or the index type's: each of those positions flows into
    /// each of the result's referent positions, and back when the result is `&mut` (invariant
    /// in its referent).
    /// The unsized coercion of `src: from`, a reference, to `dst: to`, a reference to a trait
    /// object `dyn Tr + 'q` whose every lifetime is its object lifetime (`Tr` declares none,
    /// per [`Facts::object_trait_lifetimes`], and its arguments and bindings hold no position):
    /// `&'r T` to `&'s (dyn Tr + 'q)` asks `'r: 's` and `T: Unsize<dyn Tr + 'q>`, so `T: 'q`
    /// (the Reference, "Type coercions", unsized coercions; "Trait object lifetime bounds"). So
    /// the source's reference flows into the destination's, and each of `T`'s positions into
    /// `'q`, one way. Related both ways (`relate`'s coarse path), `&&self.xs` as `&dyn Debug`
    /// made the loan of the temporary `&self.xs` equal to `'a` of `self: &At<'a>` (tempo class
    /// E, a derived `Debug`: E0597 and a placeholder error). `false`, relating nothing: not
    /// such a coercion, and `relate` applies (a trait with lifetimes keeps the object lifetime
    /// standing for them, every position equal).
    fn unsize_to_object(&mut self, src: Place, from: TyD, dst: Place, to: TyD, e: ExprId) -> Result<bool, Refusal> {
        let (s, d) = (self.info(src), self.info(dst));
        if !s.exact || !d.exact || s.len == 0 || d.len < 2 {
            return Ok(false);
        }
        if !(from.wrap.is_some() || matches!(self.node(from.ty, e)?, PNode::Ref(..))) {
            return Ok(false);
        }
        let obj = match to.wrap {
            Some(_) => to.ty,
            None => match self.node(to.ty, e)? {
                PNode::Ref(_, i) => i,
                _ => return Ok(false),
            },
        };
        let PNode::Dynamic(preds) = self.node(obj, e)? else {
            return Ok(false);
        };
        let facts: &'b F = self.facts;
        let from_inner = match from.wrap {
            Some(_) => from.ty,
            None => match self.node(from.ty, e)? {
                PNode::Ref(_, i) => i,
                _ => return Ok(false),
            },
        };
        // Exact closure-to-`dyn Fn*(Args) -> Ret + 'q` unsizing (both HRTB and non-HRTB):
        // `'r_src: 'r_dst`, every capture slot outlives `'q` (one-way), and every `dyn`
        // `Fn*` / `FnOnce::Output` predicate slot equates with the closure's corresponding
        // signature slot (`SigSlot::Owner` / `SigSlot::Static`).
        if let PNode::Closure { owner, ordinal, .. } = self.node(from_inner, e)?
            && owner == self.body.owner
            && let Some(&ce) = self.ck.closures.column::<0>().get(ordinal as usize)
        {
            let mut sig_slots: Vec<closure::SigSlot, A> = Vec::new_in(self.alloc);
            if self.sig_slots(ordinal as usize, ce, &mut sig_slots).is_ok() {
                let nsig = self.owner_sig_count(ordinal as usize, e)?;
                if s.len >= 1 + nsig {
                    let ncap = s.len - 1 - nsig;
                    let out_name = Name::of("Output");
                    // `(dyn_off, sig_base, count)` pairs to equate after validating every predicate.
                    let mut ties: Vec<(u32, usize, u32), A> = Vec::new_in(self.alloc);
                    let mut dyn_at = 2u32;
                    let mut ok = true;
                    for &p in self.list(preds, e)? {
                        let pn = self.npos(p, e)?;
                        match self.node(p, e)? {
                            PNode::ExAuto(_) if pn == 0 => {}
                            PNode::ExTrait(k, args) if facts.is_fn_trait(k) => {
                                let al = self.list(args, e)?;
                                if al.len() != 1 || self.npos(al[0], e)? != pn || (pn as usize) > sig_slots.len() {
                                    ok = false;
                                    break;
                                }
                                ties.push((dyn_at, 0, pn));
                            }
                            PNode::ExProj { trait_: k, name, args, term } if facts.is_fn_trait(k) && name == out_name => {
                                let al = self.list(args, e)?;
                                if al.len() != 1 {
                                    ok = false;
                                    break;
                                }
                                let in_n = self.npos(al[0], e)?;
                                let out_n = self.npos(term, e)?;
                                if in_n + out_n != pn || (in_n + out_n) as usize != sig_slots.len() {
                                    ok = false;
                                    break;
                                }
                                ties.push((dyn_at, 0, in_n));
                                ties.push((dyn_at + in_n, in_n as usize, out_n));
                            }
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                        dyn_at += pn;
                    }
                    if ok && dyn_at == d.len {
                        let (d0, q) = (self.origs[d.start as usize], self.origs[(d.start + 1) as usize]);
                        self.b.subset(self.origs[s.start as usize], d0);
                        for j in 0..ncap {
                            self.b.subset(self.origs[(s.start + 1 + j) as usize], q);
                        }
                        for &(dyn_off, sig_base, count) in ties.iter() {
                            for i in 0..count {
                                let d_orig = self.origs[(d.start + dyn_off + i) as usize];
                                match sig_slots[sig_base + i as usize] {
                                    closure::SigSlot::Owner(k) if k < nsig => {
                                        let c_orig = self.origs[(s.start + 1 + ncap + k) as usize];
                                        self.b.subset(c_orig, d_orig);
                                        self.b.subset(d_orig, c_orig);
                                    }
                                    closure::SigSlot::Static => {
                                        let st = self.universal(Region::Static);
                                        self.b.subset(st, d_orig);
                                        self.b.subset(d_orig, st);
                                    }
                                    closure::SigSlot::Late(_) => {}
                                    closure::SigSlot::Owner(_) => {}
                                }
                            }
                        }
                        return Ok(true);
                    }
                }
            }
        }
        if d.len != 2 {
            return Ok(false);
        }
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc);
        for &p in self.list(preds, e)? {
            let (args, term) = match self.node(p, e)? {
                PNode::ExTrait(k, args) => {
                    if facts.object_trait_lifetimes(k) != Some(false) {
                        return Ok(false);
                    }
                    (args, None)
                }
                PNode::ExProj { args, term, .. } => (args, Some(term)),
                PNode::ExAuto(_) => continue,
                _ => return Ok(false),
            };
            for &t in self.list(args, e)?.iter().chain(term.as_ref()) {
                held.clear();
                if self.npos(t, e)? > 0 || self.unplaced(t, e, &mut held)? || !held.is_empty() {
                    return Ok(false);
                }
            }
        }
        let (d0, q) = (self.origs[d.start as usize], self.origs[(d.start + 1) as usize]);
        self.b.subset(self.origs[s.start as usize], d0);
        for j in s.start + 1..s.start + s.len {
            self.b.subset(self.origs[j as usize], q);
        }
        Ok(true)
    }

    fn target_flow(&mut self, from: PInfo, skip: u32, r: PInfo, m: bool) {
        for k in 1..r.len {
            let to = self.origs[(r.start + k) as usize];
            for j in skip..from.len {
                let b = self.origs[(from.start + j) as usize];
                self.b.subset(b, to);
                if m {
                    self.b.subset(to, b);
                }
            }
        }
    }

    fn ref_done(&mut self, e: ExprId, m: Mutbl) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        let ty = TyD::of(self.expr_ty(e)?);
        let t = self.temp(ty, e)?;
        let tp = self.place_of(t);
        match v.at {
            // The Reference, "Constant promotion": a borrow of a constant expression is a
            // borrow of a `'static` slot: no loan of the body's.
            At::Const => {
                self.b.assign(tp, Operand::Const, self.span(e));
            }
            At::Place(p) => {
                let kind = if m == Mutbl::Mut { BorrowKind::Mut } else { BorrowKind::Shared };
                self.borrow(p, v.ty, kind, t, e)?;
                // Temporary lifetime extension: the operand of an extending borrow in a `let`
                // initializer lives to the end of the block holding the `let`.
                // The place's root local: "If a borrow, dereference, field, or tuple indexing
                // expression has an extended temporary scope then so does its operand" (`let x =
                // &mk().f;` extends `mk()`; an arm's `&mk().f` died at the arm's end otherwise).
                if let Expr::Ref { operand, .. } = self.expr(e)? {
                    self.extend_root(p, operand)?;
                }
            }
        }
        self.push_place(tp, ty);
        Ok(())
    }

    /// Temporary lifetime extension (the Reference, "Temporary lifetime extension": "the
    /// temporary scope of the operand of an extending borrow expression is extended" to "the
    /// end of the enclosing block" of the `let`): the temporary `temps[at]` joins the variables
    /// of the block holding the `let`, `depth` the drop scopes open when that `let` was reached
    /// (`mark_extended`), after that block's variables declared so far and so before the
    /// `let`'s own bindings (bound after its initializer): it dies at that block's end, after
    /// them. Scopes opened since (a block, an `if` or `match` arm of the initializer: their
    /// variables are `vars[scope_marks[depth]..]`) each start one later. Before, it joined the
    /// INNERMOST scope: `let x = { &mk() };` and `let x = match v { _ => &mk() };` ended
    /// `mk()` at the block's or arm's end, a false finding at every use of `x`.
    /// The temporary at the root of place `p`, extended where expression `e`'s row says
    /// ([`Lower::extended`], `mark_extended`): `e` the operand of an extending borrow or a base
    /// it is projected from. A root that is no temporary (a variable) is left where it is.
    fn extend_root(&mut self, p: Place, e: ExprId) -> Result<(), Refusal> {
        let Some(&depth) = self.extended.get(e.ix()) else { return Ok(()) };
        if depth == NOT_EXTENDED {
            return Ok(());
        }
        let pi = self.info(p);
        if let Some(at) = self.temps.iter().rposition(|l| l.0 == pi.local) {
            self.extend_temp(at, depth, e)?;
        }
        Ok(())
    }

    fn extend_temp(&mut self, at: usize, depth: u32, e: ExprId) -> Result<(), Refusal> {
        let d = depth as usize;
        if at >= self.temps.len() || (depth != STATIC_EXTENDED && d > self.scope_marks.len()) {
            return self.refuse(Form::Malformed, e);
        }
        let l = self.temps.remove(at);
        self.temp_left(at);
        // A temporary scope opened since it was made (an arm's, a tail's) and a `break` or
        // `continue` target's count start one earlier.
        for m in self.temp_marks.iter_mut() {
            if *m as usize > at {
                *m -= 1;
            }
        }
        for t in self.targets.iter_mut() {
            if t.temps as usize > at {
                t.temps -= 1;
            }
        }
        // A constant initializer's: `'static`, in no scope, so never dropped or ended.
        if depth == STATIC_EXTENDED {
            return Ok(());
        }
        match self.scope_marks.get(d) {
            Some(&m) => {
                self.vars.insert(m as usize, l);
                for x in self.scope_marks.iter_mut().skip(d) {
                    *x += 1;
                }
            }
            None => self.vars.push(l),
        }
        Ok(())
    }

    fn field_done(&mut self, e: ExprId) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        let ty = TyD::of(self.expr_ty(e)?);
        let p = self.as_place(v, e)?;
        if v.ty.wrap.is_some() {
            return self.refuse(Form::Malformed, e);
        }
        let mut field_adt: Option<ItemKey> = None;
        let (idx, drop_type) = match self.node(v.ty.ty, e)? {
            PNode::Tuple(_) => match self.expr(e)? {
                Expr::TupleField { index, .. } => (index, false),
                _ => return self.refuse(Form::UnknownField, e),
            },
            PNode::Adt(k, _) => {
                let f = self.adt_facts(k, e)?;
                if f.union {
                    return self.refuse(Form::Union, e);
                }
                field_adt = Some(k);
                match self.field_of(e) {
                    Some(i) => (i, f.drop_impl),
                    None => return self.refuse(Form::UnknownField, e),
                }
            }
            _ => return self.refuse(Form::UnknownField, e),
        };
        let fp = self.project(p, v.ty, Proj::Field(idx), ty, drop_type, e)?;
        if let Some(k) = field_adt {
            let holds_fn = match self.node(ty.ty, e)? {
                PNode::FnPtr(..) => true,
                PNode::Ref(_, inner) => matches!(self.node(inner, e)?, PNode::FnPtr(..)),
                PNode::Adt(_, l) => self.list(l, e)?.first().is_some_and(|&a| matches!(self.node(a, e), Ok(PNode::FnPtr(..)))),
                _ => false,
            };
            if holds_fn {
                self.place_field_source.push((fp, k, 0, idx));
            }
        }
        self.push_place(fp, ty);
        Ok(())
    }

    /// The base of the struct update `e`, its value on the stack, replaced by one place per
    /// field the update takes (`Checked::fru_fields`, in field order): each is `base.f` as
    /// [`Self::field_done`] projects it, so the aggregate moves or copies each by its type and
    /// the borrow check sees a partial move. A base whose type implements `Drop` marks the
    /// projection, so a move out of it is refused as for `let x = base.f;` (E0509).
    /// The fields the base-less struct expression `e` takes from its defaults, after its
    /// written ones: each a constant of the field's type (`Checked::fru_fields`, which the
    /// checker fills for every defaulted field, in field order), as a constant item's value
    /// is: no place, no loan, no move.
    fn field_defaults(&mut self, e: ExprId) {
        let ck = self.ck;
        let (_, tys) = ck.fru_fields_of(e);
        for &t in tys {
            self.vals.push(Val { at: At::Const, ty: TyD::of(t) });
        }
    }

    fn struct_base(&mut self, e: ExprId) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        let p = self.as_place(v, e)?;
        if v.ty.wrap.is_some() {
            return self.refuse(Form::Malformed, e);
        }
        let drop_type = match self.node(v.ty.ty, e)? {
            PNode::Adt(k, _) => {
                let f = self.adt_facts(k, e)?;
                if f.union {
                    return self.refuse(Form::Union, e);
                }
                f.drop_impl
            }
            PNode::Record(..) => false,
            _ => return self.refuse(Form::Malformed, e),
        };
        let ck = self.ck;
        let (idx, tys) = ck.fru_fields_of(e);
        for (&i, &t) in idx.iter().zip(tys.iter()) {
            let ft = TyD::of(t);
            let fp = self.project(p, v.ty, Proj::Field(i), ft, drop_type, e)?;
            self.push_place(fp, ft);
        }
        Ok(())
    }

    fn index_done(&mut self, e: ExprId, m: bool) -> Result<(), Refusal> {
        let idx = self.pop(e)?;
        let base = self.pop(e)?;
        let ty = TyD::of(self.expr_ty(e)?);
        let bp = self.as_place(base, e)?;
        let builtin = base.ty.wrap.is_none() && matches!(self.node(base.ty.ty, e)?, PNode::Array(..) | PNode::Slice(_));
        if builtin {
            if let At::Place(ip) = idx.at {
                // The index is read before the place is used.
                let t = self.temp(idx.ty, e)?;
                let tp = self.place_of(t);
                self.b.assign(tp, Operand::Copy(ip), self.span(e));
            }
            let p = self.project(bp, base.ty, Proj::Index, ty, false, e)?;
            self.push_place(p, ty);
            return Ok(());
        }
        // `Index::index(&base, i)` / `IndexMut::index_mut(&mut base, i)`, then `*` of the result,
        // whose lifetime is the receiver's (elision, `&self`). `ET[..]` of an extending borrow's
        // operand: the base temporary it borrows lives as the result does (`mark_extended`).
        if let Expr::Index { base: be, .. } = self.expr(e)? {
            self.extend_root(bp, be)?;
        }
        let mutbl = if m { Mutbl::Mut } else { Mutbl::Not };
        let bt = self.temp(TyD { wrap: Some(mutbl), ty: base.ty.ty }, e)?;
        self.name_local(bt, Identity::Temp(e, Role::Borrow));
        let kind = if m { BorrowKind::Mut } else { BorrowKind::Shared };
        self.borrow(bp, base.ty, kind, bt, e)?;
        let iop = self.operand(idx, e)?;
        let rt = self.temp(TyD { wrap: Some(mutbl), ty: ty.ty }, e)?;
        self.name_local(rt, Identity::Temp(e, Role::Result));
        let (btp, rp) = (self.place_of(bt), self.place_of(rt));
        let bop = if m { Operand::Move(btp) } else { Operand::Copy(btp) };
        self.b.call(&[bop, iop], rp, self.span(e));
        let (bi, ri) = (self.info(btp), self.info(rp));
        let (ob, or) = (self.origs[bi.start as usize], self.origs[ri.start as usize]);
        self.b.subset(ob, or);
        self.target_flow(bi, 1, ri, m);
        let mut inputs = bi.len.saturating_sub(1);
        if let At::Place(ip) = idx.at {
            let ii = self.info(ip);
            inputs += ii.len;
            self.target_flow(ii, 0, ri, m);
        }
        if inputs > 1 && ri.len > 1 {
            self.coarse_step(Form::Unified { why: Unified::DerefTarget }, e);
        }
        let p = self.project(rp, self.local_ty[rt.0 as usize], Proj::Deref, ty, false, e)?;
        self.push_place(p, ty);
        Ok(())
    }

    fn bin_done(&mut self, e: ExprId, op: BinOp) -> Result<(), Refusal> {
        let rhs = self.pop(e)?;
        let lhs = self.pop(e)?;
        let ty = TyD::of(self.expr_ty(e)?);
        // The Reference, "Comparison operators": the operands are place expressions, borrowed
        // by the operator: read, never moved.
        let cmp = matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge);
        let read = |s: &Self, v: Val| -> Result<Operand, Refusal> {
            match (cmp, v.at) {
                (true, At::Place(p)) => Ok(Operand::Copy(p)),
                _ => s.operand(v, e),
            }
        };
        let (a, b) = (read(self, lhs)?, read(self, rhs)?);
        let t = self.temp(ty, e)?;
        let tp = self.place_of(t);
        self.b.aggregate(tp, &[a, b], self.span(e));
        // The Reference, "Arithmetic and Logical Binary Operators": `a + b` is
        // `Add::add(a, b)`. On primitive operands, references to them included, the impls are
        // core's (`impl Add<&u32> for &u32`, `forward_ref_binop!`): every lifetime their own
        // generic, the output a primitive. Rust relates nothing then, so neither does this:
        // exact, no step (the operands' loans are live at the operator by its own use of
        // them). On any other operand the impl's signature is not read here: the hub below.
        let prim = self.prim_operand(lhs.ty, e)? && self.prim_operand(rhs.ty, e)?;
        if !cmp && !prim {
            let mut ps: Vec<Place, A> = Vec::new_in(self.alloc);
            for v in [lhs, rhs] {
                if let At::Place(p) = v.at {
                    ps.push(p);
                }
            }
            ps.push(tp);
            let mut held = 0u32;
            for &p in ps.iter() {
                held += self.info(p).len;
            }
            if held > 1 {
                self.coarse_step(Form::Unified { why: Unified::BinaryOperator }, e);
            }
            self.unify(&ps);
        }
        self.push_place(tp, ty);
        Ok(())
    }

    /// A primitive (`bool`, `char`, an integer or a float) or references to one: an operand
    /// whose operator impls are core's, generic in every lifetime (`bin_done`).
    fn prim_operand(&self, t: TyD, e: ExprId) -> Result<bool, Refusal> {
        let mut ty = t.ty;
        for _ in 0..MOST_POSITIONS {
            match self.node(ty, e)? {
                PNode::Ref(_, inner) => ty = inner,
                PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) => return Ok(true),
                _ => return Ok(false),
            }
        }
        self.refuse(Form::Bound, e)
    }

    /// An aggregate (a constructor, tuple, array, struct, cast, unary op or `format_args!`): its
    /// operands in order into a new temporary, every origin made equal.
    fn aggregate(&mut self, e: ExprId, n: u32, agg: Agg) -> Result<(), Refusal> {
        let fmt = agg == Agg::Fmt;
        let ty = TyD::of(self.expr_ty(e)?);
        let at = match self.vals.len().checked_sub(n as usize) {
            Some(a) => a,
            None => return self.refuse(Form::Malformed, e),
        };
        let mut vs: Vec<Val, A> = Vec::new_in(self.alloc);
        vs.extend_from_slice(&self.vals[at..]);
        self.vals.truncate(at);
        if n == 0 && !fmt {
            self.vals.push(Val { at: At::Const, ty });
            return Ok(());
        }
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        let mut ps: Vec<Place, A> = Vec::new_in(self.alloc);
        // The values as they go into the aggregate (for `format_args!`, the borrows).
        let mut parts: Vec<Val, A> = Vec::new_in(self.alloc);
        for v in vs.iter().copied() {
            if fmt {
                // `format_args!` borrows each argument.
                match v.at {
                    At::Const => ops.push(Operand::Const),
                    At::Place(p) => {
                        let rt = TyD { wrap: Some(Mutbl::Not), ty: v.ty.ty };
                        let t = self.temp(rt, e)?;
                        self.borrow(p, v.ty, BorrowKind::Shared, t, e)?;
                        let tp = self.place_of(t);
                        ops.push(Operand::Copy(tp));
                        ps.push(tp);
                        parts.push(Val { at: At::Place(tp), ty: rt });
                    }
                }
            } else {
                ops.push(self.operand(v, e)?);
                if let At::Place(p) = v.at {
                    ps.push(p);
                }
                parts.push(v);
            }
        }
        let t = self.temp(ty, e)?;
        let tp = self.place_of(t);
        self.b.aggregate(tp, &ops, self.span(e));
        let d = self.info(tp);
        // A tuple's element goes into its own field: related as an assignment to
        // that field is (the type of `(a, b)` is `(A, B)`: nothing relates `A`'s lifetimes to
        // `B`'s). Every other aggregate (a struct or variant, whose field types the heads do not
        // give yet) is related as a call of unknown signature.
        let elems: Option<&'c [PTy]> = match (ty.wrap, self.node(ty.ty, e)?) {
            (None, PNode::Tuple(l) | PNode::Record(_, l)) => Some(self.list(l, e)?),
            _ => None,
        };
        // A tuple-like constructor whose every field holding positions is declared as a bare
        // type parameter (`Some(T)`, `Ok(T)`, `Err(E)`; `Facts::field_param`): that field's
        // value is a value of the type argument, so it goes into the argument's own positions
        // of the ADT's (the lifetimes first, then each argument's, `npos`), as a tuple's
        // element goes into its field. thiserror's `Some(source.as_dyn_error())` went through
        // one hub, which made the `&self` loan in `&'s (dyn Error + 'static)`'s first position
        // reach its `'static` (a false "outlives" in every derived `Error::source`).
        if agg == Agg::Ctor && elems.is_none() && !fmt && d.exact && self.plant != Plant::TupleUnified {
            if let Some(spots) = self.ctor_fields(e, ty, d.len, &vs)? {
                for (j, &(off, n, at)) in spots.iter().enumerate() {
                    if let At::Place(p) = vs[j].at {
                        self.relate_at(p, d.start + off, n, TyD::of(at), e)?;
                    }
                }
                self.push_place(tp, ty);
                return Ok(());
            }
        }
        // A struct expression, or a constructor call, whose fields' declared types are known
        // (`Facts::field_regions`): each operand goes into its own field's positions only, as
        // a tuple's element goes into its own (the Reference, "Struct expressions": each
        // field's value is coerced to the field's type, the declared type at the struct's
        // arguments). Through `unify_call`'s one origin, `Checking { given: self, .. }` tied
        // `'g` of `self: &'c Granting<'g>` to `'c` and every other lifetime of the literal.
        if !fmt
            && elems.is_none()
            && d.exact
            && self.plant != Plant::TupleUnified
            && let Some((variant, fields)) = self.literal_fields(e, ty, vs.len())?
        {
            let mut slots: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
            for (j, v) in vs.iter().enumerate() {
                let held = match v.at {
                    At::Place(p) => self.info(p).len > 0,
                    At::Const => false,
                };
                if !held {
                    slots.push((0, 0));
                    continue;
                }
                match self.field_slot(d, ty.ty, variant, fields[j], v.ty, e)? {
                    Some(s) => slots.push(s),
                    None => break,
                }
            }
            if slots.len() == vs.len() {
                for (j, v) in vs.iter().enumerate() {
                    if let At::Place(p) = v.at {
                        let (s, n) = slots[j];
                        self.relate_at(p, s, n, v.ty, e)?;
                    }
                }
                self.push_place(tp, ty);
                return Ok(());
            }
        }
        // An array expression `[a, b]`: each element is coerced to the element type (the
        // Reference, "Array and array index expressions"), so each goes one way into the
        // element's positions, the array's own, as a tuple's element into its field: exact, no
        // hub (`[&'a x, &'b y]` relates `'a` and `'b` to nothing but the array's `'c`).
        if !fmt
            && elems.is_none()
            && d.exact
            && self.plant != Plant::TupleUnified
            && let (None, PNode::Array(et, _)) = (ty.wrap, self.node(ty.ty, e)?)
            && self.npos(et, e)? == d.len
        {
            for v in vs.iter() {
                if let At::Place(p) = v.at {
                    self.relate_at(p, d.start, d.len, TyD::of(et), e)?;
                }
            }
            self.push_place(tp, ty);
            return Ok(());
        }
        // An explicit cast `&T as &dyn Trait` (e.g. tracing's `&display(&x) as &dyn Value`):
        // unsizing to a trait object whose trait writes no lifetimes relates one way into the
        // object reference and its lifetime bound (`unsize_to_object`), never via `AggregateHub`.
        if !fmt
            && elems.is_none()
            && parts.len() == 1
            && matches!(self.expr(e)?, Expr::Cast { .. })
            && let At::Place(p) = parts[0].at
            && self.unsize_to_object(p, parts[0].ty, tp, ty, e)?
        {
            self.push_place(tp, ty);
            return Ok(());
        }
        match elems {
            Some(tys) if !fmt && d.exact && tys.len() == vs.len() && self.plant != Plant::TupleUnified => {
                let mut off = d.start;
                for (j, &et) in tys.iter().enumerate() {
                    let n = self.npos(et, e)?;
                    if let At::Place(p) = vs[j].at {
                        self.relate_at(p, off, n, TyD::of(et), e)?;
                    }
                    off += n;
                }
            }
            _ if self.plant == Plant::TupleUnified => {
                ps.push(tp);
                self.unify(&ps);
            }
            // `Arguments<'a>` holds each argument as `&'a T`: each position flows into `'a`,
            // nothing flows back (generic_call.rs `fmt_args_flow`, piece BR4).
            _ if fmt => self.fmt_args_flow(&parts, tp),
            _ => {
                // Every operand's positions and the aggregate's made equal through one hub
                // where Rust relates each field's value into its own field by variance: noted
                // (the coarse rule), unless one operand's one top reference flows one way
                // into the aggregate's one position (`unify_call`'s one-way top reference).
                let mut held = d.len;
                let mut one_way = false;
                for v in parts.iter() {
                    if let At::Place(p) = v.at {
                        let pi = self.info(p);
                        held += pi.len;
                        one_way = pi.len == 1 && pi.exact && (v.ty.wrap.is_some() || matches!(self.node(v.ty.ty, e)?, PNode::Ref(..)));
                    }
                }
                if held > 2 || (held == 2 && d.len == 1 && !one_way) {
                    self.coarse_step(Form::Unified { why: Unified::AggregateHub }, e);
                }
                self.unify_call(&parts, tp, e)?
            }
        }
        self.push_place(tp, ty);
        Ok(())
    }

    /// The variant of `ty`'s ADT that the aggregate `e` makes, and the field each of its `n`
    /// operands fills: a constructor call's operands are its fields in order; a struct
    /// expression's are its written fields in source order, then those its base or its
    /// defaults give (`Checked::fru_fields`, `Checked::default_fields`), as `Expr::Struct`
    /// pushes them. `None`: not such an aggregate, or a field the facts do not name.
    fn literal_fields(&self, e: ExprId, ty: TyD, n: usize) -> Result<Option<(u32, Vec<u32, A>)>, Refusal> {
        let (None, PNode::Adt(k, _)) = (ty.wrap, self.node(ty.ty, e)?) else { return Ok(None) };
        let mut out: Vec<u32, A> = Vec::new_in(self.alloc);
        let (adt, variant) = match self.expr(e)? {
            Expr::Call { callee, .. } => {
                let Some(Def::Ctor { adt, variant, .. }) = self.expr_def(callee) else { return Ok(None) };
                for j in 0..n {
                    out.push(j as u32);
                }
                (adt, variant)
            }
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                let (adt, variant) = match self.expr_def(e) {
                    Some(Def::Ctor { adt, variant, .. }) => (adt, variant),
                    Some(Def::Adt(adt)) if !self.adt_facts(adt, e)?.is_enum => (adt, 0),
                    _ => return Ok(None),
                };
                let Some(fs) = self.body.field_inits.get(fields.range()) else { return Ok(None) };
                for f in fs {
                    let Some(i) = self.facts.field(adt, variant, f.name) else { return Ok(None) };
                    out.push(i);
                }
                let ck = self.ck;
                match base {
                    blaze_typeck::StructBase::Expr(_) => out.extend_from_slice(ck.fru_fields_of(e).0),
                    blaze_typeck::StructBase::Defaults => out.extend_from_slice(ck.default_fields_of(e).0),
                    blaze_typeck::StructBase::None => {}
                }
                (adt, variant)
            }
            _ => return Ok(None),
        };
        if adt != k || out.len() != n {
            return Ok(None);
        }
        Ok(Some((variant, out)))
    }

    /// Where each operand of the constructor call `e` (of type `ty`) goes among the value's
    /// positions: `(offset, count, type)` per operand, when the callee is a variant or struct
    /// constructor of `ty`'s ADT, the operands are its fields in order, and every field an
    /// operand with positions fills is declared as a bare type parameter. `None` otherwise (the
    /// aggregate keeps `unify_call`'s one origin).
    fn ctor_fields(&self, e: ExprId, ty: TyD, len: u32, vs: &[Val]) -> Result<Option<Vec<(u32, u32, PTy), A>>, Refusal> {
        let Expr::Call { callee, .. } = self.expr(e)? else { return Ok(None) };
        let Some(Def::Ctor { adt, variant, .. }) = self.expr_def(callee) else { return Ok(None) };
        let (None, PNode::Adt(k, l)) = (ty.wrap, self.node(ty.ty, e)?) else { return Ok(None) };
        let (Some(facts), Some(count)) = (self.facts.adt(k), self.facts.field_count(adt, variant)) else { return Ok(None) };
        if k != adt || count as usize != vs.len() {
            return Ok(None);
        }
        let args = self.list(l, e)?;
        // The value's positions are the ADT's lifetimes, then each argument's, in order.
        let mut total = facts.lifetimes;
        for &a in args {
            total += self.npos(a, e)?;
        }
        if total != len {
            return Ok(None);
        }
        let mut spots: Vec<(u32, u32, PTy), A> = Vec::new_in(self.alloc);
        for (j, v) in vs.iter().enumerate() {
            let held = match v.at {
                At::Place(p) => self.info(p).len > 0,
                At::Const => false,
            };
            let Some(p) = self.facts.field_param(adt, variant, j as u32).filter(|&p| (p as usize) < args.len()) else {
                if held {
                    return Ok(None);
                }
                spots.push((0, 0, v.ty.ty));
                continue;
            };
            let mut off = facts.lifetimes;
            for &a in &args[..p as usize] {
                off += self.npos(a, e)?;
            }
            spots.push((off, self.npos(args[p as usize], e)?, args[p as usize]));
        }
        Ok(Some(spots))
    }

    /// `src` goes into the positions `start..start + n` of some place, whose type there is
    /// `dst_ty`: position by position when `src` is exact and as long, the first covariantly
    /// when it is a reference, every other both ways; else all made equal through one origin.
    fn relate_at(&mut self, src: Place, start: u32, n: u32, dst_ty: TyD, e: ExprId) -> Result<(), Refusal> {
        let s = self.info(src);
        if s.len == 0 || n == 0 || self.plant == Plant::NoRelate {
            return Ok(());
        }
        if s.exact && s.len == n {
            // Each position by its variance in the destination's type (variance.rs).
            let mut vs: Vec<Variance, A> = Vec::new_in(self.alloc);
            self.position_variances(dst_ty, &mut vs, e)?;
            if vs.len() as u32 != n {
                return self.refuse(Form::Malformed, e);
            }
            self.relate_by(s.start, start, &vs);
            return Ok(());
        }
        if s.len + n > 2 {
            self.coarse_step(Form::Unified { why: Unified::SlotInexact }, e);
        }
        let hub = self.b.origin(false);
        for k in (s.start..s.start + s.len).chain(start..start + n) {
            let o = self.origs[k as usize];
            self.b.subset(o, hub);
            self.b.subset(hub, o);
        }
        Ok(())
    }

    /// The Reference, "Evaluation order of operands": a call's, a tuple's, an array's and a
    /// struct expression's operands are evaluated left to right, and an operand that is a place
    /// expression is read (copied or moved) WHERE it is evaluated, before the next operand runs
    /// (rustc's MIR builds each into a temporary, `as_operand`). Kept as a place until the call,
    /// `m.insert(p.n, Arc::new(p))` read `p.n` after `Arc::new(p)` had moved `p` (tempo
    /// zones/sequencer proofs.rs:486 and :540, a false E0382); `f(a, g(&a))` borrowed `a`
    /// before the move that comes first (a missed E0382).
    ///
    /// The top value, a place that is not a temporary of its own, is moved or copied into a
    /// new temporary when its type holds no origin: no relation is made, so the read moves
    /// earlier and nothing else changes. A value holding origins stays a place (its read is
    /// still at the call): a temporary of it needs `relate`, by variance, stricter where an
    /// ADT's variance is unknown (D190 item 2); that residue goes with the variances.
    fn settle_operand(&mut self, e: ExprId) -> Result<(), Refusal> {
        let Some(&v) = self.vals.last() else { return self.refuse(Form::Malformed, e) };
        let At::Place(p) = v.at else { return Ok(()) };
        if self.npos_d(v.ty, e)? > 0 {
            return Ok(());
        }
        let pi = self.info(p);
        if pi.parent == u32::MAX && self.temps.iter().any(|l| l.0 == pi.local) {
            return Ok(());
        }
        let op = self.operand(v, e)?;
        let t = self.temp(v.ty, e)?;
        let tp = self.place_of(t);
        self.b.assign(tp, op, self.span(e));
        match self.vals.last_mut() {
            Some(last) => *last = Val { at: At::Place(tp), ty: v.ty },
            None => return self.refuse(Form::Malformed, e),
        }
        Ok(())
    }

    fn call_done(&mut self, e: ExprId, n: u32, value_callee: bool, mark: u32) -> Result<(), Refusal> {
        let ty = TyD::of(self.expr_ty(e)?);
        let k = n as usize + usize::from(value_callee);
        let at = match self.vals.len().checked_sub(k) {
            Some(a) => a,
            None => return self.refuse(Form::Malformed, e),
        };
        // A callee value whose type is a fn item's is a call of that item: its head answers.
        let mut by_type: Option<(ItemKey, List)> = None;
        if value_callee {
            let c = self.vals[at];
            if c.ty.wrap.is_none()
                && let PNode::FnDef(k, l) = self.node(c.ty.ty, e)?
            {
                by_type = Some((k, l));
            }
        }
        let value_callee = value_callee && by_type.is_none();
        // A call of a closure is a use of the closure value: by shared reference when it is
        // `Fn`, by mutable reference when `FnMut`, by value when `FnOnce` (the Reference,
        // "Closure types", "Call traits and coercions").
        if value_callee {
            let c = self.vals[at];
            if c.ty.wrap.is_none()
                && let At::Place(p) = c.at
                && let PNode::Closure { ordinal, .. } = self.node(c.ty.ty, e)?
            {
                let by = match self.ck.closures.column::<1>().get(ordinal as usize) {
                    Some(blaze_typeck::ClosureKind::Fn) => Some((Mutbl::Not, BorrowKind::Shared)),
                    Some(blaze_typeck::ClosureKind::FnMut) => Some((Mutbl::Mut, BorrowKind::Mut)),
                    Some(blaze_typeck::ClosureKind::FnOnce) => None,
                    None => return self.refuse(Form::Malformed, e),
                };
                if let Some((m, bk)) = by {
                    let rt = TyD { ty: c.ty.ty, wrap: Some(m) };
                    let t = self.temp(rt, e)?;
                    self.borrow(p, c.ty, bk, t, e)?;
                    self.vals[at] = Val { at: At::Place(self.place_of(t)), ty: rt };
                }
            }
        }
        let mut vs: Vec<Val, A> = Vec::new_in(self.alloc);
        vs.extend_from_slice(&self.vals[at + usize::from(by_type.is_some())..]);
        self.vals.truncate(at);
        // RFC 2025: all call arguments are evaluated BEFORE a pending two-phase borrow activates
        // at the call (`self.add_tag(entity, self.builtins.prefab)` reads `self.builtins.prefab`
        // while `self`'s two-phase borrow is still reserved, not after `Activate`). Settle any
        // non-temporary place operand into a fresh temporary before emitting `Activate`.
        let pending = self.two_phase.len();
        if (mark as usize) < pending {
            for v in vs.iter_mut() {
                let At::Place(p) = v.at else { continue };
                let pi = self.info(p);
                if pi.parent == u32::MAX && self.temps.iter().any(|l| l.0 == pi.local) {
                    continue;
                }
                let op = self.operand(*v, e)?;
                let arg_t = self.temp(v.ty, e)?;
                let arg_tp = self.place_of(arg_t);
                self.b.assign(arg_tp, op, self.span(e));
                if pi.len > 0 {
                    self.relate(p, arg_tp, v.ty, e)?;
                }
                *v = Val { at: At::Place(arg_tp), ty: v.ty };
            }
        }
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        let mut ins: Vec<Place, A> = Vec::new_in(self.alloc);
        for v in vs.iter().copied() {
            ops.push(self.operand(v, e)?);
            if let At::Place(p) = v.at {
                ins.push(p);
            }
        }
        let t = self.temp(ty, e)?;
        let tp = self.place_of(t);
        // RFC 2025: a two-phase borrow is activated at its first use, the call.
        for i in mark as usize..pending {
            let l = self.two_phase[i];
            self.b.stmt(StmtKind::Activate(l), self.span(e));
        }
        self.two_phase.truncate((mark as usize).min(pending));
        self.b.call(&ops, tp, self.span(e));
        let out = self.info(tp);
        if value_callee {
            // Outside the seam, so asked here: the witness lowering relates nothing.
            if R::RELATES {
                self.call_through_value(e, &vs, tp, ty)?;
            }
            self.push_place(tp, ty);
            return Ok(());
        }
        let callee = match by_type {
            Some(c) => Some(c),
            None => self.callee_of(e),
        };
        // THE SEAM (rule 78, D156): the call's regions related by the rule `R`
        // (one_call.rs; production's is `one_call::Production`).
        let site = one_call::CallSite { e, vs: &vs, ins: &ins, tp, ty, out, callee };
        self.call_by_rule(&site)?;
        // The call an `.await` awaits: its awaited value's positions, made at the call where
        // the arguments are live (await_call.rs `awaited_shadow`).
        if self.awaiting.last().is_some_and(|&(op, _)| op == e) {
            self.awaited_shadow(&site, t)?;
        }
        self.push_place(tp, ty);
        Ok(())
    }

    /// A call through a value, `vs[0]`: a fn pointer, a `dyn Fn`, a generic `F: Fn(..)`. Its
    /// lifetimes are its own type's (the Reference, "Higher-ranked trait bounds": such "bounds
    /// specify a bound that is true for all lifetimes", and "Lifetime elision" holds in fn
    /// pointer and closure trait signatures: `fn(&u8)` is `for<'a> fn(&'a u8)`): every region
    /// but `'static` is instantiated afresh at each call, and `'static` is the body's, so
    /// `f(&local)` with `f: fn(&'static u8)` puts the loan where it is live for ever. The
    /// erased types do not hold them: they are the slots of the body's own signature column at
    /// the parameter the value is, a fn pointer (D148). Any other
    /// value, or a type not read, is refused by name unless nothing in the call carries a
    /// lifetime (then no signature could relate or demand anything).
    fn call_through_value(&mut self, e: ExprId, vs: &[Val], dest: Place, dest_ty: TyD) -> Result<(), Refusal> {
        let Some(args) = vs.get(1..) else { return self.refuse(Form::Malformed, e) };
        let mut any = self.info(dest).len > 0;
        for v in args {
            any |= self.npos_d(v.ty, e)? > 0;
        }
        if !any {
            self.stats.calls_unified += 1;
            return Ok(());
        }
        // A closure value of this body: its result may hold what the value holds (callee_value.rs).
        if self.call_of_closure_value(e, vs, dest)? {
            self.stats.calls_unified += 1;
            return Ok(());
        }
        // A generic callable `F: Fn/FnMut/FnOnce` (or `&F` / `&mut F` / `Box<F>`): its signature
        // lives in the body owner's clause columns (`Facts::regions().clauses(owner)`).
        if self.call_through_fn_param(vs[0].ty, args, dest, dest_ty, e)? {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        let mut c = match self.expr(e)? {
            Expr::Call { callee, .. } => callee,
            _ => return self.refuse(Form::Malformed, e),
        };
        let mut steps = 0usize;
        while let Expr::Paren(i) = self.expr(c)? {
            steps += 1;
            if steps > self.body.exprs.len() {
                return self.refuse(Form::Bound, e);
            }
            c = i;
        }
        // A fn pointer stored in a struct field (`(self.cb)(args)`): its declared regions live in
        // `Facts::field_regions(adt, field_idx)`.
        if self.call_through_field_fn_ptr(c, vs[0], args, dest, dest_ty, e)? {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        let param = match self.expr(c)? {
            Expr::Local(p) => self.body.params.iter().position(|x| *x == p),
            _ => None,
        };
        let facts: &'b F = self.facts;
        let Some(param) = param else {
            if let Expr::Local(p) = self.expr(c)?
                && let Some(&(_, adt, variant, field)) = self.local_field_source.iter().find(|&&(pat, ..)| pat == p)
                && let Ok(col) = facts.regions().field(adt, variant, field)
                && dest_ty.wrap.is_none()
                && !args.iter().any(|v| v.ty.wrap.is_some())
            {
                let mut declared = col.ty();
                let mut slots = col.as_slice();
                for _ in 0..16 {
                    match self.head_node(declared, e)? {
                        PNode::Ref(_, inner) => {
                            let Some(rest) = slots.get(1..) else { break };
                            declared = inner;
                            slots = rest;
                        }
                        PNode::Adt(k, l) if facts.adt(k).is_some_and(|a| a.lifetimes == 0) => {
                            let list = self.head_list(l, e)?;
                            let Some(&inner) = list.first() else { break };
                            if list.len() > 2 {
                                break;
                            }
                            let arity = crate::one_call::FactsArity { facts };
                            let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
                            let Ok(ic) = blaze_borrow::RegionTypes::count(&heads, inner) else { break };
                            let Some(inner_slots) = slots.get(..ic.get() as usize) else { break };
                            declared = inner;
                            slots = inner_slots;
                        }
                        _ => break,
                    }
                }
                if let PNode::Dynamic(list) = self.head_node(declared, e)?
                    && !slots.iter().any(|r| matches!(r, Region::Param(_)))
                    && self.call_through_dyn_fn(list, slots, args, dest, dest_ty, e)?
                {
                    self.stats.calls_by_signature += 1;
                    return Ok(());
                }
            }
            return self.refuse(Form::CalleeValueUnknown, e);
        };
        // The parameter's regions are the body's own signature column's input `param` (D148):
        // a fn whose own regions were not read is refused by name, as at the top of the body.
        let own_sig = match facts.sig(self.body.owner) {
            Ok(s) => s,
            Err(RegionsUnread { item, why }) => return self.refuse(Form::RegionsUnread { item, why }, e),
        };
        let (Some(mut slots), Ok(mut declared)) = (own_sig.input(param as u32), self.sig_declared(own_sig, param as u32, e)) else {
            return self.refuse(Form::CalleeValueUnknown, e);
        };
        if dest_ty.wrap.is_some() || args.iter().any(|v| v.ty.wrap.is_some()) {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        // Peel `&`, `&mut`, and `Box` wrappers off a parameter holding a `dyn Fn*`, `fn(..)`,
        // or a `Fn*`-bounded type parameter.
        let mut peel_skip = 0u32;
        let mut peel_steps = 0u32;
        loop {
            peel_steps += 1;
            if peel_steps > 16 {
                return self.refuse(Form::CalleeValueUnknown, e);
            }
            match self.head_node(declared, e)? {
                PNode::Ref(_, inner) => {
                    let Some(rest) = slots.get(1..) else { return self.refuse(Form::CalleeValueUnknown, e) };
                    declared = inner;
                    slots = rest;
                    peel_skip += 1;
                }
                PNode::Adt(k, l) if facts.adt(k).is_some_and(|a| a.is_box) => {
                    let adt_lts = facts.adt(k).map_or(0, |a| a.lifetimes) as usize;
                    let list = self.head_list(l, e)?;
                    let Some(&inner) = list.first() else { return self.refuse(Form::CalleeValueUnknown, e) };
                    // Only peel `Box<T>` with no lifetime slots of its own and no allocator slots.
                    if adt_lts != 0 || list.len() > 2 {
                        break;
                    }
                    let arity = crate::one_call::FactsArity { facts };
                    let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
                    let Ok(ic) = blaze_borrow::RegionTypes::count(&heads, inner) else { break };
                    let Some(inner_slots) = slots.get(..ic.get() as usize) else { break };
                    declared = inner;
                    slots = inner_slots;
                }
                _ => break,
            }
        }
        if let PNode::Dynamic(list) = self.head_node(declared, e)? {
            if self.call_through_dyn_fn(list, slots, args, dest, dest_ty, e)? {
                self.stats.calls_by_signature += 1;
                return Ok(());
            }
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        // A type parameter bounded by `Fn`/`FnMut`/`FnOnce` (`f: impl Fn(&u8) -> bool`) has its
        // signature in the enclosing fn's own clause columns (D148 L4/L5).
        if let PNode::Param(tp) = self.head_node(declared, e)? {
            let clauses = match facts.regions().clauses(self.body.owner) {
                Ok(c) => c,
                Err(RegionsUnread { item, why }) => return self.refuse(Form::RegionsUnread { item, why }, e),
            };
            let mut fn_args: Option<(PTy, &'b [Region])> = None;
            let mut fn_out: Option<(PTy, &'b [Region])> = None;
            let mut multi = false;
            for cv in clauses.iter() {
                match self.head_node(cv.col.ty(), e)? {
                    PNode::ExTrait(_, list) => {
                        let [me, tuple] = self.head_list(list, e)? else { continue };
                        if !matches!(self.head_node(*me, e)?, PNode::Param(q) if q == tp) {
                            continue;
                        }
                        let PNode::Tuple(in_list) = self.head_node(*tuple, e)? else { continue };
                        if self.head_list(in_list, e)?.len() != args.len() {
                            continue;
                        }
                        let Some(in_slots) = cv.col.child(1) else { continue };
                        if fn_args.is_some() {
                            multi = true;
                        } else {
                            fn_args = Some((*tuple, in_slots));
                        }
                    }
                    PNode::ExProj { name, args: list, term, .. } if name == Name::of("Output") => {
                        let [me, tuple] = self.head_list(list, e)? else { continue };
                        if !matches!(self.head_node(*me, e)?, PNode::Param(q) if q == tp) {
                            continue;
                        }
                        let PNode::Tuple(in_list) = self.head_node(*tuple, e)? else { continue };
                        if self.head_list(in_list, e)?.len() != args.len() {
                            continue;
                        }
                        let Some(out_slots) = cv.col.child(2) else { continue };
                        if fn_out.is_some() {
                            multi = true;
                        } else {
                            fn_out = Some((term, out_slots));
                        }
                    }
                    _ => {}
                }
            }
            let (Some((tuple_ty, in_slots)), false) = (fn_args, multi) else {
                return self.refuse(Form::CalleeValueUnknown, e);
            };
            let mut held: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
            let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
            if !self.lay_positions(tuple_ty, in_slots, &mut held, e)? || held.iter().any(Option::is_none) {
                return self.refuse(Form::CalleeValueUnknown, e);
            }
            for r in held.iter().flatten() {
                if matches!(r, Region::Param(_)) {
                    return self.refuse(Form::CalleeValueUnknown, e);
                }
                regions.push(*r);
            }
            match fn_out {
                Some((out_ty, out_slots)) => {
                    if !self.lay_positions(out_ty, out_slots, &mut held, e)? || held.iter().any(Option::is_none) {
                        return self.refuse(Form::CalleeValueUnknown, e);
                    }
                    for r in held.iter().flatten() {
                        if matches!(r, Region::Param(_)) {
                            return self.refuse(Form::CalleeValueUnknown, e);
                        }
                        regions.push(*r);
                    }
                }
                None if self.npos_d(dest_ty, e)? != 0 => return self.refuse(Form::CalleeValueUnknown, e),
                None => {}
            }
            let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
            let mut at = 0u32;
            for v in args.iter() {
                at += self.npos_d(v.ty, e)?;
                ends.push(at);
            }
            if usize::try_from(at + self.npos_d(dest_ty, e)?).ok() != Some(regions.len()) {
                return self.refuse(Form::CalleeValueUnknown, e);
            }
            let laid = Laid { regions: &regions, ends: &ends, bounds: &[] };
            if !self.call_laid(laid, None, &[], args, dest, dest_ty, e)? {
                return self.refuse(Form::CalleeValueUnknown, e);
            }
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        // Fn pointer parameter (`fn(&u8) -> &u8`, or `&fn(..)`):
        let PNode::FnPtr(list, _) = self.head_node(declared, e)? else {
            return self.refuse(Form::CalleeValueUnknown, e);
        };
        let kids = self.head_list(list, e)?.len();
        if kids != args.len() + 1 {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        // The parameter's own place: its positions hold the regions of its column slots
        // (`signature` laid them from the same column), so a FREE lifetime of the callable (`f:
        // fn() -> &'a u8`: the fn's `'a`, a `Param`) is the origin at the position that holds it
        // (lane B15: the Reference, "Function pointer types" and "Subtyping and Variance", the
        // call relates the pointer's free regions; its bound ones, `Late`, are fresh per call).
        // The column indexes the enclosing fn's parameters. A nested flow indexes its
        // captures first and can reach the callable through a borrowed environment.
        let (own, own_ty) = self.binding_place(self.body.params[param], e)?;
        let mut held: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        if own_ty.wrap.is_some() || !self.lay_positions(declared, slots, &mut held, e)? || held.iter().any(Option::is_none) {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        let pi = self.info(own);
        if !pi.exact || pi.len < peel_skip || (pi.len - peel_skip) as usize != held.len() {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        // The pointer's positions are its inputs' then its output's (`npos`, a fn pointer has
        // none of its own): split by each argument's count, in the body's own table (the
        // declared kids are the columns' table's: never counted here); `call_laid` checks
        // each argument's and the destination's place against these ends.
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
        for r in held.iter().flatten() {
            regions.push(*r);
        }
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut at = 0u32;
        for v in args.iter() {
            at += self.npos_d(v.ty, e)?;
            ends.push(at);
        }
        if at as usize > regions.len() {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        // Each free region of the parameter's type, at its first position. Under the control
        // (`Facts::fn_binders` false) a bound one is taken as free too: one region for every
        // call, the parameter's own.
        let binders = facts.fn_binders();
        let mut seed: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        for (j, &r) in regions.iter().enumerate() {
            let free = matches!(r, Region::Param(_)) || (!binders && matches!(r, Region::Late { .. }));
            if free && !seed.iter().any(|(x, _)| *x == r) {
                seed.push((r, self.origs[(pi.start + peel_skip + j as u32) as usize]));
            }
        }
        let laid = Laid { regions: &regions, ends: &ends, bounds: &[] };
        if !self.call_laid(laid, None, &seed, args, dest, dest_ty, e)? {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        self.stats.calls_by_signature += 1;
        Ok(())
    }

    /// Calls a value whose underlying type is a generic parameter `PNode::Param(p)` bounded by
    /// `Fn`/`FnMut`/`FnOnce` in the body owner's clauses (`Facts::regions().clauses(owner)`).
    /// Returns `Ok(true)` when handled, `Ok(false)` when the callee is not such a parameter or
    /// its callable signature could not be read cleanly.
    fn call_through_fn_param(&mut self, callee_ty: TyD, args: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        if dest_ty.wrap.is_some() || args.iter().any(|v| v.ty.wrap.is_some()) {
            return Ok(false);
        }
        let facts: &'b F = self.facts;
        let mut t = callee_ty.ty;
        let mut steps = 0u32;
        loop {
            steps += 1;
            if steps > 16 {
                return Ok(false);
            }
            match self.node(t, e)? {
                PNode::Ref(_, inner) => t = inner,
                PNode::Adt(k, l) if facts.adt(k).is_some_and(|a| a.is_box) => {
                    let Some(&inner) = self.list(l, e)?.first() else { return Ok(false) };
                    t = inner;
                }
                _ => break,
            }
        }
        let PNode::Param(param_idx) = self.node(t, e)? else {
            return Ok(false);
        };
        let clauses = match facts.regions().clauses(self.body.owner) {
            Ok(c) => c,
            Err(RegionsUnread { item, why }) => return self.refuse(Form::RegionsUnread { item, why }, e),
        };
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let out_name = Name::of("Output");
        let mut args_match: Option<(PTy, &[Region])> = None;
        let mut out_match: Option<(PTy, &[Region])> = None;
        for c in clauses {
            let ct = c.col.ty();
            match blaze_borrow::RegionTypes::node(&heads, ct) {
                Ok(PNode::ExTrait(tr, _)) if facts.is_fn_trait(tr) => {
                    let Ok(kids) = blaze_borrow::RegionTypes::kids(&heads, ct) else { continue };
                    let (Some(subj), Some(tup)) = (kids.get(0), kids.get(1)) else { continue };
                    if !matches!(blaze_borrow::RegionTypes::node(&heads, subj), Ok(PNode::Param(q)) if q == param_idx) {
                        continue;
                    }
                    let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, tup) else { continue };
                    let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, tup) else { continue };
                    if tkids.len() as usize != args.len() {
                        continue;
                    }
                    let Some(tslots) = c.col.child(1) else { continue };
                    if let Some((pt, ps)) = args_match {
                        if pt != tup || ps != tslots {
                            return Ok(false);
                        }
                    } else {
                        args_match = Some((tup, tslots));
                    }
                }
                Ok(PNode::ExProj { trait_: tr, name, term, .. }) if facts.is_fn_trait(tr) && name == out_name => {
                    let Ok(kids) = blaze_borrow::RegionTypes::kids(&heads, ct) else { continue };
                    let (Some(subj), Some(tup)) = (kids.get(0), kids.get(1)) else { continue };
                    if !matches!(blaze_borrow::RegionTypes::node(&heads, subj), Ok(PNode::Param(q)) if q == param_idx) {
                        continue;
                    }
                    let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, tup) else { continue };
                    let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, tup) else { continue };
                    if tkids.len() as usize != args.len() {
                        continue;
                    }
                    let Some(oslots) = c.col.child(2) else { continue };
                    if let Some((pt, ps)) = out_match {
                        if pt != term || ps != oslots {
                            return Ok(false);
                        }
                    } else {
                        out_match = Some((term, oslots));
                    }
                }
                _ => {}
            }
        }
        let Some((args_tup, args_slots)) = args_match else {
            return Ok(false);
        };
        self.call_through_laid_fn(args_tup, args_slots, out_match, args, dest, dest_ty, e)
    }

    /// Calls a parameter whose declared type (after peeling `&`/`&mut`/`Box`) is `dyn Fn*`.
    fn call_through_dyn_fn(&mut self, list: List, slots: &[Region], args: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        let facts: &'b F = self.facts;
        // `slots[0]` is the trait object's own lifetime; `slots[1..]` are its existential bounds.
        let Some(mut rest) = slots.get(1..) else { return Ok(false) };
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let out_name = Name::of("Output");
        let mut args_match: Option<(PTy, &[Region])> = None;
        let mut out_match: Option<(PTy, &[Region])> = None;
        for &bound in self.head_list(list, e)? {
            let Ok(bc) = blaze_borrow::RegionTypes::count(&heads, bound) else { return Ok(false) };
            let bn = bc.get() as usize;
            let Some((bslots, after)) = rest.split_at_checked(bn) else { return Ok(false) };
            rest = after;
            match blaze_borrow::RegionTypes::node(&heads, bound) {
                Ok(PNode::ExTrait(tr, _)) if facts.is_fn_trait(tr) => {
                    let Ok(kids) = blaze_borrow::RegionTypes::kids(&heads, bound) else { continue };
                    // In `Dynamic`, existential `Self` is omitted so `ArgsTuple` is `kids[0]`.
                    let Some(tup) = kids.get(0) else { continue };
                    let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, tup) else { continue };
                    let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, tup) else { continue };
                    if tkids.len() as usize != args.len() {
                        continue;
                    }
                    let Ok(tc) = blaze_borrow::RegionTypes::count(&heads, tup) else { return Ok(false) };
                    if tc.get() as usize != bslots.len() {
                        return Ok(false);
                    }
                    args_match = Some((tup, bslots));
                }
                Ok(PNode::ExProj { trait_: tr, name, term, .. }) if facts.is_fn_trait(tr) && name == out_name => {
                    let Ok(tc) = blaze_borrow::RegionTypes::count(&heads, term) else { return Ok(false) };
                    let tn = tc.get() as usize;
                    let Some(oslots) = bslots.get(bslots.len().saturating_sub(tn)..) else { return Ok(false) };
                    if oslots.len() != tn {
                        return Ok(false);
                    }
                    out_match = Some((term, oslots));
                }
                _ => {}
            }
        }
        if !rest.is_empty() {
            return Ok(false);
        }
        let Some((args_tup, args_slots)) = args_match else {
            return Ok(false);
        };
        self.call_through_laid_fn(args_tup, args_slots, out_match, args, dest, dest_ty, e)
    }

    /// Relates a call through an `Fn*` callable whose laid `ArgsTuple` and optional `Output`
    /// columns are known in the body owner's region numbering.
    #[expect(clippy::too_many_arguments, reason = "laid callable signature, call operands, destination and site")]
    fn call_through_laid_fn(
        &mut self,
        args_tup: PTy,
        args_slots: &[Region],
        out_match: Option<(PTy, &[Region])>,
        args: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        if R::ONE && self.call_value_one(args_tup, args_slots, out_match, args, dest, dest_ty, e)? {
            return Ok(true);
        }
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut at = 0u32;
        for v in args.iter() {
            at += self.npos_d(v.ty, e)?;
            ends.push(at);
        }
        let dlen = self.info(dest).len;
        let mut held: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        let laid_ok = self.lay_positions(args_tup, args_slots, &mut held, e)?;
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
        if laid_ok && held.len() == at as usize && !held.iter().any(Option::is_none) {
            for r in held.iter().flatten() {
                regions.push(*r);
            }
        } else if dlen == 0 && !args_slots.contains(&Region::Static) {
            // The output carries no lifetimes and no input demands `'static`. Either an input
            // is an unnormalized alias of the same body (`on_done(id, out: C::Output)`), or a
            // 0-slot projection normalized to a compound in the body (`Zip::spec_fold` where
            // `Self::Item` normalized to `(A::Item, B::Item)`).
            if laid_ok && held.len() == at as usize {
                for (j, slot) in held.iter().enumerate() {
                    regions.push(slot.unwrap_or(Region::Late { binder: u32::MAX, var: j as u32 }));
                }
            } else if args_slots.is_empty() {
                for j in 0..at {
                    regions.push(Region::Late { binder: u32::MAX, var: j });
                }
            } else {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
        if dlen > 0 {
            let Some((out_ty, out_slots)) = out_match else {
                return Ok(false);
            };
            if !self.lay_positions(out_ty, out_slots, &mut held, e)? || held.iter().any(Option::is_none) {
                return Ok(false);
            }
            if held.len() != dlen as usize {
                return Ok(false);
            }
            for r in held.iter().flatten() {
                regions.push(*r);
            }
        }
        let mut seed: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        for &r in &regions {
            if matches!(r, Region::Param(_)) && !seed.iter().any(|(x, _)| *x == r) {
                if self.in_nested {
                    // A nested closure/async flow does not map the enclosing fn's free
                    // lifetime parameters to origins here: refuse rather than invent one.
                    return Ok(false);
                }
                seed.push((r, self.universal(r)));
            }
        }
        let laid = Laid { regions: &regions, ends: &ends, bounds: &[] };
        self.call_laid(laid, None, &seed, args, dest, dest_ty, e)
    }

    /// Calls a fn pointer loaded from a struct field (`(self.cb)(args)`, or extracted via a
    /// pattern like `let Some(dup) = self.duplicator; dup(x)`), reading the field's declared
    /// regions from `Facts::regions().field(...)` (or `Facts::field_regions(...)`).
    fn call_through_field_fn_ptr(&mut self, mut c: ExprId, callee_val: Val, args: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
        if dest_ty.wrap.is_some() || args.iter().any(|v| v.ty.wrap.is_some()) || callee_val.ty.wrap.is_some() {
            return Ok(false);
        }
        let PNode::FnPtr(list, _) = self.node(callee_val.ty.ty, e)? else {
            return Ok(false);
        };
        if self.list(list, e)?.len() != args.len() + 1 {
            return Ok(false);
        }
        let mut dsteps = 0usize;
        while let Expr::Paren(i) = self.expr(c)? {
            dsteps += 1;
            if dsteps > self.body.exprs.len() {
                return self.refuse(Form::Bound, e);
            }
            c = i;
        }
        let facts: &'b F = self.facts;
        let (adt, variant, field_idx, adt_args) = match self.expr(c)? {
            Expr::Field { base, .. } => {
                let Some(field_idx) = self.field_of(c) else {
                    return Ok(false);
                };
                let mut bt = self.adjusted_ty(base)?;
                let mut bsteps = 0u32;
                loop {
                    bsteps += 1;
                    if bsteps > 16 {
                        return Ok(false);
                    }
                    match self.node(bt, e)? {
                        PNode::Ref(_, inner) => bt = inner,
                        PNode::Adt(k, l) if facts.adt(k).is_some_and(|a| a.is_box) => {
                            let Some(&inner) = self.list(l, e)?.first() else { return Ok(false) };
                            bt = inner;
                        }
                        _ => break,
                    }
                }
                let PNode::Adt(adt, adt_args) = self.node(bt, e)? else {
                    return Ok(false);
                };
                (adt, 0u32, field_idx, Some(adt_args))
            }
            Expr::Local(p) => {
                let Some(&(_, adt, variant, field_idx)) = self.local_fn_field.iter().find(|&&(pat, ..)| pat == p) else {
                    return Ok(false);
                };
                (adt, variant, field_idx, None)
            }
            _ => return Ok(false),
        };
        let place_info = match callee_val.at {
            At::Place(p) => Some(self.info(p)),
            At::Const => None,
        };
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut at = 0u32;
        for v in args.iter() {
            at += self.npos_d(v.ty, e)?;
            ends.push(at);
        }
        let dlen = self.npos_d(dest_ty, e)?;
        // Preferred path: read the field's declared region column directly from `Facts::regions()`
        // (peeling `&`/`&mut` and zero-lifetime wrappers like `Option<fn(..)>` / `Box<fn(..)>`).
        if let Ok(col) = facts.regions().field(adt, variant, field_idx) {
            let arity = crate::one_call::FactsArity { facts };
            let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
            let mut declared = col.ty();
            let mut slots = col.as_slice();
            for _ in 0..16 {
                match self.head_node(declared, e)? {
                    PNode::Ref(_, inner) => {
                        let Some(rest) = slots.get(1..) else { break };
                        declared = inner;
                        slots = rest;
                    }
                    PNode::Adt(k, l) if facts.adt(k).is_some_and(|a| a.lifetimes == 0) => {
                        let list = self.head_list(l, e)?;
                        let Some(&inner) = list.first() else { break };
                        if list.len() > 2 {
                            break;
                        }
                        let Ok(ic) = blaze_borrow::RegionTypes::count(&heads, inner) else { break };
                        let Some(inner_slots) = slots.get(..ic.get() as usize) else { break };
                        declared = inner;
                        slots = inner_slots;
                    }
                    _ => break,
                }
            }
            if let PNode::FnPtr(fn_list, _) = self.head_node(declared, e)? {
                let fn_kids = self.head_list(fn_list, e)?;
                if fn_kids.len() == args.len() + 1 && slots.len() == (at + dlen) as usize {
                    let mut counts_ok = true;
                    let mut prev = 0u32;
                    for (i, &kid) in fn_kids[..args.len()].iter().enumerate() {
                        let want = ends[i] - prev;
                        prev = ends[i];
                        if blaze_borrow::RegionTypes::count(&heads, kid).ok().map(|c| c.get()) != Some(want) {
                            counts_ok = false;
                            break;
                        }
                    }
                    if counts_ok && blaze_borrow::RegionTypes::count(&heads, fn_kids[args.len()]).ok().map(|c| c.get()) == Some(dlen) {
                        let binders = facts.fn_binders();
                        let mut seed: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
                        let mut seed_ok = true;
                        for (j, &r) in slots.iter().enumerate() {
                            let free = matches!(r, Region::Param(_)) || (!binders && matches!(r, Region::Late { .. }));
                            if free && !seed.iter().any(|(x, _)| *x == r) {
                                let Some(pi) = place_info else {
                                    seed_ok = false;
                                    break;
                                };
                                if !pi.exact || pi.len as usize != slots.len() {
                                    seed_ok = false;
                                    break;
                                }
                                seed.push((r, self.origs[(pi.start + j as u32) as usize]));
                            }
                        }
                        if seed_ok {
                            let laid = Laid { regions: slots, ends: &ends, bounds: &[] };
                            if self.call_laid(laid, None, &seed, args, dest, dest_ty, e)? {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        let Some(fregs) = facts.field_regions(adt, variant, field_idx) else {
            return Ok(false);
        };
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut seed: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        let mut pos = 0u32;
        for &fr in fregs.iter() {
            match fr {
                FieldRegion::Static => {
                    regions.push(Region::Static);
                    pos += 1;
                }
                FieldRegion::Bound(k) => {
                    regions.push(Region::Late { binder: 0, var: k });
                    pos += 1;
                }
                FieldRegion::Lifetime(i) => {
                    let Some(pi) = place_info else { return Ok(false) };
                    if !pi.exact {
                        return Ok(false);
                    }
                    let Some(&orig) = self.origs.get((pi.start + pos) as usize) else { return Ok(false) };
                    let r = Region::Param(i);
                    if !seed.iter().any(|(x, _)| *x == r) {
                        seed.push((r, orig));
                    }
                    regions.push(r);
                    pos += 1;
                }
                FieldRegion::Arg(arg_i) => {
                    let Some(al) = adt_args else { return Ok(false) };
                    let Some(&arg_ty) = self.list(al, e)?.get(arg_i as usize) else { return Ok(false) };
                    if self.npos(arg_ty, e)? != 0 {
                        return Ok(false);
                    }
                }
            }
        }
        if at as usize > regions.len() {
            return Ok(false);
        }
        let laid = Laid { regions: &regions, ends: &ends, bounds: &[] };
        self.call_laid(laid, None, &seed, args, dest, dest_ty, e)
    }

    fn break_done(&mut self, e: ExprId, has: bool) -> Result<(), Refusal> {
        let target = match self.expr(e)? {
            Expr::Break { target, .. } => target,
            _ => return self.refuse(Form::Malformed, e),
        };
        let tg = match self.targets.iter().rev().find(|t| t.e == target) {
            Some(t) => *t,
            None => return self.refuse(Form::BreakTarget, e),
        };
        if has {
            let v = self.pop(e)?;
            if let Some(d) = tg.dest {
                let dp = self.place_of(d);
                let dt = self.local_ty[d.0 as usize];
                self.assign_val(dp, dt, v, e)?;
            }
        }
        self.exit_drops(tg.scopes, tg.temps, e)?;
        self.b.goto(tg.exit, self.span(e));
        self.fresh();
        let ty = TyD::of(self.expr_ty(e)?);
        self.vals.push(Val { at: At::Const, ty });
        Ok(())
    }

    /// Whether the `if` condition `e` is pattern-matching: a `let`, or a `&&` chain holding one
    /// (through parentheses), as `branch` lowers it. Bounded by the chain's length (rule 18).
    fn cond_has_let(&self, e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<ExprId, A> = Vec::new_in(self.alloc);
        stack.push(e);
        let mut steps = 0u32;
        while let Some(x) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.expr(x)? {
                Expr::Let { .. } => return Ok(true),
                Expr::Paren(i) => stack.push(i),
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    stack.push(lhs);
                    stack.push(rhs);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// A condition as control flow: `&&`, `||` and `!` by their short circuit, `let` as a match
    /// of one pattern, anything else a switch on its value.
    fn branch(&mut self, e: ExprId, t: Block, f: Block) -> Result<(), Refusal> {
        match self.expr(e)? {
            Expr::Paren(i) => self.tasks.push(Task::Branch(i, t, f)),
            Expr::Binary { op: BinOp::And, lhs, rhs } => {
                let mid = self.b.block();
                self.tasks.push(Task::Branch(rhs, t, f));
                self.tasks.push(Task::Start(mid));
                self.tasks.push(Task::Branch(lhs, mid, f));
            }
            Expr::Binary { op: BinOp::Or, lhs, rhs } => {
                let mid = self.b.block();
                self.tasks.push(Task::Branch(rhs, t, f));
                self.tasks.push(Task::Start(mid));
                self.tasks.push(Task::Branch(lhs, t, mid));
            }
            Expr::Unary { op: UnOp::Not, operand } if self.adjust_rows(e).0 == self.adjust_rows(e).1 && self.callee_of(e).is_none() => {
                self.tasks.push(Task::Branch(operand, f, t));
            }
            Expr::Let { pat, init } => {
                self.tasks.push(Task::LetBranch(pat, init, t, f));
                self.tasks.push(Task::Eval(init, false));
            }
            _ => {
                self.tasks.push(Task::BranchOn(e, t, f));
                self.tasks.push(Task::Eval(e, false));
            }
        }
        Ok(())
    }

    fn match_dispatch(&mut self, e: ExprId, dest: Option<Local>) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        let p = self.as_place(v, e)?;
        let arms = match self.expr(e)? {
            Expr::Match { arms, .. } => arms,
            _ => return self.refuse(Form::Malformed, e),
        };
        let arms = match self.body.arms.get(arms.range()) {
            Some(a) => a,
            None => return self.refuse(Form::Malformed, e),
        };
        let ty = TyD::of(self.expr_ty(e)?);
        if arms.is_empty() {
            self.b.stmt(StmtKind::Unreachable, self.span(e));
            self.fresh();
            self.vals.push(Val { at: At::Const, ty });
            return Ok(());
        }
        let mut binds: Vec<Block, A> = Vec::new_in(self.alloc);
        for _ in arms {
            binds.push(self.b.block());
        }
        let join = self.b.block();
        let unreachable = self.b.block();
        self.b.switch(p, &binds, self.span(e));
        let val = Val { at: At::Place(p), ty: v.ty };
        // Exec order per arm: start, scope, bindings, guard, body, arm end; then the join.
        let push = match dest {
            Some(d) => Val { at: At::Place(self.place_of(d)), ty },
            None => Val { at: At::Const, ty },
        };
        self.tasks.push(Task::PushVal(push));
        if let Some(d) = dest {
            self.tasks.push(Task::SettleDest(d, e));
        }
        self.tasks.push(Task::Start(join));
        self.tasks.push(Task::Unreachable(unreachable, e));
        for (i, arm) in arms.iter().enumerate().rev() {
            let next = binds.get(i + 1).copied().unwrap_or(unreachable);
            self.tasks.push(Task::ArmEnd(arm.body, dest, join, true));
            self.tasks.push(Task::Eval(arm.body, false));
            // The arm's body is a temporary scope of its own (`close_arm_temps`).
            self.tasks.push(Task::OpenTemps);
            // A guard over a by-value binding of a non-`Copy` type (pattern_forms.rs, "Match
            // guards"): bound by shared reference for the guard, moved once it holds.
            let moves = match arm.guard {
                Some(_) => self.guard_moves(arm.pat)? && self.plant != Plant::GuardBindsByValue,
                None => false,
            };
            if let Some(g) = arm.guard {
                let body_b = self.b.block();
                if moves {
                    self.tasks.push(Task::GuardMoves(arm.pat, val));
                }
                self.tasks.push(Task::FakeUse(Local(0), g));
                self.tasks.push(Task::Start(body_b));
                self.tasks.push(Task::Branch(g, body_b, next));
                self.tasks.push(Task::FakeBorrow(e, val));
            }
            if let (true, Some(g)) = (moves, arm.guard) {
                self.tasks.push(Task::GuardRefs(arm.pat, val, g));
            } else {
                self.tasks.push(Task::BindPat(arm.pat, val));
            }
            self.tasks.push(Task::OpenScope);
            self.tasks.push(Task::Start(binds[i]));
        }
        Ok(())
    }

    /// Whether a pattern binds a non-`Copy` value by value (its guard would see it moved).
    fn guard_moves(&self, p: PatId) -> Result<bool, Refusal> {
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc);
        stack.push(p);
        let mut steps = 0usize;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, p);
            }
            let pat = match self.body.pats.get(p.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            if let Pat::Bind { ann, .. } = pat {
                let lt = self.ck.local_ty.get(p.ix()).copied().unwrap_or(PTy(0));
                // By value when no `ref` is written and the binding's type is the matched
                // place's (`pat_ty`): one bound by reference under a default binding mode has
                // `&pat_ty` (the Reference, "Binding modes"). The binding's type alone does not
                // say it: `Some(count)` over `Option<&mut usize>` binds a `&mut usize` BY VALUE,
                // and taken as by reference it was moved before the guard's fake borrow of the
                // scrutinee, a false E0382 at the `match` (tempo zones-antithesis checks.rs:434,
                // zones/node role.rs:767).
                let Some(&pt) = self.ck.pat_ty.get(p.ix()) else { return self.refuse_pat(Form::Malformed, p) };
                let by_value = ann.by_ref.is_none() && pt == lt;
                if by_value && !self.is_copy(TyD::of(lt), self.body.root)? {
                    return Ok(true);
                }
            }
            self.sub_pats(pat, &mut stack)?;
        }
        Ok(false)
    }

    fn sub_pats(&self, pat: Pat, out: &mut Vec<PatId, A>) -> Result<(), Refusal> {
        let b = self.body;
        let list = |l: List| b.pat_lists.get(l.range()).unwrap_or(&[]);
        match pat {
            Pat::Bind { sub: Some(s), .. } => out.push(s),
            Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => out.extend_from_slice(list(elems)),
            Pat::Or { alts } => out.extend_from_slice(list(alts)),
            Pat::Ref { inner, .. } => out.push(inner),
            Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                for f in b.field_pats.get(fields.range()).unwrap_or(&[]) {
                    out.push(f.pat);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn binds(&self, p: PatId) -> Result<bool, Refusal> {
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc);
        stack.push(p);
        let mut steps = 0usize;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, p);
            }
            let pat = match self.body.pats.get(p.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            if let Pat::Bind { .. } = pat {
                return Ok(true);
            }
            self.sub_pats(pat, &mut stack)?;
        }
        Ok(false)
    }

    /// Binds `p` against the place in `v`: a work list of (pattern, place, its type, the default
    /// binding mode: 0 move, 1 `ref`, 2 `ref mut`), the checker's peeling rule repeated (a
    /// non-reference pattern derefs a reference place and makes the mode by-reference).
    fn bind_pattern(&mut self, p: PatId, v: Val) -> Result<(), Refusal> {
        self.bind_pattern_from(p, v, None)
    }

    /// [`Self::bind_pattern`], where `from` is the span of the expression the value came from
    /// (a `let`'s initializer). A by-value binding of the WHOLE pattern reads that place there,
    /// so its move or copy, and any finding about it, is at the initializer: `let c = a;` uses
    /// `a` at `a` (the Reference, "Moved and copied types"), not at `c`. A binding inside the
    /// pattern keeps its own span.
    fn bind_pattern_from(&mut self, p: PatId, v: Val, from: Option<Span>) -> Result<(), Refusal> {
        self.bind_walk(p, v, from, BindPass::Whole)
    }

    /// [`Self::bind_pattern_from`] for one pass (pattern_forms.rs [`BindPass`]): the whole
    /// pattern, or a guarded arm's bindings before and after its guard. Its work list also
    /// holds an or-pattern's blocks ([`BindItem`]): each alternative binds in a block of its
    /// own after a switch on the place, into the locals of its names' first bindings.
    fn bind_walk(&mut self, p: PatId, v: Val, from: Option<Span>, pass: BindPass) -> Result<(), Refusal> {
        let whole = p;
        let root = self.body.root;
        let place = self.as_place(v, root)?;
        // Each name's first binding (an or-pattern's alternatives bind one variable).
        let mut alias: Vec<(Name, PatId), A> = Vec::new_in(self.alloc);
        self.first_binds(p, &mut alias)?;
        let mut stack: Vec<BindItem, A> = Vec::new_in(self.alloc);
        stack.push(BindItem::Pat(p, place, v.ty, 0));
        let mut steps = 0usize;
        while let Some(item) = stack.pop() {
            steps += 1;
            // Each pattern once, and each or-pattern's alternatives a start and a goto more.
            if steps > 3 * self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, whole);
            }
            let (p, mut place, mut ty, mut mode) = match item {
                BindItem::Pat(p, pl, t, m) => (p, pl, t, m),
                BindItem::Start(b) => {
                    self.b.start(b);
                    continue;
                }
                BindItem::Goto(b, at) => {
                    self.b.goto(b, self.pat_span(at));
                    continue;
                }
            };
            let pat = match self.body.pats.get(p.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let union_pat = self.ck.union_pat_of(p);
            let peel = union_pat.is_some()
                || matches!(pat, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. } | Pat::Path(_) | Pat::Range { .. });
            if peel {
                for _ in 0..MOST_POSITIONS {
                    let (m, inner) = match (ty.wrap, self.node(ty.ty, root)?) {
                        (Some(m), _) => (m, TyD::of(ty.ty)),
                        (None, PNode::Ref(m, i)) => (m, TyD::of(i)),
                        _ => break,
                    };
                    place = self.project(place, ty, Proj::Deref, inner, false, root)?;
                    ty = inner;
                    mode = match (m, mode) {
                        (Mutbl::Not, _) => 1,
                        (Mutbl::Mut, 0) => 2,
                        (Mutbl::Mut, x) => x,
                    };
                }
            }
            match pat {
                Pat::Missing => return self.refuse_pat(Form::Missing, p),
                Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
                Pat::Bind { ann, sub, name, .. } => {
                    let lt = match self.ck.local_ty.get(p.ix()) {
                        Some(&t) => TyD::of(t),
                        None => return self.refuse_pat(Form::Malformed, p),
                    };
                    if !self.place_field_source.is_empty() && matches!(self.node(lt.ty, root)?, PNode::FnPtr(..)) {
                        let mut cur = place.0;
                        for _ in 0..8 {
                            if let Some(&(_, adt, variant, fi)) = self.place_field_source.iter().find(|&&(pl, ..)| pl.0 == cur) {
                                self.local_fn_field.push((p, adt, variant, fi));
                                break;
                            }
                            let Some(info) = self.pinfo.get(cur as usize) else { break };
                            if info.parent == u32::MAX {
                                break;
                            }
                            cur = info.parent;
                        }
                    }
                    if let Some(blaze_typeck::UnionPat::Variant(v)) = union_pat
                        && ty.wrap.is_none()
                        && let PNode::Union(ul) = self.node(ty.ty, root)?
                    {
                        let members = self.list(ul, root)?;
                        let Some(v_idx) = usize::try_from(v).ok() else {
                            return self.refuse_pat(Form::Malformed, p);
                        };
                        let Some(&mt) = members.get(v_idx) else {
                            return self.refuse_pat(Form::Malformed, p);
                        };
                        let mty = TyD::of(mt);
                        place = self.project(place, ty, Proj::Field(v), mty, false, root)?;
                        ty = mty;
                    }
                    let is_residual = matches!(union_pat, Some(blaze_typeck::UnionPat::Residual));
                    let by_ref = match ann.by_ref {
                        Some(m) => Some(m),
                        None if ann.mutable => None,
                        None => match mode {
                            1 => Some(Mutbl::Not),
                            2 => Some(Mutbl::Mut),
                            _ => None,
                        },
                    };
                    // A guarded arm (pattern_forms.rs): before the guard a by-value binding is
                    // a shared reference to its place; after it, the by-reference bindings are
                    // already made and only the by-value ones are bound.
                    let skip = match (pass, by_ref) {
                        (BindPass::GuardRefs(g), None) => {
                            self.guard_ref_binding(p, place, ty, lt, g)?;
                            true
                        }
                        (BindPass::GuardMoves, Some(_)) => true,
                        (BindPass::GuardMoves, None) => {
                            self.upvar_places.retain(|u| u.0 != p);
                            false
                        }
                        _ => false,
                    };
                    if skip {
                        if let Some(s) = sub {
                            stack.push(BindItem::Pat(s, place, ty, mode));
                        }
                        continue;
                    }
                    let l = self.binding_local(p, name, lt, &alias)?;
                    let lp = self.place_of(l);
                    self.b.stmt(StmtKind::StorageLive(l), self.pat_span(p));
                    match by_ref {
                        None => {
                            if !is_residual && (ty.wrap.is_some() || ty.ty != lt.ty) {
                                return self.refuse_pat(Form::BindingMode, p);
                            }
                            let op = self.operand(Val { at: At::Place(place), ty }, root)?;
                            let at = match from {
                                Some(s) if p == whole => s,
                                _ => self.pat_span(p),
                            };
                            self.b.assign(lp, op, at);
                            self.relate(place, lp, lt, root)?;
                        }
                        Some(m) => {
                            if !is_residual {
                                match self.node(lt.ty, root)? {
                                    PNode::Ref(rm, inner) if rm == m && ty.wrap.is_none() && inner == ty.ty => {}
                                    _ => return self.refuse_pat(Form::BindingMode, p),
                                }
                            }
                            let kind = if m == Mutbl::Mut { BorrowKind::Mut } else { BorrowKind::Shared };
                            self.borrow(place, ty, kind, l, root)?;
                        }
                    }
                    if let Some(s) = sub {
                        stack.push(BindItem::Pat(s, place, ty, mode));
                    }
                }
                Pat::Ref { inner, .. } => {
                    let it = match (ty.wrap, self.node(ty.ty, root)?) {
                        (Some(_), _) => TyD::of(ty.ty),
                        (None, PNode::Ref(_, i)) => TyD::of(i),
                        _ => return self.refuse_pat(Form::Malformed, p),
                    };
                    let q = self.project(place, ty, Proj::Deref, it, false, root)?;
                    stack.push(BindItem::Pat(inner, q, it, 0));
                }
                Pat::Or { alts } => self.or_alternatives(p, alts, place, ty, mode, pass, &mut stack)?,
                Pat::Slice { elems, rest } => self.slice_elements(p, elems, rest, place, ty, mode, &mut stack)?,
                Pat::Tuple { elems, rest } => {
                    let tys = match self.node(ty.ty, root)? {
                        PNode::Tuple(l) => self.list(l, root)?,
                        _ => return self.refuse_pat(Form::Malformed, p),
                    };
                    let es = self.body.pat_lists.get(elems.range()).unwrap_or(&[]);
                    for (j, &sp) in es.iter().enumerate() {
                        let fi = field_index(j, es.len(), tys.len(), rest);
                        let Some(&ft) = tys.get(fi as usize) else {
                            return self.refuse_pat(Form::Malformed, sp);
                        };
                        if matches!(self.body.pats.get(sp.ix()), Some(Pat::Wild)) {
                            continue;
                        }
                        let q = self.project(place, ty, Proj::Field(fi), TyD::of(ft), false, root)?;
                        stack.push(BindItem::Pat(sp, q, TyD::of(ft), mode));
                    }
                }
                Pat::TupleStruct { elems, rest, .. } => {
                    let es = self.body.pat_lists.get(elems.range()).unwrap_or(&[]);
                    let mut subs: Vec<(u32, PatId), A> = Vec::new_in(self.alloc);
                    let (adt, variant) = self.variant_of(p)?;
                    let n = match self.facts.field_count(adt, variant) {
                        Some(n) => n as usize,
                        None => return self.refuse_pat(Form::UnknownVariant, p),
                    };
                    for (j, &sp) in es.iter().enumerate() {
                        subs.push((field_index(j, es.len(), n, rest), sp));
                    }
                    self.bind_fields(p, place, ty, mode, adt, variant, &subs, &mut stack)?;
                }
                Pat::Struct { fields, .. } => {
                    let (adt, variant) = self.variant_of(p)?;
                    let mut subs: Vec<(u32, PatId), A> = Vec::new_in(self.alloc);
                    for f in self.body.field_pats.get(fields.range()).unwrap_or(&[]) {
                        match self.facts.field(adt, variant, f.name) {
                            Some(i) => subs.push((i, f.pat)),
                            None => return self.refuse_pat(Form::UnknownField, f.pat),
                        }
                    }
                    self.bind_fields(p, place, ty, mode, adt, variant, &subs, &mut stack)?;
                }
                Pat::Record { fields, .. } => {
                    let types = self.ck.types.view();
                    let Ok(PNode::Record(schema, flist)) = types.node(ty.ty) else {
                        return self.refuse_pat(Form::Malformed, p);
                    };
                    let Ok(field_tys) = types.list(flist) else {
                        return self.refuse_pat(Form::Malformed, p);
                    };
                    let mut subs: Vec<(u32, PTy, PatId), A> = Vec::new_in(self.alloc);
                    for f in self.body.field_pats.get(fields.range()).unwrap_or(&[]) {
                        let Some(i) = types.record_field_index(schema, f.name) else {
                            return self.refuse_pat(Form::UnknownField, f.pat);
                        };
                        let Some(&ft) = usize::try_from(i).ok().and_then(|ix| field_tys.get(ix)) else {
                            return self.refuse_pat(Form::Malformed, f.pat);
                        };
                        subs.push((i, ft, f.pat));
                    }
                    for (fi, ft, sp) in subs {
                        if !self.binds(sp)? {
                            continue;
                        }
                        let ft = TyD::of(ft);
                        let q = self.project(place, ty, Proj::Field(fi), ft, false, root)?;
                        stack.push(BindItem::Pat(sp, q, ft, mode));
                    }
                }
            }
        }
        Ok(())
    }

    fn pat_span(&self, p: PatId) -> Span {
        let r = self.body.pat_ranges.get(p.ix()).copied().unwrap_or(Range { start: 0, end: 0 });
        Span { lo: r.start, hi: r.end }
    }

    fn variant_of(&self, p: PatId) -> Result<(ItemKey, u32), Refusal> {
        match self.pat_def(p) {
            Some(Def::Ctor { adt, variant, .. }) => Ok((adt, variant)),
            Some(Def::Adt(adt)) => Ok((adt, 0)),
            _ => self.refuse_pat(Form::UnknownVariant, p),
        }
    }

    /// The fields of an ADT pattern: each binding subpattern's place is the variant's field; its
    /// type is the subpattern's recorded type when the subpattern does not peel (the field's own
    /// type), else the field's type rebuilt from its declaration ([`Self::peeled_field_ty`]),
    /// whose references the work list then peels as it peels a top-level pattern's.
    #[expect(clippy::too_many_arguments, reason = "one pattern's frame")]
    fn bind_fields(
        &mut self,
        p: PatId,
        place: Place,
        ty: TyD,
        mode: u8,
        adt: ItemKey,
        variant: u32,
        subs: &[(u32, PatId)],
        stack: &mut Vec<BindItem, A>,
    ) -> Result<(), Refusal> {
        let root = self.body.root;
        let f = match self.facts.adt(adt) {
            Some(f) => f,
            None => return self.refuse_pat(Form::UnknownAdt, p),
        };
        if f.union {
            return self.refuse_pat(Form::Union, p);
        }
        let base = if f.is_enum { self.project(place, ty, Proj::Downcast(variant), ty, false, root)? } else { place };
        for &(fi, sp) in subs {
            if !self.binds(sp)? {
                continue;
            }
            let spat = match self.body.pats.get(sp.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, sp),
            };
            if matches!(spat, Pat::Bind { .. }) {
                self.local_field_source.push((sp, adt, variant, fi));
            }
            let peels = matches!(spat, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. });
            let own = match self.ck.pat_ty.get(sp.ix()) {
                Some(&t) => t,
                None => return self.refuse_pat(Form::Malformed, sp),
            };
            let ft = if peels { self.peeled_field_ty(ty, adt, variant, fi, sp, own)? } else { TyD::of(own) };
            let q = self.project(base, ty, Proj::Field(fi), ft, f.drop_impl, root)?;
            stack.push(BindItem::Pat(sp, q, ft, mode));
        }
        Ok(())
    }

    /// The type of field `fi` of `variant` of the ADT `ty` (its type arguments' ADT, peeled to
    /// it), under a subpattern `sp` that peels references and whose recorded type is `own`
    /// (the type after its peels: rules/pat.rs `step_pat`). The Reference, "Binding modes":
    /// each reference the subpattern meets is dereferenced and sets the default binding mode
    /// (`&` makes it `ref`; `&mut` makes a move mode `ref mut` and leaves `ref` as it is), and
    /// the field's type is its declared type at the ADT's type arguments: a parameter's
    /// argument ([`FieldLead::Param`], or [`Facts::field_param`] when no lead is given), one
    /// reference to it or to a type without references at its head ([`FieldLead::Ref`]), or
    /// the subpattern's own type when nothing is peeled ([`FieldLead::Plain`]). The work list
    /// then peels the result as it peels any pattern's place, each peel a deref of the field
    /// place, so each binding below is a reborrow `&(*field).sub` (or `&mut`) whose region the
    /// peeled reference's outlives (`borrow`'s supporting prefixes), and a by-value binding
    /// there (`mut x` before edition 2024) moves out of the deref: E0507 (blaze-flow
    /// `MoveOut::Deref`). Checked, not assumed: the peels must land on `own`, else the
    /// binding modes the check took are not the ones rebuilt here (`Form::BindingMode`).
    fn peeled_field_ty(&self, ty: TyD, adt: ItemKey, variant: u32, fi: u32, sp: PatId, own: PTy) -> Result<TyD, Refusal> {
        let root = self.body.root;
        let args: &'c [PTy] = match (ty.wrap, self.node(ty.ty, root)?) {
            (None, PNode::Adt(k, l)) if k == adt => self.list(l, root)?,
            _ => return self.refuse_pat(Form::Malformed, sp),
        };
        let arg = |p: u32| match args.get(p as usize) {
            Some(&t) => Ok(t),
            None => self.refuse_pat(Form::Malformed, sp),
        };
        let field = match self.facts.field_lead(adt, variant, fi) {
            Some(FieldLead::Param(p)) => TyD::of(arg(p)?),
            Some(FieldLead::Ref { mutbl, param: Some(p) }) => TyD { wrap: Some(mutbl), ty: arg(p)? },
            Some(FieldLead::Ref { mutbl, param: None }) => TyD { wrap: Some(mutbl), ty: own },
            Some(FieldLead::Plain) => TyD::of(own),
            Some(FieldLead::Unread) | None => match self.facts.field_param(adt, variant, fi) {
                Some(p) => TyD::of(arg(p)?),
                None => return self.refuse_pat(Form::PeelUnderAdtField, sp),
            },
        };
        let mut t = field;
        for _ in 0..MOST_POSITIONS {
            t = match (t.wrap, self.node(t.ty, root)?) {
                (Some(_), _) => TyD::of(t.ty),
                (None, PNode::Ref(_, i)) => TyD::of(i),
                _ => break,
            };
        }
        if t.wrap.is_some() || t.ty != own {
            return self.refuse_pat(Form::BindingMode, sp);
        }
        Ok(field)
    }

    fn stmt(&mut self, s: StmtId) -> Result<(), Refusal> {
        let st = match self.body.stmts.get(s.ix()) {
            Some(x) => *x,
            None => return self.refuse(Form::Malformed, self.body.root),
        };
        match st {
            TStmt::Item => {}
            TStmt::Expr { expr, .. } => {
                self.tasks.push(Task::CloseTemps(expr));
                self.tasks.push(Task::DiscardStmt(expr));
                self.tasks.push(Task::Eval(expr, false));
                self.tasks.push(Task::OpenTemps);
            }
            TStmt::Let { pat, init: None, .. } => {
                // Declared, not initialized: each binding's storage is live, its value not.
                let from = self.vars.len();
                let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc);
                stack.push(pat);
                let mut steps = 0usize;
                while let Some(p) = stack.pop() {
                    steps += 1;
                    if steps > self.body.pats.len() + 1 {
                        return self.refuse_pat(Form::Bound, p);
                    }
                    let x = match self.body.pats.get(p.ix()) {
                        Some(x) => *x,
                        None => return self.refuse_pat(Form::Malformed, p),
                    };
                    if let Pat::Bind { .. } = x {
                        let lt = match self.ck.local_ty.get(p.ix()) {
                            Some(&t) => TyD::of(t),
                            None => return self.refuse_pat(Form::Malformed, p),
                        };
                        let l = self.new_local_with(lt, false, None, self.body.root)?;
                        self.name_local(l, Identity::Binding(p));
                        self.pat_local[p.ix()] = l.0;
                        self.b.stmt(StmtKind::StorageLive(l), self.pat_span(p));
                        self.vars.push(l);
                    }
                    self.sub_pats(x, &mut stack)?;
                }
                self.place_super_let(s, from, self.body.root)?;
            }
            TStmt::Let { init: Some(init), .. } => {
                let depth = self.scope_marks.len() as u32;
                self.mark_extended(init, depth)?;
                self.tasks.push(Task::CloseTemps(init));
                self.tasks.push(Task::LetInit(s));
                self.tasks.push(Task::Eval(init, false));
                self.tasks.push(Task::OpenTemps);
            }
        }
        Ok(())
    }

    /// The Reference, "Temporary lifetime extension": "the operand of an extending borrow
    /// expression" is extended; extending expressions are the initializer and "the operand(s) of
    /// an extending array, cast, braced struct, or tuple expression", "the final expression of
    /// any extending block expression". `depth`: the drop scopes open at the `let` (its block
    /// the innermost), where an extended temporary goes (`extend_temp`), or
    /// [`STATIC_EXTENDED`] for a constant initializer's.
    fn mark_extended(&mut self, init: ExprId, depth: u32) -> Result<(), Refusal> {
        let mut stack: Vec<ExprId, A> = Vec::new_in(self.alloc);
        stack.push(init);
        let mut steps = 0usize;
        while let Some(e) = stack.pop() {
            steps += 1;
            if steps > self.body.exprs.len() + 1 {
                return self.refuse(Form::Bound, e);
            }
            match self.expr(e)? {
                Expr::Ref { operand, .. } => {
                    // The operand and, down `&ET | *ET | ET[..] | ET.f | (ET)`, each base it is
                    // projected from (rustc region.rs `record_subexpr_extended_temp_scopes`;
                    // the Reference: "If a borrow, dereference, field, or tuple indexing
                    // expression has an extended temporary scope then so does its operand"):
                    // `&mut *cell.borrow_mut()` extends the `RefMut`, borrowed by the
                    // overloaded deref (`Lower::deref`) and so not the borrowed place's root
                    // (tempo fuzz-harness invariants/views.rs:155).
                    let mut base = operand;
                    let mut down = 0usize;
                    loop {
                        down += 1;
                        if down > self.body.exprs.len() + 1 {
                            return self.refuse(Form::Bound, e);
                        }
                        if let Some(x) = self.extended.get_mut(base.ix()) {
                            *x = depth;
                        }
                        base = match self.expr(base)? {
                            Expr::Unary { op: UnOp::Deref, operand: b }
                            | Expr::Ref { operand: b, .. }
                            | Expr::Field { base: b, .. }
                            | Expr::TupleField { base: b, .. }
                            | Expr::Index { base: b, .. }
                            | Expr::Paren(b) => b,
                            _ => break,
                        };
                    }
                    stack.push(operand);
                }
                // "the argument(s) to an extending tuple struct or tuple variant constructor
                // expression" (rustc region.rs: "Recurse into tuple constructors, such as
                // `Some(&temp())`"): `let c = Contracts(&[&cache.contracts]);`,
                // `PcrPolicy(Cow::Borrowed(&[..]))` (tempo fuzz-harness execute.rs:236,
                // precompiles zone_verifier/mod.rs:463).
                Expr::Call { callee, args } => {
                    if matches!(self.expr(callee)?, Expr::Path(_)) && matches!(self.expr_def(callee), Some(Def::Ctor { .. })) {
                        stack.extend_from_slice(self.body.expr_lists.get(args.range()).unwrap_or(&[]));
                    }
                }
                Expr::Paren(i) | Expr::Cast { operand: i, .. } => stack.push(i),
                Expr::Tuple { elems } | Expr::Array { elems } => {
                    stack.extend_from_slice(self.body.expr_lists.get(elems.range()).unwrap_or(&[]));
                }
                Expr::Struct { fields, .. } | Expr::Record { fields, .. } => {
                    for f in self.body.field_inits.get(fields.range()).unwrap_or(&[]) {
                        stack.push(f.expr);
                    }
                }
                Expr::Block { tail: Some(t), .. } => {
                    self.note_extending_block(e);
                    stack.push(t);
                }
                // "the arm(s) of an extending match expression" and "the final expressions of
                // an extending if expression's consequent, else if, and else blocks" (Rust
                // 1.79): `let x = match v { _ => &mk() };`, `let x = if c { &a() } else {
                // &b() };`. Each arm's body is a temporary scope of its own (`ArmEnd`), so
                // without this its borrowed temporary died at the arm's end.
                Expr::If { then, else_, .. } => {
                    stack.push(then);
                    if let Some(x) = else_ {
                        stack.push(x);
                    }
                }
                Expr::Match { arms, .. } => {
                    let Some(arms) = self.body.arms.get(arms.range()) else { return self.refuse(Form::Malformed, e) };
                    for arm in arms {
                        stack.push(arm.body);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn let_init(&mut self, s: StmtId) -> Result<(), Refusal> {
        let (pat, init, else_) = match self.body.stmts.get(s.ix()) {
            Some(TStmt::Let { pat, init, else_, .. }) => (*pat, *init, *else_),
            _ => return self.refuse(Form::Malformed, self.body.root),
        };
        let root = self.body.root;
        let v = self.pop(root)?;
        match else_ {
            Some(x) => {
                self.super_let_else(s, x)?;
                let p = self.as_place(v, x)?;
                let (bind, eb) = (self.b.block(), self.b.block());
                self.b.switch(p, &[bind, eb], self.span(x));
                self.b.start(eb);
                // Exec: the else block (it diverges), then the bindings in `bind`.
                self.tasks.push(Task::BindPat(pat, Val { at: At::Place(p), ty: v.ty }));
                self.tasks.push(Task::Start(bind));
                self.tasks.push(Task::Goto(bind, x));
                self.tasks.push(Task::Discard);
                self.tasks.push(Task::Eval(x, false));
            }
            None => {
                // `let _ = place;` (any pattern binding nothing) reads and moves nothing but
                // mentions the place: lowered as nothing, `let r; { let a = 1; r = &a; } let _ =
                // *r;` was accepted with a dangling borrow. A pattern that binds reads the place
                // through its bindings (each a copy, move or borrow of a part of it).
                if let Some(i) = init
                    && self.binds_nothing(pat)?
                {
                    self.mention(v, i)?;
                }
                // The Reference, "Temporary lifetime extension", "Extending based on patterns":
                // "If the pattern in a let statement is an extending pattern then the temporary
                // scope of the initializer expression is extended" to the enclosing block
                // (`let S { ref x, .. } = make();`, tempo's `PermitFixture::new()`). Before, the
                // initializer's temporary was dropped at the statement's end under the `ref`
                // binding's loan: a false "dropped while the borrow is live".
                if init.is_some()
                    && self.pat_extends(pat)?
                    && let At::Place(p) = v.at
                {
                    let pi = self.info(p);
                    if pi.parent == u32::MAX
                        && let Some(at) = self.temps.iter().rposition(|l| l.0 == pi.local)
                    {
                        let l = self.temps.remove(at);
                        self.temp_left(at);
                        self.vars.push(l);
                    }
                }
                let from = init.map(|i| self.span(i));
                let vars_from = self.vars.len();
                self.bind_pattern_from(pat, v, from)?;
                self.place_super_let(s, vars_from, root)?;
            }
        }
        Ok(())
    }
}

/// The field a positional subpattern names: before `..` its position; after, counted from the end.
fn field_index(j: usize, written: usize, fields: usize, rest: Option<u32>) -> u32 {
    match rest {
        Some(r) if j >= r as usize => (fields - (written - j)) as u32,
        _ => j as u32,
    }
}

/// A fn's regions come from its head record's column (D148, piece L3): the first controls of
/// flow.md "Lifetimes are produced once and carried beside the types", at the lowering.
#[cfg(test)]
mod region_record_tests {
    use std::vec;

    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{ExprId, IntSuffix, ItemKey, Mutbl, Name, PTy, UintTy};

    use crate::region_fixture::{Decls, FnHead, findings};
    use crate::tests::findings_with;
    use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Region, UnreadWhy};

    /// The lowering's facts beside the heads ([`Decls`], laid in the checked record's own table,
    /// region_fixture.rs `lower`: a refused head is `HeadRefused`, an undeclared one `NoHead`);
    /// no ADT; a callee's facts those of its head (none for a refused head).
    struct Record {
        refused: vec::Vec<ItemKey>,
    }

    impl Facts for Record {
        // Every other fact not known here, written out: `Facts` has no default answer.
        fn regions(&self) -> &dyn crate::RegionFacts {
            &crate::NoRegions
        }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
            None
        }
        fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
            None
        }
        fn constant_initializer(&self) -> bool {
            false
        }
        fn fn_binders(&self) -> bool {
            true
        }
        fn closure_bodies_checked(&self) -> bool {
            false
        }
        fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
            None
        }
        fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
            None
        }
        fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
            None
        }
        fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
            None
        }
        fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            None
        }
        fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
            if self.refused.contains(&k) { None } else { Some(CalleeFacts { writes_lifetimes: true, self_ref: false }) }
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
            None
        }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
            None
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    /// The world: `fn f<'a, 'b>(y: &'b u8, x: &'a u8) -> &'a u8` and the two callers.
    struct World {
        fx: Fx,
        f: ItemKey,
        clean: ItemKey,
        control: ItemKey,
        ru8: PTy,
        unit: PTy,
    }

    fn world() -> World {
        let mut fx = Fx::new();
        let u8_ = fx.u8();
        let ru8 = fx.r(Mutbl::Not, u8_);
        let unit = fx.unit();
        let f = fx.func("f", 0, &[ru8, ru8], ru8, vec![]);
        let clean = fx.func("order_clean", 0, &[], unit, vec![]);
        let control = fx.func("order_control", 0, &[], unit, vec![]);
        fx.finish();
        World { fx, f, clean, control, ru8, unit }
    }

    /// `f`'s head with column `f_col` (`f` refused when `None`; its two lifetimes late-bound,
    /// in no bound), and the callers' own heads (`fn() -> ()`, no lifetime).
    fn heads(w: &World, f_col: Option<[Region; 3]>) -> (Record, Decls) {
        let (ru8, unit) = (w.ru8, w.unit);
        let mut decls = Decls::new();
        let mut refused = vec::Vec::new();
        match f_col {
            Some(col) => decls.func(w.f, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[ru8, ru8], output: ru8, slots: &col }),
            None => {
                decls.refuse(w.f);
                refused.push(w.f);
            }
        }
        for owner in [w.clean, w.control] {
            decls.func(owner, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: unit, slots: &[] });
        }
        (Record { refused }, decls)
    }

    /// `let outer = 1u8; let result = { let inner = 2u8; f(&first, &second) }; let used = result;`
    /// with `(first, second)` `(inner, outer)` when `clean` (order_clean: `f(&b, &a)`, `a` the
    /// outer), `(outer, inner)` otherwise (order_control: `f(&b, &a)`, `b` the outer).
    fn body(clean: bool) -> (Bb, ExprId) {
        let mut b = Bb::new();
        let (outer, inner, result, used) = (b.bind("outer"), b.bind("inner"), b.bind("result"), b.bind("used"));
        let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s_outer = b.let_(outer, None, one);
        let two = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s_inner = b.let_(inner, None, two);
        let (first, second) = if clean { (inner, outer) } else { (outer, inner) };
        let (l1, l2) = (b.local(first), b.local(second));
        let (r1, r2) = (b.borrow(Mutbl::Not, l1), b.borrow(Mutbl::Not, l2));
        let callee = b.path(&["f"]);
        let call = b.call(callee, &[r1, r2]);
        let block = b.block(&[s_inner], Some(call));
        let s_result = b.let_(result, None, block);
        let lr = b.local(result);
        let s_used = b.let_(used, None, lr);
        let root = b.block(&[s_outer, s_result, s_used], None);
        (b, root)
    }

    /// THE FIRST CONTROL of D148, through the head record: `f<'a, 'b>(y: &'b u8, x: &'a u8) ->
    /// &'a u8` numbers `'a` `Param(0)` and `'b` `Param(1)` (declaration order), its column
    /// `[Param(1), Param(0), Param(0)]`. order_clean (`f(&b, &a)`, `a` outer) is clean;
    /// order_control (`a` inner) holds `a`'s loan in `result` past `a`'s scope: E0597
    /// (`Invalidated` at the storage end). PLANTED: the column of a reader that numbers by
    /// first appearance, `[Param(0), Param(1), Param(0)]`, ties the result to the FIRST
    /// argument: order_control is then accepted with a dangling borrow, and order_clean is
    /// wrongly caught. The mapping of slots to origins is what decides both.
    ///
    /// Prints: `order_clean: []`, `order_control: ["borrow Invalidated ..`, and the planted pair.
    #[test]
    fn a_callees_regions_come_from_its_head_record() {
        let w = world();
        let (a, b) = (Region::Param(0), Region::Param(1));
        let (record, decls) = heads(&w, Some([b, a, a]));
        let (bc, rc) = body(true);
        let clean = findings(&w.fx, &bc, w.clean, rc, &record, &decls, Plant::None).expect("order_clean is lowered");
        std::println!("order_clean: {clean:?}");
        assert!(clean.is_empty(), "order_clean is clean: {clean:?}");
        let (bx, rx) = body(false);
        let control = findings(&w.fx, &bx, w.control, rx, &record, &decls, Plant::None).expect("order_control is lowered");
        std::println!("order_control: {control:?}");
        assert!(control.iter().any(|l| l.starts_with("borrow Invalidated")), "order_control is E0597: {control:?}");

        let (planted, p_decls) = heads(&w, Some([a, b, a]));
        let p_control = findings(&w.fx, &bx, w.control, rx, &planted, &p_decls, Plant::None).expect("lowered");
        let p_clean = findings(&w.fx, &bc, w.clean, rc, &planted, &p_decls, Plant::None).expect("lowered");
        std::println!("planted first-appearance column: order_control {p_control:?}, order_clean {p_clean:?}");
        assert!(p_control.is_empty(), "CONTROL: the swapped column accepts the dangling borrow: {p_control:?}");
        assert!(p_clean.iter().any(|l| l.starts_with("borrow Invalidated")), "CONTROL: the swapped column catches the clean body: {p_clean:?}");
    }

    /// A callee whose head was refused (its own error, E0106 for one, is the head's) is refused
    /// by name, the callee and why, never checked under a guess: `Form::RegionsUnread { item: f,
    /// why: HeadRefused }`. PLANTED (`Plant::UnknownUnified`, the guess the refusal replaces):
    /// the same call is related through one origin and the body is lowered, so the refusal is
    /// what keeps it unchecked (rule 23).
    #[test]
    fn a_refused_head_is_refused_by_name() {
        let w = world();
        let (record, decls) = heads(&w, None);
        let (bx, rx) = body(false);
        let got = findings(&w.fx, &bx, w.control, rx, &record, &decls, Plant::None);
        std::println!("a refused head: {got:?}");
        assert!(
            matches!(got, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::HeadRefused }, .. }) if item == w.f),
            "refused by the callee's name: {got:?}"
        );
    }

    /// How order_control's last statement discards `*result`.
    #[derive(Clone, Copy, Debug)]
    enum Discarded {
        /// `let _ = *result;`
        LetWild,
        /// `_ = *result;`
        AssignUnderscore,
        /// `*result;`
        Statement,
    }

    /// order_control in the ladder's shape: `let outer = 1u8; let mut result = &outer; { let
    /// inner = 2u8; result = f(&outer, &inner); }`, `result` holding `inner`'s loan past its
    /// scope, then `*result` evaluated and discarded as `how` says, the ONLY statement after the
    /// inner block. (`let result = { let inner = 2u8; f(..) };` is no fixture for it: the
    /// block's value is moved into `result` after `inner`'s storage ends, a use of its own.)
    /// The fixture has no deref, assignment or underscore builder: each takes the slot of a
    /// read of a local (as hub_call_tests.rs `deref`).
    fn mention_body(how: Discarded) -> (Bb, ExprId) {
        use blaze_typeck::{Expr, UnOp};
        let mut b = Bb::new();
        let (outer, inner, result) = (b.bind("outer"), b.bind("inner"), b.bind_mut("result"));
        let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s_outer = b.let_(outer, None, one);
        let lo = b.local(outer);
        let first = b.borrow(Mutbl::Not, lo);
        let s_result = b.let_(result, None, first);
        let two = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s_inner = b.let_(inner, None, two);
        let (l1, l2) = (b.local(outer), b.local(inner));
        let (r1, r2) = (b.borrow(Mutbl::Not, l1), b.borrow(Mutbl::Not, l2));
        let callee = b.path(&["f"]);
        let call = b.call(callee, &[r1, r2]);
        let dest = b.local(result);
        let set = b.local(result);
        b.exprs[set.ix()] = Expr::Assign { lhs: dest, rhs: call };
        let s_set = b.semi(set);
        let inner_block = b.block(&[s_inner, s_set], None);
        let s_block = b.semi(inner_block);
        let lr = b.local(result);
        let star = b.local(result);
        b.exprs[star.ix()] = Expr::Unary { op: UnOp::Deref, operand: lr };
        let last = match how {
            Discarded::LetWild => {
                let w = b.wild();
                b.let_(w, None, star)
            }
            Discarded::AssignUnderscore => {
                let under = b.local(result);
                b.exprs[under.ix()] = Expr::Underscore;
                let assign = b.local(result);
                b.exprs[assign.ix()] = Expr::Assign { lhs: under, rhs: star };
                b.semi(assign)
            }
            Discarded::Statement => b.semi(star),
        };
        let root = b.block(&[s_outer, s_result, s_block, last], None);
        (b, root)
    }

    /// THE MENTION (blaze-flow `StmtKind::Mention`): order_control's `result` used only by
    /// evaluating `*result` and discarding it is E0597 in each of the three forms, the place
    /// dereferencing `result` where `a` is dead (`let _ = *result;` and `_ = *result;` a
    /// mention, `*result;` a read). PLANTED (`Plant::MentionNothing`, the lowering before the
    /// statement: each lowered as nothing): all three are clean, the dangling borrow accepted,
    /// so the finding comes from the mention and the read and from nothing else (rule 23).
    #[test]
    fn a_mention_through_a_dangling_reference_is_e0597() {
        let w = world();
        let (a, b) = (Region::Param(0), Region::Param(1));
        let (record, decls) = heads(&w, Some([b, a, a]));
        for how in [Discarded::LetWild, Discarded::AssignUnderscore, Discarded::Statement] {
            let (bx, rx) = mention_body(how);
            let got = findings(&w.fx, &bx, w.control, rx, &record, &decls, Plant::None).expect("lowered");
            std::println!("{how:?}: {got:?}");
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{how:?} of a dangling reference is E0597: {got:?}");
            let planted = findings(&w.fx, &bx, w.control, rx, &record, &decls, Plant::MentionNothing).expect("lowered");
            std::println!("{how:?}, lowered as nothing: {planted:?}");
            assert!(planted.is_empty(), "CONTROL: {how:?} lowered as nothing accepts the dangling borrow: {planted:?}");
        }
    }

    /// The facts of a head store apart from the checked record (front2: `Store::tys`, where
    /// heads.rs lays every signature; the checked record's table is a fresh numbering,
    /// blaze-typeck writeback.rs): `regions` laid over `tys`, which `region_tys` names.
    struct Apart<'f> {
        record: &'f Record,
        regions: &'f crate::region_fixture::Regions,
        tys: Option<blaze_typeck::PTys<'f>>,
    }

    impl Facts for Apart<'_> {
        // Every other fact not known here, written out: `Facts` has no default answer.
        fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
            None
        }
        fn constant_initializer(&self) -> bool {
            false
        }
        fn fn_binders(&self) -> bool {
            true
        }
        fn closure_bodies_checked(&self) -> bool {
            false
        }
        fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
            None
        }
        fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
            None
        }
        fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
            None
        }
        fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
            None
        }
        fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
            self.record.adt(k)
        }
        fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
            self.record.callee(k)
        }
        fn regions(&self) -> &dyn crate::RegionFacts {
            self.regions
        }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
            self.tys
        }
        fn field(&self, a: ItemKey, v: u32, n: Name) -> Option<u32> {
            self.record.field(a, v, n)
        }
        fn field_count(&self, a: ItemKey, v: u32) -> Option<u32> {
            self.record.field_count(a, v)
        }
        fn param_copy(&self, i: u32) -> bool {
            self.record.param_copy(i)
        }
    }

    /// `b` lowered with `decls` laid into a table of their own (`apart`), numbered apart from
    /// the checked record's by nodes interned first; `named`: whether the facts name that table.
    fn lowered_apart(w: &World, b: &Bb, owner: ItemKey, root: ExprId, record: &Record, decls: &Decls, named: bool) -> Result<vec::Vec<std::string::String>, Refusal> {
        use blaze_typeck::{PNode, PortableTys};
        let view = b.view(owner, root);
        let c = crate::region_fixture::checked(&w.fx, &view);
        let mut apart = PortableTys::new_in(crate::region_fixture::alloc());
        for n in [PNode::Bool, PNode::Char, PNode::Str, PNode::Never, PNode::Bool] {
            apart.intern(n).expect("a node");
        }
        let regions = decls.lay(w.fx.t.view(), &mut apart);
        let facts = Apart { record, regions: &regions, tys: named.then(|| apart.view()) };
        let lowered = crate::oracle::lower_checked(&view, &c, &facts, Plant::None, crate::region_fixture::alloc())?;
        let rec = blaze_flow::record::check(&lowered.flow, crate::region_fixture::alloc()).expect("bounded");
        let mut lines = vec::Vec::new();
        for f in rec.borrows.iter() {
            lines.push(std::format!("borrow {f:?}"));
        }
        Ok(lines)
    }

    /// The columns are read in the table they were laid over (front2's case: the head store's
    /// `tys`, never the checked record's), [`Facts::region_tys`]: order_clean is clean and
    /// order_control E0597 with `f`'s column in a table of its own. CONTROL (the cause, rule
    /// 23: `region_tys` not named, so each column's `PTy` is read in the checked record's
    /// table, the bug front2 had: `SigUnaligned` on order_clean, `Malformed` on a local fn):
    /// refused by name, `SigColumnRoot` (the root read as another node) or `SigColumnType` (no
    /// such type there), never lowered under another type's slots. The checked table of these
    /// bodies holds no fn pointer, so the root cannot read as one by chance.
    #[test]
    fn columns_are_read_in_their_own_table() {
        let w = world();
        let (a, b) = (Region::Param(0), Region::Param(1));
        let (record, decls) = heads(&w, Some([b, a, a]));
        let (bc, rc) = body(true);
        let clean = lowered_apart(&w, &bc, w.clean, rc, &record, &decls, true).expect("order_clean is lowered");
        std::println!("apart: order_clean {clean:?}");
        assert!(clean.is_empty(), "order_clean is clean: {clean:?}");
        let (bx, rx) = body(false);
        let control = lowered_apart(&w, &bx, w.control, rx, &record, &decls, true).expect("order_control is lowered");
        std::println!("apart: order_control {control:?}");
        assert!(control.iter().any(|l| l.starts_with("borrow Invalidated")), "order_control is E0597: {control:?}");
        let unnamed = lowered_apart(&w, &bc, w.clean, rc, &record, &decls, false);
        std::println!("apart, the table not named: {unnamed:?}");
        assert!(
            matches!(unnamed, Err(Refusal { form: Form::SigColumnRoot | Form::SigColumnType, .. })),
            "CONTROL: a column read in another table is refused by name: {unnamed:?}"
        );
    }

    /// No facts given ([`crate::NoRegions`]): a call carrying a lifetime is refused by name with
    /// `NoHead`, not related by elision. Control: a body whose calls carry no lifetime is
    /// lowered (nothing to guess).
    #[test]
    fn no_record_is_refused_not_elided() {
        let w = world();
        struct Bare;
        impl Facts for Bare {
            // Every other fact not known here, written out: `Facts` has no default answer.
            fn regions(&self) -> &dyn crate::RegionFacts {
                &crate::NoRegions
            }
            fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
                None
            }
            fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
                None
            }
            fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
                None
            }
            fn constant_initializer(&self) -> bool {
                false
            }
            fn fn_binders(&self) -> bool {
                true
            }
            fn closure_bodies_checked(&self) -> bool {
                false
            }
            fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
                &[]
            }
            fn async_bodies_checked(&self) -> bool {
                false
            }
            fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
                None
            }
            fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
                None
            }
            fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
                None
            }
            fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
                None
            }
            fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
                None
            }
            fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
                None
            }
            fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
                None
            }
            fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
                None
            }
            fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
                None
            }
            fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
                None
            }
            fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
                None
            }
            fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
                None
            }
            fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
                Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
            }
            fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
                None
            }
            fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
                None
            }
            fn param_copy(&self, _: u32) -> bool {
                false
            }
        }
        let (bx, rx) = body(false);
        let got = findings_with(&w.fx, &bx, w.control, rx, &Bare, Plant::None);
        std::println!("no record: {got:?}");
        assert!(matches!(got, Err(Refusal { form: Form::RegionsUnread { why: UnreadWhy::NoHead, .. }, .. })), "{got:?}");
        let mut b = Bb::new();
        let x = b.bind("x");
        let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s = b.let_(x, None, one);
        let root = b.block(&[s], None);
        let plain = findings_with(&w.fx, &b, w.clean, root, &Bare, Plant::None);
        assert!(matches!(&plain, Ok(f) if f.is_empty()), "CONTROL: nothing carries a lifetime: {plain:?}");
    }
}
