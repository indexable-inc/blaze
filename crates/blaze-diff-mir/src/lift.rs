//! Lifting: a function's monomorphized runtime MIR as an expression graph.
//!
//! The lift runs the body symbolically. Each local holds a value ([`Val`]): a float is a node
//! of the graph, a `bool` a condition node, an integer only ever a constant (a loop counter, a
//! `powi` exponent, an index), a tuple or array its elements. MIR's sharing is kept: a local
//! read twice is one node used twice, and the graph is hash-consed besides.
//!
//! - **Straight-line code** maps statement by statement: float arithmetic and comparisons, the
//!   float intrinsics and std's float methods ([`METHODS`]) that the derivative rule table
//!   (`blaze_diff::diff::RULES`) has an operation for.
//! - **A branch** on a condition that is not a constant runs both arms to the function's
//!   return and joins the two results with `select`. Both arms are pure (nothing here has an
//!   effect), so computing both is the same function.
//! - **A loop** unrolls: its exit test is on integers that are constants on every path, so
//!   each test folds and the walk takes the one way. A loop whose exit depends on a float is
//!   [`NotLifted::Loop`]; so is one that turns more than [`MAX_UNROLL`] times.
//!
//! Everything else is a typed refusal naming the thing ([`NotLifted`]): a call of a function
//! the table does not know, memory (a reference, a static, a place behind a pointer), a type
//! that is not a scalar float, an operation with no expression form. The lift never guesses
//! and never falls back; what layer 1 would do with the same code is [`NotLifted::layer_1`].
//!
//! Every walk is bounded: [`MAX_UNROLL`] visits of a block on one path, [`MAX_STEPS`] blocks
//! over all paths.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use blaze_diff::{Graph, Id, Op, Width};
use hir_def::{FunctionId, HasModule, ItemContainerId, Lookup, signatures::FunctionSignature};
use hir_ty::{
    InferBodyId, MemoryMap,
    db::HirDatabase,
    intrinsic::Intrinsic,
    next_solver::{ConstKind, StoredTy, Ty, TyKind},
};
use hir_ty_mir::mir::{
    AggregateKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, MirSpan, Operand, OperandKind,
    ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, TerminatorKind, UnOp,
    return_slot,
};
use rustc_type_ir::{FloatTy, inherent::IntoKind};

/// The most times one path may enter one block: a loop of more turns is not unrolled.
pub const MAX_UNROLL: u32 = 64;
/// The most blocks the walk runs over all paths: past it the branches make too many paths.
pub const MAX_STEPS: u32 = 20_000;
/// The most fields a tuple written field by field may have.
const MAX_FIELDS: usize = 64;

/// std's float methods the lift knows, by name, with the operation each is: the inherent
/// methods of `f32` and `f64` in `core` and `std`. Each is the operation `blaze_diff::eval`
/// computes by the same method.
pub const METHODS: &[(&str, Op)] = &[
    ("abs", Op::Abs),
    ("cos", Op::Cos),
    ("exp", Op::Exp),
    ("ln", Op::Ln),
    ("max", Op::Max),
    ("min", Op::Min),
    ("mul_add", Op::MulAdd),
    ("powf", Op::Powf),
    ("powi", Op::Powi),
    ("sin", Op::Sin),
    ("sqrt", Op::Sqrt),
    ("tan", Op::Tan),
    ("tanh", Op::Tanh),
];

/// The operation a float intrinsic is, for the ones the rule table has. `fmuladd` is not one:
/// whether it fuses is the target's choice, so it has no one value.
pub fn intrinsic_op(intrinsic: Intrinsic) -> Option<Op> {
    Some(match intrinsic {
        Intrinsic::Sqrt => Op::Sqrt,
        Intrinsic::Sin => Op::Sin,
        Intrinsic::Cos => Op::Cos,
        Intrinsic::Exp => Op::Exp,
        Intrinsic::Log => Op::Ln,
        Intrinsic::Fabs => Op::Abs,
        Intrinsic::Powf => Op::Powf,
        Intrinsic::Powi => Op::Powi,
        Intrinsic::Fma => Op::MulAdd,
        Intrinsic::MinimumNumberNsz(_) => Op::Min,
        Intrinsic::MaximumNumberNsz(_) => Op::Max,
        _ => return None,
    })
}

/// The name in `core::intrinsics` of the intrinsic that computes `op`, for the ones that have
/// one.
pub fn intrinsic_name(op: Op) -> Option<&'static str> {
    Some(match op {
        Op::Sqrt => "sqrt",
        Op::Sin => "sin",
        Op::Cos => "cos",
        Op::Exp => "exp",
        Op::Ln => "log",
        Op::Abs => "fabs",
        Op::Powf => "powf",
        Op::Powi => "powi",
        Op::MulAdd => "fma",
        _ => return None,
    })
}

