//! The HIR evaluator's operations and programs against a reference written as the MIR
//! evaluator computes: each value its native integer type read from its bytes, each check the
//! assert of `hir-ty-mir`'s `transform/asserts.rs`. The differential against the MIR
//! evaluator itself, over a database, is `hir-ty-mir`'s `tests/consteval/hir_eval.rs`.

use hir_def::hir::{ArithOp, BinaryOp, CmpOp, LogicOp, Ordering, UnaryOp};

use super::{MAX_NODES, Op, Stop, exec, ops};
use crate::consteval::hir_eval::{Kind, Panic};

/// Runs `$body` with `$t` the native type of the integer kind `$k` and `$u` the unsigned type
/// of its width.
macro_rules! on_int {
    ($k:expr, |$t:ident, $u:ident| $body:expr) => {
        match $k {
            Kind::I8 => on_int!(@ i8, u8, $t, $u, $body),
            Kind::I16 => on_int!(@ i16, u16, $t, $u, $body),
            Kind::I32 => on_int!(@ i32, u32, $t, $u, $body),
            Kind::I64 => on_int!(@ i64, u64, $t, $u, $body),
            Kind::I128 => on_int!(@ i128, u128, $t, $u, $body),
            Kind::U8 => on_int!(@ u8, u8, $t, $u, $body),
            Kind::U16 => on_int!(@ u16, u16, $t, $u, $body),
            Kind::U32 => on_int!(@ u32, u32, $t, $u, $body),
            Kind::U64 => on_int!(@ u64, u64, $t, $u, $body),
            Kind::U128 => on_int!(@ u128, u128, $t, $u, $body),
            Kind::Bool | Kind::Char => unreachable!("not an integer"),
        }
    };
    (@ $native:ty, $unsigned:ty, $t:ident, $u:ident, $body:expr) => {{
        #[allow(unused)]
        type $t = $native;
        #[allow(unused)]
        type $u = $unsigned;
        $body
    }};
}

/// The native value of `T` whose little-endian bytes are the low bytes of `v`.
macro_rules! read {
    ($t:ty, $v:expr) => {
        <$t>::from_le_bytes($v.to_le_bytes()[..size_of::<$t>()].try_into().unwrap())
    };
}

/// `v`'s little-endian bytes, zero-extended.
macro_rules! bits {
    ($v:expr) => {{
        let bytes = $v.to_le_bytes();
        let mut out = [0; 16];
        out[..bytes.len()].copy_from_slice(&bytes);
        u128::from_le_bytes(out)
    }};
}

mod reference {
    use super::*;

    pub(super) fn unary(op: UnaryOp, k: Kind, a: u128) -> Result<u128, Panic> {
        match (op, k) {
            (UnaryOp::Not, Kind::Bool) => Ok(u128::from(a == 0)),
            (UnaryOp::Not, _) => on_int!(k, |T, U| Ok(bits!(!read!(T, a)))),
            (UnaryOp::Neg, _) => on_int!(k, |T, U| match read!(T, a).checked_neg() {
                Some(r) => Ok(bits!(r)),
                None => Err(Panic::Neg),
            }),
            (UnaryOp::Deref, _) => unreachable!(),
        }
    }

