//! `#[loop_match]` and `#[const_continue]` (feature `loop_match`): rustc's checks of the two
//! attributes and the arm each `#[const_continue]` jumps to, over a body's expression store
//! and its inference rather than HIR and THIR.
//!
//! rustc checks in three places (MIT or Apache-2.0): the shape of the loop in THIR building
//! (`rustc_mir_build/src/thir/cx/expr.rs`, `ExprKind::Loop`), the label of a
//! `#[const_continue]` in type checking (`rustc_hir_typeck/src/loops.rs`, `is_loop_match`), and
//! the state's type, the arms' guards and the break's value in MIR building
//! (`builder/expr/into.rs`, `ExprKind::LoopMatch`, and `builder/scope.rs`,
//! `break_const_continuable_scope`). Each is ported here with its message, one walk of the body
//! per pass in rustc's order. The label check needs no types and is made in a body with a type
//! error too; the others are made only in a body without one, as rustc builds no THIR for it.
//! Every refusal but `NotEarly` is fatal in rustc, so the check keeps none after the first.
//!
//! The arm a `#[const_continue]` jumps to is decided here, once per body: the first arm whose
//! pattern accepts the break's value, as rustc's `static_pattern_match` decides it. MIR
//! lowering (`lower/loop_match.rs`) takes the decision from [`loop_match_check`] and never
//! makes its own, so a jump it lowers is one this check proved.

use std::cmp::Ordering;

use base_db::Db;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, HasModule, VariantId,
    expr_store::{Body, ExpressionStore, scope::ExprScopes},
    hir::{
        BindingId, ExprId, LabelId, Literal, MatchArm, Pat, PatId, RangeOp, Statement, UnaryOp,
    },
    resolver::{ValueNs, resolver_for_scope},
    signatures::VariantFields,
};
use hir_ty::{
    InferenceDiagnostic, InferenceResult, ParamEnvAndCrate,
    db::HirDatabase,
    method_resolution::CandidateId,
    mir::{IsSigned, pad16},
    next_solver::{GenericArgs, StoredTy, Ty, TyKind},
    utils::detect_variant_from_bytes,
};
use rustc_apfloat::{
    Float,
    ieee::{Double, Half, Quad, Single},
};
use rustc_type_ir::{FloatTy, TypeVisitableExt, inherent::IntoKind};

use crate::mir::Expr;

/// Why rustc cannot evaluate a `#[const_continue]`'s value early enough to pick an arm: the
/// label of its "could not determine the target branch" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotEarlyWhy {
    /// A const generic parameter.
    ConstParam,
    /// A `const { .. }` block.
    ConstBlock,
    /// Anything that is not a literal, a fieldless variant or a named constant.
    Other,
}

/// What rustc refuses of a `#[loop_match]` loop or a `#[const_continue]` break, and the nodes
/// its message is at. `N` is the node an expression is named by: an [`ExprId`] here, its
/// syntax in `hir`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoopMatchErrorKind<N = ExprId> {
    /// "expected a single assignment expression": the loop's body, or the one expression in
    /// it, is not `state = ..`.
    MissingAssignment { at: N },
    /// "statements are not allowed in this position within a `#[loop_match]`": from the first
    /// statement to the last, each its expression, or the block it is in when it is a `let` or
    /// an item.
    BadStatements { first: N, last: N },
    /// "this expression must be a single `match` wrapped in a labeled block".
    BadRhs { at: N },
    /// "invalid match on `#[loop_match]` state": the scrutinee is not a local of this body.
    InvalidMatch { scrutinee: N },
    /// "invalid update of the `#[loop_match]` state": the assignment's left side is not the
    /// scrutinee's local, or the assignment itself when that side is a pattern.
    InvalidUpdate { lhs: N, scrutinee: N },
    /// "this `#[loop_match]` state value has type `{ty}`, which is not supported".
    UnsupportedType { scrutinee: N, ty: StoredTy },
    /// "match arms that are part of a `#[loop_match]` cannot have guards".
    Guard { guard: N },
    /// "a `#[const_continue]` must break to a label with a value".
    MissingLabelOrValue { at: N },
    /// "`#[const_continue]` must break to a labeled block that participates in a
    /// `#[loop_match]`", at the break's label when `label`, else the break.
    BadLabel { at: N, label: bool },
    /// "could not determine the target branch for this `#[const_continue]`".
    NotEarly { value: N, why: NotEarlyWhy },
    /// The same error, labeled "this value is too generic": a constant with generic
    /// arguments.
    TooGeneric { value: N },
    /// "the target of this `#[const_continue]` is not statically known": no arm accepts the
    /// value.
    UnknownJumpTarget { at: N },
    /// blaze's own: a constant this check evaluates fails to, or a pattern of the loop is of a
    /// kind no fieldless state is matched with. `what` names it. rustc's const evaluation
    /// reports the constant's error; blaze names the jump it cannot decide.
    Unevaluated { at: N, what: &'static str },
}

