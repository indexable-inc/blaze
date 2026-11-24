//! The driver's machine (typeck.md, "The walk: a work stack in rustc's order").
//!
//! A stack of [`Frame`]s over the body's flat arrays. A rule never calls the checker of a child:
//! it pushes itself back with its next state, then the child, and reads the child's type from
//! `ret` when it resumes. No native recursion follows the expression tree, the type tree or
//! the pattern tree, so depth costs frames in a column sized from the body, not native stack.
//! Every column and queue is plain data, so a run can stop anywhere at `Need`.

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_own::{Alloc, Soa, Vec};
use core::mem::DropGuard;

use crate::body::BodyView;
use crate::columns::{AdjustKind, CaptureKind, UnionPat};
use crate::diag::{About, At, Because, DiagCode, PendingDiag, PendingGoal};
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{
    ExprId, FloatTy, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, RecordSchema, StmtId, UintTy,
};
use crate::portable::{PNode, PTy, PTys};
use crate::rules::async_fn::Witness;
use crate::ty::{Cause, Goal, Obligation, Ty, TyFlags, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::{Def, PClause, Reads, Shape, World};

/// rustc's default `recursion_limit`: the autoderef bound (E0055 past it).
pub(crate) const RECURSION_LIMIT: u32 = 128;

#[derive(Clone, Copy)]
enum AliasUse {
    /// Expose the value so a caller can inspect its type constructor.
    Shape(Cause),
    /// Let lazy relation constrain the term of a projection obligation.
    Equality,
}

/// A planted bug for the read-list property (`tests::read_list_property`): outside tests there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// A field access reads its ADT through the unrecorded backdoor.
    #[cfg(test)]
    PeekFields,
    /// Autoderef stops one step early (`suite::control_autoderef_short`).
    #[cfg(test)]
    AutoderefShort,
    /// The owner's where-clauses never reach the param env (`suite::control_where_clause_ignored`).
    #[cfg(test)]
    IgnoreWhereClauses,
    /// An async block's output is its first statement's type, not its tail's
    /// (`suite::async_block::control_output_from_first`).
    #[cfg(test)]
    AsyncOutputFromFirst,
    /// A defining body's output keeps its opaque as written: no hidden-type variable, no bounds
    /// (`suite::opaque::control_opaque_not_opened`).
    #[cfg(test)]
    OpaqueNotOpened,
    /// A method call registers none of its method's own where-clauses, so a `where Self: Sized`
    /// method is callable on a trait object (`suite::dyn_objects::control_sized_method_callable`).
    #[cfg(test)]
    MethodClausesDropped,
    /// An unsizing coercion takes any `Unsize`/`CoerceUnsized` target without asking the solver,
    /// so it can invent an auto trait (`suite::dyn_objects::control_coercion_invents_auto`).
    #[cfg(test)]
    UnsizeUnasked,
    /// `start_body` does not select after lowering the signature, so a body value binds the
    /// output's projection variable first and the projection fails with no place
    /// (`suite::deferred_place::signature_projection_is_settled_before_the_body`).
    #[cfg(test)]
    SignatureUnsettled,
    /// The lub of two fn items is not tried: two fn items of one signature at one coercion site
    /// are E0308 (`suite::fn_item_lub::control_without_the_lub_the_second_arm_is_e0308`).
    #[cfg(test)]
    FnLubOff,
    /// The lub's second direction is not tried: a fn item then a fn pointer at one coercion
    /// site is E0308 (`suite::fn_lub_earlier::control_without_the_second_direction_the_else_is_e0308`).
    #[cfg(test)]
    LubEarlierOff,
    /// A noop reborrow on an earlier source counts as an adjustment: three arms `a, b, d` with
    /// `a, b: &mut [u8; 4]`, `d: &mut [u8]` are E0308 at `d`
    /// (`suite::fn_lub_gates::control_without_the_reborrow_rule_the_last_arm_is_e0308`).
    #[cfg(test)]
    LubReborrowOff,
    /// Every unmet where-clause filed at the whole expression the item was used at, as before
    /// rules/obligation_site.rs (`suite::obligation_range::control_whole_expression_fails_all_four`).
    #[cfg(test)]
    ClauseAtWholeExpr,
    /// The obligations of the signature and the projection goals a relation defers take no
    /// place (`Cause::Body`), as before they were given one
    /// (`suite::deferred_place::a_deferred_projection_is_at_the_relating_expression`).
    #[cfg(test)]
    ObligationsWithoutPlace,
}

/// What the context expects of an expression (rustc's `Expectation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expectation {
    None,
    HasType(Ty),
    Castable(Ty),
    RvalueLikeUnsized(Ty),
}

impl Expectation {
    /// `ExpectHasType`'s type only (`only_has_type`).
    pub(crate) fn only_has_type(self) -> Option<Ty> {
        match self {
            Expectation::HasType(t) => Some(t),
            _ => None,
        }
    }

    /// Any expectation's type (`to_option`).
    pub(crate) fn to_option(self) -> Option<Ty> {
        match self {
            Expectation::None => None,
            Expectation::HasType(t) | Expectation::Castable(t) | Expectation::RvalueLikeUnsized(t) => {
                Some(t)
            }
        }
    }
}

/// What one source of a coercion site ([`Machine::coerce_many_push`]) is, with its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A value: the tail, an arm, a `break`'s or `return`'s operand.
    Expr(ExprId),
    /// The forced `()` of the expression named: a block with no tail that does not diverge, or
    /// a `break` or `return` with no value.
    Unit(ExprId),
}

/// Where a finished expression's result goes, after it is written to its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Then {
    /// Nowhere: the parent reads it from `ret`.
    Nothing,
    /// Coerced to `to` (`check_expr_coercible_to_type`); `ret` is `to`.
    Coerce { to: Ty, two_phase: bool },
    /// Must have type `to`, `!` allowed (`check_expr_has_type_or_error`); `ret` is `to`.
    Demand(Ty),
    /// Pushed into a coercion site (`CoerceMany::coerce`).
    CoerceMany(u32),
}

/// What a frame checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Expr(ExprId),
    Pat(PatId),
    Stmt(StmtId),
}

/// One frame of the walk: the node, the step of its rule, its expectation, where its result
/// goes, the divergence flag rustc saves around each expression, and a rule's own scratch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub node: Node,
    pub state: u8,
    pub saved: bool,
    pub expect: Expectation,
    pub then: Then,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub t: Ty,
    pub u: Ty,
    pub l: TyList,
}

/// What a rule's step did.
pub(crate) enum Flow {
    /// It pushed its continuation and a child.
    Wait,
    /// The node's type.
    Done(Ty),
}

/// A coercion site with several sources (`CoerceMany`): `if`/`else`, `match` arms, a block's
/// tail and its breaks, a loop's breaks, a body's returns.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CoerceMany {
    pub expected: Ty,
    pub final_ty: Option<Ty>,
    pub pushed: u32,
}

/// An enclosing loop or block that a `break` may leave (`BreakableCtxt`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BreakCtx {
    pub target: ExprId,
    pub coerce: Option<u32>,
    pub may_break: bool,
}

/// A cast to check after fallback (`deferred_cast_checks`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CastCheck {
    pub expr: ExprId,
    pub operand: ExprId,
    pub from: Ty,
    pub to: Ty,
}

/// A closure whose captures are analysed after selection (`closure_analyze`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingClosure {
    pub expr: ExprId,
    /// Where its signature's expectation came from (D148; rules/closure.rs `sig_from`).
    pub sig_from: crate::columns::SigFrom,
}

/// An async block's (or async closure's) future whose interior is fixed after the walk
/// (`Machine::analyze_futures`): its expression, the body walked for the interior, the interior
/// variable, and how it captures.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingFuture {
    pub expr: ExprId,
    pub body: ExprId,
    pub interior: Ty,
    pub capture: crate::body::Capture,
    /// Tuple of values known to be in the unpolled future. Before rules/async_fn.rs
    /// `analyze_coroutine_captures`: the captured places' environment, where rules/closure.rs
    /// `analyze_closures` decided them (capture_places.rs), else `None`.
    pub initial: Option<Ty>,
    pub witness: crate::CoroutineWitness,
}

/// What `Machine::snapshot` saved: the type context's snapshot and each column's length.
pub(crate) struct MachineSnapshot<S> {
    ty: S,
    diags: usize,
    why_steps: usize,
    dyn_seen: usize,
    errors: u32,
    adjust: usize,
    adj_scratch: usize,
    callees: usize,
    defs: usize,
    const_args: usize,
    fields: usize,
    captures: usize,
    capture_place_rows: usize,
    capture_proj_rows: usize,
    casts: usize,
    asms: usize,
    numeric: usize,
    var_origins: usize,
    diverging: usize,
    closures: usize,
    projected: usize,
    fru_fields: usize,
    default_fields: usize,
    union_fields: usize,
    union_pats: usize,
    inferred_err_coercions: usize,
    deferred_try_obs: usize,
}

/// Per-match narrowing state for a structural union scrutinee (`rules/control.rs`).
pub(crate) struct MatchNarrow<'a> {
    /// Original union member list of the scrutinee (after optional ref-peeling).
    pub orig_members: TyList,
    /// Uncovered indices into `orig_members`, in original order.
    pub remaining: Vec<u32, Alloc<'a>>,
    /// Default binding mode accumulated from peeling references off the scrutinee.
    pub peeled_mode: u32,
}

/// Where an overloaded dereference is asked ([`Machine::overloaded_deref_at`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DerefAt {
    /// An autoderef step (method probe, field access, coercion). It needs the target now, as
    /// rustc's `Autoderef` stops at a type it cannot structurally normalize.
    Autoderef,
    /// The operator `*`. Once `t: Deref` may hold, the target may be a variable that is
    /// decided later (rustc's `lookup_derefing`).
    Operator,
}

/// An autoderef step: the type, and whether it was reached by `Deref::deref`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub ty: Ty,
    pub overloaded: bool,
}

/// The steps [`Machine::lower_holes`] takes over one written type that holds a hole (rule 18):
/// two per written node, so a type of 32,768 written nodes.
const HOLE_STEPS: usize = 1 << 16;

/// What the nodes under the one being lowered were lowered to ([`Machine::lower_at`]).
pub(crate) struct Lowered<'a> {
    /// By node index: a node without a hole in its subtree, lowered once. A map of the nodes
    /// the walk reached, never a vector by index: a body's type sits above the whole records'
    /// table, so a vector would cost the table's length per lowering (runner 2, 2026-11-16).
    memo: blaze_hash::HashMap<u32, Ty, Alloc<'a>>,
    /// A node that holds a hole is lowered once per written occurrence
    /// ([`Machine::lower_holes`]): `(node, type)` for each occurrence its parent has not taken.
    pool: Vec<(u32, Ty), Alloc<'a>>,
}

/// How a portable type's generic parameters and holes are lowered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Lower {
    /// A world's answer: parameter `i` is `args[i]`; a hole is malformed.
    Args(TyList),
    /// A defining body's coroutine record. A bare future expression belongs to
    /// that body and cannot be interpreted as an expression of the reader.
    CoroutineArgs { args: TyList, owner: ItemKey, ordinal: u32 },
    /// A type written in the body: parameters are the owner's own; a hole is a fresh variable.
    Body,
    /// The owner's where-clause, assumed in its param env: parameters are the owner's own, an
    /// alias stays the alias (rustc keeps a param env's projections unnormalized: a clause
    /// `U: Inject<S, <U as UnionFrom<S>>::Path>` is assumed as written), a hole is malformed.
    Assumed,
}

/// The types every body uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Common {
    pub unit: Ty,
    pub bool_: Ty,
    pub char_: Ty,
    pub str_: Ty,
    pub static_str: Ty,
    pub never: Ty,
    pub err: Ty,
    pub usize_: Ty,
    pub u8_: Ty,
    pub i32_: Ty,
    pub f64_: Ty,
    pub empty: TyList,
}

