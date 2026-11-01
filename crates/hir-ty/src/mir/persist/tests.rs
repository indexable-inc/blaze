//! A body holding every kind of statement, terminator, rvalue, operand, projection, constant
//! (scalars, slices, a reference into a memory map that names a static, a zero-sized value, a
//! function item), a closure, a coroutine's locals, drop and unwind paths and an `asm!` block
//! reads back equal; equal bodies write equal bytes; damaged words are errors, never panics; a
//! read allocates only the body it makes.
//!
//! hir-ty's test binary links no MIR lowering, so the body is built by hand. Its ids are this
//! database's own (no relocating scope); the same body written from two databases is
//! `body_store::mir`'s test, inside the item store's scope.

use std::panic::{AssertUnwindSafe, catch_unwind};

use base_db::{Db, all_crates};
use blaze_alloc_count::measure;
use blaze_format::{Aligned, FormatError, open_as};
use blaze_hash::HashMap;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumVariantId, ExpressionStoreOwnerId, FieldId, FunctionId,
    ModuleDefId, StaticId, StructId, UnionId, VariantId,
    expr_store::ExpressionStore,
    hir::{AsmOptions, AsmPiece, ClosureKind, Expr, InlineAsmRegOrRegClass},
    nameres::crate_def_map,
};
use hir_expand::fixture;
use la_arena::{Arena, ArenaMap};
use rustc_type_ir::inherent::Ty as _;
use smallvec::smallvec;

use super::{MirRoot, SCHEMA, idx, read, write};
use crate::{
    CallableDefId, ComplexMemoryMap, Extent, InferBodyId, MemoryMap, MemoryPieces, StaticMemory,
    closure_analysis::{self, PlaceBase, ProjectionKind},
    db::{InternedClosure, InternedClosureId},
    infer::PointerCast,
    intrinsic::Intrinsic,
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind,
        CastKind, CoroutineLocals, FieldIndex, InlineAsm, Inlined, InlinedBinding, InlinedBody,
        InlinedCallee, InlinedPromoted, InlinedScope, InlinedSpan, Local, LocalId, MirBody,
        MirPhase, MirSpan, MutBorrowKind, Operand, OperandKind, Place, PlaceElem, Projection,
        ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, SwitchTargets, Terminator,
        TerminatorKind, UnOp, UnreachableAfterCall,
    },
    next_solver::{ConstExt as _, DbInterner, GenericArgs, Ty, abi::Safety, default_types},
};

const FIXTURE: &str = r#"
//- minicore: sized, copy, fn
pub struct S { pub a: u32, pub b: u8 }
pub enum E { A = 1, B }
pub union U { pub x: u32, pub y: f32 }
pub static ST: u32 = 7;
pub const K: u32 = 3;
pub fn callee(x: u32) -> u32 { x }
pub fn owner(s: S) -> u32 { let c = |y: u32| y + s.a; c(1) }
"#;

fn db() -> Db {
    let db = fixture::db(FIXTURE).into_db();
    db
}

/// The item of the fixture named `name`: its type, else its value.
fn item(db: &Db, name: &str) -> ModuleDefId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for (found, per_ns) in module.scope(db).entries() {
                if found.as_str() == name
                    && let Some(def) = per_ns.take_types().or(per_ns.take_values())
                {
                    return def;
                }
            }
        }
    }
    panic!("no item `{name}` in the fixture")
}

struct Items<'db> {
    owner: FunctionId,
    callee: FunctionId,
    s: StructId,
    variant: EnumVariantId,
    u: UnionId,
    st: StaticId,
    k: ConstId,
    closure: InternedClosureId<'db>,
}