    pub(super) fn binary(op: BinaryOp, lk: Kind, rk: Kind, a: u128, b: u128) -> Result<u128, Panic> {
        match op {
            BinaryOp::CmpOp(cmp) => {
                let order = match lk {
                    Kind::Bool | Kind::Char => a.cmp(&b),
                    _ => on_int!(lk, |T, U| read!(T, a).cmp(&read!(T, b))),
                };
                Ok(u128::from(match cmp {
                    CmpOp::Eq { negated } => order.is_eq() != negated,
                    CmpOp::Ord { ordering: Ordering::Less, strict: true } => order.is_lt(),
                    CmpOp::Ord { ordering: Ordering::Less, strict: false } => order.is_le(),
                    CmpOp::Ord { ordering: Ordering::Greater, strict: true } => order.is_gt(),
                    CmpOp::Ord { ordering: Ordering::Greater, strict: false } => order.is_ge(),
                }))
            }
            BinaryOp::ArithOp(ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor)
                if lk == Kind::Bool =>
            {
                let (x, y) = (a != 0, b != 0);
                Ok(u128::from(match op {
                    BinaryOp::ArithOp(ArithOp::BitAnd) => x & y,
                    BinaryOp::ArithOp(ArithOp::BitOr) => x | y,
                    _ => x ^ y,
                }))
            }
            BinaryOp::ArithOp(arith @ (ArithOp::Shl | ArithOp::Shr)) => {
                let amount: u128 = on_int!(rk, |R, RU| read!(R, b) as RU as u128);
                on_int!(lk, |T, U| {
                    if amount >= u128::from(T::BITS) {
                        return Err(if arith == ArithOp::Shl { Panic::Shl } else { Panic::Shr });
                    }
                    let x = read!(T, a);
                    Ok(bits!(if arith == ArithOp::Shl { x << amount } else { x >> amount }))
                })
            }
            BinaryOp::ArithOp(arith) => on_int!(lk, |T, U| {
                let (x, y) = (read!(T, a), read!(T, b));
                let r = match arith {
                    ArithOp::Add => x.checked_add(y).ok_or(Panic::Add)?,
                    ArithOp::Sub => x.checked_sub(y).ok_or(Panic::Sub)?,
                    ArithOp::Mul => x.checked_mul(y).ok_or(Panic::Mul)?,
                    ArithOp::Div => x.checked_div(y).ok_or(Panic::Div)?,
                    ArithOp::Rem => x.checked_rem(y).ok_or(Panic::Rem)?,
                    ArithOp::BitAnd => x & y,
                    ArithOp::BitOr => x | y,
                    ArithOp::BitXor => x ^ y,
                    ArithOp::Shl | ArithOp::Shr => unreachable!(),
                };
                Ok(bits!(r))
            }),
            BinaryOp::LogicOp(_) | BinaryOp::Assignment { .. } => unreachable!(),
        }
    }

    pub(super) fn cast(from: Kind, to: Kind, a: u128) -> u128 {
        if to == Kind::Char {
            assert_eq!(from, Kind::U8);
            return u128::from(u32::from(read!(u8, a) as char));
        }
        match from {
            Kind::Bool => on_int!(to, |D, DU| bits!((a != 0) as D)),
            Kind::Char => {
                let c = char::from_u32(read!(u32, a)).unwrap();
                on_int!(to, |D, DU| bits!(c as D))
            }
            _ => on_int!(from, |T, TU| {
                let x = read!(T, a);
                on_int!(to, |D, DU| bits!(x as D))
            }),
        }
    }
}

/// A seeded xorshift: the generated cases are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[self.below(of.len())]
    }

    /// A value of `k`, an edge of its range half the time.
    fn value(&mut self, k: Kind) -> u128 {
        match k {
            Kind::Bool => (self.next() & 1).into(),
            Kind::Char => self.pick(&[0, 'a' as u128, 0x7f, 0xff, 0xd7ff, 0xe000, 0x10ffff]),
            _ if self.next() & 1 == 0 => {
                let bits = u128::from(k.bits());
                let edges = [
                    0,
                    1,
                    2,
                    u128::MAX,
                    u128::MAX - 1,
                    1 << (bits - 1),
                    (1 << (bits - 1)) - 1,
                    (1 << (bits - 1)) + 1,
                    bits - 1,
                    bits,
                    bits + 1,
                    u128::MAX - bits + 1,
                ];
                k.wrap(self.pick(&edges))
            }
            _ => k.wrap(u128::from(self.next()) << 64 | u128::from(self.next())),
        }
    }
}

