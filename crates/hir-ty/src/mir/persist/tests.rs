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
use std::alloc::Global;

use super::{Locations, MirRoot, SCHEMA, WriteError, idx, read, write};
use crate::{
    CallableDefId, ComplexMemoryMap, Extent, InferBodyId, MemoryMap, MemoryPieces, StaticMemory,
    closure_analysis::{self, PlaceBase, ProjectionKind},
    db::{InternedClosure, InternedClosureId},
    infer::PointerCast,
    intrinsic::Intrinsic,
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind,
        CastKind, ComptimeBinder, CoroutineLocals, FieldIndex, InlineAsm, Inlined, InlinedBinding,
        InlinedBody, InlinedCallee, InlinedPromoted, InlinedScope, InlinedSpan, Local, LocalId,
        MirBody, MirPhase, MirSpan, MutBorrowKind, Operand, OperandKind, Place, PlaceElem,
        Projection, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, SwitchTargets,
        Terminator, TerminatorKind, UnOp, UnreachableAfterCall,
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
        InternedClosure {
            owner: InferBodyId::DefWithBodyId(def),
            expr,
            kind: ClosureKind::Closure,
        },
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
    // A vtable's id names the `dyn` type too, which must read back: const eval puts a `dyn`
    // there, and the map stores any type as it stores the other.
    complex.vtable.vtable_id(types.u32, tuple);
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
    ];
    let projections: [PlaceElem; 7] = [
        ProjectionElem::Deref,
        ProjectionElem::Field(FieldIndex(1)),
        ProjectionElem::Index(index),
        ProjectionElem::ConstantIndex { offset: 1 << 40, min_length: 3, from_end: true },
        ProjectionElem::Subslice { from: 1, to: u64::MAX },
        ProjectionElem::Downcast(VariantId::EnumVariantId(it.variant)),
        ProjectionElem::ReflectedField(ComptimeBinder(3)),
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
        statements.push(assign(
            ret,
            Rvalue::Ref(kind, place(s, &projections[..2])),
            MirSpan::Unknown,
        ));
    }
    statements.push(assign(
        ret,
        Rvalue::Len(place(bytes, &[ProjectionElem::Deref])),
        MirSpan::Unknown,
    ));
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
        statements.push(assign(
            ret,
            Rvalue::Cast(kind, copy(f), types.u64.store()),
            MirSpan::Unknown,
        ));
    }
    for op in all_bin_ops() {
        statements.push(assign(
            ret,
            Rvalue::BinaryOp(op.clone(), copy(ret), copy(ret)),
            MirSpan::Unknown,
        ));
        statements.push(assign(
            pair,
            Rvalue::CheckedBinaryOp(op, copy(ret), copy(ret)),
            MirSpan::Unknown,
        ));
    }
    statements.push(assign(flag, Rvalue::UnaryOp(UnOp::Not, copy(flag)), MirSpan::Unknown));
    statements.push(assign(ret, Rvalue::UnaryOp(UnOp::Neg, copy(ret)), MirSpan::Unknown));
    statements.push(assign(ret, Rvalue::Discriminant(place(s, &[])), MirSpan::Unknown));
    for kind in [
        AggregateKind::Array(types.u32.store()),
        AggregateKind::Tuple(tuple.store()),
        AggregateKind::Adt(VariantId::StructId(it.s), args.store()),
        AggregateKind::Union(it.u, FieldId { parent: VariantId::UnionId(it.u), local_id: idx(1) }),
        AggregateKind::Closure(fn_ty.store()),
    ] {
        let operands = vec![copy(ret), copy(flag)].into_boxed_slice();
        statements.push(assign(pair, Rvalue::Aggregate(kind, operands), MirSpan::Unknown));
    }
    statements.push(assign(
        ret,
        Rvalue::Aggregate(AggregateKind::Array(types.u32.store()), Box::new([])),
        MirSpan::Unknown,
    ));
    statements.push(assign(
        ret,
        Rvalue::CopyForDeref(place(bytes, &[ProjectionElem::Deref])),
        MirSpan::Unknown,
    ));
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

    let mut blocks: Vec<BasicBlock> =
        vec![BasicBlock { statements, terminator: None, is_cleanup: false }];
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
        TerminatorKind::SwitchInt {
            discr: copy(flag),
            targets: SwitchTargets::static_if(1, block(2), block(3)),
        },
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
        TerminatorKind::Yield {
            value: copy(ret),
            resume: block(1),
            resume_arg: place(ret, &[]),
            drop: None,
        },
        TerminatorKind::CoroutineDrop,
        TerminatorKind::FalseEdge { real_target: block(1), imaginary_target: block(2) },
        TerminatorKind::FalseUnwind { real_target: block(1), unwind: Some(block(3)) },
        TerminatorKind::FalseUnwind { real_target: block(1), unwind: None },
        TerminatorKind::ComptimeFor {
            binder: ComptimeBinder(3),
            domain: tuple.store(),
            body: block(1),
            after: block(2),
        },
        TerminatorKind::ComptimeNext { head: block(0) },
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
            _ => blocks.push(BasicBlock {
                statements: Vec::new(),
                terminator,
                is_cleanup: n % 4 == 3,
            }),
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
    // Each of `owner`'s three bindings captured, so every key is one of its store's.
    let mut upvar_locals = HashMap::new_in(Global);
    upvar_locals.insert(
        idx(2),
        vec![
            (s, capture(PlaceBase::Upvar { closure: idx(3), var_id: idx(0) })),
            (ret, capture(PlaceBase::Local(idx(2)))),
        ],
    );
    upvar_locals
        .insert(idx(1), vec![(f, capture(PlaceBase::Rvalue)), (f, capture(PlaceBase::StaticItem))]);
    upvar_locals.insert(idx(0), Vec::new());

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
        coroutine: Some(CoroutineLocals { poll, context, span: MirSpan::ExprId(idx(1)) }),
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
        // Built MIR, the only MIR kept, has no inlined code (`write` refuses a body that does).
        inlined: Inlined::default(),
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

