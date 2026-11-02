//! What each stencil computes, per op (stencil.md, "Semantics, op by op"): Cranelift's
//! semantics, written in LLVM IR that means exactly that, not LLVM's nearest instruction.
//! Each body is given its operands (values of [`Shape::arg`]'s types) and gives its results
//! (of [`Shape::result`]'s); [`super::ir`] reads the operands and writes the results.
//!
//! The rules that differ from LLVM's instruction of the same name:
//!
//! - divisions trap on a zero divisor (`Trap`, `int_divz`) and `sdiv` on `MIN / -1`
//!   (`TrapOverflow`, `int_ovf`); `srem` of `MIN % -1` is 0, never a fault;
//! - shift amounts are taken modulo the lane width; rotates likewise (`fshl`/`fshr`);
//! - `fmin`/`fmax` are `llvm.minimum`/`llvm.maximum`: NaN in, NaN out, `-0 < +0`;
//! - `nearest` is `llvm.roundeven` (half to even);
//! - `fma` keeps its multiplicands in Cranelift's order, which decides the NaN of two;
//! - `fcvt_to_*` trap on NaN (`Trap`, `bad_toint`) and out of range (`TrapOverflow`);
//!   the `_sat` forms saturate, with NaN to 0 (`llvm.fpto*i.sat`);
//! - atomics are sequentially consistent;
//! - `fvdemote` zeroes its upper lanes; `swizzle`'s and `shuffle`'s out-of-range lanes are 0.

use crate::{
    format::HoleKind,
    generate::ir::{B, bits, float, icmp, ir, lane_bits, mangle, splat, vec_of, zero},
    shape::{Disp, FloatCc, IntCc, Isa, Op, RmwOp, Shape, Ty},
};

