//! Lowering: an expression graph as a runtime MIR body the normal pipeline compiles.
//!
//! Each node the roots reach is computed once, in id order (arguments first), into a local of
//! its own: the graph's sharing is the body's. An operator is a `BinaryOp` or `UnaryOp`; a
//! function is a `Call` of the same intrinsic or std method the lifted body called
//! ([`Callees`]), in a block of its own as every call is; a `select` is a `SwitchInt` over two
//! blocks that each assign the local. Both arms' values are computed before the switch: every
//! node is pure and total (a float operation never traps), so that is the same function.
//!
//! The body has no storage statements (its locals live for the whole call), no unwind edges
//! (the calls are of float functions that do not unwind) and is in [`MirPhase::Runtime`].

use std::alloc::Global;

use base_db::Db;
use blaze_diff::{Graph, Id, Node, Op, Sort, Width};
use blaze_hash::HashMap;
use hir_ty::next_solver::{DbInterner, Ty, TyKind};
use hir_ty_mir::mir::{
    AggregateKind, BasicBlock, BasicBlockId, BinOp, Local, LocalId, MirBody, MirPhase, MirSpan,
    Operand, OperandKind, Place, Rvalue, Statement, StatementKind, SwitchTargets, Terminator,
    TerminatorKind, UnOp,
};
use la_arena::{Arena, ArenaMap};
use rustc_type_ir::{
    IntTy,
    inherent::{IntoKind, Ty as _},
};

use crate::lift::{Callees, Signature};

/// Why a graph did not lower.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotLowered {
    /// The graph is over one float width and the signature's float type is another.
    Width { graph: Width },
    /// The expression names variable `index`, and the signature has `params` parameters.
    NoSuchParam { index: u32, params: u32 },
    /// No function is known to call for `op`: the lifted body did not call one, and none was
    /// resolved.
    NoCallee { op: Op },
    /// `roots` roots do not make a value of the return type `ty`: one root is a float, several
    /// are a tuple or an array.
    Return { ty: String, roots: usize },
}

impl std::fmt::Display for NotLowered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotLowered::Width { graph } => {
                write!(
                    f,
                    "the expression is over {}, and the function's floats are not",
                    graph.name()
                )
            }
            NotLowered::NoSuchParam { index, params } => {
                write!(f, "the expression names parameter {index}, and the function has {params}")
            }
            NotLowered::NoCallee { op } => {
                write!(
                    f,
                    "no function is known to call for `{op}`: `core::intrinsics` has none here"
                )
            }
            NotLowered::Return { ty, roots } => {
                write!(f, "{roots} results do not make a value of the return type `{ty}`")
            }
        }
    }
}

impl std::error::Error for NotLowered {}

struct Builder {
    blocks: Arena<BasicBlock>,
    locals: Arena<Local>,
    current: BasicBlockId,
}

impl Builder {
    fn local(&mut self, ty: Ty<'_>) -> LocalId {
        self.locals.alloc(Local { ty: ty.store() })
    }

    fn assign(&mut self, local: LocalId, rvalue: Rvalue) {
        let kind = StatementKind::Assign(Place::from(local).store(), rvalue);
        self.blocks[self.current].statements.push(Statement { kind, span: MirSpan::Unknown });
    }

    /// Ends the current block with `kind` and makes `next` the current one.
    fn end(&mut self, kind: TerminatorKind, next: BasicBlockId) {
        self.blocks[self.current].terminator = Some(Terminator { span: MirSpan::Unknown, kind });
        self.current = next;
    }

    fn block(&mut self) -> BasicBlockId {
        self.blocks.alloc(BasicBlock::default())
    }
}

fn copy(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Copy(Place::from(local).store()), span: None }
}