/// What a body of the fixture is read against: `owner`'s store, whose bindings (`s`, `c`, `y`)
/// the sample's binding ids index, and no location literals.
fn locations(db: &Db) -> Locations<'_> {
    let ModuleDefId::FunctionId(owner) = item(db, "owner") else { panic!("`owner` is a function") };
    let store =
        ExpressionStore::of(db, ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(owner)));
    Locations { store, exprs: &[], word: 8 }
}

fn read_back<'db>(db: &'db Db, bytes: &[u8]) -> Result<MirBody<'db>, FormatError> {
    read(db, &Aligned::copy_from(bytes), locations(db))
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
        upvar_locals: HashMap::new_in(Global),
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
    let mut map = HashMap::with_capacity_in(64, Global);
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

/// A body `HeapToStack` moved an allocation of is optimized MIR, which is never kept: its
/// write is refused, never a body read back with its locals retyped and nothing to say so.
#[test]
fn a_body_with_allocations_moved_into_its_frame_is_refused() {
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    body.inlined.moved_to_frame = 1;
    let why = write(&db, &body, None)
        .expect_err("a body with an allocation moved into its frame")
        .to_string();
    assert!(why.contains("moved into its frame"), "{why}");
}

#[test]
fn an_allocation_into_a_static_is_relocated_in_a_relocating_scope() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let mut table = crate::check_cache::ids::Table::default();
    let relocated = crate::check_cache::ids::relocating(&mut table, || write(&db, &body, None))
        .expect("a static an allocation points into is written by its place in the table");
    assert_ne!(
        relocated,
        written(&db, &body),
        "the static is by its table index, not its own bits"
    );
}

/// Where the words column starts in `bytes`, and how many bytes it has.
fn words_of(bytes: &[u8]) -> (usize, usize) {
    let aligned = Aligned::copy_from(bytes);
    let root = open_as::<MirRoot>(&aligned, SCHEMA).unwrap();
    let words = root.words.follow(&aligned).unwrap();
    let at = (words.as_ptr() as usize) - (aligned.as_ptr() as usize);
    (at, words.len() * 4)
}