impl<N> LoopMatchErrorKind<N> {
    /// The same refusal, each expression named by `f`'s node, or the first `Err` it gives.
    pub fn try_map<M, E>(
        self,
        mut f: impl FnMut(N) -> Result<M, E>,
    ) -> Result<LoopMatchErrorKind<M>, E> {
        use LoopMatchErrorKind::*;
        Ok(match self {
            MissingAssignment { at } => MissingAssignment { at: f(at)? },
            BadStatements { first, last } => BadStatements { first: f(first)?, last: f(last)? },
            BadRhs { at } => BadRhs { at: f(at)? },
            InvalidMatch { scrutinee } => InvalidMatch { scrutinee: f(scrutinee)? },
            InvalidUpdate { lhs, scrutinee } => {
                InvalidUpdate { lhs: f(lhs)?, scrutinee: f(scrutinee)? }
            }
            UnsupportedType { scrutinee, ty } => UnsupportedType { scrutinee: f(scrutinee)?, ty },
            Guard { guard } => Guard { guard: f(guard)? },
            MissingLabelOrValue { at } => MissingLabelOrValue { at: f(at)? },
            BadLabel { at, label } => BadLabel { at: f(at)?, label },
            NotEarly { value, why } => NotEarly { value: f(value)?, why },
            TooGeneric { value } => TooGeneric { value: f(value)? },
            UnknownJumpTarget { at } => UnknownJumpTarget { at: f(at)? },
            Unevaluated { at, what } => Unevaluated { at: f(at)?, what },
        })
    }

    /// Whether rustc stops compiling at this refusal: every one is `emit_fatal` but
    /// `ConstContinueNotMonomorphicConst` (`builder/scope.rs`), which is `emit_err` and lets
    /// MIR building go on to the next break.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, LoopMatchErrorKind::NotEarly { .. })
    }

    /// rustc's message, or blaze's for [`LoopMatchErrorKind::Unevaluated`].
    pub fn message(&self) -> &'static str {
        use LoopMatchErrorKind::*;
        match self {
            MissingAssignment { .. } => "expected a single assignment expression",
            BadStatements { .. } => {
                "statements are not allowed in this position within a `#[loop_match]`"
            }
            BadRhs { .. } => "this expression must be a single `match` wrapped in a labeled block",
            InvalidMatch { .. } => "invalid match on `#[loop_match]` state",
            InvalidUpdate { .. } => "invalid update of the `#[loop_match]` state",
            UnsupportedType { .. } => "this `#[loop_match]` state value has a type that is not supported",
            Guard { .. } => "match arms that are part of a `#[loop_match]` cannot have guards",
            MissingLabelOrValue { .. } => "a `#[const_continue]` must break to a label with a value",
            BadLabel { .. } => {
                "`#[const_continue]` must break to a labeled block that participates in a `#[loop_match]`"
            }
            NotEarly { .. } | TooGeneric { .. } => {
                "could not determine the target branch for this `#[const_continue]`"
            }
            UnknownJumpTarget { .. } => "the target of this `#[const_continue]` is not statically known",
            Unevaluated { .. } => "blaze could not decide the target arm of this `#[const_continue]`",
        }
    }
}

/// One refusal of [`loop_match_check`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoopMatchError {
    /// The innermost closure, coroutine or `async` block the loop or break is in, whose MIR
    /// is not built; `None` for the item's own body.
    pub body: Option<ExprId>,
    pub kind: LoopMatchErrorKind,
}

/// A `#[loop_match]` loop the check accepted: what MIR lowering needs to lower its jumps.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoopMatchLoop {
    pub loop_expr: ExprId,
    /// The `match` on the state, the labeled block's tail.
    pub match_expr: ExprId,
    /// The arms some `#[const_continue]` jumps to, by index, in increasing order.
    pub targets: Box<[u32]>,
}

/// A `#[const_continue]` the check accepted, and the arm it jumps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstContinueJump {
    pub break_expr: ExprId,
    pub loop_expr: ExprId,
    /// The index of the first arm of the loop's `match` whose pattern accepts the value.
    pub arm: u32,
}

/// What [`loop_match_check`] finds in a body and the closures in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LoopMatchCheck {
    errors: Box<[LoopMatchError]>,
    /// By `loop_expr`, for a binary search.
    loops: Box<[LoopMatchLoop]>,
    /// By `break_expr`, for a binary search.
    jumps: Box<[ConstContinueJump]>,
}

impl LoopMatchCheck {
    #[inline]
    pub fn errors(&self) -> &[LoopMatchError] {
        &self.errors
    }

    /// Whether rustc builds no MIR for `body` (a closure's expression, or `None` for the
    /// item's own): a `#[loop_match]` or `#[const_continue]` in it is refused.
    pub fn fails_body(&self, body: Option<ExprId>) -> bool {
        self.errors.iter().any(|it| it.body == body)
    }

    /// The accepted loop `loop_expr`, `None` for one the check refused or never saw.
    pub fn loop_of(&self, loop_expr: ExprId) -> Option<&LoopMatchLoop> {
        let at = self.loops.binary_search_by_key(&loop_expr, |it| it.loop_expr).ok()?;
        Some(&self.loops[at])
    }

    /// The jump of the accepted `#[const_continue]` `break_expr`.
    pub fn jump_of(&self, break_expr: ExprId) -> Option<ConstContinueJump> {
        let at = self.jumps.binary_search_by_key(&break_expr, |it| it.break_expr).ok()?;
        Some(self.jumps[at])
    }
}