/// The results of `shape` computed from `args`, in [`super::ir::B`]'s stencil.
pub fn body(b: &mut B<'_>, shape: Shape, args: &[String]) -> Vec<String> {
    let ty = shape.ty;
    let t = ir(ty);
    let x = || args[0].as_str();
    let y = || args[1].as_str();
    let z = || args[2].as_str();
    let one = |v: String| vec![v];
    match shape.op {
        Op::Copy => one(x().to_owned()),
        Op::Iconst => one(b.imm(HoleKind::Imm0, ty, shape.aux == 1)),
        Op::Fconst => {
            let it = bits(ty);
            let v = b.imm(HoleKind::Imm0, it, shape.aux == 1);
            one(b.cast("bitcast", it, &v, ty))
        }
        Op::Vconst => one(b.val(&format!("load {t}, ptr @{}, align 16", HoleKind::Const.symbol()))),

        Op::Iadd => one(b.val(&format!("add {t} {}, {}", x(), y()))),
        Op::Isub => one(b.val(&format!("sub {t} {}, {}", x(), y()))),
        Op::Imul => one(b.val(&format!("mul {t} {}, {}", x(), y()))),
        Op::Ineg => one(b.val(&format!("sub {t} {}, {}", zero(ty), x()))),
        Op::Iabs => one(b.call(t, &format!("llvm.abs.{}", mangle(ty)), &[(t, x()), ("i1", "false")])),
        Op::Umulhi => one(mulhi(b, ty, x(), y(), false)),
        Op::Smulhi => one(mulhi(b, ty, x(), y(), true)),
        Op::Udiv | Op::Urem => {
            b.exit_test(HoleKind::Trap, ty, y(), true);
            let op = if shape.op == Op::Udiv { "udiv" } else { "urem" };
            one(b.val(&format!("{op} {t} {}, {}", x(), y())))
        }
        Op::Sdiv => {
            b.exit_test(HoleKind::Trap, ty, y(), true);
            let min = b.val(&format!("icmp eq {t} {}, {}", x(), min(ty)));
            let neg = b.val(&format!("icmp eq {t} {}, -1", y()));
            let overflow = b.val(&format!("and i1 {min}, {neg}"));
            b.exit_if(HoleKind::TrapOverflow, &overflow);
            one(b.val(&format!("sdiv {t} {}, {}", x(), y())))
        }
        Op::Srem => {
            b.exit_test(HoleKind::Trap, ty, y(), true);
            let neg = b.val(&format!("icmp eq {t} {}, -1", y()));
            let divisor = b.val(&format!("select i1 {neg}, {t} 1, {t} {}", y()));
            one(b.val(&format!("srem {t} {}, {divisor}", x())))
        }
        Op::SqmulRoundSat => one(sqmul_round_sat(b, ty, x(), y())),
        Op::AvgRound => {
            let or = b.val(&format!("or {t} {}, {}", x(), y()));
            let xor = b.val(&format!("xor {t} {}, {}", x(), y()));
            let half = b.val(&format!("lshr {t} {xor}, {}", splat(ty, 1)));
            one(b.val(&format!("sub {t} {or}, {half}")))
        }
        Op::UaddSat => one(intrinsic2(b, "llvm.uadd.sat", ty, x(), y())),
        Op::SaddSat => one(intrinsic2(b, "llvm.sadd.sat", ty, x(), y())),
        Op::UsubSat => one(intrinsic2(b, "llvm.usub.sat", ty, x(), y())),
        Op::SsubSat => one(intrinsic2(b, "llvm.ssub.sat", ty, x(), y())),
        Op::Smin => one(intrinsic2(b, "llvm.smin", ty, x(), y())),
        Op::Umin => one(intrinsic2(b, "llvm.umin", ty, x(), y())),
        Op::Smax => one(intrinsic2(b, "llvm.smax", ty, x(), y())),
        Op::Umax => one(intrinsic2(b, "llvm.umax", ty, x(), y())),

        Op::UaddOverflow => overflow(b, "llvm.uadd.with.overflow", ty, x(), y()),
        Op::SaddOverflow => overflow(b, "llvm.sadd.with.overflow", ty, x(), y()),
        Op::UsubOverflow => overflow(b, "llvm.usub.with.overflow", ty, x(), y()),
        Op::SsubOverflow => overflow(b, "llvm.ssub.with.overflow", ty, x(), y()),
        Op::UmulOverflow => overflow(b, "llvm.umul.with.overflow", ty, x(), y()),
        Op::SmulOverflow => overflow(b, "llvm.smul.with.overflow", ty, x(), y()),
        Op::UaddOverflowCin => carry(b, "llvm.uadd.with.overflow", false, ty, x(), y(), z()),
        Op::SaddOverflowCin => carry(b, "llvm.sadd.with.overflow", true, ty, x(), y(), z()),
        Op::UsubOverflowBin => carry(b, "llvm.usub.with.overflow", false, ty, x(), y(), z()),
        Op::SsubOverflowBin => carry(b, "llvm.ssub.with.overflow", true, ty, x(), y(), z()),
        Op::UaddOverflowTrap => {
            let (sum, carry) = with_overflow(b, "llvm.uadd.with.overflow", ty, x(), y());
            b.exit_if(HoleKind::Trap, &carry);
            one(sum)
        }

        Op::Band => one(bitwise(b, "and", ty, x(), y())),
        Op::Bor => one(bitwise(b, "or", ty, x(), y())),
        Op::Bxor => one(bitwise(b, "xor", ty, x(), y())),
        Op::Bnot => {
            let it = bits(ty);
            let v = b.cast("bitcast", ty, x(), it);
            let r = b.val(&format!("xor {} {v}, {}", ir(it), splat(it, -1)));
            one(b.cast("bitcast", it, &r, ty))
        }
        Op::Rotl | Op::Rotr => {
            let amount = b.cast("zext", Ty::I8, y(), ty);
            let name = if shape.op == Op::Rotl { "llvm.fshl" } else { "llvm.fshr" };
            let name = format!("{name}.{}", mangle(ty));
            one(b.call(t, &name, &[(t, x()), (t, x()), (t, amount.as_str())]))
        }
        Op::Ishl | Op::Ushr | Op::Sshr => {
            let op = match shape.op {
                Op::Ishl => "shl",
                Op::Ushr => "lshr",
                _ => "ashr",
            };
            let lane = ty.lane();
            let amount = b.cast("zext", Ty::I8, y(), lane);
            let masked = b.val(&format!("and {} {amount}, {}", ir(lane), lane_bits(ty) - 1));
            let amount = if ty.is_vector() { splat_value(b, ty, &masked) } else { masked };
            one(b.val(&format!("{op} {t} {}, {amount}", x())))
        }
        Op::Bitrev => one(b.call(t, &format!("llvm.bitreverse.{}", mangle(ty)), &[(t, x())])),
        Op::Clz => one(b.call(t, &format!("llvm.ctlz.{}", mangle(ty)), &[(t, x()), ("i1", "false")])),
        Op::Ctz => one(b.call(t, &format!("llvm.cttz.{}", mangle(ty)), &[(t, x()), ("i1", "false")])),
        Op::Cls => {
            let sign = b.val(&format!("ashr {t} {}, {}", x(), lane_bits(ty) - 1));
            let flipped = b.val(&format!("xor {t} {}, {sign}", x()));
            let lz = b.call(t, &format!("llvm.ctlz.{}", mangle(ty)), &[(t, flipped.as_str()), ("i1", "false")]);
            one(b.val(&format!("sub {t} {lz}, 1")))
        }
        Op::Bswap => one(b.call(t, &format!("llvm.bswap.{}", mangle(ty)), &[(t, x())])),
        Op::Popcnt => one(b.call(t, &format!("llvm.ctpop.{}", mangle(ty)), &[(t, x())])),
        Op::Bitselect => {
            let it = bits(ty);
            let i = ir(it);
            let c = b.cast("bitcast", ty, x(), it);
            let a = b.cast("bitcast", ty, y(), it);
            let d = b.cast("bitcast", ty, z(), it);
            let taken = b.val(&format!("and {i} {a}, {c}"));
            let not = b.val(&format!("xor {i} {c}, {}", splat(it, -1)));
            let other = b.val(&format!("and {i} {d}, {not}"));
            let r = b.val(&format!("or {i} {taken}, {other}"));
            one(b.cast("bitcast", it, &r, ty))
        }
        Op::Bmask => {
            let src = shape.arg(0);
            let nz = b.val(&format!("icmp ne {} {}, 0", ir(src), x()));
            one(b.val(&format!("sext i1 {nz} to {t}")))
        }

        Op::Icmp => {
            let cc = IntCc::from_u8(shape.aux).expect("an icmp's condition");
            let c = b.val(&format!("icmp {} {t} {}, {}", icmp(cc), x(), y()));
            one(mask(b, ty, &c))
        }
        Op::Fcmp => {
            let cc = FloatCc::from_u8(shape.aux).expect("an fcmp's condition");
            let c = b.val(&format!("fcmp {} {t} {}, {}", fcmp(cc), x(), y()));
            one(mask(b, ty, &c))
        }
        Op::Select => {
            let cond = shape.arg(0);
            let c = b.val(&format!("icmp ne {} {}, 0", ir(cond), x()));
            one(b.val(&format!("select i1 {c}, {t} {}, {t} {}", y(), z())))
        }
        Op::SelectIcmp => {
            let cc = IntCc::from_u8(shape.aux & 0xf).expect("a select_icmp's condition");
            let c = b.val(&format!("icmp {} {t} {}, {}", icmp(cc), x(), y()));
            one(b.val(&format!("select i1 {c}, {t} {}, {t} {}", x(), y())))
        }

        Op::Fadd => one(b.val(&format!("fadd {t} {}, {}", x(), y()))),
        Op::Fsub => one(b.val(&format!("fsub {t} {}, {}", x(), y()))),
        Op::Fmul => one(b.val(&format!("fmul {t} {}, {}", x(), y()))),
        Op::Fdiv => one(b.val(&format!("fdiv {t} {}, {}", x(), y()))),
        Op::Fneg => one(b.val(&format!("fneg {t} {}", x()))),
        Op::Sqrt => one(intrinsic1(b, "llvm.sqrt", ty, x())),
        Op::Fabs => one(intrinsic1(b, "llvm.fabs", ty, x())),
        Op::Ceil => one(intrinsic1(b, "llvm.ceil", ty, x())),
        Op::Floor => one(intrinsic1(b, "llvm.floor", ty, x())),
        Op::Trunc => one(intrinsic1(b, "llvm.trunc", ty, x())),
        Op::Nearest => one(intrinsic1(b, "llvm.roundeven", ty, x())),
        Op::Fcopysign => one(intrinsic2(b, "llvm.copysign", ty, x(), y())),
        Op::Fmin => one(intrinsic2(b, "llvm.minimum", ty, x(), y())),
        Op::Fmax => one(intrinsic2(b, "llvm.maximum", ty, x(), y())),
        Op::Fma => one(fma(b, ty, shape.aux == 1, x(), y(), z())),

        Op::Bitcast => one(b.cast("bitcast", shape.arg(0), x(), ty)),
        Op::Ireduce => one(b.cast("trunc", shape.arg(0), x(), ty)),
        Op::Uextend => one(b.cast("zext", shape.arg(0), x(), ty)),
        Op::Sextend => one(b.cast("sext", shape.arg(0), x(), ty)),
        Op::Fpromote => one(b.val(&format!("fpext float {} to double", x()))),
        Op::Fdemote => one(b.val(&format!("fptrunc double {} to float", x()))),
        Op::Fvdemote => {
            let low = b.val(&format!("fptrunc <2 x double> {} to <2 x float>", x()));
            one(b.val(&format!(
                "shufflevector <2 x float> {low}, <2 x float> zeroinitializer, {}",
                lanes_mask(0..4)
            )))
        }
        Op::FvpromoteLow => {
            let low = b.val(&format!("shufflevector <4 x float> {}, <4 x float> poison, {}", x(), lanes_mask(0..2)));
            one(b.val(&format!("fpext <2 x float> {low} to <2 x double>")))
        }
        Op::FcvtToUint | Op::FcvtToSint => one(fcvt_to_int(b, shape, x())),
        Op::FcvtToUintSat | Op::FcvtToSintSat => {
            let src = shape.arg(0);
            let kind = if shape.op == Op::FcvtToUintSat { "fptoui" } else { "fptosi" };
            let name = format!("llvm.{kind}.sat.{}.{}", mangle(ty), mangle(src));
            one(b.call(t, &name, &[(ir(src), x())]))
        }
        Op::FcvtFromUint => one(b.val(&format!("uitofp {} {} to {t}", ir(shape.arg(0)), x()))),
        Op::FcvtFromSint => one(b.val(&format!("sitofp {} {} to {t}", ir(shape.arg(0)), x()))),
        Op::Isplit => {
            let lo = b.val(&format!("trunc i128 {} to i64", x()));
            let high = b.val(&format!("lshr i128 {}, 64", x()));
            let hi = b.val(&format!("trunc i128 {high} to i64"));
            vec![lo, hi]
        }
        Op::Iconcat => {
            let lo = b.val(&format!("zext i64 {} to i128", x()));
            let hi = b.val(&format!("zext i64 {} to i128", y()));
            let high = b.val(&format!("shl i128 {hi}, 64"));
            one(b.val(&format!("or i128 {lo}, {high}")))
        }
        Op::Snarrow | Op::Unarrow | Op::Uunarrow => one(narrow(b, shape, x(), y())),
        Op::SwidenLow | Op::SwidenHigh | Op::UwidenLow | Op::UwidenHigh => {
            let src = shape.arg(0);
            let half = ty.lanes();
            let range = if matches!(shape.op, Op::SwidenLow | Op::UwidenLow) { 0..half } else { half..2 * half };
            let part = b.val(&format!("shufflevector {} {}, {} poison, {}", ir(src), x(), ir(src), lanes_mask(range)));
            let ext = if matches!(shape.op, Op::SwidenLow | Op::SwidenHigh) { "sext" } else { "zext" };
            one(b.val(&format!("{ext} {} {part} to {t}", vec_of(half, lane_bits(src)))))
        }
        Op::IaddPairwise => {
            let n = ty.lanes();
            let evens = (0..n).map(|i| 2 * i);
            let even = b.val(&format!("shufflevector {t} {}, {t} {}, {}", x(), y(), lanes_mask(evens)));
            let odd = b.val(&format!("shufflevector {t} {}, {t} {}, {}", x(), y(), lanes_mask((0..n).map(|i| 2 * i + 1))));
            one(b.val(&format!("add {t} {even}, {odd}")))
        }
        Op::ScalarToVector => {
            one(b.val(&format!("insertelement {t} zeroinitializer, {} {}, i64 0", ir(ty.lane()), x())))
        }
        Op::Splat => one(splat_value(b, ty, x())),
        Op::Swizzle => {
            let name = "llvm.aarch64.neon.tbl1.v16i8";
            one(b.call("<16 x i8>", name, &[("<16 x i8>", x()), ("<16 x i8>", y())]))
        }
        Op::Shuffle => {
            let mask = b.val(&format!("load <16 x i8>, ptr @{}, align 16", HoleKind::Const.symbol()));
            let name = "llvm.aarch64.neon.tbl2.v16i8";
            one(b.call("<16 x i8>", name, &[("<16 x i8>", x()), ("<16 x i8>", y()), ("<16 x i8>", mask.as_str())]))
        }
        Op::Insertlane if b.isa == Isa::X86_64 && lane_bits(ty) == 8 => one(insert_byte(b, x(), y(), shape.aux)),
        Op::Insertlane => {
            one(b.val(&format!("insertelement {t} {}, {} {}, i64 {}", x(), ir(ty.lane()), y(), shape.aux)))
        }
        Op::Extractlane if b.isa == Isa::X86_64 && lane_bits(ty) == 8 => one(extract_byte(b, x(), shape.aux)),
        Op::Extractlane => one(b.val(&format!("extractelement {t} {}, i64 {}", x(), shape.aux))),
        Op::VanyTrue => {
            let lane = ir(ty.lane());
            let any = b.call(lane, &format!("llvm.vector.reduce.or.{}", mangle(ty)), &[(t, x())]);
            let c = b.val(&format!("icmp ne {lane} {any}, 0"));
            one(b.val(&format!("zext i1 {c} to i8")))
        }
        Op::VallTrue => {
            let n = ty.lanes();
            let each = b.val(&format!("icmp ne {t} {}, zeroinitializer", x()));
            let lanes = format!("<{n} x i1>");
            let all = b.call("i1", &format!("llvm.vector.reduce.and.v{n}i1"), &[(lanes.as_str(), each.as_str())]);
            one(b.val(&format!("zext i1 {all} to i8")))
        }
        Op::VhighBits => {
            let n = ty.lanes();
            let it = ty.int_lanes();
            let v = b.cast("bitcast", ty, x(), it);
            let neg = b.val(&format!("icmp slt {} {v}, zeroinitializer", ir(it)));
            let packed = b.val(&format!("bitcast <{n} x i1> {neg} to i{n}"));
            let result = shape.result(0);
            if u32::from(result.bytes()) * 8 == u32::from(n) {
                one(packed)
            } else {
                one(b.val(&format!("zext i{n} {packed} to {}", ir(result))))
            }
        }
        Op::X86Cvtt2dq => {
            let name = "llvm.x86.sse2.cvttps2dq";
            one(b.call("<4 x i32>", name, &[("<4 x float>", x())]))
        }
        Op::Blendv | Op::X86Pshufb | Op::X86Pmulhrsw | Op::X86Pmaddubsw => {
            unreachable!("{shape} is of no instruction set's library")
        }

        Op::Load => one(load(b, ty, ty, None, x())),
        Op::Uload8 => one(load(b, Ty::I8, ty, Some("zext"), x())),
        Op::Sload8 => one(load(b, Ty::I8, ty, Some("sext"), x())),
        Op::Uload16 => one(load(b, Ty::I16, ty, Some("zext"), x())),
        Op::Sload16 => one(load(b, Ty::I16, ty, Some("sext"), x())),
        Op::Uload32 => one(load(b, Ty::I32, ty, Some("zext"), x())),
        Op::Sload32 => one(load(b, Ty::I32, ty, Some("sext"), x())),
        Op::Uload8x8 | Op::Sload8x8 | Op::Uload16x4 | Op::Sload16x4 | Op::Uload32x2 | Op::Sload32x2 => {
            let ext = if matches!(shape.op, Op::Sload8x8 | Op::Sload16x4 | Op::Sload32x2) { "sext" } else { "zext" };
            let narrow = vec_of(ty.lanes(), lane_bits(ty) / 2);
            let at = address(b, x());
            let v = b.val(&format!("load {narrow}, ptr {at}, align 1"));
            one(b.val(&format!("{ext} {narrow} {v} to {t}")))
        }
        Op::Store => {
            store(b, ty, x(), y());
            vec![]
        }
        Op::Istore8 | Op::Istore16 | Op::Istore32 => {
            let narrow = match shape.op {
                Op::Istore8 => Ty::I8,
                Op::Istore16 => Ty::I16,
                _ => Ty::I32,
            };
            let v = b.cast("trunc", ty, x(), narrow);
            store(b, narrow, &v, y());
            vec![]
        }
        Op::StackAddr => {
            let offset = b.offset();
            let at = b.frame_at(&offset);
            one(b.val(&format!("ptrtoint ptr {at} to i64")))
        }
        Op::AtomicLoad => {
            let p = pointer(b, x());
            one(b.val(&format!("load atomic {t}, ptr {p} seq_cst, align {}", ty.bytes())))
        }
        Op::AtomicStore => {
            let p = pointer(b, y());
            b.stmt(&format!("store atomic {t} {}, ptr {p} seq_cst, align {}", x(), ty.bytes()));
            vec![]
        }
        Op::AtomicRmw => {
            let op = match RmwOp::from_u8(shape.aux).expect("an atomic_rmw's op") {
                RmwOp::Add => "add",
                RmwOp::Sub => "sub",
                RmwOp::And => "and",
                RmwOp::Nand => "nand",
                RmwOp::Or => "or",
                RmwOp::Xor => "xor",
                RmwOp::Xchg => "xchg",
                RmwOp::Umin => "umin",
                RmwOp::Umax => "umax",
                RmwOp::Smin => "min",
                RmwOp::Smax => "max",
            };
            let p = pointer(b, x());
            one(b.val(&format!("atomicrmw {op} ptr {p}, {t} {} seq_cst, align {}", y(), ty.bytes())))
        }
        Op::AtomicCas => {
            let p = pointer(b, x());
            let pair = b.val(&format!("cmpxchg ptr {p}, {t} {}, {t} {} seq_cst seq_cst, align {}", y(), z(), ty.bytes()));
            one(b.val(&format!("extractvalue {{ {t}, i1 }} {pair}, 0")))
        }
        Op::Fence => {
            // x86-64's `fence seq_cst` is a `lock or` on the stack; Cranelift's fence is `mfence`.
            match b.isa {
                Isa::X86_64 => b.stmt("call void asm sideeffect \"mfence\", \"~{memory}\"()"),
                Isa::Aarch64 => b.stmt("fence seq_cst"),
            }
            vec![]
        }
        Op::LoadConfined | Op::StoreConfined => confined(b, shape, args),

        Op::Jump => vec![],
        Op::Brif => {
            b.exit_test(HoleKind::Target, ty, x(), false);
            vec![]
        }
        Op::BrIcmp => {
            let cc = IntCc::from_u8(shape.aux).expect("a br_icmp's condition");
            b.exit_cmp(HoleKind::Target, cc, ty, x(), y());
            vec![]
        }
        Op::Trapz => {
            b.exit_test(HoleKind::Trap, ty, x(), true);
            vec![]
        }
        Op::Trapnz => {
            b.exit_test(HoleKind::Trap, ty, x(), false);
            vec![]
        }
    }
}

