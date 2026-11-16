//! Overloaded indexing and raw borrows (walk/index.rs): `w[i]` on a type with an `Index` impl
//! is a call of `Index::index` whose returned reference is the place; `&raw const place` is the
//! place's address. Built from a body the checker types, then run natively.

use super::*;

const WIDGET: ItemKey = ItemKey(10);
const INDEX_FN: ItemKey = ItemKey(12);
const U64: PNode = PNode::Uint(UintTy::U64);

/// `<Widget as Index<usize>>::index` on the host: counts the call in `calls` and returns
/// `&self.data[i]`. The counter stands in for a `Cell<u64>` field: an `Index` impl that counts
/// its calls behind `&self` writes through interior mutability.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_index(this: *mut u64, i: u64) -> *const u64 {
    // SAFETY: the built code passes the address of a live `Widget` (`repr(C)`: `data: [u64; 4]`
    // at 0, `calls: u64` at 32) and an index below 4; the test's body indexes 0, 2 and 3.
    unsafe {
        *this.add(4) += 1;
        this.add(i as usize)
    }
}

/// `w.<name>` with `w` a fresh use of binding `w`.
fn field_of(b: &mut BodyBuf, w: PatId, name: &str) -> ExprId {
    let w_e = b.local(w);
    b.field(w_e, name)
}

/// `w[i]` with `i` a `usize` literal.
fn index_of(b: &mut BodyBuf, w: PatId, i: u128) -> ExprId {
    let w_e = b.local(w);
    let i = b.int(i, Some(IntSuffix::Uint(UintTy::Usize)));
    b.index(w_e, i)
}

/// `Widget { data: [10, 20, 30, 40], calls: 0 }`.
fn widget_lit(b: &mut BodyBuf) -> ExprId {
    let elems: [ExprId; 4] = [10, 20, 30, 40].map(|v| b.int(v, None));
    let data = b.array(&elems);
    let zero = b.int(0, None);
    let path = b.path("Widget");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("data"), expr: data });
    b.field_inits.push(FieldInit { name: Name::of("calls"), expr: zero });
    b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: None })
}