/// Where `body` indexes past a table, if it does: an expression, pattern or binding past the
/// store, a union's field past its union, a reference into the body's own tables past them, a
/// switch whose targets are not one more than its values, a binder past the universes, any
/// inlined span or table (built MIR has none). What a read gives back never does, whichever
/// word was damaged: a body that does panics where it is used (`store[expr]`,
/// `SwitchTargets::otherwise`, `UniverseIndex::from_u32`, a union's field offset).
fn out_of_range(db: &Db, body: &MirBody<'_>, store: &ExpressionStore) -> Option<String> {
    let len = |len: usize| len as u32;
    let mut check = InRange {
        db,
        exprs: len(store.expr_count()),
        pats: len(store.pat_count()),
        bindings: len(store.bindings().count()),
        bad: Vec::new(), // alloc: a test's list
    };
    for data in body.basic_blocks.values() {
        for statement in &data.statements {
            check.span(statement.span);
            check.statement(&statement.kind);
        }
        if let Some(terminator) = &data.terminator {
            check.span(terminator.span);
            check.terminator(&terminator.kind);
        }
    }
    for (binding, _) in body.binding_locals.iter() {
        check.binding(binding, "a binding mapped to a local");
    }
    for (&binding, places) in body.upvar_locals.iter() {
        check.binding(binding, "a captured binding");
        for (_, place) in places {
            match place.base {
                PlaceBase::Local(it) => check.binding(it, "a captured place's binding"),
                PlaceBase::Upvar { closure, var_id } => {
                    check.expr(closure, "a captured upvar's closure");
                    check.binding(var_id, "a captured upvar's binding");
                }
                PlaceBase::Rvalue | PlaceBase::StaticItem => {}
            }
        }
    }
    for &(_, expr) in &body.promoted_locals {
        check.expr(expr, "a promoted local's expression");
    }
    for &(_, binding) in &body.guard_refs {
        check.binding(binding, "a guard reference's binding");
    }
    for &(block, statement, _) in &body.panic_unwinds {
        let statements = body.basic_blocks[block].statements.len();
        check.is(statement < statements, || {
            format!("a panic unwind's statement {statement} of {statements}")
        });
    }
    for call in &body.unreachable_after_calls {
        check.span(call.call);
        check.span(call.next);
    }
    check.is(body.inlined == Inlined::default(), || "an inlined table".to_owned());
    (!check.bad.is_empty()).then(|| check.bad.join("; "))
}

struct InRange<'a> {
    db: &'a Db,
    exprs: u32,
    pats: u32,
    bindings: u32,
    bad: Vec<String>,
}

