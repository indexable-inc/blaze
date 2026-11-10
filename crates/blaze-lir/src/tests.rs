//! blaze-lir's tests: the record and the text form round-trip byte for byte over generated
//! LIR (rule `persisted-state-round-trips`), the requests-only walk asks for what the full
//! build refers to, corrupted bytes are refused and never panic a reader, and the verifier
//! refuses each rule's breach.
//!
//! The generator draws a plan of plain numbers first and builds it after, so one plan builds
//! the same function into either sink.

use std::alloc::Global;

use blaze_format::{Aligned, SpanRef};
use blaze_ids::ContentId;
use hegel::{TestCase, generators as gs};

use crate::{
    builder::{
        AsmOperand, AsmPiece, BuildError, Builder, DataReloc, Facts, Full, MemAttrs, Param, RegKind,
        RequestsOnly, Sink, Variadic,
    },
    flags::{AtomicRmw, FlagClass, Flags, FloatCond, IntCond, Ordering},
    func::{
        AsmId, AsmOperandKind, Block, BlockKind, Conv, Ext, FnAttrs, Func, IntrinsicId, MemAttrsRec,
        ParamFactsRec, ParamKind, RefId, RefKind, SigId, SlotId, SlotKind, Value,
    },
    op::Op,
    parse, persist, print,
    ty::{Scalar, Ty},
    verify::{self, verify},
};

const fn must(ty: Option<Ty>) -> Ty {
    match ty {
        Some(ty) => ty,
        None => panic!("a vector LIR has"),
    }
}

const V4I32: Ty = must(Ty::vector(Scalar::I32, 4));
const V2F64: Ty = must(Ty::vector(Scalar::F64, 2));
const V16I8: Ty = must(Ty::vector(Scalar::I8, 16));

/// The types a plan's numbers pick from.
const TYS: [Ty; 13] = [
    Ty::I1,
    Ty::I8,
    Ty::I16,
    Ty::I32,
    Ty::I64,
    Ty::I128,
    Ty::F16,
    Ty::F32,
    Ty::F64,
    Ty::F128,
    Ty::PTR,
    V4I32,
    V2F64,
];

const INTS: [Ty; 7] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128, V4I32, V16I8];
const SCALAR_INTS: [Ty; 5] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128];
const FLOATS: [Ty; 5] = [Ty::F16, Ty::F32, Ty::F64, Ty::F128, V2F64];

#[derive(Clone, Debug)]
struct Plan {
    ptr_bits: u32,
    params: Vec<u32>,
    rets: Vec<u32>,
    blocks: Vec<BlockPlan>,
}

#[derive(Clone, Debug)]
struct BlockPlan {
    params: Vec<u32>,
    cold: bool,
    ops: Vec<Vec<u32>>,
    term: Vec<u32>,
}

fn words(tc: &TestCase, n: usize) -> Vec<u32> {
    tc.draw(gs::vecs(gs::integers::<u32>()).min_size(n).max_size(n))
}

fn plan(tc: &TestCase) -> Plan {
    let ptr_bits = if tc.draw(gs::booleans()) { 64 } else { 32 };
    let params = tc.draw(gs::vecs(gs::integers::<u32>()).max_size(4));
    let rets = tc.draw(gs::vecs(gs::integers::<u32>()).max_size(2));
    let blocks = tc.draw(gs::integers::<u32>().min_value(1).max_value(5));
    let blocks = (0..blocks)
        .map(|_| BlockPlan {
            params: tc.draw(gs::vecs(gs::integers::<u32>()).max_size(2)),
            cold: tc.draw(gs::booleans()),
            ops: tc.draw(gs::vecs(gs::vecs(gs::integers::<u32>()).min_size(4).max_size(4)).max_size(10)),
            term: words(tc, 4),
        })
        .collect();
    Plan { ptr_bits, params, rets, blocks }
}

fn pick<T: Copy>(of: &[T], word: u32) -> T {
    of[word as usize % of.len()]
}

/// The rows every generated function may name.
struct Tables {
    callee: RefId,
    callee_sig: SigId,
    pair: RefId,
    pair_sig: SigId,
    data: RefId,
    tls: RefId,
    slot: SlotId,
    pause: IntrinsicId,
    nop: AsmId,
    mov: AsmId,
}

/// Builds a plan into a sink.
struct Gen<S: Sink> {
    b: Builder<Global, S>,
    pool: Vec<(Value, Ty)>,
    /// How many of `pool` are the entry block's, which dominate every block.
    entry: usize,
    t: Tables,
    blocks: Vec<Block>,
    block_params: Vec<Vec<Ty>>,
    rets: Vec<Ty>,
}