/// Why a branch did not lift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchWhy {
    /// It switches on a value that is neither a `bool` nor a constant integer.
    NotACondition,
    /// One way panics (an overflow check, a bounds check, an `assert!`): a panic is not a
    /// value a `select` can hold.
    Panics,
    /// The branches make more than [`MAX_STEPS`] blocks of paths.
    TooManyPaths,
    /// The two arms return values of different shapes.
    ArmsDiffer,
}

/// Why a body did not lift: the thing, and where it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotLifted {
    /// The loop's exit depends on a value that is not a constant (`turns: None`), or it turns
    /// more than `turns` times.
    Loop {
        span: MirSpan,
        turns: Option<u32>,
    },
    Branch {
        span: MirSpan,
        why: BranchWhy,
    },
    /// A call of something the rule table has no operation for.
    Call {
        callee: String,
        span: MirSpan,
    },
    /// A read or write that is not of a scalar local: a reference, a static, a place behind a
    /// pointer, a local read before it is written.
    Memory {
        place: String,
        span: MirSpan,
    },
    /// A value of a type the graph has no node for.
    Type {
        ty: String,
        span: MirSpan,
    },
    /// An operation with no expression form: `%` on floats, a cast, a shift.
    Operation {
        what: String,
        span: MirSpan,
    },
}

impl NotLifted {
    /// Where the thing is written in the body.
    pub fn span(&self) -> MirSpan {
        match self {
            NotLifted::Loop { span, .. }
            | NotLifted::Branch { span, .. }
            | NotLifted::Call { span, .. }
            | NotLifted::Memory { span, .. }
            | NotLifted::Type { span, .. }
            | NotLifted::Operation { span, .. } => *span,
        }
    }

    /// What automatic differentiation on MIR (layer 1 of docs/future/autodiff.md) does with
    /// the same code: what the user gets by removing `symbolic`.
    pub fn layer_1(&self) -> &'static str {
        match self {
            NotLifted::Loop { .. } => {
                "differentiates the loop as it runs, turn by turn (reverse mode keeps a tape of the values each turn needs)"
            }
            NotLifted::Branch { .. } => "differentiates the arm that runs",
            NotLifted::Call { .. } => {
                "differentiates the callee's body, or uses its `#[blaze::derivative(of = ..)]` rule, and refuses a callee with neither"
            }
            NotLifted::Memory { .. } => {
                "carries a derivative beside each value in memory, through references and slices"
            }
            NotLifted::Type { .. } => "differentiates each float field of the value",
            NotLifted::Operation { .. } => {
                "differentiates the operation where it has a derivative and refuses it where it has none"
            }
        }
    }
}

impl std::fmt::Display for NotLifted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotLifted::Loop { turns: None, .. } => f.write_str(
                "a loop whose number of turns depends on a value that is not a constant",
            ),
            NotLifted::Loop { turns: Some(turns), .. } => {
                write!(f, "a loop that turns more than {turns} times, the most a loop is unrolled")
            }
            NotLifted::Branch { why, .. } => f.write_str(match why {
                BranchWhy::NotACondition => {
                    "a branch on a value that is neither a `bool` nor a constant integer"
                }
                BranchWhy::Panics => "a branch one way of which panics",
                BranchWhy::TooManyPaths => {
                    "branches that make too many paths to join with `select`"
                }
                BranchWhy::ArmsDiffer => "a branch whose arms return values of different shapes",
            }),
            NotLifted::Call { callee, .. } => {
                write!(f, "a call of {callee}, which has no derivative rule")
            }
            NotLifted::Memory { place, .. } => write!(f, "memory: {place}"),
            NotLifted::Type { ty, .. } => {
                write!(
                    f,
                    "a value of type `{ty}`, which is not a scalar `f32` or `f64` of the function's width"
                )
            }
            NotLifted::Operation { what, .. } => write!(f, "{what}, which has no expression form"),
        }
    }
}

impl std::error::Error for NotLifted {}

/// What a lifted function takes and returns, for lowering an expression over the same
/// parameters back to a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature<'db> {
    /// The body the expression was lifted from: a lowered body is its owner's.
    pub owner: InferBodyId<'db>,
    /// The float type: every parameter's, and a scalar result's.
    pub float: StoredTy,
    /// How many parameters there are, each of type `float`: variable `i` is parameter `i`.
    pub params: u32,
    /// The return type: `float`, or a tuple or array of it.
    pub ret: StoredTy,
}

/// The function a call of each operation calls, as the lifted body called it: lowering calls
/// the same one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Callees {
    seen: Vec<(Op, Operand, Option<Intrinsic>)>,
}

impl Callees {
    /// The callee of `op` and the intrinsic it is, if any.
    pub fn of(&self, op: Op) -> Option<(&Operand, Option<Intrinsic>)> {
        self.seen.iter().find(|it| it.0 == op).map(|it| (&it.1, it.2))
    }

