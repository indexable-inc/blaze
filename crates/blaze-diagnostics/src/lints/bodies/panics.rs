//! `arithmetic_overflow` and `unconditional_panic`: what rustc's known-panics lint finds, by
//! propagating the constants of a body's MIR after drop elaboration: an `Assert` it knows
//! fails, an arithmetic operation it knows overflows, a shift by as many bits as its left side
//! has or more, and a call with a `#[rustc_panics_when_zero]` const parameter of 0.
//!
//! Follows `compiler/rustc_mir_transform/src/known_panics_lint.rs` (`KnownPanicsLint::run_lint`:
//! the bodies it runs on; `ConstPropagator`: `check_unary_op`, `check_binary_op`,
//! `check_rvalue`, `check_assertion`, `eval_rvalue`, `visit_assign`, `visit_statement`,
//! `visit_terminator`, `visit_basic_block_data`; `Value::project`; `CanConstProp`),
//! `compiler/rustc_mir_transform/src/diagnostics.rs` (`AssertLint`, `ConstNIsZero`),
//! `compiler/rustc_middle/src/mir/terminator.rs` (`AssertKind`'s `Display`) and
//! `compiler/rustc_mir_transform/src/lib.rs` (the lint runs after `ElaborateDrops` and
//! `CleanupPostBorrowck`, which removes fake borrows) (MIT or Apache-2.0).
//!
//! rustc's interpreter gives the values; here [`scalar`] does, for what the lint can know: a
//! value of a primitive type, and of an enum whose layout is a scalar or a pair, the variant.
//! A constant is evaluated by const evaluation (`const_eval`, `anon_const_eval`). What is not
//! known here that rustc knows: `line!()` and `column!()`, whose values MIR leaves for the
//! code generator to place, and `f16` and `f128` arithmetic.
//!
//! Cost: one pass over a body's statements decides whether it has anything to check (an
//! operation, an `Assert`, a call with const arguments); most bodies stop there. A body that
//! has is walked once per reachable block. Values are `Copy` [`Value`]s of at most 48 bytes:
//! a scalar inline, an enum constant as its 4-byte interned allocation (read in place), an
//! aggregate as a `u32` into the body's append-only arena of fields. Visited blocks and
//! block-local writes are bitsets. Every buffer is reused across the bodies of an owner, so a
//! body allocates nothing once the first has sized them, and a body with no finding
//! allocates no output.

mod scalar;

use std::fmt;

use base_db::Db;
use blaze_ids::bits::BitSet;
use either::Either;
use hir::InFile;
use hir_def::{
    AdtId, CallableDefId, ConstParamId, DefWithBodyId, EnumVariantId, GenericDefId, GenericParamId, ItemContainerId,
    Lookup, VariantId, hir::ExprId, layout::BackendRepr, src::HasChildSourceView as _,
};
use hir_expand::{MacroDefKind, builtin::BuiltinFnLikeExpander};
use hir_ty::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosureId},
    method_resolution::lookup_impl_const,
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlockId, BinOp, BorrowKind, CastKind, LocalId, MirBody, MirSpan,
        Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace, Terminator, TerminatorKind,
        UnOp,
    },
    next_solver::{
        AliasConst, AliasConstKind, Allocation, Const, ConstKind, DbInterner, FreeConstAliasId, GenericArgKind,
        InherentAssocConstId, ParamEnv, TraitAssocConstId, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    utils::detect_variant_from_bytes,
};
use hir_ty_borrowck::NotChecked;
use rustc_type_ir::{
    TypeVisitableExt as _, UintTy,
    inherent::{IntoKind as _, SliceLike as _},
};
use smallvec::SmallVec;
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use self::scalar::{Kind, Output, Scalar};
use super::BodyCx;
use crate::{
    cx::def_range_view,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet = super::lint_set(["arithmetic_overflow", "unconditional_panic"]);

/// The known-panics lint of the body and of the closures in it, each closure after the body
/// it is in, in the order they are written. None of a body rustc builds no runtime MIR of
/// before it aborts, or whose type checking or borrow checking failed.
pub(super) fn lints<'db>(b: &BodyCx<'_, '_, 'db>) -> Vec<Found> {
    if b.mir_tainted() || !b.elaborated || borrowck_failed(b) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let db = b.db();
    // alloc: the lints found, empty until one is
    let mut out = Vec::new();
    let mut state: Option<(Owner<'db>, Scratch<'db>)> = None;
    let mut run = |body: &'db MirBody<'db>| {
        if body.coroutine.is_some() || !has_checks(body) {
            return;
        }
        let (owner, scratch) = state.get_or_insert_with(|| (Owner::new(b), Scratch::default()));
        Propagator::new(b, owner, scratch, body, &mut out).run();
    };
    let closures = match runs_on(b) {
        true => {
            // alloc: empty, allocates nothing
            let Some(body) = b.runtime_mir() else { return Vec::new() };
            run(body);
            &body.closures
        }
        false => match b.mir() {
            Some(body) => &body.closures,
            // alloc: empty, allocates nothing
            None => return Vec::new(),
        },
    };
    let mut stack: SmallVec<[InternedClosureId<'db>; 4]> = SmallVec::new();
    stack.extend(closures.iter().rev().copied());
    while let Some(closure) = stack.pop() {
        let Ok(body) = db.runtime_mir_body_for_closure(closure) else { continue };
        run(body);
        stack.extend(body.closures.iter().rev().copied());
    }
    out
}

/// `run_lint`'s kinds of body: functions, methods and associated constants. rustc leaves
/// other constants, statics and discriminants to const evaluation.
fn runs_on(b: &BodyCx<'_, '_, '_>) -> bool {
    match b.owner {
        DefWithBodyId::FunctionId(_) => true,
        DefWithBodyId::ConstId(it) => {
            matches!(it.lookup(b.db()).container, ItemContainerId::ImplId(_) | ItemContainerId::TraitId(_))
        }
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => false,
    }
}

/// rustc's `tainted_by_errors` from borrow checking the body and its closures, and from what
/// stops it before (const checking, a lowering error).
fn borrowck_failed(b: &BodyCx<'_, '_, '_>) -> bool {
    let result = hir_ty_borrowck::borrowck(b.db(), b.owner);
    result.diagnostics.iter().any(|it| it.is_error())
        || result.not_checked.is_some_and(|it| it.refuses() && it != NotChecked::TypeErrors)
}

/// The body has something the lint checks: an arithmetic operation, an `Assert`, or a call
/// with const arguments. Without one the propagation can find nothing.
fn has_checks(body: &MirBody<'_>) -> bool {
    body.basic_blocks.iter().any(|(_, block)| {
        let operation = block.statements.iter().any(|it| {
            matches!(
                it.kind,
                StatementKind::Assign(_, Rvalue::BinaryOp(..) | Rvalue::CheckedBinaryOp(..) | Rvalue::UnaryOp(..))
            )
        });
        operation
            || match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Assert { .. }) => true,
                Some(TerminatorKind::Call { func, .. }) => func
                    .fn_def()
                    .is_some_and(|(_, args)| args.iter().any(|it| matches!(it.kind(), GenericArgKind::Const(_)))),
                _ => false,
            }
    })
}