/// The machine of one run.
pub(crate) struct Machine<'b, 'w, 'c, 'a, W: World, C: TypeCtx> {
    pub body: BodyView<'b>,
    pub reads: Reads<'w, 'a, W>,
    pub ty: &'c mut C,
    pub alloc: Alloc<'a>,
    pub plant: Plant,
    pub common: Common,
    /// The type a finished child left for its parent.
    pub ret: Ty,
    /// Whether the code checked so far always diverges (rustc's `Diverges::Always`).
    pub diverges: bool,
    pub frames: Vec<Frame, Alloc<'a>>,
    pub max_frames: usize,
    /// Foreign records whose output, witness, or signature is being instantiated.
    lowering_records: Vec<(crate::world::Question, PTy), Alloc<'a>>,
    // Results, by node.
    pub expr_ty: Vec<Ty, Alloc<'a>>,
    /// Whether the walk finished each expression: one no list or parent holds stays `false`,
    /// and the writeback refuses it by name ([`crate::error::Malformed::Unreached`]).
    pub reached: Vec<bool, Alloc<'a>>,
    pub pat_ty: Vec<Ty, Alloc<'a>>,
    pub local_ty: Vec<Ty, Alloc<'a>>,
    // Sparse results, rows read whole.
    /// Adjustments in the order made; scanned by expression (a receiver's autoref).
    pub adjust: Soa<(ExprId, AdjustKind, Ty), Alloc<'a>>,
    pub callees: Vec<(ExprId, ItemKey, TyList), Alloc<'a>>,
    /// What each value or struct path resolved to, at its expression or pattern.
    pub defs: Vec<(At, Def), Alloc<'a>>,
    /// The generic arguments of each constant a value path names whose arguments are not empty
    /// (`<T as Tr>::C`: `[Self, trait args..]`; an inherent impl's constant: the impl's), at its
    /// expression or pattern: written back as `Checked::const_args` and `Checked::pat_const_args`
    /// (rules/lit_path.rs `record_const_args`).
    pub const_args: Vec<(At, TyList), Alloc<'a>>,
    pub fields: Vec<(ExprId, u32, u32), Alloc<'a>>,
    pub captures: Vec<(ExprId, PatId, CaptureKind), Alloc<'a>>,
    /// The captured places of each closure the place analysis decided, in field order
    /// (capture_places.rs `CapturePlaces::file_places`): the closure, the root binding, the run
    /// of `capture_proj_rows`, how, and the stored type. Written back as
    /// `Checked::capture_places`.
    pub capture_place_rows: Vec<(ExprId, PatId, u32, u32, CaptureKind, Ty), Alloc<'a>>,
    /// Their projections: kind (0 deref, 1 field, 2 index), variant, field, type after.
    pub capture_proj_rows: Vec<(u8, u32, u32, Ty), Alloc<'a>>,
    pub diags: Vec<PendingDiag, Alloc<'a>>,
    /// The steps of every finding's why-chain (`PendingDiag::why` ranges into it), in the
    /// order asked; exported at writeback as the record's `why` column.
    pub why_steps: Vec<crate::diag::PendingWhyStep, Alloc<'a>>,
    /// Each one-segment value path that did not resolve, and its context's expectation: the
    /// holes `rules::suggest` ranks fixes for at writeback (empty in a body with none).
    pub named_holes: Vec<(ExprId, Expectation), Alloc<'a>>,
    /// The traits whose `dyn` was checked in this body ([`Machine::check_dyn`]: once per trait).
    pub dyn_seen: Vec<ItemKey, Alloc<'a>>,
    /// The number of errors reported: the body is "tainted by errors" when it is not zero.
    pub errors: u32,
    // Queues, processed where rustc processes them.
    pub casts: Vec<CastCheck, Alloc<'a>>,
    /// `asm!` operands to check after fallback (asm.rs).
    pub asms: Vec<crate::asm::AsmCheck, Alloc<'a>>,
    pub numeric: Vec<Ty, Alloc<'a>>,
    /// Where each named inference variable was made, in the order named ([`Machine::fresh_from`],
    /// and an ambiguous projection's answer in `projection_for_use`):
    /// what an E0282 names (rules/writeback.rs `undecided_of`). Rolled back with the variables.
    pub var_origins: Vec<(Ty, crate::diag::VarOrigin), Alloc<'a>>,
    pub diverging: Vec<Ty, Alloc<'a>>,
    pub closures: Vec<PendingClosure, Alloc<'a>>,
    /// The owner's generic parameter count (parents first), from its signature: a closure of
    /// this body is written back with the arguments `Param(0..own_params)`.
    pub own_params: u32,
    /// Parameter indices and declared const types for this body's nominal closures and futures.
    /// The world owns these rows. Writeback only needs their indices.
    pub own_consts: &'w [(u32, PTy)],
    /// What each capture's use needs of its closure (`analyze_closures`' scratch).
    pub capture_uses: Vec<(ExprId, crate::ty::ClosureKind), Alloc<'a>>,
    /// Each analysed closure's settled kind, for `Checked::closures`.
    pub closure_kinds: Vec<(ExprId, crate::ty::ClosureKind, Option<Ty>), Alloc<'a>>,
    pub futures: Vec<PendingFuture, Alloc<'a>>,
    /// Each `.await` in an async context and the future it holds while suspended
    /// (`<T as IntoFuture>::IntoFuture`): part of the enclosing future's interior.
    pub awaited: Vec<(ExprId, Ty), Alloc<'a>>,
    /// Each field a struct update takes from its base (rules/adt.rs `record_fru_fields`): the
    /// struct expression, the field's index in the struct, its type at the struct's arguments.
    /// Written back as `Checked::fru_fields`; rolled back with a probe.
    pub fru_fields: Vec<(ExprId, u32, Ty), Alloc<'a>>,
    /// Each field a base-less struct expression `S { a: 1, .. }` takes from its default
    /// (rules/adt.rs `record_default_fields`): the expression, the field's index in the
    /// variant, the default's key. Written back as `Checked::default_fields`; rolled back with
    /// a probe.
    pub default_fields: Vec<(ExprId, u32, ItemKey), Alloc<'a>>,
    /// Each overloaded index expression that is a mutable place and so calls
    /// `IndexMut::index_mut` (rules/mut_places.rs), in body order. Filled after the walk, so
    /// no probe sees it; written back as `Checked::index_mut`.
    pub index_mut: Vec<ExprId, Alloc<'a>>,
    /// The typed lints' findings (rules/args_inline.rs), filed after the end sequence: written
    /// back as `Checked::findings`.
    pub typed_findings: Vec<crate::rules::args_inline::TypedFinding, Alloc<'a>>,
    /// Each field expression whose field is a union's (rules/field.rs), in walk order: read by
    /// the unsafety pass (rules/unsafety.rs), which sorts it.
    pub union_fields: Vec<ExprId, Alloc<'a>>,
    /// Structural union pattern bindings/narrowings (rules/pat.rs, rules/control.rs); rolled back
    /// with a probe and written back sorted as `Checked::union_pats`.
    pub union_pats: Vec<(PatId, UnionPat), Alloc<'a>>,
    /// Per-match narrowing stack for structural union scrutinees (`rules/control.rs`).
    pub match_narrow: Vec<MatchNarrow<'a>, Alloc<'a>>,
    /// Registered structural record schemas -> ranges into `record_names`.
    pub record_schemas: Vec<(RecordSchema, List), Alloc<'a>>,
    /// Canonical sorted field names referenced by `record_schemas`.
    pub record_names: Vec<Name, Alloc<'a>>,
    /// What the unsafety pass could not decide (rules/unsafety.rs): written back as
    /// `Checked::undecided`, never a pass and never an error.
    pub undecided: Vec<crate::rules::unsafety::Undecided, Alloc<'a>>,
    pub coerce_many: Vec<CoerceMany, Alloc<'a>>,
    /// Each coercion site's value sources so far, `(site, expression)`, in push order; a `!`
    /// source is not listed. Read when the site's fn items meet at a fn pointer
    /// (rules/coerce_lub.rs): each earlier source then records `ReifyFnPointer` (rustc's
    /// `CoerceMany::pushed` expressions).
    pub coerce_sources: Vec<(u32, ExprId), Alloc<'a>>,
    pub breaks: Vec<BreakCtx, Alloc<'a>>,
    /// The innermost body's return site (the fn's, or a closure's).
    pub ret_sites: Vec<u32, Alloc<'a>>,
    /// One per open body, as `ret_sites`: whether it is async (where `.await` may be).
    pub async_sites: Vec<bool, Alloc<'a>>,
    /// Arguments whose formal input the call's expected type made unsized (`str`, a slice, a
    /// trait object): the type, an `RvalueLikeUnsized` hint for the argument only
    /// (rules/expect_inputs.rs), taken when the argument is checked.
    pub unsized_arg_hints: Vec<(ExprId, Ty), Alloc<'a>>,
    /// The adjustments of the coercion being tried, moved to `adjust` once it commits.
    pub adj_scratch: Vec<(AdjustKind, Ty), Alloc<'a>>,
    /// Each opaque this body defines and its hidden type's variable (rules/opaque_def.rs).
    pub defining: Vec<(ItemKey, Ty), Alloc<'a>>,
    /// While set, `lower` makes an opaque its hidden type's variable (rules/opaque_def.rs).
    pub opening: bool,
    /// While a trait fn's signature is instantiated at a CONCRETE `Self` (rules/rpitit_use.rs):
    /// each of its `impl Trait`s that is a synthetic associated type of the trait, as (the
    /// opaque, the trait, the type's name). `lower` makes such an opaque the projection
    /// `<Self as Tr>::{name}`, which the impl's row normalizes. Empty otherwise.
    pub rpitit: Vec<(ItemKey, ItemKey, Name), Alloc<'a>>,
    /// The promoted borrows (rules/promote.rs `analyze_promotions`), sorted by expression.
    pub promoted: Vec<ExprId, Alloc<'a>>,
    /// Each projection lowered while instantiating a callee's signature at a call: the call,
    /// the trait, the name, `[Self, trait args..]` (src/selected.rs; written back as
    /// `Checked::selected`). Rolled back with a probe.
    /// A trait predicate with erased lifetime arguments has no associated name.
    pub projected: Vec<(ExprId, ItemKey, Option<Name>, TyList), Alloc<'a>>,
    /// Set while the body's OWN signature is lowered (rules/control.rs): each projection in it
    /// is a `projected` row at the body's root, so the borrow check maps it through the
    /// selected impl's associated type (blaze-flow-lower `Lower::lay_through_impl`).
    pub sig_rows: bool,
    /// What an auto-trait goal of a future that failed only on the superset interior becomes:
    /// the gap, or under `check_deferring` a stalled row (src/stalled.rs).
    pub interior: crate::stalled::Interior,
    /// Futures needing suspension analysis and their original solver witness variables.
    /// The body check computes their candidate tuples separately for flow's input.
    pub pending_witnesses: Vec<(usize, Ty), Alloc<'a>>,
    /// The stalled auto-trait goals, in the order they failed (src/stalled.rs).
    pub stalled: Vec<crate::stalled::PendingStalled, Alloc<'a>>,
    /// The place of the node the walk is checking (set by `run` for each frame): the cause of
    /// the projection goals [`Machine::unify`] defers, so one that fails later is a finding at
    /// the expression, pattern or statement that related the types (rule 19).
    pub place: Cause,
    /// When the owner's return signature contains any `_` placeholder (`-> _`, `-> Option<_>`,
    /// `-> Result<T, _>`), the lowered output type whose finalized concrete type is exported as
    /// `Checked::inferred_output`.
    pub inferred_output_ty: Option<Ty>,
    /// When the owner's return signature is `Result<T, _>` (or `T ! _`), the inference variable
    /// representing the error slot while collecting error members from `Err(...)` and `?`.
    pub inferred_err_var: Option<Ty>,
    /// Coercions into `inferred_err_var` deferred until `finish_body` so multiple distinct error
    /// types unify into a single canonical error union instead of forcing the first error type.
    pub inferred_err_coercions: Vec<(Option<ExprId>, Ty), Alloc<'a>>,
    /// `FromResidual::from_residual` predicate registrations deferred until `inferred_err_var` is
    /// finalized in `finish_body`, preventing `impl<T> From<T> for T` from binding `inferred_err_var`
    /// prematurely to the first `?` error type.
    pub deferred_try_obs: Vec<(ItemKey, TyList, Cause), Alloc<'a>>,
}

impl<'b, 'w, 'c, 'a, W: World, C: TypeCtx> Machine<'b, 'w, 'c, 'a, W, C> {
    pub(crate) fn new_in(
        body: BodyView<'b>,
        reads: Reads<'w, 'a, W>,
        ty: &'c mut C,
        plant: Plant,
        alloc: Alloc<'a>,
    ) -> Result<Machine<'b, 'w, 'c, 'a, W, C>, CheckError> {
        let empty = ty.mk_list(&[])?;
        let unit = ty.mk(TyView::Tuple(empty))?;
        let str_ = ty.mk(TyView::Str)?;
        let common = Common {
            unit,
            bool_: ty.mk(TyView::Bool)?,
            char_: ty.mk(TyView::Char)?,
            str_,
            static_str: ty.mk(TyView::Ref(Mutbl::Not, str_))?,
            never: ty.mk(TyView::Never)?,
            err: ty.mk(TyView::Error)?,
            usize_: ty.mk(TyView::Uint(UintTy::Usize))?,
            u8_: ty.mk(TyView::Uint(UintTy::U8))?,
            i32_: ty.mk(TyView::Int(IntTy::I32))?,
            f64_: ty.mk(TyView::Float(FloatTy::F64))?,
            empty,
        };
        let err = common.err;
        let mut expr_ty = Vec::with_capacity_in(body.exprs.len(), alloc);
        expr_ty.resize(body.exprs.len(), err);
                let mut reached = Vec::with_capacity_in(body.exprs.len(), alloc);
                reached.resize(body.exprs.len(), false);
        let mut pat_ty = Vec::with_capacity_in(body.pats.len(), alloc);
        pat_ty.resize(body.pats.len(), err);
        let mut local_ty = Vec::with_capacity_in(body.pats.len(), alloc);
        local_ty.resize(body.pats.len(), err);
        Ok(Machine {
            body,
            reads,
            ty,
            plant,
            common,
            ret: err,
            diverges: false,
            frames: Vec::new_in(alloc),
            lowering_records: Vec::new_in(alloc),
            max_frames: 4 * body.nodes() + 64,
            expr_ty,
            reached,
            pat_ty,
            local_ty,
            adjust: Soa::new_in(alloc),
            callees: Vec::new_in(alloc),
            defs: Vec::new_in(alloc),
            const_args: Vec::new_in(alloc),
            fields: Vec::new_in(alloc),
            captures: Vec::new_in(alloc),
            capture_place_rows: Vec::new_in(alloc),
            capture_proj_rows: Vec::new_in(alloc),
            diags: Vec::new_in(alloc),
            why_steps: Vec::new_in(alloc),
            named_holes: Vec::new_in(alloc),
            dyn_seen: Vec::new_in(alloc),
            errors: 0,
            casts: Vec::new_in(alloc),
            asms: Vec::new_in(alloc),
            numeric: Vec::new_in(alloc),
            var_origins: Vec::new_in(alloc),
            diverging: Vec::new_in(alloc),
            closures: Vec::new_in(alloc),
            own_params: 0,
            own_consts: &[],
            capture_uses: Vec::new_in(alloc),
            closure_kinds: Vec::new_in(alloc),
            futures: Vec::new_in(alloc),
            awaited: Vec::new_in(alloc),
            fru_fields: Vec::new_in(alloc),
            default_fields: Vec::new_in(alloc),
            index_mut: Vec::new_in(alloc),
            typed_findings: Vec::new_in(alloc),
            union_fields: Vec::new_in(alloc),
            union_pats: Vec::new_in(alloc),
            match_narrow: Vec::new_in(alloc),
            record_schemas: {
                let mut rs = Vec::new_in(alloc);
                let mut rn_len = 0u32;
                for (schema, names) in body.types.record_schemas() {
                    if rs.iter().any(|&(s, _)| s == schema) {
                        continue;
                    }
                    let len = u32::try_from(names.len()).map_err(|_| CheckError::Limit(Limit::Table))?;
                    let list = if len == 0 {
                        List::EMPTY
                    } else {
                        let start = rn_len;
                        rn_len = rn_len.checked_add(len).ok_or(CheckError::Limit(Limit::Table))?;
                        List { start, len }
                    };
                    rs.push((schema, list));
                }
                rs
            },
            record_names: {
                let mut rn = Vec::new_in(alloc);
                let mut seen: Vec<RecordSchema, Alloc<'a>> = Vec::new_in(alloc);
                for (schema, names) in body.types.record_schemas() {
                    if seen.contains(&schema) {
                        continue;
                    }
                    seen.push(schema);
                    rn.extend_from_slice(names);
                }
                rn
            },
            undecided: Vec::new_in(alloc),
            coerce_many: Vec::new_in(alloc),
            coerce_sources: Vec::new_in(alloc),
            breaks: Vec::new_in(alloc),
            ret_sites: Vec::new_in(alloc),
            async_sites: Vec::new_in(alloc),
            unsized_arg_hints: Vec::new_in(alloc),
            adj_scratch: Vec::new_in(alloc),
            defining: Vec::new_in(alloc),
            opening: false,
            rpitit: Vec::new_in(alloc),
            promoted: Vec::new_in(alloc),
            projected: Vec::new_in(alloc),
            sig_rows: false,
            interior: crate::stalled::Interior::Gap,
            pending_witnesses: Vec::new_in(alloc),
            stalled: Vec::new_in(alloc),
            place: Cause::Body,
            inferred_output_ty: None,
            inferred_err_var: None,
            inferred_err_coercions: Vec::new_in(alloc),
            deferred_try_obs: Vec::new_in(alloc),
            alloc,
        })
    }

    // The stack.