const INTS: [Kind; 10] = [
    Kind::I8,
    Kind::I16,
    Kind::I32,
    Kind::I64,
    Kind::I128,
    Kind::U8,
    Kind::U16,
    Kind::U32,
    Kind::U64,
    Kind::U128,
];

const ARITH: [ArithOp; 10] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Rem,
    ArithOp::Shl,
    ArithOp::Shr,
    ArithOp::BitAnd,
    ArithOp::BitOr,
    ArithOp::BitXor,
];

const CMP: [CmpOp; 6] = [
    CmpOp::Eq { negated: false },
    CmpOp::Eq { negated: true },
    CmpOp::Ord { ordering: Ordering::Less, strict: true },
    CmpOp::Ord { ordering: Ordering::Less, strict: false },
    CmpOp::Ord { ordering: Ordering::Greater, strict: true },
    CmpOp::Ord { ordering: Ordering::Greater, strict: false },
];

#[test]
fn ops_match_the_reference() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..2_000 {
        for k in Kind::ALL {
            let (a, b) = (rng.value(k), rng.value(k));
            if k.is_int() {
                for op in ARITH {
                    let rk = if matches!(op, ArithOp::Shl | ArithOp::Shr) { rng.pick(&INTS) } else { k };
                    let b = if rk == k { b } else { rng.value(rk) };
                    let op = BinaryOp::ArithOp(op);
                    assert_eq!(
                        ops::binary(op, k, rk, a, b),
                        reference::binary(op, k, rk, a, b),
                        "{a:#x} {op:?} {b:#x} as {k:?}, {rk:?}"
                    );
                }
                assert_eq!(ops::unary(UnaryOp::Not, k, a), reference::unary(UnaryOp::Not, k, a));
                if k.is_signed() {
                    let neg = UnaryOp::Neg;
                    assert_eq!(ops::unary(neg, k, a), reference::unary(neg, k, a), "-{a:#x} {k:?}");
                }
                for to in INTS {
                    assert_eq!(ops::cast(k, to, a), reference::cast(k, to, a), "{a:#x} {k:?} as {to:?}");
                }
            } else {
                for to in INTS {
                    assert_eq!(ops::cast(k, to, a), reference::cast(k, to, a), "{a:#x} {k:?} as {to:?}");
                }
            }
            if k == Kind::Bool {
                assert_eq!(ops::unary(UnaryOp::Not, k, a), reference::unary(UnaryOp::Not, k, a));
                for op in [ArithOp::BitAnd, ArithOp::BitOr, ArithOp::BitXor] {
                    let op = BinaryOp::ArithOp(op);
                    assert_eq!(ops::binary(op, k, k, a, b), reference::binary(op, k, k, a, b));
                }
            }
            for cmp in CMP {
                let op = BinaryOp::CmpOp(cmp);
                assert_eq!(ops::binary(op, k, k, a, b), reference::binary(op, k, k, a, b));
            }
        }
        let a = rng.value(Kind::U8);
        assert_eq!(ops::cast(Kind::U8, Kind::Char, a), reference::cast(Kind::U8, Kind::Char, a));
    }
}

