//! Integer arithmetic by the Reference (walk/arith_checks.rs), built and run natively: for
//! generated operands, edges first, the built program's OUTCOME (its value, or which of the
//! pinned core's `panic_const_*` entry points it called) is the operator's definition computed
//! here, with overflow checks on and off.
//!
//! The Reference, "Arithmetic and Logical Binary Operators" ([expr.arith-logic.behavior]):
//! "† For integer types, division by zero panics."; "Overflow" ([expr.operator.int-overflow]):
//! "Integer operators will panic when they overflow when compiled in debug mode. .. When +, *
//! or binary - create a value greater than the maximum value, or less than the minimum value
//! that can be stored. .. Applying unary - to the most negative value of any signed integer
//! type, unless the operand is a literal expression .. Using / or %, where the left-hand
//! argument is the smallest integer of a signed integer type and the right-hand argument is
//! -1. These checks occur even when -C overflow-checks is disabled, for legacy reasons. Using
//! << or >> where the right operand is greater than or equal to the number of bits in the type
//! of the left operand, or is negative." With the checks off the operators wrap (the pinned
//! core's `wrapping_*` docs; a shift's amount is masked: `wrapping_shl` "the RHS of a wrapping
//! shift-left is restricted to the range of the type, rather than the bits shifted out of the
//! LHS being returned to the other end").
//!
//! The entry points are the pinned core's (library/core/src/panicking.rs `pub mod
//! panic_const`): each `-> !`, so a test cannot return from one. Each run is a forked child:
//! a host entry point ends the child with its own status (`10 + PanicConst::ALL` index), a
//! returned value is written to a pipe; the parent reads which.
//!
//! The program, at `T` and an operator drawn:
//! ```text
//! fn main() -> i64 {
//!     let x: U = <a's bits>; let y: U = <b's bits>;     // U: the unsigned type of T's width
//!     let a: T = x as T;    let b: T = y as T;
//!     (a op b) as i64                                   // or (-a) as i64
//! }
//! ```

use std::alloc::Global;
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Adt, BinOp, BodyView, Checked, Expr, ExprId, InferCtx, IntTy, ItemKey, LangItem, Needs, Outcome, PNode, PTy, PortableTys, UintTy, UnOp,
};

use super::run_native_with;
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, Built, PanicConst, Planted, Subst};

struct World {
    fx: Fx,
    main: ItemKey,
    /// One fn per entry point, in [`PanicConst::ALL`]'s order.
    panics: StdVec<ItemKey>,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let i64_ = fx.ty(PNode::Int(IntTy::I64));
    let never = fx.ty(PNode::Never);
    let main = fx.func("main", 0, &[], i64_, StdVec::new());
    let panics = PanicConst::ALL.iter().map(|p| fx.func(p.lang(), 0, &[], never, StdVec::new())).collect();
    fx.finish();
    World { fx, main, panics }
}

struct ArithAdts<'a> {
    w: &'a World,
    lits: &'a [(ExprId, u128)],
    checks: bool,
    planted: Planted,
}

impl LayoutFacts for ArithAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn repr(&self, _: ItemKey) -> ReprAttrs {
        ReprAttrs::RUST
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }
}

impl Adts for ArithAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, item: ItemKey, expr: ExprId) -> Option<u128> {
        if item != self.w.main {
            return None;
        }
        self.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.w.fx.sig_of(item).map(|_| Abi::Rust)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, crate::BuildError> {
        Ok(None)
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        PanicConst::ALL.iter().position(|p| p.lang() == name).map(|at| self.w.panics[at])
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        if self.checks { crate::OverflowChecks::On } else { crate::OverflowChecks::Off }
    }

    fn planted(&self) -> Planted {
        self.planted
    }
}

/// An integer type of the table: its width and signedness.
#[derive(Clone, Copy, Debug)]
struct Int {
    bits: u32,
    signed: bool,
}

const INTS: [Int; 6] = [
    Int { bits: 8, signed: true },
    Int { bits: 32, signed: true },
    Int { bits: 64, signed: true },
    Int { bits: 8, signed: false },
    Int { bits: 32, signed: false },
    Int { bits: 64, signed: false },
];

impl Int {
    fn node(self) -> PNode {
        match (self.bits, self.signed) {
            (8, true) => PNode::Int(IntTy::I8),
            (32, true) => PNode::Int(IntTy::I32),
            (64, true) => PNode::Int(IntTy::I64),
            (8, false) => PNode::Uint(UintTy::U8),
            (32, false) => PNode::Uint(UintTy::U32),
            _ => PNode::Uint(UintTy::U64),
        }
    }