/// The smallest value of the integer type `ty`, as LLVM reads it.
fn min(ty: Ty) -> String {
    (-(1i128 << (lane_bits(ty) - 1))).to_string()
}

/// `<op>.<ty>(x)`.
fn intrinsic1(b: &mut B<'_>, op: &str, ty: Ty, x: &str) -> String {
    let t = ir(ty);
    b.call(t, &format!("{op}.{}", mangle(ty)), &[(t, x)])
}

/// `<op>.<ty>(x, y)`.
fn intrinsic2(b: &mut B<'_>, op: &str, ty: Ty, x: &str, y: &str) -> String {
    let t = ir(ty);
    b.call(t, &format!("{op}.{}", mangle(ty)), &[(t, x), (t, y)])
}

/// An `<op>.with.overflow` intrinsic: the value and its `i1` overflow.
fn with_overflow(b: &mut B<'_>, op: &str, ty: Ty, x: &str, y: &str) -> (String, String) {
    let t = ir(ty);
    let pair_ty = format!("{{ {t}, i1 }}");
    let pair = b.call(&pair_ty, &format!("{op}.{}", mangle(ty)), &[(t, x), (t, y)]);
    let value = b.val(&format!("extractvalue {pair_ty} {pair}, 0"));
    let flag = b.val(&format!("extractvalue {pair_ty} {pair}, 1"));
    (value, flag)
}