/// rustc's checks of the `#[loop_match]` loops and `#[const_continue]` breaks of `def` and
/// the closures in it, and the arm each accepted break jumps to.
#[blaze_query::tracked(returns(ref))]
pub fn loop_match_check(db: &Db, def: DefWithBodyId) -> LoopMatchCheck {
    let body = Body::of(db, def);
    if !body.has_loop_match() {
        return LoopMatchCheck::default();
    }
    let infer = InferenceResult::of(db, def);
    let mut checker = Checker {
        db,
        def,
        infer,
        store: body,
        // alloc: only at the first refusal, so none for a body without one.
        errors: Vec::new(),
        // alloc: one entry per `#[loop_match]` loop.
        loops: Vec::new(),
        // alloc: one entry per `#[const_continue]`.
        jumps: Vec::new(),
    };
    // Type checking's `loops::check` runs before inference, so its label check is made in a
    // body with a type error too. Its refusal is fatal: rustc stops there.
    checker.walk(body.root_expr(), Phase::Labels);
    if !checker.errors.is_empty() {
        return checker.finish();
    }
    // rustc builds no THIR for a body whose type check is tainted, and none of the checks
    // after it runs; inference reports that body's errors.
    if !builds_thir(infer) {
        return LoopMatchCheck::default();
    }
    // THIR building's checks, all fatal, over the whole body before any of MIR building's.
    checker.walk(body.root_expr(), Phase::Thir);
    if !checker.errors.is_empty() {
        return checker.finish();
    }
    checker.walk(body.root_expr(), Phase::Mir);
    checker.finish()
}

/// Whether rustc builds THIR for the body: its type check is not tainted. E0695 is
/// `loops::check`'s, emitted before the inference context exists, so it does not taint the
/// body in rustc (nightly-2026-10-27 goes on to THIR's `#[const_continue]` error), while blaze's
/// inference counts it in `has_errors`. A body whose only tainting diagnostics are E0695 is
/// read as untainted.
///
/// Limit: `has_errors` also holds the resolver's and the trait solver's errors, which leave
/// no diagnostic here; one of those beside an E0695 is read as untainted too.
fn builds_thir(infer: &InferenceResult<'_>) -> bool {
    if infer.has_type_mismatches() || infer.is_erroneous() {
        return false;
    }
    if !infer.has_errors {
        return true;
    }
    let mut e0695 = false;
    for diagnostic in infer.diagnostics() {
        match diagnostic {
            InferenceDiagnostic::UnlabeledJumpInLabeledBlock { .. } => e0695 = true,
            other if other.taints_body() => return false,
            _ => {}
        }
    }
    e0695
}

/// Which of rustc's passes a walk of the body makes the checks of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `rustc_hir_typeck/src/loops.rs`: what a `#[const_continue]` breaks to. Needs no types.
    Labels,
    /// `rustc_mir_build/src/thir/cx/expr.rs`: the loop's shape and the break's value.
    Thir,
    /// `rustc_mir_build/src/builder`: the state's type, guards, and each jump's arm.
    Mir,
}

impl Checker<'_> {
    /// The check's result: the errors up to the first fatal one, where rustc stops, and the
    /// loops and jumps of a body with none.
    fn finish(self) -> LoopMatchCheck {
        let Checker { mut errors, loops, mut jumps, .. } = self;
        if let Some(fatal) = errors.iter().position(|it| it.kind.is_fatal()) {
            errors.truncate(fatal + 1);
        }
        if !errors.is_empty() {
            // A refused body is not lowered: nothing of it is read but its errors.
            return LoopMatchCheck {
                errors: errors.into_boxed_slice(),
                loops: Box::default(),
                jumps: Box::default(),
            };
        }
        let mut accepted: Vec<LoopMatchLoop> = (loops.into_iter())
            .filter_map(|it| it.accepted)
            .map(|it| {
                // alloc: the targets of one loop, once per loop
                let mut targets: Vec<u32> = (jumps.iter())
                    .filter(|jump| jump.loop_expr == it.loop_expr)
                    .map(|jump| jump.arm)
                    .collect();
                targets.sort_unstable();
                targets.dedup();
                LoopMatchLoop {
                    loop_expr: it.loop_expr,
                    match_expr: it.match_expr,
                    targets: targets.into_boxed_slice(),
                }
            })
            // alloc: the accepted loops, once per body with a `#[loop_match]`
            .collect();
        accepted.sort_unstable_by_key(|it| it.loop_expr);
        jumps.sort_unstable_by_key(|it| it.break_expr);
        LoopMatchCheck {
            errors: Box::default(),
            loops: accepted.into_boxed_slice(),
            jumps: jumps.into_boxed_slice(),
        }
    }
}

/// What a fieldless state is: the types rustc's `is_supported_loop_match_type` accepts.
#[derive(Debug, Clone, Copy)]
enum StateKind {
    Int,
    Uint,
    Float(FloatTy),
    Bool,
    Char,
    Enum(EnumId),
}

/// A value of the state, as a pattern compares it.
#[derive(Debug, Clone, Copy)]
enum Value {
    Int(i128),
    Uint(u128),
    Bool(bool),
    Char(u32),
    Half(Half),
    Single(Single),
    Double(Double),
    Quad(Quad),
    Variant(EnumVariantId),
}