    fn unsigned(self) -> PNode {
        Int { signed: false, ..self }.node()
    }

    fn mask(self) -> u128 {
        (1u128 << self.bits) - 1
    }

    /// Bits `v` (masked) read as a value of this type.
    fn value(self, v: u128) -> i128 {
        let v = v & self.mask();
        if self.signed && v >> (self.bits - 1) == 1 { v as i128 - (1i128 << self.bits) } else { v as i128 }
    }

    fn min(self) -> i128 {
        if self.signed { -(1i128 << (self.bits - 1)) } else { 0 }
    }

    fn max(self) -> i128 {
        if self.signed { (1i128 << (self.bits - 1)) - 1 } else { self.mask() as i128 }
    }

    /// `v` wrapped into this type, then `as i64`.
    fn wrapped_as_i64(self, v: i128) -> i64 {
        self.value(v as u128) as i64
    }
}

/// An operator of the table.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Form {
    Bin(BinOp),
    Neg,
}

const FORMS: [Form; 8] = [
    Form::Bin(BinOp::Add),
    Form::Bin(BinOp::Sub),
    Form::Bin(BinOp::Mul),
    Form::Bin(BinOp::Div),
    Form::Bin(BinOp::Rem),
    Form::Bin(BinOp::Shl),
    Form::Bin(BinOp::Shr),
    Form::Neg,
];

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq)]
enum End {
    Value(i64),
    Panic(PanicConst),
}

/// THE ORACLE: the operator's definition at `t` on bits `a` and `b`, by the module doc's rules.
fn definition(form: Form, t: Int, checks: bool, a: u128, b: u128) -> End {
    let (x, y) = (t.value(a), t.value(b));
    let fits = |v: i128| (t.min()..=t.max()).contains(&v);
    let exact = |v: Option<i128>, p: PanicConst| match v {
        Some(v) if fits(v) || !checks => End::Value(t.wrapped_as_i64(v)),
        // Past i128 only for a 64-bit product, which wraps to the low 64 bits of the product.
        None if !checks => End::Value(t.wrapped_as_i64((x as u128).wrapping_mul(y as u128) as i128)),
        _ => End::Panic(p),
    };
    match form {
        Form::Bin(BinOp::Add) => exact(x.checked_add(y), PanicConst::AddOverflow),
        Form::Bin(BinOp::Sub) => exact(x.checked_sub(y), PanicConst::SubOverflow),
        Form::Bin(BinOp::Mul) => exact(x.checked_mul(y), PanicConst::MulOverflow),
        Form::Bin(op @ (BinOp::Div | BinOp::Rem)) => {
            let div = op == BinOp::Div;
            if y == 0 {
                End::Panic(if div { PanicConst::DivByZero } else { PanicConst::RemByZero })
            } else if t.signed && x == t.min() && y == -1 {
                End::Panic(if div { PanicConst::DivOverflow } else { PanicConst::RemOverflow })
            } else {
                End::Value(t.wrapped_as_i64(if div { x / y } else { x % y }))
            }
        }
        Form::Bin(op) => {
            // The amount read as unsigned: a negative amount is past the width.
            let amount = b & t.mask();
            let shl = op == BinOp::Shl;
            if checks && amount >= u128::from(t.bits) {
                return End::Panic(if shl { PanicConst::ShlOverflow } else { PanicConst::ShrOverflow });
            }
            let n = (amount % u128::from(t.bits)) as u32;
            End::Value(if shl { t.wrapped_as_i64(((a & t.mask()) << n) as i128) } else { t.wrapped_as_i64(x >> n) })
        }
        Form::Neg => {
            if checks && x == t.min() {
                End::Panic(PanicConst::NegOverflow)
            } else {
                End::Value(t.wrapped_as_i64(-x))
            }
        }
    }
}

/// An expression node (the fixture's `Bb::e` is its crate's own).
fn put(b: &mut Bb, x: Expr) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = x;
    at
}