/// The value and its overflow flag as an `i8` 0 or 1.
fn overflow(b: &mut B<'_>, op: &str, ty: Ty, x: &str, y: &str) -> Vec<String> {
    let (value, flag) = with_overflow(b, op, ty, x, y);
    let flag = b.val(&format!("zext i1 {flag} to i8"));
    vec![value, flag]
}

/// `x op y op (c != 0)` with the overflow of the whole: either step's for unsigned (they
/// can't both carry), one or the other's but not both for signed (a step that overflows
/// by one and a carry that brings it back in range).
fn carry(b: &mut B<'_>, op: &str, signed: bool, ty: Ty, x: &str, y: &str, c: &str) -> Vec<String> {
    let t = ir(ty);
    let nz = b.val(&format!("icmp ne i8 {c}, 0"));
    let c = b.val(&format!("zext i1 {nz} to {t}"));
    let (first, o1) = with_overflow(b, op, ty, x, y);
    let (value, o2) = with_overflow(b, op, ty, &first, &c);
    let join = if signed { "xor" } else { "or" };
    let flag = b.val(&format!("{join} i1 {o1}, {o2}"));
    let flag = b.val(&format!("zext i1 {flag} to i8"));
    vec![value, flag]
}

/// `and`/`or`/`xor` in the integers of the same bits (floats are their bits).
fn bitwise(b: &mut B<'_>, op: &str, ty: Ty, x: &str, y: &str) -> String {
    let it = bits(ty);
    let a = b.cast("bitcast", ty, x, it);
    let c = b.cast("bitcast", ty, y, it);
    let r = b.val(&format!("{op} {} {a}, {c}", ir(it)));
    b.cast("bitcast", it, &r, ty)
}