/// Which lint a finding is of.
#[derive(Clone, Copy)]
enum Lint {
    ArithmeticOverflow,
    UnconditionalPanic,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "arithmetic_overflow",
            Lint::UnconditionalPanic => "unconditional_panic",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "this arithmetic operation will overflow",
            Lint::UnconditionalPanic => "this operation will panic at runtime",
        }
    }
}

/// rustc's `AssertKind` as the lint labels it, each value known or `_`.
enum Panic {
    BoundsCheck { len: Option<Scalar>, index: Option<Scalar> },
    Shl(Option<Scalar>),
    Shr(Option<Scalar>),
    Overflow { op: &'static str, l: Option<Scalar>, r: Option<Scalar> },
    OverflowNeg(Option<Scalar>),
    DivisionByZero(Option<Scalar>),
    RemainderByZero(Option<Scalar>),
}

impl Panic {
    /// `AssertKind::Overflow`, of an operation that can overflow.
    fn overflow(op: &BinOp, l: Option<Scalar>, r: Option<Scalar>) -> Option<Panic> {
        let op = match op {
            BinOp::Shl => return Some(Panic::Shl(r)),
            BinOp::Shr => return Some(Panic::Shr(r)),
            BinOp::Add | BinOp::AddWithOverflow => "+",
            BinOp::Sub | BinOp::SubWithOverflow => "-",
            BinOp::Mul | BinOp::MulWithOverflow => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            _ => return None,
        };
        Some(Panic::Overflow { op, l, r })
    }
}

impl fmt::Display for Panic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let int = |value: &Option<Scalar>, alternate: bool| match value {
            Some(it) => scalar::const_int(*it, alternate),
            // alloc: the lint's text, for a lint found
            None => "_".to_owned(),
        };
        match self {
            Panic::BoundsCheck { len, index } => write!(
                f,
                "index out of bounds: the length is {} but the index is {}",
                int(len, false),
                int(index, false)
            ),
            Panic::Shl(by) => write!(f, "attempt to shift left by `{}`, which would overflow", int(by, true)),
            Panic::Shr(by) => write!(f, "attempt to shift right by `{}`, which would overflow", int(by, true)),
            Panic::Overflow { op, l, r } => {
                write!(f, "attempt to compute `{} {op} {}`, which would overflow", int(l, true), int(r, true))
            }
            Panic::OverflowNeg(value) => write!(f, "attempt to negate `{}`, which would overflow", int(value, true)),
            Panic::DivisionByZero(value) => write!(f, "attempt to divide `{}` by zero", int(value, true)),
            Panic::RemainderByZero(value) => {
                write!(f, "attempt to calculate the remainder of `{}` with a divisor of zero", int(value, true))
            }
        }
    }
}

/// rustc's `Value`: what the propagation knows a local holds. An enum constant whose layout
/// is a scalar or a pair is its interned allocation, read in place; an aggregate is its row
/// in [`Scratch::aggregates`].
#[derive(Clone, Copy)]
enum Value<'db> {
    Uninit,
    Scalar(Scalar),
    Enum(Allocation<'db>),
    Aggregate(u32),
}

