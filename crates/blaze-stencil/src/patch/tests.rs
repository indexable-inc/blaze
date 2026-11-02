//! The patcher's passes on small functions built with `FunctionBuilder`, against a mock library
//! (written with [`Writer`]: each stencil is a header naming its shape, one field per hole and
//! a tail jump) and a mock glue whose pieces are single marker bytes. [`decode`] reads a
//! function's code back into stencils (with every hole's value), relays and glue pieces, so
//! each test states what the code is.

use std::sync::OnceLock;

use cranelift_codegen::{
    ir::{
        self, AbiParam, BlockArg, ConstantData, InstBuilder, Opcode, Signature, SourceLoc,
        StackSlotData, StackSlotKind, TrapCode, UserFuncName, condcodes::IntCC, types,
    },
    isa::CallConv,
    settings,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use blaze_bytes::le::u32 as u32_at;
use super::{Kind, NONE, Plan, Sel, compile_in, slots::GLOBAL, survey_in};
use crate::{
    Cpu, Error, Os, Target, TlsModel,
    arch::{Area, Cx, Glue},
    code::{self, Code},
    enc::{self, Form},
    shape::{self, Disp, Dst, IntCc, Isa, Op, Shape, Src, Ty},
    table::format::{Hole, HoleKind, Reader, Variant, Writer},
};

/// What the tests read of a compile's plan, copied out of its frame.
#[derive(Default)]
struct Seen {
    slot: Vec<u32>,
    last: Vec<u32>,
    sels: Vec<Sel>,
    stack: Vec<u32>,
    base: u32,
    relays: bool,
    area: (u32, u32),
}

impl Seen {
    fn of(plan: &Plan<'_>) -> Seen {
        Seen {
            slot: plan.slot.to_vec(),
            last: plan.last.to_vec(),
            sels: plan.sels.to_vec(),
            stack: plan.stack.to_vec(),
            base: plan.base,
            relays: plan.relays,
            area: plan.area,
        }
    }

    fn stack_slot(&self, slot: ir::StackSlot) -> u32 {
        self.stack[slot.as_u32() as usize]
    }

    fn area(&self) -> Area {
        Area { size: self.area.0, align: self.area.1 }
    }
}

/// The plan of the last compile, where the tests used to find the patcher's scratch.
#[derive(Default)]
struct Scratch {
    plan: Seen,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch::default()
    }
}

const HEADER: u8 = 0x5c;
const TAIL: u8 = 0xe9;
const PROLOGUE: u8 = 0xf0;
const RET: u8 = 0xc3;
const COUNT: u8 = 0xbe;
const LANDING: u8 = 0x1a;
const TRAP: u8 = 0xcc;
const RELAY: u8 = 0x4e;
const STUB: [u8; 2] = [0x0f, 0x0b];

/// The ops the mock library has every shape of.
const OPS: [Op; 26] = [
    Op::Load,
    Op::Store,
    Op::LoadConfined,
    Op::StoreConfined,
    Op::Ireduce,
    Op::Uextend,
    Op::Snarrow,
    Op::SwidenLow,
    Op::Copy,
    Op::Iconst,
    Op::Fconst,
    Op::Vconst,
    Op::Iadd,
    Op::Isub,
    Op::Imul,
    Op::Udiv,
    Op::Band,
    Op::Icmp,
    Op::Select,
    Op::SelectIcmp,
    Op::UaddOverflow,
    Op::StackAddr,
    Op::Jump,
    Op::Brif,
    Op::BrIcmp,
    Op::Trapz,
];

/// The holes of the mock stencil of `shape`, in the order of their fields.
fn fields(shape: Shape) -> Vec<(HoleKind, Form)> {
    let info = shape.op.info();
    let mut out = Vec::new();
    match shape.op {
        Op::Iconst | Op::Fconst => {
            out.push((HoleKind::Imm0, if shape.aux == 1 { Form::Abs64 } else { Form::Abs32S }));
        }
        _ => {
            const SLOTS: [HoleKind; 3] = [HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2];
            const IMMS: [HoleKind; 3] = [HoleKind::Imm0, HoleKind::Imm1, HoleKind::Imm2];
            for (n, src) in shape.srcs.into_iter().enumerate() {
                match src {
                    Src::Slot => out.push((SLOTS[n], Form::Abs32S)),
                    Src::Imm => out.push((IMMS[n], Form::Abs32S)),
                    Src::Acc | Src::None => {}
                }
            }
        }
    }
    let displaced = shape.confined().is_some_and(|(_, disp)| disp != Disp::Zero);
    if matches!(shape.op, Op::Load | Op::Store | Op::StackAddr) || displaced {
        out.push((HoleKind::Offset, Form::Abs32S));
    }
    if matches!(shape.dst, Dst::Slot | Dst::Both) {
        out.push((HoleKind::Result0, Form::Abs32S));
    }
    if info.results > 1 {
        out.push((HoleKind::Result1, Form::Abs32S));
    }
    if info.branches {
        out.push((HoleKind::Target, Form::Rel32));
    }
    if info.traps {
        out.push((HoleKind::Trap, Form::Rel32));
    }
    if matches!(shape.op, Op::Vconst | Op::Shuffle) {
        out.push((HoleKind::Const, Form::Rel32));
    }
    out
}

/// Where a mock load or store "accesses memory": its first field.
const FAULT: u16 = 5;

/// The mock stencil of `shape`: `HEADER`, its key, its fields, then `jmp Next` as its tail.
fn stencil(shape: Shape) -> (Vec<u8>, Vec<Hole>) {
    let mut code = vec![HEADER];
    code.extend_from_slice(&shape.key().to_le_bytes());
    let mut holes = Vec::new();
    for (kind, form) in fields(shape) {
        let addend = if form.pc_relative() { -4 } else { 0 };
        holes.push(Hole { at: code.len() as u16, form, kind, addend });
        code.resize(code.len() + usize::from(form.bytes()), 0);
    }
    code.push(TAIL);
    holes.push(Hole { at: code.len() as u16, form: Form::Rel32, kind: HoleKind::Next, addend: -4 });
    code.extend_from_slice(&[0; 4]);
    (code, holes)
}

fn build(keep: impl Fn(Shape) -> bool) -> Vec<u8> {
    let mut writer = Writer::new(Variant::X86_64);
    shape::each_on(Isa::X86_64, |shape| {
        if keep(shape) {
            let (code, holes) = stencil(shape);
            let access = matches!(shape.op, Op::Load | Op::Store | Op::LoadConfined | Op::StoreConfined);
            let faults: &[u16] = if access { &[FAULT] } else { &[] };
            writer.push_with_faults(shape, &code, 5, &holes, faults).unwrap();
        }
    });
    writer.finish(|_| [0; 32]).unwrap()
}

fn library() -> Reader<'static> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    Reader::new(BYTES.get_or_init(|| build(|shape| OPS.contains(&shape.op)))).unwrap()
}

/// The glue of the tests: one marker byte per piece, slots near below 1 KiB.
struct Mock;

#[derive(Default)]
struct Frame;

impl Glue for Mock {
    const ARCH: blaze_codegen::Arch = blaze_codegen::Arch::X86_64;
    const FRAME: u8 = 12;
    const NEAR: [u32; 2] = [1024, 2048];
    const NOP: &'static [u8] = &[0x90];
    type Frame = Frame;