/// The high half of the double-width product.
fn mulhi(b: &mut B<'_>, ty: Ty, x: &str, y: &str, signed: bool) -> String {
    let t = ir(ty);
    let w = lane_bits(ty);
    let wide = wide_of(ty);
    let ext = if signed { "sext" } else { "zext" };
    let a = b.val(&format!("{ext} {t} {x} to {wide}"));
    let c = b.val(&format!("{ext} {t} {y} to {wide}"));
    let m = b.val(&format!("mul {wide} {a}, {c}"));
    let high = b.val(&format!("lshr {wide} {m}, {}", wide_splat(ty, i128::from(w))));
    b.val(&format!("trunc {wide} {high} to {t}"))
}

/// `(x * y + 2^(w-2)) >> (w-1)`, saturated (only `MIN * MIN` saturates).
fn sqmul_round_sat(b: &mut B<'_>, ty: Ty, x: &str, y: &str) -> String {
    let t = ir(ty);
    let w = lane_bits(ty);
    let wide = wide_of(ty);
    let a = b.val(&format!("sext {t} {x} to {wide}"));
    let c = b.val(&format!("sext {t} {y} to {wide}"));
    let m = b.val(&format!("mul {wide} {a}, {c}"));
    let r = b.val(&format!("add {wide} {m}, {}", wide_splat(ty, 1i128 << (w - 2))));
    let s = b.val(&format!("ashr {wide} {r}, {}", wide_splat(ty, i128::from(w - 1))));
    let wide_name = format!("v{}i{}", ty.lanes(), 2 * w);
    let lo = wide_splat(ty, -(1i128 << (w - 1)));
    let hi = wide_splat(ty, (1i128 << (w - 1)) - 1);
    let s = b.call(&wide, &format!("llvm.smax.{wide_name}"), &[(wide.as_str(), s.as_str()), (wide.as_str(), lo.as_str())]);
    let s = b.call(&wide, &format!("llvm.smin.{wide_name}"), &[(wide.as_str(), s.as_str()), (wide.as_str(), hi.as_str())]);
    b.val(&format!("trunc {wide} {s} to {t}"))
}