impl InRange<'_> {
    fn is(&mut self, holds: bool, what: impl FnOnce() -> String) {
        if !holds {
            self.bad.push(what());
        }
    }

    fn expr(&mut self, expr: hir_def::hir::ExprId, what: &str) {
        let (raw, len) = (expr.into_raw().into_u32(), self.exprs);
        self.is(raw < len, || format!("{what} {raw} of {len} expressions"));
    }

    fn binding(&mut self, binding: hir_def::hir::BindingId, what: &str) {
        let (raw, len) = (binding.into_raw().into_u32(), self.bindings);
        self.is(raw < len, || format!("{what} {raw} of {len} bindings"));
    }

    fn span(&mut self, span: MirSpan) {
        let (raw, len, of) = match span {
            MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => {
                (it.into_raw().into_u32(), self.exprs, "expressions")
            }
            MirSpan::PatId(it) => (it.into_raw().into_u32(), self.pats, "patterns"),
            MirSpan::BindingId(it) => (it.into_raw().into_u32(), self.bindings, "bindings"),
            MirSpan::Inlined(n) => (n, 0, "inlined spans"),
            MirSpan::SelfParam | MirSpan::Unknown => return,
        };
        self.is(raw < len, || format!("a span {raw} of {len} {of}"));
    }

    fn operand(&mut self, operand: &Operand) {
        if let Some(span) = operand.span {
            self.span(span);
        }
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            self.place(place);
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Use(it)
            | Rvalue::Repeat(it, _)
            | Rvalue::Cast(_, it, _)
            | Rvalue::UnaryOp(_, it) => self.operand(it),
            Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                self.operand(a);
                self.operand(b);
            }
            Rvalue::Aggregate(kind, operands) => {
                if let AggregateKind::Union(union, FieldId { parent, local_id }) = *kind {
                    let fields = VariantId::UnionId(union).fields(self.db).fields().len();
                    let field = local_id.into_raw().into_u32() as usize;
                    self.is(parent == VariantId::UnionId(union) && field < fields, || {
                        format!("a union's field {field} of {parent:?}, of {fields}")
                    });
                }
                operands.iter().for_each(|it| self.operand(it));
            }
            Rvalue::Ref(..)
            | Rvalue::Len(_)
            | Rvalue::Discriminant(_)
            | Rvalue::CopyForDeref(_) => {}
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn statement(&mut self, kind: &StatementKind) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                self.place(place);
                self.rvalue(rvalue);
            }
            StatementKind::FakeRead(place) | StatementKind::Deinit(place) => self.place(place),
            StatementKind::InlineAsm(asm) => {
                for piece in &asm.template {
                    if let AsmPiece::Operand { index, .. } = *piece {
                        let operands = asm.operands.len();
                        self.is(index < operands, || {
                            format!("an `asm!` operand {index} of {operands}")
                        });
                    }
                }
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { input: Some(it), .. }
                        | AsmOperand::Const(it)
                        | AsmOperand::Sym(it) => self.operand(it),
                        AsmOperand::Reg { input: None, .. } | AsmOperand::Label => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn binder(&mut self, ComptimeBinder(universe): ComptimeBinder) {
        self.is(universe <= rustc_type_ir::UniverseIndex::MAX_AS_U32, || {
            format!("a binder {universe:#x}")
        });
    }

    fn place(&mut self, place: &StoredPlace) {
        for elem in place.projection.as_slice() {
            if let ProjectionElem::ReflectedField(binder) = *elem {
                self.binder(binder);
            }
        }
    }

    fn terminator(&mut self, kind: &TerminatorKind) {
        if let TerminatorKind::SwitchInt { targets, .. } = kind {
            let (values, targets) = (targets.values.len(), targets.targets.len());
            self.is(targets == values + 1, || {
                format!("a switch of {values} values and {targets} targets")
            });
        }
        if let TerminatorKind::ComptimeFor { binder, .. } = *kind {
            self.binder(binder);
        }
        match kind {
            TerminatorKind::SwitchInt { discr: it, .. }
            | TerminatorKind::DropAndReplace { value: it, .. }
            | TerminatorKind::Yield { value: it, .. } => self.operand(it),
            TerminatorKind::Call { func, args, .. } => {
                self.operand(func);
                args.iter().for_each(|it| self.operand(it));
            }
            TerminatorKind::TailCall { func, args, fn_span } => {
                self.operand(func);
                args.iter().for_each(|it| self.operand(it));
                self.span(*fn_span);
            }
            TerminatorKind::Assert { cond, msg, .. } => {
                self.operand(cond);
                match msg {
                    AssertKind::BoundsCheck { len: a, index: b }
                    | AssertKind::Overflow(_, a, b)
                    | AssertKind::MisalignedPointerDereference { required: a, found: b } => {
                        self.operand(a);
                        self.operand(b);
                    }
                    AssertKind::OverflowNeg(a)
                    | AssertKind::DivisionByZero(a)
                    | AssertKind::RemainderByZero(a)
                    | AssertKind::InvalidEnumConstruction(a) => self.operand(a),
                    AssertKind::NullPointerDereference | AssertKind::NullReferenceConstructed => {}
                }
            }
            _ => {}
        }
    }
}