    fn frame(_: &ir::Function, _: &Target, _: Area) -> Result<Frame, Error> {
        Ok(Frame)
    }

    fn takes(func: &ir::Function, _: &Target, inst: ir::Inst) -> bool {
        matches!(func.dfg.insts[inst].opcode(), Opcode::Return | Opcode::Trap)
    }

    fn bound(_: &ir::Function, _: &Target, _: ir::Inst) -> u32 {
        16
    }

    fn bound_fixed(_: &Frame) -> u32 {
        16
    }

    fn prologue(cx: &mut Cx<'_, '_, Mock>) -> Result<(), Error> {
        cx.buf.put1(PROLOGUE);
        Ok(())
    }

    fn inst(cx: &mut Cx<'_, '_, Mock>, inst: ir::Inst) -> Result<(), Error> {
        match cx.func.dfg.insts[inst].opcode() {
            Opcode::Return => cx.buf.put1(RET),
            Opcode::Trap => {
                let code = cx.func.dfg.insts[inst].trap_code().unwrap();
                let stub = cx.trap(code);
                cx.buf.put1(TRAP);
                let at = cx.buf.len();
                cx.buf.put4(0);
                cx.buf.fixup(at, Form::Rel32, stub, -4);
            }
            opcode => return Err(Error::at(inst, format!("the mock glue has no `{opcode}`"))),
        }
        Ok(())
    }

    fn landing(cx: &mut Cx<'_, '_, Mock>, _: ir::Inst) -> Result<(), Error> {
        cx.buf.put1(LANDING);
        Ok(())
    }

    fn trap(cx: &mut Cx<'_, '_, Mock>, _: TrapCode) {
        cx.buf.put(&STUB);
    }

    fn count_back_edge(cx: &mut Cx<'_, '_, Mock>) {
        cx.buf.put1(COUNT);
    }

    fn relay(cx: &mut Cx<'_, '_, Mock>, from: u32, to: u32, bytes: u8) {
        cx.buf.put1(RELAY);
        cx.buf.put1(bytes);
        cx.buf.put4(from);
        cx.buf.put4(to);
    }

    fn unwind(_: &mut Cx<'_, '_, Mock>) -> Result<Option<code::UnwindKind>, Error> {
        Ok(None)
    }

    fn slot_base(_: &Frame) -> (u16, i32) {
        (6, -16)
    }

    fn frame_size(_: &Frame) -> u32 {
        0
    }
}

const TARGET: Target = Target {
    arch: blaze_codegen::Arch::X86_64,
    os: Os::Linux,
    cpu: Cpu::Baseline,
    pic: true,
    tls: TlsModel::ElfGd,
    probe_stack: true,
    probe_log2: 12,
};

/// One stencil of the code, with each hole's value (a place for the PC-relative ones).
#[derive(Clone, Debug, PartialEq)]
struct Decoded {
    at: u32,
    shape: Shape,
    /// Whether its tail jump was kept.
    tail: bool,
    /// Where its `Next` goes: the end of its body when the tail was dropped.
    next: u32,
    fields: Vec<(HoleKind, i64)>,
}

impl Decoded {
    fn field(&self, kind: HoleKind) -> i64 {
        self.fields.iter().find(|it| it.0 == kind).unwrap_or_else(|| panic!("{} has no {kind:?}", self.shape)).1
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Glue { at: u32, byte: u8 },
    Relay { bytes: u8, from: u32, to: u32 },
    Stencil(Decoded),
}

/// The pieces of `code`'s first `end` bytes.
fn decode_to(code: &Code, end: usize) -> Vec<Piece> {
    let bytes = &code.bytes;
    let mut at = 0;
    let mut out = Vec::new();
    while at < end {
        match bytes[at] {
            HEADER => {
                let shape = Shape::from_key(u32_at(bytes, at + 1)).unwrap();
                let mut p = at + 5;
                let mut values = Vec::new();
                for (kind, form) in fields(shape) {
                    let mut value = enc::read(form, bytes, p);
                    if form.pc_relative() {
                        value += p as i64 + 4;
                    }
                    values.push((kind, value));
                    p += usize::from(form.bytes());
                }
                let tail = bytes.get(p) == Some(&TAIL);
                let next = if tail {
                    p += 5;
                    (p as i64 + enc::read(Form::Rel32, bytes, p - 4)) as u32
                } else {
                    p as u32
                };
                out.push(Piece::Stencil(Decoded { at: at as u32, shape, tail, next, fields: values }));
                at = p;
            }
            RELAY => {
                out.push(Piece::Relay {
                    bytes: bytes[at + 1],
                    from: u32_at(bytes, at + 2),
                    to: u32_at(bytes, at + 6),
                });
                at += 10;
            }
            TRAP => {
                out.push(Piece::Glue { at: at as u32, byte: TRAP });
                at += 5;
            }
            0x0f => {
                assert_eq!(bytes[at + 1], STUB[1]);
                out.push(Piece::Glue { at: at as u32, byte: 0x0f });
                at += 2;
            }
            byte @ (PROLOGUE | RET | COUNT | LANDING) => {
                out.push(Piece::Glue { at: at as u32, byte });
                at += 1;
            }
            // `finish`'s padding of the code to its constants' alignment.
            byte if byte == Mock::NOP[0] => at += 1,
            byte => panic!("byte {byte:#x} at {at:#x} is no piece"),
        }
    }
    out
}

fn decode(code: &Code) -> Vec<Piece> {
    decode_to(code, code.bytes.len())
}

fn stencils(pieces: &[Piece]) -> Vec<Decoded> {
    pieces
        .iter()
        .filter_map(|it| match it {
            Piece::Stencil(it) => Some(it.clone()),
            _ => None,
        })
        .collect()
}

fn glue(pieces: &[Piece]) -> Vec<(u32, u8)> {
    pieces
        .iter()
        .filter_map(|it| match it {
            Piece::Glue { at, byte } => Some((*at, *byte)),
            _ => None,
        })
        .collect()
}

/// A function of `params` and `returns`, its entry block made with the parameters, the rest
/// by `body`.
fn function(
    params: &[ir::Type],
    returns: &[ir::Type],
    body: impl FnOnce(&mut FunctionBuilder<'_>, &[ir::Value]),
) -> ir::Function {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.extend(params.iter().map(|ty| AbiParam::new(*ty)));
    sig.returns.extend(returns.iter().map(|ty| AbiParam::new(*ty)));
    let mut func = ir::Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut ctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut func, &mut ctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        body(&mut b, &params);
        b.seal_all_blocks();
    }
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&func, &flags).unwrap_or_else(|e| panic!("{func}\n{e}"));
    func
}

fn compile(scratch: &mut Scratch, func: &ir::Function) -> Code {
    try_compile(scratch, func, &library()).unwrap_or_else(|e| panic!("{func}\n{e}"))
}

/// A compile against `library`, keeping what the tests read of its plan.
fn try_compile(scratch: &mut Scratch, func: &ir::Function, library: &Reader<'_>) -> Result<Code, Error> {
    compile_in::<Mock, _>(func, &TARGET, library, |code, plan| {
        scratch.plan = Seen::of(plan);
        code.to_code()
    })
}