    /// Runs frames until the stack is empty.
    pub(crate) fn run(&mut self) -> Result<(), Stop> {
        let bound = 16 * (self.body.nodes() as u64 + 1) + 1024;
        let mut steps = 0u64;
        while let Some(f) = self.frames.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            if self.reads.logging() {
                let bytes = self.state_bytes();
                self.reads.at(steps as u32, bytes);
            }
            // A statement's place is its pattern or expression (statements keep no range); an
            // item statement relates nothing, so the place before it stays.
            self.place = match f.node {
                Node::Expr(e) => Cause::Expr(e),
                Node::Pat(p) => Cause::Pat(p),
                Node::Stmt(s) => match self.body.stmt(s)? {
                    crate::body::Stmt::Let { pat, .. } => Cause::Pat(pat),
                    crate::body::Stmt::Expr { expr, .. } => Cause::Expr(expr),
                    crate::body::Stmt::Item => self.place,
                },
            };
            match f.node {
                Node::Expr(e) => self.step_expr(f, e)?,
                Node::Pat(p) => self.step_pat(f, p)?,
                Node::Stmt(s) => self.step_stmt(f, s)?,
            }
        }
        // Past the walk no node is being checked: a relation made now (fallback) names its
        // own place through `unify_at`, or has none.
        self.place = Cause::Body;
        if self.own_params != 0 && (!self.closures.is_empty() || !self.futures.is_empty()) {
            self.own_consts = self.reads.const_params(self.body.owner)?.params;
        }
        Ok(())
    }

    /// The checker's live state in bytes, for the ask log: what a saved state would hold (the
    /// frames and the columns written so far; the type context's tables are not counted).
    fn state_bytes(&self) -> u64 {
        let b = core::mem::size_of::<Frame>() * self.frames.len()
            + core::mem::size_of_val(&self.diags[..])
            + core::mem::size_of::<(ExprId, AdjustKind, Ty)>() * self.adjust.len()
            + core::mem::size_of_val(&self.callees[..])
            + core::mem::size_of_val(&self.defs[..])
            + core::mem::size_of_val(&self.fields[..])
            + core::mem::size_of_val(&self.captures[..])
            + core::mem::size_of_val(&self.capture_place_rows[..])
            + core::mem::size_of_val(&self.capture_proj_rows[..])
            + core::mem::size_of_val(&self.casts[..])
            + core::mem::size_of_val(&self.asms[..])
            + core::mem::size_of_val(&self.numeric[..])
            + core::mem::size_of_val(&self.closures[..]);
        b as u64
    }

    pub(crate) fn push(&mut self, f: Frame) -> Result<(), Stop> {
        if self.frames.len() >= self.max_frames {
            return Err(CheckError::Limit(Limit::Frames).into());
        }
        self.frames.push(f);
        Ok(())
    }

    fn frame(&self, node: Node, expect: Expectation, then: Then) -> Frame {
        let err = self.common.err;
        Frame {
            node,
            state: 0,
            saved: false,
            expect,
            then,
            a: 0,
            b: 0,
            c: 0,
            t: err,
            u: err,
            l: self.common.empty,
        }
    }

    /// Pushes the check of `e`.
    pub(crate) fn push_expr(&mut self, e: ExprId, expect: Expectation, then: Then) -> Result<(), Stop> {
        let f = self.frame(Node::Expr(e), expect, then);
        self.push(f)
    }

    /// Pushes the check of `e` coerced to `to` (`check_expr_coercible_to_type`).
    pub(crate) fn push_coercible(&mut self, e: ExprId, to: Ty) -> Result<(), Stop> {
        self.push_expr(e, Expectation::HasType(to), Then::Coerce { to, two_phase: false })
    }

    /// Pushes the check of `e` against `to`, `!` allowed (`check_expr_has_type_or_error`).
    pub(crate) fn push_has_type(&mut self, e: ExprId, to: Ty) -> Result<(), Stop> {
        self.push_expr(e, Expectation::HasType(to), Then::Demand(to))
    }

    /// Pushes the check of pattern `p` against `expected` with default binding mode `mode`
    /// (0 move, 1 `ref`, 2 `ref mut`).
    pub(crate) fn push_pat(&mut self, p: PatId, expected: Ty, mode: u32) -> Result<(), Stop> {
        let mut f = self.frame(Node::Pat(p), Expectation::HasType(expected), Then::Nothing);
        f.a = mode;
        self.push(f)
    }

    pub(crate) fn push_stmt(&mut self, s: StmtId) -> Result<(), Stop> {
        let f = self.frame(Node::Stmt(s), Expectation::None, Then::Nothing);
        self.push(f)
    }

    /// Pushes `f` back to resume at `state`.
    pub(crate) fn resume(&mut self, mut f: Frame, state: u8) -> Result<(), Stop> {
        f.state = state;
        self.push(f)
    }

    fn step_expr(&mut self, mut f: Frame, e: ExprId) -> Result<(), Stop> {
        if f.state == 0 {
            // `check_expr_with_expectation_and_args`: the divergence of what came before is
            // saved and the expression starts at "maybe".
            f.saved = self.diverges;
            self.diverges = false;
        }
        let expr = self.body.expr(e)?;
        if let Flow::Done(ty) = self.expr_rule(f, e, expr)? {
            self.finish_expr(f, e, ty)?;
        }
        Ok(())
    }

    /// Writes `e`'s type, folds divergence, and sends the result where `f.then` says.
    fn finish_expr(&mut self, f: Frame, e: ExprId, ty: Ty) -> Result<(), Stop> {
        self.expr_ty[e.ix()] = ty;
                self.reached[e.ix()] = true;
        if self.is_never(ty) {
            self.diverges = true;
        }
        self.diverges |= f.saved;
        self.ret = match f.then {
            Then::Nothing => ty,
            Then::Coerce { to, two_phase } => {
                // `demand_coerce_diag`: the target with the pending obligations selected first.
                let to = self.resolve_with_obligations(to)?;
                if !self.coerce(Some(e), ty, to, two_phase)? {
                    self.diag(DiagCode::Mismatch, At::Expr(e), About::types(to, ty));
                }
                to
            }
            Then::Demand(to) => {
                self.demand_has_type(e, ty, to)?;
                to
            }
            Then::CoerceMany(site) => {
                self.coerce_many_push(site, Source::Expr(e), ty)?;
                ty
            }
        };
        Ok(())
    }

    /// `check_expr_has_type_or_error`'s demand: `!` becomes any type by `NeverToAny`; otherwise
    /// the types must be equal.
    pub(crate) fn demand_has_type(&mut self, e: ExprId, ty: Ty, to: Ty) -> Result<(), Stop> {
        if self.is_never(ty) && !self.is_never(to) {
            self.adjust.push((e, AdjustKind::NeverToAny, to));
            if self.is_ty_var(to) {
                self.diverging.push(to);
            }
            return Ok(());
        }
        if !self.unify(to, ty)? {
            self.diag(DiagCode::Mismatch, At::Expr(e), About::types(to, ty));
        }
        Ok(())
    }

    // Coercion sites.

    pub(crate) fn new_coerce_many(&mut self, expected: Ty) -> Result<u32, Stop> {
        let at = u32::try_from(self.coerce_many.len())
            .map_err(|_| CheckError::Limit(Limit::Table))?;
        self.coerce_many.push(CoerceMany { expected, final_ty: None, pushed: 0 });
        Ok(at)
    }

    /// `CoerceMany::coerce` (and `coerce_forced_unit` for [`Source::Unit`]). A mismatch is
    /// reported at the source's own expression: the forced `()` names the block, `break` or
    /// `return` it stands for, so no finding of a site has an empty place.
    pub(crate) fn coerce_many_push(&mut self, site: u32, source: Source, ty: Ty) -> Result<(), Stop> {
        let (e, at) = match source {
            Source::Expr(e) => (Some(e), At::Expr(e)),
            Source::Unit(of) => (None, At::Expr(of)),
        };
        let cm = self.coerce_many[site as usize];
        let ty = self.try_structurally_resolve(ty)?;
        let merged = cm.final_ty.unwrap_or(cm.expected);
        if self.is_err(ty) || self.is_err(merged) {
            self.coerce_many[site as usize].final_ty = Some(self.common.err);
            return Ok(());
        }
        let ok = match e {
            None => {
                // A block with no tail, or a `break`/`return` with no value: `()` is unified,
                // not coerced.
                if self.unify(cm.expected, ty)? { Some(cm.expected) } else { None }
            }
            Some(e) if cm.pushed == 0 => {
                // The first source is coerced to the expected type.
                if self.coerce(Some(e), ty, cm.expected, false)? { Some(cm.expected) } else { None }
            }
            Some(e) => self.coercion_lub(site, e, merged, ty)?,
        };
        match ok {
            Some(t) => {
                let entry = &mut self.coerce_many[site as usize];
                entry.final_ty = Some(t);
                entry.pushed += 1;
                if let Some(e) = e
                    && !self.is_never(ty)
                {
                    self.coerce_sources.push((site, e));
                }
            }
            None => {
                let code = if cm.pushed == 0 { DiagCode::Mismatch } else { DiagCode::IncompatibleArms };
                self.diag(code, at, About::types(merged, ty));
                self.coerce_many[site as usize].final_ty = Some(self.common.err);
            }
        }
        Ok(())
    }

    /// `try_find_coercion_lub`: two fn items (or a fn item and a closure that captures nothing)
    /// meet at their fn pointer, every source reified (rules/coerce_lub.rs); else the new source
    /// coerced to the merged type; else every earlier source coerced to the new source's type
    /// when none carries an adjustment (rules/coerce_lub.rs `lub_earlier_to_new`); else the two
    /// unified as their least upper bound (equal, regions being erased).
    fn coercion_lub(&mut self, site: u32, e: ExprId, merged: Ty, ty: Ty) -> Result<Option<Ty>, Stop> {
        #[cfg(test)]
        let fn_lub_on = self.plant != Plant::FnLubOff;
        #[cfg(not(test))]
        let fn_lub_on = true;
        if fn_lub_on {
            match self.fn_lub(site, e, merged, ty)? {
                crate::rules::coerce_lub::FnLub::NotFns => {}
                crate::rules::coerce_lub::FnLub::Merged(t) => return Ok(Some(t)),
                crate::rules::coerce_lub::FnLub::Mismatch => return Ok(None),
            }
        }
        if self.coerce(Some(e), ty, merged, false)? {
            return Ok(Some(merged));
        }
        #[cfg(test)]
        let earlier_on = self.plant != Plant::LubEarlierOff;
        #[cfg(not(test))]
        let earlier_on = true;
        if earlier_on && let Some(t) = self.lub_earlier_to_new(site, merged, ty)? {
            return Ok(Some(t));
        }
        if self.unify(merged, ty)? {
            return Ok(Some(merged));
        }
        Ok(None)
    }

    /// `CoerceMany::complete`: the merged type, or `!` when nothing was pushed.
    pub(crate) fn coerce_many_complete(&mut self, site: u32) -> Ty {
        match self.coerce_many[site as usize].final_ty {
            Some(t) => t,
            None => self.common.never,
        }
    }

    // Types.

    pub(crate) fn mk(&mut self, v: TyView) -> Result<Ty, Stop> {
        Ok(self.ty.mk(v)?)
    }

    pub(crate) fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, Stop> {
        Ok(self.ty.mk_list(tys)?)
    }

    /// A fresh type variable.
    pub(crate) fn fresh(&mut self) -> Result<Ty, Stop> {
        Ok(self.ty.fresh(VarKind::Ty)?)
    }

    /// A fresh type variable made for `origin`, kept in [`Machine::var_origins`]: an E0282
    /// about it names where it was made.
    pub(crate) fn fresh_from(&mut self, origin: crate::diag::VarOrigin) -> Result<Ty, Stop> {
        let v = self.fresh()?;
        self.var_origins.push((v, origin));
        Ok(v)
    }

    /// A fresh integer variable, queued for fallback.
    pub(crate) fn fresh_int(&mut self) -> Result<Ty, Stop> {
        let v = self.ty.fresh(VarKind::Int)?;
        self.numeric.push(v);
        Ok(v)
    }

    /// A fresh float variable, queued for fallback.
    pub(crate) fn fresh_float(&mut self) -> Result<Ty, Stop> {
        let v = self.ty.fresh(VarKind::Float)?;
        self.numeric.push(v);
        Ok(v)
    }

    /// `n` fresh type variables.
    /// The `usize` value `n` (an array length) as a const term.
    pub(crate) fn len_value(&mut self, n: u64) -> Result<Ty, Stop> {
        let ty = self.common.usize_;
        self.mk(TyView::ConstValue { ty, bits: u128::from(n) })
    }

    /// A length written in the body (`[x; N]`): an `Infer` hole (`_`) is a fresh const
    /// variable, anything else the body's term lowered.
    pub(crate) fn lower_len(&mut self, len: PTy, at: At) -> Result<Ty, Stop> {
        if let PNode::Infer = self.body.types.node(len)? {
            return self.ty.fresh(VarKind::Const).map_err(Stop::from);
        }
        self.lower_body_ty(len, at)
    }

    /// The value of const term `t` once resolved, when it is a value that fits a `u64`.
    pub(crate) fn len_of(&mut self, t: Ty) -> Option<u64> {
        match self.view(t) {
            TyView::ConstValue { bits, .. } => u64::try_from(bits).ok(),
            _ => None,
        }
    }

    pub(crate) fn fresh_args(&mut self, n: u32) -> Result<TyList, Stop> {
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        for _ in 0..n {
            v.push(self.fresh()?);
        }
        self.mk_list(&v)
    }

    /// Fresh arguments for parameters `from..from + n` of `item`'s generics (the Reference,
    /// "Generic parameters"): a const parameter's is a const variable, a type parameter's a
    /// type variable.
    pub(crate) fn fresh_generic_args(&mut self, item: ItemKey, from: u32, n: u32) -> Result<TyList, Stop> {
        if n == 0 {
            return self.mk_list(&[]);
        }
        let consts = self.reads.const_params(item)?;
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        for i in from..from + n {
            v.push(match consts.type_of(i) {
                Some(_) => self.ty.fresh(VarKind::Const)?,
                None => self.fresh_from(crate::diag::VarOrigin::GenericArg { item, index: i })?,
            });
        }
        self.mk_list(&v)
    }

    /// The list's types, copied out.
    pub(crate) fn list(&self, l: TyList) -> Vec<Ty, Alloc<'a>> {
        let n = self.ty.list_len(l);
        let mut v = Vec::with_capacity_in(n as usize, self.alloc);
        for i in 0..n {
            v.push(self.ty.list_get(l, i));
        }
        v
    }

    /// One level of `t`, resolved at its top.
    pub(crate) fn view(&mut self, t: Ty) -> TyView {
        let t = self.ty.shallow_resolve(t);
        self.ty.view(t)
    }

    pub(crate) fn is_err(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Error)
    }

    pub(crate) fn is_never(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Never)
    }

    /// An unresolved general type variable (not an integer or float one).
    pub(crate) fn is_ty_var(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Infer(Var { kind: VarKind::Ty, .. }))
    }

    /// Unifies; what the relation defers is an obligation of the node being checked
    /// ([`Machine::place`]).
    pub(crate) fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, Stop> {
        let cause = self.place;
        self.unify_at(a, b, cause)
    }

    /// Unifies; what the relation defers is an obligation of `cause` ([`TypeCtx::unify_at`]).
    pub(crate) fn unify_at(&mut self, a: Ty, b: Ty, cause: Cause) -> Result<bool, Stop> {
        #[cfg(test)]
        let cause = if self.plant == Plant::ObligationsWithoutPlace { Cause::Body } else { cause };
        // A pending projection's term takes its constraint from this equality. Selecting it
        // first would lose the obligation's place if its output disagrees with that constraint.
        let a = self.normalize_aliases(a, AliasUse::Equality)?;
        let b = self.normalize_aliases(b, AliasUse::Equality)?;
        Ok(self.ty.unify_at(a, b, cause)?)
    }

    /// Unifies, reporting a mismatch at `at`, and what the relation defers there too.
    pub(crate) fn demand_eq(&mut self, at: At, expected: Ty, found: Ty) -> Result<bool, Stop> {
        let ok = self.unify_at(expected, found, at.cause())?;
        if !ok {
            self.diag(DiagCode::Mismatch, at, About::types(expected, found));
        }
        Ok(ok)
    }

    /// Records a diagnostic about `about` (src/diag/about.rs: no finding says nothing). The body
    /// is tainted by errors from now on.
    pub(crate) fn diag(&mut self, code: DiagCode, at: At, about: About) {
        self.errors += 1;
        let code = match (code, about) {
            (DiagCode::Mismatch, About::Types { expected: Some(e), found: f, .. })
                if self.pattern_over(e, f) || self.pattern_over(f, e) =>
            {
                DiagCode::PatternNotBase
            }
            _ => code,
        };
        self.diags.push(PendingDiag::new(code, at, about));
    }

    /// Whether `p` is a pattern type over `other` (typeck.md, "Pattern types"): its base is
    /// `other`, or `other` is an integer or float variable (an unsuffixed literal). Such a
    /// mismatch is `DiagCode::PatternNotBase`, which names both types.
    fn pattern_over(&mut self, p: Ty, other: Ty) -> bool {
        let TyView::Pat(base, _) = self.view(p) else { return false };
        let (base, other) = (self.ty.shallow_resolve(base), self.ty.shallow_resolve(other));
        base == other || matches!(self.ty.view(other), TyView::Infer(Var { kind: VarKind::Int | VarKind::Float, .. }))
    }

    // Selection.

    /// `select_obligations_where_possible`, reporting what failed (E0277).
    pub(crate) fn select(&mut self) -> Result<(), Stop> {
        self.ty.select_where_possible(&mut self.reads)?;
        self.report_fulfill_errors(false)
    }

    /// `select_all_obligations_or_error`: ambiguity is "type annotations needed", reported
    /// only when the body has no other error (rustc's `tainted_by_errors`).
    pub(crate) fn select_all(&mut self) -> Result<(), Stop> {
        // A direct forwarding await may have completed a witness during body analysis.
        self.bind_witnesses(true)?;
        self.ty.select_all(&mut self.reads)?;
        self.report_fulfill_errors(true)?;
        // Every obligation has now been decided, stalled, or reported. Flow consumes
        // the candidate tuple from the checked record, including for a stalled goal.
        self.bind_witnesses(false)
    }

    fn bind_witnesses(&mut self, exact: bool) -> Result<(), Stop> {
        for i in 0..self.pending_witnesses.len() {
            let (index, solver) = self.pending_witnesses[i];
            let f = self.futures[index];
            if (f.witness == crate::CoroutineWitness::Complete) == exact {
                self.demand_eq(At::Expr(f.expr), solver, f.interior)?;
            }
        }
        Ok(())
    }

    /// Ambiguity follows rustc's `maybe_report_ambiguity`: a trait goal other than `Sized` is
    /// E0283 "cannot satisfy", anything else (a `Sized` goal, a projection, well-formedness)
    /// E0282.
    fn report_fulfill_errors(&mut self, ambiguity: bool) -> Result<(), Stop> {
        let first = self.diags.len();
        self.report_each_fulfill_error(ambiguity)?;
        self.drop_projections_of_unmet_traits(first)
    }

    /// One finding per failed obligation at a place: an E0277 on `<X as Tr<..>>::A == T`
    /// whose trait goal `X: Tr<..>` is reported unmet at the same place is that goal's
    /// consequence, not a second error (rustc's `report_fulfillment_errors` suppresses an
    /// error another implies, a projection's under its trait's). Before (tempo
    /// crates/node/src/executed_state.rs:123, run a0700): one `.await` printed "is not a
    /// future" three times, for `X: IntoFuture`, `<X as IntoFuture>::IntoFuture` and
    /// `::Output`. Only findings of this round (`self.diags[first..]`) are compared; a dropped
    /// one carries no why-chain (an unmet projection asks none), so no row is left behind.
    fn drop_projections_of_unmet_traits(&mut self, first: usize) -> Result<(), Stop> {
        let mut i = first;
        while i < self.diags.len() {
            let d = self.diags[i];
            let mut implied = false;
            if let (DiagCode::Unsatisfied, Some((PendingGoal::Projection { trait_, args, .. }, _))) = (d.code, d.unmet) {
                for j in first..self.diags.len() {
                    let o = self.diags[j];
                    if let (DiagCode::Unsatisfied, Some((PendingGoal::Trait { trait_: t2, args: a2 }, _))) = (o.code, o.unmet)
                        && o.at == d.at
                        && t2 == trait_
                        && self.same_args(args, a2)?
                    {
                        implied = true;
                        break;
                    }
                }
            }
            if implied {
                self.diags.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// Whether lists `a` and `b` hold the same types, each resolved.
    fn same_args(&mut self, a: TyList, b: TyList) -> Result<bool, Stop> {
        if a == b {
            return Ok(true);
        }
        let n = self.ty.list_len(a);
        if n != self.ty.list_len(b) {
            return Ok(false);
        }
        for i in 0..n {
            let (x, y) = (self.ty.list_get(a, i), self.ty.list_get(b, i));
            if self.ty.resolve(x)? != self.ty.resolve(y)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn report_each_fulfill_error(&mut self, ambiguity: bool) -> Result<(), Stop> {
        // Bounded by the context's error count; each `next_error` removes one.
        while let Some(mut err) = self.ty.next_error() {
            // An unmet (or mismatched) clause goes to the text that introduced it; an ambiguous
            // one stays on the whole use: what it lacks is an annotation, not a bound.
            let at = match (err.kind, err.obligation.cause) {
                (crate::ty::FulfillErrorKind::Ambiguous, cause) => At::of(cause),
                (_, Cause::Clause { expr, nth }) => self.clause_at(expr, nth)?,
                (_, cause) => At::of(cause),
            };
            // Flow lowering can replace this body's provisional witness. Keep both sides
            // of the coercion so its pointer impls and object predicates are checked again.
            let ambiguous = err.kind == crate::ty::FulfillErrorKind::Ambiguous;
            let unsizing = if !ambiguous || ambiguity { self.unsize_interior_gap(err.obligation.goal)? } else { Witness::Fixed };
            // The solver's own cause first: the opaque its evaluation stalled on, at any depth
            // (through a local async block's interior, a closure's capture, an impl's
            // where-clause), so the check loop reveals it and asks again (W3).
            if ambiguity && ambiguous && self.errors == 0 && !matches!(unsizing, Witness::ForeignSuperset(..))
                && let Some(gap) = self.hidden_stall(err.obligation.goal)?
            {
                return Err(self.remaining_opaque_witness(gap, err.obligation, at)?.into());
            }
            if ambiguity && ambiguous && self.errors == 0 && !matches!(unsizing, Witness::ForeignSuperset(..))
                && let Some(gap) = self.opaque_ambiguity_gap(err.obligation.goal)?
            {
                // Let the producer supply its hidden type before deciding what
                // is still waiting on this body's suspension points.
                return Err(self.remaining_opaque_witness(gap, err.obligation, at)?.into());
            }
            if matches!(unsizing, Witness::LocalFlow | Witness::ForeignSuperset(..)) || (!ambiguous && unsizing == Witness::External) {
                if unsizing == Witness::LocalFlow && self.defer_local_unsizing(err, at) {
                    continue;
                }
                return Err(self.future_witness_gap(crate::INTERIOR_GAP, at).into());
            }
            // An auto trait of an async block's future that failed on the superset interior
            // and is not known to fail on the exact one: a typed gap, never E0277
            // (rules/async_fn.rs, 2).
            let mut witness = if err.kind == crate::ty::FulfillErrorKind::Unsatisfied || (ambiguity && ambiguous) {
                self.coroutine_interior_gap(err.obligation.goal)?
            } else {
                Witness::Fixed
            };
            // A goal that is no auto trait may still fail only by one nested under it: axum's
            // `Handler` impl for an async fn holds exactly when the fn's future is `Send`
            // (tempo crates/zones/rpc/src/server.rs:63, `post(handle_rpc)`). The failure is then
            // classified by that clause, as the auto-trait goal itself would be: a gap, or a
            // stalled row re-asking it on the exact interior, never E0277 on a superset.
            let mut nested: Option<(ItemKey, TyList)> = None;
            if witness == Witness::Fixed
                && (err.kind == crate::ty::FulfillErrorKind::Unsatisfied || (ambiguity && ambiguous))
                && err.because.is_none()
                && let crate::ty::Goal::Trait { trait_, args } = err.obligation.goal
                && let Some((w, leaf)) = self.interior_candidate(trait_, args)?
            {
                // Several suspect clauses (or one naming the probe's variables): no one clause
                // can be re-asked on the exact interior, so the gap.
                let Some(leaf) = leaf else {
                    return Err(self.witness_gap_of(w, at).into());
                };
                witness = w;
                nested = Some(leaf);
                // The goal now stands or falls by that one clause: an ambiguous goal (the
                // clause undecided on the superset interior: `async { .. }: Job`, which gave
                // E0283) is classified as an unmet one is, so an `External` witness is the gap
                // and a stalled row's finding, should the exact interior fail it, is E0277.
                err.kind = crate::ty::FulfillErrorKind::Unsatisfied;
            }
            if witness == Witness::FailedCapture {
                err.kind = crate::ty::FulfillErrorKind::Unsatisfied;
            }
            let unmet = err.kind == crate::ty::FulfillErrorKind::Unsatisfied;
            // Another fn's superset record (`Witness::ForeignSuperset`) leaves an ambiguous goal
            // open as it leaves an unmet one: a type its producer's body names but never stores
            // may be the one the solver could not decide.
            if matches!(witness, Witness::External | Witness::LocalFlow | Witness::ForeignSuperset(..))
                && (unmet || matches!(witness, Witness::LocalFlow | Witness::ForeignSuperset(..)))
            {
                // Under `check_deferring`: a stalled row holding the finding the goal becomes if
                // it fails on the exact interior (src/stalled.rs), made as the unmet-goal finding
                // below is made; otherwise the gap.
                match (self.interior, witness, err.obligation.goal) {
                    (crate::stalled::Interior::Defer, Witness::LocalFlow, crate::ty::Goal::Trait { trait_, args }) => {
                        let ty = self.ty.list_get(args, 0);
                        let leaf = match nested {
                            Some((lt, la)) => Some(PendingGoal::Trait { trait_: lt, args: la }),
                            None if unmet => self.leaf_above_interior(trait_, args)?.map(|(trait_, args)| PendingGoal::Trait { trait_, args }),
                            None => None,
                        };
                        let diag = match err.because {
                            Some(b) => PendingDiag::because(DiagCode::Unsatisfied, at, b, Some(ty), None),
                            None => PendingDiag::unmet(DiagCode::Unsatisfied, at, Some(ty), PendingGoal::Trait { trait_, args }, leaf),
                        };
                        // A stalled row re-asks an auto trait of a future (src/stalled.rs): the
                        // nested leaf's when the goal itself is none; its finding stays the goal's.
                        let (row_trait, row_ty) = match nested {
                            Some((lt, la)) => (lt, self.ty.list_get(la, 0)),
                            None => (trait_, ty),
                        };
                        self.stalled.push(crate::stalled::PendingStalled { trait_: row_trait, ty: row_ty, diag });
                        continue;
                    }
                    _ => return Err(self.witness_gap_of(witness, at).into()),
                }
            }
            match err.kind {
                crate::ty::FulfillErrorKind::Ambiguous => {
                    if ambiguity && self.errors == 0 {
                        let code = match err.obligation.goal {
                            crate::ty::Goal::Trait { trait_, .. } if self.lang(LangItem::Sized)? != Some(trait_) => {
                                DiagCode::CannotSatisfy
                            }
                            _ => DiagCode::AnnotationsNeeded,
                        };
                        // The finding names the goal left undecided, entire, as an unmet one
                        // does: "cannot satisfy `T: Tr`" says which goal had two answers.
                        match err.obligation.goal {
                            crate::ty::Goal::Trait { trait_, args } => {
                                self.push_goal_finding(code, at, PendingGoal::Trait { trait_, args }, false)?;
                            }
                            crate::ty::Goal::Projection { trait_, args, name, term } => {
                                self.push_goal_finding(code, at, PendingGoal::Projection { trait_, args, name, term }, false)?;
                            }
                            // `ty` well formed, undecided: the type is what the finding is about.
                            crate::ty::Goal::WellFormed(t) => self.diag(code, at, About::found(t)),
                            // A `where const` condition not decided at these types: the condition,
                            // with the bound's self type.
                            crate::ty::Goal::ConstHolds { cond, args } => {
                                let found = if self.ty.list_len(args) > 0 { Some(self.ty.list_get(args, 0)) } else { None };
                                self.errors += 1;
                                self.diags.push(PendingDiag::because(code, at, Because::Generic(cond), found, None));
                            }
                            crate::ty::Goal::ForFields { container, .. } => self.diag(code, at, About::found(container)),
                        }
                    }
                }
                crate::ty::FulfillErrorKind::Unsatisfied | crate::ty::FulfillErrorKind::Mismatch => match err.because {
                    // An impl's `where const` condition is the reason: the finding names it,
                    // with the bound's self type.
                    Some(because) => {
                        let found = match err.obligation.goal {
                            crate::ty::Goal::Trait { args, .. } if self.ty.list_len(args) > 0 => {
                                Some(self.ty.list_get(args, 0))
                            }
                            crate::ty::Goal::ForFields { container, .. } => Some(container),
                            _ => None,
                        };
                        self.errors += 1;
                        self.diags.push(PendingDiag::because(DiagCode::Unsatisfied, at, because, found, None));
                    }
                    // The finding carries the goal entire (trait or projection, every
                    // argument, the term) and the leaf where-clause that failed under it;
                    // `found` is the goal's self type.
                    None => match err.obligation.goal {
                        crate::ty::Goal::Trait { trait_, args } => {
                            self.push_goal_finding(DiagCode::Unsatisfied, at, PendingGoal::Trait { trait_, args }, true)?;
                        }
                        // A projection whose trait goal selects and decides the associated type
                        // to another type than its term is E0271, "type mismatch resolving
                        // `<T as Tr>::A == U`", naming the type found (rustc's
                        // `report_projection_error`); one whose trait goal fails, or whose
                        // type is not decided, is the unmet goal (E0277).
                        crate::ty::Goal::Projection { trait_, args, name, term } => {
                            let goal = PendingGoal::Projection { trait_, args, name, term };
                            match self.projection_found(trait_, args, name)? {
                                Some(found) if err.kind == crate::ty::FulfillErrorKind::Mismatch => {
                                    let mut diag = PendingDiag::unmet(DiagCode::ProjectionMismatch, at, Some(found), goal, None);
                                    diag.expected = Some(term);
                                    diag.name = Some(name);
                                    self.errors += 1;
                                    self.diags.push(diag);
                                }
                                _ => self.push_goal_finding(DiagCode::Unsatisfied, at, goal, true)?,
                            }
                        }
                        // `ty` is not well formed: the type is what the finding is about.
                        crate::ty::Goal::WellFormed(t) => self.diag(DiagCode::Unsatisfied, at, About::found(t)),
                        // A `where const` condition false at these types, with no reason given
                        // by the solver: the condition, with the bound's self type.
                        crate::ty::Goal::ConstHolds { cond, args } => {
                            let found = if self.ty.list_len(args) > 0 { Some(self.ty.list_get(args, 0)) } else { None };
                            self.errors += 1;
                            self.diags.push(PendingDiag::because(DiagCode::Unsatisfied, at, Because::False(cond), found, None));
                        }
                        crate::ty::Goal::ForFields { container, .. } => self.diag(DiagCode::Unsatisfied, at, About::found(container)),
                    },
                },
            }
        }
        Ok(())
    }

    /// The leaf goal under an unmet `args[0]: trait_<args[1..]>`: the goal the solver's chain
    /// of why it has no solution ends at ([`TypeCtx::why_leaf`], blaze-solve why_no.rs), the
    /// one that fails by itself (`None`: the goal itself fails so, or the leaf names a type the
    /// walk left open: the goal itself is what to fix). Asked in a probe: nothing is bound.
    pub(crate) fn failed_leaf(&mut self, trait_: ItemKey, args: TyList) -> Result<Option<(ItemKey, TyList)>, Stop> {
        self.ty.why_leaf(Goal::Trait { trait_, args }, &mut self.reads)
    }

    /// The leaf of a stalled row's finding (src/stalled.rs): the deepest goal of the solver's
    /// chain under `args[0]: trait_<..>` above its first step into a future's interior. The
    /// steps below that one walk the SUPERSET interior (rules/coroutine.rs `analyze_futures`):
    /// a slot there may hold a value the exact interior does not, so its goal is never named;
    /// the value is the exact interior's to say (front2 stalled_goals.rs `describe_storage`).
    /// `None`: the root itself, or no step above the interior's.
    fn leaf_above_interior(&mut self, trait_: ItemKey, args: TyList) -> Result<Option<(ItemKey, TyList)>, Stop> {
        let mut steps: Vec<crate::diag::PendingWhyStep, Alloc<'a>> = Vec::new_in(self.alloc);
        if self.ty.why_chain(Goal::Trait { trait_, args }, &mut steps, &mut self.reads)?.is_none() {
            return Ok(None);
        }
        let into = |s: &crate::diag::PendingWhyStep| matches!(s.reached, crate::diag::WhyReached::CoroutineSlot { .. } | crate::diag::WhyReached::Interior { .. });
        let above = steps.iter().position(into).unwrap_or(steps.len());
        Ok(if above >= 2 { steps.get(above - 1).and_then(|s| s.goal) } else { None })
    }

    /// [`Machine::failed_leaf`] with the whole chain: its steps appended to `why_steps` and the
    /// finding's [`crate::diag::PendingWhy`] (`None`, and no step, when the goal holds under
    /// the walk).
    fn why_chain(&mut self, trait_: ItemKey, args: TyList) -> Result<Option<crate::diag::PendingWhy>, Stop> {
        let start = self.why_steps.len();
        let Some(leaf) = self.ty.why_chain(Goal::Trait { trait_, args }, &mut self.why_steps, &mut self.reads)? else {
            self.why_steps.truncate(start);
            return Ok(None);
        };
        let len = self.why_steps.len() - start;
        let (Ok(start), Ok(len)) = (u32::try_from(start), u32::try_from(len)) else {
            return Err(CheckError::Limit(crate::error::Limit::Steps).into());
        };
        Ok(Some(crate::diag::PendingWhy { start, len, leaf }))
    }

    /// An undecided trait goal's typed cause when the solver stalled on an opaque type whose
    /// hidden type this body is not given (`TypeCtx::hidden_unknown`: one ordinary, cached
    /// evaluation; never the diagnostic walk): the gap `OpaqueHiddenUnknown` the check loop
    /// answers by revealing it (front2 opaque_auto.rs `after_auto_gap`). This body's own
    /// opaques are not asked for. `trait_` is the goal's: the auto trait itself, or the trait
    /// whose impl's where-clause reached it (axum's `Handler`, `Fut: Send`).
    fn hidden_stall(&mut self, goal: Goal) -> Result<Option<CheckError>, Stop> {
        let Goal::Trait { trait_, .. } = goal else { return Ok(None) };
        let defining = &self.defining;
        let skip = |k: ItemKey| defining.iter().any(|&(d, _)| d == k);
        let found = self.ty.hidden_unknown(goal, &skip, &mut self.reads)?;
        Ok(found.map(|opaque| CheckError::OpaqueHiddenUnknown { opaque, trait_ }))
    }

    /// [`Machine::why_chain`] for an undecided goal (`TypeCtx::why_maybe_chain`): the chain to
    /// the first goal under `goal` undecided by itself, its steps appended to `why_steps`;
    /// `None`, and no step, when the goal is decided under the walk.
    fn why_maybe_chain(&mut self, goal: Goal) -> Result<Option<crate::diag::PendingWhy>, Stop> {
        let start = self.why_steps.len();
        let Some(leaf) = self.ty.why_maybe_chain(goal, &mut self.why_steps, &mut self.reads)? else {
            self.why_steps.truncate(start);
            return Ok(None);
        };
        let len = self.why_steps.len() - start;
        let (Ok(start), Ok(len)) = (u32::try_from(start), u32::try_from(len)) else {
            return Err(CheckError::Limit(crate::error::Limit::Steps).into());
        };
        Ok(Some(crate::diag::PendingWhy { start, len, leaf }))
    }

    /// The list `l` of `types` under the impl arguments `iargs`, each type resolved.
    fn lower_resolved(&mut self, types: PTys<'_>, l: crate::ids::List, iargs: TyList) -> Result<TyList, Stop> {
        let parts = types.list(l)?;
        let mut tys: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(parts.len(), self.alloc);
        for &p in parts {
            let t = self.lower(types, p, Lower::Args(iargs), Cause::Body)?;
            tys.push(self.ty.resolve(t)?);
        }
        Ok(self.ty.mk_list(&tys)?)
    }

    /// Impl `imp` instantiated with fresh variables against `args[0]: trait_<args[1..]>`, in
    /// the caller's probe: `None` when its header does not match; else its arguments, and
    /// whether its projection clauses were bound (`false`: one normalized to a type its term
    /// cannot be, so the impl fails on that clause and no trait clause is its leaf).
    ///
    /// The projection clauses are bound first: a parameter only a where-clause's projection
    /// binds (axum's `impl<F, Fut, ..> Handler<(M, T1, ..), S> for F where F: FnOnce(T1, ..)
    /// -> Fut, Fut: Future<Output = Res> + Send`) is the type the projection normalizes to, as
    /// selection's nested goals bind it. Left a variable, every trait clause on it "may hold",
    /// and a failing `Fut: Send` (tempo crates/zones/rpc/src/server.rs:63) was never seen. A
    /// projection that does not normalize now leaves its term as it was.
    fn instantiate_for_leaf(&mut self, imp: ItemKey, trait_: ItemKey, args: TyList) -> Result<Option<(TyList, bool)>, Stop> {
        let Some(h) = self.reads.impl_header(imp)? else { return Ok(None) };
        if h.trait_ != Some(trait_) || h.polarity != crate::world::ImplPolarity::Positive || h.trait_args.list().len + 1 != self.ty.list_len(args) {
            return Ok(None);
        }
        let iargs = self.fresh_generic_args(imp, 0, h.generics)?;
        let impl_self = self.lower(h.types, h.self_ty, Lower::Args(iargs), Cause::Body)?;
        if !self.ty.unify(self.ty.list_get(args, 0), impl_self)? {
            return Ok(None);
        }
        let rest = h.types.list(h.trait_args.list())?;
        for (i, &p) in rest.iter().enumerate() {
            let t = self.lower(h.types, p, Lower::Args(iargs), Cause::Body)?;
            if !self.ty.unify(self.ty.list_get(args, i as u32 + 1), t)? {
                return Ok(None);
            }
        }
        let wc = self.reads.predicates(imp)?;
        for c in wc.clauses {
            let PClause::Projection { trait_: pt, args: pa, name, term } = *c else { continue };
            let pa = self.lower_resolved(wc.types, pa, iargs)?;
            let term = self.lower(wc.types, term, Lower::Args(iargs), Cause::Body)?;
            let got = match self.ty.normalize(pt, pa, name, &mut self.reads)? {
                Some(crate::ty::Normalized::Certain(t) | crate::ty::Normalized::Ambiguous(t)) => t,
                None => continue,
            };
            if !self.ty.unify(term, got)? {
                return Ok(Some((iargs, false)));
            }
        }
        Ok(Some((iargs, true)))
    }

    /// For an unmet or ambiguous `args[0]: trait_<args[1..]>` that is no auto trait: whether
    /// some impl whose header matches has no clause that is decided and cannot hold, and at
    /// least one auto-trait clause of a future, whose answer on the superset interior is not
    /// the exact one's (rules/async_fn.rs, 2). The goal holds when any impl does, so such an
    /// impl leaves it undecided: `Some` with that clause's witness and, when it is the impl's
    /// one such clause and names no probe variable, the clause (a stalled row can re-ask it);
    /// several give [`Witness::External`] and no clause, so the gap. `None`: every matching
    /// impl fails by a decided clause (or one the initial capture fails), and the goal's
    /// failure is a finding. One level: a clause that is itself no auto trait is not looked
    /// under (rustc reports such a goal with no leaf either).
    fn interior_candidate(&mut self, trait_: ItemKey, args: TyList) -> Result<Option<(Witness, Option<(ItemKey, TyList)>)>, Stop> {
        if self.ty.list_len(args) == 0 {
            return Ok(None);
        }
        let self_ty = self.ty.list_get(args, 0);
        let self_ty = self.ty.resolve(self_ty)?;
        let Some(shape) = self.shape(self_ty) else { return Ok(None) };
        let impls = self.reads.impls_of(trait_, shape)?;
        for &imp in impls {
            let found = self.probe(|m| m.interior_clauses_of(imp, trait_, args))?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// [`Self::interior_candidate`] for one impl, in the caller's probe.
    fn interior_clauses_of(&mut self, imp: ItemKey, trait_: ItemKey, args: TyList) -> Result<Option<(Witness, Option<(ItemKey, TyList)>)>, Stop> {
        let before_impl = self.ty.var_mark();
        let Some((iargs, true)) = self.instantiate_for_leaf(imp, trait_, args)? else { return Ok(None) };
        let wc = self.reads.predicates(imp)?;
        let mut suspects = 0u32;
        let mut first: Option<(Witness, ItemKey, TyList)> = None;
        for c in wc.clauses {
            let PClause::Trait { trait_: ct, args: ca } = *c else { continue };
            let ca = self.lower_resolved(wc.types, ca, iargs)?;
            let goal = Goal::Trait { trait_: ct, args: ca };
            // An auto-trait clause on a future is a suspect whatever the solver said of it: on
            // the superset interior it fails or stays ambiguous (the goal above it is then
            // `Ambiguous`, E0283's case: `async { .. }: Job` gave `CannotSatisfy`), and either
            // answer is the superset's, not the exact interior's. Any other clause is decided:
            // one that cannot hold fails the impl.
            let w = self.coroutine_interior_gap(goal)?;
            match w {
                Witness::FailedCapture => return Ok(None),
                Witness::Fixed => {
                    if self.ty.may_hold(goal, &mut self.reads)? {
                        continue;
                    }
                    return Ok(None);
                }
                Witness::LocalFlow | Witness::External | Witness::ForeignSuperset(..) => {}
            }
            suspects += 1;
            if first.is_none() {
                first = Some((w, ct, ca));
            }
        }
        match (suspects, first) {
            (1, Some((w, ct, ca))) if !self.ty.list_has_infer_since(ca, before_impl) => Ok(Some((w, Some((ct, ca))))),
            (_, Some((w, _, _))) => Ok(Some((w, None))),
            (_, None) => Ok(None),
        }
    }

    /// `try_structurally_resolve_type`: selects pending obligations first when the top is a
    /// variable, then lazily normalizes projections throughout the type before structural use.
    pub(crate) fn try_structurally_resolve(&mut self, t: Ty) -> Result<Ty, Stop> {
        self.normalize_aliases(t, AliasUse::Shape(self.place))
    }

    fn normalize_aliases(&mut self, raw_root: Ty, usage: AliasUse) -> Result<Ty, Stop> {
        let flags = self.ty.ty_flags(raw_root);
        if flags.is_empty() {
            return Ok(raw_root);
        }
        let mut root = self.ty.shallow_resolve(raw_root);
        if root != raw_root && self.ty.ty_flags(root).is_empty() {
            return Ok(root);
        }
        if flags.has(TyFlags::HAS_INFER) && matches!(self.ty.view(root), TyView::Infer(_) | TyView::ConstInfer(_)) {
            if matches!(usage, AliasUse::Equality) {
                return Ok(root);
            }
            self.select()?;
            let resolved = self.ty.shallow_resolve(root);
            if resolved == root || self.ty.ty_flags(resolved).is_empty() {
                return Ok(resolved);
            }
            root = resolved;
        }

        // Keep solved projections local to this walk: a later relation may bind a variable.
        let mut work: Vec<(Ty, bool), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut done = HashMap::new_in(self.alloc);
        let mut items = Vec::new_in(self.alloc);
        work.push((root, false));
        while let Some((raw, expanded)) = work.pop() {
            let t = self.ty.shallow_resolve(raw);
            // A variable scheduled by its parent may now be bound to a clean subtree.
            if self.ty.ty_flags(t).is_empty() {
                continue;
            }
            let view = self.ty.view(t);
            if !expanded && matches!(view, TyView::Infer(_) | TyView::ConstInfer(_)) {
                if matches!(usage, AliasUse::Shape(_)) {
                    self.select()?;
                    let resolved = self.ty.shallow_resolve(t);
                    if resolved != t {
                        work.push((resolved, false));
                    }
                }
                // An unbound variable is its own replacement.
                continue;
            }
            if done.contains_key(&t) {
                continue;
            }
            if !expanded {
                work.push((t, true));
                self.push_normalization_children(view, &mut work);
                continue;
            }
            let replacement = match view {
                TyView::Alias { trait_, name, args } => {
                    let args = self.normalized_list(args, &done, &mut items)?;
                    self.projection_for_use(t, trait_, args, name, usage)?
                }
                other => self.rebuild_normalized(t, other, &done, &mut items)?,
            };
            done.insert(t, replacement);
        }
        Ok(self.normalized_child(root, &done))
    }

    fn projection_for_use(&mut self, original: Ty, trait_: ItemKey, args: TyList, name: Name, usage: AliasUse) -> Result<Ty, Stop> {
        use crate::ty::Normalized;

        let projection = TyView::Alias { trait_, args, name };
        match usage {
            AliasUse::Equality => {
                // Lazy equality emits `projection == term` and relates the other side to
                // `term`. A rigid projection still compares as a constructor if its current
                // arguments give that same projection back (e.g. `T::Item` under `T: Tr`).
                let lazy = self.mk(projection)?;
                if original == lazy {
                    return Ok(lazy);
                }
                self.probe(|m| {
                    // Only the constructor over the existing `args` can leave the probe.
                    // Fulfillment owns any bindings needed to reduce it to another type.
                    Ok(match m.ty.normalize(trait_, args, name, &mut m.reads)? {
                        Some(Normalized::Certain(value)) if m.ty.view(value) == projection => value,
                        _ => lazy,
                    })
                })
            }
            AliasUse::Shape(cause) => match self.ty.normalize(trait_, args, name, &mut self.reads)? {
                Some(Normalized::Certain(value)) => Ok(value),
                Some(Normalized::Ambiguous(value)) => {
                    self.ty.register(Obligation { goal: Goal::Projection { trait_, args, name, term: value }, cause })?;
                    // The answer stands in for the projection: an E0282 about it names the
                    // projection (rule 19), not "a type the check made".
                    if self.is_ty_var(value) {
                        self.var_origins.push((value, crate::diag::VarOrigin::Projection { trait_, name }));
                    }
                    Ok(value)
                }
                None => self.mk(projection),
            },
        }
    }

    /// Push in reverse so selection and obligation registration still visit children in order.
    fn push_normalization_children(&self, view: TyView, work: &mut Vec<(Ty, bool), Alloc<'a>>) {
        let push = |t: Ty, work: &mut Vec<(Ty, bool), Alloc<'a>>| {
            if !self.ty.ty_flags(t).is_empty() {
                work.push((t, false));
            }
        };
        let list = |l: TyList, work: &mut Vec<(Ty, bool), Alloc<'a>>| {
            if !self.ty.list_flags(l).is_empty() {
                for i in (0..self.ty.list_len(l)).rev() {
                    push(self.ty.list_get(l, i), work);
                }
            }
        };
        match view {
            TyView::Tuple(l)
            | TyView::Record(_, l)
            | TyView::Union(l)
            | TyView::Adt(_, l)
            | TyView::FnDef(_, l)
            | TyView::FnPtr(l, _)
            | TyView::Opaque(_, l)
            | TyView::Dynamic(l)
            | TyView::ExTrait(_, l)
            | TyView::ExProj { args_term: l, .. }
            | TyView::Alias { args: l, .. } => list(l, work),
            TyView::ClosureOf { args, sig, upvars, .. } => {
                push(sig, work);
                if let Some(upvars) = upvars {
                    push(upvars, work);
                }
                list(args, work);
            }
            TyView::CoroutineOf { args, output, interior, .. } => {
                list(args, work);
                push(interior, work);
                push(output, work);
            }
            TyView::Ref(_, t) | TyView::Ptr(_, t) | TyView::Slice(t) | TyView::Closure(_, t) | TyView::ConstValue { ty: t, .. } => push(t, work),
            TyView::Future(_, output, interior) | TyView::Array(output, interior) => {
                push(interior, work);
                push(output, work);
            }
            TyView::Pat(base, l) => {
                list(l, work);
                push(base, work);
            }
            TyView::Bool
            | TyView::Char
            | TyView::Int(_)
            | TyView::Uint(_)
            | TyView::Float(_)
            | TyView::Str
            | TyView::Never
            | TyView::Param(_)
            | TyView::Infer(_)
            | TyView::ConstParam(_)
            | TyView::ConstInfer(_)
            | TyView::ExAuto(_)
            | TyView::Foreign(_)
            | TyView::Error => {}
        }
    }

    fn normalized_child(&mut self, child: Ty, done: &HashMap<Ty, Ty, Alloc<'a>>) -> Ty {
        if self.ty.ty_flags(child).is_empty() {
            return child;
        }
        let child = self.ty.shallow_resolve(child);
        done.get(&child).copied().unwrap_or(child)
    }

    fn normalized_list(
        &mut self, list: TyList, done: &HashMap<Ty, Ty, Alloc<'a>>, items: &mut Vec<Ty, Alloc<'a>>,
    ) -> Result<TyList, Stop> {
        if self.ty.list_flags(list).is_empty() {
            return Ok(list);
        }
        items.clear();
        let n = self.ty.list_len(list);
        for i in 0..n {
            let old = self.ty.list_get(list, i);
            let new = self.normalized_child(old, done);
            if items.is_empty() {
                if new == old {
                    continue;
                }
                // The prefix needs copying only when the list actually changes.
                items.extend((0..i).map(|j| self.ty.list_get(list, j)));
            }
            items.push(new);
        }
        if items.is_empty() { Ok(list) } else { self.mk_list(items) }
    }

    fn rebuild_normalized(
        &mut self, original: Ty, view: TyView, done: &HashMap<Ty, Ty, Alloc<'a>>, items: &mut Vec<Ty, Alloc<'a>>,
    ) -> Result<Ty, Stop> {
        use TyView::*;
        let rebuilt = match view {
            Tuple(l) => Tuple(self.normalized_list(l, done, items)?),
            Record(s, l) => Record(s, self.normalized_list(l, done, items)?),
            Union(l) => Union(self.normalized_list(l, done, items)?),
            Ref(m, t) => Ref(m, self.normalized_child(t, done)),
            Ptr(m, t) => Ptr(m, self.normalized_child(t, done)),
            Array(e, n) => Array(self.normalized_child(e, done), self.normalized_child(n, done)),
            Slice(t) => Slice(self.normalized_child(t, done)),
            Pat(b, l) => Pat(self.normalized_child(b, done), self.normalized_list(l, done, items)?),
            Adt(k, l) => Adt(k, self.normalized_list(l, done, items)?),
            FnDef(k, l) => FnDef(k, self.normalized_list(l, done, items)?),
            FnPtr(l, h) => FnPtr(self.normalized_list(l, done, items)?, h),
            Opaque(k, l) => Opaque(k, self.normalized_list(l, done, items)?),
            Closure(e, t) => Closure(e, self.normalized_child(t, done)),
            // The capture tuple (EB6) is a child like the signature: rebuilt from what the walk
            // normalized, or kept as it is when the walk did not visit it.
            ClosureOf { owner, ordinal, args, sig, kind, upvars } => ClosureOf {
                owner,
                ordinal,
                args: self.normalized_list(args, done, items)?,
                sig: self.normalized_child(sig, done),
                kind,
                upvars: upvars.map(|u| self.normalized_child(u, done)),
            },
            CoroutineOf { owner, ordinal, args, output, interior } => CoroutineOf {
                owner,
                ordinal,
                args: self.normalized_list(args, done, items)?,
                output: self.normalized_child(output, done),
                interior: self.normalized_child(interior, done),
            },
            Future(e, o, i) => Future(e, self.normalized_child(o, done), self.normalized_child(i, done)),
            Dynamic(l) => Dynamic(self.normalized_list(l, done, items)?),
            ExTrait(k, l) => ExTrait(k, self.normalized_list(l, done, items)?),
            ExProj { trait_, name, args_term } => ExProj { trait_, name, args_term: self.normalized_list(args_term, done, items)? },
            ConstValue { ty, bits } => ConstValue { ty: self.normalized_child(ty, done), bits },
            // The caller handles projections through the solver. Leaves have no children to rebuild.
            Alias { .. } | Bool | Char | Int(_) | Uint(_) | Float(_) | Str | Never | Param(_)
            | Infer(_) | ConstParam(_) | ConstInfer(_) | ExAuto(_) | Foreign(_) | Error => return Ok(original),
        };
        if rebuilt == view { Ok(original) } else { self.mk(rebuilt) }
    }

    /// `resolve_vars_with_obligations` (rustc_hir_typeck fn_ctxt/_impl.rs), the one home for the
    /// rule every `check_expr_coercible_to_type` site keeps (`demand_coerce_diag` calls it on
    /// the expected type before `coerce`): a target that still holds a variable ANYWHERE (not
    /// only at the top: `&?R`) has the pending obligations selected first, so a goal that one
    /// candidate decides fixes the variable before the source is coerced to it. This is what
    /// makes `p == &x` with `p: *const T` (the one `impl PartialEq for *const T` fixes the right
    /// operand's variable to `*const T`, then `&T` coerces to it), `(**self).partial_cmp(other)`
    /// with `other: &ArrayVec<T, N>` (`impl PartialOrd for [T]` fixes `Rhs = [T]`, then the
    /// argument deref-coerces) and `Ptr::Target::lt(self, other)` under `Ptr::Target:
    /// PartialOrd<Q::Target>` (the where-clause fixes `Rhs`) type as the pinned nightly types
    /// them; without it the variable is unified with the source's own type and the goal fails
    /// (E0277). Coercion also needs the shape of an already bound target, e.g. to unsize an
    /// array to a projected slice. A target with neither variables nor aliases is returned as is.
    pub(crate) fn resolve_with_obligations(&mut self, t: Ty) -> Result<Ty, Stop> {
        self.try_structurally_resolve(t)
    }

    /// `structurally_resolve_type`: as `try_structurally_resolve`, and a type variable left is
    /// E0282 (unless the body already has errors) and becomes the error type.
    pub(crate) fn structurally_resolve(&mut self, t: Ty, at: At) -> Result<Ty, Stop> {
        let t = self.try_structurally_resolve(t)?;
        if self.is_ty_var(t) {
            if self.errors == 0 {
                // What is undecided, not only where it was needed (rule 19): the variable's
                // origin and the goals pending on it.
                let undecided = self.undecided_of(t)?;
                self.errors += 1;
                self.diags.push(PendingDiag::new(DiagCode::AnnotationsNeeded, at, About::found(t)).with_undecided(undecided));
            } else {
                self.errors += 1;
            }
            let err = self.common.err;
            self.unify(t, err)?;
            return Ok(err);
        }
        Ok(t)
    }

    // Snapshots: `DropGuard`s, so any early return rolls back.

    /// A snapshot of the whole run's undoable state (rustc's `InferCtxt` snapshot also covers
    /// the fulfilment context and, through `TypeckResults` being written only on success, the
    /// diagnostics a probe may not leave behind): the type context's (variables and pending
    /// obligations), and the length of every append-only column a rule writes. The read list
    /// is kept on rollback: a probe's failure was decided by what it read.
    pub(crate) fn snapshot(&mut self) -> MachineSnapshot<C::Snapshot> {
        MachineSnapshot {
            ty: self.ty.snapshot(),
            diags: self.diags.len(),
            why_steps: self.why_steps.len(),
            dyn_seen: self.dyn_seen.len(),
            errors: self.errors,
            adjust: self.adjust.len(),
            adj_scratch: self.adj_scratch.len(),
            callees: self.callees.len(),
            defs: self.defs.len(),
            const_args: self.const_args.len(),
            fields: self.fields.len(),
            captures: self.captures.len(),
            capture_place_rows: self.capture_place_rows.len(),
            capture_proj_rows: self.capture_proj_rows.len(),
            casts: self.casts.len(),
            asms: self.asms.len(),
            numeric: self.numeric.len(),
            var_origins: self.var_origins.len(),
            diverging: self.diverging.len(),
            closures: self.closures.len(),
            projected: self.projected.len(),
            fru_fields: self.fru_fields.len(),
            default_fields: self.default_fields.len(),
            union_fields: self.union_fields.len(),
            union_pats: self.union_pats.len(),
            inferred_err_coercions: self.inferred_err_coercions.len(),
            deferred_try_obs: self.deferred_try_obs.len(),
        }
    }

    /// Undoes everything since `s`.
    pub(crate) fn rollback_to(&mut self, s: MachineSnapshot<C::Snapshot>) {
        self.ty.rollback_to(s.ty);
        self.diags.truncate(s.diags);
        self.why_steps.truncate(s.why_steps);
        self.dyn_seen.truncate(s.dyn_seen);
        self.errors = s.errors;
        self.adjust.truncate(s.adjust);
        self.adj_scratch.truncate(s.adj_scratch);
        self.callees.truncate(s.callees);
        self.defs.truncate(s.defs);
        self.const_args.truncate(s.const_args);
        self.fields.truncate(s.fields);
        self.captures.truncate(s.captures);
        self.capture_place_rows.truncate(s.capture_place_rows);
        self.capture_proj_rows.truncate(s.capture_proj_rows);
        self.casts.truncate(s.casts);
        self.asms.truncate(s.asms);
        self.numeric.truncate(s.numeric);
        self.var_origins.truncate(s.var_origins);
        self.diverging.truncate(s.diverging);
        self.closures.truncate(s.closures);
        self.projected.truncate(s.projected);
        self.fru_fields.truncate(s.fru_fields);
        self.default_fields.truncate(s.default_fields);
        self.union_fields.truncate(s.union_fields);
        self.union_pats.truncate(s.union_pats);
        self.inferred_err_coercions.truncate(s.inferred_err_coercions);
        self.deferred_try_obs.truncate(s.deferred_try_obs);
    }

    /// Runs `f` in a snapshot that is always rolled back (`probe`).
    pub(crate) fn probe<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let snap = self.snapshot();
        let mut g = DropGuard::new((self, snap), |(m, s)| m.rollback_to(s));
        f(&mut *g.0)
    }

    /// Runs `f` in a snapshot kept only when it returns `Ok(Some(_))` (`commit_if_ok`).
    pub(crate) fn commit_if<T, E>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let snap = self.snapshot();
        let mut g = DropGuard::new((self, snap), |(m, s)| m.rollback_to(s));
        match f(&mut *g.0) {
            Ok(Some(v)) => {
                let (m, s) = DropGuard::dismiss(g);
                m.ty.commit(s.ty);
                Ok(Some(v))
            }
            other => other,
        }
    }

    // Lowering portable types.

    fn in_body_record<T>(
        &mut self, record: crate::world::Question, node: PTy, read: impl FnOnce(&mut Self) -> Result<T, Stop>,
    ) -> Result<T, Stop> {
        if self.lowering_records.contains(&(record, node)) {
            return Err(CheckError::Malformed(Malformed::BodyRecordCycle { record, node: node.0 }).into());
        }
        const MAX_ACTIVE_RECORDS: usize = 256;
        if self.lowering_records.len() >= MAX_ACTIVE_RECORDS {
            return Err(CheckError::Limit(Limit::HiddenBodyNodes { record, node, limit: MAX_ACTIVE_RECORDS }).into());
        }
        self.lowering_records.push((record, node));
        let value = read(self);
        self.lowering_records.pop();
        value
    }

    /// Lowers `root` of a portable table, children first, with an explicit stack. An alias is
    /// normalized by a fresh variable and a projection obligation, as rustc normalizes a
    /// signature when it instantiates it.
    /// `cause` is the place the type is instantiated for (the call whose callee's signature it
    /// is, the path naming the item): a projection in it that fails is reported there.
    /// A parameter past `Lower::Args`'s list is `Malformed::Param` naming this call's caller
    /// (`ParamSite::Lower`).
    #[track_caller]
    pub(crate) fn lower(&mut self, types: PTys<'_>, root: PTy, mode: Lower, cause: Cause) -> Result<Ty, Stop> {
        let by = core::panic::Location::caller();
        self.lower_at(types, root, mode, cause, None).map_err(|e| match e {
            Stop::Error(CheckError::Malformed(Malformed::Param { index, args, site: crate::error::ParamSite::Lower { at: None } })) => {
                let site = crate::error::ParamSite::Lower { at: Some(by) };
                Stop::Error(CheckError::Malformed(Malformed::Param { index, args, site }))
            }
            e => e,
        })
    }

    /// [`Machine::lower`]; with `dyn_at`, each trait object's principal in the type is checked
    /// for dyn compatibility and reported there ([`Machine::check_dyn`]).
    fn lower_at(&mut self, types: PTys<'_>, root: PTy, mode: Lower, cause: Cause, dyn_at: Option<At>) -> Result<Ty, Stop> {
        let n = root.0 as usize + 1;
        // `len_now`: a live view's later types are lowered too (portable.rs `PTys`).
        if n > types.len_now() {
            return Err(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into());
        }
        let mut low = Lowered { memo: blaze_hash::HashMap::new_in(self.alloc), pool: Vec::new_in(self.alloc) };
        // A node is `holey` when a hole (`_`) is in its subtree. The table interns nodes, so
        // two written holes are ONE `Infer` node, and `Vec<_>` written twice is one node too: a
        // holey node is never memoized (one memo entry would make the two holes one
        // variable); it is lowered once per written occurrence, after this loop
        // ([`Machine::lower_holes`]). Filled (and read) only once a hole was met, so a type
        // without holes pays nothing.
        // A set of the walk's nodes, never a vector by index (as `low.memo`).
        let mut holey: blaze_hash::HashSet<u32, Alloc<'a>> = blaze_hash::HashSet::new_in(self.alloc);
        let mut stack: Vec<(u32, bool), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        stack.push((root.0, false));
        // Each node is expanded once and finished once; each list entry is pushed at most once
        // per expansion of its parent.
        let bound = 2 * (n + types.lists_len_now()) + 2;
        let mut steps = 0usize;
        while let Some((at, expanded)) = stack.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::LowerSteps { owner: self.body.owner, node: PTy(at), steps, limit: bound }).into());
            }
            if low.memo.contains_key(&at) || holey.contains(&at) {
                continue;
            }
            let node = types.node(PTy(at))?;
            if !expanded {
                stack.push((at, true));
                kids.clear();
                children(node, types, &mut kids)?;
                for &c in kids.iter() {
                    if c.0 >= at {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: at, child: c.0 }).into());
                    }
                    if !low.memo.contains_key(&c.0) && !holey.contains(&c.0) {
                        stack.push((c.0, false));
                    }
                }
                continue;
            }
            let mut hole = matches!(node, PNode::Infer);
            if !holey.is_empty() && !hole {
                kids.clear();
                children(node, types, &mut kids)?;
                hole = kids.iter().any(|c| holey.contains(&c.0));
            }
            if hole {
                holey.insert(at);
                continue;
            }
            let ty = self.lower_node(node, types, &mut low, mode, cause)?;
            low.memo.insert(at, ty);
        }
        // The lowered nodes are exactly the type's subtree: checked in node order, so the
        // findings do not follow the maps' order.
        if let Some(at) = dyn_at {
            let mut reached: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(low.memo.len() + holey.len(), self.alloc);
            reached.extend(low.memo.iter().map(|(&i, _)| i));
            reached.extend(holey.iter().copied());
            reached.sort_unstable();
            for &i in reached.iter() {
                if let PNode::Dynamic(l) = types.node(PTy(i))?
                    && let Some(principal) = crate::portable::object_principal(types, l)
                {
                    self.check_dyn(principal, at)?;
                }
            }
        }
        if holey.contains(&root.0) {
            return self.lower_holes(types, root, mode, cause, &mut low, &holey);
        }
        match low.memo.get(&root.0) {
            Some(&t) => Ok(t),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into()),
        }
    }

    /// Lowers a type that holds a hole, once per WRITTEN OCCURRENCE of each holey node (the
    /// Reference, "Inferred type": "The inferred type asks the compiler to infer the type if
    /// possible based on the surrounding information available": each written `_` asks on its
    /// own, so `(_, _)` is two variables, and `(Vec<_>, Vec<_>)` two too). `low.memo` holds
    /// every node of the subtree without a hole ([`Machine::lower_at`] lowered those once: a
    /// type without a hole means the same wherever it is written); each occurrence of a holey
    /// node goes to `low.pool` when it is lowered, and its parent takes it from there
    /// ([`Machine::lowered`]). The walk is over the written tree, so its steps are the
    /// written type's size; a table whose sharing makes that tree larger than
    /// [`HOLE_STEPS`] is refused ([`Limit::LowerHoleSteps`]).
    fn lower_holes(&mut self, types: PTys<'_>, root: PTy, mode: Lower, cause: Cause, low: &mut Lowered<'a>, holey: &blaze_hash::HashSet<u32, Alloc<'a>>) -> Result<Ty, Stop> {
        let mut stack: Vec<(u32, bool), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        stack.push((root.0, false));
        let mut steps = 0usize;
        while let Some((at, expanded)) = stack.pop() {
            steps += 1;
            if steps > HOLE_STEPS {
                return Err(CheckError::Limit(Limit::LowerHoleSteps { owner: self.body.owner, node: PTy(at), steps, limit: HOLE_STEPS }).into());
            }
            let node = types.node(PTy(at))?;
            if expanded {
                let ty = self.lower_node(node, types, low, mode, cause)?;
                low.pool.push((at, ty));
                continue;
            }
            stack.push((at, true));
            kids.clear();
            children(node, types, &mut kids)?;
            // An array's `_` length is made by the array's own arm (a const variable), which
            // never reads the node: no occurrence is lowered for it.
            let len_hole = match (node, mode) {
                (PNode::Array(_, len), Lower::Body) if matches!(types.node(len)?, PNode::Infer) => Some(len),
                _ => None,
            };
            for &c in kids.iter() {
                // `lower_at` checked the order of every node reached here.
                if holey.contains(&c.0) && Some(c) != len_hole {
                    stack.push((c.0, false));
                }
            }
        }
        Self::lowered(low, root)
    }

    /// What the child `t` of the node being lowered was lowered to: the memo's entry, or (a
    /// node that holds a hole) one of its occurrences waiting in the pool: which one does not
    /// matter, each is a lowering of the same node with its own fresh variables.
    fn lowered(low: &mut Lowered<'a>, t: PTy) -> Result<Ty, Stop> {
        if let Some(&t) = low.memo.get(&t.0) {
            return Ok(t);
        }
        match low.pool.iter().rposition(|&(n, _)| n == t.0) {
            Some(i) => Ok(low.pool.swap_remove(i).1),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: t.0, child: t.0 }).into()),
        }
    }

    fn lowered_list(&mut self, memo: &mut Lowered<'a>, types: PTys<'_>, l: crate::ids::List) -> Result<TyList, Stop> {
        let src = types.list(l)?;
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(src.len(), self.alloc);
        for &t in src {
            v.push(Self::lowered(memo, t)?);
        }
        self.mk_list(&v)
    }

    fn lower_node(&mut self, node: PNode, types: PTys<'_>, memo: &mut Lowered<'a>, mode: Lower, cause: Cause) -> Result<Ty, Stop> {
        // A trait fn's `impl Trait` at a concrete `Self` (rules/rpitit_use.rs): the projection
        // it is in Rust (RFC 3425; the Reference, "Impl trait"), which the impl's row
        // normalizes to the type the impl's fn returns.
        if let PNode::Opaque(k, l) = node
            && let Some(&(_, trait_, name)) = self.rpitit.iter().find(|r| r.0 == k)
        {
            let args = self.lowered_list(memo, types, l)?;
            return self.lowered_projection(trait_, name, args, mode, cause);
        }
        let view = match node {
            PNode::Bool => TyView::Bool,
            PNode::Char => TyView::Char,
            PNode::Int(i) => TyView::Int(i),
            PNode::Uint(u) => TyView::Uint(u),
            PNode::Float(f) => TyView::Float(f),
            PNode::Str => TyView::Str,
            PNode::Never => TyView::Never,
            PNode::Tuple(l) => TyView::Tuple(self.lowered_list(memo, types, l)?),
            PNode::Record(s, l) => {
                if let Some(names) = types.record_names(s) {
                    self.register_record_schema(s, names)?;
                }
                TyView::Record(s, self.lowered_list(memo, types, l)?)
            }
            PNode::Union(l) => TyView::Union(self.lowered_list(memo, types, l)?),
            PNode::Ref(m, t) => TyView::Ref(m, Self::lowered(memo, t)?),
            PNode::Ptr(m, t) => TyView::Ptr(m, Self::lowered(memo, t)?),
            // A hole for a length (`[u8; _]`) is a const variable, not the type variable the
            // shared `Infer` node lowered to.
            PNode::Array(t, n) => {
                let len = match (types.node(n)?, mode) {
                    (PNode::Infer, Lower::Body) => self.ty.fresh(VarKind::Const)?,
                    _ => Self::lowered(memo, n)?,
                };
                TyView::Array(Self::lowered(memo, t)?, len)
            }
            PNode::Slice(t) => TyView::Slice(Self::lowered(memo, t)?),
            PNode::Pat(t, l) => TyView::Pat(Self::lowered(memo, t)?, self.lowered_list(memo, types, l)?),
            PNode::Adt(k, l) => TyView::Adt(k, self.lowered_list(memo, types, l)?),
            PNode::FnDef(k, l) => TyView::FnDef(k, self.lowered_list(memo, types, l)?),
            PNode::FnPtr(l, h) => TyView::FnPtr(self.lowered_list(memo, types, l)?, h),
            // A closure of another body: its identity from the record, its signature and kind
            // from its owner's check (typeck.md, "A closure's identity").
            PNode::Closure { owner, ordinal, args } => {
                let args = self.lowered_list(memo, types, args)?;
                let Some((facts, upvars)) = self.reads.closure(owner, ordinal)? else {
                    return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(crate::world::Question::Closure(owner, ordinal))).into());
                };
                let record = crate::world::Question::Closure(owner, ordinal);
                let sig = self.in_body_record(record, facts.sig, |m| m.lower(facts.types, facts.sig, Lower::Args(args), cause))?;
                let upvars = upvars.map(|p| self.in_body_record(record, p, |m| m.lower(facts.types, p, Lower::Args(args), cause))).transpose()?;
                TyView::ClosureOf { owner, ordinal, args, sig, kind: facts.kind, upvars }
            }
            PNode::Future(block, output, interior) => {
                if let Lower::CoroutineArgs { owner, ordinal, .. } = mode {
                    let use_site = if matches!(cause, Cause::Body) { At::Expr(self.body.root) } else { At::of(cause) };
                    return Err(CheckError::Malformed(Malformed::NestedFutureIdentity {
                        owner, ordinal, block, reader: Some((self.body.owner, self.range_of(use_site))),
                    }).into());
                }
                TyView::Future(block, Self::lowered(memo, output)?, Self::lowered(memo, interior)?)
            }
            // A future of another body: its identity here, its output and interior from its
            // owner's check (`Question::Coroutine`), as a closure's signature is.
            PNode::Coroutine { owner, ordinal, args } => {
                let args = self.lowered_list(memo, types, args)?;
                let Some(facts) = self.reads.coroutine_record(owner, ordinal)? else {
                    let site = match cause {
                        Cause::Body => At::Expr(self.body.root),
                        _ => At::of(cause),
                    };
                    return Err(CheckError::Malformed(Malformed::CoroutineFactsMissing {
                        reader: self.body.owner, at: self.range_of(site), owner, ordinal,
                    }).into());
                };
                if facts.is == crate::InteriorIs::Superset {
                    return Err(self.witness_gap_of(Witness::ForeignSuperset(owner, ordinal), At::of(cause)).into());
                }
                let mode = Lower::CoroutineArgs { args, owner, ordinal };
                let record = crate::world::Question::Coroutine(owner, ordinal);
                let output = self.in_body_record(record, facts.output, |m| m.lower(facts.types, facts.output, mode, cause))?;
                let interior = self.in_body_record(record, facts.interior, |m| m.lower(facts.types, facts.interior, mode, cause))?;
                TyView::CoroutineOf { owner, ordinal, args, output, interior }
            }
            PNode::Param(i) => match mode {
                Lower::Args(args) | Lower::CoroutineArgs { args, .. } => {
                    let len = self.ty.list_len(args);
                    if i >= len {
                        // `Machine::lower` fills in its caller.
                        let site = crate::error::ParamSite::Lower { at: None };
                        return Err(CheckError::Malformed(Malformed::Param { index: i, args: len, site }).into());
                    }
                    return Ok(self.ty.list_get(args, i));
                }
                Lower::Body | Lower::Assumed => TyView::Param(i),
            },
            PNode::ConstParam(i) => match mode {
                Lower::Args(args) | Lower::CoroutineArgs { args, .. } => {
                    let len = self.ty.list_len(args);
                    if i >= len {
                        let site = crate::error::ParamSite::Lower { at: None };
                        return Err(CheckError::Malformed(Malformed::Param { index: i, args: len, site }).into());
                    }
                    return Ok(self.ty.list_get(args, i));
                }
                Lower::Body | Lower::Assumed => TyView::ConstParam(i),
            },
            PNode::ConstValue { ty, bits } => TyView::ConstValue { ty: Self::lowered(memo, ty)?, bits },
            PNode::Alias { trait_, name, args } if matches!(mode, Lower::Assumed) => {
                TyView::Alias { trait_, name, args: self.lowered_list(memo, types, args)? }
            }
            PNode::Alias { trait_, name, args } => {
                let args = self.lowered_list(memo, types, args)?;
                // A written projection onto a generic associated type with the wrong count
                // of arguments is E0107 and the error type (rules/assoc_args.rs); a world's
                // answer (`Lower::Args`) is not re-checked here.
                if matches!(mode, Lower::Body) {
                    let written = self.ty.list_len(args);
                    if !self.alias_arity(trait_, name, written, cause)? {
                        return self.mk(TyView::Error);
                    }
                }
                return self.lowered_projection(trait_, name, args, mode, cause);
            }
            PNode::Opaque(k, _) if self.opening => return self.defining_var(k),
            PNode::Opaque(k, l) => TyView::Opaque(k, self.lowered_list(memo, types, l)?),
            // The rows are in `PortableTys::dynamic`'s order, which lowering keeps.
            PNode::Dynamic(l) => TyView::Dynamic(self.lowered_list(memo, types, l)?),
            PNode::ExTrait(k, l) => TyView::ExTrait(k, self.lowered_list(memo, types, l)?),
            PNode::ExProj { trait_, name, args, term } => {
                let mut v: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
                for &a in types.list(args)? {
                    v.push(Self::lowered(memo, a)?);
                }
                v.push(Self::lowered(memo, term)?);
                TyView::ExProj { trait_, name, args_term: self.mk_list(&v)? }
            }
            PNode::ExAuto(k) => TyView::ExAuto(k),
            PNode::Foreign(k) => TyView::Foreign(k),
            PNode::Infer => match mode {
                Lower::Body => return self.fresh(),
                Lower::Args(_) | Lower::CoroutineArgs { .. } | Lower::Assumed => return Err(CheckError::Malformed(Malformed::HoleInAnswer).into()),
            },
            PNode::Error => TyView::Error,
        };
        self.mk(view)
    }

    /// Registers `schema -> sorted_names` on this machine (idempotent).
    pub(crate) fn register_record_schema(&mut self, schema: RecordSchema, sorted_names: &[Name]) -> Result<(), Stop> {
        if self.record_schemas.iter().any(|&(s, _)| s == schema) {
            return Ok(());
        }
        let len = u32::try_from(sorted_names.len()).map_err(|_| CheckError::Limit(Limit::Table))?;
        let list = if len == 0 {
            List::EMPTY
        } else {
            let start = u32::try_from(self.record_names.len()).map_err(|_| CheckError::Limit(Limit::Table))?;
            self.record_names.extend_from_slice(sorted_names);
            List { start, len }
        };
        self.record_schemas.push((schema, list));
        Ok(())
    }

    /// Canonical sorted field names for `schema`, if known to this machine.
    pub(crate) fn record_names(&self, schema: RecordSchema) -> Option<&[Name]> {
        let &(_, list) = self.record_schemas.iter().find(|&&(s, _)| s == schema)?;
        if list.len == 0 {
            return Some(&[]);
        }
        self.record_names.get(list.range())
    }

    /// Canonical field index of `name` in `schema`, if known.
    pub(crate) fn record_field_index(&self, schema: RecordSchema, name: Name) -> Option<u32> {
        let names = self.record_names(schema)?;
        names.iter().position(|&n| n == name).and_then(|i| u32::try_from(i).ok())
    }

    /// A written type of the body.
    /// A written type of the body, at `at`: a `dyn` in it is checked for dyn compatibility.
    pub(crate) fn lower_body_ty(&mut self, t: PTy, at: At) -> Result<Ty, Stop> {
        let types = self.body.types;
        self.lower_at(types, t, Lower::Body, at.cause(), Some(at))
    }

    /// The owner's signature type `t` of `types`, checked for `dyn` as written in the body; a
    /// projection in it that fails is a finding at `cause` ([`Machine::sig_place`]).
    pub(crate) fn lower_sig_ty(&mut self, types: PTys<'_>, t: PTy, cause: Cause) -> Result<Ty, Stop> {
        #[cfg(test)]
        let cause = if self.plant == Plant::ObligationsWithoutPlace { Cause::Body } else { cause };
        self.lower_at(types, t, Lower::Body, cause, Some(At::Body))
    }

    /// Where an obligation of the owner's signature type `i` is filed: input `i`'s parameter
    /// pattern, and for the output (`i` past the inputs) the body's own expression. The record's
    /// signature keeps no range of its types (world.rs `FnSig`), so the finding is at the
    /// nearest place the body writes: the parameter the type is written beside, the body whose
    /// value the output is. A body with fewer parameter patterns than inputs has no such place
    /// (`Cause::Body`), and a finding there stops the check as `DiagWithoutPlace`.
    pub(crate) fn sig_place(&self, i: usize, inputs: usize) -> Cause {
        if i >= inputs {
            return Cause::Expr(self.body.root);
        }
        match self.body.params.get(i) {
            Some(&p) => Cause::Pat(p),
            None => Cause::Body,
        }
    }

    /// `principal` in a `dyn` written at `at` (the Reference, "Traits", "Dyn compatibility"):
    /// E0038 naming the trait, the member and the rule when it is not dyn compatible, once per
    /// trait per body; a trait the world cannot decide is the typed gap `DynUndecided`.
    pub(crate) fn check_dyn(&mut self, principal: ItemKey, at: At) -> Result<(), Stop> {
        if self.dyn_seen.contains(&principal) {
            return Ok(());
        }
        match crate::dyn_compat::verdict(&mut self.reads, principal)? {
            crate::dyn_compat::Verdict::Compatible => {}
            crate::dyn_compat::Verdict::Not { trait_, by, member, rule } => {
                self.errors += 1;
                let because = Because::Dyn { trait_, by, member, rule };
                self.diags.push(PendingDiag::because(DiagCode::DynIncompatible, at, because, None, member));
            }
            crate::dyn_compat::Verdict::Gap { trait_, by, why } => {
                return Err(CheckError::DynUndecided { trait_, by, why }.into());
            }
        }
        self.dyn_seen.push(principal);
        Ok(())
    }

    /// A list of a world's answer, lowered with `args`.
    pub(crate) fn lower_list(&mut self, types: PTys<'_>, l: crate::ids::List, args: TyList, cause: Cause) -> Result<TyList, Stop> {
        let src = types.list(l)?;
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(src.len(), self.alloc);
        for &t in src {
            v.push(self.lower(types, t, Lower::Args(args), cause)?);
        }
        self.mk_list(&v)
    }

    /// The active unresolved root of `self.inferred_err_var`, if still an inference variable.
    pub(crate) fn active_inferred_err_var(&mut self) -> Option<Ty> {
        let ev = self.inferred_err_var?;
        let r = self.ty.shallow_resolve(ev);
        self.is_ty_var(r).then_some(r)
    }

    /// Registers `item`'s where-clauses, instantiated with `args`
    /// (`add_required_obligations_for_hir`).
    pub(crate) fn register_predicates(&mut self, item: ItemKey, args: TyList, cause: Cause) -> Result<(), Stop> {
        if self.inferred_err_var.is_some()
            && self.reads.lang_path(LangItem::TryTraitFromResidual)? == Some(Def::Fn(item))
        {
            self.deferred_try_obs.push((item, args, cause));
            return Ok(());
        }
        let preds = self.reads.predicates(item)?;
        for (nth, clause) in preds.clauses.iter().enumerate() {
            let Some(goal) = self.clause_goal(preds.types, *clause, args, cause)? else { continue };
            // A use's clause is filed, if unmet, where its text is (rules/obligation_site.rs).
            let ob_cause = match cause {
                Cause::Expr(expr) => Cause::Clause { expr, nth: u32::try_from(nth).unwrap_or(u32::MAX) },
                other => other,
            };
            self.ty.register(Obligation { goal, cause: ob_cause })?;
        }
        Ok(())
    }

    /// One where-clause of an item as the goal it is under the item's arguments `args`: a
    /// trait clause, a projection binding (`L: Launch<T, Node = Handle<N, A>>` is the goal
    /// `<L as Launch<T>>::Node == Handle<N, A>`, the one goal that decides an impl parameter
    /// only such a binding names), a `where const` condition. `None` for an outlives clause:
    /// the borrow checker's (D148), the checker takes it as true. The one lowering of a
    /// where-clause into a goal (rule 30): every path that instantiates an item takes its
    /// clauses through it.
    pub(crate) fn clause_goal(&mut self, types: PTys<'_>, clause: PClause, args: TyList, cause: Cause) -> Result<Option<Goal>, Stop> {
        Ok(Some(match clause {
            PClause::Trait { trait_, args: l } => Goal::Trait { trait_, args: self.lower_list(types, l, args, cause)? },
            PClause::Projection { trait_, args: l, name, term } => Goal::Projection {
                trait_,
                args: self.lower_list(types, l, args, cause)?,
                name,
                term: self.lower(types, term, Lower::Args(args), cause)?,
            },
            PClause::Const { cond, args: l } => Goal::ConstHolds { cond, args: self.lower_list(types, l, args, cause)? },
            PClause::ForFields { container, trait_, args: l } => Goal::ForFields {
                container: self.lower(types, container, Lower::Args(args), cause)?,
                trait_,
                args: self.lower_list(types, l, args, cause)?,
            },
            PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => return Ok(None),
        }))
    }

    /// Registers `args[0]: trait_<args[1..]>`.
    pub(crate) fn register_trait(&mut self, trait_: ItemKey, args: TyList, cause: Cause) -> Result<(), Stop> {
        self.ty.register(Obligation { goal: Goal::Trait { trait_, args }, cause })?;
        Ok(())
    }

    /// A projection `lower` meets, `<args[0] as trait_<args[1..]>>::name` with `args` lowered
    /// (a written one past its arity check): a fresh variable and its projection obligation.
    /// A callee's signature instantiated at a call (`Lower::Args`, at an expression): the impl
    /// chosen for it is written back per call (src/selected.rs). The body's own signature
    /// (rules/control.rs): its projection's impl is written back at the body's root, for the
    /// borrow check's design B (blaze-flow-lower `Lower::lay_through_impl`). A fn body's root
    /// is its block (an async fn's, its `async move` block), never a call, so no call's rows
    /// share that expression; every other reader of the rows asks at a call.
    fn lowered_projection(&mut self, trait_: ItemKey, name: Name, args: TyList, mode: Lower, cause: Cause) -> Result<Ty, Stop> {
        let term = self.fresh()?;
        self.ty.register(Obligation { goal: Goal::Projection { trait_, args, name, term }, cause })?;
        if let (Lower::Args(_), Cause::Expr(e)) = (mode, cause) {
            self.projected.push((e, trait_, Some(name), args));
        }
        if self.sig_rows && matches!(mode, Lower::Body) {
            self.projected.push((self.body.root, trait_, Some(name), args));
        }
        Ok(term)
    }

    /// `<args[0] as trait_<args[1..]>>::name` as a fresh variable and a projection obligation.
    pub(crate) fn project(&mut self, trait_: ItemKey, args: TyList, name: Name, cause: Cause) -> Result<Ty, Stop> {
        let term = self.fresh()?;
        self.ty.register(Obligation { goal: Goal::Projection { trait_, args, name, term }, cause })?;
        Ok(term)
    }

    pub(crate) fn lang(&mut self, item: LangItem) -> Result<Option<ItemKey>, Stop> {
        self.reads.lang_item(item)
    }

    /// The lang item `item` that the form at `at` needs; when the world cannot answer it, the
    /// one finding "the lang item `item` is not known to this world" (`missing_lang`).
    pub(crate) fn need_lang(&mut self, item: LangItem, at: At) -> Result<Option<ItemKey>, Stop> {
        let got = self.lang(item)?;
        if got.is_none() {
            self.missing_lang(item, at);
        }
        Ok(got)
    }

    /// Records `DiagCode::MissingLangItem` naming `item` at `at`. The body is tainted.
    pub(crate) fn missing_lang(&mut self, item: LangItem, at: At) {
        self.errors += 1;
        self.diags.push(PendingDiag::because(DiagCode::MissingLangItem, at, Because::NoLangItem(item), None, None));
    }

    /// A finding of `code` about `goal` entire (undecided, or unmet when `unmet`), with the leaf
    /// where-clause that failed under an unmet one where the checker names one; `found` is the
    /// goal's self type.
    /// The type `<args[0] as trait_<args[1..]>>::name` is when one impl or bound selects it
    /// and decides it with no variable of its own (asked in a probe: nothing stays bound): the
    /// type an E0271 says was found. `None` when the trait goal fails or the type is not decided.
    pub(crate) fn projection_found(&mut self, trait_: ItemKey, args: TyList, name: Name) -> Result<Option<Ty>, Stop> {
        self.probe(|m| -> Result<Option<Ty>, Stop> {
            let mark = m.ty.var_mark();
            let Some(crate::ty::Normalized::Certain(value)) = m.ty.normalize(trait_, args, name, &mut m.reads)? else {
                return Ok(None);
            };
            let value = m.ty.resolve(value)?;
            let one = m.mk_list(&[value])?;
            Ok((!m.ty.list_has_infer_since(one, mark)).then_some(value))
        })
    }

    fn push_goal_finding(&mut self, code: DiagCode, at: At, goal: PendingGoal, unmet: bool) -> Result<(), Stop> {
        let (PendingGoal::Trait { trait_, args } | PendingGoal::Projection { trait_, args, .. }) = goal;
        let found = if self.ty.list_len(args) > 0 { Some(self.ty.list_get(args, 0)) } else { None };
        let (leaf, why) = match (unmet, goal) {
            // An unmet trait goal: one walk of the solver gives the chain and its leaf, the
            // last step's goal under the root (`TypeCtx::why_leaf`'s rule).
            (true, PendingGoal::Trait { .. }) => match self.why_chain(trait_, args)? {
                Some(why) => {
                    let last = if why.len >= 2 { self.why_steps.get((why.start + why.len - 1) as usize).and_then(|s| s.goal) } else { None };
                    (last, Some(why))
                }
                None => (None, None),
            },
            (true, PendingGoal::Projection { .. }) => (self.failed_leaf(trait_, args)?, None),
            // An undecided goal (E0283, E0282): the chain to the goal under it that is
            // undecided by itself (`Solver::why_maybe`). Its leaf is no where-clause to fix:
            // the chain says what is not known.
            (false, PendingGoal::Trait { .. }) => (None, self.why_maybe_chain(Goal::Trait { trait_, args })?),
            (false, PendingGoal::Projection { name, term, .. }) => (None, self.why_maybe_chain(Goal::Projection { trait_, args, name, term })?),
        };
        let leaf = leaf.map(|(trait_, args)| PendingGoal::Trait { trait_, args });
        self.errors += 1;
        self.diags.push(match why {
            Some(why) => PendingDiag::unmet_why(code, at, found, goal, leaf, why),
            None => PendingDiag::unmet(code, at, found, goal, leaf),
        });
        Ok(())
    }

    /// The impl-lookup shape of `t`, or `None` for a variable or the error type.
    /// A trait object's principal trait and its arguments after `Self`, from its rows `l`.
    pub(crate) fn principal_args(&mut self, l: TyList) -> Option<(ItemKey, TyList)> {
        if self.ty.list_len(l) == 0 {
            return None;
        }
        let first = self.ty.list_get(l, 0);
        match self.view(first) {
            TyView::ExTrait(k, args) => Some((k, args)),
            _ => None,
        }
    }

    fn principal(&mut self, l: TyList) -> Option<ItemKey> {
        self.principal_args(l).map(|(k, _)| k)
    }

    pub(crate) fn shape(&mut self, t: Ty) -> Option<Shape> {
        Some(match self.view(t) {
            TyView::Bool => Shape::Bool,
            TyView::Char => Shape::Char,
            TyView::Int(i) => Shape::Int(i),
            TyView::Uint(u) => Shape::Uint(u),
            TyView::Float(f) => Shape::Float(f),
            TyView::Str => Shape::Str,
            TyView::Never => Shape::Never,
            TyView::Tuple(l) => Shape::Tuple(self.ty.list_len(l)),
            TyView::Record(s, l) => Shape::Record(s.0, self.ty.list_len(l)),
            TyView::Union(l) => Shape::Union(self.ty.list_len(l)),
            TyView::Ref(m, _) => Shape::Ref(m),
            TyView::Ptr(m, _) => Shape::Ptr(m),
            TyView::Array(..) => Shape::Array,
            TyView::Slice(_) => Shape::Slice,
            TyView::Pat(..) => Shape::Pat,
            TyView::Adt(k, _) => Shape::Adt(k),
            TyView::FnDef(k, _) => Shape::FnDef(k),
            TyView::FnPtr(l, _) => Shape::FnPtr(self.ty.list_len(l).saturating_sub(1)),
            // A future buckets with closures: neither can be named in an impl header, so only
            // blanket impls apply to either.
            TyView::Closure(..) | TyView::ClosureOf { .. } | TyView::Future(..) | TyView::CoroutineOf { .. } => Shape::Closure,
            TyView::Param(_) => Shape::Param,
            TyView::Opaque(k, _) => Shape::Opaque(k),
            TyView::Foreign(k) => Shape::Foreign(k),
            TyView::Dynamic(l) => Shape::Dynamic(self.principal(l)),
            TyView::Alias { .. } | TyView::Infer(_) | TyView::Error => return None,
            // A const term or an object's row is no self type.
            TyView::ConstValue { .. } | TyView::ConstParam(_) | TyView::ConstInfer(_) => return None,
            TyView::ExTrait(..) | TyView::ExProj { .. } | TyView::ExAuto(_) => return None,
        })
    }

    /// The autoderef steps of `base` (rustc's `Autoderef`): `base` first, then each builtin or
    /// overloaded deref, until a type that does not deref, a variable, or the recursion limit
    /// (the second value: E0055 when true).
    pub(crate) fn autoderef(&mut self, base: Ty, raw_ptrs: bool) -> Result<(Vec<Step, Alloc<'a>>, bool), Stop> {
        #[cfg_attr(not(test), expect(unused_mut, reason = "only the planted bug changes the steps"))]
        let mut out = self.autoderef_all(base, raw_ptrs)?;
        #[cfg(test)]
        if self.plant == Plant::AutoderefShort && out.0.len() > 1 {
            out.0.pop();
        }
        Ok(out)
    }

    fn autoderef_all(&mut self, base: Ty, raw_ptrs: bool) -> Result<(Vec<Step, Alloc<'a>>, bool), Stop> {
        let mut steps: Vec<Step, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut cur = self.try_structurally_resolve(base)?;
        steps.push(Step { ty: cur, overloaded: false });
        for _ in 0..RECURSION_LIMIT {
            let (next, overloaded) = match self.view(cur) {
                TyView::Ref(_, t) => (t, false),
                TyView::Ptr(_, t) if raw_ptrs => (t, false),
                // An object can itself implement Deref, e.g. `impl Deref for dyn Inspector`.
                // Its Target is another receiver step with its own inherent methods.
                TyView::Adt(..) | TyView::Param(_) | TyView::Opaque(..) | TyView::Dynamic(_) => {
                    match self.overloaded_deref_at(cur, DerefAt::Autoderef)? {
                        Some(t) => (t, true),
                        None => return Ok((steps, false)),
                    }
                }
                _ => return Ok((steps, false)),
            };
            cur = self.try_structurally_resolve(next)?;
            steps.push(Step { ty: cur, overloaded });
        }
        Ok((steps, true))
    }

    /// `<t as Deref>::Target`, if `t: Deref` may hold. Whether the target must also select
    /// now is `at`'s ([`DerefAt`]).
    pub(crate) fn overloaded_deref_at(&mut self, t: Ty, at: DerefAt) -> Result<Option<Ty>, Stop> {
        let Some(deref) = self.lang(LangItem::Deref)? else {
            return Ok(None);
        };
        let args = self.mk_list(&[t])?;
        if !self.ty.may_hold(Goal::Trait { trait_: deref, args }, &mut self.reads)? {
            return Ok(None);
        }
        let target = Name::of("Target");
        match self.ty.normalize(deref, args, target, &mut self.reads)? {
            Some(crate::ty::Normalized::Certain(t)) => Ok(Some(t)),
            // The impl is selected and gives the target; a where-clause of it is undecided now
            // (a closure's kind settles at the end of the body): the target is the step, and
            // the projection stays an obligation at the node being checked, proved or refused
            // with the rest (the Reference, "Type coercions", "Coercion types": `&T` to `&U`
            // when `T: Deref<Target = U>`; the bound must hold).
            Some(crate::ty::Normalized::Ambiguous(t)) => {
                let cause = self.place;
                self.ty.register(Obligation { goal: Goal::Projection { trait_: deref, args, name: target, term: t }, cause })?;
                Ok(Some(t))
            }
            // The operator `*`: `t: Deref` may hold, and the target is not decided yet. One
            // case is turso's `s.iter().map(|e| *e)` on a `SkipSet<?T>`: the impl's implied
            // `?T: Sized` is ambiguous, and the target is `?T` itself. The target is then a
            // fresh variable, and the projection is an obligation proved or refused with the
            // rest. rustc does the same: `lookup_method_for_operator` checks only
            // `predicate_may_hold`, and the method's output is normalized with its
            // obligations registered.
            None if at == DerefAt::Operator => {
                let u = self.fresh()?;
                let cause = self.place;
                self.ty.register(Obligation { goal: Goal::Projection { trait_: deref, args, name: target, term: u }, cause })?;
                Ok(Some(u))
            }
            None => Ok(None),
        }
    }

    /// Records the adjustments of an autoderef to step `n` of `steps` on `e`.
    pub(crate) fn adjust_autoderef(&mut self, e: ExprId, steps: &[Step], n: usize) {
        for s in steps.iter().take(n + 1).skip(1) {
            let kind = AdjustKind::Deref { overloaded: if s.overloaded { Some(Mutbl::Not) } else { None } };
            self.adjust.push((e, kind, s.ty));
        }
    }

    fn future_witness_gap(&self, reason: &'static str, at: At) -> CheckError {
        let at = if matches!(at, At::Body) { At::Expr(self.body.root) } else { at };
        CheckError::FutureWitness { reason, at: self.range_of(at) }
    }

    /// The gap of a goal left open by `witness`: another fn's superset record names that
    /// record (`CheckError::ForeignWitness`, rule 19); any other source is this body's own
    /// interior gap.
    fn witness_gap_of(&self, witness: Witness, at: At) -> CheckError {
        match witness {
            Witness::ForeignSuperset(item, ordinal) => {
                let at = if matches!(at, At::Body) { At::Expr(self.body.root) } else { at };
                CheckError::ForeignWitness { gap: crate::EXTERNAL_WITNESS_GAP, item, ordinal: Some(ordinal), at: self.range_of(at) }
            }
            Witness::Fixed | Witness::FailedCapture | Witness::LocalFlow | Witness::External => {
                self.future_witness_gap(crate::INTERIOR_GAP, at)
            }
        }
    }

    /// The range of an `At`.
    pub(crate) fn range_of(&self, at: At) -> crate::ids::Range {
        match at {
            At::Expr(e) => self.body.expr_range(e),
            At::Pat(p) => self.body.pat_range(p),
            At::Body => crate::ids::Range::default(),
            At::Written(r) => r,
        }
    }
}