fn items(db: &Db) -> Items<'_> {
    let ModuleDefId::FunctionId(owner) = item(db, "owner") else { panic!("`owner` is a function") };
    let ModuleDefId::FunctionId(callee) = item(db, "callee") else { panic!("`callee`") };
    let ModuleDefId::AdtId(AdtId::StructId(s)) = item(db, "S") else { panic!("`S`") };
    let ModuleDefId::AdtId(AdtId::EnumId(e)) = item(db, "E") else { panic!("`E`") };
    let ModuleDefId::AdtId(AdtId::UnionId(u)) = item(db, "U") else { panic!("`U`") };
    let ModuleDefId::StaticId(st) = item(db, "ST") else { panic!("`ST`") };
    let ModuleDefId::ConstId(k) = item(db, "K") else { panic!("`K`") };
    let Some(VariantId::EnumVariantId(variant)) = AdtId::EnumId(e).variants(db).next() else {
        panic!("`E::A`")
    };
    let def = DefWithBodyId::FunctionId(owner);
    let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(def));
    let expr = store
        .exprs()
        .find_map(|(id, expr)| matches!(expr, Expr::Closure { .. }).then_some(id))
        .expect("`owner` has a closure");
    let closure = InternedClosureId::new(
        db,
        InternedClosure { owner: InferBodyId::DefWithBodyId(def), expr, kind: ClosureKind::Closure },
    );
    Items { owner, callee, s, variant, u, st, k, closure }
}

fn place<'db>(local: LocalId, projection: &[PlaceElem]) -> StoredPlace {
    Place::<'db> { local, projection: Projection::new_from_slice(projection) }.store()
}

fn copy(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Copy(place(local, &[])), span: Some(MirSpan::ExprId(idx(0))) }
}

fn assign(local: LocalId, rvalue: Rvalue, span: MirSpan) -> Statement {
    Statement { kind: StatementKind::Assign(place(local, &[]), rvalue), span }
}