fn inst_of(func: &ir::Function, value: ir::Value) -> ir::Inst {
    match func.dfg.value_def(value) {
        ir::ValueDef::Result(inst, _) => inst,
        def => panic!("{value} is {def:?}"),
    }
}

/// The frame offset of `value`'s slot.
fn place(scratch: &Scratch, value: ir::Value) -> i64 {
    let slot = scratch.plan.slot[value.as_u32() as usize];
    assert_ne!(slot, NONE, "{value} has no slot");
    i64::from(scratch.plan.base + slot)
}

fn slot_of(scratch: &Scratch, value: ir::Value) -> u32 {
    scratch.plan.slot[value.as_u32() as usize]
}

fn shape(op: Op, ty: Ty, srcs: [Src; 3], dst: Dst) -> Shape {
    Shape { op, ty, aux: 0, srcs, dst }
}

const S: Src = Src::Slot;
const A: Src = Src::Acc;
const I: Src = Src::Imm;
const N: Src = Src::None;

#[test]
fn straight_line_code_is_stencils_back_to_back_through_the_accumulator() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let v = b.ins().iadd(p[0], p[1]);
        let w = b.ins().imul(v, p[0]);
        b.ins().return_(&[w]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let pieces = decode(&code);
    assert_eq!(glue(&pieces).iter().map(|it| it.1).collect::<Vec<_>>(), [PROLOGUE, RET]);
    let s = stencils(&pieces);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].shape, shape(Op::Iadd, Ty::I64, [S, S, N], Dst::Acc));
    assert_eq!(s[1].shape, shape(Op::Imul, Ty::I64, [A, S, N], Dst::Slot));
    assert!(!s[0].tail && !s[1].tail, "a straight line keeps no tail jump");
    assert_eq!(s[0].next, s[1].at);
    let p = func.dfg.block_params(func.layout.entry_block().unwrap()).to_vec();
    assert_eq!(s[0].field(HoleKind::Slot0), place(&scratch, p[0]));
    assert_eq!(s[0].field(HoleKind::Slot1), place(&scratch, p[1]));
    assert_eq!(s[1].field(HoleKind::Slot1), place(&scratch, p[0]));
    let v = func.dfg.inst_args(s_inst(&func, 1))[0];
    assert_eq!(inst_of(&func, v), s_inst(&func, 0));
    assert_eq!(slot_of(&scratch, v), NONE, "a value only the next stencil reads has no slot");
    assert_eq!(code.align, super::ALIGN);
    assert_eq!(code.size() as usize, code.bytes.len());
}

/// The `n`th instruction of the entry block.
fn s_inst(func: &ir::Function, n: usize) -> ir::Inst {
    func.layout.block_insts(func.layout.entry_block().unwrap()).nth(n).unwrap()
}

#[test]
fn constants_fold_into_immediates_or_take_a_stencil_of_their_own() {
    let func = function(&[types::I32], &[types::I32], |b, p| {
        let k = b.ins().iconst(types::I32, -1);
        let v = b.ins().iadd(p[0], k);
        b.ins().return_(&[v]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let s = stencils(&decode(&code));
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].shape, shape(Op::Iadd, Ty::I32, [S, I, N], Dst::Slot));
    assert_eq!(s[0].field(HoleKind::Imm1), -1);
    assert_eq!(scratch.plan.sels[s_inst(&func, 0).as_u32() as usize].kind, Kind::Skip);

    let func = function(&[types::I64], &[types::I64], |b, p| {
        let k = b.ins().iconst(types::I64, 1i64 << 40);
        let v = b.ins().iadd(p[0], k);
        b.ins().return_(&[v]);
    });
    let code = compile(&mut scratch, &func);
    let s = stencils(&decode(&code));
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].shape, Shape { aux: 1, ..shape(Op::Iconst, Ty::I64, [N; 3], Dst::Acc) });
    assert_eq!(s[0].field(HoleKind::Imm0), 1i64 << 40);
    assert_eq!(s[1].shape, shape(Op::Iadd, Ty::I64, [S, A, N], Dst::Slot));
}

#[test]
fn narrowing_and_widening_are_named_by_their_result_type() {
    let func = function(&[types::I32X4, types::I32X4], &[types::I32X4], |b, p| {
        let narrow = b.ins().snarrow(p[0], p[1]);
        let wide = b.ins().swiden_low(narrow);
        b.ins().return_(&[wide]);
    });
    let mut scratch = Scratch::new();
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.iter().map(|it| (it.shape.op, it.shape.ty)).collect::<Vec<_>>(), [
        (Op::Snarrow, Ty::I16x8),
        (Op::SwidenLow, Ty::I32x4)
    ]);
}

#[test]
fn local_slots_are_reused_after_their_last_use_and_never_by_an_operand() {
    let mut values = Vec::new();
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (a, x) = (p[0], p[1]);
        let v1 = b.ins().iadd(a, x);
        let v2 = b.ins().isub(a, x);
        let v3 = b.ins().imul(v1, v2);
        let v4 = b.ins().iadd(a, x);
        let v5 = b.ins().isub(a, x);
        let v6 = b.ins().imul(v4, v5);
        let v7 = b.ins().iadd(v3, v6);
        b.ins().return_(&[v7]);
        values = vec![v1, v2, v3, v4, v5, v6, v7];
    });
    let mut scratch = Scratch::new();
    compile(&mut scratch, &func);
    let [v1, v2, v3, v4, _, _, v7] = values[..] else { unreachable!() };
    assert_eq!(slot_of(&scratch, v2), NONE);
    assert_ne!(slot_of(&scratch, v3), slot_of(&scratch, v1), "a result shares no slot with an operand");
    assert_eq!(slot_of(&scratch, v4), slot_of(&scratch, v1), "v1's slot is free after its last use");
    assert_ne!(slot_of(&scratch, v7), slot_of(&scratch, v3));
    let p = func.dfg.block_params(func.layout.entry_block().unwrap()).to_vec();
    assert_ne!(slot_of(&scratch, p[0]), slot_of(&scratch, p[1]));
    for v in [v1, v3, v4, v7] {
        assert!(![p[0], p[1]].iter().any(|it| slot_of(&scratch, *it) == slot_of(&scratch, v)));
    }
}

#[test]
fn values_other_blocks_read_keep_a_slot_of_their_own() {
    let mut values = Vec::new();
    let func = function(&[types::I64], &[types::I64], |b, p| {
        let next = b.create_block();
        let x = b.ins().iadd(p[0], p[0]);
        b.ins().jump(next, &[]);
        b.switch_to_block(next);
        let y = b.ins().iadd(x, x);
        let w = b.ins().isub(y, y);
        let z = b.ins().iadd(w, x);
        b.ins().return_(&[z]);
        values = vec![x, y, w, z];
    });
    let mut scratch = Scratch::new();
    compile(&mut scratch, &func);
    let [x, y, w, z] = values[..] else { unreachable!() };
    assert_eq!(scratch.plan.last[x.as_u32() as usize], GLOBAL);
    for local in [y, w, z] {
        let slot = slot_of(&scratch, local);
        if slot != NONE {
            assert_ne!(slot, slot_of(&scratch, x), "{local} took the slot of a value live across blocks");
        }
    }
    assert_ne!(slot_of(&scratch, y), NONE, "y has two uses: its slot and the accumulator");
}