/// The module doc's `main`.
fn main_body(form: Form, t: Int, a: u128, b: u128) -> (Bb, ExprId, StdVec<(ExprId, u128)>) {
    let mut bb = Bb::new();
    let mut lits = StdVec::new();
    let mut stmts = StdVec::new();
    let (ty, uty, i64_) = (bb.ty(t.node()), bb.ty(t.unsigned()), bb.ty(PNode::Int(IntTy::I64)));
    let mut operand = |bb: &mut Bb, stmts: &mut StdVec<_>, raw: &str, name: &str, bits: u128| {
        let lit = bb.int(None);
        lits.push((lit, bits & t.mask()));
        let p = bb.bind(raw);
        stmts.push(bb.let_(p, Some(uty), lit));
        let x = bb.local(p);
        let cast = put(bb, Expr::Cast { operand: x, ty });
        let q = bb.bind(name);
        stmts.push(bb.let_(q, Some(ty), cast));
        bb.local(q)
    };
    let l = operand(&mut bb, &mut stmts, "x", "a", a);
    let value = match form {
        Form::Bin(op) => {
            let r = operand(&mut bb, &mut stmts, "y", "b", b);
            put(&mut bb, Expr::Binary { op, lhs: l, rhs: r })
        }
        Form::Neg => put(&mut bb, Expr::Unary { op: UnOp::Neg, operand: l }),
    };
    let cast = put(&mut bb, Expr::Cast { operand: value, ty: i64_ });
    let root = bb.block(&stmts, Some(cast));
    (bb, root, lits)
}

fn checked_of(w: &World, view: &BodyView<'_>) -> Result<Checked<'static>, String> {
    let mut ty = InferCtx::new_in(Alloc::global());
    let c = match blaze_typeck::check(view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => return Err(format!("the check of main needed {:?}", n.questions)),
        Err(error) => return Err(format!("the check of main failed: {error:?}")),
    };
    if !c.diags.is_empty() {
        return Err(format!("the check of main gave diagnostics: {:?}", c.diags));
    }
    Ok(c)
}

unsafe extern "C" {
    fn fork() -> i32;
    fn _exit(status: i32) -> !;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
}

/// The child's status when it returned a value; an entry point's is `PANICKED + its index`;
/// `BROKE` when the run itself panicked.
const RETURNED: i32 = 0;
const PANICKED: i32 = 10;
const BROKE: i32 = 99;

macro_rules! host_panics {
    ($($name:ident = $at:expr,)+) => {
        $(
            /// An entry point on the host: ends the child with its own status.
            extern "C" fn $name() {
                // SAFETY: only ever called in the forked child (`run`), which has nothing to flush.
                unsafe { _exit(PANICKED + $at) }
            }
        )+
        /// The host fns, in [`PanicConst::ALL`]'s order.
        const HOSTS: [extern "C" fn(); 10] = [$($name),+];
    };
}

host_panics! {
    host_add = 0, host_sub = 1, host_mul = 2, host_div = 3, host_rem = 4,
    host_neg = 5, host_shr = 6, host_shl = 7, host_div_zero = 8, host_rem_zero = 9,
}

/// Builds `main` and runs it in a forked child: how it ended.
fn run(w: &World, form: Form, t: Int, a: u128, b: u128, checks: bool, planted: Planted) -> Result<End, String> {
    let (mb, root, lits) = main_body(form, t, a, b);
    let adts = ArithAdts { w, lits: &lits, checks, planted };
    let view = mb.view(w.main, root);
    let checked = checked_of(w, &view)?;
    let sig = w.fx.sig_of(w.main).ok_or_else(|| String::from("no signature for main"))?;
    let mut ground = PortableTys::new_in(Alloc::global());
    let built: Built<Global> = {
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        crate::build(&view, &checked, &sig, &mut subst, &adts, Global).map_err(|error| format!("the build of main failed: {error}"))?
    };
    blaze_lir::verify(built.func.view()).map_err(|error| format!("main does not verify: {error}"))?;
    let host: StdVec<(ItemKey, u64)> = w.panics.iter().zip(HOSTS).map(|(&k, f)| (k, f as usize as u64)).collect();
    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds two ints, as `pipe` writes.
    if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
        return Err(String::from("pipe failed"));
    }
    // SAFETY: the child runs the placed code and leaves by `_exit` alone (never back into the
    // test harness); macOS's libSystem makes malloc safe across `fork`.
    let pid = unsafe { fork() };
    if pid == 0 {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_native_with(&host, &[(w.main, None, &built)])));
        let status = match ran {
            Ok(v) => {
                let bytes = v.to_le_bytes();
                // SAFETY: eight bytes from a live array to the pipe's write end.
                unsafe { write(fds[1], bytes.as_ptr(), 8) };
                RETURNED
            }
            Err(_) => BROKE,
        };
        // SAFETY: the child ends here.
        unsafe { _exit(status) }
    }
    // SAFETY: the parent's copy of the write end is closed so the read sees the child's end.
    unsafe { close(fds[1]) };
    let mut status = 0i32;
    // SAFETY: `pid` is this process's child; `status` is a live int.
    let waited = unsafe { waitpid(pid, &mut status, 0) };
    let mut bytes = [0u8; 8];
    // SAFETY: up to eight bytes into a live array, from the pipe's read end.
    let got = unsafe { read(fds[0], bytes.as_mut_ptr(), 8) };
    // SAFETY: the read end, opened above.
    unsafe { close(fds[0]) };
    if waited != pid || status & 0x7f != 0 {
        return Err(format!("the child ended by signal {} (wait gave {waited})", status & 0x7f));
    }
    match (status >> 8) & 0xff {
        RETURNED if got == 8 => Ok(End::Value(i64::from_le_bytes(bytes))),
        code if (PANICKED..PANICKED + 10).contains(&code) => Ok(End::Panic(PanicConst::ALL[(code - PANICKED) as usize])),
        code => Err(format!("the child ended with status {code} (read {got} bytes)")),
    }
}