/// The body that computes `roots` of `graph` from the parameters of `signature`: variable `i`
/// is parameter `i`. One root is returned as it is; several as the tuple or array the
/// signature's return type is. `callees` names the function to call for each operation that
/// is one.
pub fn lower<'db>(
    db: &'db Db,
    graph: &Graph,
    roots: &[Id],
    signature: &Signature<'db>,
    callees: &Callees,
) -> Result<MirBody<'db>, NotLowered> {
    let interner = DbInterner::new_no_crate(db);
    let float: Ty<'db> = signature.float.as_ref();
    let ret: Ty<'db> = signature.ret.as_ref();
    let (bool_ty, i32_ty) = (Ty::new_bool(interner), Ty::new_int(interner, IntTy::I32));
    let float_width = match float.kind() {
        TyKind::Float(rustc_type_ir::FloatTy::F32) => Some(Width::F32),
        TyKind::Float(rustc_type_ir::FloatTy::F64) => Some(Width::F64),
        _ => None,
    };
    if float_width != Some(graph.width()) {
        return Err(NotLowered::Width { graph: graph.width() });
    }

    let mut blocks = Arena::new();
    let start = blocks.alloc(BasicBlock::default());
    let mut builder = Builder { blocks, locals: Arena::new(), current: start };
    let return_local = builder.local(ret);
    // alloc: the body's parameter list, a few locals
    let params: Vec<LocalId> = (0..signature.params).map(|_| builder.local(float)).collect();

    let reached = graph.reached(roots);
    // alloc: each node's operand, scratch of this lowering
    let mut operands: Vec<Option<Operand>> = vec![None; graph.len()];
    for (at, node) in graph.nodes().iter().enumerate() {
        if !reached[at] {
            continue;
        }
        let of = |arg: Id| -> Operand {
            // clone: an operand naming a local or a constant
            operands[arg.index()].clone().expect("an argument's id is below its user's")
        };
        let operand = match node.op {
            Op::Const => {
                let bytes = node.payload.to_le_bytes();
                let size = if graph.width() == Width::F32 { 4 } else { 8 };
                Operand::from_scalar_bytes(&bytes[..size], float)
            }
            Op::Bool => Operand::from_scalar_bytes(&[u8::from(node.payload != 0)], bool_ty),
            Op::Var => {
                let index = node.payload as u32;
                match params.get(index as usize) {
                    Some(&param) => copy(param),
                    None => {
                        return Err(NotLowered::NoSuchParam { index, params: signature.params });
                    }
                }
            }
            _ => {
                let ty = if node.op.sort() == Sort::Bool { bool_ty } else { float };
                let local = builder.local(ty);
                emit(&mut builder, node, local, &of, i32_ty, callees)?;
                copy(local)
            }
        };
        operands[at] = Some(operand);
    }

    // clone: an operand naming a local or a constant
    let result = |root: &Id| operands[root.index()].clone().expect("a root is reached");
    let value = match (roots, ret.kind()) {
        ([root], TyKind::Float(_)) => Rvalue::Use(result(root)),
        ([_, ..], TyKind::Tuple(_)) => {
            Rvalue::Aggregate(AggregateKind::Tuple(ret.store()), roots.iter().map(result).collect())
        }
        ([_, ..], TyKind::Array(element, _)) => Rvalue::Aggregate(
            AggregateKind::Array(element.store()),
            roots.iter().map(result).collect(),
        ),
        // alloc: an error's text
        _ => return Err(NotLowered::Return { ty: format!("{ret:?}"), roots: roots.len() }),
    };
    builder.assign(return_local, value);
    let current = builder.current;
    builder.end(TerminatorKind::Return, current);

    Ok(MirBody {
        basic_blocks: builder.blocks,
        locals: builder.locals,
        start_block: start,
        owner: signature.owner,
        binding_locals: ArenaMap::new(),
        upvar_locals: HashMap::new_in(Global),
        param_locals: params,
        // alloc: none, empty lists
        promoted_locals: Vec::new(),
        closures: Vec::new(),
        coroutine: None,
        unreached: Vec::new(),
        scope_ends: Vec::new(),
        inherits_overflow_checks: false,
        phase: MirPhase::Runtime,
        upvars_in_place: Vec::new(),
        panic_unwinds: Vec::new(),
        promoted_constants: Vec::new(),
        unreachable_after_calls: Vec::new(),
        guard_refs: Vec::new(),
        capture_copies: Vec::new(),
        inlined: Default::default(),
    })
}

/// Computes `node` into `local`.
fn emit(
    builder: &mut Builder,
    node: &Node,
    local: LocalId,
    of: &dyn Fn(Id) -> Operand,
    i32_ty: Ty<'_>,
    callees: &Callees,
) -> Result<(), NotLowered> {
    let [a, b, c] = node.args;
    let operator = match node.op {
        Op::Add => Some(BinOp::Add),
        Op::Sub => Some(BinOp::Sub),
        Op::Mul => Some(BinOp::Mul),
        Op::Div => Some(BinOp::Div),
        Op::Lt => Some(BinOp::Lt),
        Op::Le => Some(BinOp::Le),
        Op::Gt => Some(BinOp::Gt),
        Op::Ge => Some(BinOp::Ge),
        Op::Eq => Some(BinOp::Eq),
        Op::Ne => Some(BinOp::Ne),
        // The operands are `bool`s, already computed: `&` and `|`.
        Op::And => Some(BinOp::BitAnd),
        Op::Or => Some(BinOp::BitOr),
        _ => None,
    };
    if let Some(operator) = operator {
        builder.assign(local, Rvalue::BinaryOp(operator, of(a), of(b)));
        return Ok(());
    }
    match node.op {
        Op::Neg => builder.assign(local, Rvalue::UnaryOp(UnOp::Neg, of(a))),
        Op::Not => builder.assign(local, Rvalue::UnaryOp(UnOp::Not, of(a))),
        Op::Select => {
            let (then, otherwise, after) = (builder.block(), builder.block(), builder.block());
            let targets = SwitchTargets::static_if(0, otherwise, then);
            builder.end(TerminatorKind::SwitchInt { discr: of(a), targets }, then);
            builder.assign(local, Rvalue::Use(of(b)));
            builder.end(TerminatorKind::Goto { target: after }, otherwise);
            builder.assign(local, Rvalue::Use(of(c)));
            builder.end(TerminatorKind::Goto { target: after }, after);
        }
        op => {
            let Some((func, intrinsic)) = callees.of(op) else {
                return Err(NotLowered::NoCallee { op });
            };
            // alloc: the call's argument list
            let mut args: Vec<Operand> = node.args().iter().map(|&arg| of(arg)).collect();
            if op == Op::Powi {
                args.push(Operand::from_scalar_bytes(&node.exponent().to_le_bytes(), i32_ty));
            }
            let after = builder.block();
            let call = TerminatorKind::Call {
                // clone: an operand naming a function
                func: func.clone(),
                args: args.into_boxed_slice(),
                destination: Place::from(local).store(),
                target: Some(after),
                cleanup: None,
                from_hir_call: true,
                intrinsic,
            };
            builder.end(call, after);
        }
    }
    Ok(())
}