#[test]
fn icmp_and_brif_fuse_and_fall_through_to_the_next_block() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (then, other) = (b.create_block(), b.create_block());
        let c = b.ins().icmp(IntCC::SignedLessThan, p[0], p[1]);
        b.ins().brif(c, then, &[], other, &[]);
        b.switch_to_block(then);
        b.ins().return_(&[p[0]]);
        b.switch_to_block(other);
        b.ins().return_(&[p[1]]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let pieces = decode(&code);
    let s = stencils(&pieces);
    assert_eq!(s.len(), 1);
    let fused = Shape {
        aux: IntCc::SignedGreaterThanOrEqual as u8,
        ..shape(Op::BrIcmp, Ty::I64, [S, S, N], Dst::None)
    };
    assert_eq!(s[0].shape, fused, "complemented so the taken block falls through");
    assert!(!s[0].tail);
    let rets: Vec<u32> = glue(&pieces).iter().filter(|it| it.1 == RET).map(|it| it.0).collect();
    assert_eq!(s[0].next, rets[0]);
    assert_eq!(s[0].field(HoleKind::Target), i64::from(rets[1]));
    assert_eq!(scratch.plan.sels[s_inst(&func, 0).as_u32() as usize].kind, Kind::Fused);

    // An `icmp` against a constant folds it.
    let func = function(&[types::I64], &[types::I64], |b, p| {
        let (then, other) = (b.create_block(), b.create_block());
        let c = b.ins().icmp_imm(IntCC::Equal, p[0], 7);
        b.ins().brif(c, other, &[], then, &[]);
        b.switch_to_block(then);
        b.ins().return_(&[p[0]]);
        b.switch_to_block(other);
        b.ins().return_(&[p[0]]);
    });
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].shape, Shape { aux: IntCc::Equal as u8, ..shape(Op::BrIcmp, Ty::I64, [S, I, N], Dst::None) });
    assert_eq!(s[0].field(HoleKind::Imm1), 7);

    // Not right before its branch: written apart.
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (then, other) = (b.create_block(), b.create_block());
        let c = b.ins().icmp(IntCC::Equal, p[0], p[1]);
        let d = b.ins().iadd(p[0], p[1]);
        b.ins().brif(c, then, &[], other, &[]);
        b.switch_to_block(then);
        b.ins().return_(&[d]);
        b.switch_to_block(other);
        b.ins().return_(&[p[0]]);
    });
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.iter().map(|it| it.shape.op).collect::<Vec<_>>(), [Op::Icmp, Op::Iadd, Op::Brif]);
}

#[test]
fn select_of_icmp_is_one_stencil_either_way_round() {
    let func = function(&[types::I64, types::I64], &[types::I64, types::I64], |b, p| {
        let c = b.ins().icmp(IntCC::UnsignedLessThan, p[0], p[1]);
        let min = b.ins().select(c, p[0], p[1]);
        let d = b.ins().icmp(IntCC::UnsignedLessThan, p[0], p[1]);
        let max = b.ins().select(d, p[1], p[0]);
        b.ins().return_(&[min, max]);
    });
    let mut scratch = Scratch::new();
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.len(), 2);
    let ult = IntCc::UnsignedLessThan as u8 | (Ty::I64 as u8) << 4;
    let ugt = IntCc::UnsignedGreaterThan as u8 | (Ty::I64 as u8) << 4;
    assert_eq!(s[0].shape, Shape { aux: ult, ..shape(Op::SelectIcmp, Ty::I64, [S, S, N], Dst::Slot) });
    assert_eq!(s[1].shape, Shape { aux: ugt, ..shape(Op::SelectIcmp, Ty::I64, [S, S, N], Dst::Slot) });
    let p = func.dfg.block_params(func.layout.entry_block().unwrap()).to_vec();
    assert_eq!(s[1].field(HoleKind::Slot0), place(&scratch, p[1]));
    assert_eq!(s[1].field(HoleKind::Slot1), place(&scratch, p[0]));
}

#[test]
fn an_fma_by_an_fneg_is_the_negated_fma_of_its_operand_wherever_the_fneg_is() {
    use crate::arch::aarch64::{Aarch64, tests::{LINUX, library_of}};
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    let library = Reader::new(BYTES.get_or_init(|| library_of(&[Op::Fma, Op::Fneg, Op::Fadd, Op::Copy, Op::Jump]))).unwrap();
    let negated = |srcs| Shape { aux: 1, ..shape(Op::Fma, Ty::F64, srcs, Dst::Slot) };
    let plan = |func: &ir::Function| {
        compile_in::<Aarch64, _>(func, &LINUX, &library, |_, plan| Seen::of(plan)).unwrap_or_else(|e| panic!("{e}"))
    };
    let sel = |plan: &Seen, func: &ir::Function, value| plan.sels[inst_of(func, value).as_u32() as usize];
    let mut values = Vec::new();

    // Its one use: the `fneg` is dropped and its operand handed on in the accumulator.
    let func = function(&[types::F64; 3], &[types::F64], |b, p| {
        let y = b.ins().fadd(p[1], p[1]);
        let n = b.ins().fneg(y);
        let r = b.ins().fma(p[0], n, p[2]);
        b.ins().return_(&[r]);
        values = vec![y, n, r];
    });
    let seen = plan(&func);
    assert_eq!(sel(&seen, &func, values[0]).shape, shape(Op::Fadd, Ty::F64, [S, S, N], Dst::Acc));
    assert_eq!(sel(&seen, &func, values[1]).kind, Kind::Fused);
    let fma = sel(&seen, &func, values[2]);
    assert_eq!((fma.kind, fma.shape), (Kind::Stencil, negated([S, A, S])));
    assert_eq!(fma.args[1], values[0]);

    // In another block and read there too: the `fneg` stays, and its operand, now read in
    // that block as well, keeps a slot of its own besides the accumulator.
    let func = function(&[types::F64; 3], &[types::F64, types::F64], |b, p| {
        let next = b.create_block();
        let y = b.ins().fadd(p[1], p[1]);
        let n = b.ins().fneg(y);
        b.ins().jump(next, &[]);
        b.switch_to_block(next);
        let r = b.ins().fma(p[0], n, p[2]);
        b.ins().return_(&[r, n]);
        values = vec![y, n, r];
    });
    let seen = plan(&func);
    assert_eq!(sel(&seen, &func, values[0]).shape, shape(Op::Fadd, Ty::F64, [S, S, N], Dst::Both));
    assert_eq!(sel(&seen, &func, values[1]).shape, shape(Op::Fneg, Ty::F64, [A, N, N], Dst::Slot));
    let fma = sel(&seen, &func, values[2]);
    assert_eq!(fma.shape, negated([S, S, S]));
    assert_eq!(fma.args[1], values[0]);
    let y = values[0].as_u32() as usize;
    assert_eq!(seen.last[y], GLOBAL);
    assert_ne!(seen.slot[y], NONE);
}