/// The children of a node, in order.
pub(crate) fn children(node: PNode, types: PTys<'_>, out: &mut Vec<PTy, Alloc<'_>>) -> Result<(), CheckError> {
    types.children_of(node, out)
}

#[cfg(test)]
#[test]
fn normalization_follows_a_variable_bound_to_a_projection() {
    use crate::fixture::{Bb, Fx};
    use blaze_solve::Tys as _;
    use blaze_solve::term::Ty as SolverTy;
    use blaze_solve::tys::Relate;

    let alloc = Alloc::global();
    let mut world = Fx::new();
    let trait_ = world.tr("HasOutput", 1, std::vec![], &["Output"]);
    let boolean = world.ty(PNode::Bool);
    let byte = world.u8();
    world.imp("HasOutput for bool", 0, boolean, Some((trait_, &[])), &[("Output", byte)], std::vec![]);
    world.finish();

    let mut body = Bb::new();
    let root = body.bool();
    let mut cx = crate::InferCtx::new_in(alloc);
    let reads = Reads::new_in(&world, alloc);
    let mut m = Machine::new_in(body.view(ItemKey(42), root), reads, &mut cx, Plant::None, alloc).unwrap();
    let (boolean, byte) = (m.common.bool_, m.common.u8_);
    let var = m.fresh().unwrap();
    let elements = m.mk_list(&[boolean, var, var]).unwrap();
    let tuple = m.mk(TyView::Tuple(elements)).unwrap();
    assert!(!m.ty.ty_flags(tuple).has(TyFlags::HAS_ALIAS));

    // Applying a solver response structurally can bind the variable to the projection itself.
    let args = m.mk_list(&[boolean]).unwrap();
    let alias = m.mk(TyView::Alias { trait_, name: Name::of("Output"), args }).unwrap();
    let mut deferred = Vec::new_in(alloc);
    assert!(m.ty.terms.unify(SolverTy(var.0), SolverTy(alias.0), Relate::Structural, &mut deferred).unwrap());
    assert!(deferred.is_empty());
    assert_eq!(m.ty.shallow_resolve(var), alias);

    let elements = m.mk_list(&[boolean, byte, byte]).unwrap();
    let expected = m.mk(TyView::Tuple(elements)).unwrap();
    assert_eq!(m.try_structurally_resolve(tuple), Ok(expected));
}