const _: () = assert!(size_of::<Value<'static>>() <= 48);

/// An aggregate's fields, `len` rows of [`Scratch::fields`] from `start`; `variant` is `None`
/// for what is not an enum.
struct Aggregate {
    variant: Option<EnumVariantId>,
    start: u32,
    len: u32,
}

impl<'db> Value<'db> {
    /// rustc's `Value::immediate`: what is not an aggregate.
    fn immediate(self) -> Option<Value<'db>> {
        match self {
            Value::Scalar(_) | Value::Enum(_) => Some(self),
            Value::Uninit | Value::Aggregate(_) => None,
        }
    }

    fn scalar(self) -> Option<Scalar> {
        match self {
            Value::Scalar(it) => Some(it),
            _ => None,
        }
    }
}

/// rustc's `ConstPropMode`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    FullConstProp,
    OnlyInsideOwnBlock,
    NoPropagation,
}

const _: () = assert!(size_of::<Mode>() == 1);

/// How `CanConstProp` takes a use of a local.
enum Use {
    Store,
    Read,
    /// A borrow, a drop or a `yield`'s resume argument: what the propagation cannot follow.
    Escape,
}

/// What the bodies of one owner share: its trait environment and what answers in it.
struct Owner<'db> {
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    krate: base_db::Crate,
    pointer_size: Option<u8>,
}

impl<'db> Owner<'db> {
    fn new(b: &BodyCx<'_, '_, 'db>) -> Self {
        let db = b.db();
        let krate = b.cx.krate.base();
        Owner {
            infcx: DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::non_body_analysis()),
            env: db.trait_environment(b.owner.generic_def(db)),
            krate,
            pointer_size: db.target_data_layout(krate).ok().map(|it| it.pointer_size().bytes() as u8),
        }
    }
}

/// The propagation's buffers, cleared and reused for each body of an owner. `aggregates` and
/// `fields` are append-only within a body: a local's new aggregate is new rows, and the rows
/// of the one it replaces stay until the body is done.
#[derive(Default)]
struct Scratch<'db> {
    locals: Vec<Value<'db>>,
    modes: Vec<Mode>,
    assigned: BitSet,
    visited: BitSet,
    /// rustc's `written_only_inside_own_block_locals`: the set, and the order to clear it in.
    written: BitSet,
    written_list: Vec<u32>,
    worklist: Vec<BasicBlockId>,
    aggregates: Vec<Aggregate>,
    fields: Vec<Value<'db>>,
}

/// rustc's `ConstPropagator` over one body.
struct Propagator<'a, 'b, 'db> {
    b: &'a BodyCx<'a, 'b, 'db>,
    db: &'db Db,
    owner: &'a Owner<'db>,
    body: &'db MirBody<'db>,
    s: &'a mut Scratch<'db>,
    out: &'a mut Vec<Found>,
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

/// rustc's `CheckedBinaryOp` is the operation that gives the overflow with the result.
fn with_overflow(op: &BinOp) -> &BinOp {
    match op {
        BinOp::Add => &BinOp::AddWithOverflow,
        BinOp::Sub => &BinOp::SubWithOverflow,
        BinOp::Mul => &BinOp::MulWithOverflow,
        op => op,
    }
}

fn is_indirect(place: &StoredPlace) -> bool {
    place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref))
}

impl<'a, 'b, 'db> Propagator<'a, 'b, 'db> {
    fn new(
        b: &'a BodyCx<'a, 'b, 'db>,
        owner: &'a Owner<'db>,
        s: &'a mut Scratch<'db>,
        body: &'db MirBody<'db>,
        out: &'a mut Vec<Found>,
    ) -> Self {
        let locals = body.locals.len();
        s.locals.clear();
        s.locals.resize(locals, Value::Uninit);
        s.written.reset(locals);
        s.written_list.clear();
        s.visited.reset(body.basic_blocks.len());
        s.worklist.clear();
        s.worklist.push(body.start_block);
        s.aggregates.clear();
        s.fields.clear();
        let mut propagator = Propagator { b, db: b.db(), owner, body, s, out };
        propagator.can_const_prop();
        propagator
    }