#[test]
fn block_arguments_move_in_parallel_and_a_swap_goes_through_the_scratch_slot() {
    let mut values = Vec::new();
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (body, exit) = (b.create_block(), b.create_block());
        let x = b.append_block_param(body, types::I64);
        let y = b.append_block_param(body, types::I64);
        b.ins().jump(body, &[BlockArg::Value(p[0]), BlockArg::Value(p[1])]);
        b.switch_to_block(body);
        let z = b.ins().iadd(x, y);
        b.ins().brif(z, body, &[BlockArg::Value(y), BlockArg::Value(x)], exit, &[]);
        b.switch_to_block(exit);
        b.ins().return_(&[z]);
        values = vec![x, y];
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let pieces = decode(&code);
    let [x, y] = values[..] else { unreachable!() };
    let (x, y) = (place(&scratch, x), place(&scratch, y));
    let p = func.dfg.block_params(func.layout.entry_block().unwrap()).to_vec();
    let copies: Vec<(i64, i64)> = stencils(&pieces)
        .iter()
        .filter(|it| it.shape.op == Op::Copy)
        .map(|it| (it.field(HoleKind::Slot0), it.field(HoleKind::Result0)))
        .collect();
    assert_eq!(
        copies,
        [(place(&scratch, p[0]), x), (place(&scratch, p[1]), y), (x, 0), (y, x), (0, y)],
        "the entry edge's two moves, then the swap through scratch slot 0"
    );
    let markers: Vec<u8> = glue(&pieces).iter().map(|it| it.1).collect();
    assert_eq!(markers, [PROLOGUE, COUNT, RET], "the back edge counts");
    let s = stencils(&pieces);
    let body = s.iter().find(|it| it.shape.op == Op::Iadd).unwrap().at;
    let brif = s.iter().find(|it| it.shape.op == Op::Brif).unwrap();
    let count = glue(&pieces).iter().find(|it| it.1 == COUNT).unwrap().0;
    let ret = glue(&pieces).iter().find(|it| it.1 == RET).unwrap().0;
    assert_eq!(brif.field(HoleKind::Target), i64::from(count), "the taken edge has code of its own");
    assert!(brif.tail && brif.next == ret, "the other edge is a jump to the exit");
    let jump = s.iter().find(|it| it.shape.op == Op::Jump).expect("the back edge's jump");
    assert_eq!(jump.next, body);
    assert_eq!(s.iter().filter(|it| it.shape.op == Op::Jump).count(), 1, "the entry edge falls through");
}