    /// Records `func` as the callee of `op`, unless one is known.
    pub fn note(&mut self, op: Op, func: &Operand, intrinsic: Option<Intrinsic>) {
        if self.of(op).is_none() {
            // clone: an operand naming a function, once per operation
            self.seen.push((op, func.clone(), intrinsic));
        }
    }
}

/// A lifted function.
#[derive(Debug)]
pub struct Lifted<'db> {
    pub graph: Graph,
    /// The result: one root for a scalar, one per element for a tuple or array, in order.
    pub roots: Vec<Id>,
    pub signature: Signature<'db>,
    pub callees: Callees,
}

/// A local's value in the symbolic run.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Val {
    /// A float: a node of sort float.
    Float(Id),
    /// A `bool`: a node of sort bool, [`Op::Bool`] when it is a constant.
    Cond(Id),
    /// An integer constant: its bits, its size in bytes (8 at most) and whether it is signed.
    Int { bits: u128, size: u8, signed: bool },
    /// A tuple or an array, by element.
    Tuple(Vec<Val>),
    /// An element of a tuple written field by field that no write has reached yet.
    Unset,
}

type State = Vec<Option<Val>>;

fn local_index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

/// The width of a float type the graph can be over.
fn width_of(ty: Ty<'_>) -> Option<Width> {
    match ty.kind() {
        TyKind::Float(FloatTy::F32) => Some(Width::F32),
        TyKind::Float(FloatTy::F64) => Some(Width::F64),
        _ => None,
    }
}

/// A place as a refusal names it: `_3`, `*_3`, `_3.0`.
fn place_text(place: &StoredPlace) -> String {
    use std::fmt::Write as _;
    // alloc: an error's text
    let mut text = format!("_{}", local_index(place.local));
    for elem in place.projection.as_slice() {
        match elem {
            ProjectionElem::Deref => text.insert(0, '*'),
            ProjectionElem::Field(field) => {
                let _ = write!(text, ".{}", field.0);
            }
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
                text.push_str("[..]")
            }
            _ => text.push_str(".."),
        }
    }
    text
}

/// The value of a constant operand of a scalar type, as its bits, and how many bytes it has
/// when the operand says.
fn constant_bits(operand: &Operand) -> Option<(u128, Option<usize>)> {
    match &operand.kind {
        OperandKind::Allocation { allocation } => {
            let allocation = allocation.as_ref();
            let bytes = &allocation.memory;
            if bytes.len() > 16 || !matches!(allocation.memory_map, MemoryMap::Empty) {
                return None;
            }
            let mut padded = [0u8; 16];
            padded[..bytes.len()].copy_from_slice(bytes);
            Some((u128::from_le_bytes(padded), Some(bytes.len())))
        }
        OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
            ConstKind::Value(value) => value.try_to_leaf().map(|it| (it.to_bits_unchecked(), None)),
            _ => None,
        },
        _ => None,
    }
}

/// An integer type's size in bytes, where the type fixes it at 8 or less, and whether it is
/// signed.
fn int_shape(ty: Ty<'_>) -> Option<(Option<u8>, bool)> {
    use rustc_type_ir::{IntTy, UintTy};
    Some(match ty.kind() {
        TyKind::Int(int) => (
            match int {
                IntTy::I8 => Some(1),
                IntTy::I16 => Some(2),
                IntTy::I32 => Some(4),
                IntTy::I64 => Some(8),
                _ => None,
            },
            true,
        ),
        TyKind::Uint(uint) => (
            match uint {
                UintTy::U8 => Some(1),
                UintTy::U16 => Some(2),
                UintTy::U32 => Some(4),
                UintTy::U64 => Some(8),
                _ => None,
            },
            false,
        ),
        _ => return None,
    })
}

/// `bits` kept to `size` bytes.
fn wrap(bits: u128, size: u8) -> u128 {
    bits & ((1u128 << (8 * u32::from(size))) - 1)
}

/// The number `bits` of `size` bytes is: sign-extended when `signed`.
fn number(bits: u128, size: u8, signed: bool) -> i128 {
    if signed {
        let shift = 128 - 8 * u32::from(size);
        ((bits << shift) as i128) >> shift
    } else {
        bits as i128
    }
}

/// Whether `value` is one a `size`-byte integer holds.
fn fits(value: i128, size: u8, signed: bool) -> bool {
    let bits = 8 * u32::from(size);
    if signed {
        value >= -(1i128 << (bits - 1)) && value < (1i128 << (bits - 1))
    } else {
        value >= 0 && value < (1i128 << bits)
    }
}

/// Whether `func` is an inherent method of `f32` or `f64` in `core` or `std`, and the
/// operation it is when [`METHODS`] names it.
fn float_method(db: &Db, func: FunctionId) -> Option<Op> {
    let origin = &func.krate(db).data(db).origin;
    if !matches!(origin, CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Std)) {
        return None;
    }
    let ItemContainerId::ImplId(imp) = func.lookup(db).container else { return None };
    if !matches!(db.impl_self_ty(imp).skip_binder().kind(), TyKind::Float(_)) {
        return None;
    }
    let name = FunctionSignature::of(db, func).name.as_str();
    METHODS.iter().find(|it| it.0 == name).map(|it| it.1)
}