impl<S: Sink> Gen<S> {
    fn new(plan: &Plan) -> Gen<S> {
        let mut b = Builder::<Global, S>::new_in(plan.ptr_bits, Global);
        let params: Vec<Param> = plan.params.iter().map(|&w| param(pick(&TYS, w), w)).collect();
        let rets: Vec<Ty> = plan.rets.iter().map(|&w| pick(&TYS, w)).collect();
        let ret_params: Vec<Param> = rets.iter().map(|&ty| Param::of(ty)).collect();
        let sig = b.sig(Conv::C, Variadic::No, &params, &ret_params);
        b.set_signature(sig);
        b.set_attrs(FnAttrs(FnAttrs::NOUNWIND).with_inline(crate::func::Inline::Hint));
        b.set_features("+avx2,+fma");
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let pair_sig =
            b.sig(Conv::SysV64, Variadic::No, &[], &[Param::of(Ty::I64), Param::of(Ty::F64)]);
        let callee = b.reference(RefKind::Func, 7);
        let pair = b.reference(RefKind::Func, 9);
        let data = b.data(b"hello", 0, &[]);
        let ptr_bytes = plan.ptr_bits / 8;
        b.data(&[0; 16], 3, &[DataReloc { offset: 16 - ptr_bytes, target: callee, addend: -4 }]);
        let tls = b.reference(RefKind::Tls, 3);
        let slot = b.slot(16, 3, SlotKind::Named);
        let void = b.sig(Conv::C, Variadic::No, &[], &[]);
        let pause = b.intrinsic("llvm.x86.sse2.pause", void);
        let nop = b.asm_block(&[AsmPiece::Text("nop")], &[], crate::func::AsmRec::NOMEM, "");
        let mov = b.asm_block(
            &[
                AsmPiece::Text("mov "),
                AsmPiece::Operand { index: 0, modifier: None },
                AsmPiece::Text(", "),
                AsmPiece::Operand { index: 1, modifier: Some('e') },
            ],
            &[
                AsmOperand { kind: AsmOperandKind::Out, reg: "reg", reg_kind: RegKind::Class, aux: 0 },
                AsmOperand { kind: AsmOperandKind::In, reg: "rax", reg_kind: RegKind::Reg, aux: 0 },
            ],
            crate::func::AsmRec::PURE | crate::func::AsmRec::NOMEM,
            "C",
        );
        let blocks: Vec<Block> = plan
            .blocks
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let kind = if i > 0 && it.cold { BlockKind::Cold } else { BlockKind::Normal };
                b.create_block(kind)
            })
            .collect();
        let block_params = plan
            .blocks
            .iter()
            .enumerate()
            .map(|(i, it)| match i {
                0 => params.iter().map(|it| Ty(it.ty.0)).collect(),
                _ => it.params.iter().map(|&w| pick(&TYS, w)).collect(),
            })
            .collect();
        Gen {
            b,
            pool: Vec::new(),
            entry: 0,
            t: Tables { callee, callee_sig, pair, pair_sig, data, tls, slot, pause, nop, mov },
            blocks,
            block_params,
            rets,
        }
    }

    fn keep(&mut self, value: Value) -> Value {
        let ty = self.b.ty(value);
        self.pool.push((value, ty));
        value
    }

    /// A value of `ty`: one made before in a block that dominates this one, or a constant.
    fn value(&mut self, ty: Ty, seed: u32) -> Value {
        let have: Vec<Value> = self.pool.iter().filter(|it| it.1 == ty).map(|it| it.0).collect();
        if !have.is_empty() && seed % 3 != 0 {
            return pick(&have, seed / 3);
        }
        let made = match ty.form() {
            crate::ty::Form::Vector => {
                let n = ty.const_words() as usize;
                let bits = ty.bits(self.b.ptr_bits());
                let mut words = vec![seed; n];
                let spare = 32 * n as u32 - bits;
                if spare > 0 {
                    words[n - 1] &= u32::MAX >> spare;
                }
                self.b.vconst(ty, &words)
            }
            crate::ty::Form::Scalar | crate::ty::Form::Scalable | crate::ty::Form::Compound => {
                match ty.elem() {
                    Scalar::Ptr => self.b.null(),
                    Scalar::F16 | Scalar::F32 | Scalar::F64 | Scalar::F128 => {
                        self.b.fconst(ty, (seed as u128) * 0x9e37_79b9_7f4a_7c15)
                    }
                    Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::I128 => {
                        let value = match seed % 4 {
                            0 => seed as u128,
                            1 => ((seed as u128) << 40) | 0xffff,
                            2 => u128::MAX - seed as u128,
                            _ => (seed as u128) << 100,
                        };
                        self.b.iconst(ty, value)
                    }
                    Scalar::None => self.b.uninit(Ty::I8),
                }
            }
        };
        self.keep(made)
    }

    fn flags(class: FlagClass, word: u32) -> Flags {
        Flags(class.allowed().0 & word as u8)
    }

    fn op(&mut self, w: &[u32]) {
        let (a, x, y) = (w[1], w[2], w[3]);
        if y % 7 == 0 {
            let span = SpanRef { anchor: ContentId([a as u8; 32]), index: x % 3 };
            self.b.set_loc(Some(span));
        } else if y % 7 == 1 {
            self.b.set_loc(None);
        }
        match w[0] % 16 {
            0 => {
                let ty = pick(&INTS, a);
                let op = pick(
                    &[
                        Op::Add, Op::Sub, Op::Mul, Op::And, Op::Or, Op::Xor, Op::Shl, Op::Lshr, Op::Ashr,
                        Op::Rotl, Op::Rotr, Op::Umin, Op::Smax, Op::UaddSat, Op::SsubSat, Op::Umulhi,
                    ],
                    x,
                );
                let (l, r) = (self.value(ty, y), self.value(ty, x ^ y));
                let made = self.b.binary(op, Self::flags(op.info().flags, y >> 8), l, r);
                self.keep(made);
                if y & 1 == 0 {
                    let unary = pick(&[Op::Neg, Op::Not, Op::Clz, Op::Ctz, Op::Popcnt, Op::Bitrev], y >> 3);
                    let made = self.b.unary(unary, Self::flags(unary.info().flags, x), made);
                    self.keep(made);
                }
            }
            1 => {
                let ty = pick(&SCALAR_INTS, a);
                let op = pick(&[Op::Udiv, Op::Sdiv, Op::Urem, Op::Srem], x);
                let (l, r) = (self.value(ty, y), self.value(ty, x));
                let made = self.b.binary(op, Self::flags(op.info().flags, y), l, r);
                self.keep(made);
            }
            2 => {
                let ty = pick(&[Ty::I8, Ty::I32, Ty::I64, Ty::PTR, V4I32], a);
                let (l, r) = (self.value(ty, y), self.value(ty, x));
                let made = self.b.icmp(pick(&IntCond::ALL, x), l, r);
                self.keep(made);
            }
            3 => {
                let ty = pick(&FLOATS, a);
                let (l, r) = (self.value(ty, y), self.value(ty, x));
                let made = self.b.fcmp(pick(&FloatCond::ALL, x), l, r);
                self.keep(made);
            }
            4 => {
                let ty = pick(&TYS, a);
                let cond = self.value(Ty::I1, x);
                let (l, r) = (self.value(ty, y), self.value(ty, x ^ 5));
                let made = self.b.select(Self::flags(FlagClass::Select, y), cond, l, r);
                self.keep(made);
            }
            5 => {
                let ty = pick(&FLOATS, a);
                let op = pick(
                    &[Op::Fadd, Op::Fsub, Op::Fmul, Op::Fdiv, Op::Frem, Op::FminNum, Op::Fmaximum, Op::Fcopysign],
                    x,
                );
                let (l, r) = (self.value(ty, y), self.value(ty, x));
                let made = self.b.binary(op, Self::flags(FlagClass::Float, y >> 4), l, r);
                self.keep(made);
                let unary = pick(&[Op::Fneg, Op::Fabs, Op::Sqrt, Op::Floor, Op::Nearest, Op::Fround], y);
                let made = self.b.unary(unary, Self::flags(FlagClass::Float, x), made);
                self.keep(made);
                let made = self.b.ternary(Op::Fma, Flags::NONE, made, l, r);
                self.keep(made);
            }
            6 => {
                let ty = pick(&SCALAR_INTS, a);
                let op = pick(
                    &[
                        Op::UaddOverflow, Op::SaddOverflow, Op::UsubOverflow, Op::SsubOverflow,
                        Op::UmulOverflow, Op::SmulOverflow,
                    ],
                    x,
                );
                let (l, r) = (self.value(ty, y), self.value(ty, x));
                let pair = self.b.overflow(op, l, r);
                for k in 0..2 {
                    let made = self.b.project(pair, k);
                    self.keep(made);
                }
            }
            7 => {
                let (op, from, to) = pick(
                    &[
                        (Op::Zext, Ty::I8, Ty::I32),
                        (Op::Sext, Ty::I16, Ty::I64),
                        (Op::Trunc, Ty::I64, Ty::I8),
                        (Op::Fpext, Ty::F32, Ty::F64),
                        (Op::Fptrunc, Ty::F64, Ty::F16),
                        (Op::FptoSi, Ty::F64, Ty::I32),
                        (Op::FptoUi, Ty::F32, Ty::I64),
                        (Op::SitoFp, Ty::I32, Ty::F64),
                        (Op::UitoFp, Ty::I128, Ty::F32),
                        (Op::Bitcast, Ty::I32, Ty::F32),
                        (Op::Bitcast, V4I32, V2F64),
                        (Op::Freeze, Ty::I64, Ty::I64),
                    ],
                    a,
                );
                let v = self.value(from, y);
                let made = self.b.convert(op, Self::flags(op.info().flags, x), to, v);
                self.keep(made);
            }
            8 => {
                let slot = self.t.slot;
                if x % 4 == 0 {
                    self.b.slot_live(slot);
                }
                let base = self.b.slot_addr(slot);
                self.keep(base);
                let usize_ty = self.b.usize_ty();
                let offset = self.b.iconst(usize_ty, (y % 8) as u128);
                let p = self.b.ptr_add(Self::flags(FlagClass::PtrAdd, x), base, offset);
                self.keep(p);
                let ty = pick(&[Ty::I8, Ty::I32, Ty::I64, Ty::F64, Ty::PTR, V4I32], a);
                let attrs = if ty.elem() == Scalar::I8 {
                    mem(0, 0, 0, MemAttrsRec::NOUNDEF, Some((0, 1)))
                } else if ty.elem() == Scalar::Ptr {
                    mem(2, 0, 2, MemAttrsRec::NONNULL | MemAttrsRec::NOUNDEF, None)
                } else {
                    MemAttrs::aligned((y % 4) as u8)
                };
                let flags = Self::flags(FlagClass::Mem, x >> 2);
                let loaded = self.b.load(ty, flags, p, (y % 64) as i32 - 8, attrs);
                self.keep(loaded);
                let stored = self.value(ty, x);
                self.b.store(flags, stored, base, 4, attrs);
                if x % 4 == 1 {
                    self.b.slot_dead(slot);
                }
                let addr = self.b.convert(Op::PtrAddr, Flags::NONE, usize_ty, p);
                self.keep(addr);
                let made = self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, addr);
                self.keep(made);
            }
            9 => {
                let p = self.value(Ty::PTR, y);
                let attrs = MemAttrs::aligned(3);
                let volatile = if x & 1 == 0 { Flags::NONE } else { Flags::VOLATILE };
                let load_ord = pick(&[Ordering::SeqCst, Ordering::Relaxed, Ordering::Acquire], a);
                let made = self.b.atomic_load(Ty::I64, volatile.with_ordering(load_ord), p, attrs);
                self.keep(made);
                let store_ord = pick(&[Ordering::SeqCst, Ordering::Relaxed, Ordering::Release], x);
                let v = self.value(Ty::I64, x);
                self.b.atomic_store(volatile.with_ordering(store_ord), v, p, attrs);
                let rmw_ord = pick(&Ordering::ALL, y);
                let made = self.b.atomic_rmw(pick(&AtomicRmw::ALL, a), Flags::NONE.with_ordering(rmw_ord), p, v, attrs);
                self.keep(made);
                let weak = if y & 2 == 0 { Flags::NONE } else { Flags::WEAK };
                let failure = pick(&[Ordering::SeqCst, Ordering::Relaxed, Ordering::Acquire], y);
                let flags = weak.with_ordering(pick(&Ordering::ALL, x)).with_failure(failure);
                let pair = self.b.cmpxchg(flags, p, v, made, attrs);
                let made = self.b.project(pair, 1);
                self.keep(made);
                let fence = pick(&[Ordering::SeqCst, Ordering::Acquire, Ordering::Release, Ordering::AcqRel], a);
                let scope = if x & 4 == 0 { Flags::NONE } else { Flags::SINGLE_THREAD };
                self.b.fence(scope.with_ordering(fence));
            }
            10 => {
                let arg = self.value(Ty::I64, y);
                let cold = if x & 1 == 0 { Flags::NONE } else { Flags::COLD };
                let made = self.b.call(cold, self.t.callee, self.t.callee_sig, &[arg]);
                self.keep(made);
                let pair = self.b.call(Flags::NONE, self.t.pair, self.t.pair_sig, &[]);
                let made = self.b.project(pair, x % 2);
                self.keep(made);
                let callee = self.b.func_addr(self.t.callee);
                let arg = self.value(Ty::I64, a);
                let made = self.b.call_indirect(Flags::NONE, callee, self.t.callee_sig, &[arg]);
                self.keep(made);
                let made = self.b.data_addr(self.t.data);
                self.keep(made);
                let made = self.b.tls_addr(self.t.tls);
                self.keep(made);
            }
            11 => {
                let lane = self.value(Ty::I32, y);
                let splat = self.b.convert(Op::Splat, Flags::NONE, V4I32, lane);
                self.keep(splat);
                let other = self.value(V4I32, x);
                let made = self.b.insert(other, lane, a % 4);
                self.keep(made);
                let made = self.b.extract(made, x % 4);
                self.keep(made);
                let mask = [a % 8, x % 8, y % 8, (a ^ x) % 8];
                let shuffled = self.b.shuffle(V4I32, splat, other, &mask);
                self.keep(shuffled);
                let made = self.b.convert(Op::ReduceAdd, Flags::NONE, Ty::I32, shuffled);
                self.keep(made);
                let lanes = self.b.icmp(IntCond::Slt, splat, other);
                self.keep(lanes);
                let made = self.b.convert(Op::Bitmask, Flags::NONE, Ty::I8, lanes);
                self.keep(made);
                let start = self.value(Ty::F64, y);
                let vector = self.value(V2F64, x);
                let made = self.b.reduce_ordered(Op::ReduceFadd, Self::flags(FlagClass::Float, a), start, vector);
                self.keep(made);
            }
            12 => {
                self.b.target_intrinsic(self.t.pause, &[]);
            }
            13 => {
                self.b.asm(self.t.nop, Ty::NONE, &[]);
                let input = self.value(Ty::I64, y);
                let made = self.b.asm(self.t.mov, Ty::I64, &[input]);
                self.keep(made);
            }
            14 => {
                let (dst, src) = (self.value(Ty::PTR, y), self.value(Ty::PTR, x));
                let usize_ty = self.b.usize_ty();
                let len = self.value(usize_ty, a);
                let attrs = mem(3, 2, 0, 0, None);
                let op = pick(&[Op::Memcpy, Op::Memmove], a);
                self.b.bulk(op, Self::flags(FlagClass::Bulk, x), dst, src, len, attrs);
                let byte = self.value(Ty::I8, y);
                self.b.bulk(Op::Memset, Flags::NONE, dst, byte, len, MemAttrs::aligned(0));
                let made = self.b.memcmp(dst, src, len, MemAttrs::default());
                self.keep(made);
            }
            _ => {
                let cond = self.value(Ty::I1, x);
                self.b.assume(cond);
                let ty = pick(&TYS, a);
                let v = self.value(ty, y);
                let made = self.b.unary(Op::Freeze, Flags::NONE, v);
                self.keep(made);
                let made = self.b.machine(pick(&[Op::FrameAddress, Op::ReturnAddress, Op::StackPointer], y));
                self.keep(made);
            }
        }
    }

    /// The arguments block `to` takes.
    fn args(&mut self, to: usize, seed: u32) -> Vec<Value> {
        let tys = self.block_params[to].clone();
        tys.iter().enumerate().map(|(i, &ty)| self.value(ty, seed.wrapping_add(i as u32))).collect()
    }

    fn ret(&mut self, seed: u32) {
        let rets = self.rets.clone();
        let values: Vec<Value> =
            rets.iter().enumerate().map(|(i, &ty)| self.value(ty, seed.wrapping_add(i as u32))).collect();
        self.b.ret(&values);
    }

    fn block(&mut self, plan: &Plan, i: usize) {
        let n = plan.blocks.len();
        self.b.begin(self.blocks[i]);
        self.pool.truncate(self.entry);
        let params = self.block_params[i].clone();
        for ty in params {
            let made = self.b.param(ty);
            self.keep(made);
        }
        for w in &plan.blocks[i].ops {
            self.op(w);
        }
        let term = &plan.blocks[i].term;
        // A target: any block but the entry, which has no predecessors.
        let target = |word: u32| 1 + word as usize % (n - 1);
        let mut invoked = None;
        match term[0] % 6 {
            0 | 1 if n > 1 => {
                let to = target(term[1]);
                let args = self.args(to, term[2]);
                self.b.jump((self.blocks[to], &args));
            }
            2 if n > 1 => {
                let (a, b) = (target(term[1]), target(term[2]));
                let (args_a, args_b) = (self.args(a, term[3]), self.args(b, term[1]));
                let cond = self.value(Ty::I1, term[3]);
                let flags = match term[3] % 3 {
                    0 => Flags::NONE,
                    1 => Flags::EXPECT_TRUE,
                    _ => Flags::EXPECT_FALSE | Flags::UNPREDICTABLE,
                };
                self.b.br(flags, cond, (self.blocks[a], &args_a), (self.blocks[b], &args_b));
            }
            3 if n > 1 => {
                let scrutinee = self.value(pick(&SCALAR_INTS, term[1]), term[2]);
                let default = target(term[3]);
                let args = self.args(default, term[1]);
                // Distinct case values, each truncated to the scrutinee's width the same way.
                let bits = self.b.ty(scrutinee).bits(self.b.ptr_bits());
                let cases: Vec<(u128, usize, Vec<Value>)> = (0..term[2] % 4)
                    .map(|k| {
                        let to = target(term[1].wrapping_add(k));
                        (k as u128 * 3 + (bits as u128 % 2), to, self.args(to, k))
                    })
                    .collect();
                let cases: Vec<(u128, (Block, &[Value]))> =
                    cases.iter().map(|(value, to, args)| (*value, (self.blocks[*to], &args[..]))).collect();
                self.b.switch(scrutinee, (self.blocks[default], &args), &cases);
            }
            4 => {
                let arg = self.value(Ty::I64, term[1]);
                let normal = self.b.create_block(BlockKind::Normal);
                let unwind = self.b.create_block(pick(&[BlockKind::Cleanup, BlockKind::Terminate], term[2]));
                let made = self.b.invoke(Flags::NONE, self.t.callee, self.t.callee_sig, &[arg], normal, unwind);
                invoked = Some((normal, unwind, made));
            }
            _ => self.ret(term[1]),
        }
        if i == 0 {
            self.entry = self.pool.len();
        }
        if let Some((normal, unwind, made)) = invoked {
            let before = self.pool.len();
            self.b.begin(normal);
            self.keep(made);
            self.ret(term[3]);
            self.pool.truncate(before);
            self.b.begin(unwind);
            let exception = self.b.param(Ty::PTR);
            self.b.resume(exception);
        }
    }
}