#[test]
fn far_slots_are_relayed_through_near_ones() {
    let params = [types::I64; 140];
    let func = function(&params, &[types::I64], |b, p| {
        let mut sum = b.ins().iadd(p[0], p[1]);
        for &param in &p[2..] {
            sum = b.ins().iadd(sum, param);
        }
        b.ins().return_(&[sum]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    assert!(scratch.plan.relays);
    assert_eq!(scratch.plan.base, 32 + 16 * 5);
    let pieces = decode(&code);
    let insts: Vec<ir::Inst> = func
        .layout
        .block_insts(func.layout.entry_block().unwrap())
        .filter(|inst| scratch.plan.sels[inst.as_u32() as usize].kind == Kind::Stencil)
        .collect();
    let mut far = 0;
    let mut n = 0;
    for (at, piece) in pieces.iter().enumerate() {
        let Piece::Stencil(it) = piece else { continue };
        let inst = insts[n];
        n += 1;
        assert_eq!(it.shape.op, Op::Iadd);
        if it.shape.srcs[0] != Src::Acc {
            continue;
        }
        let read = func.dfg.inst_args(inst)[1];
        let slot = place(&scratch, read) as u32;
        if slot >= Mock::NEAR[0] {
            far += 1;
            assert_eq!(pieces[at - 1], Piece::Relay { bytes: 8, from: slot, to: 32 + 16 });
            assert_eq!(it.field(HoleKind::Slot1), 48);
        } else {
            assert_eq!(it.field(HoleKind::Slot1), i64::from(slot));
        }
    }
    assert_eq!(far, (0..140).filter(|k| 112 + 8 * k >= 1024).count());
    let Some(Piece::Relay { from, .. }) = pieces.iter().rev().find(|it| matches!(it, Piece::Relay { .. })) else {
        panic!("the last result is far and relayed back");
    };
    assert_eq!(*from, 32 + 16 * 3);
}

#[test]
fn instructions_of_one_trap_code_share_its_stub() {
    let func = function(&[types::I64, types::I64, types::I64], &[types::I64], |b, p| {
        let q = b.ins().udiv(p[0], p[1]);
        let r = b.ins().udiv(q, p[2]);
        b.ins().trapz(p[0], TrapCode::unwrap_user(1));
        b.ins().trapz(p[1], TrapCode::unwrap_user(1));
        b.ins().return_(&[r]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let pieces = decode(&code);
    let stubs: Vec<u32> = glue(&pieces).iter().filter(|it| it.1 == STUB[0]).map(|it| it.0).collect();
    assert_eq!(stubs.len(), 2);
    let traps: Vec<i64> = stencils(&pieces).iter().map(|it| it.field(HoleKind::Trap)).collect();
    assert_eq!(traps.len(), 4);
    assert_eq!(traps[0], traps[1]);
    assert_eq!(traps[2], traps[3]);
    assert_ne!(traps[0], traps[2]);
    for trap in &traps {
        assert!(stubs.contains(&(*trap as u32)));
    }
    assert_eq!(
        code.traps,
        [(traps[0] as u32, TrapCode::INTEGER_DIVISION_BY_ZERO), (traps[2] as u32, TrapCode::unwrap_user(1))],
        "each stub's offset and code, by offset"
    );
    assert_eq!(code.trap_at(traps[2] as u32), Some(TrapCode::unwrap_user(1)));
    assert_eq!(code.trap_at(traps[2] as u32 + 1), None);
}

#[test]
fn a_jump_to_the_next_block_is_dropped_and_one_back_is_counted() {
    let func = function(&[types::I64], &[types::I64], |b, p| {
        let next = b.create_block();
        b.ins().jump(next, &[]);
        b.switch_to_block(next);
        b.ins().return_(&[p[0]]);
    });
    let mut scratch = Scratch::new();
    let pieces = decode(&compile(&mut scratch, &func));
    assert_eq!(pieces, [Piece::Glue { at: 0, byte: PROLOGUE }, Piece::Glue { at: 1, byte: RET }]);

    let func = function(&[types::I64], &[types::I64], |b, p| {
        let (body, exit) = (b.create_block(), b.create_block());
        b.ins().jump(body, &[]);
        b.switch_to_block(body);
        b.ins().brif(p[0], body, &[], exit, &[]);
        b.switch_to_block(exit);
        b.ins().return_(&[p[0]]);
    });
    let pieces = decode(&compile(&mut scratch, &func));
    let s = stencils(&pieces);
    let marks = glue(&pieces);
    assert_eq!(s.iter().map(|it| it.shape.op).collect::<Vec<_>>(), [Op::Brif, Op::Jump]);
    assert_eq!(s[0].field(HoleKind::Target), i64::from(marks[1].0));
    assert_eq!(marks[1].1, COUNT);
    assert_eq!(s[1].next, s[0].at, "the back edge jumps to its block after counting");
    assert_eq!(s[0].next, marks[2].0);
}

#[test]
fn constants_are_placed_once_each_16_aligned_after_the_code() {
    let a: Vec<u8> = (1..=16).collect();
    let b_bytes: Vec<u8> = (17..=32).collect();
    let func = function(&[], &[types::I8X16], |b, _| {
        let ka = b.func.dfg.constants.insert(ConstantData::from(a.clone()));
        let kb = b.func.dfg.constants.insert(ConstantData::from(b_bytes.clone()));
        let c1 = b.ins().vconst(types::I8X16, ka);
        let c2 = b.ins().vconst(types::I8X16, kb);
        let c3 = b.ins().vconst(types::I8X16, ka);
        let v = b.ins().band(c1, c2);
        let w = b.ins().band(v, c3);
        b.ins().return_(&[w]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let first = |pieces: &[Piece]| {
        stencils(pieces).iter().filter(|it| it.shape.op == Op::Vconst).map(|it| it.field(HoleKind::Const) as usize).min()
    };
    // The constants follow the code: decode up to the first one.
    let mut end = code.bytes.len();
    for at in (0..code.bytes.len()).step_by(16) {
        if code.bytes[at..].starts_with(&a) || code.bytes[at..].starts_with(&b_bytes) {
            end = at;
            break;
        }
    }
    let pieces = decode_to(&code, end);
    assert_eq!(first(&pieces), Some(end));
    let consts: Vec<usize> = stencils(&pieces)
        .iter()
        .filter(|it| it.shape.op == Op::Vconst)
        .map(|it| it.field(HoleKind::Const) as usize)
        .collect();
    assert_eq!(consts.len(), 3);
    assert_eq!(consts[0], consts[2], "one constant, placed once");
    assert_ne!(consts[0], consts[1]);
    for at in &consts {
        assert_eq!(at % 16, 0);
    }
    assert_eq!(code.bytes[consts[0]..consts[0] + 16], a[..]);
    assert_eq!(code.bytes[consts[1]..consts[1] + 16], b_bytes[..]);
    assert_eq!(code.bytes.len(), end + 32);
}

#[test]
fn a_result_nothing_reads_writes_a_scratch_slot() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (sum, _carry) = b.ins().uadd_overflow(p[0], p[1]);
        b.ins().return_(&[sum]);
    });
    let mut scratch = Scratch::new();
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s[0].shape.op, Op::UaddOverflow);
    assert_eq!(s[0].field(HoleKind::Result1), 16);
    assert!(s[0].field(HoleKind::Result0) >= i64::from(scratch.plan.base));
}

#[test]
fn stack_slots_follow_the_value_slots_at_their_alignment() {
    let mut slot = None;
    let func = function(&[types::I64], &[types::I64], |b, p| {
        let ss = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 40, 5));
        let addr = b.ins().stack_addr(types::I64, ss, 8);
        let v = b.ins().iadd(addr, p[0]);
        b.ins().return_(&[v]);
        slot = Some(ss);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let ss = slot.unwrap();
    let offset = scratch.plan.stack_slot(ss);
    assert_eq!(offset % 32, 0);
    assert!(offset >= scratch.plan.base);
    let area = scratch.plan.area();
    assert_eq!(area.align, 32);
    assert_eq!(area.size % 16, 0);
    assert!(area.size >= offset + 40);
    let s = stencils(&decode(&code));
    assert_eq!(s[0].shape, shape(Op::StackAddr, Ty::I64, [N; 3], Dst::Acc));
    assert_eq!(s[0].field(HoleKind::Offset), i64::from(offset) + 8);
    assert_eq!(code.slots.base, 6);
    assert_eq!(code.slots.offsets, [offset as i32 - 16]);
}

/// Cranelift 0.136's `stack_load`/`stack_store` are builder sugar for `stack_addr` and a
/// `load`/`store`, so there is no stack load or store opcode to select.
#[test]
fn stack_loads_and_stores_are_an_address_and_an_access() {
    let func = function(&[types::I64], &[types::I64], |b, p| {
        let ss = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
        b.ins().stack_store(types::I64, p[0], ss, 8);
        let v = b.ins().stack_load(types::I64, types::I64, ss, 8);
        b.ins().return_(&[v]);
    });
    let opcodes: Vec<Opcode> =
        func.layout.block_insts(func.layout.entry_block().unwrap()).map(|inst| func.dfg.insts[inst].opcode()).collect();
    assert_eq!(opcodes, [Opcode::StackAddr, Opcode::Store, Opcode::StackAddr, Opcode::Load, Opcode::Return]);
    let mut scratch = Scratch::new();
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.iter().map(|it| it.shape.op).collect::<Vec<_>>(), [
        Op::StackAddr,
        Op::Store,
        Op::StackAddr,
        Op::Load
    ]);
    let offset = i64::from(scratch.plan.stack_slot(ir::StackSlot::from_u32(0)));
    assert_eq!(s[0].field(HoleKind::Offset), offset + 8);
    assert_eq!(s[1].field(HoleKind::Offset), 0, "the store's own offset: the address has the slot's");
}

#[test]
fn memory_accesses_that_can_fault_are_trap_sites_with_their_flags_code() {
    let (oob, user) = (TrapCode::HEAP_OUT_OF_BOUNDS, TrapCode::unwrap_user(7));
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let ss = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let flags = ir::MemFlagsData::new();
        b.ins().store(flags.with_trap_code(Some(user)), p[1], p[0], 0);
        let v = b.ins().load(types::I64, flags.with_trap_code(Some(oob)), p[0], 8);
        b.ins().stack_store(types::I64, v, ss, 0);
        let w = b.ins().stack_load(types::I64, types::I64, ss, 0);
        let q = b.ins().udiv(w, p[1]);
        b.ins().return_(&[q]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let pieces = decode(&code);
    let s = stencils(&pieces);
    let at = |op: Op| s.iter().find(|it| it.shape.op == op).unwrap().at + u32::from(FAULT);
    let stub = glue(&pieces).iter().find(|it| it.1 == STUB[0]).unwrap().0;
    assert_eq!(
        code.traps,
        [(at(Op::Store), user), (at(Op::Load), oob), (stub, TrapCode::INTEGER_DIVISION_BY_ZERO)],
        "the `notrap` stack accesses are not trap sites"
    );
    assert_eq!(code.trap_at(at(Op::Load)), Some(oob));
}

#[test]
fn lines_change_where_the_source_location_does() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        b.set_srcloc(SourceLoc::new(1));
        let v = b.ins().iadd(p[0], p[1]);
        let w = b.ins().iadd(v, p[1]);
        b.set_srcloc(SourceLoc::new(2));
        let x = b.ins().imul(w, p[0]);
        b.ins().return_(&[x]);
    });
    let mut scratch = Scratch::new();
    let code = compile(&mut scratch, &func);
    let s = stencils(&decode(&code));
    assert_eq!(code.lines, [(s[0].at, SourceLoc::new(1)), (s[2].at, SourceLoc::new(2))]);
    assert_eq!(code.line_at(s[1].at), Some(SourceLoc::new(1)));
    assert_eq!(code.line_at(0), None);
}

#[test]
fn what_no_stencil_and_no_glue_takes_is_an_error_naming_it() {
    let func = function(&[], &[types::I64], |b, _| {
        let r = b.ins().get_stack_pointer(types::I64);
        b.ins().return_(&[r]);
    });
    let mut scratch = Scratch::new();
    let error = try_compile(&mut scratch, &func, &library()).unwrap_err();
    assert_eq!(error.inst, Some(s_inst(&func, 0)));
    assert!(error.what.contains("get_stack_pointer"), "{error}");

    static NO_UDIV: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = NO_UDIV.get_or_init(|| build(|shape| OPS.contains(&shape.op) && shape.op != Op::Udiv));
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let q = b.ins().udiv(p[0], p[1]);
        b.ins().return_(&[q]);
    });
    let error = try_compile(&mut scratch, &func, &Reader::new(bytes).unwrap()).unwrap_err();
    assert_eq!(error.inst, Some(s_inst(&func, 0)));
    assert!(error.what.contains("udiv") && error.what.contains("i64"), "{error}");
}