/// The integer type of lanes twice as wide as `ty`'s, as many of them.
fn wide_of(ty: Ty) -> String {
    let w = 2 * lane_bits(ty);
    if ty.is_vector() { vec_of(ty.lanes(), w) } else { format!("i{w}") }
}

/// `value` in every lane of [`wide_of`]`(ty)`.
fn wide_splat(ty: Ty, value: i128) -> String {
    if ty.is_vector() { format!("splat (i{} {value})", 2 * lane_bits(ty)) } else { value.to_string() }
}

/// A comparison's `i1` (or `<n x i1>`) as Cranelift's result: an `i8` 0 or 1, or lanes of
/// all ones or zeros.
fn mask(b: &mut B<'_>, ty: Ty, c: &str) -> String {
    if ty.is_vector() {
        let n = ty.lanes();
        b.val(&format!("sext <{n} x i1> {c} to {}", ir(ty.int_lanes())))
    } else {
        b.val(&format!("zext i1 {c} to i8"))
    }
}

/// LLVM's `fcmp` predicate of a condition.
fn fcmp(cc: FloatCc) -> &'static str {
    match cc {
        FloatCc::Ordered => "ord",
        FloatCc::Unordered => "uno",
        FloatCc::Equal => "oeq",
        FloatCc::NotEqual => "une",
        FloatCc::OrderedNotEqual => "one",
        FloatCc::UnorderedOrEqual => "ueq",
        FloatCc::LessThan => "olt",
        FloatCc::LessThanOrEqual => "ole",
        FloatCc::GreaterThan => "ogt",
        FloatCc::GreaterThanOrEqual => "oge",
        FloatCc::UnorderedOrLessThan => "ult",
        FloatCc::UnorderedOrLessThanOrEqual => "ule",
        FloatCc::UnorderedOrGreaterThan => "ugt",
        FloatCc::UnorderedOrGreaterThanOrEqual => "uge",
    }
}

/// A `shufflevector` mask of the lanes `lanes`.
fn lanes_mask(lanes: impl Iterator<Item = u8>) -> String {
    let lanes: Vec<String> = lanes.map(|it| format!("i32 {it}")).collect();
    format!("<{} x i32> <{}>", lanes.len(), lanes.join(", "))
}

/// `v` (of `ty`'s lane type) in every lane of `ty`.
fn splat_value(b: &mut B<'_>, ty: Ty, v: &str) -> String {
    let t = ir(ty);
    let one = b.val(&format!("insertelement {t} poison, {} {v}, i64 0", ir(ty.lane())));
    b.val(&format!("shufflevector {t} {one}, {t} poison, <{} x i32> zeroinitializer", ty.lanes()))
}

/// `fcvt_to_uint`/`fcvt_to_sint`: NaN traps with `bad_toint`, a value whose truncation is
/// out of the integer's range with `int_ovf`.
fn fcvt_to_int(b: &mut B<'_>, shape: Shape, x: &str) -> String {
    let (ty, src) = (shape.ty, shape.arg(0));
    let f = ir(src);
    let nan = b.val(&format!("fcmp uno {f} {x}, {x}"));
    b.exit_if(HoleKind::Trap, &nan);
    let n = lane_bits(ty) as i32;
    let precision = if src == Ty::F32 { 24 } else { 53 };
    let signed = shape.op == Op::FcvtToSint;
    let (low, high) = if signed {
        let high = float(2f64.powi(n - 1));
        let low = if n <= precision {
            format!("ogt {f} {x}, {}", float(-(2f64.powi(n - 1)) - 1.0))
        } else {
            format!("oge {f} {x}, {}", float(-(2f64.powi(n - 1))))
        };
        (low, high)
    } else {
        (format!("ogt {f} {x}, {}", float(-1.0)), float(2f64.powi(n)))
    };
    let above = b.val(&format!("fcmp {low}"));
    let below = b.val(&format!("fcmp olt {f} {x}, {high}"));
    let inside = b.val(&format!("and i1 {above}, {below}"));
    let outside = b.val(&format!("xor i1 {inside}, true"));
    b.exit_if(HoleKind::TrapOverflow, &outside);
    let op = if signed { "fptosi" } else { "fptoui" };
    b.val(&format!("{op} {f} {x} to {}", ir(ty)))
}