struct Lifter<'a, 'db> {
    db: &'db Db,
    body: &'a MirBody<'db>,
    graph: Graph,
    callees: Callees,
    steps: u32,
}

impl<'db> Lifter<'_, 'db> {
    fn type_error(&self, ty: Ty<'db>, span: MirSpan) -> NotLifted {
        // alloc: an error's text
        NotLifted::Type { ty: format!("{ty:?}"), span }
    }

    fn memory(&self, place: &StoredPlace, what: &str, span: MirSpan) -> NotLifted {
        // alloc: an error's text
        NotLifted::Memory { place: format!("{what} `{}`", place_text(place)), span }
    }

    fn operation(&self, what: impl Into<String>, span: MirSpan) -> NotLifted {
        NotLifted::Operation { what: what.into(), span }
    }

    fn is_const(&self, cond: Id) -> Option<bool> {
        let node = self.graph.node(cond);
        (node.op == Op::Bool).then_some(node.payload != 0)
    }

    fn cond(&mut self, value: bool) -> Val {
        Val::Cond(self.graph.bool(value))
    }

    fn read(&self, place: &StoredPlace, state: &State, span: MirSpan) -> Result<Val, NotLifted> {
        let Some(mut value) = state[local_index(place.local)].as_ref() else {
            return Err(self.memory(
                place,
                "a read of what the function did not compute, at",
                span,
            ));
        };
        for elem in place.projection.as_slice() {
            let index = match *elem {
                ProjectionElem::Field(field) => field.0 as usize,
                ProjectionElem::ConstantIndex { offset, from_end: false, .. } => offset as usize,
                ProjectionElem::Index(local) => match state[local_index(local)] {
                    Some(Val::Int { bits, .. }) => bits as usize,
                    _ => {
                        return Err(self.memory(place, "an index that is not a constant, at", span));
                    }
                },
                _ => return Err(self.memory(place, "a read through", span)),
            };
            value = match value {
                Val::Tuple(elements) => match elements.get(index) {
                    Some(element) => element,
                    None => return Err(self.memory(place, "an index out of bounds, at", span)),
                },
                _ => return Err(self.memory(place, "a read through", span)),
            };
        }
        if *value == Val::Unset {
            return Err(self.memory(
                place,
                "a read of what the function did not compute, at",
                span,
            ));
        }
        // clone: a node id, or a small tuple of them
        Ok(value.clone())
    }

    fn write(
        &self,
        place: &StoredPlace,
        value: Val,
        state: &mut State,
        span: MirSpan,
    ) -> Result<(), NotLifted> {
        let slot = &mut state[local_index(place.local)];
        match place.projection.as_slice() {
            [] => *slot = Some(value),
            // A tuple built field by field, as deaggregated MIR builds one.
            [ProjectionElem::Field(field)] => {
                let field = field.0 as usize;
                match slot.get_or_insert_with(|| Val::Tuple(Vec::new())) {
                    Val::Tuple(elements) if field < MAX_FIELDS => {
                        if elements.len() <= field {
                            elements.resize(field + 1, Val::Unset);
                        }
                        elements[field] = value;
                    }
                    _ => return Err(self.memory(place, "a write through", span)),
                }
            }
            _ => return Err(self.memory(place, "a write through", span)),
        }
        Ok(())
    }

    fn constant(
        &mut self,
        operand: &Operand,
        ty: Ty<'db>,
        span: MirSpan,
    ) -> Result<Val, NotLifted> {
        let Some((bits, bytes)) = constant_bits(operand) else {
            return Err(self.type_error(ty, span));
        };
        match ty.kind() {
            TyKind::Float(_) if width_of(ty) == Some(self.graph.width()) => {
                Ok(Val::Float(self.graph.const_bits(bits as u64)))
            }
            TyKind::Bool => Ok(self.cond(bits != 0)),
            TyKind::Int(_) | TyKind::Uint(_) => {
                let (size, signed) = int_shape(ty).expect("an integer type");
                // An allocation says its size, which is the type's on this target.
                let size = match (bytes, size) {
                    (Some(bytes @ 1..=8), _) => bytes as u8,
                    (None, Some(size)) => size,
                    _ => return Err(self.type_error(ty, span)),
                };
                Ok(Val::Int { bits: wrap(bits, size), size, signed })
            }
            // `()`: a call's or a block's value that nothing reads.
            TyKind::Tuple(_) if bytes == Some(0) => Ok(Val::Tuple(Vec::new())),
            _ => Err(self.type_error(ty, span)),
        }
    }

    fn operand(
        &mut self,
        operand: &Operand,
        state: &State,
        span: MirSpan,
    ) -> Result<Val, NotLifted> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.read(place, state, span),
            OperandKind::Constant { ty, .. } => self.constant(operand, ty.as_ref(), span),
            OperandKind::Allocation { allocation } => {
                self.constant(operand, allocation.as_ref().ty, span)
            }
            OperandKind::Static(_) => {
                // alloc: an error's text
                Err(NotLifted::Memory { place: "a static".to_owned(), span })
            }
        }
    }

    fn float(&mut self, operand: &Operand, state: &State, span: MirSpan) -> Result<Id, NotLifted> {
        match self.operand(operand, state, span)? {
            Val::Float(id) => Ok(id),
            other => Err(self.operation(format!("a float operation on {other:?}"), span)),
        }
    }

    fn binary(&mut self, op: &BinOp, a: Val, b: Val, span: MirSpan) -> Result<Val, NotLifted> {
        match (a, b) {
            (Val::Float(a), Val::Float(b)) => {
                let op = match op {
                    BinOp::Add => Op::Add,
                    BinOp::Sub => Op::Sub,
                    BinOp::Mul => Op::Mul,
                    BinOp::Div => Op::Div,
                    BinOp::Lt => Op::Lt,
                    BinOp::Le => Op::Le,
                    BinOp::Gt => Op::Gt,
                    BinOp::Ge => Op::Ge,
                    BinOp::Eq => Op::Eq,
                    BinOp::Ne => Op::Ne,
                    other => return Err(self.operation(format!("`{other}` on floats"), span)),
                };
                let id = self.graph.binary(op, a, b);
                Ok(if op.sort() == blaze_diff::Sort::Bool { Val::Cond(id) } else { Val::Float(id) })
            }
            (Val::Cond(a), Val::Cond(b)) => {
                let folded = self.is_const(a).zip(self.is_const(b));
                match (op, folded) {
                    (BinOp::BitAnd, Some((a, b))) => Ok(self.cond(a & b)),
                    (BinOp::BitOr, Some((a, b))) => Ok(self.cond(a | b)),
                    (BinOp::BitXor | BinOp::Ne, Some((a, b))) => Ok(self.cond(a != b)),
                    (BinOp::Eq, Some((a, b))) => Ok(self.cond(a == b)),
                    (BinOp::BitAnd, None) => Ok(Val::Cond(self.graph.binary(Op::And, a, b))),
                    (BinOp::BitOr, None) => Ok(Val::Cond(self.graph.binary(Op::Or, a, b))),
                    (other, _) => Err(self.operation(format!("`{other}` on conditions"), span)),
                }
            }
            (
                Val::Int { bits: a, size, signed },
                Val::Int { bits: b, size: b_size, signed: b_signed },
            ) => {
                if size != b_size || signed != b_signed {
                    return Err(self.operation(format!("`{op}` on integers of two types"), span));
                }
                let (x, y) = (number(a, size, signed), number(b, size, signed));
                let int = |bits: u128| Val::Int { bits: wrap(bits, size), size, signed };
                // The exact result, `None` when it does not fit an `i128` (then it overflows).
                let checked = |exact: Option<i128>, wrapped: u128, lifter: &mut Self| {
                    let overflowed = exact.is_none_or(|it| !fits(it, size, signed));
                    Val::Tuple(vec![int(wrapped), lifter.cond(overflowed)]) // alloc: a pair
                };
                Ok(match op {
                    BinOp::Add => int(a.wrapping_add(b)),
                    BinOp::Sub => int(a.wrapping_sub(b)),
                    BinOp::Mul => int(a.wrapping_mul(b)),
                    BinOp::AddWithOverflow => checked(x.checked_add(y), a.wrapping_add(b), self),
                    BinOp::SubWithOverflow => checked(x.checked_sub(y), a.wrapping_sub(b), self),
                    BinOp::MulWithOverflow => checked(x.checked_mul(y), a.wrapping_mul(b), self),
                    BinOp::BitAnd => int(a & b),
                    BinOp::BitOr => int(a | b),
                    BinOp::BitXor => int(a ^ b),
                    BinOp::Lt => self.cond(x < y),
                    BinOp::Le => self.cond(x <= y),
                    BinOp::Gt => self.cond(x > y),
                    BinOp::Ge => self.cond(x >= y),
                    BinOp::Eq => self.cond(x == y),
                    BinOp::Ne => self.cond(x != y),
                    BinOp::Div | BinOp::Rem if y != 0 && !(signed && y == -1) => {
                        let exact = if matches!(op, BinOp::Div) { x / y } else { x % y };
                        int(exact as u128)
                    }
                    other => return Err(self.operation(format!("`{other}` on integers"), span)),
                })
            }
            (a, b) => Err(self.operation(format!("`{op}` on {a:?} and {b:?}"), span)),
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue, state: &State, span: MirSpan) -> Result<Val, NotLifted> {
        match rvalue {
            Rvalue::Use(operand) => self.operand(operand, state, span),
            Rvalue::BinaryOp(op, a, b) | Rvalue::CheckedBinaryOp(op, a, b) => {
                let (a, b) = (self.operand(a, state, span)?, self.operand(b, state, span)?);
                self.binary(op, a, b, span)
            }
            Rvalue::UnaryOp(op, operand) => match (op, self.operand(operand, state, span)?) {
                (UnOp::Neg, Val::Float(a)) => Ok(Val::Float(self.graph.unary(Op::Neg, a))),
                (UnOp::Neg, Val::Int { bits, size, signed }) => {
                    Ok(Val::Int { bits: wrap(bits.wrapping_neg(), size), size, signed })
                }
                (UnOp::Not, Val::Int { bits, size, signed }) => {
                    Ok(Val::Int { bits: wrap(!bits, size), size, signed })
                }
                (UnOp::Not, Val::Cond(a)) => Ok(match self.is_const(a) {
                    Some(value) => self.cond(!value),
                    None => Val::Cond(self.graph.unary(Op::Not, a)),
                }),
                (op, other) => Err(self.operation(format!("`{op:?}` on {other:?}"), span)),
            },
            Rvalue::Cast(kind, operand, ty) => {
                let ty = ty.as_ref();
                match (kind, self.operand(operand, state, span)?) {
                    (CastKind::IntToFloat, Val::Int { bits, size, signed })
                        if width_of(ty) == Some(self.graph.width()) =>
                    {
                        // Rounded once, to the width asked: as the cast does.
                        let value = number(bits, size, signed);
                        let payload = match self.graph.width() {
                            Width::F32 => u64::from((value as f32).to_bits()),
                            Width::F64 => (value as f64).to_bits(),
                        };
                        Ok(Val::Float(self.graph.const_bits(payload)))
                    }
                    (CastKind::IntToInt, Val::Int { bits, size, signed }) => match int_shape(ty) {
                        Some((Some(to), to_signed)) => {
                            let value = number(bits, size, signed);
                            Ok(Val::Int {
                                bits: wrap(value as u128, to),
                                size: to,
                                signed: to_signed,
                            })
                        }
                        _ => Err(self.type_error(ty, span)),
                    },
                    (CastKind::FloatToFloat, Val::Float(a))
                        if width_of(ty) == Some(self.graph.width()) =>
                    {
                        Ok(Val::Float(a))
                    }
                    (kind, _) => {
                        Err(self.operation(format!("a `{kind:?}` cast to `{ty:?}`"), span))
                    }
                }
            }
            Rvalue::Aggregate(AggregateKind::Tuple(_) | AggregateKind::Array(_), operands) => {
                // alloc: the elements of one small tuple or array
                let mut elements = Vec::with_capacity(operands.len());
                for operand in operands.iter() {
                    elements.push(self.operand(operand, state, span)?);
                }
                Ok(Val::Tuple(elements))
            }
            Rvalue::Aggregate(..) => Err(self.operation("a struct, enum or closure value", span)),
            Rvalue::Ref(_, place) => Err(self.memory(place, "a reference to", span)),
            Rvalue::Len(place) => Err(self.memory(place, "the length of", span)),
            Rvalue::Discriminant(place) => Err(self.memory(place, "the discriminant of", span)),
            Rvalue::CopyForDeref(place) => Err(self.memory(place, "a read through", span)),
            Rvalue::Repeat(..) => Err(self.operation("an array repeat expression", span)),
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn statement(&mut self, statement: &Statement, state: &mut State) -> Result<(), NotLifted> {
        match &statement.kind {
            StatementKind::Assign(place, rvalue) => {
                let value = self.rvalue(rvalue, state, statement.span)?;
                self.write(place, value, state, statement.span)
            }
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::FakeRead(_)
            | StatementKind::Deinit(_)
            | StatementKind::Nop => Ok(()),
            StatementKind::InlineAsm(_) => Err(self.operation("inline assembly", statement.span)),
        }
    }

    /// The operation a call's callee is.
    fn callee(
        &self,
        func: &Operand,
        intrinsic: Option<Intrinsic>,
        span: MirSpan,
    ) -> Result<Op, NotLifted> {
        // alloc: an error's text, on the refusal's path alone
        let refused = |callee: String| NotLifted::Call { callee, span };
        if let Some(intrinsic) = intrinsic {
            return intrinsic_op(intrinsic)
                .ok_or_else(|| refused(format!("the intrinsic `{intrinsic}`")));
        }
        let Some((callee, _)) = func.fn_item() else {
            return Err(refused("a function pointer, a closure or a constructor".to_owned()));
        };
        float_method(self.db, callee).ok_or_else(|| {
            refused(format!("`{}`", FunctionSignature::of(self.db, callee).name.as_str()))
        })
    }

    fn call(
        &mut self,
        op: Op,
        args: &[Operand],
        state: &State,
        span: MirSpan,
    ) -> Result<Val, NotLifted> {
        let arity = if op == Op::Powi { 2 } else { op.arity() };
        if args.len() != arity {
            return Err(
                self.operation(format!("a call of `{op}` with {} arguments", args.len()), span)
            );
        }
        let a = self.float(&args[0], state, span)?;
        Ok(Val::Float(match op {
            Op::Powi => match self.operand(&args[1], state, span)? {
                Val::Int { bits, size, signed } => {
                    let exponent = number(bits, size, signed);
                    let Ok(exponent) = i32::try_from(exponent) else {
                        return Err(self.operation("a `powi` exponent that is not an `i32`", span));
                    };
                    self.graph.powi(a, exponent)
                }
                _ => {
                    // alloc: an error's text
                    let callee = "`powi` with an exponent that is not a constant".to_owned();
                    return Err(NotLifted::Call { callee, span });
                }
            },
            Op::MulAdd => {
                let (b, c) =
                    (self.float(&args[1], state, span)?, self.float(&args[2], state, span)?);
                self.graph.mul_add(a, b, c)
            }
            _ if arity == 2 => {
                let b = self.float(&args[1], state, span)?;
                self.graph.binary(op, a, b)
            }
            _ => self.graph.unary(op, a),
        }))
    }

    /// Whether `to` is reached from `from` by one edge or more.
    fn reaches(&self, from: BasicBlockId, to: BasicBlockId) -> bool {
        // alloc: one flag per block and the walk's stack, on a data-dependent branch alone
        let mut seen = vec![false; self.body.basic_blocks.len()];
        let mut stack = vec![from];
        // Bounded by the blocks: each is pushed once.
        while let Some(block) = stack.pop() {
            let Some(terminator) = &self.body.basic_blocks[block].terminator else { continue };
            for next in terminator.kind.successors() {
                if next == to {
                    return true;
                }
                if !std::mem::replace(&mut seen[block_index(next)], true) {
                    stack.push(next);
                }
            }
        }
        false
    }

    /// `if cond { a } else { b }` of two results.
    fn join(&mut self, cond: Id, a: Val, b: Val, span: MirSpan) -> Result<Val, NotLifted> {
        let differ = NotLifted::Branch { span, why: BranchWhy::ArmsDiffer };
        match (a, b) {
            (Val::Float(a), Val::Float(b)) => Ok(Val::Float(self.graph.select(cond, a, b))),
            (Val::Cond(a), Val::Cond(b)) => {
                // (cond and a) or (not cond and b)
                let not = self.graph.unary(Op::Not, cond);
                let (then, otherwise) =
                    (self.graph.binary(Op::And, cond, a), self.graph.binary(Op::And, not, b));
                Ok(Val::Cond(self.graph.binary(Op::Or, then, otherwise)))
            }
            (a @ Val::Int { .. }, b @ Val::Int { .. }) if a == b => Ok(a),
            (Val::Tuple(a), Val::Tuple(b)) if a.len() == b.len() => {
                // alloc: the elements of one small tuple
                let mut elements = Vec::with_capacity(a.len());
                for (a, b) in a.into_iter().zip(b) {
                    elements.push(self.join(cond, a, b, span)?);
                }
                Ok(Val::Tuple(elements))
            }
            _ => Err(differ),
        }
    }

    /// Runs the body from `block` to its return, with the locals as `state`. `visits` counts
    /// how often this path entered each block.
    fn run(
        &mut self,
        mut block: BasicBlockId,
        mut state: State,
        mut visits: Vec<u32>,
    ) -> Result<Val, NotLifted> {
        let body = self.body;
        // Bounded: each turn counts a step against `MAX_STEPS`.
        loop {
            let data = &body.basic_blocks[block];
            let span = data.terminator.as_ref().map_or(MirSpan::Unknown, |it| it.span);
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return Err(NotLifted::Branch { span, why: BranchWhy::TooManyPaths });
            }
            let entered = &mut visits[block_index(block)];
            *entered += 1;
            if *entered > MAX_UNROLL {
                return Err(NotLifted::Loop { span, turns: Some(MAX_UNROLL) });
            }
            for statement in &data.statements {
                self.statement(statement, &mut state)?;
            }
            let Some(terminator) = &data.terminator else {
                return Err(self.operation("a block without a terminator", span));
            };
            match &terminator.kind {
                TerminatorKind::Goto { target }
                | TerminatorKind::FalseEdge { real_target: target, .. }
                | TerminatorKind::FalseUnwind { real_target: target, .. }
                // Every value here is a scalar or a tuple of scalars: nothing to drop.
                | TerminatorKind::Drop { target, .. } => block = *target,
                TerminatorKind::Return => {
                    return match state[local_index(return_slot())].take() {
                        Some(value) => Ok(value),
                        None => Err(self.operation("a return without a value", span)),
                    };
                }
                TerminatorKind::Call { func, args, destination, target, intrinsic, .. } => {
                    let op = self.callee(func, *intrinsic, span)?;
                    let value = self.call(op, args, &state, span)?;
                    self.callees.note(op, func, *intrinsic);
                    self.write(destination, value, &mut state, span)?;
                    let Some(target) = target else {
                        return Err(self.operation("a call that does not return", span));
                    };
                    block = *target;
                }
                TerminatorKind::Assert { cond, expected, target, .. } => {
                    let holds = match self.operand(cond, &state, span)? {
                        Val::Cond(cond) => self.is_const(cond),
                        _ => None,
                    };
                    // A check that holds on every path is no branch; any other can panic.
                    if holds != Some(*expected) {
                        return Err(NotLifted::Branch { span, why: BranchWhy::Panics });
                    }
                    block = *target;
                }
                TerminatorKind::SwitchInt { discr, targets } => {
                    let cond = match self.operand(discr, &state, span)? {
                        Val::Int { bits, .. } => {
                            block = targets.target_for_value(bits);
                            continue;
                        }
                        Val::Cond(cond) => cond,
                        _ => {
                            return Err(NotLifted::Branch { span, why: BranchWhy::NotACondition });
                        }
                    };
                    if let Some(value) = self.is_const(cond) {
                        block = targets.target_for_value(u128::from(value));
                        continue;
                    }
                    let (then, otherwise) = (targets.target_for_value(1), targets.target_for_value(0));
                    if then == otherwise {
                        block = then;
                        continue;
                    }
                    // A test in a loop that one way leaves the loop: the loop's exit, and it
                    // depends on a value.
                    if self.reaches(block, block)
                        && !(self.reaches(then, block) && self.reaches(otherwise, block))
                    {
                        return Err(NotLifted::Loop { span, turns: None });
                    }
                    // clone: the locals and the visit counts, once per data-dependent branch
                    let taken = self.run(then, state.clone(), visits.clone())?;
                    let not_taken = self.run(otherwise, state, visits)?;
                    return self.join(cond, taken, not_taken, span);
                }
                TerminatorKind::TailCall { .. } => {
                    // alloc: an error's text
                    return Err(NotLifted::Call { callee: "a tail call (`become`)".to_owned(), span });
                }
                TerminatorKind::Unreachable | TerminatorKind::Abort | TerminatorKind::UnwindResume => {
                    return Err(NotLifted::Branch { span, why: BranchWhy::Panics });
                }
                _ => return Err(self.operation("a terminator of a coroutine or of built MIR", span)),
            }
        }
    }
}

/// The expression graph of `body`, a monomorphized runtime body of a function over scalar
/// floats of one width: variable `i` is parameter `i`, and the roots are its result.
pub fn lift<'db>(db: &'db Db, body: &MirBody<'db>) -> Result<Lifted<'db>, NotLifted> {
    let ty_of = |local: LocalId| body.locals[local].ty.as_ref();
    // The width: the first float among the parameters, the result and then the locals.
    let float = body
        .param_locals
        .iter()
        .copied()
        .chain([return_slot()])
        .chain(body.locals.iter().map(|(local, _)| local))
        .map(ty_of)
        .find(|ty| width_of(*ty).is_some());
    let Some(float) = float else {
        // alloc: an error's text
        let ty = format!("{:?}", ty_of(return_slot()));
        return Err(NotLifted::Type { ty, span: MirSpan::Unknown });
    };
    let width = width_of(float).expect("found by its width");
    let mut lifter =
        Lifter { db, body, graph: Graph::new(width), callees: Callees::default(), steps: 0 };
    // alloc: one slot per local, the symbolic run's state
    let mut state: State = vec![None; body.locals.len()];
    for (index, &param) in body.param_locals.iter().enumerate() {
        let ty = ty_of(param);
        if width_of(ty) != Some(width) {
            return Err(lifter.type_error(ty, MirSpan::Unknown));
        }
        state[local_index(param)] = Some(Val::Float(lifter.graph.var(index as u32)));
    }
    // alloc: one count per block, the path's visits
    let visits = vec![0u32; body.basic_blocks.len()];
    let result = lifter.run(body.start_block, state, visits)?;
    // alloc: one root per scalar of the result
    let mut roots = Vec::new();
    match result {
        Val::Float(id) => roots.push(id),
        Val::Tuple(elements) if !elements.is_empty() => {
            for element in elements {
                match element {
                    Val::Float(id) => roots.push(id),
                    _ => return Err(lifter.type_error(ty_of(return_slot()), MirSpan::Unknown)),
                }
            }
        }
        _ => return Err(lifter.type_error(ty_of(return_slot()), MirSpan::Unknown)),
    }
    let signature = Signature {
        owner: body.owner,
        float: float.store(),
        params: body.param_locals.len() as u32,
        ret: ty_of(return_slot()).store(),
    };
    Ok(Lifted { graph: lifter.graph, roots, signature, callees: lifter.callees })
}