/// The body: every statement in block 0, each terminator kind ending a block of its own.
fn sample<'db>(db: &'db Db, it: &Items<'db>) -> MirBody<'db> {
    let interner = DbInterner::new_no_crate(db);
    let types = &default_types().types;
    let empty = default_types().empty.generic_args;
    let three = default_types().consts.u8_values[3];
    let s_ty = Ty::new_adt(interner, AdtId::StructId(it.s), empty);
    let fn_ty = Ty::new_fn_def(interner, CallableDefId::FunctionId(it.callee).into(), empty);
    let tuple = Ty::new_tup(interner, &[types.u32, types.bool]);
    let args = GenericArgs::new_from_slice(&[types.u32.into()]);

    let mut locals = Arena::new();
    let ret = locals.alloc(Local { ty: types.u32.store() });
    let s = locals.alloc(Local { ty: s_ty.store() });
    let f = locals.alloc(Local { ty: fn_ty.store() });
    let pair = locals.alloc(Local { ty: tuple.store() });
    let flag = locals.alloc(Local { ty: types.bool.store() });
    let bytes = locals.alloc(Local { ty: types.static_u8_slice.store() });
    let index = locals.alloc(Local { ty: types.usize.store() });
    let poll = locals.alloc(Local { ty: types.unit.store() });
    let context = locals.alloc(Local { ty: types.mut_unit_ptr.store() });

    let mut complex = ComplexMemoryMap {
        memory: MemoryPieces::from_columns(
            vec![9, 8, 7, 6],
            vec![0x2000],
            vec![Extent { start: 0, len: 4 }],
        ),
        statics: vec![StaticMemory { start: 0x3000, size: 4, id: it.st }],
        type_ids: vec![Some(types.u8), None],
        value_pointers: vec![0],
        ..ComplexMemoryMap::default()
    };
    complex.vtable.id(types.u32);
    let mut constants = vec![
        Operand::from_bytes(Box::new([1, 0, 0, 0]), types.u32),
        Operand::const_zst(types.unit),
        Operand::from_fn(db, it.callee, empty),
        Operand::from_concrete_const(
            Box::new([0; 16]),
            MemoryMap::Simple(Box::new([1, 2, 3])),
            types.static_u8_slice,
        ),
        Operand::from_concrete_const(
            Box::new([0, 0x20, 0, 0, 0, 0, 0, 0]),
            MemoryMap::Complex(Box::new(complex)),
            types.static_u8_slice,
        ),
        Operand { kind: OperandKind::Static(it.st), span: None },
        Operand {
            kind: OperandKind::Constant { konst: three.store(), ty: types.u8.store() },
            span: Some(MirSpan::PatId(idx(0))),
        },
    ];
    // The same allocation twice: written once, read back as one.
    constants.push(Operand::from_bytes(Box::new([1, 0, 0, 0]), types.u32));

    let spans = [
        MirSpan::ExprId(idx(1)),
        MirSpan::ExprEnd(idx(1)),
        MirSpan::PatId(idx(0)),
        MirSpan::BindingId(idx(0)),
        MirSpan::SelfParam,
        MirSpan::Unknown,
        MirSpan::Inlined(0),
    ];
    let projections: [PlaceElem; 6] = [
        ProjectionElem::Deref,
        ProjectionElem::Field(FieldIndex(1)),
        ProjectionElem::Index(index),
        ProjectionElem::ConstantIndex { offset: 1 << 40, min_length: 3, from_end: true },
        ProjectionElem::Subslice { from: 1, to: u64::MAX },
        ProjectionElem::Downcast(VariantId::EnumVariantId(it.variant)),
    ];

    let mut statements = Vec::new();
    for (n, constant) in constants.into_iter().enumerate() {
        statements.push(assign(ret, Rvalue::Use(constant), spans[n % spans.len()]));
    }
    statements.push(assign(
        ret,
        Rvalue::Use(Operand { kind: OperandKind::Move(place(s, &projections)), span: None }),
        MirSpan::Unknown,
    ));
    statements.push(assign(ret, Rvalue::Repeat(copy(ret), three.store()), MirSpan::Unknown));
    for kind in [
        BorrowKind::Shared,
        BorrowKind::Shallow,
        BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture },
        BorrowKind::Mut { kind: MutBorrowKind::Default },
        BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow },
    ] {
        statements.push(assign(ret, Rvalue::Ref(kind, place(s, &projections[..2])), MirSpan::Unknown));
    }
    statements.push(assign(ret, Rvalue::Len(place(bytes, &[ProjectionElem::Deref])), MirSpan::Unknown));
    for kind in [
        CastKind::PointerExposeAddress,
        CastKind::PointerFromExposedAddress,
        CastKind::PtrToPtr,
        CastKind::PointerCoercion(PointerCast::ReifyFnPointer),
        CastKind::PointerCoercion(PointerCast::UnsafeFnPointer),
        CastKind::PointerCoercion(PointerCast::ClosureFnPointer(Safety::Unsafe)),
        CastKind::PointerCoercion(PointerCast::ClosureFnPointer(Safety::Safe)),
        CastKind::PointerCoercion(PointerCast::MutToConstPointer),
        CastKind::PointerCoercion(PointerCast::ArrayToPointer),
        CastKind::PointerCoercion(PointerCast::Unsize),
        CastKind::DynStar,
        CastKind::IntToInt,
        CastKind::FloatToInt,
        CastKind::FloatToFloat,
        CastKind::IntToFloat,
        CastKind::FnPtrToPtr,
        CastKind::Transmute,
    ] {
        statements.push(assign(ret, Rvalue::Cast(kind, copy(f), types.u64.store()), MirSpan::Unknown));
    }
    for op in all_bin_ops() {
        statements.push(assign(ret, Rvalue::BinaryOp(op.clone(), copy(ret), copy(ret)), MirSpan::Unknown));
        statements.push(assign(pair, Rvalue::CheckedBinaryOp(op, copy(ret), copy(ret)), MirSpan::Unknown));
    }
    statements.push(assign(flag, Rvalue::UnaryOp(UnOp::Not, copy(flag)), MirSpan::Unknown));
    statements.push(assign(ret, Rvalue::UnaryOp(UnOp::Neg, copy(ret)), MirSpan::Unknown));
    statements.push(assign(ret, Rvalue::Discriminant(place(s, &[])), MirSpan::Unknown));
    for kind in [
        AggregateKind::Array(types.u32.store()),
        AggregateKind::Tuple(tuple.store()),
        AggregateKind::Adt(VariantId::StructId(it.s), args.store()),
        AggregateKind::Union(
            it.u,
            FieldId { parent: VariantId::UnionId(it.u), local_id: idx(1) },
        ),
        AggregateKind::Closure(fn_ty.store()),
    ] {
        let operands = vec![copy(ret), copy(flag)].into_boxed_slice();
        statements.push(assign(pair, Rvalue::Aggregate(kind, operands), MirSpan::Unknown));
    }
    statements.push(assign(ret, Rvalue::Aggregate(AggregateKind::Array(types.u32.store()), Box::new([])), MirSpan::Unknown));
    statements.push(assign(ret, Rvalue::CopyForDeref(place(bytes, &[ProjectionElem::Deref])), MirSpan::Unknown));
    for kind in [
        StatementKind::FakeRead(place(s, &[])),
        StatementKind::Deinit(place(s, &[])),
        StatementKind::StorageLive(index),
        StatementKind::StorageDead(index),
        StatementKind::Nop,
        StatementKind::InlineAsm(Box::new(InlineAsm {
            template: Box::new([
                AsmPiece::Lit("mov {0}, {1:e} // ünïcode".into()),
                AsmPiece::Operand { index: 0, modifier: None },
                AsmPiece::Operand { index: 1, modifier: Some('e') },
                AsmPiece::Lit("".into()),
            ]),
            operands: Box::new([
                AsmOperand::Reg {
                    reg: InlineAsmRegOrRegClass::Reg(intern::Symbol::intern("eax")),
                    input: Some(copy(ret)),
                    output: Some(place(ret, &[])),
                    written: true,
                },
                AsmOperand::Reg {
                    reg: InlineAsmRegOrRegClass::RegClass(intern::Symbol::intern("reg")),
                    input: None,
                    output: None,
                    written: false,
                },
                AsmOperand::Const(copy(ret)),
                AsmOperand::Label,
                AsmOperand::Sym(Operand::from_fn(db, it.callee, empty)),
            ]),
            options: AsmOptions::NOMEM | AsmOptions::NOSTACK | AsmOptions::MAY_UNWIND,
            taken: Some(place(index, &[])),
            naked: false,
            clobber_abi: true,
        })),
    ] {
        statements.push(Statement { kind, span: MirSpan::BindingId(idx(0)) });
    }

    let mut blocks: Vec<BasicBlock> = vec![BasicBlock { statements, terminator: None, is_cleanup: false }];
    let block = |n: u32| -> BasicBlockId { idx(n) };
    let terminators = vec![
        TerminatorKind::Goto { target: block(1) },
        TerminatorKind::SwitchInt {
            discr: copy(ret),
            targets: SwitchTargets {
                values: smallvec![0, u128::MAX, 1 << 100],
                targets: smallvec![block(2), block(3), block(4), block(5)],
            },
        },
        TerminatorKind::SwitchInt { discr: copy(flag), targets: SwitchTargets::static_if(1, block(2), block(3)) },
        TerminatorKind::UnwindResume,
        TerminatorKind::Abort,
        TerminatorKind::Return,
        TerminatorKind::Unreachable,
        TerminatorKind::Drop { place: place(s, &[]), target: block(1), unwind: Some(block(3)) },
        TerminatorKind::Drop { place: place(s, &[]), target: block(1), unwind: None },
        TerminatorKind::DropAndReplace {
            place: place(s, &[]),
            value: copy(s),
            target: block(1),
            unwind: Some(block(3)),
        },
        TerminatorKind::Call {
            func: Operand::from_fn(db, it.callee, empty),
            args: Box::new([copy(ret)]),
            destination: place(ret, &[]),
            target: Some(block(1)),
            cleanup: Some(block(3)),
            from_hir_call: true,
            intrinsic: None,
        },
        TerminatorKind::Call {
            func: copy(f),
            args: Box::new([]),
            destination: place(ret, &[]),
            target: None,
            cleanup: None,
            from_hir_call: false,
            intrinsic: Some(Intrinsic::from_index(0).expect("an intrinsic of index 0")),
        },
        TerminatorKind::TailCall {
            func: copy(f),
            args: Box::new([copy(ret), copy(ret)]),
            fn_span: MirSpan::ExprId(idx(2)),
        },
        TerminatorKind::Yield {
            value: copy(ret),
            resume: block(1),
            resume_arg: place(ret, &[]),
            drop: Some(block(3)),
        },
        TerminatorKind::Yield { value: copy(ret), resume: block(1), resume_arg: place(ret, &[]), drop: None },
        TerminatorKind::CoroutineDrop,
        TerminatorKind::FalseEdge { real_target: block(1), imaginary_target: block(2) },
        TerminatorKind::FalseUnwind { real_target: block(1), unwind: Some(block(3)) },
        TerminatorKind::FalseUnwind { real_target: block(1), unwind: None },
    ];
    let asserts = [
        AssertKind::BoundsCheck { len: copy(index), index: copy(index) },
        AssertKind::Overflow(BinOp::Add, copy(ret), copy(ret)),
        AssertKind::OverflowNeg(copy(ret)),
        AssertKind::DivisionByZero(copy(ret)),
        AssertKind::RemainderByZero(copy(ret)),
        AssertKind::MisalignedPointerDereference { required: copy(index), found: copy(index) },
        AssertKind::NullPointerDereference,
        AssertKind::NullReferenceConstructed,
        AssertKind::InvalidEnumConstruction(copy(ret)),
    ];
    let asserts = asserts.into_iter().enumerate().map(|(n, msg)| TerminatorKind::Assert {
        cond: copy(flag),
        expected: n % 2 == 0,
        msg,
        target: block(1),
        cleanup: (n % 3 != 0).then(|| block(3)),
    });
    for (n, kind) in terminators.into_iter().chain(asserts).enumerate() {
        let span = spans[n % spans.len()];
        let terminator = Some(Terminator { span, kind });
        match blocks.last_mut() {
            Some(last) if last.terminator.is_none() => last.terminator = terminator,
            _ => blocks.push(BasicBlock { statements: Vec::new(), terminator, is_cleanup: n % 4 == 3 }),
        }
    }
    // A block still being built has no terminator.
    blocks.push(BasicBlock::default());
    let mut basic_blocks = Arena::new();
    for block in blocks {
        basic_blocks.alloc(block);
    }

    let mut binding_locals = ArenaMap::default();
    binding_locals.insert(idx(0), s);
    binding_locals.insert(idx(2), ret);
    let capture = |base| closure_analysis::Place {
        base_ty: s_ty.store(),
        base,
        projections: Box::new([
            closure_analysis::Projection { ty: s_ty.store(), kind: ProjectionKind::Deref },
            closure_analysis::Projection {
                ty: types.u32.store(),
                kind: ProjectionKind::Field { field_idx: 0, variant_idx: 1 },
            },
            closure_analysis::Projection { ty: types.u32.store(), kind: ProjectionKind::Index },
            closure_analysis::Projection { ty: types.u32.store(), kind: ProjectionKind::Subslice },
            closure_analysis::Projection {
                ty: types.u32.store(),
                kind: ProjectionKind::UnwrapUnsafeBinder,
            },
        ]),
    };
    let mut upvar_locals = HashMap::default();
    upvar_locals.insert(
        idx(5),
        vec![
            (s, capture(PlaceBase::Upvar { closure: idx(3), var_id: idx(0) })),
            (ret, capture(PlaceBase::Local(idx(2)))),
        ],
    );
    upvar_locals.insert(idx(1), vec![(f, capture(PlaceBase::Rvalue)), (f, capture(PlaceBase::StaticItem))]);
    upvar_locals.insert(idx(3), Vec::new());

    MirBody {
        basic_blocks,
        locals,
        start_block: idx(0),
        owner: InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it.owner)),
        binding_locals,
        upvar_locals,
        param_locals: vec![s, f],
        promoted_locals: vec![(bytes, idx(4))],
        closures: vec![it.closure, it.closure],
        coroutine: Some(CoroutineLocals { poll, context }),
        unreached: vec![block(2)],
        scope_ends: vec![index, pair, s],
        inherits_overflow_checks: true,
        phase: MirPhase::Built,
        upvars_in_place: vec![(s, 0), (f, 7)],
        panic_unwinds: vec![(block(0), 3, block(3))],
        promoted_constants: vec![bytes],
        unreachable_after_calls: vec![
            UnreachableAfterCall {
                call: MirSpan::ExprId(idx(1)),
                next: MirSpan::PatId(idx(0)),
                descr: "definition",
                ty: types.never.store(),
            },
            UnreachableAfterCall {
                call: MirSpan::ExprId(idx(2)),
                next: MirSpan::ExprEnd(idx(2)),
                descr: "expression",
                ty: types.never.store(),
            },
        ],
        guard_refs: vec![(ret, idx(2))],
        capture_copies: vec![(s, FieldIndex(0)), (f, FieldIndex(2))],
        inlined: Inlined {
            callees: vec![
                InlinedCallee { body: InlinedBody::Fn(it.callee), args: args.store(), written_checked: true },
                InlinedCallee { body: InlinedBody::Closure(it.closure), args: empty.store(), written_checked: false },
            ],
            scopes: vec![InlinedScope { callee: 1, call_site: MirSpan::ExprId(idx(1)) }],
            spans: vec![InlinedSpan { scope: 0, span: MirSpan::PatId(idx(0)) }],
            bindings: vec![InlinedBinding { scope: 0, binding: idx(1), local: ret }],
            promoted: vec![InlinedPromoted { local: bytes, callee: 0, expr: idx(3) }],
        },
    }
}