#[test]
fn damaged_words_are_errors_never_panics() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let bytes = written(&db, &body);
    let (start, len) = words_of(&bytes);
    let locations = locations(&db);
    assert_eq!(out_of_range(&db, &body, locations.store), None, "the sample is in range");
    let mut errors = 0;
    // Every byte of every word, the store's ids and the body's references into its own tables
    // among them: a flip of the low bit moves an index by one, past a table it was last in;
    // a flip of a high bit moves it far past any.
    for at in start..start + len {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut damaged = bytes.clone(); // clone: a test's copy
            damaged[at] ^= flip;
            let damaged = Aligned::copy_from(&damaged);
            let (result, used) = measure(|| {
                catch_unwind(AssertUnwindSafe(|| {
                    read(&db, &damaged, locations)
                        .map(|body| out_of_range(&db, &body, locations.store))
                }))
            });
            // A damaged length or index is refused before it sizes anything: what a read
            // allocates stays near the buffer's size, never a count a damaged word names.
            assert!(
                used.largest <= DAMAGED_READ_LARGEST,
                "reading the body with byte {at} xor {flip:#x} made an allocation of {} bytes",
                used.largest
            );
            match result {
                Ok(Ok(None)) => {}
                Ok(Ok(Some(why))) => {
                    panic!(
                        "the body with byte {at} xor {flip:#x} read as a body past its tables: {why}"
                    )
                }
                Ok(Err(_)) => errors += 1,
                Err(_) => panic!("reading the body with byte {at} xor {flip:#x} panicked"),
            }
        }
    }
    assert!(errors > 0, "no damage was seen");
}

/// The largest allocation a read of a damaged sample body may make: the buffer is a few KiB,
/// so this leaves room for interning only, far below a list or map sized by a damaged word.
const DAMAGED_READ_LARGEST: u64 = 4 << 20;

/// The binding that `binding_locals` names past its store's: the map is dense up to its last
/// binding, so a damaged binding word once sized it at up to 2^32 slots (17 GB at `0xff..`),
/// the 32 GB peaks of `damaged_words_are_errors_never_panics`.
#[test]
fn a_binding_past_the_store_is_refused_before_it_sizes_the_map() {
    // A binding the store has none of, unique among the body's words, so its word is found.
    const MARKER: u32 = 0x0001_2345;
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    let local = body.binding_locals[idx(2)];
    body.binding_locals.insert(idx(MARKER), local);
    let bytes = written(&db, &body);
    let (start, len) = words_of(&bytes);
    let found: Vec<usize> = (start..start + len)
        .step_by(4)
        .filter(|&at| bytes[at..at + 4] == MARKER.to_le_bytes())
        .collect(); // alloc: a test's list
    let [at] = found[..] else { panic!("the marker binding is in {} words, not one", found.len()) };
    let locations = locations(&db);
    let bindings = locations.store.bindings().count() as u64;
    for high in [0u8, 0x01, 0x80, 0xff] {
        let mut damaged = bytes.clone(); // clone: a test's copy
        damaged[at + 3] = high;
        let binding = u32::from_le_bytes(damaged[at..at + 4].try_into().unwrap());
        let damaged = Aligned::copy_from(&damaged);
        let (result, used) = measure(|| read(&db, &damaged, locations).map(drop));
        match result {
            Err(FormatError::Bound { at: byte, what, found, bound }) => {
                assert_eq!(
                    what,
                    "a binding mapped to a local, below the body's store's binding count"
                );
                assert_eq!((found, bound), (u64::from(binding), bindings));
                // The error names the byte its word starts at in the buffer.
                assert_eq!(byte, at as u64, "binding {binding:#x}'s error is at its word's byte");
            }
            other => panic!("binding {binding:#x} of a store of {bindings} read as {other:?}"),
        }
        assert!(
            used.largest <= DAMAGED_READ_LARGEST,
            "binding {binding:#x} made an allocation of {} bytes",
            used.largest
        );
    }
}

/// `body`, written and read back against the sample's store: refused with a `Bound` naming
/// `what`, the value `found` and the `bound`.
#[track_caller]
fn refused_past(db: &Db, body: &MirBody<'_>, what: &str, found: u64, bound: u64) {
    match read_back(db, &written(db, body)) {
        Err(FormatError::Bound { what: why, found: value, bound: past, .. }) => {
            assert_eq!(why, what);
            assert_eq!((value, past), (found, bound), "{what}");
        }
        other => panic!("{what} {found} of {bound} read as {other:?}"),
    }
}

/// `body`, written and read back against the sample's store: refused as `Invalid` for `what`.
#[track_caller]
fn refused_invalid(db: &Db, body: &MirBody<'_>, what: &str) {
    match read_back(db, &written(db, body)) {
        Err(FormatError::Invalid { what: why, .. }) => assert_eq!(why, what),
        other => panic!("{what} read as {other:?}"),
    }
}