impl Value {
    /// How `self` compares with `other`, `None` when they are of no one order: two variants
    /// are only equal or not.
    fn compare(self, other: Value) -> Option<Ordering> {
        use Value::*;
        match (self, other) {
            (Int(a), Int(b)) => Some(a.cmp(&b)),
            (Uint(a), Uint(b)) => Some(a.cmp(&b)),
            (Bool(a), Bool(b)) => Some(a.cmp(&b)),
            (Char(a), Char(b)) => Some(a.cmp(&b)),
            (Half(a), Half(b)) => a.partial_cmp(&b),
            (Single(a), Single(b)) => a.partial_cmp(&b),
            (Double(a), Double(b)) => a.partial_cmp(&b),
            (Quad(a), Quad(b)) => a.partial_cmp(&b),
            (Variant(a), Variant(b)) if a == b => Some(Ordering::Equal),
            _ => None,
        }
    }

    fn negate(self) -> Option<Value> {
        use Value::*;
        Some(match self {
            Int(it) => Int(it.checked_neg()?),
            Half(it) => Half(-it),
            Single(it) => Single(-it),
            Double(it) => Double(-it),
            Quad(it) => Quad(-it),
            Uint(_) | Bool(_) | Char(_) | Variant(_) => return None,
        })
    }
}

/// A `#[loop_match]` loop whose body is `state = 'blk: { .. }`, the shape rustc's
/// `is_loop_match` names the labeled block of, and what each phase found of it.
struct LoopEntry {
    loop_expr: ExprId,
    /// The labeled block's label; a `#[const_continue]` to it is to this loop. `None` for an
    /// unlabeled block, which no break can name.
    label: Option<LabelId>,
    /// Set once THIR's checks of the loop held.
    shape: Option<Shape>,
    /// Set once MIR building's checks of the loop held.
    state: Option<State>,
    accepted: Option<Accepted>,
}

/// What THIR's checks find the loop's body is.
#[derive(Clone, Copy)]
struct Shape {
    match_expr: ExprId,
    scrutinee: ExprId,
}

/// What MIR building's checks give.
#[derive(Clone, Copy)]
struct State {
    match_expr: ExprId,
    kind: StateKind,
}

struct Accepted {
    loop_expr: ExprId,
    match_expr: ExprId,
}

struct Checker<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    infer: &'db InferenceResult<'db>,
    store: &'db ExpressionStore,
    errors: Vec<LoopMatchError>,
    loops: Vec<LoopEntry>,
    jumps: Vec<ConstContinueJump>,
}

impl<'db> Checker<'db> {
    /// The pre-order rustc's passes visit in: a loop before the breaks in it. `body` is the
    /// closure the walk is in.
    fn walk(&mut self, root: ExprId, phase: Phase) {
        // alloc: the walk's stack, once per body with a `#[loop_match]` and phase
        let mut stack = vec![(root, None)];
        // alloc: the children of one expression, reused
        let mut children = Vec::new();
        while let Some((expr, body)) = stack.pop() {
            let inner = match self.store.expr(expr) {
                Expr::Loop { body: loop_body, .. } if self.store.is_loop_match(expr) => {
                    match phase {
                        Phase::Labels => self.register_loop(expr, loop_body),
                        Phase::Thir => self.check_shape(expr, loop_body, body),
                        Phase::Mir => self.check_state(expr, body),
                    }
                    body
                }
                Expr::Break { expr: value, label } if self.store.is_const_continue(expr) => {
                    match phase {
                        Phase::Labels => self.check_label(expr, label, body),
                        Phase::Thir => self.check_value_given(expr, label, value, body),
                        Phase::Mir => self.check_jump(expr, value, label, body),
                    }
                    body
                }
                Expr::Closure { .. } => Some(expr),
                _ => body,
            };
            children.clear();
            self.store.walk_child_exprs(expr, |child| children.push(child));
            stack.extend(children.iter().rev().map(|&child| (child, inner)));
        }
    }

    fn refuse(&mut self, body: Option<ExprId>, kind: LoopMatchErrorKind) {
        self.errors.push(LoopMatchError { body, kind });
    }

    /// The node a statement is reported at: its expression, or the block it is in.
    fn statement_node(statement: Statement<'_>, block: ExprId) -> ExprId {
        match statement {
            Statement::Expr { expr, .. } => expr,
            Statement::Let { .. } | Statement::Item(_) => block,
        }
    }

    /// rustc's `is_loop_match` (`rustc_hir_typeck/src/loops.rs`): the labeled block a loop
    /// whose body is `state = 'blk: { .. }` or `state = 'blk: { .. };` names, whatever is in
    /// the block. It checks nothing; THIR building refuses what else is wrong.
    fn register_loop(&mut self, loop_expr: ExprId, loop_body: ExprId) {
        let Expr::Block { statements, tail, .. } = self.store.expr(loop_body) else { return };
        let assignment = match (statements.len(), tail) {
            (0, Some(tail)) => tail,
            (1, None) => match statements.first() {
                Some(Statement::Expr { expr, .. }) => expr,
                _ => return,
            },
            _ => return,
        };
        let Expr::Assignment { value: rhs, .. } = self.store.expr(assignment) else { return };
        let Expr::Block { label, .. } = self.store.expr(rhs) else { return };
        self.loops.push(LoopEntry { loop_expr, label, shape: None, state: None, accepted: None });
    }

    fn entry_of_loop(&self, loop_expr: ExprId) -> Option<usize> {
        self.loops.iter().position(|it| it.loop_expr == loop_expr)
    }