fn facts(deref: u64, size: u32, range: Option<(u128, u128)>, align: u8, bits: u8) -> Facts {
    Facts { deref, size, range, align, bits }
}

fn mem(align: u8, src_align: u8, pointee_align: u8, bits: u8, range: Option<(u128, u128)>) -> MemAttrs {
    MemAttrs { align, src_align, pointee_align, bits, range }
}

fn param(ty: Ty, word: u32) -> Param {
    let mut param = Param::of(ty);
    let ptr = ty.elem() == Scalar::Ptr;
    if ptr && word % 2 == 0 {
        let bits = ParamFactsRec::NONNULL | ParamFactsRec::NOALIAS | ParamFactsRec::NOUNDEF;
        param.facts = facts(8, 0, None, 3, bits);
    } else if ptr && word % 5 == 1 {
        param.kind = ParamKind::Byval;
        param.facts = facts(0, 24, None, 3, 0);
    } else if ty.elem() == Scalar::I8 {
        param.ext = Ext::Zext;
        param.facts = facts(0, 0, Some((0, 1)), 0, ParamFactsRec::NOUNDEF);
    } else if ty.elem() == Scalar::I16 && word % 3 == 0 {
        param.ext = Ext::Sext;
    }
    param
}

fn build<S: Sink>(plan: &Plan) -> Builder<Global, S> {
    let mut made = Gen::<S>::new(plan);
    for i in 0..plan.blocks.len() {
        made.block(plan, i);
    }
    made.b
}