#[test]
fn every_check_fails_where_rustc_s_does() {
    use ArithOp::*;
    let run = |op, k, a: i128, b: i128| {
        ops::binary(BinaryOp::ArithOp(op), k, k, k.wrap(a as u128), k.wrap(b as u128))
    };
    for k in INTS {
        let unused = 128 - k.bits();
        let (min, max) =
            if k.is_signed() { (i128::MIN >> unused, i128::MAX >> unused) } else { (0, -1) };
        assert_eq!(run(Add, k, max, 1), Err(Panic::Add), "{k:?}");
        assert_eq!(run(Sub, k, min, 1), Err(Panic::Sub), "{k:?}");
        assert_eq!(run(Mul, k, max, 2), Err(Panic::Mul), "{k:?}");
        assert_eq!(run(Div, k, 1, 0), Err(Panic::Div), "{k:?}");
        assert_eq!(run(Rem, k, 1, 0), Err(Panic::Rem), "{k:?}");
        assert_eq!(run(Shl, k, 1, i128::from(k.bits())), Err(Panic::Shl), "{k:?}");
        assert_eq!(run(Shr, k, 1, i128::from(k.bits())), Err(Panic::Shr), "{k:?}");
        assert!(run(Shl, k, 1, i128::from(k.bits()) - 1).is_ok(), "{k:?}");
        if k.is_signed() {
            assert_eq!(run(Div, k, min, -1), Err(Panic::Div), "{k:?}");
            assert_eq!(run(Rem, k, min, -1), Err(Panic::Rem), "{k:?}");
            assert_eq!(run(Shl, k, 1, -1), Err(Panic::Shl), "{k:?}");
            assert_eq!(ops::unary(UnaryOp::Neg, k, k.wrap(min as u128)), Err(Panic::Neg));
        }
    }
}

/// An expression of the shape without paths, which need a database.
enum Tree {
    Lit(Kind, u128),
    Unary(UnaryOp, Box<Tree>),
    Binary(BinaryOp, Box<Tree>, Box<Tree>),
    Cast(Kind, Box<Tree>),
}

impl Tree {
    fn kind(&self) -> Kind {
        match self {
            Tree::Lit(k, _) => *k,
            Tree::Unary(_, e) => e.kind(),
            Tree::Binary(BinaryOp::CmpOp(_) | BinaryOp::LogicOp(_), ..) => Kind::Bool,
            Tree::Binary(_, lhs, _) => lhs.kind(),
            Tree::Cast(to, _) => *to,
        }
    }

    fn generate(rng: &mut Rng, k: Kind, depth: u32) -> Tree {
        let sub = |rng: &mut Rng, k| Box::new(Tree::generate(rng, k, depth - 1));
        if depth == 0 || rng.below(4) == 0 {
            return Tree::Lit(k, rng.value(k));
        }
        match k {
            Kind::Char => Tree::Cast(k, sub(rng, Kind::U8)),
            Kind::Bool => match rng.below(5) {
                0 => Tree::Unary(UnaryOp::Not, sub(rng, k)),
                1 => {
                    let of = rng.pick(&Kind::ALL);
                    Tree::Binary(BinaryOp::CmpOp(rng.pick(&CMP)), sub(rng, of), sub(rng, of))
                }
                2 => {
                    let op = rng.pick(&[LogicOp::And, LogicOp::Or]);
                    Tree::Binary(BinaryOp::LogicOp(op), sub(rng, k), sub(rng, k))
                }
                3 => {
                    let op = rng.pick(&[ArithOp::BitAnd, ArithOp::BitOr, ArithOp::BitXor]);
                    Tree::Binary(BinaryOp::ArithOp(op), sub(rng, k), sub(rng, k))
                }
                _ => Tree::Lit(k, rng.value(k)),
            },
            _ => match rng.below(4) {
                0 if k.is_signed() => Tree::Unary(UnaryOp::Neg, sub(rng, k)),
                0 => Tree::Unary(UnaryOp::Not, sub(rng, k)),
                1 => {
                    let from = rng.pick(&Kind::ALL);
                    let from = if from == Kind::Char || rng.below(2) == 0 { from } else { rng.pick(&INTS) };
                    Tree::Cast(k, sub(rng, from))
                }
                _ => {
                    let op = rng.pick(&ARITH);
                    let rk = if matches!(op, ArithOp::Shl | ArithOp::Shr) { rng.pick(&INTS) } else { k };
                    Tree::Binary(BinaryOp::ArithOp(op), sub(rng, k), sub(rng, rk))
                }
            },
        }
    }

