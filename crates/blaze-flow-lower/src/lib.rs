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
/// Step 1 of an `.await` whose future is not its operand: `into_future` by its recorded head.
mod await_call;
/// A call of a head with a hidden input (`T::Out`), related per call (sig-assoc-per-call).
mod hidden_call;
#[cfg(test)]
mod hidden_call_tests;
/// A local's storage end invalidates its loans whatever its type (storage-end-loans).
#[cfg(test)]
mod storage_end_tests;
/// A hidden input read through the impl the checker selected at the call (impl-per-call).
mod impl_call;
#[cfg(test)]
mod impl_call_tests;
pub use generic_call::{AssocRead, CallBounds, Mark, Misfit, Shape, ShapeBuf, ShapeRefusal, Skel, read_shape};
pub use impl_call::ImplSource;
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
mod super_let;
#[cfg(test)]
mod tests;
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

pub use sig::{
    AliasScope, AssocWhy, BodyOut, BodyState, CallableRefusal, ImplText, ImplTrait, ImplTraitWhy, PathIs, PathWhy, Region, SigBuf, SigLife, SigRefusal, SigWorld,
    TextBound, impl_header_end, read_async_signature, read_async_signature_with, read_callable_param, read_callable_param_with, read_impl_signature,
    read_impl_signature_with, read_impl_trait, read_signature, read_signature_with,
};
/// `Self::Name` read through the impl's own text (piece sig-assoc-projection).
#[cfg(test)]
mod sig_assoc_tests;
/// A written path resolved before its lifetimes are read: aliases expanded (piece sig-paths).
#[cfg(test)]
mod sig_alias_tests;

pub mod coroutine;
pub mod identity;
pub mod state_recipe;
#[cfg(test)]
mod coroutine_tests;

pub use coroutine::{InteriorPart, LoweredAsync, interior, interior_parts, lower_async};

pub mod closure;