#[test]
fn the_survey_lists_every_refused_instruction_by_its_opcode_and_type() {
    static NO_UDIV: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = NO_UDIV.get_or_init(|| build(|shape| OPS.contains(&shape.op) && shape.op != Op::Udiv));
    let library = Reader::new(bytes).unwrap();
    let func = function(&[types::I64, types::I64, types::I32, types::I32], &[types::I64], |b, p| {
        let sp = b.ins().get_stack_pointer(types::I64);
        let q = b.ins().udiv(p[0], p[1]);
        let r = b.ins().udiv(p[2], p[3]);
        let r = b.ins().uextend(types::I64, r);
        let s = b.ins().iadd(q, r);
        let s = b.ins().iadd(s, sp);
        b.ins().return_(&[s]);
    });
    let gaps: Vec<_> = survey_in::<Mock>(&func, &TARGET, &library).iter().map(|e| (e.inst, e.gap)).collect();
    let gap = |opcode, ty| Some(crate::Gap { opcode, ty });
    assert_eq!(
        gaps,
        [
            (Some(s_inst(&func, 0)), gap(ir::Opcode::GetStackPointer, types::I64)),
            (Some(s_inst(&func, 1)), gap(ir::Opcode::Udiv, types::I64)),
            (Some(s_inst(&func, 2)), gap(ir::Opcode::Udiv, types::I32)),
        ]
    );
    let mut scratch = Scratch::new();
    let first = try_compile(&mut scratch, &func, &library).unwrap_err();
    assert_eq!(first.gap, gap(ir::Opcode::GetStackPointer, types::I64), "a compile stops at the first, typed");
}

#[test]
fn the_survey_of_a_function_that_compiles_is_empty() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let s = b.ins().iadd(p[0], p[1]);
        b.ins().return_(&[s]);
    });
    assert_eq!(survey_in::<Mock>(&func, &TARGET, &library()), []);
}

#[test]
fn without_accumulator_shapes_every_value_goes_through_its_slot() {
    static SLOTS_ONLY: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = SLOTS_ONLY.get_or_init(|| {
        build(|shape| {
            OPS.contains(&shape.op)
                && !shape.srcs.contains(&Src::Acc)
                && !matches!(shape.dst, Dst::Acc | Dst::Both)
        })
    });
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let v = b.ins().iadd(p[0], p[1]);
        let w = b.ins().imul(v, p[0]);
        b.ins().return_(&[w]);
    });
    let mut scratch = Scratch::new();
    let code = try_compile(&mut scratch, &func, &Reader::new(bytes).unwrap()).unwrap();
    let s = stencils(&decode(&code));
    assert_eq!(s[0].shape, shape(Op::Iadd, Ty::I64, [S, S, N], Dst::Slot));
    assert_eq!(s[1].shape, shape(Op::Imul, Ty::I64, [S, S, N], Dst::Slot));
    assert_eq!(s[1].field(HoleKind::Slot0), s[0].field(HoleKind::Result0));
}

#[test]
fn a_warm_compile_into_a_reused_code_allocates_nothing() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (body, exit) = (b.create_block(), b.create_block());
        let x = b.append_block_param(body, types::I64);
        let y = b.append_block_param(body, types::I64);
        b.ins().jump(body, &[BlockArg::Value(p[0]), BlockArg::Value(p[1])]);
        b.switch_to_block(body);
        b.set_srcloc(SourceLoc::new(3));
        let z = b.ins().iadd(x, y);
        let q = b.ins().udiv(z, y);
        b.ins().brif(q, body, &[BlockArg::Value(y), BlockArg::Value(x)], exit, &[]);
        b.switch_to_block(exit);
        b.ins().return_(&[z]);
    });
    let library = library();
    let mut code = Code::default();
    let into = |code: &mut Code| {
        compile_in::<Mock, _>(&func, &TARGET, &library, |it, _| code.copy_from(it)).unwrap();
    };
    into(&mut code);
    let ((), used) = blaze_alloc_count::measure(|| into(&mut code));
    assert_eq!(
        used,
        blaze_alloc_count::Used::default(),
        "a warm compile: its columns on the frame, its code into the vectors of the last"
    );
    assert!(!code.bytes.is_empty() && !code.lines.is_empty() && !code.traps.is_empty());
    assert!(blaze_mem::frame::spare_frames() > 0, "the compile's frame went back to the thread's spares");
}

#[test]
fn copying_into_a_reused_code_gives_the_fresh_code() {
    let small = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let q = b.ins().udiv(p[0], p[1]);
        b.ins().return_(&[q]);
    });
    let big = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let c = b.ins().icmp(IntCC::Equal, p[0], p[1]);
        let m = b.ins().select(c, p[0], p[1]);
        let q = b.ins().udiv(m, p[1]);
        let r = b.ins().iadd(q, p[0]);
        b.ins().return_(&[r]);
    });
    let library = library();
    let mut reused = Code::default();
    for func in [&big, &small, &big] {
        compile_in::<Mock, _>(func, &TARGET, &library, |it, _| reused.copy_from(it)).unwrap();
        assert_eq!(reused, compile(&mut Scratch::new(), func), "{func}");
    }
}

#[test]
fn many_constants_are_each_placed_once_in_first_use_order() {
    let values: Vec<[u8; 16]> = (0..20u8).map(|i| [i.wrapping_mul(13).wrapping_add(1); 16]).collect();
    let func = function(&[], &[types::I8X16], |b, _| {
        let handles: Vec<_> =
            values.iter().map(|it| b.func.dfg.constants.insert(ConstantData::from(it.to_vec()))).collect();
        let mut acc = b.ins().vconst(types::I8X16, handles[0]);
        for i in 1..40 {
            let k = b.ins().vconst(types::I8X16, handles[i % 20]);
            acc = b.ins().band(acc, k);
        }
        b.ins().return_(&[acc]);
    });
    let first = compile(&mut Scratch::new(), &func);
    let again = compile(&mut Scratch::new(), &func);
    assert_eq!(first, again, "the constant area is laid out the same each compile");
    let mut at = first.bytes.len() - 20 * 16;
    for value in &values {
        assert_eq!(first.bytes[at..at + 16], value[..], "each constant once, in the order first used");
        at += 16;
    }
}