fn generated(plan: &Plan) -> Func<Global> {
    match build::<Full>(plan).finish_checked() {
        Ok(func) => func,
        Err(error) => panic!("the generator made LIR the builder refuses: {error}"),
    }
}

#[hegel::test]
fn a_record_reads_back_as_the_function(tc: TestCase) {
    let func = generated(&plan(&tc));
    let view = func.view();
    let bytes = persist::to_bytes(&view);
    let aligned = Aligned::copy_from(&bytes);
    let read = persist::read(&aligned).expect("a record written reads");
    assert_eq!(read, view);
    assert_eq!(persist::to_bytes(&read), bytes, "a record read and written again is the same bytes");
    verify(read).expect("a record read is the function verified");
}

#[hegel::test]
fn the_text_form_reads_back_as_the_function(tc: TestCase) {
    let func = generated(&plan(&tc));
    let text = print(func.view());
    let parsed = match parse(&text) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{error}\n{text}"),
    };
    assert_eq!(parsed.view(), func.view(), "\n{text}");
    assert_eq!(print(parsed.view()), text);
}

#[hegel::test]
fn the_walk_requests_what_the_function_refers_to(tc: TestCase) {
    let plan = plan(&tc);
    let func = generated(&plan);
    let requests = build::<RequestsOnly>(&plan).finish().expect("the walk of a valid lowering").0;
    assert_eq!(requests.refs, func.refs);
    assert_eq!(requests.datas, func.datas);
    assert_eq!(requests.data_bytes, func.data_bytes);
    assert_eq!(requests.data_relocs, func.data_relocs);
    assert_eq!(requests.intrinsics, func.intrinsics);
    assert_eq!(requests.names, func.names);
    assert_eq!(requests.tys, func.tys, "the walk types every value as the build does");
    assert!(requests.ops.is_empty() && requests.extra.is_empty(), "the walk makes no instructions");
}