pub use closure::{ClosureFlow, External, Need, Requirement, lower_async_fn, lower_closure, lower_closure_external, requirement};

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
pub trait Facts {
    fn adt(&self, adt: ItemKey) -> Option<AdtFacts>;
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts>;
    /// The lifetime part of `item`'s signature (flow.md, "What the signature must say (H1)"):
    /// the body's own gives its placeholders and what is known among them; a callee's is
    /// instantiated at each call. `None`: not known, and the older stand-ins apply (elision from
    /// [`CalleeFacts`], a call that writes lifetimes refused).
    fn sig(&self, _item: ItemKey) -> Option<SigLife<'_>> {
        None
    }
    /// The texts of impl `imp` (its item text and each associated type member's `(name,
    /// text)`), for a call whose hidden projection the type check selected `imp` for
    /// (`Checked::selected`; impl_call.rs). Whoever answers files the read of those texts in
    /// the body's read list, keyed by the impl (rule 21). `None`: not given, and such a call is
    /// refused by name (`Form::CallHiddenInput`).
    fn impl_source(&self, _imp: ItemKey) -> Option<ImplSource<'_>> {
        None
    }
    /// The text of the body's own fn, from its attributes or `fn` keyword on (what
    /// [`read_signature`] is given for it). A call through one of the body's parameters (a fn
    /// pointer, a `dyn Fn`, an `F: Fn(..)`) reads that parameter's written type from it
    /// ([`read_callable_param`]: question T3's stand-in). `None`: not known, and such a call
    /// whose arguments or result carry a lifetime is refused ([`Form::CalleeValueUnknown`]).
    fn own_text(&self) -> Option<&str> {
        None
    }
    /// Whether the caller checks each closure's BODY on its own (its captures as inputs of
    /// their modes). The lowering makes a closure expression the borrows and moves of its
    /// captures and does not descend into its body; while nothing checks that body, a body
    /// holding a closure is refused (`Form::Closure`: not checked, never accepted), which is
    /// what `false` says.
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    /// The requirements closure `closure`'s own check put on the flow that makes it
    /// ([`Requirement`]: relations among the closure value's regions its body needs; closure.rs
    /// [`requirement`]): each is emitted as a constraint at the closure expression, so one the
    /// owner cannot prove is the owner's finding at the closure's range. The caller checks a
    /// closure's flow BEFORE the flow that holds its expression and answers with what it found;
    /// empty: nothing required (or the closure not checked yet: then its outlives results must
    /// still be refused, never dropped).
    fn closure_requirements(&self, _closure: ExprId) -> &[Requirement] {
        &[]
    }
    /// Whether the caller checks each async block's BODY on its own ([`lower_async`]). The
    /// lowering makes an async block expression the borrows and moves of its captures (as a
    /// closure's) and does not descend into its body; while nothing checks that body, a body
    /// holding an async block is refused (`Form::AsyncBlock`), which is what `false` says.
    fn async_bodies_checked(&self) -> bool {
        false
    }
    /// Whether `ty` (a type of the body's record, `Checked::types`) implements `Future`. An
    /// `.await`'s operand must: the future it holds is then the operand itself (the library's
    /// `impl<F: Future> IntoFuture for F`, core/src/future/into_future.rs). `None`: not known,
    /// and such an `.await` is refused (`Form::AwaitOperand`). A future of this body
    /// (`PNode::Future`) or a coroutine (`PNode::Coroutine`) is not asked.
    fn is_future(&self, _ty: PTy) -> Option<bool> {
        None
    }
    /// The generic shape of callee `item`'s head ([`read_shape`]): where its generic
    /// parameters stand among its positions. With [`Facts::sig`], a call whose type arguments
    /// carry origins is related by the instantiated signature (generic_call.rs); `None`: such
    /// a call keeps the one origin of `unify_call`.
    fn shape(&self, _item: ItemKey) -> Option<Shape<'_>> {
        None
    }
    /// Whether trait `trait_`, the principal of a trait object of the body's record, declares
    /// a lifetime parameter. `Some(false)` lets an unsized coercion to that object relate the
    /// source's positions into the object lifetime one way ([`Lower::unsize_to_object`]);
    /// `None` (not known) or `Some(true)` keeps the coarse relation, every position equal.
    /// Whoever answers files the read of the trait's text in the body's read list (rule 21).
    fn object_trait_lifetimes(&self, _trait: ItemKey) -> Option<bool> {
        None
    }
    /// The type-outlives bounds callee `item` declares (`T: 'a`; generic_call.rs
    /// [`CallBounds`]). With [`Facts::sig`] and [`Facts::shape`], a call whose type arguments
    /// carry origins, or whose head writes lifetimes, is related by the instantiated
    /// signature. `None`: not known, and such a call keeps `unify_call`'s one origin, or is
    /// refused [`Form::CallWritesLifetimes`] when the head writes lifetimes.
    fn call_bounds(&self, _item: ItemKey) -> Option<CallBounds<'_>> {
        None
    }
    /// The trait bounds with lifetime arguments of fn `item` ([`Ties`]): a callee's tie its
    /// trait lifetimes to what the call's arguments answer them with, the body's own say what
    /// its type parameters answer them with (trait_ties.rs). Whoever answers files the read of
    /// the fn's text and each trait's header in the body's read list (rule 21). `None`: not
    /// known, and a callee's trait lifetimes are fresh per call as before piece RB8.
    fn ties(&self, _item: ItemKey) -> Option<Ties<'_>> {
        None
    }
    /// The index of field `name` of `variant`.
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32>;
    /// The type parameter field `field` of `variant` is declared as, when its declared type is
    /// a bare one (`Some(T)`'s field is `T`): its index among the ADT's type arguments. `None`:
    /// not known or another type, and a constructor call filling it keeps `unify_call`'s one
    /// origin.
    fn field_param(&self, _adt: ItemKey, _variant: u32, _field: u32) -> Option<u32> {
        None
    }
    /// How field `field` of `variant`'s DECLARED type begins ([`FieldLead`]): what a pattern
    /// that peels references under that field (`Some(Cmd::Hello { who })` against
    /// `Option<&Cmd>`, a field declared `&'a E` matched by `E::V(x)`) peels. `None`: not known,
    /// and only a field [`Facts::field_param`] names is taken; a peeling pattern under any
    /// other field is refused (`Form::PeelUnderAdtField`). Whoever answers files the read of
    /// the ADT's field types in the body's read list (rule 21).
    fn field_lead(&self, _adt: ItemKey, _variant: u32, _field: u32) -> Option<FieldLead> {
        None
    }
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
    fn field_regions(&self, _adt: ItemKey, _variant: u32, _field: u32) -> Option<&[FieldRegion]> {
        None
    }
    /// For each type argument of `adt` (the record's argument list, as
    /// [`Facts::field_param`] counts it), whether its `Copy` impl needs that argument to be
    /// `Copy`: `[false]` for `impl<T: ?Sized> Copy for NonNull<T>`, every `true` for a derived
    /// `Copy` (the derive bounds every type parameter by `Copy`). `None`: not read, and every
    /// argument must be `Copy` (the derive's rule, [`AdtFacts::copy`]). Only an impl whose
    /// bounds are nothing but `Copy` on parameters (or none) may be answered: a bound the
    /// lowering does not check (`T: Tr`) gives `None`. Whoever answers files the read of the
    /// impl's header in the body's read list (rule 21).
    fn copy_args(&self, _adt: ItemKey) -> Option<&[bool]> {
        None
    }
    /// The number of fields of `variant`.
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32>;
    /// Whether the body's generic parameter `index` is bounded by `Copy`.
    fn param_copy(&self, index: u32) -> bool;
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
    /// An `async` closure, or an `.await` inside a closure, within an async block
    /// (coroutine.rs): its body is no part of the block's flow.
    AsyncClosure,
    /// An `.await` whose held future, `<T as IntoFuture>::IntoFuture`, has no row in the
    /// record (`Checked::awaited_of`: not wholly known), or whose operand is a reference the
    /// record does not name.
    AwaitOperand,
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
    /// The callee's signature is not known (no head, or a lifetime part that does not line
    /// up) and an argument or the result carries a lifetime: one origin for all cannot say
    /// "this argument must be `'static`", so the body is not checked rather than accepted.
    CalleeUnknown,
    /// A call through a value (a fn pointer, a closure, a `dyn Fn`, an `F: Fn(..)`) whose own
    /// signature's lifetimes are not known, with an argument or result that carries one. Known
    /// are: a body parameter's written callable type (`call_through_value`, sig/value.rs; it
    /// needs [`Facts::own_text`]), and a closure made in this body, whose result may hold
    /// anything the closure holds (callee_value.rs). Not known: a field holding a closure
    /// (`(self.f)(x)`), a callable type parameter bound in an impl's header or a `where`
    /// clause the reader does not read.
    CalleeValueUnknown,
    /// The callee's head names its generic parameter `param` (`PNode::Param`, parents first),
    /// but the call's argument list from the type checker has only `args`: a malformed record,
    /// never related another way (generic_call.rs `call_by_generic_signature`).
    CalleeParamPastArgs { callee: ItemKey, param: u32, args: u32 },
    /// A call of `callee` whose type argument for its parameter `param` is under a
    /// type-outlives bound this does not relate: the head declares `T: 'a` over an argument
    /// whose lifetimes the lowering does not hold as positions (a type parameter of the caller,
    /// an alias, an opaque type, a trait object, a closure), or the argument carries a lifetime
    /// under a bound the reader did not resolve (the container's header declares one). Not
    /// checked rather than accepted (generic_call.rs `call_by_generic_signature`).
    CallOutlivesUnread { callee: ItemKey, param: u32 },
    /// A call of `callee` whose signature's regions include a trait lifetime of a bound on its
    /// parameter `param` (a trait method's `Self: Visitor<'de>`, a `V: Visitor<'de>`), which
    /// is the region the call's argument for `param` answers that bound with, not one the call
    /// may choose; and `why` says that answer is not known here (trait_ties.rs). Not checked
    /// rather than accepted with a fresh region.
    CallTieUnplaced { callee: ItemKey, param: u32, why: TieMiss },
    /// A call of `callee` whose head holds a hidden position at its input `input` (a
    /// projection over its type parameter, `x: T::Out`; `u32::MAX`: the call as a whole) whose
    /// argument carries a lifetime there, where the impl is not known at the call (a type
    /// argument carries an origin or names the caller's own generics): the impl may demand
    /// `'static` or a type argument's lifetime, so the body is not checked rather than
    /// related coarsely (hidden_call.rs).
    CallHiddenInput { callee: ItemKey, input: u32 },
    /// The body's type parameter `param` is required to outlive a lifetime its read bounds do
    /// not prove, and some bound on it was not read (a trait bound, its container's header, a
    /// type in its signature whose implied bounds are not read): not checked rather than
    /// reported (param_outlives.rs). Made by the borrow check's caller from
    /// [`Lowered::params`], not by the lowering.
    ParamOutlivesUnread { param: u32 },
    /// The signature's lifetime part does not have one region per origin position of the
    /// body's parameters and return type.
    SigUnaligned,
    /// The body of an `async fn` whose written return type's regions were not read
    /// (`BodyOut::AsyncUnread`): its value cannot be checked against them, so it is not checked.
    AsyncOutputUnread,
    /// A capture of an `async fn`'s root block that is not one of the fn's parameters by value
    /// (coroutine.rs `async_fn_root`): its regions are not the signature's to lay.
    AsyncFnCapture,
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
}