fn all_bin_ops() -> [BinOp; 20] {
    [
        BinOp::Add,
        BinOp::Sub,
        BinOp::Mul,
        BinOp::Div,
        BinOp::Rem,
        BinOp::BitXor,
        BinOp::BitAnd,
        BinOp::BitOr,
        BinOp::Shl,
        BinOp::Shr,
        BinOp::Eq,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Ne,
        BinOp::Ge,
        BinOp::Gt,
        BinOp::Offset,
        BinOp::AddWithOverflow,
        BinOp::SubWithOverflow,
        BinOp::MulWithOverflow,
    ]
}

fn written(db: &Db, body: &MirBody<'_>) -> Vec<u8> {
    write(db, body, None).expect("the sample body has a word for everything")
}

fn read_back<'db>(db: &'db Db, bytes: &[u8]) -> Result<MirBody<'db>, FormatError> {
    read(db, &Aligned::copy_from(bytes), None)
}

#[test]
fn every_kind_reads_back_equal() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    assert_eq!(read_back(&db, &written(&db, &body)).unwrap(), body);

    let owners = [
        DefWithBodyId::StaticId(it.st),
        DefWithBodyId::ConstId(it.k),
        DefWithBodyId::VariantId(it.variant),
    ];
    for owner in owners {
        let mut other = body.clone(); // clone: a test's copy
        other.owner = InferBodyId::DefWithBodyId(owner);
        other.phase = MirPhase::Runtime;
        other.coroutine = None;
        assert_eq!(read_back(&db, &written(&db, &other)).unwrap(), other, "{owner:?}");
    }
}