#[hegel::test]
fn corrupted_bytes_are_refused_without_a_panic(tc: TestCase) {
    let func = generated(&plan(&tc));
    let mut bytes = persist::to_bytes(&func.view());
    let flips = tc.draw(gs::vecs(gs::tuples!(gs::integers::<u32>(), gs::integers::<u8>())).min_size(1).max_size(4));
    for (at, byte) in flips {
        let at = at as usize % bytes.len();
        bytes[at] ^= byte | 1;
    }
    let aligned = Aligned::copy_from(&bytes);
    if let Ok(view) = persist::read(&aligned)
        && verify(view).is_ok()
    {
        let text = print(view);
        assert_eq!(parse(&text).map(|it| print(it.view())).as_deref(), Ok(text.as_str()));
    }
}

#[hegel::test]
fn corrupted_columns_are_refused_without_a_panic(tc: TestCase) {
    let mut func = generated(&plan(&tc));
    let which = tc.draw(gs::integers::<u32>().max_value(4));
    let at = tc.draw(gs::integers::<u32>());
    let word = tc.draw(gs::integers::<u32>());
    let column: &mut [u32] = match which {
        0 => &mut func.data[..],
        1 => &mut func.extra[..],
        2 => &mut func.block_starts[..],
        3 => &mut func.attrs[..],
        _ => &mut func.locs[..],
    };
    if !column.is_empty() {
        let len = column.len();
        column[at as usize % len] = word;
    }
    let _ = verify(func.view());
}