/// Which fact a call of a head that writes lifetimes lacked ([`Form::CallWritesLifetimes`]):
/// relating it takes the head's lifetime part, its generic shape and its declared bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallLacks {
    /// No lifetime part for the callee ([`Facts::sig`] gave `None`).
    Sig,
    /// No generic shape for the callee ([`Facts::shape`] gave `None`).
    Shape,
    /// No declared type-outlives bounds for the callee ([`Facts::call_bounds`] gave `None`).
    Bounds,
    /// Every row was given, and they did not line up with the call: the [`Misfit`] says
    /// where (another count of inputs than the call has operands, a part's positions that do
    /// not match its place, a hidden region, a projection in an input).
    Unaligned(Misfit),
}

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// Which row a call of `item`, whose head writes lifetimes, lacked to be related by its
    /// signature: the first of [`Facts::sig`], [`Facts::shape`], [`Facts::call_bounds`] that
    /// is `None`, else [`CallLacks::Unaligned`] with why the last instantiation tried at this
    /// call did not line up (`misfit`, reset at each try).
    fn call_lacks(&self, item: ItemKey) -> CallLacks {
        if self.facts.sig(item).is_none() {
            CallLacks::Sig
        } else if self.facts.shape(item).is_none() {
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
    /// Calls of a known head whose lifetime part holds a hidden position (`Region::Hidden`: an
    /// opaque type, or a projection over the head's own type parameter, `<T as Tr>::X`, whose
    /// impl is chosen per call), so not instantiated: related by elision or one origin
    /// instead, a superset for the output (sig/impl_life.rs's doc says why, and what is left).
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

pub struct Lowered<A: Allocator + Clone> {
    pub flow: Flow<A>,
    pub stats: Stats,
    /// The placeholders standing for the body's type parameters (param_outlives.rs): an
    /// `UnprovenOutlives` from one is E0310 when `certain`, else `Form::ParamOutlivesUnread`.
    pub params: Vec<ParamOrigin, A>,
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
}

/// Lowers one checked body.
pub fn lower<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_planted(body, ck, facts, Plant::None, alloc)
}

#[doc(hidden)]
pub fn lower_planted<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    plant: Plant,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    if body.coroutine.is_some() {
        return Err(Refusal { form: Form::Coroutine, range: range_of(body, body.root) });
    }
    let mut l = Lower::new(body, ck, facts, alloc)?;
    l.plant = plant;
    l.run()?;
    let stats = l.stats;
    let flow = l.b.finish().map_err(|_| Refusal { form: Form::Builder, range: range_of(body, body.root) })?;
    Ok(Lowered { flow, stats, params: l.param_origs })
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
    OpenScope,
    CloseScope(ExprId),
    Start(Block),
    Goto(Block, ExprId),
    Unreachable(Block, ExprId),
    PushVal(Val),
    Discard,
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

struct Lower<'b, 'c, F: Facts, A: Allocator + Clone> {
    body: &'b BodyView<'b>,
    ck: &'c Checked<'c>,
    tys: PTys<'c>,
    facts: &'b F,
    b: FlowBuilder<A>,
    alloc: A,
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
    temps: Vec<Local, A>,
    temp_marks: Vec<u32, A>,
    targets: Vec<Target, A>,
    two_phase: Vec<Loan, A>,
    extended: Vec<bool, A>,
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
    ret_block: Block,
    stats: Stats,
    plant: Plant,
    /// The flow's arguments: the body's parameters, or an async block's captures.
    nargs: u32,
    /// The async block whose body is lowered (`lower_async`); `None` for a fn body.
    async_root: Option<ExprId>,
    /// Per capture by reference of the async block: the pattern, the place `*arg` an
    /// `Expr::Local` of it reads, and its type.
    upvar_places: Vec<(PatId, Place, TyD), A>,
    /// Per suspension, in statement order, its `Expr::Await`.
    points: Vec<ExprId, A>,
    /// Per local: what it stands for (identity.rs): the one table of identities, emitted in
    /// `LoweredAsync::identity`.
    identity: Vec<Identity, A>,
    /// Why the last `call_by_generic_signature` related nothing (generic_call.rs `Misfit`):
    /// what a `CallWritesLifetimes { lacks: Unaligned(..) }` refusal says.
    misfit: Misfit,
}

/// Every type walk visits each node at most once per position: bounded by the table's size.
const MOST_POSITIONS: u32 = 4096;

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    fn new(body: &'b BodyView<'b>, ck: &'c Checked<'c>, facts: &'b F, alloc: A) -> Result<Self, Refusal> {
        let n = body.params.len() as u32;
        let b = FlowBuilder::new_in(1 + n, n, alloc.clone());
        let mut pat_local = Vec::new_in(alloc.clone());
        pat_local.resize(body.pats.len(), u32::MAX);
        let mut extended = Vec::new_in(alloc.clone());
        extended.resize(body.exprs.len(), false);
        let mut is_target = Vec::new_in(alloc.clone());
        is_target.resize(body.exprs.len(), false);
        for e in body.exprs {
            if let Expr::Break { target, .. } = *e
                && let Some(t) = is_target.get_mut(target.ix())
            {
                *t = true;
            }
        }
        let mut l = Lower {
            body,
            ck,
            tys: ck.types.view(),
            facts,
            b,
            alloc: alloc.clone(),
            pat_local,
            local_ty: Vec::new_in(alloc.clone()),
            local_origs: Vec::new_in(alloc.clone()),
            origs: Vec::new_in(alloc.clone()),
            pinfo: Vec::new_in(alloc.clone()),
            vals: Vec::new_in(alloc.clone()),
            tasks: Vec::new_in(alloc.clone()),
            vars: Vec::new_in(alloc.clone()),
            scope_marks: Vec::new_in(alloc.clone()),
            temps: Vec::new_in(alloc.clone()),
            temp_marks: Vec::new_in(alloc.clone()),
            targets: Vec::new_in(alloc.clone()),
            two_phase: Vec::new_in(alloc.clone()),
            extended,
            super_ext: Vec::new_in(alloc.clone()),
            is_target,
            uni: Vec::new_in(alloc.clone()),
            param_origs: Vec::new_in(alloc.clone()),
            param_implied: Vec::new_in(alloc.clone()),
            param_unsure: Vec::new_in(alloc.clone()),
            param_all_unsure: true,
            ret_block: Block(0),
            stats: Stats::default(),
            plant: Plant::None,
            nargs: n,
            async_root: None,
            upvar_places: Vec::new_in(alloc.clone()),
            points: Vec::new_in(alloc.clone()),
            identity: Vec::new_in(alloc.clone()),
            misfit: Misfit::NotTried,
        };
        Ok(l)
    }

    // ---- refusals and small reads ----

    fn refuse<T>(&self, form: Form, e: ExprId) -> Result<T, Refusal> {
        Err(Refusal { form, range: range_of(self.body, e) })
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

    fn expr_def(&self, e: ExprId) -> Option<Def> {
        let col = self.ck.expr_defs.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some(self.ck.expr_defs.column::<1>()[i])
    }

    fn pat_def(&self, p: PatId) -> Option<Def> {
        let col = self.ck.pat_defs.column::<0>();
        let i = col.iter().position(|x| *x == p)?;
        Some(self.ck.pat_defs.column::<1>()[i])
    }

    fn callee_of(&self, e: ExprId) -> Option<(ItemKey, List)> {
        let col = self.ck.callees.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some((self.ck.callees.column::<1>()[i], self.ck.callees.column::<2>()[i]))
    }

    fn field_of(&self, e: ExprId) -> Option<u32> {
        let col = self.ck.fields.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some(self.ck.fields.column::<2>()[i])
    }

    fn adt_facts(&self, k: ItemKey, e: ExprId) -> Result<AdtFacts, Refusal> {
        match self.facts.adt(k) {
            Some(f) => Ok(f),
            None => self.refuse(Form::UnknownAdt, e),
        }
    }

    // ---- type facts, each a bounded work list ----

    /// The number of origin positions of `t`: one per reference, one per lifetime parameter of an ADT, one
    /// per opaque, alias or trait object; in pre-order, a node's own before its children's.
    fn npos(&self, t: PTy, e: ExprId) -> Result<u32, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let mut n = 0u32;
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Ref(_, i) => {
                    n += 1;
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) => stack.push(i),
                PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Adt(k, l) => {
                    n += self.adt_facts(k, e)?.lifetimes;
                    stack.extend_from_slice(self.list(l, e)?);
                }
                PNode::Opaque(_, _) | PNode::Alias { .. } | PNode::Dynamic(_) => n += 1,
                // A closure of this body is its captures (the Reference, "Closure types": "A
                // closure expression produces a closure value with a unique, anonymous type",
                // "roughly" a struct of the captured variables): one position per capture by
                // reference, then the captured type's own.
                PNode::Closure { owner, ordinal, .. } if owner == self.body.owner => {
                    let ck: &'c Checked<'c> = self.ck;
                    let Some(&ce) = ck.closures.column::<0>().get(ordinal as usize) else {
                        return self.refuse(Form::Malformed, e);
                    };
                    for (i, x) in ck.captures.column::<0>().iter().enumerate() {
                        if *x != ce {
                            continue;
                        }
                        if ck.captures.column::<2>()[i] != blaze_typeck::CaptureKind::ByValue {
                            n += 1;
                        }
                        // The captured VARIABLE's type (`local_ty`), as `closure_value` and the
                        // closure's own flow (closure.rs) lay it: a binding by reference
                        // (`Effect::Unlock { lock, .. } = &mut effect`, `for (name, _) in
                        // map.iter()`) is `&T` there, while `pat_ty` is the type its pattern
                        // met after the peels, `T`; one position short, and the closure value
                        // was refused `Malformed` (census 1118: twelve bodies of bstd).
                        match ck.local_ty.get(ck.captures.column::<1>()[i].ix()) {
                            Some(&t) => stack.push(t),
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
                    for (i, x) in ck.captures.column::<0>().iter().enumerate() {
                        if *x != fe {
                            continue;
                        }
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
        Ok(n)
    }

    /// Whether every origin position of `t` ([`Lower::npos`]'s count) is an opaque type's: no
    /// reference, no ADT lifetime parameter, no projection, trait object, closure or async block
    /// position among them (each of those may stand for an elided lifetime of a written
    /// output). A bounded work list, as `npos`.
    fn positions_all_opaque(&self, t: PTy, e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Opaque(_, _) => {}
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Adt(k, l) => {
                    if self.adt_facts(k, e)?.lifetimes > 0 {
                        return Ok(false);
                    }
                    stack.extend_from_slice(self.list(l, e)?);
                }
                PNode::Ref(..) | PNode::Alias { .. } | PNode::Dynamic(_) | PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Ok(false),
                _ => {}
            }
        }
        Ok(true)
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
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc.clone());
        // What each capture relates once the aggregate is made: its place, where its positions
        // start in the closure's, how many, and its type.
        let mut rel: Vec<(Place, u32, u32, TyD), A> = Vec::new_in(self.alloc.clone());
        let mut at = ci.start;
        for (i, x) in ck.captures.column::<0>().iter().enumerate() {
            if *x != e {
                continue;
            }
            let p = ck.captures.column::<1>()[i];
            // In an async block's body, a variable the block captures by reference is `*arg`.
            let (pl, lt) = match self.upvar_place(p) {
                Some(at) => at,
                None => {
                    let l = match self.pat_local.get(p.ix()) {
                        Some(&l) if l != u32::MAX => Local(l),
                        _ => return self.refuse(Form::UnknownLocal, e),
                    };
                    (self.place_of(l), self.local_ty[l.0 as usize])
                }
            };
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
        if at != ci.start + ci.len {
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
        if self.plant != Plant::NoRequirements {
            let facts: &'b F = self.facts;
            for r in facts.closure_requirements(e) {
                let from = self.need_origin(r.from, ci, e)?;
                let into = self.need_origin(r.into, ci, e)?;
                self.b.subset(from, into);
            }
        }
        self.push_place(cp, ty);
        Ok(())
    }

    /// The origin a requirement's end names in the closure value laid at `ci`.
    fn need_origin(&mut self, n: Need, ci: PInfo, e: ExprId) -> Result<Origin, Refusal> {
        match n {
            Need::Static => Ok(self.universal(Region::Static)),
            Need::Capture(k) if ci.exact && k < ci.len => Ok(self.origs[(ci.start + k) as usize]),
            Need::Capture(_) => self.refuse(Form::Malformed, e),
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
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t.ty);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
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
                PNode::Ref(Mutbl::Mut, _) => return Ok(false),
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
                PNode::Adt(k, l) => {
                    if !self.adt_facts(k, e)?.copy {
                        return Ok(false);
                    }
                    // The type is `Copy` when the arguments its `Copy` impl bounds are (the
                    // Reference, "Copy": an impl's bounds decide; `impl<T: ?Sized> Copy for
                    // NonNull<T>` bounds none, so `NonNull<Header>` is `Copy` and
                    // `(head.release)(job.0)` after `job.0.as_ref()` reads a copy, task/threads.rs
                    // `run_lent`). Unread: every argument, the derive's rule.
                    let args = self.list(l, e)?;
                    let facts: &'b F = self.facts;
                    match facts.copy_args(k) {
                        None => stack.extend_from_slice(args),
                        Some(need) if need.len() == args.len() => {
                            for (j, &a) in args.iter().enumerate() {
                                if need[j] {
                                    stack.push(a);
                                }
                            }
                        }
                        Some(_) => return self.refuse(Form::CopyArgsUnaligned { adt: k }, e),
                    }
                }
                PNode::Param(i) => {
                    if !self.facts.param_copy(i) {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether dropping a value of `t` may run code (more is safe: a `Drop` of a value that needs
    /// none is a deep write the borrow check may refuse, never a missed one).
    fn needs_drop(&self, t: TyD, e: ExprId) -> Result<bool, Refusal> {
        if t.wrap.is_some() || self.is_copy(t, e)? {
            return Ok(false);
        }
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t.ty);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
                PNode::Adt(k, l) => {
                    if self.adt_facts(k, e)?.needs_drop {
                        return Ok(true);
                    }
                    stack.extend_from_slice(self.list(l, e)?);
                }
                PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Dynamic(_) => return Ok(true),
                // A future holds its captures and its saved values: dropping it may run code.
                PNode::Future(..) | PNode::Coroutine { .. } => return Ok(true),
                _ => {}
            }
        }
        Ok(false)
    }

    fn is_unit_or_never(&self, t: PTy, e: ExprId) -> Result<bool, Refusal> {
        Ok(match self.node(t, e)? {
            PNode::Never => true,
            PNode::Tuple(l) => l.len == 0,
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

    /// A new local of type `t`, each of its positions a fresh origin (a placeholder for the
    /// signature's), or the origins given (`reuse`, for the return place under elision).
    fn new_local_with(&mut self, t: TyD, placeholder: bool, at: Option<Local>, e: ExprId) -> Result<Local, Refusal> {
        let l = match at {
            Some(l) => l,
            None => self.b.add_local(),
        };
        let n = self.npos_d(t, e)?;
        let start = self.origs.len() as u32;
        for _ in 0..n {
            let o = self.b.origin(placeholder);
            self.b.mentions(l.0, o);
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
    /// `Drop` type, an element of an unsized `[T]`).
    fn project_mark(&mut self, parent: Place, parent_ty: TyD, proj: Proj, child: TyD, mark: blaze_flow::PlaceMark, e: ExprId) -> Result<Place, Refusal> {
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
                    (Proj::Field(i), PNode::Tuple(l)) => {
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
        let mut at: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
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
    /// position when both are exact, the first covariantly when it is a reference, every other
    /// both ways (invariant: more relations refuse more, never less); else all made equal.
    fn relate(&mut self, src: Place, dst: Place, dst_ty: TyD, e: ExprId) -> Result<(), Refusal> {
        let (s, d) = (self.info(src), self.info(dst));
        if s.len == 0 || d.len == 0 || self.plant == Plant::NoRelate {
            return Ok(());
        }
        if s.exact && d.exact && s.len == d.len {
            let top_ref = dst_ty.wrap.is_some() || matches!(self.node(dst_ty.ty, e)?, PNode::Ref(..));
            for k in 0..s.len {
                let (a, b) = (self.origs[(s.start + k) as usize], self.origs[(d.start + k) as usize]);
                self.b.subset(a, b);
                if !(k == 0 && top_ref) {
                    self.b.subset(b, a);
                }
            }
            return Ok(());
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
                for k in 0..pi.len {
                    let (a, b) = (self.origs[(pi.start + k) as usize], self.origs[(ti.start + 1 + k) as usize]);
                    self.b.subset(a, b);
                    if object {
                        continue;
                    }
                    if kind != BorrowKind::Shared || k > 0 || !matches!(self.node(pty.ty, e)?, PNode::Ref(..)) {
                        self.b.subset(b, a);
                    }
                }
            } else {
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
    fn exit_drops(&mut self, scopes: u32, temps: u32, e: ExprId) -> Result<(), Refusal> {
        let from = self.temps.len();
        for i in (temps as usize..from).rev() {
            let l = self.temps[i];
            self.drop_local(l, e)?;
        }
        let vfrom = match self.scope_marks.get(scopes as usize) {
            Some(&m) => m as usize,
            None => self.vars.len(),
        };
        for i in (vfrom..self.vars.len()).rev() {
            let l = self.vars[i];
            // A parameter (or a closure's or async block's argument) too: its drop when it
            // needs one, and its storage's end whatever its type (storage-end-loans: before,
            // a parameter of a type with no drop glue was never ended, and a loan of it that
            // must outlive the function was accepted).
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
        let facts: &'b F = self.facts;
        match facts.sig(body.owner) {
            Some(sig) => {
                self.signature(sig, nparams, root)?;
                self.own_alias_known(nparams, root)?;
            }
            None => {
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
        let o = self.b.origin(true);
        if r == Region::Static {
            for i in 0..self.uni.len() {
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
        let mut stack: Vec<(PTy, u32), A> = Vec::new_in(self.alloc.clone());
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
                PNode::Tuple(l) => {
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

    /// The parameters and the return place from the signature's lifetime part: one placeholder
    /// per region, each position of each parameter and of the return type taking its region's;
    /// known among them exactly what the signature states: its declared bounds, `'static`, and
    /// the bounds implied by its types being well formed. Nothing else is known, so a `subset`
    /// the body derives between two placeholders beyond these is a finding (flow.md section 3).
    fn signature(&mut self, sig: SigLife<'_>, nparams: u32, root: ExprId) -> Result<(), Refusal> {
        if sig.ends.len() != nparams as usize {
            return self.refuse(Form::SigUnaligned, root);
        }
        // The signature is laid: its types' implied bounds on the type parameters are read
        // below (`sig_param_bounds` sets this again where it cannot read them).
        self.param_all_unsure = false;
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        let mut lo = 0usize;
        for i in 0..=nparams as usize {
            let (l, t, hi) = if i < nparams as usize {
                let p = self.body.params[i];
                let t = match self.ck.pat_ty.get(p.ix()) {
                    Some(&t) => TyD::of(t),
                    None => return self.refuse_pat(Form::Malformed, p),
                };
                (Local(1 + i as u32), t, sig.ends[i] as usize)
            } else {
                (Local(0), TyD::of(self.adjusted_ty(root)?), sig.regions.len())
            };
            let n = self.npos_d(t, root)? as usize;
            if i == nparams as usize && n == 0 {
                // The body's value has no origin position (it diverges, or the output has
                // none): nothing borrowed can reach the return place.
                self.set_local(l, t, self.origs.len() as u32, 0);
                break;
            }
            if hi < lo || hi > sig.regions.len() {
                return self.refuse(Form::SigUnaligned, root);
            }
            // An `async fn`'s body returns its future, the root async block's value (the
            // Reference, "Async functions"; `BodyOut`): an opaque type of ONE region, which
            // every position of the block's captures stands at (`impl Future + 'a` outlives
            // `'a` when each capture does). What the future yields is checked in the root
            // block's own flow, against the written return type (coroutine.rs
            // `async_fn_root`).
            let regions: &[Region] = &sig.regions[lo..hi];
            let future = i == nparams as usize && sig.body != BodyOut::Output;
            if (future && regions.len() != 1) || (!future && regions.len() != n) {
                return self.refuse(Form::SigUnaligned, root);
            }
            let start = self.origs.len() as u32;
            for k in 0..n {
                let region = if future { regions[0] } else { regions[k] };
                let o = self.universal(region);
                self.b.mentions(l.0, o);
                self.origs.push(o);
            }
            self.set_local(l, t, start, n as u32);
            if i < nparams as usize {
                self.vars.push(l);
            }
            self.sig_param_bounds(t, start, root)?;
            self.ref_ranges(t, root, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner) = ranges[j];
                let outer = self.origs[(start + at) as usize];
                for k in 1..=inner {
                    let o = self.origs[(start + at + k) as usize];
                    if o != outer && self.plant != Plant::NoKnown {
                        self.b.known(o, outer);
                    }
                }
            }
            lo = hi;
        }
        for &(a, b) in sig.bounds {
            let (oa, ob) = (self.universal(a), self.universal(b));
            if self.plant != Plant::NoKnown {
                self.b.known(oa, ob);
            }
        }
        self.stats.sig_read = true;
        Ok(())
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
        if life.ends.len() != vs.len() || life.regions.iter().any(|r| matches!(r, Region::Hidden(_))) {
            return Ok(false);
        }
        let mut lo = 0usize;
        for (i, v) in vs.iter().enumerate() {
            let hi = life.ends[i] as usize;
            if hi < lo || hi > life.regions.len() {
                return Ok(false);
            }
            if let At::Place(p) = v.at {
                let s = self.info(p);
                if !s.exact || s.len as usize != hi - lo {
                    return Ok(false);
                }
            }
            lo = hi;
        }
        let d = self.info(dest);
        if !d.exact || d.len as usize != life.regions.len() - lo {
            return Ok(false);
        }
        let out_at = lo;
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc.clone());
        if let Some((item, g)) = tie {
            let args: &'c [PTy] = self.list(g, e)?;
            let mut impl_ties = trait_ties::ImplTies::new_in(self.alloc.clone());
            self.tie_bounds(item, life, args, &mut inst, &mut impl_ties, e)?;
            // No type argument here holds a position (the caller's condition for this path),
            // so an impl's answer names none: a row that does is malformed.
            self.tie_impl_relations(&impl_ties, &mut inst, |_, _, _| None, e)?;
        }
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        lo = 0;
        for i in 0..=vs.len() {
            // The operands, then the destination.
            let (place, ty, into_callee) = if i < vs.len() {
                let v = vs[i];
                match v.at {
                    At::Place(p) => (p, v.ty, true),
                    At::Const => {
                        lo = life.ends[i] as usize;
                        continue;
                    }
                }
            } else {
                lo = out_at;
                (dest, dest_ty, false)
            };
            let s = self.info(place);
            let top_ref = ty.wrap.is_some() || matches!(self.node(ty.ty, e)?, PNode::Ref(..));
            for k in 0..s.len {
                let here = self.origs[(s.start + k) as usize];
                let there = self.instance(&mut inst, life.regions[lo + k as usize]);
                let (from, into) = if into_callee { (here, there) } else { (there, here) };
                self.b.subset(from, into);
                if !(k == 0 && top_ref) {
                    self.b.subset(into, from);
                }
            }
            self.ref_ranges(ty, e, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner) = ranges[j];
                let outer = self.instance(&mut inst, life.regions[lo + at as usize]);
                for k in 1..=inner {
                    let o = self.instance(&mut inst, life.regions[lo + (at + k) as usize]);
                    if o != outer {
                        self.b.subset(o, outer);
                    }
                }
            }
            if i < vs.len() {
                lo = life.ends[i] as usize;
            }
        }
        for &(a, b) in life.bounds {
            let (oa, ob) = (self.instance(&mut inst, a), self.instance(&mut inst, b));
            self.b.subset(oa, ob);
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
            Task::OpenScope => {
                self.scope_marks.push(self.vars.len() as u32);
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
            Task::Discard => {
                self.vals.pop();
                Ok(())
            }
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
                self.push_dest(dest, e)
            }
            Task::ArmEnd(e, dest, join, close) => {
                let v = self.pop(e)?;
                if let Some(d) = dest {
                    let dp = self.place_of(d);
                    let dt = self.local_ty[d.0 as usize];
                    self.assign_val(dp, dt, v, e)?;
                }
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
                self.push_dest(tg.dest, e)
            }
            Task::BreakDone(e, has) => self.break_done(e, has),
            Task::ReturnDone(e, has) => {
                let v = if has { self.pop(e)? } else { Val { at: At::Const, ty: self.local_ty[0] } };
                let rp = self.place_of(Local(0));
                let rt = self.local_ty[0];
                self.assign_val(rp, rt, v, e)?;
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
                self.assign_val(rp, rt, v, e)?;
                self.exit_drops(0, 0, e)?;
                self.b.goto(self.ret_block, self.span(e));
                self.b.start(self.ret_block);
                self.b.ret(self.span(e));
                Ok(())
            }
            Task::CallDone(e, n, value_callee, mark) => self.call_done(e, n, value_callee, mark),
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

    fn close_scope(&mut self, e: ExprId) -> Result<(), Refusal> {
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

    fn eval(&mut self, e: ExprId, m: bool) -> Result<(), Refusal> {
        self.tasks.push(Task::Adjust(e, m));
        let ty = TyD::of(self.expr_ty(e)?);
        let body = self.body;
        match self.expr(e)? {
            Expr::Missing => return self.refuse(Form::Missing, e),
            Expr::Lit(_) | Expr::OffsetOf { .. } | Expr::ConstBlock { .. } => self.vals.push(Val { at: At::Const, ty }),
            Expr::Local(p) if self.upvar_place(p).is_some() => {
                let Some((pl, lt)) = self.upvar_place(p) else {
                    return self.refuse(Form::UnknownLocal, e);
                };
                self.push_place(pl, lt);
            }
            Expr::Local(p) => match self.pat_local.get(p.ix()) {
                Some(&l) if l != u32::MAX => {
                    let pl = self.place_of(Local(l));
                    let lt = self.local_ty[l as usize];
                    self.push_place(pl, lt);
                }
                _ => return self.refuse(Form::UnknownLocal, e),
            },
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
                    let own_temps = body.edition >= Edition::E2024;
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
                self.tasks.push(Task::PushVal(value));
                self.tasks.push(Task::CloseScope(e));
                self.tasks.push(Task::Start(join));
                match else_ {
                    Some(x) => {
                        self.tasks.push(Task::ArmEnd(x, dest, join, false));
                        self.tasks.push(Task::Eval(x, false));
                    }
                    None => self.tasks.push(Task::Goto(join, e)),
                }
                self.tasks.push(Task::Start(eb));
                self.tasks.push(Task::ArmEnd(then, dest, join, false));
                self.tasks.push(Task::Eval(then, false));
                self.tasks.push(Task::Start(tb));
                self.tasks.push(Task::Branch(cond, tb, eb));
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
                for &a in args.iter().rev() {
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
                for &a in args.iter().rev() {
                    self.tasks.push(Task::Eval(a, false));
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
                    self.tasks.push(Task::PushVal(Val { at: At::Const, ty }));
                    self.tasks.push(Task::Discard);
                    self.tasks.push(Task::Eval(rhs, false));
                } else {
                    if !self.is_place_expr(lhs)? {
                        return self.refuse(Form::DestructuringAssign, e);
                    }
                    // The Reference, "Assignment expressions": "evaluates its right hand operand
                    // first, then the left hand place".
                    self.tasks.push(Task::AssignDone(e));
                    self.tasks.push(Task::Eval(lhs, true));
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
                for &a in es.iter().rev() {
                    self.tasks.push(Task::Eval(a, false));
                }
            }
            Expr::Repeat { elem, .. } => {
                self.tasks.push(Task::AggDone(e, 1));
                self.tasks.push(Task::Eval(elem, false));
            }
            Expr::Struct { fields, base, .. } => {
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
                for f in fs.iter().rev() {
                    self.tasks.push(Task::Eval(f.expr, false));
                }
            }
            Expr::Index { base, index } => {
                self.tasks.push(Task::IndexDone(e, m));
                self.tasks.push(Task::Eval(index, false));
                self.tasks.push(Task::Eval(base, m));
            }
            Expr::Closure { is_async, .. } => {
                if is_async || !self.facts.closure_bodies_checked() {
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
                AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. } if matches!(v.at, At::Const) => {
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
        self.target_flow(bi, 1, ri, m);
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
        if !s.exact || !d.exact || s.len == 0 || d.len != 2 {
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
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc.clone());
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
                // initializer lives to the end of the enclosing block.
                if let Expr::Ref { operand, .. } = self.expr(e)?
                    && self.extended.get(operand.ix()).copied().unwrap_or(false)
                {
                    let pi = self.info(p);
                    if pi.parent == u32::MAX
                        && let Some(at) = self.temps.iter().rposition(|l| l.0 == pi.local)
                    {
                        let l = self.temps.remove(at);
                        self.vars.push(l);
                    }
                }
            }
        }
        self.push_place(tp, ty);
        Ok(())
    }

    fn field_done(&mut self, e: ExprId) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        let ty = TyD::of(self.expr_ty(e)?);
        let p = self.as_place(v, e)?;
        if v.ty.wrap.is_some() {
            return self.refuse(Form::Malformed, e);
        }
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
                match self.field_of(e) {
                    Some(i) => (i, f.drop_impl),
                    None => return self.refuse(Form::UnknownField, e),
                }
            }
            _ => return self.refuse(Form::UnknownField, e),
        };
        let fp = self.project(p, v.ty, Proj::Field(idx), ty, drop_type, e)?;
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
        // whose lifetime is the receiver's (elision, `&self`).
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
        if let At::Place(ip) = idx.at {
            let ii = self.info(ip);
            self.target_flow(ii, 0, ri, m);
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
        if !cmp {
            let mut ps: Vec<Place, A> = Vec::new_in(self.alloc.clone());
            for v in [lhs, rhs] {
                if let At::Place(p) = v.at {
                    ps.push(p);
                }
            }
            ps.push(tp);
            self.unify(&ps);
        }
        self.push_place(tp, ty);
        Ok(())
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
        let mut vs: Vec<Val, A> = Vec::new_in(self.alloc.clone());
        vs.extend_from_slice(&self.vals[at..]);
        self.vals.truncate(at);
        if n == 0 && !fmt {
            self.vals.push(Val { at: At::Const, ty });
            return Ok(());
        }
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc.clone());
        let mut ps: Vec<Place, A> = Vec::new_in(self.alloc.clone());
        // The values as they go into the aggregate (for `format_args!`, the borrows).
        let mut parts: Vec<Val, A> = Vec::new_in(self.alloc.clone());
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
            (None, PNode::Tuple(l)) => Some(self.list(l, e)?),
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
            let mut slots: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
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
            _ => self.unify_call(&parts, tp, e)?,
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
        let mut out: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        let (adt, variant) = match self.expr(e)? {
            Expr::Call { callee, .. } => {
                let Some(Def::Ctor { adt, variant, .. }) = self.expr_def(callee) else { return Ok(None) };
                for j in 0..n {
                    out.push(j as u32);
                }
                (adt, variant)
            }
            Expr::Struct { fields, base, .. } => {
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
        let mut spots: Vec<(u32, u32, PTy), A> = Vec::new_in(self.alloc.clone());
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
            let top_ref = dst_ty.wrap.is_some() || matches!(self.node(dst_ty.ty, e)?, PNode::Ref(..));
            for k in 0..n {
                let (a, b) = (self.origs[(s.start + k) as usize], self.origs[(start + k) as usize]);
                self.b.subset(a, b);
                if !(k == 0 && top_ref) {
                    self.b.subset(b, a);
                }
            }
            return Ok(());
        }
        let hub = self.b.origin(false);
        for k in (s.start..s.start + s.len).chain(start..start + n) {
            let o = self.origs[k as usize];
            self.b.subset(o, hub);
            self.b.subset(hub, o);
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
        let mut vs: Vec<Val, A> = Vec::new_in(self.alloc.clone());
        vs.extend_from_slice(&self.vals[at + usize::from(by_type.is_some())..]);
        self.vals.truncate(at);
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc.clone());
        let mut ins: Vec<Place, A> = Vec::new_in(self.alloc.clone());
        for v in vs.iter().copied() {
            ops.push(self.operand(v, e)?);
            if let At::Place(p) = v.at {
                ins.push(p);
            }
        }
        let t = self.temp(ty, e)?;
        let tp = self.place_of(t);
        // RFC 2025: a two-phase borrow is activated at its first use, the call.
        let pending = self.two_phase.len();
        for i in mark as usize..pending {
            let l = self.two_phase[i];
            self.b.stmt(StmtKind::Activate(l), self.span(e));
        }
        self.two_phase.truncate((mark as usize).min(pending));
        self.b.call(&ops, tp, self.span(e));
        let out = self.info(tp);
        if value_callee {
            self.call_through_value(e, &vs, tp, ty)?;
            self.push_place(tp, ty);
            return Ok(());
        }
        let callee = match by_type {
            Some(c) => Some(c),
            None => self.callee_of(e),
        };
        let sig = callee.and_then(|(item, _)| self.facts.callee(item));
        let generic_origins = match callee {
            Some((_, g)) => {
                let mut any = false;
                for &a in self.list(g, e)? {
                    any |= self.npos(a, e)? > 0;
                }
                any
            }
            None => false,
        };
        let facts: &'b F = self.facts;
        // A declared `T: 'a` over an argument with no origin position that holds a type
        // parameter of the body (or a type not placed): `call_by_signature` reads no bounds, so
        // such a call goes by `call_by_generic_signature` or is refused (param_outlives.rs).
        // A head whose lifetime part was read but whose bounds were not filed: every argument
        // is under a bound not read.
        let owes = match callee {
            Some((item, g)) if facts.sig(item).is_some() => match facts.call_bounds(item) {
                Some(b) => self.owes_type_outlives(b, g, e)?,
                None => self.owes_type_outlives(CallBounds { outlives: &[], unread_below: u32::MAX, trait_bounded: &[] }, g, e)?,
            },
            _ => None,
        };
        // A head with a hidden position (`x: T::Out`): related per call, never coarsely where
        // an argument's demand could be missed (hidden_call.rs). A call that `owes` a declared
        // `T: 'a` goes on to `call_by_generic_signature`, which refuses a hidden head.
        if owes.is_none()
            && let Some((item, g)) = callee
            && let Some(life) = facts.sig(item)
            && life.holds_hidden()
            && self.call_through_hidden(item, life, sig, g, generic_origins, &vs, tp, ty, e)? == hidden_call::HiddenCall::BySignature
        {
            self.stats.calls_by_signature += 1;
            self.push_place(tp, ty);
            return Ok(());
        }
        if !generic_origins
            && owes.is_none()
            && let Some((item, g)) = callee
            && let Some(life) = facts.sig(item)
            && self.call_by_signature(life, Some((item, g)), &vs, tp, ty, e)?
        {
            self.stats.calls_by_signature += 1;
            self.push_place(tp, ty);
            return Ok(());
        }
        // A known head whose type arguments carry origins (derived `eq`:
        // `PartialEq::eq(&__self_0, &__arg1_0)` with `Self = &F`, piece BR4), or that writes
        // lifetimes `call_by_signature` above could not line up (`pick::<&u8>`, piece BR3): by
        // its signature instantiated with them and its declared bounds, never through one hub
        // (generic_call.rs); and a call that `owes` a bound over a caller's type parameter
        // (param_outlives.rs).
        if sig.is_some_and(|s| generic_origins || owes.is_some() || s.writes_lifetimes)
            && let Some((item, g)) = callee
            && let Some(life) = facts.sig(item)
            && let Some(shape) = facts.shape(item)
            && let Some(bounds) = facts.call_bounds(item)
            && self.call_by_generic_signature(item, life, shape, bounds, g, &vs, tp, ty, e)?
        {
            self.stats.calls_by_signature += 1;
            self.push_place(tp, ty);
            return Ok(());
        }
        if let Some(param) = owes
            && let Some((item, _)) = callee
        {
            return self.refuse(Form::CallOutlivesUnread { callee: item, param }, e);
        }
        if let Some((item, _)) = callee
            && facts.sig(item).is_some_and(|l| l.holds_hidden())
        {
            self.stats.calls_hidden += 1;
        }
        match sig {
            Some(s) if s.writes_lifetimes => {
                // `sig` is `callee`'s head, so `callee` is known here.
                let Some((item, _)) = callee else {
                    return self.refuse(Form::Malformed, e);
                };
                return self.refuse(Form::CallWritesLifetimes { callee: item, lacks: self.call_lacks(item) }, e);
            }
            Some(s) if !generic_origins => {
                if out.len > 0 {
                    let first_in = vs.first().and_then(|v| match v.at {
                        At::Place(p) => Some(p),
                        At::Const => None,
                    });
                    let mut from: Option<Origin> = None;
                    if s.self_ref
                        && !value_callee
                        && let Some(p) = first_in
                        && self.info(p).len > 0
                    {
                        from = Some(self.origs[self.info(p).start as usize]);
                    }
                    if from.is_none() {
                        let total: u32 = ins.iter().map(|&p| self.info(p).len).sum();
                        if total == 1 {
                            from = ins.iter().find(|&&p| self.info(p).len == 1).map(|&p| self.origs[self.info(p).start as usize]);
                        }
                    }
                    match from {
                        Some(o) => {
                            for k in out.start..out.start + out.len {
                                let x = self.origs[k as usize];
                                self.b.subset(o, x);
                            }
                            // `&'s Self` is well formed only when `Self: 's`, and the callee may
                            // return what `Self` holds as if it lived for `'s`: every position
                            // of the receiver flows into the output, not its first alone.
                            if s.self_ref
                                && !value_callee
                                && let Some(p) = first_in
                            {
                                let fi = self.info(p);
                                for j in fi.start + 1..fi.start + fi.len {
                                    let y = self.origs[j as usize];
                                    for k in out.start..out.start + out.len {
                                        let x = self.origs[k as usize];
                                        self.b.subset(y, x);
                                    }
                                }
                            }
                        }
                        // An output whose every position is an opaque type's (an `async fn`'s
                        // future, an `-> impl Trait`): no elided lifetime is written there, and
                        // the opaque captures the inputs' lifetimes (the Reference, "Async
                        // functions": `async fn f(x: A) -> T` is `fn f(x: A) -> impl Future<..>`
                        // whose future "holds its arguments"; "Impl trait", "Capturing": an
                        // `impl Trait` in return position captures all in-scope generic
                        // parameters, lifetimes among them, in edition 2024). So every input
                        // origin flows into each output position: exact for a capture of all,
                        // a superset (more demanded, never less) for one that captures fewer.
                        // With no input origin (`add_slowly(seed, 10)`), nothing is related: the
                        // future holds no lifetime of the call's.
                        None if ty.wrap.is_none() && self.positions_all_opaque(ty.ty, e)? => {
                            for &p in ins.iter() {
                                let fi = self.info(p);
                                for j in fi.start..fi.start + fi.len {
                                    let y = self.origs[j as usize];
                                    for k in out.start..out.start + out.len {
                                        let x = self.origs[k as usize];
                                        self.b.subset(y, x);
                                    }
                                }
                            }
                        }
                        None => return self.refuse(Form::ElisionAmbiguous, e),
                    }
                }
                self.stats.calls_elided += 1;
            }
            _ => {
                // No head for the callee: one origin for all cannot say "this argument must be
                // `'static`" (`spawn(&local)`), so where anything carries a lifetime the body
                // is refused by name, not accepted. A known head that writes no lifetime, called
                // with generic arguments that carry origins, keeps the one origin.
                if sig.is_none() && self.plant != Plant::UnknownUnified {
                    let mut any = out.len > 0;
                    for v in vs.iter() {
                        any |= self.npos_d(v.ty, e)? > 0;
                    }
                    if any {
                        return self.refuse(Form::CalleeUnknown, e);
                    }
                }
                self.unify_call(&vs, tp, e)?;
                self.stats.calls_unified += 1;
            }
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
    /// erased types do not hold them: they are read from the written type of the body's
    /// parameter the value is ([`read_callable_param`], question T3's stand-in). Any other
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
        let param = match self.expr(c)? {
            Expr::Local(p) => self.body.params.iter().position(|x| *x == p),
            _ => None,
        };
        let facts: &'b F = self.facts;
        let (Some(param), Some(text)) = (param, facts.own_text()) else {
            return self.refuse(Form::CalleeValueUnknown, e);
        };
        if dest_ty.wrap.is_some() || args.iter().any(|v| v.ty.wrap.is_some()) {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        for v in args {
            inputs.push(v.ty.ty);
        }
        let adt_lifetimes = |k: ItemKey| facts.adt(k).map(|f| f.lifetimes);
        let mut buf: SigBuf<A> = SigBuf::new_in(self.alloc.clone());
        if read_callable_param(text, self.tys, param as u32, &inputs, dest_ty.ty, &adt_lifetimes, &mut buf, self.alloc.clone()).is_err() {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        if !self.call_by_signature(buf.life(), None, args, dest, dest_ty, e)? {
            return self.refuse(Form::CalleeValueUnknown, e);
        }
        self.stats.calls_by_signature += 1;
        Ok(())
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
        let mut binds: Vec<Block, A> = Vec::new_in(self.alloc.clone());
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
        self.tasks.push(Task::Start(join));
        self.tasks.push(Task::Unreachable(unreachable, e));
        for (i, arm) in arms.iter().enumerate().rev() {
            let next = binds.get(i + 1).copied().unwrap_or(unreachable);
            self.tasks.push(Task::ArmEnd(arm.body, dest, join, true));
            self.tasks.push(Task::Eval(arm.body, false));
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
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc.clone());
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
                let lt = self.ck.local_ty.get(p.ix()).copied().unwrap_or(PTy(0));
                let by_ref = matches!(self.tys.node(lt), Ok(PNode::Ref(..)));
                if !by_ref && !self.is_copy(TyD::of(lt), self.body.root)? {
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
            Pat::Struct { fields, .. } => {
                for f in b.field_pats.get(fields.range()).unwrap_or(&[]) {
                    out.push(f.pat);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn binds(&self, p: PatId) -> Result<bool, Refusal> {
        let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc.clone());
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
        let mut alias: Vec<(Name, PatId), A> = Vec::new_in(self.alloc.clone());
        self.first_binds(p, &mut alias)?;
        let mut stack: Vec<BindItem, A> = Vec::new_in(self.alloc.clone());
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
            let peel = matches!(pat, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Slice { .. } | Pat::Path(_) | Pat::Range { .. });
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
                Pat::Bind { ann, sub, name } => {
                    let lt = match self.ck.local_ty.get(p.ix()) {
                        Some(&t) => TyD::of(t),
                        None => return self.refuse_pat(Form::Malformed, p),
                    };
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
                            if ty.wrap.is_some() || ty.ty != lt.ty {
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
                            match self.node(lt.ty, root)? {
                                PNode::Ref(rm, inner) if rm == m && ty.wrap.is_none() && inner == ty.ty => {}
                                _ => return self.refuse_pat(Form::BindingMode, p),
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
                    let mut subs: Vec<(u32, PatId), A> = Vec::new_in(self.alloc.clone());
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
                    let mut subs: Vec<(u32, PatId), A> = Vec::new_in(self.alloc.clone());
                    for f in self.body.field_pats.get(fields.range()).unwrap_or(&[]) {
                        match self.facts.field(adt, variant, f.name) {
                            Some(i) => subs.push((i, f.pat)),
                            None => return self.refuse_pat(Form::UnknownField, f.pat),
                        }
                    }
                    self.bind_fields(p, place, ty, mode, adt, variant, &subs, &mut stack)?;
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
            let peels = matches!(spat, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Slice { .. });
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
                self.tasks.push(Task::Discard);
                self.tasks.push(Task::Eval(expr, false));
                self.tasks.push(Task::OpenTemps);
            }
            TStmt::Let { pat, init: None, .. } => {
                // Declared, not initialized: each binding's storage is live, its value not.
                let from = self.vars.len();
                let mut stack: Vec<PatId, A> = Vec::new_in(self.alloc.clone());
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
                self.mark_extended(init)?;
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
    /// any extending block expression".
    fn mark_extended(&mut self, init: ExprId) -> Result<(), Refusal> {
        let mut stack: Vec<ExprId, A> = Vec::new_in(self.alloc.clone());
        stack.push(init);
        let mut steps = 0usize;
        while let Some(e) = stack.pop() {
            steps += 1;
            if steps > self.body.exprs.len() + 1 {
                return self.refuse(Form::Bound, e);
            }
            match self.expr(e)? {
                Expr::Ref { operand, .. } => {
                    if let Some(x) = self.extended.get_mut(operand.ix()) {
                        *x = true;
                    }
                    stack.push(operand);
                }
                Expr::Paren(i) | Expr::Cast { operand: i, .. } => stack.push(i),
                Expr::Tuple { elems } | Expr::Array { elems } => {
                    stack.extend_from_slice(self.body.expr_lists.get(elems.range()).unwrap_or(&[]));
                }
                Expr::Struct { fields, .. } => {
                    for f in self.body.field_inits.get(fields.range()).unwrap_or(&[]) {
                        stack.push(f.expr);
                    }
                }
                Expr::Block { tail: Some(t), .. } => {
                    self.note_extending_block(e);
                    stack.push(t);
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