/// An expression, pattern or binding past the body's store (each kind of field that holds
/// one) is refused, never a body that panics on `store[id]` where it is used. Each is the
/// first id past the store, so the bound is exact.
#[test]
fn a_store_id_past_the_store_is_refused() {
    let db = db();
    let it = items(&db);
    let store = locations(&db).store;
    let (exprs, pats, bindings) =
        (store.expr_count() as u32, store.pat_count() as u32, store.bindings().count() as u32);
    type Damage = Box<dyn Fn(&mut MirBody<'_>)>;
    let first_statement = |span: MirSpan| -> Damage {
        Box::new(move |body: &mut MirBody<'_>| body.basic_blocks[idx(0)].statements[0].span = span)
    };
    let captured = |base: PlaceBase| -> Damage {
        Box::new(move |body: &mut MirBody<'_>| {
            body.upvar_locals.get_mut(&idx(2)).unwrap()[0].1.base = base
        })
    };
    let cases: Vec<(Damage, &str, u32)> = vec![
        (
            first_statement(MirSpan::ExprId(idx(exprs))),
            "a span's expression, below the store's expression count",
            exprs,
        ),
        (
            first_statement(MirSpan::ExprEnd(idx(exprs))),
            "a span's expression end, below the store's expression count",
            exprs,
        ),
        (
            first_statement(MirSpan::PatId(idx(pats))),
            "a span's pattern, below the store's pattern count",
            pats,
        ),
        (
            first_statement(MirSpan::BindingId(idx(bindings))),
            "a span's binding, below the store's binding count",
            bindings,
        ),
        (
            Box::new(move |body: &mut MirBody<'_>| body.promoted_locals[0].1 = idx(exprs)),
            "a promoted local's expression, below the store's expression count",
            exprs,
        ),
        (
            Box::new(move |body: &mut MirBody<'_>| body.guard_refs[0].1 = idx(bindings)),
            "a guard reference's binding, below the store's binding count",
            bindings,
        ),
        (
            Box::new(move |body: &mut MirBody<'_>| {
                drop(body.upvar_locals.insert(idx(bindings), Vec::new()))
            }),
            "a captured binding, below the store's binding count",
            bindings,
        ),
        (
            captured(PlaceBase::Local(idx(bindings))),
            "a captured place's binding, below the store's binding count",
            bindings,
        ),
        (
            captured(PlaceBase::Upvar { closure: idx(exprs), var_id: idx(0) }),
            "a captured upvar's closure, below the store's expression count",
            exprs,
        ),
        (
            captured(PlaceBase::Upvar { closure: idx(0), var_id: idx(bindings) }),
            "a captured upvar's binding, below the store's binding count",
            bindings,
        ),
    ];
    for (damage, what, bound) in cases {
        let mut body = sample(&db, &it);
        damage(&mut body);
        refused_past(&db, &body, what, u64::from(bound), u64::from(bound));
    }
}

/// The union aggregate's statement of the sample's first block.
fn union_aggregate<'b>(body: &'b mut MirBody<'_>) -> &'b mut FieldId {
    body.basic_blocks[idx(0)]
        .statements
        .iter_mut()
        .find_map(|it| match &mut it.kind {
            StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Union(_, field), _)) => {
                Some(field)
            }
            _ => None,
        })
        .expect("the sample has a union aggregate")
}

/// A union aggregate's field past its union's fields is refused, never an offset asked of
/// the union's layout past its fields; so is a field of another variant than the union.
#[test]
fn a_union_field_past_its_union_is_refused() {
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    // `U` has `x` and `y`.
    union_aggregate(&mut body).local_id = idx(2);
    refused_past(&db, &body, "a union aggregate's field, below its union's field count", 2, 2);
    let mut body = sample(&db, &it);
    union_aggregate(&mut body).parent = VariantId::StructId(it.s);
    refused_invalid(&db, &body, "a union aggregate's field of a struct, not its union");
}

/// A panic unwind's statement past its block's statements is refused: it names no statement,
/// so the pass that makes the `Assert`s would never find it.
#[test]
fn a_panic_unwind_past_its_blocks_statements_is_refused() {
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    let (block, _, _) = body.panic_unwinds[0];
    let statements = body.basic_blocks[block].statements.len();
    body.panic_unwinds[0].1 = statements;
    let what = "a panic unwind's statement, below its block's statement count";
    refused_past(&db, &body, what, statements as u64, statements as u64);
}