#[cfg(test)]
#[test]
fn projection_equality_keeps_the_term_and_place() {
    use crate::fixture::{Bb, Fx};
    use blaze_solve::Tys as _;
    use blaze_solve::term::{Rigid, Ty as SolverTy, TyKind};

    let alloc = blaze_own::testing::heap().alloc();
    let mut world = Fx::new();
    let trait_ = world.tr("HasOutput", 1, std::vec![], &["Output"]);
    let boolean = world.ty(PNode::Bool);
    let receipt = world.strukt("Receipt", 0, &[]);
    // By path: a world trait in scope has an `adt(&self, ..)` that plain method lookup finds first.
    let output = Fx::adt(&mut world, receipt, &[]);
    world.imp("HasOutput for bool", 0, boolean, Some((trait_, &[])), &[("Output", output)], std::vec![]);
    world.finish();

    let mut body = Bb::new();
    let root = body.bool();
    let mut cx = crate::InferCtx::new_in(alloc);
    let reads = Reads::new_in(&world, alloc);
    let mut m = Machine::new_in(body.view(ItemKey(42), root), reads, &mut cx, Plant::None, alloc).unwrap();
    let name = Name::of("Output");
    let args = m.mk_list(&[m.common.bool_]).unwrap();
    let alias = m.mk(TyView::Alias { trait_, name, args }).unwrap();
    let TyKind::Alias(_, kind, def, list) = m.ty.terms.kind(SolverTy(alias.0)) else { panic!("expected a projection") };
    let rigid = Ty(m.ty.terms.mk(TyKind::Alias(Rigid::Yes, kind, def, list)).unwrap().0);
    let empty = m.mk_list(&[]).unwrap();
    let receipt = m.mk(TyView::Adt(receipt, empty)).unwrap();
    let cause = Cause::Expr(root);

    // Even a rigid projection instantiated at bool has the impl's concrete output.
    assert_eq!(m.unify_at(receipt, rigid, cause), Ok(true));
    m.select_all().unwrap();
    assert!(m.diags.is_empty(), "{:?}", m.diags);

    // Equality descends through both slices. Its one unmet goal names the element
    // projection and the required element type at the expression that related them.
    // `bool: HasOutput` holds and decides `Output` to `Receipt`, not `u8`: rustc's E0271,
    // "type mismatch resolving `<bool as HasOutput>::Output == u8`" (`report_projection_error`),
    // with the type the impl decided as found and the term as expected.
    let actual = m.mk(TyView::Slice(alias)).unwrap();
    let expected = m.mk(TyView::Slice(m.common.u8_)).unwrap();
    assert_eq!(m.unify_at(actual, expected, cause), Ok(true));
    m.select_all().unwrap();
    let [diag] = m.diags.as_slice() else { panic!("expected one projection error: {:?}", m.diags) };
    assert_eq!((diag.code, diag.at), (DiagCode::ProjectionMismatch, At::Expr(root)));
    assert_eq!((diag.found, diag.expected.map(|t| m.ty.shallow_resolve(t)), diag.name), (Some(receipt), Some(m.common.u8_), Some(name)));
    let Some((PendingGoal::Projection { trait_: tr, name: assoc, args: substs, term }, None)) = diag.unmet else {
        panic!("expected the failed projection: {diag:?}");
    };
    assert_eq!((tr, assoc, substs, m.ty.shallow_resolve(term)), (trait_, name, args, m.common.u8_));
}