    /// `visit_body`: the blocks from the start, the last found first, each once.
    fn run(&mut self) {
        let body = self.body;
        while let Some(block) = self.s.worklist.pop() {
            if !self.s.visited.insert(block_index(block)) {
                continue;
            }
            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shallow, _)) => {}
                    StatementKind::Assign(place, rvalue) => self.assign(place, rvalue, statement.span),
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => self.remove_const(*local),
                    StatementKind::Deinit(place) if !is_indirect(place) => self.remove_const(place.local),
                    StatementKind::InlineAsm(asm) => {
                        let outputs = asm.operands.iter().filter_map(|it| match it {
                            AsmOperand::Reg { output, .. } => output.as_ref(),
                            _ => None,
                        });
                        for place in outputs.chain(&asm.taken) {
                            if !is_indirect(place) {
                                self.remove_const(place.local);
                            }
                        }
                    }
                    StatementKind::Deinit(_) | StatementKind::FakeRead(_) | StatementKind::Nop => {}
                }
            }
            if let Some(terminator) = &data.terminator {
                self.terminator(terminator);
            }
            for at in 0..self.s.written_list.len() {
                let local = self.s.written_list[at] as usize;
                if self.s.written.remove(local) {
                    self.s.locals[local] = Value::Uninit;
                }
            }
            self.s.written_list.clear();
        }
    }

    /// `CanConstProp::check`: a local of a union, of a type without a layout or of 1024 bytes
    /// or more is never known; one assigned more than once (a parameter is on entry) is known
    /// only in the block it is assigned in; one borrowed or dropped is never known.
    fn can_const_prop(&mut self) {
        let body = self.body;
        let mut modes = std::mem::take(&mut self.s.modes);
        modes.clear();
        modes.extend(body.locals.iter().map(|(_, local)| {
            let ty = local.ty.as_ref();
            let fits = self.scalar_kind(ty).is_some()
                || (!ty.is_union() && self.layout_size(ty).is_some_and(|it| it < 1024));
            match fits {
                true => Mode::FullConstProp,
                false => Mode::NoPropagation,
            }
        }));
        let assigned = &mut self.s.assigned;
        assigned.reset(modes.len());
        for &param in &body.param_locals {
            assigned.insert(index(param));
        }
        let mut place = |place: &StoredPlace, mut use_: Use| {
            if let Some(ProjectionElem::Deref) = place.projection.as_slice().first() {
                use_ = Use::Read;
            }
            let at = index(place.local);
            match use_ {
                Use::Store if !assigned.insert(at) => {
                    if modes[at] == Mode::FullConstProp {
                        modes[at] = Mode::OnlyInsideOwnBlock;
                    }
                }
                Use::Escape => modes[at] = Mode::NoPropagation,
                Use::Store | Use::Read => {}
            }
        };
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shallow, _)) => {}
                    StatementKind::Assign(to, rvalue) => {
                        place(to, Use::Store);
                        if let Rvalue::Ref(_, borrowed) = rvalue {
                            place(borrowed, Use::Escape);
                        }
                    }
                    StatementKind::InlineAsm(asm) => {
                        for operand in asm.operands.iter() {
                            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                                place(output, Use::Store);
                            }
                        }
                        if let Some(taken) = &asm.taken {
                            place(taken, Use::Store);
                        }
                    }
                    StatementKind::Deinit(deinit) => place(deinit, Use::Store),
                    StatementKind::FakeRead(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Drop { place: dropped, .. } | TerminatorKind::DropAndReplace { place: dropped, .. }) => {
                    place(dropped, Use::Escape)
                }
                Some(TerminatorKind::Call { destination, .. }) => place(destination, Use::Store),
                Some(TerminatorKind::Yield { resume_arg, .. }) => place(resume_arg, Use::Escape),
                _ => {}
            }
        }
        self.s.modes = modes;
    }

    fn remove_const(&mut self, local: LocalId) {
        let at = index(local);
        self.s.locals[at] = Value::Uninit;
        self.s.written.remove(at);
    }

    /// `access_mut` and the write through it.
    fn set(&mut self, local: LocalId, value: Value<'db>) -> Option<()> {
        let at = index(local);
        match self.s.modes[at] {
            Mode::NoPropagation => return None,
            Mode::OnlyInsideOwnBlock => {
                if self.s.written.insert(at) {
                    self.s.written_list.push(at as u32);
                }
            }
            Mode::FullConstProp => {}
        }
        self.s.locals[at] = value;
        Some(())
    }

    /// `visit_assign`.
    fn assign(&mut self, place: &StoredPlace, rvalue: &Rvalue, span: MirSpan) {
        if self.check_rvalue(rvalue, span).is_none() || is_indirect(place) {
            return;
        }
        if self.s.modes[index(place.local)] != Mode::NoPropagation && self.eval_rvalue(rvalue, place).is_none() {
            self.remove_const(place.local);
        }
    }

    /// `check_rvalue`: the lints of a unary or binary operation; `None` to keep the result
    /// unknown, as of a borrow (whose local is then unknown) or of what names a generic
    /// parameter.
    fn check_rvalue(&mut self, rvalue: &Rvalue, span: MirSpan) -> Option<()> {
        match rvalue {
            Rvalue::UnaryOp(op, arg) => self.check_unary_op(*op, arg, span)?,
            Rvalue::BinaryOp(op, left, right) => self.check_binary_op(op, left, right, span)?,
            Rvalue::CheckedBinaryOp(op, left, right) => self.check_binary_op(with_overflow(op), left, right, span)?,
            Rvalue::Ref(_, place) => {
                self.remove_const(place.local);
                return None;
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
            Rvalue::Use(_)
            | Rvalue::Repeat(..)
            | Rvalue::Len(_)
            | Rvalue::Cast(..)
            | Rvalue::Discriminant(_)
            | Rvalue::Aggregate(..)
            | Rvalue::CopyForDeref(_) => {}
        }
        match self.rvalue_has_param(rvalue) {
            true => None,
            false => Some(()),
        }
    }

    /// `check_unary_op`: `-x` of an integer that is its type's minimum.
    fn check_unary_op(&mut self, op: UnOp, arg: &Operand, span: MirSpan) -> Option<()> {
        let arg = self.eval_operand(arg)?;
        if let (UnOp::Neg, Value::Scalar(arg)) = (op, arg)
            && arg.is_integral()
        {
            let zero = Scalar::new(0, arg.size, arg.kind);
            if scalar::overflows(&BinOp::SubWithOverflow, zero, arg)? {
                self.report(span, Lint::ArithmeticOverflow, Panic::OverflowNeg(Some(arg)));
                return None;
            }
        }
        Some(())
    }

    /// `check_binary_op`: a shift by the width of its left side or more, even when the left
    /// side is not known, and an addition, subtraction or multiplication that overflows.
    fn check_binary_op(&mut self, op: &BinOp, left: &Operand, right: &Operand, span: MirSpan) -> Option<()> {
        let r = self.eval_operand(right);
        let l = self.eval_operand(left);
        if let BinOp::Shl | BinOp::Shr = op {
            let r = r?;
            let left_bits = u128::from(self.scalar_kind(self.operand_ty(left)?)?.1) * 8;
            if let Value::Scalar(by) = r
                && by.bits >= left_bits
            {
                self.report(span, Lint::ArithmeticOverflow, Panic::overflow(op, None, Some(by))?);
                return None;
            }
        }
        let op = with_overflow(op);
        if let (Some(Value::Scalar(l)), Some(Value::Scalar(r))) = (l, r)
            && l.is_integral()
            && matches!(op, BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow)
            && scalar::overflows(op, l, r)?
        {
            self.report(span, Lint::ArithmeticOverflow, Panic::overflow(op, Some(l), Some(r))?);
            return None;
        }
        Some(())
    }

    /// `check_assertion`: an `Assert` whose condition is known not to be what it expects, of
    /// a bounds check or a division (the overflows are linted at their operation).
    fn check_assertion(&mut self, expected: bool, msg: &AssertKind, cond: &Operand, span: MirSpan) {
        let Some(Value::Scalar(value)) = self.eval_operand(cond) else { return };
        if value.kind != Kind::Bool || value.bits == u128::from(expected) {
            return;
        }
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &cond.kind {
            self.remove_const(place.local);
        }
        let int = |this: &Self, op: &Operand| this.eval_operand(op).and_then(Value::scalar);
        let panic = match msg {
            AssertKind::DivisionByZero(op) => Panic::DivisionByZero(int(self, op)),
            AssertKind::RemainderByZero(op) => Panic::RemainderByZero(int(self, op)),
            AssertKind::Overflow(op @ (BinOp::Div | BinOp::Rem), l, r) => {
                let Some(panic) = Panic::overflow(op, int(self, l), int(self, r)) else { return };
                panic
            }
            AssertKind::BoundsCheck { len, index } => Panic::BoundsCheck { len: int(self, len), index: int(self, index) },
            AssertKind::Overflow(..) | AssertKind::OverflowNeg(_) => return,
            // rustc's "need proper const propagator for these": never linted here.
            AssertKind::MisalignedPointerDereference { .. }
            | AssertKind::NullPointerDereference
            | AssertKind::NullReferenceConstructed
            | AssertKind::InvalidEnumConstruction(_) => return,
        };
        self.report(span, Lint::UnconditionalPanic, panic);
    }

    /// `eval_rvalue`: the value an assignment to a whole local gives it, if known. The
    /// local's layout is known: `can_const_prop` gave it a mode that propagates.
    fn eval_rvalue(&mut self, rvalue: &Rvalue, dest: &StoredPlace) -> Option<()> {
        if !dest.projection.is_empty() {
            return None;
        }
        let value = match rvalue {
            Rvalue::Use(operand) => self.eval_operand(operand)?,
            Rvalue::CopyForDeref(place) => self.eval_place(place)?,
            Rvalue::BinaryOp(op, left, right) | Rvalue::CheckedBinaryOp(op, left, right) => {
                let l = self.eval_operand(left)?.scalar()?;
                let r = self.eval_operand(right)?.scalar()?;
                let op = match rvalue {
                    Rvalue::CheckedBinaryOp(..) => with_overflow(op),
                    _ => op,
                };
                match scalar::binary_op(op, l, r)? {
                    Output::One(value) => Value::Scalar(value),
                    Output::Pair(value, overflowed) => {
                        let fields = [Value::Scalar(value), Value::Scalar(Scalar::from_bool(overflowed))];
                        self.aggregate(None, fields)
                    }
                }
            }
            Rvalue::UnaryOp(op, operand) => {
                let value = self.eval_operand(operand)?.scalar()?;
                Value::Scalar(scalar::unary_op(*op, value)?)
            }
            Rvalue::Aggregate(AggregateKind::Union(..), _) => return None,
            Rvalue::Aggregate(kind, operands) => {
                let variant = match kind {
                    AggregateKind::Adt(VariantId::EnumVariantId(it), _) => Some(*it),
                    _ => None,
                };
                let start = self.s.fields.len() as u32;
                self.s.fields.reserve(operands.len());
                for operand in operands.iter() {
                    let value = self.eval_operand(operand).unwrap_or(Value::Uninit);
                    self.s.fields.push(value);
                }
                self.s.aggregates.push(Aggregate { variant, start, len: operands.len() as u32 });
                Value::Aggregate(self.s.aggregates.len() as u32 - 1)
            }
            Rvalue::Cast(kind, operand, to) => {
                let value = self.eval_operand(operand)?.scalar()?;
                let (to, size) = self.scalar_kind(to.as_ref())?;
                Value::Scalar(match kind {
                    CastKind::IntToInt | CastKind::IntToFloat => scalar::int_cast(value, to, size)?,
                    CastKind::FloatToFloat | CastKind::FloatToInt => scalar::float_cast(value, to, size)?,
                    _ => return None,
                })
            }
            Rvalue::Discriminant(place) => {
                let variant = match self.get_const(place)? {
                    Value::Scalar(_) => None,
                    Value::Enum(allocation) => Some(self.read_discriminant(allocation)?),
                    Value::Aggregate(at) => self.s.aggregates[at as usize].variant,
                    Value::Uninit => return None,
                };
                let bits = match variant {
                    Some(it) => self.db.const_eval_discriminant(it).ok()? as u128,
                    None => 0,
                };
                let (kind, size) = self.scalar_kind(self.body.locals[dest.local].ty.as_ref())?;
                Value::Scalar(Scalar::new(bits, size, kind))
            }
            Rvalue::Repeat(..) | Rvalue::Ref(..) | Rvalue::Len(_) => return None,
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
        };
        self.set(dest.local, value)
    }

    fn aggregate<const N: usize>(&mut self, variant: Option<EnumVariantId>, fields: [Value<'db>; N]) -> Value<'db> {
        let start = self.s.fields.len() as u32;
        self.s.fields.extend(fields);
        self.s.aggregates.push(Aggregate { variant, start, len: N as u32 });
        Value::Aggregate(self.s.aggregates.len() as u32 - 1)
    }

    /// `read_discriminant` of an enum constant, from its bytes.
    fn read_discriminant(&self, allocation: Allocation<'db>) -> Option<EnumVariantId> {
        let TyKind::Adt(def, _) = allocation.ty.kind() else { return None };
        let AdtId::EnumId(enum_) = def.def_id() else { return None };
        let layout = self.db.layout_of_ty(allocation.ty.store(), self.env_and_crate().store()).ok()?;
        if matches!(layout.variants, hir_def::layout::Variants::Empty) {
            return None;
        }
        let target = self.db.target_data_layout(self.owner.krate).ok()?;
        detect_variant_from_bytes(&layout, self.db, target, &allocation.memory, enum_).map(|(it, _)| it)
    }

    /// `visit_terminator`: an `Assert`'s check, a call's const parameters, and the blocks to
    /// go on with: of a `SwitchInt` on a known value, only the one it goes to.
    fn terminator(&mut self, terminator: &Terminator) {
        match &terminator.kind {
            TerminatorKind::Assert { cond, expected, msg, .. } => {
                self.check_assertion(*expected, msg, cond, terminator.span)
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                if let Some(Value::Scalar(value)) = self.eval_operand(discr) {
                    self.s.worklist.push(targets.target_for_value(value.bits));
                    return;
                }
            }
            TerminatorKind::Call { func, .. } => self.check_const_params(func, terminator.span),
            _ => {}
        }
        self.s.worklist.extend(terminator.kind.successors());
        if !matches!(terminator.kind, TerminatorKind::FalseUnwind { .. }) {
            self.s.worklist.extend(terminator.kind.unwind());
        }
    }

    /// `ConstNIsZero`: a call of a function with a `#[rustc_panics_when_zero]` const
    /// parameter, given 0. The callee's generics are read only for an argument of 0.
    fn check_const_params(&mut self, func: &Operand, span: MirSpan) {
        let Some((callable, args)) = func.fn_def() else { return };
        let db = self.db;
        let mut generics = None;
        for (at, arg) in args.iter().enumerate() {
            let GenericArgKind::Const(value) = arg.kind() else { continue };
            if try_const_usize(db, value) != Some(0) {
                continue;
            }
            let generics = generics.get_or_insert_with(|| {
                let def: GenericDefId = match callable {
                    CallableDefId::FunctionId(it) => it.into(),
                    CallableDefId::StructId(it) => AdtId::from(it).into(),
                    CallableDefId::EnumVariantId(it) => AdtId::from(it.lookup(db).parent).into(),
                };
                hir_ty::generics::generics(db, def)
            });
            let Some((GenericParamId::ConstParamId(param), _)) = generics.iter(false).nth(at) else { continue };
            let Some(name) = panics_when_zero(db, param) else { continue };
            // alloc: the lint's text, for a lint found
            self.emit(span, Lint::UnconditionalPanic, format!("const parameter `{name}` is zero"));
        }
    }

    /// `eval_operand`: the value of an operand, if known and not an aggregate.
    fn eval_operand(&self, op: &Operand) -> Option<Value<'db>> {
        match &op.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.eval_place(place),
            OperandKind::Constant { konst, ty } => self.eval_constant(konst.as_ref(), ty.as_ref()),
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                if allocation.ty.has_non_region_param() {
                    return None;
                }
                if let TyKind::Uint(UintTy::U32) = allocation.ty.kind()
                    && let Some(MirSpan::ExprId(expr)) = op.span
                    && self.is_placed(expr)
                {
                    return None;
                }
                self.imm(allocation)
            }
            OperandKind::Static(_) => None,
        }
    }

    /// `eval_constant`: a constant evaluated, unless it names a generic parameter.
    fn eval_constant(&self, konst: Const<'db>, ty: Ty<'db>) -> Option<Value<'db>> {
        if konst.has_non_region_param() || ty.has_non_region_param() {
            return None;
        }
        let allocation = match konst.kind() {
            ConstKind::Value(value) => {
                let leaf = value.try_to_leaf()?;
                let (kind, size) = self.scalar_kind(ty)?;
                if u64::from(size) != leaf.size().bytes() {
                    return None;
                }
                return Some(Value::Scalar(Scalar::new(leaf.to_bits_unchecked(), size, kind)));
            }
            ConstKind::Alias(_, AliasConst { kind, args, .. }) => match kind {
                AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
                | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
                | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
                    let (id, args) = lookup_impl_const(&self.owner.infcx, self.owner.env, id, args);
                    self.db.const_eval(id, args, None).ok()?
                }
                AliasConstKind::Anon { def_id } => self.db.anon_const_eval(def_id.0, args, None).ok()?,
                AliasConstKind::InherentSelf { .. } => return None,
            },
            _ => return None,
        };
        self.imm(allocation)
    }

    /// The value an allocation holds, if it is one the lint reads: a primitive's bytes as a
    /// scalar, an enum whose layout is a scalar or a pair as the allocation itself.
    fn imm(&self, allocation: Allocation<'db>) -> Option<Value<'db>> {
        let memory = &allocation.memory;
        if let Some((kind, size)) = self.scalar_kind(allocation.ty) {
            if memory.len() != usize::from(size) {
                return None;
            }
            let mut bytes = [0; 16];
            bytes[..memory.len()].copy_from_slice(memory);
            return Some(Value::Scalar(Scalar::new(u128::from_le_bytes(bytes), size, kind)));
        }
        let TyKind::Adt(def, _) = allocation.ty.kind() else { return None };
        if !def.is_enum() {
            return None;
        }
        let layout = self.db.layout_of_ty(allocation.ty.store(), self.env_and_crate().store()).ok()?;
        let pair = matches!(layout.backend_repr, BackendRepr::Scalar(_) | BackendRepr::ScalarPair { .. });
        (pair && layout.size.bytes() == memory.len() as u64).then_some(Value::Enum(allocation))
    }

    /// `eval_place`: the value of a place, if known and not an aggregate.
    fn eval_place(&self, place: &StoredPlace) -> Option<Value<'db>> {
        self.get_const(place)?.immediate()
    }

    /// `get_const`: what is known of a place, through the fields and elements of aggregates.
    fn get_const(&self, place: &StoredPlace) -> Option<Value<'db>> {
        let mut value = self.s.locals[index(place.local)];
        for elem in place.projection.as_slice() {
            let Value::Aggregate(aggregate) = value else { return None };
            let aggregate = &self.s.aggregates[aggregate as usize];
            let at = match elem {
                ProjectionElem::Field(field) => field.0,
                ProjectionElem::Index(local) => match self.s.locals[index(*local)] {
                    Value::Scalar(it) if Some(it.size) == self.owner.pointer_size => u32::try_from(it.bits).ok()?,
                    _ => return None,
                },
                ProjectionElem::ConstantIndex { offset, from_end: false, .. } => u32::try_from(*offset).ok()?,
                _ => return None,
            };
            value = match at < aggregate.len {
                true => self.s.fields[(aggregate.start + at) as usize],
                false => Value::Uninit,
            };
        }
        Some(value)
    }

    /// `rvalue.has_param()`: a type or constant in it, or the type of a field it reads, names
    /// a generic parameter.
    fn rvalue_has_param(&self, rvalue: &Rvalue) -> bool {
        let mut found = false;
        rvalue.for_each_operand(&mut |it| found |= self.operand_has_param(it));
        found
            || match rvalue {
                Rvalue::Repeat(_, count) => count.as_ref().has_non_region_param(),
                Rvalue::Ref(_, place) | Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                    self.place_has_param(place)
                }
                Rvalue::Cast(_, _, ty) => ty.as_ref().has_non_region_param(),
                Rvalue::Aggregate(kind, _) => match kind {
                    AggregateKind::Array(ty) | AggregateKind::Tuple(ty) | AggregateKind::Closure(ty) => {
                        ty.as_ref().has_non_region_param()
                    }
                    AggregateKind::Adt(_, args) => args.as_ref().has_non_region_param(),
                    AggregateKind::Union(..) => false,
                },
                _ => false,
            }
    }

    fn operand_has_param(&self, operand: &Operand) -> bool {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place_has_param(place),
            OperandKind::Constant { konst, ty } => {
                konst.as_ref().has_non_region_param() || ty.as_ref().has_non_region_param()
            }
            OperandKind::Allocation { allocation } => allocation.as_ref().ty.has_non_region_param(),
            OperandKind::Static(_) => false,
        }
    }

    /// A field projection of the place is of a type that names a generic parameter: rustc's
    /// `PlaceElem::Field` holds the field's type. A field of a local whose type names none
    /// names none, so only a generic local's projections are computed.
    fn place_has_param(&self, place: &StoredPlace) -> bool {
        let local = self.body.locals[place.local].ty.as_ref();
        if place.projection.is_empty() || !local.has_non_region_param() {
            return false;
        }
        let mut ty = PlaceTy::from_ty(local);
        for elem in place.projection.as_slice() {
            ty = ty.multi_projection_ty(&self.owner.infcx, self.owner.env, std::slice::from_ref(elem));
            if let ProjectionElem::Field(_) = elem
                && ty.ty.has_non_region_param()
            {
                return true;
            }
        }
        false
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        Some(match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
                self.body.locals[place.local].ty.as_ref()
            }
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                place.as_ref().ty(self.body, &self.owner.infcx, self.owner.env).ty
            }
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(_) => return None,
        })
    }

    /// What a value of `ty` is, with its size, when it is a primitive the lint computes with.
    fn scalar_kind(&self, ty: Ty<'db>) -> Option<(Kind, u8)> {
        let int = |signed: bool, bits: Option<u64>| {
            let size = match bits {
                Some(bits) => (bits / 8) as u8,
                None => self.owner.pointer_size?,
            };
            Some((Kind::Int { signed, pointer_sized: bits.is_none() }, size))
        };
        match ty.kind() {
            TyKind::Bool => Some((Kind::Bool, 1)),
            TyKind::Char => Some((Kind::Char, 4)),
            TyKind::Int(it) => int(true, it.bit_width()),
            TyKind::Uint(it) => int(false, it.bit_width()),
            TyKind::Float(it) => Some((Kind::Float(it), (it.bit_width() / 8) as u8)),
            _ => None,
        }
    }

    fn env_and_crate(&self) -> ParamEnvAndCrate<'db> {
        ParamEnvAndCrate { param_env: self.owner.env, krate: self.owner.krate }
    }

    fn layout_size(&self, ty: Ty<'db>) -> Option<u64> {
        let layout = self.db.layout_of_ty(ty.store(), self.env_and_crate().store()).ok()?;
        Some(layout.size.bytes())
    }

    /// A literal `line!()` or `column!()` expands to, whose value lowering leaves to the code
    /// generator.
    fn is_placed(&self, expr: ExprId) -> bool {
        let Ok(source) = self.b.source_map.expr_syntax(expr) else { return false };
        let Some(call) = source.file_id.macro_file() else { return false };
        matches!(
            call.loc(self.db).def.kind,
            MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::Line | BuiltinFnLikeExpander::Column)
        )
    }

    /// `report_assert_as_lint`.
    fn report(&mut self, span: MirSpan, lint: Lint, panic: Panic) {
        // alloc: the lint's text, for a lint found
        self.emit(span, lint, panic.to_string());
    }

    /// The lint at what `span` is of, labelled `label` there, once: none in a macro of
    /// another crate, as these lints are not reported in external macros.
    fn emit(&mut self, span: MirSpan, lint: Lint, label: String) {
        let (node, span) = self.location(span);
        if self.b.in_external_macro(node) {
            return;
        }
        // clone: a lint's span, for a lint found
        let mut diag = Diag::lint(lint.name(), lint.message(), span.clone());
        diag.span_label(span, label);
        let found = self.b.lint(diag, Phase::Mir, node);
        if !self.out.iter().any(|it| it.diag == found.diag && it.lint_at == found.lint_at) {
            self.out.push(found);
        }
    }

    /// The node and span of what `span` is of; of what lowering made with no expression, the
    /// body's item. rustc's expression in parentheses has the span of the parentheses.
    fn location(&self, span: MirSpan) -> (InFile<SyntaxNodePtr>, Span) {
        if let MirSpan::ExprId(_) = span
            && let Some(node) = self.b.mir_node(span)
        {
            let mut syntax = self.b.cx.syntax_view(node).value;
            while let Some(paren) = syntax.parent().and_then(ast::ParenExpr::cast) {
                syntax = paren.syntax();
            }
            let node = InFile::new(node.file_id, ast::ptr(syntax));
            return (node, self.b.cx.span_of(node));
        }
        if let Some(node) = self.b.mir_node(span)
            && let Some(span) = self.b.mir_span(span)
        {
            return (node, span);
        }
        let owner = self.b.owner_node();
        let node = self.b.cx.syntax_view(owner);
        (owner, self.b.cx.span(InFile::new(owner.file_id, def_range_view(node.value))))
    }
}

/// The name of `param` when it is marked `#[rustc_panics_when_zero]`.
fn panics_when_zero(db: &Db, param: ConstParamId) -> Option<String> {
    let source = param.parent().child_source_view(db, param.local_id());
    let Either::Left(ast::TypeOrConstParam::Const(node)) = source.value? else { return None };
    let marked = node.attrs().any(|attr| attr.path().is_some_and(|path| path.syntax().text() == "rustc_panics_when_zero"));
    // alloc: the lint's text, for a lint found
    marked.then(|| node.name().map(|it| it.text().to_string()))?
}