#[test]
fn an_empty_body_reads_back_equal() {
    let db = db();
    let it = items(&db);
    let mut basic_blocks = Arena::new();
    basic_blocks.alloc(BasicBlock::default());
    let body = MirBody {
        basic_blocks,
        locals: Arena::new(),
        start_block: idx(0),
        owner: InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it.callee)),
        binding_locals: ArenaMap::default(),
        upvar_locals: HashMap::default(),
        param_locals: Vec::new(),
        promoted_locals: Vec::new(),
        closures: Vec::new(),
        coroutine: None,
        unreached: Vec::new(),
        scope_ends: Vec::new(),
        inherits_overflow_checks: false,
        phase: MirPhase::Built,
        upvars_in_place: Vec::new(),
        panic_unwinds: Vec::new(),
        promoted_constants: Vec::new(),
        unreachable_after_calls: Vec::new(),
        guard_refs: Vec::new(),
        capture_copies: Vec::new(),
        inlined: Inlined::default(),
    };
    assert_eq!(read_back(&db, &written(&db, &body)).unwrap(), body);
}

#[test]
fn equal_bodies_write_equal_bytes() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let bytes = written(&db, &body);
    assert_eq!(written(&db, &sample(&db, &it)), bytes);
    // What was read writes what was read from.
    assert_eq!(written(&db, &read_back(&db, &bytes).unwrap()), bytes);

    // The captures' map, filled in another order.
    let mut reordered = body.clone(); // clone: a test's copy
    let mut entries: Vec<_> = reordered.upvar_locals.drain().collect();
    entries.reverse();
    let mut map = HashMap::with_capacity_and_hasher(64, Default::default());
    map.extend(entries);
    reordered.upvar_locals = map;
    assert_eq!(reordered, body);
    assert_eq!(written(&db, &reordered), bytes);
}