/// A body with any inlined table or span is optimized MIR, never kept: `write` refuses it, as
/// it refuses specialized calls and allocations moved into the frame.
#[test]
fn a_body_with_inlined_code_is_refused_on_write() {
    let db = db();
    let it = items(&db);
    let empty = default_types().empty.generic_args;
    let ret = idx(0);
    type Damage = Box<dyn Fn(&mut MirBody<'_>)>;
    let cases: Vec<(Damage, &str)> = vec![
        (
            Box::new(move |body: &mut MirBody<'_>| {
                body.inlined.callees.push(InlinedCallee {
                    body: InlinedBody::Fn(it.callee),
                    args: empty.store(),
                    written_checked: true,
                })
            }),
            "a body with inlined callees, which only optimized MIR has",
        ),
        (
            Box::new(|body: &mut MirBody<'_>| {
                body.inlined
                    .scopes
                    .push(InlinedScope { callee: 0, call_site: MirSpan::ExprId(idx(1)) })
            }),
            "a body with inlined scopes, which only optimized MIR has",
        ),
        (
            Box::new(|body: &mut MirBody<'_>| {
                body.inlined.spans.push(InlinedSpan { scope: 0, span: MirSpan::PatId(idx(0)) })
            }),
            "a body with inlined spans, which only optimized MIR has",
        ),
        (
            Box::new(move |body: &mut MirBody<'_>| {
                body.inlined.bindings.push(InlinedBinding { scope: 0, binding: idx(1), local: ret })
            }),
            "a body with inlined bindings, which only optimized MIR has",
        ),
        (
            Box::new(move |body: &mut MirBody<'_>| {
                body.inlined.promoted.push(InlinedPromoted { local: ret, callee: 0, expr: idx(3) })
            }),
            "a body with inlined promoted locals, which only optimized MIR has",
        ),
        (
            Box::new(|body: &mut MirBody<'_>| body.inlined.devirtualized.push([7; 32])),
            "a body with devirtualized calls, which only optimized MIR has",
        ),
        (
            Box::new(|body: &mut MirBody<'_>| body.inlined.rely([7; 32])),
            "a body that relied on facts, which only optimized MIR has",
        ),
        (
            Box::new(|body: &mut MirBody<'_>| {
                body.basic_blocks[idx(0)].statements[0].span = MirSpan::Inlined(0)
            }),
            "an inlined span, which only optimized MIR has",
        ),
    ];
    for (damage, wanted) in cases {
        let mut body = sample(&db, &it);
        damage(&mut body);
        match write(&db, &body, None) {
            Err(WriteError::Unwritable(why)) => assert_eq!(why, wanted),
            other => panic!("{wanted}: written as {other:?}"),
        }
    }
}

/// An inlined span's word is refused on reading too: built MIR has none, so no index of one
/// is ever checked against tables the record does not have.
#[test]
fn an_inlined_span_is_refused_on_read() {
    // An expression the store has none of, unique among the body's words, so its word is found;
    // its tag, the word before, becomes an inlined span's.
    const MARKER: u32 = 0x0001_2345;
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    body.basic_blocks[idx(0)].statements[0].span = MirSpan::ExprId(idx(MARKER));
    let mut bytes = written(&db, &body);
    let (start, len) = words_of(&bytes);
    let found: Vec<usize> = (start..start + len)
        .step_by(4)
        .filter(|&at| bytes[at..at + 4] == MARKER.to_le_bytes())
        .collect(); // alloc: a test's list
    let [at] = found[..] else {
        panic!("the marker expression is in {} words, not one", found.len())
    };
    assert_eq!(bytes[at - 4..at], 0u32.to_le_bytes(), "the marker's tag is an expression's");
    bytes[at - 4..at].copy_from_slice(&6u32.to_le_bytes());
    match read_back(&db, &bytes) {
        Err(FormatError::Invalid { what, at: byte }) => {
            assert_eq!(what, "an inlined span, which only optimized MIR has and is never kept");
            assert_eq!(byte, (at - 4) as u64, "the error is at the tag's byte");
        }
        other => panic!("an inlined span read as {other:?}"),
    }
}