/// The function of the text form's example, built by hand.
fn example() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    b.set_attrs(FnAttrs(FnAttrs::NOUNWIND));
    b.mem(mem(3, 0, 0, MemAttrsRec::NOUNDEF, None));
    let (entry, trap, done) =
        (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Cold), b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let sum = b.overflow(Op::UaddOverflow, x, y);
    let (wrapped, carried) = (b.project(sum, 0), b.project(sum, 1));
    b.br(Flags::EXPECT_FALSE, carried, (trap, &[]), (done, &[wrapped]));
    b.begin(trap);
    b.trap(0);
    b.begin(done);
    let result = b.param(Ty::I64);
    b.ret(&[result]);
    b.finish_checked().expect("the example is valid")
}

const EXAMPLE: &str = "\
func ptr64 sig s0 attrs [nounwind] features \"\"
type t0 = {i64, i1}
sig s0 = c (i64, i64) -> (i64)
mem m0 = align 3 src_align 0 pointee_align 0 range none noundef
block0 normal:
  v0 = param i64
  v1 = param i64
  v2 = uadd_overflow t0 v0, v1
  v3 = project i64 v2, 0
  v4 = project i1 v2, 1
  br.expect_false v4, block1(), block2(v3)
block1 cold:
  trap 0
block2 normal:
  v7 = param i64
  return (v7)