    /// The program `Compiler` makes of this tree: postorder, a jump after the left side of
    /// `&&` and `||`.
    fn compile(&self, ops: &mut Vec<Op>) {
        match self {
            Tree::Lit(_, v) => ops.push(Op::Push(*v)),
            Tree::Unary(op, e) => {
                e.compile(ops);
                ops.push(Op::Unary(*op, e.kind()));
            }
            Tree::Binary(BinaryOp::LogicOp(logic), lhs, rhs) => {
                lhs.compile(ops);
                let jump = ops.len();
                ops.push(Op::Short { on: *logic == LogicOp::Or, skip: 0 });
                rhs.compile(ops);
                let skip = (ops.len() - jump - 1) as u16;
                ops[jump] = Op::Short { on: *logic == LogicOp::Or, skip };
            }
            Tree::Binary(op, lhs, rhs) => {
                lhs.compile(ops);
                rhs.compile(ops);
                ops.push(Op::Binary(*op, lhs.kind(), rhs.kind()));
            }
            Tree::Cast(to, e) => {
                e.compile(ops);
                ops.push(Op::Cast(e.kind(), *to));
            }
        }
    }

    /// The value as the MIR of the tree computes it: operands left to right, each checked
    /// before its operation, the right side of `&&` and `||` only when needed.
    fn eval(&self) -> Result<u128, Panic> {
        match self {
            Tree::Lit(_, v) => Ok(*v),
            Tree::Unary(op, e) => reference::unary(*op, e.kind(), e.eval()?),
            Tree::Binary(BinaryOp::LogicOp(logic), lhs, rhs) => {
                let a = lhs.eval()?;
                if (a == 1) == (*logic == LogicOp::Or) { Ok(a) } else { rhs.eval() }
            }
            Tree::Binary(op, lhs, rhs) => {
                let a = lhs.eval()?;
                let b = rhs.eval()?;
                reference::binary(*op, lhs.kind(), rhs.kind(), a, b)
            }
            Tree::Cast(to, e) => Ok(reference::cast(e.kind(), *to, e.eval()?)),
        }
    }
}

#[test]
fn generated_trees_match() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut ops = Vec::new();
    let (mut failed, mut total) = (0, 0);
    for _ in 0..20_000 {
        let kind = rng.pick(&Kind::ALL);
        let tree = Tree::generate(&mut rng, kind, 6);
        ops.clear();
        tree.compile(&mut ops);
        if ops.len() > MAX_NODES {
            continue;
        }
        let expected = tree.eval();
        failed += usize::from(expected.is_err());
        total += 1;
        let got = exec::<()>(&ops, |_| unreachable!("no paths"));
        assert_eq!(got, expected.map_err(Stop::Panic), "{ops:?}");
    }
    // Both outcomes are exercised: the generator is not all overflow, nor none.
    assert!(failed >= 1_000 && total - failed >= 1_000, "{failed} of {total} failed");
}

#[test]
fn programs_allocate_nothing() {
    let literal = [Op::Push(7)];
    let tree = [
        Op::Push(200),
        Op::Push(100),
        Op::Binary(BinaryOp::ArithOp(ArithOp::Add), Kind::U8, Kind::U8),
        Op::Cast(Kind::U8, Kind::I64),
    ];
    let (_, control) = blaze_alloc_count::measure(|| std::hint::black_box(Box::new(1u64)));
    assert_eq!(control.allocations, 1, "the test binary counts allocations");
    blaze_alloc_count::assert_at_most(0, "a literal's program", || {
        exec::<()>(&literal, |_| unreachable!()).unwrap()
    });
    blaze_alloc_count::assert_at_most(0, "a failing program", || {
        exec::<()>(&tree, |_| unreachable!()).unwrap_err()
    });
}

#[test]
fn sizes() {
    assert!(size_of::<Op>() <= 32, "{}", size_of::<Op>());
    assert_eq!(size_of::<Kind>(), 1);
    assert_eq!(size_of::<Panic>(), 1);
}