/// A switch whose targets are not one more than its values is refused: `write` always writes
/// one more (`SwitchTargets::new`), and with none `SwitchTargets::otherwise` panics, with as
/// many it takes the last value's block for the rest.
#[test]
fn a_switch_whose_targets_are_not_one_more_than_its_values_is_refused() {
    let db = db();
    let it = items(&db);
    for count in [0, 3, 5] {
        let mut body = sample(&db, &it);
        let switch =
            body.basic_blocks
                .values_mut()
                .find_map(|block| match &mut block.terminator {
                    Some(Terminator {
                        kind: TerminatorKind::SwitchInt { targets, .. }, ..
                    }) if targets.values.len() == 3 => Some(targets),
                    _ => None,
                })
                .expect("the sample has a switch of three values");
        switch.targets.resize(count, idx(1));
        refused_past(&db, &body, "a switch's targets, one more than its values", count as u64, 4);
    }
}

/// A binder past the universes (`UniverseIndex::MAX_AS_U32`) is refused, a `for const` loop's
/// and a reflected field's: `ComptimeBinder::ty` asserts it is not.
#[test]
fn a_binder_past_the_universes_is_refused() {
    let db = db();
    let it = items(&db);
    let past = rustc_type_ir::UniverseIndex::MAX_AS_U32 + 1;
    let mut body = sample(&db, &it);
    for block in body.basic_blocks.values_mut() {
        if let Some(Terminator { kind: TerminatorKind::ComptimeFor { binder, .. }, .. }) =
            &mut block.terminator
        {
            *binder = ComptimeBinder(past);
        }
    }
    refused_past(
        &db,
        &body,
        "a `for const` loop's binder, below the universe count",
        past.into(),
        past.into(),
    );

    let mut body = sample(&db, &it);
    let index = idx(6);
    let reflected = place(index, &[ProjectionElem::ReflectedField(ComptimeBinder(past))]);
    body.basic_blocks[idx(0)].statements[0].kind = StatementKind::FakeRead(reflected);
    refused_past(
        &db,
        &body,
        "a reflected field's binder, below the universe count",
        past.into(),
        past.into(),
    );
}

/// An `asm!` template's operand past the block's operands is refused.
#[test]
fn an_asm_operand_past_the_blocks_operands_is_refused() {
    let db = db();
    let it = items(&db);
    let mut body = sample(&db, &it);
    let asm = body.basic_blocks[idx(0)]
        .statements
        .iter_mut()
        .find_map(|it| match &mut it.kind {
            StatementKind::InlineAsm(asm) => Some(asm),
            _ => None,
        })
        .expect("the sample has an `asm!` block");
    let operands = asm.operands.len();
    let Some(AsmPiece::Operand { index, .. }) = asm.template.get_mut(2) else {
        panic!("piece 2 is an operand")
    };
    *index = operands;
    let what = "an `asm!` template's operand, below the block's operand count";
    refused_past(&db, &body, what, operands as u64, operands as u64);
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
                TerminatorKind::Call { args, .. } | TerminatorKind::TailCall { args, .. } => {
                    vec(args)
                }
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
    let Inlined {
        callees,
        scopes,
        spans,
        bindings,
        promoted,
        devirtualized,
        specialized,
        moved_to_frame: _,
        relied,
    } = &body.inlined;
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
        + vec(promoted)
        + vec(devirtualized)
        + vec(specialized)
        + vec(relied);
    blocks
}

#[test]
fn a_read_allocates_only_the_body_it_makes() {
    let db = db();
    let it = items(&db);
    let body = sample(&db, &it);
    let bytes = Aligned::copy_from(&written(&db, &body));
    // Warm: every type, projection and allocation is interned already, as after a first read.
    let locations = locations(&db);
    drop(read(&db, &bytes, locations).unwrap());
    let (again, used) = measure(|| read(&db, &bytes, locations).unwrap());
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
        Operand {
            kind: OperandKind::Constant { konst: three.store(), ty: callee.store() },
            span: None,
        },
        Operand {
            kind: OperandKind::Constant { konst: three.store(), ty: types.u8.store() },
            span: None,
        },
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