    /// MIR building's checks of a loop whose shape held: the state's type, then the arms'
    /// guards.
    fn check_state(&mut self, loop_expr: ExprId, body: Option<ExprId>) {
        use LoopMatchErrorKind::*;
        let Some(entry) = self.entry_of_loop(loop_expr) else { return };
        let Some(Shape { match_expr, scrutinee }) = self.loops[entry].shape else {
            return;
        };
        let ty = self.infer.expr_ty(scrutinee);
        let Some(kind) = self.state_kind(ty) else {
            return self.refuse(body, UnsupportedType { scrutinee, ty: ty.store() });
        };
        let Expr::Match { arms, .. } = self.store.expr(match_expr) else { return };
        if let Some(guard) = arms.iter().find_map(|arm| arm.guard) {
            return self.refuse(body, Guard { guard });
        }
        let loop_entry = &mut self.loops[entry];
        loop_entry.state = Some(State { match_expr, kind });
        loop_entry.accepted = Some(Accepted { loop_expr, match_expr });
    }

    /// rustc's THIR shape checks of `loop_expr` (`thir/cx/expr.rs`, `ExprKind::Loop`).
    fn check_shape(&mut self, loop_expr: ExprId, loop_body: ExprId, body: Option<ExprId>) {
        use LoopMatchErrorKind::*;
        // `state = rhs` or `state = rhs;`, and nothing else.
        let Expr::Block { statements, tail, .. } = self.store.expr(loop_body) else {
            return self.refuse(body, MissingAssignment { at: loop_body });
        };
        let assignment = match (statements.len(), tail) {
            (0, Some(tail)) => tail,
            (0, None) => return self.refuse(body, MissingAssignment { at: loop_body }),
            (1, None) => match statements.first() {
                Some(Statement::Expr { expr, .. }) => expr,
                _ => return self.refuse(body, MissingAssignment { at: loop_body }),
            },
            _ => {
                let first = statements.first().map_or(loop_body, |it| Self::statement_node(it, loop_body));
                let last = match tail {
                    Some(tail) => tail,
                    None => statements.last().map_or(loop_body, |it| Self::statement_node(it, loop_body)),
                };
                // rustc's span runs from the first statement to the last; a tail is no
                // statement, so with one statement and a tail the span is that statement's.
                let last = if tail.is_some() && statements.len() == 1 { first } else { last };
                return self.refuse(body, BadStatements { first, last });
            }
        };
        let Expr::Assignment { target, value: rhs } = self.store.expr(assignment) else {
            return self.refuse(body, MissingAssignment { at: assignment });
        };
        let Expr::Block { statements, tail, .. } = self.store.expr(rhs) else {
            return self.refuse(body, BadRhs { at: rhs });
        };
        // The block may define items, and nothing else, before its `match`.
        if let Some(statement) = statements.iter().find(|it| !matches!(it, Statement::Item(_))) {
            let at = Self::statement_node(statement, rhs);
            return self.refuse(body, BadStatements { first: at, last: at });
        }
        let Some(tail) = tail else { return self.refuse(body, BadRhs { at: rhs }) };
        let Expr::Match { expr: scrutinee, .. } = self.store.expr(tail) else {
            return self.refuse(body, BadRhs { at: tail });
        };
        let Some(state) = self.local_of(scrutinee, body) else {
            return self.refuse(body, InvalidMatch { scrutinee });
        };
        let lhs = match self.store.pat(target) {
            Pat::Expr(lhs) => lhs,
            _ => assignment,
        };
        if lhs == assignment || self.local_of(lhs, body) != Some(state) {
            return self.refuse(body, InvalidUpdate { lhs, scrutinee });
        }
        // Every shape that gets here is one `register_loop` named.
        let Some(entry) = self.entry_of_loop(loop_expr) else { return };
        self.loops[entry].shape = Some(Shape { match_expr: tail, scrutinee });
    }