#[test]
fn a_description_lowering_does_not_make_is_refused() {
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    body.unreachable_after_calls[0].descr = "statement";
    assert!(write(&db, &body, None).is_err());
}

#[test]
fn an_allocation_into_a_static_is_relocated_in_a_relocating_scope() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let mut table = crate::check_cache::ids::Table::default();
    let relocated = crate::check_cache::ids::relocating(&mut table, || write(&db, &body, None))
        .expect("a static an allocation points into is written by its place in the table");
    assert_ne!(relocated, written(&db, &body), "the static is by its table index, not its own bits");
}

/// Where the words column starts in `bytes`, and how many bytes it has.
fn words_of(bytes: &[u8]) -> (usize, usize) {
    let aligned = Aligned::copy_from(bytes);
    let root = open_as::<MirRoot>(&aligned, SCHEMA).unwrap();
    let words = root.words.follow(&aligned).unwrap();
    let at = (words.as_ptr() as usize) - (aligned.as_ptr() as usize);
    (at, words.len() * 4)
}

#[test]
fn damaged_words_are_errors_never_panics() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let bytes = written(&db, &body);
    let (start, len) = words_of(&bytes);
    let mut errors = 0;
    for at in start..start + len {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut damaged = bytes.clone(); // clone: a test's copy
            damaged[at] ^= flip;
            let result = catch_unwind(AssertUnwindSafe(|| read_back(&db, &damaged).map(drop)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(_)) => errors += 1,
                Err(_) => panic!("reading the body with byte {at} xor {flip:#x} panicked"),
            }
        }
    }
    assert!(errors > 0, "no damage was seen");
}