/// The Reference, "Array and slice indexing expressions": "For other types an index expression
/// a[b] is equivalent to *std::ops::Index::index(&a, b), or *std::ops::IndexMut::index_mut(&mut
/// a, b) in a mutable place expression context." "Raw borrow operators": "`&raw const expr`
/// then creates a const raw pointer of type `*const T` to the given place".
/// ```text
/// #[repr(C)] struct Widget { data: [u64; 4], calls: u64 }   // `calls` a Cell in spirit
/// impl Index<usize> for Widget { type Output = u64; fn index(&self, i: usize) -> &u64 }  // host_index
/// fn main() -> u64 {
///     let w = Widget { data: [10, 20, 30, 40], calls: 0 };
///     let q = &raw const w.calls;   // before any call
///     let a = w[2];                 // index: calls 1, a = 30
///     let b = w[0] + w[3];          // index twice: calls 3, b = 10 + 40 = 50
///     let p = &raw const w.data;
///     let c = (*p)[1];              // builtin, no call: 20
///     *q * 100_000 + a * 1000 + b * 10 + c
/// }
/// ```
/// Each `w[i]` calls `index` once and reads the element its reference points to, so `*q` is 3
/// and the result is 3 * 100_000 + 30 * 1000 + 50 * 10 + 20 = 330520. `q` is taken before the
/// calls: an address reads the count the calls left (3); a raw borrow that copied the place
/// would read 0 (30520). An index read twice per use reads 5 (530520); a builtin element of
/// the struct is not a form at all.
///
/// The same body with `w[1] = 9` is a mutable place expression context, which needs
/// `IndexMut::index_mut`; the check selects `Index` only (blaze-typeck rules/field.rs:
/// "Omitted for now: `IndexMut` selection for mutable places"), so the build refuses it by name
/// rather than write through `Index::index`'s shared reference.
#[test]
fn an_overloaded_index_calls_index_once_per_use_and_a_raw_borrow_is_the_address() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U64);
    w.bounds_fn();
    let array = w.array(U64, 4);
    w.strukt(10, "Widget", &[("data", array), ("calls", U64)]);
    w.reprs.push((WIDGET, ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let widget = PNode::Adt(WIDGET, List::EMPTY);
    let r_widget = w.ref_(Mutbl::Not, widget);
    let r_u64 = w.ref_(Mutbl::Not, U64);
    w.func(12, "widget_index", &[r_widget, USIZE], r_u64);
    w.index_impl(widget, USIZE, U64, INDEX_FN);

    let mut b = BodyBuf::new();
    let (wp, q, a, bb, p, c) = (b.bind("w"), b.bind("q"), b.bind("a"), b.bind("b"), b.bind("p"), b.bind("c"));
    let lit = widget_lit(&mut b);
    let calls = field_of(&mut b, wp, "calls");
    let q_init = b.e(Expr::RawRef { mutbl: Mutbl::Not, operand: calls });
    // `Index::Output` is a projection the check resolves late: the element reads are bound
    // at `u64` before arithmetic, so `+` and `*` meet integers, not `Add` on a variable.
    let u64_ty = b.types.intern(U64).expect("a type interns");
    let (b0, b3) = (b.bind("b0"), b.bind("b3"));
    let a_init = index_of(&mut b, wp, 2);
    let w0 = index_of(&mut b, wp, 0);
    let w3 = index_of(&mut b, wp, 3);
    let b0_e = b.local(b0);
    let b3_e = b.local(b3);
    let b_init = b.bin(BinOp::Add, b0_e, b3_e);
    let data = field_of(&mut b, wp, "data");
    let p_init = b.e(Expr::RawRef { mutbl: Mutbl::Not, operand: data });
    let p_e = b.local(p);
    let star_p = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: p_e });
    let one = b.int(1, None);
    let c_init = b.index(star_p, one);
    let q_e = b.local(q);
    let count = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: q_e });
    let k = b.int(100_000, None);
    let sum = b.bin(BinOp::Mul, count, k);
    let a_e = b.local(a);
    let k = b.int(1000, None);
    let a_k = b.bin(BinOp::Mul, a_e, k);
    let sum = b.bin(BinOp::Add, sum, a_k);
    let b_e = b.local(bb);
    let k = b.int(10, None);
    let b_k = b.bin(BinOp::Mul, b_e, k);
    let sum = b.bin(BinOp::Add, sum, b_k);
    let c_e = b.local(c);
    let sum = b.bin(BinOp::Add, sum, c_e);
    let root = b.block(
        &[
            Stmt::Let { pat: wp, ty: None, init: Some(lit), else_: None },
            Stmt::Let { pat: q, ty: None, init: Some(q_init), else_: None },
            Stmt::Let { pat: a, ty: Some(u64_ty), init: Some(a_init), else_: None },
            Stmt::Let { pat: b0, ty: Some(u64_ty), init: Some(w0), else_: None },
            Stmt::Let { pat: b3, ty: Some(u64_ty), init: Some(w3), else_: None },
            Stmt::Let { pat: bb, ty: Some(u64_ty), init: Some(b_init), else_: None },
            Stmt::Let { pat: p, ty: None, init: Some(p_init), else_: None },
            Stmt::Let { pat: c, ty: None, init: Some(c_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    // One call target per use of `index`, all the same instance `index::<Widget, usize>`.
    assert!(built.refs.iter().any(|i| i.item == INDEX_FN), "the build calls `Index::index`");
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64), (INDEX_FN, host_index as usize as u64)];
        assert_eq!(run_native_with(&host, &[(MAIN, None, &built)]), 330520);
    }
    let _ = built;

    // `let w = Widget { .. }; w[1] = 9; 0`: refused by name, never `Index::index` written through.
    let mut m = BodyBuf::new();
    let wp = m.bind("w");
    let lit = widget_lit(&mut m);
    let place = index_of(&mut m, wp, 1);
    let nine = m.int(9, None);
    let assign = m.e(Expr::Assign { lhs: place, rhs: nine });
    let zero = m.int(0, None);
    let root = m.block(
        &[Stmt::Let { pat: wp, ty: None, init: Some(lit), else_: None }, Stmt::Expr { expr: assign, semi: true }],
        Some(zero),
    );
    let error = build(&w, &m, root).err().expect("a write through an overloaded index is refused");
    assert!(error.contains("records no `IndexMut::index_mut`"), "{error}");
}