    /// The local `expr` names, when it is a path to a binding of `body` itself: rustc's
    /// `local`, which refuses an upvar.
    fn local_of(&self, expr: ExprId, body: Option<ExprId>) -> Option<BindingId> {
        let Expr::Path(path) = self.store.expr(expr) else { return None };
        let scope = ExprScopes::of(self.db, self.def).scope_for(expr);
        let resolver = resolver_for_scope(self.db, self.def, scope);
        let hygiene = self.store.expr_path_hygiene(expr);
        match resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene)? {
            ValueNs::LocalBinding(binding) if self.store.binding_owner(binding) == body => {
                Some(binding)
            }
            _ => None,
        }
    }

    /// rustc's `is_supported_loop_match_type`: integers, floats, `bool`, `char`, and enums
    /// whose variants have no fields.
    fn state_kind(&self, ty: Ty<'db>) -> Option<StateKind> {
        Some(match ty.kind() {
            TyKind::Int(_) => StateKind::Int,
            TyKind::Uint(_) => StateKind::Uint,
            TyKind::Float(float) => StateKind::Float(float),
            TyKind::Bool => StateKind::Bool,
            TyKind::Char => StateKind::Char,
            TyKind::Adt(adt, _) => {
                let AdtId::EnumId(id) = adt.def_id() else { return None };
                let fieldless = (id.enum_variants(self.db).variants.values()).all(|&(variant, _)| {
                    VariantFields::of(self.db, variant.into()).fields().is_empty()
                });
                if !fieldless {
                    return None;
                }
                StateKind::Enum(id)
            }
            _ => return None,
        })
    }

    /// The loop whose labeled block is `label`.
    fn entry_of_label(&self, label: LabelId) -> Option<usize> {
        self.loops.iter().position(|it| it.label == Some(label))
    }

    /// Type checking's `BadLabel` (`rustc_hir_typeck/src/loops.rs`): a `#[const_continue]`
    /// breaks to the labeled block of a `#[loop_match]` loop.
    fn check_label(&mut self, break_expr: ExprId, label: Option<LabelId>, body: Option<ExprId>) {
        use LoopMatchErrorKind::*;
        let Some(label) = label else {
            // rustc's `require_label_in_labeled_block` comes first in the same visit: an
            // unlabeled break whose innermost breakable is a labeled block gets E0695, which is
            // not fatal, and this check is skipped. THIR building then refuses the break
            // (`check_value_given`); nightly-2026-10-27 prints both. Inference reports that
            // E0695 here.
            let e0695 = self.infer.diagnostics().iter().any(|it| {
                matches!(it, InferenceDiagnostic::UnlabeledJumpInLabeledBlock { expr, .. } if *expr == break_expr)
            });
            if !e0695 {
                self.refuse(body, BadLabel { at: break_expr, label: false });
            }
            return;
        };
        if self.entry_of_label(label).is_none() {
            self.refuse(body, BadLabel { at: break_expr, label: true });
        }
    }

    /// THIR building's `MissingLabelOrValue` (`thir/cx/expr.rs`): a `#[const_continue]` has a
    /// label and a value. rustc makes it only in a body whose type check held, so a break
    /// with no value in a block typed other than `()` gets inference's mismatch alone. An
    /// unlabeled one gets here only with its E0695 (`check_label`), which taints nothing.
    fn check_value_given(
        &mut self,
        break_expr: ExprId,
        label: Option<LabelId>,
        value: Option<ExprId>,
        body: Option<ExprId>,
    ) {
        if label.is_none() || value.is_none() {
            self.refuse(body, LoopMatchErrorKind::MissingLabelOrValue { at: break_expr });
        }
    }

    /// MIR building's `break_const_continuable_scope` (`builder/scope.rs`): the value is
    /// known early, and an arm accepts it.
    fn check_jump(
        &mut self,
        break_expr: ExprId,
        value: Option<ExprId>,
        label: Option<LabelId>,
        body: Option<ExprId>,
    ) {
        use LoopMatchErrorKind::*;
        // The Labels and Thir phases refused a break with neither, and a refusal there stops
        // the check before this phase.
        let (Some(label), Some(value)) = (label, value) else { return };
        let Some(entry) = self.entry_of_label(label) else { return };
        // A loop refused earlier is not lowered, so rustc never reaches its breaks' values.
        let (loop_expr, Some(state)) = (self.loops[entry].loop_expr, self.loops[entry].state)
        else {
            return;
        };
        let value = match self.value_of(value, state.kind) {
            Ok(it) => it,
            Err(kind) => return self.refuse(body, kind),
        };
        let Expr::Match { arms, .. } = self.store.expr(state.match_expr) else {
            unreachable!("a loop's state comes from its `match`");
        };
        match self.pick(arms, state, value, break_expr) {
            Ok(Some(arm)) => {
                let arm = u32::try_from(arm).expect("a `match` has fewer than 2^32 arms");
                self.jumps.push(ConstContinueJump { break_expr, loop_expr, arm });
            }
            Ok(None) => self.refuse(body, UnknownJumpTarget { at: break_expr }),
            Err(kind) => self.refuse(body, kind),
        }
    }

    /// rustc's `static_pattern_match`: the first arm whose pattern accepts `value`.
    fn pick(
        &self,
        arms: &[MatchArm],
        state: State,
        value: Value,
        break_expr: ExprId,
    ) -> Result<Option<usize>, LoopMatchErrorKind> {
        for (index, arm) in arms.iter().enumerate() {
            if self.accepts(arm.pat, state, value, break_expr)? {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    fn accepts(
        &self,
        pat: PatId,
        state: State,
        value: Value,
        break_expr: ExprId,
    ) -> Result<bool, LoopMatchErrorKind> {
        let unevaluated = |what| LoopMatchErrorKind::Unevaluated { at: break_expr, what };
        let equal = |bound: Value| bound.compare(value) == Some(Ordering::Equal);
        match self.store.pat(pat) {
            Pat::Wild | Pat::Bind { subpat: None, .. } => Ok(true),
            Pat::Bind { subpat: Some(subpat), .. } => self.accepts(subpat, state, value, break_expr),
            Pat::Or(alternatives) => {
                for &alternative in alternatives {
                    if self.accepts(alternative, state, value, break_expr)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Pat::Lit(expr) => Ok(equal(self.pattern_value(expr, state, break_expr)?)),
            Pat::Range { start, end, range_type } => {
                if let Some(start) = start {
                    let start = self.pattern_value(start, state, break_expr)?;
                    match value.compare(start) {
                        Some(Ordering::Greater | Ordering::Equal) => {}
                        Some(Ordering::Less) => return Ok(false),
                        None => return Err(unevaluated("a range bound of another kind than the state")),
                    }
                }
                if let Some(end) = end {
                    let end = self.pattern_value(end, state, break_expr)?;
                    let inside = match (value.compare(end), range_type) {
                        (Some(Ordering::Less), _) => true,
                        (Some(Ordering::Equal), RangeOp::Inclusive) => true,
                        (Some(_), _) => false,
                        (None, _) => {
                            return Err(unevaluated("a range bound of another kind than the state"));
                        }
                    };
                    return Ok(inside);
                }
                Ok(true)
            }
            Pat::Path(path) => {
                if let Some(VariantId::EnumVariantId(variant)) =
                    self.infer.variant_resolution_for_pat(pat)
                {
                    return Ok(equal(Value::Variant(variant)));
                }
                let (konst, args) = match self.infer.assoc_resolutions_for_pat(pat) {
                    Some((CandidateId::ConstId(konst), args)) => (konst, args),
                    _ => {
                        let scope = ExprScopes::of(self.db, self.def).scope_for(state.match_expr);
                        let resolver = resolver_for_scope(self.db, self.def, scope);
                        let hygiene = self.store.pat_path_hygiene(pat);
                        match resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene) {
                            Some(ValueNs::ConstId(konst)) => (konst, GenericArgs::empty()),
                            Some(ValueNs::EnumVariantId(variant)) => {
                                return Ok(equal(Value::Variant(variant)));
                            }
                            _ => return Err(unevaluated("a path pattern that is no constant or variant")),
                        }
                    }
                };
                Ok(equal(self.const_value(konst, args, state.kind, break_expr)?))
            }
            Pat::TupleStruct { args: [], .. } | Pat::Record { args: [], .. } => {
                match self.infer.variant_resolution_for_pat(pat) {
                    Some(VariantId::EnumVariantId(variant)) => Ok(equal(Value::Variant(variant))),
                    _ => Err(unevaluated("a struct pattern of no variant")),
                }
            }
            _ => Err(unevaluated("a pattern no fieldless state is matched with")),
        }
    }

    /// The value of a literal pattern or a range bound: a literal, or a path to a constant.
    fn pattern_value(
        &self,
        expr: ExprId,
        state: State,
        break_expr: ExprId,
    ) -> Result<Value, LoopMatchErrorKind> {
        let unevaluated = |what| LoopMatchErrorKind::Unevaluated { at: break_expr, what };
        match self.store.expr(expr) {
            Expr::Literal(literal) => literal_value(literal, state.kind)
                .ok_or_else(|| unevaluated("a literal of another type than the state")),
            Expr::UnaryOp { expr: inner, op: UnaryOp::Neg } => {
                let Expr::Literal(literal) = self.store.expr(inner) else {
                    return Err(unevaluated("a negated pattern that is no literal"));
                };
                (literal_value(literal, state.kind).and_then(Value::negate))
                    .ok_or_else(|| unevaluated("a negated literal of another type than the state"))
            }
            Expr::Path(_) => match self.named_const(expr) {
                Ok((konst, args)) => self.const_value(konst, args, state.kind, break_expr),
                Err(_) => Err(unevaluated("a pattern bound that is no literal or constant")),
            },
            _ => Err(unevaluated("a pattern bound that is no literal or constant")),
        }
    }

    /// The `#[const_continue]`'s value: THIR's `Literal`, `NonHirLiteral`, `ZstLiteral`,
    /// `Adt` (a fieldless variant) or `NamedConst`, evaluated; anything else is refused with
    /// what rustc says of it.
    fn value_of(&self, value: ExprId, kind: StateKind) -> Result<Value, LoopMatchErrorKind> {
        use LoopMatchErrorKind::*;
        let not_early = |why| NotEarly { value, why };
        match self.store.expr(value) {
            Expr::Literal(literal) => literal_value(literal, kind)
                .ok_or(Unevaluated { at: value, what: "a literal of another type than the state" }),
            // THIR folds the sign into the literal (`ExprKind::Literal { neg: true }`).
            Expr::UnaryOp { expr: inner, op: UnaryOp::Neg } => {
                let Expr::Literal(literal) = self.store.expr(inner) else {
                    return Err(not_early(NotEarlyWhy::Other));
                };
                (literal_value(literal, kind).and_then(Value::negate)).ok_or(Unevaluated {
                    at: value,
                    what: "a negated literal of another type than the state",
                })
            }
            Expr::Path(_) => {
                if let Some(VariantId::EnumVariantId(variant)) =
                    self.infer.variant_resolution_for_expr(value)
                {
                    return Ok(Value::Variant(variant));
                }
                let (konst, args) = self.named_const(value).map_err(not_early)?;
                self.const_value(konst, args, kind, value)
            }
            // `E::A {}` and `E::A()` of a variant with no fields are THIR's `Adt` too.
            Expr::RecordLit { fields: [], .. } | Expr::Call { args: [], .. } => {
                match self.variant_of_constructor(value) {
                    Some(variant) => Ok(Value::Variant(variant)),
                    None => Err(not_early(NotEarlyWhy::Other)),
                }
            }
            Expr::Const(_) => Err(not_early(NotEarlyWhy::ConstBlock)),
            _ => Err(not_early(NotEarlyWhy::Other)),
        }
    }

    /// The fieldless variant `E::A {}` or `E::A()` builds.
    fn variant_of_constructor(&self, expr: ExprId) -> Option<EnumVariantId> {
        match self.store.expr(expr) {
            Expr::RecordLit { .. } => match self.infer.variant_resolution_for_expr(expr)? {
                VariantId::EnumVariantId(variant) => Some(variant),
                _ => None,
            },
            Expr::Call { callee, .. } => match self.infer.expr_ty(callee).kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    hir_def::CallableDefId::EnumVariantId(variant) => Some(variant),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// The constant a path expression names, with its generic arguments: rustc's
    /// `NamedConst`, or why it is none.
    fn named_const(&self, expr: ExprId) -> Result<(ConstId, GenericArgs<'db>), NotEarlyWhy> {
        if let Some((CandidateId::ConstId(konst), args)) = self.infer.assoc_resolutions_for_expr(expr)
        {
            return Ok((konst, args));
        }
        let Expr::Path(path) = self.store.expr(expr) else { return Err(NotEarlyWhy::Other) };
        let scope = ExprScopes::of(self.db, self.def).scope_for(expr);
        let resolver = resolver_for_scope(self.db, self.def, scope);
        let hygiene = self.store.expr_path_hygiene(expr);
        match resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene) {
            Some(ValueNs::ConstId(konst)) => Ok((konst, GenericArgs::empty())),
            Some(ValueNs::GenericParam(_)) => Err(NotEarlyWhy::ConstParam),
            _ => Err(NotEarlyWhy::Other),
        }
    }

    /// The value of constant `konst`, evaluated as the state's type: rustc's
    /// `const_eval_resolve` of a `NamedConst`, refused as too generic with generic arguments.
    fn const_value(
        &self,
        konst: ConstId,
        args: GenericArgs<'db>,
        kind: StateKind,
        at: ExprId,
    ) -> Result<Value, LoopMatchErrorKind> {
        use LoopMatchErrorKind::*;
        if args.has_non_region_param() {
            return Err(TooGeneric { value: at });
        }
        let krate = self.def.krate(self.db);
        let env = ParamEnvAndCrate {
            param_env: self.db.trait_environment(self.def.generic_def(self.db)),
            krate,
        };
        let unevaluated = |what| Unevaluated { at, what };
        let allocation = (self.db.const_eval(konst, args, Some(env)))
            .map_err(|_| unevaluated("a constant whose evaluation fails"))?;
        let bytes = &allocation.memory[..];
        Ok(match kind {
            StateKind::Int => Value::Int(i128::from_le_bytes(pad16(bytes, IsSigned::Yes))),
            StateKind::Uint => Value::Uint(u128::from_le_bytes(pad16(bytes, IsSigned::No))),
            StateKind::Bool => Value::Bool(bytes.first().is_some_and(|&it| it != 0)),
            StateKind::Char => {
                let bits = u128::from_le_bytes(pad16(bytes, IsSigned::No));
                Value::Char(u32::try_from(bits).map_err(|_| unevaluated("a `char` out of range"))?)
            }
            StateKind::Float(float) => {
                let bits = u128::from_le_bytes(pad16(bytes, IsSigned::No));
                match float {
                    FloatTy::F16 => Value::Half(Half::from_bits(bits)),
                    FloatTy::F32 => Value::Single(Single::from_bits(bits)),
                    FloatTy::F64 => Value::Double(Double::from_bits(bits)),
                    FloatTy::F128 => Value::Quad(Quad::from_bits(bits)),
                }
            }
            StateKind::Enum(id) => {
                let ty = allocation.ty;
                let layout = (self.db.layout_of_ty(ty.store(), env.store()))
                    .map_err(|_| unevaluated("an enum constant whose layout is unknown"))?;
                let target = (self.db.target_data_layout(krate))
                    .map_err(|_| unevaluated("an enum constant of a target with no layout"))?;
                let (variant, _) = detect_variant_from_bytes(&layout, self.db, target, bytes, id)
                    .ok_or_else(|| unevaluated("an enum constant of no variant"))?;
                Value::Variant(variant)
            }
        })
    }
}

/// A literal as a value of a state of `kind`, `None` when it is of another type.
fn literal_value(literal: &Literal, kind: StateKind) -> Option<Value> {
    Some(match (kind, literal) {
        (StateKind::Int, Literal::Int(it, _)) => Value::Int(*it),
        (StateKind::Int, Literal::Uint(it, _)) => Value::Int(i128::try_from(*it).ok()?),
        (StateKind::Uint, Literal::Uint(it, _)) => Value::Uint(*it),
        (StateKind::Uint, Literal::Int(it, _)) => Value::Uint(u128::try_from(*it).ok()?),
        (StateKind::Bool, Literal::Bool(it)) => Value::Bool(*it),
        (StateKind::Char, Literal::Char(it)) => Value::Char(u32::from(*it)),
        // Each width parses the literal's text, as rustc's does, never through another
        // width: `0.1f32` is the `f32` nearest to a tenth.
        (StateKind::Float(float), Literal::Float(it, _)) => match float {
            FloatTy::F16 => Value::Half(it.to_f16()),
            FloatTy::F32 => Value::Single(it.to_f32()),
            FloatTy::F64 => Value::Double(it.to_f64()),
            FloatTy::F128 => Value::Quad(it.to_f128()),
        },
        _ => return None,
    })
}