#[test]
fn a_cut_or_foreign_buffer_is_an_error() {
    let db = db();
    let it = items(&db);
    let bytes = written(&db, &sample(&db, &it));
    for len in [0, 16, bytes.len() / 2, bytes.len() - 16] {
        assert!(read_back(&db, &bytes[..len]).is_err(), "cut to {len} bytes");
    }
    let mut foreign = bytes.clone(); // clone: a test's copy
    foreign[8] ^= 1;
    assert!(matches!(read_back(&db, &foreign), Err(FormatError::Schema { .. })));

    // The phase word, the first after the owner's three: a phase of no kind.
    let (start, _) = words_of(&bytes);
    let mut damaged = bytes.clone(); // clone: a test's copy
    damaged[start + 12..start + 16].copy_from_slice(&7u32.to_le_bytes());
    match read_back(&db, &damaged) {
        Err(FormatError::Invalid { what, .. }) => assert_eq!(what, "a MIR phase of no kind"),
        other => panic!("a phase of no kind read as {other:?}"),
    }
}

/// The heap blocks `body` owns: what a read of it must make, and nothing more.
fn owned_blocks(body: &MirBody<'_>) -> u64 {
    fn vec<T>(it: &[T]) -> u64 {
        u64::from(!it.is_empty())
    }
    fn operand_lists(kind: &StatementKind) -> u64 {
        match kind {
            StatementKind::Assign(_, Rvalue::Aggregate(_, operands)) => vec(operands),
            StatementKind::InlineAsm(asm) => {
                let lits = asm
                    .template
                    .iter()
                    .filter(|it| matches!(it, AsmPiece::Lit(text) if !text.is_empty()))
                    .count() as u64;
                1 + vec(&asm.template) + vec(&asm.operands) + lits
            }
            _ => 0,
        }
    }
    let mut blocks = 2 + u64::from(body.binding_locals.values().next().is_some());
    for block in body.basic_blocks.values() {
        blocks += vec(&block.statements);
        blocks += block.statements.iter().map(|it| operand_lists(&it.kind)).sum::<u64>();
        if let Some(Terminator { kind, .. }) = &block.terminator {
            blocks += match kind {
                TerminatorKind::Call { args, .. } | TerminatorKind::TailCall { args, .. } => vec(args),
                TerminatorKind::SwitchInt { targets, .. } => {
                    u64::from(targets.values.spilled()) + u64::from(targets.targets.spilled())
                }
                _ => 0,
            };
        }
    }
    blocks += u64::from(!body.upvar_locals.is_empty());
    for places in body.upvar_locals.values() {
        blocks += vec(places) + places.iter().map(|(_, it)| vec(&it.projections)).sum::<u64>();
    }
    let Inlined { callees, scopes, spans, bindings, promoted } = &body.inlined;
    blocks += vec(&body.param_locals)
        + vec(&body.promoted_locals)
        + vec(&body.closures)
        + vec(&body.unreached)
        + vec(&body.scope_ends)
        + vec(&body.upvars_in_place)
        + vec(&body.panic_unwinds)
        + vec(&body.promoted_constants)
        + vec(&body.unreachable_after_calls)
        + vec(&body.guard_refs)
        + vec(&body.capture_copies)
        + vec(callees)
        + vec(scopes)
        + vec(spans)
        + vec(bindings)
        + vec(promoted);
    blocks
}