/// `snarrow`/`unarrow`/`uunarrow`: `x`'s lanes then `y`'s, each saturated to the narrow lane.
fn narrow(b: &mut B<'_>, shape: Shape, x: &str, y: &str) -> String {
    let ty = shape.ty;
    let wide = shape.arg(0);
    let n = ty.lanes();
    let w = lane_bits(wide);
    let narrow_bits = lane_bits(ty);
    let both = vec_of(n, w);
    let name = format!("v{n}i{w}");
    let lanes = |value: i128| format!("splat (i{w} {value})");
    let joined = b.val(&format!("shufflevector {} {x}, {} {y}, {}", ir(wide), ir(wide), lanes_mask(0..n)));
    let clamp = |b: &mut B<'_>, op: &str, v: &str, value: i128| {
        let bound = lanes(value);
        b.call(&both, &format!("{op}.{name}"), &[(both.as_str(), v), (both.as_str(), bound.as_str())])
    };
    let clamped = match shape.op {
        Op::Snarrow => {
            let low = clamp(b, "llvm.smax", &joined, -(1i128 << (narrow_bits - 1)));
            clamp(b, "llvm.smin", &low, (1i128 << (narrow_bits - 1)) - 1)
        }
        Op::Unarrow => {
            let low = clamp(b, "llvm.smax", &joined, 0);
            clamp(b, "llvm.smin", &low, (1i128 << narrow_bits) - 1)
        }
        _ => clamp(b, "llvm.umin", &joined, (1i128 << narrow_bits) - 1),
    };
    b.val(&format!("trunc {both} {clamped} to {}", ir(ty)))
}

/// An `i64` address as a pointer.
fn pointer(b: &mut B<'_>, p: &str) -> String {
    b.val(&format!("inttoptr i64 {p} to ptr"))
}

/// The address `p` plus the access's `Offset`.
fn address(b: &mut B<'_>, p: &str) -> String {
    let offset = b.offset();
    let sum = b.val(&format!("add i64 {p}, {offset}"));
    pointer(b, &sum)
}

/// A little-endian load of `mem` at `p` + offset, extended to `ty` by `ext`.
fn load(b: &mut B<'_>, mem: Ty, ty: Ty, ext: Option<&str>, p: &str) -> String {
    let at = address(b, p);
    let v = b.val(&format!("load {}, ptr {at}, align 1", ir(mem)));
    match ext {
        Some(ext) => b.cast(ext, mem, &v, ty),
        None => v,
    }
}

/// A little-endian store of `v` (of `ty`) at `p` + offset.
fn store(b: &mut B<'_>, ty: Ty, v: &str, p: &str) {
    let at = address(b, p);
    b.stmt(&format!("store {} {v}, ptr {at}, align 1", ir(ty)));
}

/// A confined access: the plain one at `vmctx + zext(trunc(p) + off)`, the 32-bit sum
/// wrapping as `ireduce.i32 (iadd_imm p, off)` does.
fn confined(b: &mut B<'_>, shape: Shape, args: &[String]) -> Vec<String> {
    let (access, disp) = shape.confined().expect("a confined access");
    let ty = shape.ty;
    let t = ir(ty);
    let (value, p, vmctx) = match shape.op {
        Op::StoreConfined => (args[0].as_str(), args[1].as_str(), args[2].as_str()),
        _ => ("", args[0].as_str(), args[1].as_str()),
    };
    let sum = match disp {
        Disp::Zero => b.cast("trunc", Ty::I64, p, Ty::I32),
        Disp::Lo12 => {
            let v = b.add_lo12(HoleKind::Offset, p);
            b.cast("trunc", Ty::I64, &v, Ty::I32)
        }
        Disp::Imm32 => {
            let p = b.cast("trunc", Ty::I64, p, Ty::I32);
            let off = b.imm(HoleKind::Offset, Ty::I32, false);
            b.val(&format!("add i32 {p}, {off}"))
        }
    };
    let wide = b.cast("zext", Ty::I32, &sum, Ty::I64);
    let addr = b.val(&format!("add i64 {vmctx}, {wide}"));
    let at = pointer(b, &addr);
    let load = |b: &mut B<'_>, mem: &str, ext: Option<&str>| {
        let v = b.val(&format!("load {mem}, ptr {at}, align 1"));
        match ext {
            Some(ext) => vec![b.val(&format!("{ext} {mem} {v} to {t}"))],
            None => vec![v],
        }
    };
    let store = |b: &mut B<'_>, narrow: Ty| {
        let v = b.cast("trunc", ty, value, narrow);
        b.stmt(&format!("store {} {v}, ptr {at}, align 1", ir(narrow)));
        vec![]
    };
    match access {
        Op::Load => load(b, t, None),
        Op::Uload8 | Op::Sload8 | Op::Uload16 | Op::Sload16 | Op::Uload32 | Op::Sload32 => {
            let (mem, ext) = match access {
                Op::Uload8 => (Ty::I8, "zext"),
                Op::Sload8 => (Ty::I8, "sext"),
                Op::Uload16 => (Ty::I16, "zext"),
                Op::Sload16 => (Ty::I16, "sext"),
                Op::Uload32 => (Ty::I32, "zext"),
                _ => (Ty::I32, "sext"),
            };
            load(b, ir(mem), Some(ext))
        }
        Op::Uload8x8 | Op::Sload8x8 | Op::Uload16x4 | Op::Sload16x4 | Op::Uload32x2 | Op::Sload32x2 => {
            let ext = if matches!(access, Op::Sload8x8 | Op::Sload16x4 | Op::Sload32x2) { "sext" } else { "zext" };
            let narrow = vec_of(ty.lanes(), lane_bits(ty) / 2);
            load(b, &narrow, Some(ext))
        }
        Op::AtomicLoad => vec![b.val(&format!("load atomic {t}, ptr {at} seq_cst, align {}", ty.bytes()))],
        Op::Store => store(b, ty),
        Op::Istore8 => store(b, Ty::I8),
        Op::Istore16 => store(b, Ty::I16),
        Op::Istore32 => store(b, Ty::I32),
        Op::AtomicStore => {
            b.stmt(&format!("store atomic {t} {value}, ptr {at} seq_cst, align {}", ty.bytes()));
            vec![]
        }
        other => unreachable!("{shape} fuses `{}`, which no confined op does", other.info().name),
    }
}