";

#[test]
fn the_example_prints_as_written_and_parses_as_built() {
    let built = example();
    assert_eq!(print(built.view()), EXAMPLE);
    let parsed = parse(EXAMPLE).expect("the example parses");
    assert_eq!(parsed.view(), built.view());
}

/// The rule `verify` refuses `func` by.
fn refused(func: &Func<Global>) -> &'static str {
    match verify(func.view()) {
        Ok(()) => panic!("LIR that breaks a rule verified:\n{}", print(func.view())),
        Err(error) => error.rule,
    }
}

#[test]
fn a_use_its_value_does_not_dominate_is_refused() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let [entry, left, right, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let cond = b.param(Ty::I1);
    b.br(Flags::NONE, cond, (left, &[]), (right, &[]));
    b.begin(left);
    let only_left = b.iconst(Ty::I64, 1);
    b.jump((join, &[]));
    b.begin(right);
    b.jump((join, &[]));
    b.begin(join);
    b.ret(&[only_left]);
    match b.finish_checked() {
        Err(BuildError::Verify(error)) => assert_eq!(error.rule, "a use its value does not dominate"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn each_broken_rule_is_refused() {
    // The control: the example verifies.
    let base = example();
    verify(base.view()).expect("the control verifies");
    // The last instruction of an operation.
    let at = |func: &Func<Global>, op: Op| func.ops.iter().rposition(|&it| it == op as u8).expect("the operation");

    let mut func = base.clone();
    let br = at(&func, Op::Br);
    func.flags[br] = Flags::EXPECT_TRUE.0 | Flags::EXPECT_FALSE.0;
    assert_eq!(refused(&func), "a branch expected both ways");

    let mut func = base.clone();
    let project = at(&func, Op::Project);
    func.flags[project] = Flags::NSW.0;
    assert_eq!(refused(&func), "a flag bit its operation's class does not have");

    let mut func = base.clone();
    func.extra.push(0);
    assert_eq!(refused(&func), "words in `extra` no instruction takes");

    let mut func = base.clone();
    let project = at(&func, Op::Project);
    func.tys[project] = Ty::I32.0;
    assert_eq!(refused(&func), "a projection of a member its compound has not");

    let mut func = base.clone();
    let ret = at(&func, Op::Return);
    let first = func.data[ret] as usize;
    func.extra[first + 1] = 1_000;
    assert_eq!(refused(&func), "a value past the last instruction");

    let mut func = base.clone();
    func.block_kinds[1] = BlockKind::Cleanup as u8;
    assert_eq!(refused(&func), "a landing block whose parameters are not the one exception pointer");

    let mut func = base.clone();
    let br = at(&func, Op::Br);
    let first = func.data[br] as usize;
    // The branch's false edge (after the condition and the true edge's block and count) goes
    // to the entry.
    func.extra[first + 3] = 0;
    assert_eq!(refused(&func), "an edge to the entry block, which has no predecessors");

    let mut func = base.clone();
    let trap = at(&func, Op::Trap);
    func.ops[trap] = Op::Unreachable as u8;
    func.data[trap] = 5;
    assert_eq!(refused(&func), "a `data` word where its operation has no operand");

    let mut func = base.clone();
    let trap = at(&func, Op::Trap);
    func.ops[trap] = Op::Fence as u8;
    func.data[trap] = 0;
    assert_eq!(refused(&func), "a block whose last instruction is not a terminator");

    let mut func = base;
    func.mem_attrs[0] = MemAttrsRec::default();
    assert_eq!(refused(&func), "attributes that claim nothing, which are written as none");
}

#[test]
fn a_jump_to_a_landing_block_is_refused() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let (entry, landing) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Cleanup));
    b.begin(entry);
    let null = b.null();
    b.jump((landing, &[null]));
    b.begin(landing);
    let exception = b.param(Ty::PTR);
    b.resume(exception);
    match b.finish_checked() {
        Err(BuildError::Verify(error)) => {
            assert_eq!(error.rule, "a jump to a landing block, which only unwind edges reach")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn atomics_refuse_orderings_their_operation_cannot_have() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.param(Ty::PTR);
    b.atomic_load(Ty::I64, Flags::NONE.with_ordering(Ordering::Release), p, MemAttrs::aligned(3));
    b.ret(&[]);
    match b.finish_checked() {
        Err(BuildError::Verify(error)) => assert_eq!(error.rule, "an atomic load that releases"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_builder_names_its_misuse() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.iconst(Ty::I32, 3);
    b.param(Ty::I32);
    b.ret(&[]);
    assert_eq!(b.finish().map(|_| ()), Err(BuildError::LateParam { block: Block(0) }));

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let (entry, never) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    b.begin(entry);
    b.jump((never, &[]));
    assert_eq!(b.finish().map(|_| ()), Err(BuildError::Unbegun { block: never }));
}

#[test]
fn constants_take_the_narrowest_form() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let small = b.iconst(Ty::I64, u64::MAX as u128);
    let wide = b.iconst(Ty::I64, 1 << 40);
    let byte = b.iconst(Ty::I8, 0x1ff);
    b.ret(&[]);
    let func = b.finish_checked().expect("valid");
    let view = func.view();
    assert_eq!(view.op(small.index()), Op::Iconst, "-1 fits a word sign-extended");
    assert_eq!(view.data[small.index()], u32::MAX);
    assert_eq!(view.op(wide.index()), Op::IconstWide);
    assert_eq!(view.operands(wide.index()).constant(), &[0u32, 1 << 8][..]);
    assert_eq!(view.data[byte.index()], 0xff, "a constant is truncated to its width");
    let _ = verify::structure(view);
}