/// The words of the oracle's finding (`blaze_sim::control` looks for them).
const SAYS: &str = "where the Reference gives";

fn agree(w: &World, form: Form, t: Int, checks: bool, a: u128, b: u128, planted: Planted) -> Result<(), Broke> {
    let want = definition(form, t, checks, a, b);
    let what = || format!("{form:?} at {t:?} on {} and {} (overflow checks {checks})", t.value(a), t.value(b));
    let got = run(w, form, t, a, b, checks, planted).map_err(|e| Broke::Oracle(format!("{}: {e}", what())))?;
    if got != want {
        return Err(Broke::Oracle(format!("{} ended {got:?}, {SAYS} {want:?}", what())));
    }
    Ok(())
}

/// An operand's bits: an edge of the type (0, 1, -1, MIN, MAX, MIN + 1, the width and around
/// it), or any bits.
fn operand(tape: &Tape, t: Int, label: &'static str) -> u128 {
    let edges = [0, 1, t.mask(), t.min() as u128, t.max() as u128, (t.min() + 1) as u128, u128::from(t.bits), u128::from(t.bits) - 1, 2];
    if tape.coin(label) { edges[tape.index(edges.len(), label)] & t.mask() } else { u128::from(tape.next_u64(label)) & t.mask() }
}

fn case(w: &World, tape: &Tape, planted: Planted) -> Result<(), Broke> {
    let form = FORMS[tape.index(FORMS.len(), "operator")];
    let t = INTS[tape.index(INTS.len(), "type")];
    // `-` on an unsigned type is no builtin operator (the checker refuses it).
    let t = if form == Form::Neg { Int { signed: true, ..t } } else { t };
    let checks = tape.coin("overflow checks");
    // A quarter of the cases are a pair at the edge of some check, drawn as a pair: two
    // independent edges would meet (`MIN`, `-1`) once in thousands of cases.
    let (min, max, ones, width) = (t.min() as u128 & t.mask(), t.max() as u128, t.mask(), u128::from(t.bits));
    let pairs = [(min, ones), (max, ones), (min, 0), (max, max), (min, min), (min, 1), (1, width), (1, width - 1), (max, 2)];
    let (a, b) = if tape.index(4, "an edge pair") == 0 {
        pairs[tape.index(pairs.len(), "which pair")]
    } else {
        (operand(tape, t, "a"), operand(tape, t, "b"))
    };
    agree(w, form, t, checks, a, b, planted)
}

/// THE PROPERTY: for generated operands (edges half the time), every operator at every type of
/// the table, checks on and off, ends as the Reference defines: a value, or which panic.
#[test]
fn integer_arithmetic_ends_as_the_reference_defines() {
    let w = world();
    blaze_sim::check(Config::cases(150), |tape: &Tape| case(&w, tape, Planted::Nothing));
}

/// The control (rule 8): with no check of `MIN % -1` planted (what the walk built before:
/// the machine's `srem` gives 0), the same property fails, in the oracle's words.
#[test]
fn the_control_no_check_on_rem_is_caught() {
    let w = world();
    blaze_sim::control(Config::cases(3000), Planted::Nothing, Planted::NoRemOverflowCheck, SAYS, |tape: &Tape, planted: Planted| {
        case(&w, tape, planted)
    });
}

/// The edges no draw decides: each signed type's `MIN / -1`, `MIN % -1`, `x / 0`, `x % 0` in
/// both settings, and `+ - * << >> -` at the edges with checks on and off.
#[test]
fn the_edges_end_as_the_reference_defines() {
    let w = world();
    for t in INTS {
        let edges = [0, 1, t.mask(), t.min() as u128, t.max() as u128, u128::from(t.bits)];
        for form in FORMS {
            if form == Form::Neg && !t.signed {
                continue;
            }
            for checks in [false, true] {
                for a in edges {
                    for b in edges {
                        agree(&w, form, t, checks, a & t.mask(), b & t.mask(), Planted::Nothing).unwrap_or_else(|e| panic!("{e:?}"));
                    }
                }
            }
        }
    }
}