/// `x * y + z` fused, with `x` and `y` in the registers Cranelift's AArch64 lowering puts them
/// in, by asm: `fmadd d, x, y, z` and `fmla z, x, y`. Which NaN a fused multiply-add returns
/// when both multiplicands are NaN is the first's, and LLVM takes `llvm.fma`'s multiplicands
/// as commutative, so the intrinsic may swap them. `negated` is `fmsub d, x, y, z` and
/// `fmls z, x, y`, `z - x * y`, which flip `x`'s sign before the multiply, NaN or not: what
/// Cranelift writes for `fma x (fneg y) z`.
fn fma(b: &mut B<'_>, ty: Ty, negated: bool, x: &str, y: &str, z: &str) -> String {
    let t = ir(ty);
    let asm = match (ty, negated) {
        (Ty::F32, false) => "fmadd ${0:s}, ${1:s}, ${2:s}, ${3:s}\", \"=w,w,w,w",
        (Ty::F64, false) => "fmadd ${0:d}, ${1:d}, ${2:d}, ${3:d}\", \"=w,w,w,w",
        (Ty::F32x4, false) => "fmla $0.4s, $2.4s, $3.4s\", \"=w,0,w,w",
        (Ty::F64x2, false) => "fmla $0.2d, $2.2d, $3.2d\", \"=w,0,w,w",
        (Ty::F32, true) => "fmsub ${0:s}, ${1:s}, ${2:s}, ${3:s}\", \"=w,w,w,w",
        (Ty::F64, true) => "fmsub ${0:d}, ${1:d}, ${2:d}, ${3:d}\", \"=w,w,w,w",
        (Ty::F32x4, true) => "fmls $0.4s, $2.4s, $3.4s\", \"=w,0,w,w",
        (Ty::F64x2, true) => "fmls $0.2d, $2.2d, $3.2d\", \"=w,0,w,w",
        (_, true) => unreachable!("an `fma` of {} with an operand negated, which no shape is", ty.name()),
        (_, false) => {
            let name = format!("llvm.fma.{}", mangle(ty));
            return b.call(t, &name, &[(t, x), (t, y), (t, z)]);
        }
    };
    let args = if ty.is_vector() { format!("{t} {z}, {t} {x}, {t} {y}") } else { format!("{t} {x}, {t} {y}, {t} {z}") };
    b.val(&format!("call {t} asm \"{asm}\"({args})"))
}

/// The `i16` word `word` of `<16 x i8>` `v` (as an `i32`, zero-extended), by `pextrw` in
/// asm: SSE2 has no `pextrb`, LLVM lowers a byte extract without it through the stack, and
/// it folds a plain word extract plus shift back into that byte extract.
fn pextrw(b: &mut B<'_>, v: &str, word: u8) -> String {
    let words = b.val(&format!("bitcast <16 x i8> {v} to <2 x i64>"));
    b.val(&format!("call i32 asm \"pextrw $${word}, $1, $0\", \"=r,x\"(<2 x i64> {words})"))
}

/// Byte `lane` of `<16 x i8>` `v` on x86-64 (see [`pextrw`]).
fn extract_byte(b: &mut B<'_>, v: &str, lane: u8) -> String {
    let word = pextrw(b, v, lane / 2);
    let shifted = b.val(&format!("lshr i32 {word}, {}", u32::from(lane % 2) * 8));
    b.val(&format!("trunc i32 {shifted} to i8"))
}

/// `<16 x i8>` `v` with byte `lane` set to `x` on x86-64: the word holding it, merged and
/// put back by `pinsrw` in asm (SSE2 has no `pinsrb`; see [`pextrw`]).
fn insert_byte(b: &mut B<'_>, v: &str, x: &str, lane: u8) -> String {
    let word = pextrw(b, v, lane / 2);
    let (keep, shift) = if lane % 2 == 0 { (0xff00_u32, 0) } else { (0x00ff, 8) };
    let kept = b.val(&format!("and i32 {word}, {keep}"));
    let wide = b.val(&format!("zext i8 {x} to i32"));
    let byte = b.val(&format!("shl i32 {wide}, {shift}"));
    let merged = b.val(&format!("or i32 {kept}, {byte}"));
    let words = b.val(&format!("bitcast <16 x i8> {v} to <2 x i64>"));
    let set = b.val(&format!(
        "call <2 x i64> asm \"pinsrw $${}, $2, $0\", \"=x,0,r\"(<2 x i64> {words}, i32 {merged})",
        lane / 2
    ));
    b.val(&format!("bitcast <2 x i64> {set} to <16 x i8>"))
}
