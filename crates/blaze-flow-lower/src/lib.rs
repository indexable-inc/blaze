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
pub use generic_call::{CallBounds, Mark, Shape, ShapeBuf, read_shape};
pub use impl_call::ImplSource;
/// A caller's own type parameters under a callee's `T: 'a` (static-param-gap-1).
mod param_outlives;
pub use param_outlives::ParamOrigin;
mod super_let;
#[cfg(test)]
mod tests;

pub use sig::{
    AssocWhy, CallableRefusal, ImplText, Region, SigBuf, SigLife, SigRefusal, impl_header_end, read_callable_param, read_impl_signature,
    read_signature,
};
/// `Self::Name` read through the impl's own text (piece sig-assoc-projection).
#[cfg(test)]
mod sig_assoc_tests;

pub mod coroutine;
pub mod identity;
pub mod state_recipe;
#[cfg(test)]
mod coroutine_tests;

pub use coroutine::{InteriorPart, LoweredAsync, interior, interior_parts, lower_async};

pub mod closure;

pub use closure::{lower_async_fn, lower_closure};

/// What the lowering needs to know about an ADT (flow.md section 7, question (e)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdtFacts {
    /// A `Copy` impl exists; the type is `Copy` when its type arguments are (the derive's rule).
    pub copy: bool,
    /// The type implements `Drop` (its fields cannot be moved out).
    pub drop_impl: bool,
    /// Dropping it may run code (a `Drop` impl, or a field that does).
    pub needs_drop: bool,
    /// Its head declares lifetime parameters: one origin stands for all of them, invariant.
    pub lifetimes: bool,
    pub union: bool,
    pub is_enum: bool,
    pub is_box: bool,
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
    /// The type-outlives bounds callee `item` declares (`T: 'a`; generic_call.rs
    /// [`CallBounds`]). With [`Facts::sig`] and [`Facts::shape`], a call whose type arguments
    /// carry origins, or whose head writes lifetimes, is related by the instantiated
    /// signature. `None`: not known, and such a call keeps `unify_call`'s one origin, or is
    /// refused [`Form::CallWritesLifetimes`] when the head writes lifetimes.
    fn call_bounds(&self, _item: ItemKey) -> Option<CallBounds<'_>> {
        None
    }
    /// The index of field `name` of `variant`.
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32>;
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
    StructUpdate,
    DestructuringAssign,
    OrPatternBinding,
    SlicePatternBinding,
    /// A peeling pattern under an ADT's field binds: the peeled references are not recorded
    /// (typeck's `pat_adjustments`, omitted there).
    PeelUnderAdtField,
    /// The binding's recorded type is not the place's type or a reference to it.
    BindingMode,
    /// A guard over a by-value binding of a non-`Copy` type.
    GuardMovesBinding,
    /// The callee's signature writes lifetimes (needs the heads' lifetime part).
    CallWritesLifetimes,
    /// The callee's signature is not known (no head, or a lifetime part that does not line
    /// up) and an argument or the result carries a lifetime: one origin for all cannot say
    /// "this argument must be `'static`", so the body is not checked rather than accepted.
    CalleeUnknown,
    /// A call through a value (a fn pointer, a closure, a `dyn Fn`, an `F: Fn(..)`) whose own
    /// signature's lifetimes are not known, with an argument or result that carries one.
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
/// when they are the place's own type's positions, else the whole of an ADT's.
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

    /// The number of origin positions of `t`: one per reference, one per ADT with lifetimes, one
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
                    if self.adt_facts(k, e)?.lifetimes {
                        n += 1;
                    }
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
                        match ck.pat_ty.get(ck.captures.column::<1>()[i].ix()) {
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
        self.push_place(cp, ty);
        Ok(())
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
                    stack.extend_from_slice(self.list(l, e)?);
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
        let p = self.b.project_marked(parent, proj, drop_type);
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
                    _ => {}
                },
            }
        } else if proj == Proj::Deref {
            info.deref = 1;
        }
        self.ensure_pinfo(p, info);
        Ok(p)
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
                for k in 0..pi.len {
                    let (a, b) = (self.origs[(pi.start + k) as usize], self.origs[(ti.start + 1 + k) as usize]);
                    self.b.subset(a, b);
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

    fn drop_local(&mut self, l: Local, e: ExprId) -> Result<(), Refusal> {
        let t = self.local_ty[l.0 as usize];
        if self.needs_drop(t, e)? {
            let p = self.place_of(l);
            self.b.stmt(StmtKind::Drop(p), self.span(e));
        }
        self.b.stmt(StmtKind::StorageDead(l), self.span(e));
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
            if l.0 > self.nargs {
                self.drop_local(l, e)?;
            } else {
                let t = self.local_ty[l.0 as usize];
                if self.needs_drop(t, e)? {
                    let p = self.place_of(l);
                    self.b.stmt(StmtKind::Drop(p), self.span(e));
                }
            }
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
            Some(sig) => self.signature(sig, nparams, root)?,
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
                    let mut off = at + u32::from(self.adt_facts(k, e)?.lifetimes);
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
            if hi < lo || hi > sig.regions.len() || hi - lo != n {
                return self.refuse(Form::SigUnaligned, root);
            }
            let start = self.origs.len() as u32;
            for k in lo..hi {
                let o = self.universal(sig.regions[k]);
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
    fn call_by_signature(&mut self, life: SigLife<'_>, vs: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<bool, Refusal> {
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
            Task::CtorDone(e, n) | Task::AggDone(e, n) | Task::FmtDone(e, n) => self.aggregate(e, n, matches!(t, Task::FmtDone(..))),
            Task::OpDone(e) => self.aggregate(e, 1, false),
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
                if base.is_some() {
                    return self.refuse(Form::StructUpdate, e);
                }
                let fs = match body.field_inits.get(fields.range()) {
                    Some(a) => a,
                    None => return self.refuse(Form::Malformed, e),
                };
                self.tasks.push(Task::AggDone(e, fs.len() as u32));
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
        let (ob, or) = (self.origs[self.info(bp).start as usize], self.origs[self.info(rp).start as usize]);
        self.b.subset(ob, or);
        self.project(rp, self.local_ty[rt.0 as usize], Proj::Deref, out, false, e)
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
        let (ob, or) = (self.origs[self.info(btp).start as usize], self.origs[self.info(rp).start as usize]);
        self.b.subset(ob, or);
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
    fn aggregate(&mut self, e: ExprId, n: u32, fmt: bool) -> Result<(), Refusal> {
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
            && let Some((item, _)) = callee
            && let Some(life) = facts.sig(item)
            && self.call_by_signature(life, &vs, tp, ty, e)?
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
            Some(s) if s.writes_lifetimes => return self.refuse(Form::CallWritesLifetimes, e),
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
        if !self.call_by_signature(buf.life(), args, dest, dest_ty, e)? {
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
            if let Some(g) = arm.guard {
                if self.guard_moves(arm.pat)? {
                    return self.refuse_pat(Form::GuardMovesBinding, arm.pat);
                }
                let body_b = self.b.block();
                self.tasks.push(Task::FakeUse(Local(0), g));
                self.tasks.push(Task::Start(body_b));
                self.tasks.push(Task::Branch(g, body_b, next));
                self.tasks.push(Task::FakeBorrow(e, val));
            }
            self.tasks.push(Task::BindPat(arm.pat, val));
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
        let whole = p;
        let root = self.body.root;
        let place = self.as_place(v, root)?;
        let mut stack: Vec<(PatId, Place, TyD, u8), A> = Vec::new_in(self.alloc.clone());
        stack.push((p, place, v.ty, 0));
        let mut steps = 0usize;
        while let Some((p, mut place, mut ty, mut mode)) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, p);
            }
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
                Pat::Bind { ann, sub, .. } => {
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
                    let l = self.new_local_with(lt, false, None, root)?;
                    self.name_local(l, Identity::Binding(p));
                    self.pat_local[p.ix()] = l.0;
                    let lp = self.place_of(l);
                    self.b.stmt(StmtKind::StorageLive(l), self.pat_span(p));
                    self.vars.push(l);
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
                        stack.push((s, place, ty, mode));
                    }
                }
                Pat::Ref { inner, .. } => {
                    let it = match (ty.wrap, self.node(ty.ty, root)?) {
                        (Some(_), _) => TyD::of(ty.ty),
                        (None, PNode::Ref(_, i)) => TyD::of(i),
                        _ => return self.refuse_pat(Form::Malformed, p),
                    };
                    let q = self.project(place, ty, Proj::Deref, it, false, root)?;
                    stack.push((inner, q, it, 0));
                }
                Pat::Or { alts } => {
                    for &a in self.body.pat_lists.get(alts.range()).unwrap_or(&[]) {
                        if self.binds(a)? {
                            return self.refuse_pat(Form::OrPatternBinding, p);
                        }
                    }
                }
                Pat::Slice { .. } => {
                    if self.binds(p)? {
                        return self.refuse_pat(Form::SlicePatternBinding, p);
                    }
                }
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
                        stack.push((sp, q, TyD::of(ft), mode));
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
    /// type); a peeling subpattern that binds is refused (the peeled references are not recorded).
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
        stack: &mut Vec<(PatId, Place, TyD, u8), A>,
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
            let ft = match self.ck.pat_ty.get(sp.ix()) {
                Some(&t) => TyD::of(t),
                None => return self.refuse_pat(Form::Malformed, sp),
            };
            if peels {
                return self.refuse_pat(Form::PeelUnderAdtField, sp);
            }
            let q = self.project(base, ty, Proj::Field(fi), ft, f.drop_impl, root)?;
            stack.push((sp, q, ft, mode));
        }
        Ok(())
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