#[test]
fn compiling_the_same_function_twice_gives_the_same_code() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let c = b.ins().icmp(IntCC::Equal, p[0], p[1]);
        let m = b.ins().select(c, p[0], p[1]);
        let q = b.ins().udiv(m, p[1]);
        b.ins().return_(&[q]);
    });
    let mut scratch = Scratch::new();
    let first = compile(&mut scratch, &func);
    let again = compile(&mut scratch, &func);
    let fresh = compile(&mut Scratch::new(), &func);
    assert_eq!(first, again);
    assert_eq!(first, fresh);
}

/// The values of a confined access's address, in the order [`confined`] makes them.
const SUM: usize = 0;
const REDUCE: usize = 1;
const EXTEND: usize = 2;
const ADDRESS: usize = 3;

/// A load (or a store of `x`) at `offset` from `iadd vmctx, uextend.i64 (ireduce.i32 sum)`, `sum`
/// being `iadd p, iconst off` or, without `off`, `p`, of the parameters `[p, vmctx, x]`. The
/// value `again` names is returned too, a second use. The address's values go to `parts`.
fn confined(
    off: Option<i64>,
    offset: i32,
    store: bool,
    again: Option<usize>,
    parts: &mut Vec<ir::Value>,
) -> ir::Function {
    let mut returns = Vec::new();
    if !store {
        returns.push(types::I64);
    }
    returns.extend(again.map(|n| if n == REDUCE { types::I32 } else { types::I64 }));
    function(&[types::I64; 3], &returns, |b, p| {
        let flags = ir::MemFlagsData::new().with_trap_code(Some(TrapCode::HEAP_OUT_OF_BOUNDS));
        let sum = match off {
            Some(off) => {
                let k = b.ins().iconst(types::I64, off);
                b.ins().iadd(p[0], k)
            }
            None => p[0],
        };
        let reduce = b.ins().ireduce(types::I32, sum);
        let extend = b.ins().uextend(types::I64, reduce);
        let address = b.ins().iadd(p[1], extend);
        let mut results = Vec::new();
        if store {
            b.ins().store(flags, p[2], address, offset);
        } else {
            results.push(b.ins().load(types::I64, flags, address, offset));
        }
        *parts = vec![sum, reduce, extend, address];
        results.extend(again.map(|n| parts[n]));
        b.ins().return_(&results);
    })
}

#[test]
fn a_confined_access_is_one_stencil_of_its_address_and_access() {
    let wraps = (1i64 << 32) + 5;
    for (off, disp, offset) in [
        (None, Disp::Zero, None),
        (Some(0), Disp::Zero, None),
        (Some(16), Disp::Imm32, Some(16)),
        (Some(-8), Disp::Imm32, Some(-8)),
        (Some(wraps), Disp::Imm32, Some(5)),
    ] {
        for store in [false, true] {
            let mut parts = Vec::new();
            let func = confined(off, 0, store, None, &mut parts);
            let mut scratch = Scratch::new();
            let code = compile(&mut scratch, &func);
            let s = stencils(&decode(&code));
            assert_eq!(s.len(), 1, "{func}");
            let access = if store { Op::Store } else { Op::Load };
            assert_eq!(s[0].shape.confined(), Some((access, disp)), "{func}");
            let p = func.dfg.block_params(func.layout.entry_block().unwrap()).to_vec();
            let operands = if store { vec![p[2], p[0], p[1]] } else { vec![p[0], p[1]] };
            for (kind, value) in [HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2].into_iter().zip(operands) {
                assert_eq!(s[0].field(kind), place(&scratch, value), "{func}");
            }
            let hole = s[0].fields.iter().find(|it| it.0 == HoleKind::Offset).map(|it| it.1);
            assert_eq!(hole, offset, "the offset wrapped to 32 bits: {func}");
            for value in &parts[usize::from(off.is_none())..] {
                assert_eq!(scratch.plan.sels[inst_of(&func, *value).as_u32() as usize].kind, Kind::Fused);
            }
            assert_eq!(
                code.traps,
                [(s[0].at + u32::from(FAULT), TrapCode::HEAP_OUT_OF_BOUNDS)],
                "the access's fault keeps its code: {func}"
            );
        }
    }
}

#[test]
fn a_confined_access_is_apart_when_it_has_an_offset_or_a_value_of_its_address_another_use() {
    let ops = |code: &Code| stencils(&decode(code)).iter().map(|it| it.shape.op).collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut apart = [Op::Iadd, Op::Ireduce, Op::Uextend, Op::Iadd, Op::Load];
    let func = confined(Some(16), 8, false, None, &mut parts);
    assert_eq!(ops(&compile(&mut Scratch::new(), &func)), apart, "{func}");
    for again in [REDUCE, EXTEND, ADDRESS] {
        let func = confined(Some(16), 0, false, Some(again), &mut parts);
        assert_eq!(ops(&compile(&mut Scratch::new(), &func)), apart, "{func}");
    }

    let func = confined(Some(16), 0, false, Some(SUM), &mut parts);
    let s = stencils(&decode(&compile(&mut Scratch::new(), &func)));
    assert_eq!(s.iter().map(|it| it.shape.op).collect::<Vec<_>>(), [Op::Iadd, Op::LoadConfined]);
    assert_eq!(s[1].shape.confined(), Some((Op::Load, Disp::Zero)), "the sum, used again, is `p`");

    static UNFUSED: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = UNFUSED.get_or_init(|| build(|shape| OPS.contains(&shape.op) && shape.confined().is_none()));
    let library = Reader::new(bytes).unwrap();
    for store in [false, true] {
        apart[4] = if store { Op::Store } else { Op::Load };
        let func = confined(Some(16), 0, store, None, &mut parts);
        let code = try_compile(&mut Scratch::new(), &func, &library).unwrap();
        assert_eq!(ops(&code), apart, "without the fused shape: {func}");
    }
}

#[test]
fn a_confined_access_keeps_its_operands_live_to_itself() {
    let mut values = Vec::new();
    let func = function(&[types::I64; 3], &[types::I64, types::I64], |b, p| {
        let sum = b.ins().iadd(p[0], p[2]);
        let reduce = b.ins().ireduce(types::I32, sum);
        let extend = b.ins().uextend(types::I64, reduce);
        let address = b.ins().iadd(p[1], extend);
        let y = b.ins().imul(p[2], p[2]);
        let z = b.ins().iadd(y, y);
        let v = b.ins().load(types::I64, ir::MemFlagsData::new(), address, 0);
        b.ins().return_(&[v, z]);
        values = vec![sum, y, z];
    });
    let mut scratch = Scratch::new();
    let s = stencils(&decode(&compile(&mut scratch, &func)));
    assert_eq!(s.last().unwrap().shape.confined(), Some((Op::Load, Disp::Zero)));
    let [sum, y, z] = values[..] else { unreachable!() };
    assert_eq!(s.last().unwrap().field(HoleKind::Slot0), place(&scratch, sum));
    for later in [y, z] {
        assert_ne!(slot_of(&scratch, later), slot_of(&scratch, sum), "{later} took the slot of `p`");
    }
}