#[test]
fn a_read_allocates_only_the_body_it_makes() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let bytes = Aligned::copy_from(&written(&db, &body));
    // Warm: every type, projection and allocation is interned already, as after a first read.
    drop(read(&db, &bytes, None).unwrap());
    let (again, used) = measure(|| read(&db, &bytes, None).unwrap());
    assert_eq!(again, body);
    // The reader's own: the type table's places and the allocations' slots. Each allocation
    // read is made whole (its bytes and its map's columns) before interning finds it.
    let allocations = 3;
    let budget = owned_blocks(&body) + 2 + allocations * 12;
    assert!(
        used.allocations <= budget,
        "reading a body of {} blocks made {} allocations, budget {budget}",
        owned_blocks(&body),
        used.allocations
    );
}

/// blaze-diagnostics' `fn_def` before it called [`Operand::fn_def`].
fn fn_def_reference<'db>(func: &Operand) -> Option<(CallableDefId, GenericArgs<'db>)> {
    use rustc_type_ir::inherent::IntoKind as _;

    use crate::next_solver::{CallableIdWrapper, TyKind};
    let ty: Ty<'db> = match &func.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        _ => return None,
    };
    let TyKind::FnDef(CallableIdWrapper(callable), args) = ty.kind() else { return None };
    Some((callable, args.skip_binder()))
}

/// blaze-lints' and codegen's `fn_item` before they called [`Operand::fn_item`].
fn fn_item_reference<'db>(operand: &Operand) -> Option<(FunctionId, GenericArgs<'db>)> {
    use rustc_type_ir::inherent::IntoKind as _;

    use crate::next_solver::TyKind;
    let ty: Ty<'db> = match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        _ => return None,
    };
    let TyKind::FnDef(def, args) = ty.kind() else { return None };
    match def.0 {
        CallableDefId::FunctionId(func) => Some((func, args.skip_binder())),
        _ => None,
    }
}

#[test]
fn a_callee_is_read_off_its_operand_as_each_old_copy_read_it() {
    let db = db();
    let it = items(&db);
    let interner = DbInterner::new_no_crate(&db);
    let types = &default_types().types;
    let empty = default_types().empty.generic_args;
    let args = GenericArgs::new_from_slice(&[types.u32.into()]);
    let three = default_types().consts.u8_values[3];
    let callee = Ty::new_fn_def(interner, CallableDefId::FunctionId(it.callee).into(), args);
    let ctor = Ty::new_fn_def(interner, CallableDefId::EnumVariantId(it.variant).into(), empty);
    let operands = [
        Operand::from_fn(&db, it.callee, empty),
        Operand::from_fn(&db, it.callee, args),
        Operand::const_zst(ctor),
        Operand { kind: OperandKind::Constant { konst: three.store(), ty: callee.store() }, span: None },
        Operand { kind: OperandKind::Constant { konst: three.store(), ty: types.u8.store() }, span: None },
        Operand::from_bytes(Box::new([1, 0, 0, 0]), types.u32),
        Operand { kind: OperandKind::Static(it.st), span: None },
        copy(LocalId::from_raw(la_arena::RawIdx::from(1))),
    ];
    for operand in &operands {
        assert_eq!(operand.fn_def(), fn_def_reference(operand), "{operand:?}");
        assert_eq!(operand.fn_item(), fn_item_reference(operand), "{operand:?}");
    }
    assert_eq!(operands[1].fn_item(), Some((it.callee, args)));
    assert_eq!(operands[2].fn_def(), Some((CallableDefId::EnumVariantId(it.variant), empty)));
    assert_eq!(operands[2].fn_item(), None);
}
