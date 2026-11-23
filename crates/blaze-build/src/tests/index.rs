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
    b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None })
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
/// `IndexMut::index_mut`; this world has no `IndexMut`, so the check refuses it (E0596,
/// blaze-typeck rules/mut_places.rs) and nothing is built through `Index::index`'s shared
/// reference.
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

    // `let w = Widget { .. }; w[1] = 9; 0`: the check's E0596, never `Index::index` written through.
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
    let error = build(&w, &m, root).err().expect("a write through an overloaded index with no `IndexMut` is refused");
    assert!(error.contains("IndexMut"), "{error}");
}

const INDEX_MUT_FN: ItemKey = ItemKey(13);

/// `<Widget as IndexMut<usize>>::index_mut` on the host: counts the call in `writes` (at 40)
/// and returns `&mut self.data[i]`.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_index_mut(this: *mut u64, i: u64) -> *mut u64 {
    // SAFETY: the built code passes the address of a live `Widget` (`repr(C)`: `data: [u64;
    // 4]` at 0, `calls` at 32, `writes` at 40) and an index below 4.
    unsafe {
        *this.add(5) += 1;
        this.add(i as usize)
    }
}

/// The Reference, "Array and slice indexing expressions": "*std::ops::IndexMut::index_mut(&mut
/// a, b) in a mutable place expression context"; "Compound assignment expressions": the place
/// is evaluated once.
/// ```text
/// #[repr(C)] struct Widget { data: [u64; 4], calls: u64, writes: u64 }
/// impl Index<usize> for Widget { .. }      // host_index: calls += 1
/// impl IndexMut<usize> for Widget { .. }   // host_index_mut: writes += 1
/// fn main() -> u64 {
///     let w = Widget { data: [10, 20, 30, 40], calls: 0, writes: 0 };
///     w[1] = 9;                  // index_mut: writes 1, data[1] = 9
///     w[2] += 5;                 // index_mut once: writes 2, data[2] = 35
///     let r = &mut w[3];         // index_mut: writes 3
///     *r = 7;                    // data[3] = 7
///     let ref mut s = w[0];      // index_mut: writes 4
///     *s = 1;                    // data[0] = 1
///     let a: u64 = w[1] + w[2];  // index twice: calls 2, a = 44
///     let b: u64 = w[3] + w[0];  // index twice: calls 4, b = 8
///     w.calls * 100_000 + w.writes * 10_000 + a * 100 + b
/// }
/// ```
/// = 4 * 100_000 + 4 * 10_000 + 4400 + 8 = 444408. Every write through `Index::index` instead
/// reads calls 8, writes 0 and is refused before that (the check's `Checked::index_mut`, then
/// walk/index.rs `index_call`); `w[2] += 5` evaluating its place twice reads writes 5 (454408);
/// a write that missed the element leaves a or b different.
#[test]
fn a_mutable_overloaded_index_calls_index_mut_once_per_use() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U64);
    w.bounds_fn();
    let array = w.array(U64, 4);
    w.strukt(10, "Widget", &[("data", array), ("calls", U64), ("writes", U64)]);
    w.reprs.push((WIDGET, ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let widget = PNode::Adt(WIDGET, List::EMPTY);
    let r_widget = w.ref_(Mutbl::Not, widget);
    let m_widget = w.ref_(Mutbl::Mut, widget);
    let r_u64 = w.ref_(Mutbl::Not, U64);
    let m_u64 = w.ref_(Mutbl::Mut, U64);
    w.func(12, "widget_index", &[r_widget, USIZE], r_u64);
    w.func(13, "widget_index_mut", &[m_widget, USIZE], m_u64);
    w.index_impl(widget, USIZE, U64, INDEX_FN);
    w.index_mut_impl(INDEX_MUT_FN);

    let mut b = BodyBuf::new();
    let wp = b.bind("w");
    let elems: [ExprId; 4] = [10, 20, 30, 40].map(|v| b.int(v, None));
    let data = b.array(&elems);
    let (zero_a, zero_b) = (b.int(0, None), b.int(0, None));
    let path = b.path("Widget");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("data"), expr: data });
    b.field_inits.push(FieldInit { name: Name::of("calls"), expr: zero_a });
    b.field_inits.push(FieldInit { name: Name::of("writes"), expr: zero_b });
    let lit = b.e(Expr::Struct { path, fields: List { start, len: 3 }, base: blaze_typeck::StructBase::None });
    let u64_ty = b.types.intern(U64).expect("a type interns");

    let w1 = index_of(&mut b, wp, 1);
    let nine = b.int(9, None);
    let set = b.e(Expr::Assign { lhs: w1, rhs: nine });
    let w2 = index_of(&mut b, wp, 2);
    let five = b.int(5, None);
    let add = b.e(Expr::AssignOp { op: BinOp::Add, lhs: w2, rhs: five });
    let (r, s) = (b.bind("r"), b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(Mutbl::Mut), mutable: false }, name: Name::of("s"), sub: None, ty: None }));
    let w3 = index_of(&mut b, wp, 3);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: w3 });
    let r_e = b.local(r);
    let star_r = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r_e });
    let seven = b.int(7, None);
    let set_r = b.e(Expr::Assign { lhs: star_r, rhs: seven });
    let w0 = index_of(&mut b, wp, 0);
    let s_e = b.local(s);
    let star_s = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: s_e });
    let one = b.int(1, None);
    let set_s = b.e(Expr::Assign { lhs: star_s, rhs: one });
    let (a, bb) = (b.bind("a"), b.bind("b"));
    let (x1, x2) = (index_of(&mut b, wp, 1), index_of(&mut b, wp, 2));
    let a_init = b.bin(BinOp::Add, x1, x2);
    let (x3, x0) = (index_of(&mut b, wp, 3), index_of(&mut b, wp, 0));
    let b_init = b.bin(BinOp::Add, x3, x0);
    let calls = field_of(&mut b, wp, "calls");
    let k = b.int(100_000, None);
    let sum = b.bin(BinOp::Mul, calls, k);
    let writes = field_of(&mut b, wp, "writes");
    let k = b.int(10_000, None);
    let wk = b.bin(BinOp::Mul, writes, k);
    let sum = b.bin(BinOp::Add, sum, wk);
    let a_e = b.local(a);
    let k = b.int(100, None);
    let ak = b.bin(BinOp::Mul, a_e, k);
    let sum = b.bin(BinOp::Add, sum, ak);
    let b_e = b.local(bb);
    let sum = b.bin(BinOp::Add, sum, b_e);
    let root = b.block(
        &[
            Stmt::Let { pat: wp, ty: None, init: Some(lit), else_: None },
            Stmt::Expr { expr: set, semi: true },
            Stmt::Expr { expr: add, semi: true },
            Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None },
            Stmt::Expr { expr: set_r, semi: true },
            Stmt::Let { pat: s, ty: None, init: Some(w0), else_: None },
            Stmt::Expr { expr: set_s, semi: true },
            Stmt::Let { pat: a, ty: Some(u64_ty), init: Some(a_init), else_: None },
            Stmt::Let { pat: bb, ty: Some(u64_ty), init: Some(b_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert!(built.refs.iter().any(|i| i.item == INDEX_MUT_FN), "the build calls `IndexMut::index_mut`");
    assert!(built.refs.iter().any(|i| i.item == INDEX_FN), "and `Index::index` for the reads");
    #[cfg(target_arch = "aarch64")]
    {
        let host = [
            (ItemKey(5), host_bounds as usize as u64),
            (INDEX_FN, host_index as usize as u64),
            (INDEX_MUT_FN, host_index_mut as usize as u64),
        ];
        assert_eq!(run_native_with(&host, &[(MAIN, None, &built)]), 444408);
    }
    let _ = built;
}

/// `<Rows as Index<usize>>::index` on the host, for a `repr(C)` value of two two-word rows
/// (words 0..4), `calls` at word 4 and `writes` at word 5: counts the call, returns `&rows[i]`.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_row(this: *mut u64, i: u64) -> *const u64 {
    // SAFETY: the built code passes the address of a live value of that layout and `i < 2`.
    unsafe {
        *this.add(4) += 1;
        this.add(2 * i as usize)
    }
}

/// `<Rows as IndexMut<usize>>::index_mut` on the host: counts the call in `writes`, returns
/// `&mut rows[i]`.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_row_mut(this: *mut u64, i: u64) -> *mut u64 {
    // SAFETY: as `host_row`.
    unsafe {
        *this.add(5) += 1;
        this.add(2 * i as usize)
    }
}

/// `Cell::bump(&mut self)` on the host: `self.v += 100` (`v` the first word).
#[cfg(target_arch = "aarch64")]
extern "C" fn host_bump(cell: *mut u64) {
    // SAFETY: the built code passes `&mut` of a live `Cell` (`repr(C)`: `v` at 0).
    unsafe { *cell += 100 }
}

/// The struct literal `<name> { <fields> }`.
fn lit(b: &mut BodyBuf, name: &str, fields: &[(&str, ExprId)]) -> ExprId {
    let path = b.path(name);
    let start = b.field_inits.len() as u32;
    for &(f, expr) in fields {
        b.field_inits.push(FieldInit { name: Name::of(f), expr });
    }
    b.e(Expr::Struct { path, fields: List { start, len: fields.len() as u32 }, base: blaze_typeck::StructBase::None })
}

/// A world with `#[repr(C)] struct Rows { rows: [<row>; 2], calls: u64, writes: u64 }` (`row`
/// two words) and `impl Index<usize> for Rows { type Output = <row>; }` plus `IndexMut`, their
/// fns 12 and 13 (`host_row`, `host_row_mut`).
fn rows_world(w: &mut TestWorld, row: PNode) {
    w.func(1, "main", &[], U64);
    w.bounds_fn();
    let rows = w.array(row, 2);
    w.strukt(10, "Rows", &[("rows", rows), ("calls", U64), ("writes", U64)]);
    w.reprs.push((WIDGET, ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let rows_ty = PNode::Adt(WIDGET, List::EMPTY);
    let (r_rows, m_rows) = (w.ref_(Mutbl::Not, rows_ty), w.ref_(Mutbl::Mut, rows_ty));
    let (r_row, m_row) = (w.ref_(Mutbl::Not, row), w.ref_(Mutbl::Mut, row));
    w.func(12, "rows_index", &[r_rows, USIZE], r_row);
    w.func(13, "rows_index_mut", &[m_rows, USIZE], m_row);
    w.index_impl(rows_ty, USIZE, row, INDEX_FN);
    w.index_mut_impl(INDEX_MUT_FN);
}

/// `Rows { rows: <rows>, calls: 0, writes: 0 }` bound to `w`: the `let`.
fn let_rows(b: &mut BodyBuf, wp: PatId, rows: [ExprId; 2]) -> Stmt {
    let data = b.array(&rows);
    let (z1, z2) = (b.int(0, None), b.int(0, None));
    let init = lit(b, "Rows", &[("rows", data), ("calls", z1), ("writes", z2)]);
    Stmt::Let { pat: wp, ty: None, init: Some(init), else_: None }
}

/// `w.calls * 100_000 + w.writes * 10_000 + a`.
fn tally(b: &mut BodyBuf, wp: PatId, a: PatId) -> ExprId {
    let calls = field_of(b, wp, "calls");
    let k = b.int(100_000, None);
    let sum = b.bin(BinOp::Mul, calls, k);
    let writes = field_of(b, wp, "writes");
    let k = b.int(10_000, None);
    let wk = b.bin(BinOp::Mul, writes, k);
    let sum = b.bin(BinOp::Add, sum, wk);
    let a_e = b.local(a);
    b.bin(BinOp::Add, sum, a_e)
}

/// Runs `built` with the rows host fns (and `host_bump` as fn 15).
#[cfg(target_arch = "aarch64")]
fn run_rows(built: &Built<Global>) -> u64 {
    let host = [
        (ItemKey(5), host_bounds as usize as u64),
        (INDEX_FN, host_row as usize as u64),
        (INDEX_MUT_FN, host_row_mut as usize as u64),
        (ItemKey(15), host_bump as usize as u64),
    ];
    run_native_with(&host, &[(MAIN, None, built)])
}

/// Nested: the Reference's rule applies to the overloaded index under a builtin one, whose
/// place is assigned (`w[1][0] = 5`: `(*index_mut(&mut w, 1))[0] = 5`) or compound-assigned
/// (`w[0][1] += 30`, the place evaluated once):
/// ```text
/// impl Index<usize> for Rows { type Output = [u64; 2]; }  impl IndexMut<usize> for Rows {}
/// fn main() -> u64 {
///     let w = Rows { rows: [[1, 2], [3, 4]], calls: 0, writes: 0 };
///     w[1][0] = 5;                       // index_mut: writes 1
///     w[0][1] += 30;                     // index_mut once: writes 2, 32
///     let a: u64 = w[1][0] + w[0][1];    // index twice: calls 2, a = 37
///     w.calls * 100_000 + w.writes * 10_000 + a
/// }
/// ```
/// = 220037. Writes through `Index::index` would count calls 4 and writes 0 (400037); a write
/// to the wrong element leaves `a` other than 37.
#[test]
fn a_nested_index_through_index_mut_writes_its_element() {
    let mut w = TestWorld::new();
    let row = w.array(U64, 2);
    rows_world(&mut w, row);
    let mut b = BodyBuf::new();
    let wp = b.bind("w");
    let rows = [[1u128, 2], [3, 4]].map(|r| {
        let es = r.map(|v| b.int(v, None));
        b.array(&es)
    });
    let s0 = let_rows(&mut b, wp, rows);
    let at = |b: &mut BodyBuf, i: u128, j: u128| {
        let row = index_of(b, wp, i);
        let j = b.int(j, Some(IntSuffix::Uint(UintTy::Usize)));
        b.index(row, j)
    };
    let p10 = at(&mut b, 1, 0);
    let five = b.int(5, None);
    let set = b.e(Expr::Assign { lhs: p10, rhs: five });
    let p01 = at(&mut b, 0, 1);
    let thirty = b.int(30, None);
    let add = b.e(Expr::AssignOp { op: BinOp::Add, lhs: p01, rhs: thirty });
    let a = b.bind("a");
    let (x, y) = (at(&mut b, 1, 0), at(&mut b, 0, 1));
    let a_init = b.bin(BinOp::Add, x, y);
    let u64_ty = b.types.intern(U64).expect("a type interns");
    let tail = tally(&mut b, wp, a);
    let root = b.block(
        &[
            s0,
            Stmt::Expr { expr: set, semi: true },
            Stmt::Expr { expr: add, semi: true },
            Stmt::Let { pat: a, ty: Some(u64_ty), init: Some(a_init), else_: None },
        ],
        Some(tail),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert!(built.refs.iter().any(|i| i.item == INDEX_MUT_FN), "the build calls `IndexMut::index_mut`");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_rows(&built), 220037);
    let _ = built;
}

/// A field of the element (`w[1].v = 7`) and a `&mut self` method on it (`w[0].bump()`, its
/// receiver's `&mut` autoref the mutable context) go through `IndexMut::index_mut`:
/// ```text
/// #[repr(C)] struct Cell { v: u64, pad: u64 }  impl Cell { fn bump(&mut self) }  // v += 100
/// impl Index<usize> for Rows { type Output = Cell; }  impl IndexMut<usize> for Rows {}
/// fn main() -> u64 {
///     let w = Rows { rows: [Cell { v: 1, pad: 0 }, Cell { v: 2, pad: 0 }], calls: 0, writes: 0 };
///     w[1].v = 7;                        // index_mut: writes 1
///     w[0].bump();                       // index_mut: writes 2, v = 101
///     let a: u64 = w[0].v + w[1].v;      // index twice: calls 2, a = 108
///     w.calls * 100_000 + w.writes * 10_000 + a
/// }
/// ```
/// = 220108. A `bump` on a copy of the element leaves `a` 8; a field write through `index`
/// is refused.
#[test]
fn a_field_and_a_mut_self_method_of_an_element_go_through_index_mut() {
    const CELL: ItemKey = ItemKey(16);
    let mut w = TestWorld::new();
    w.strukt(16, "Cell", &[("v", U64), ("pad", U64)]);
    w.reprs.push((CELL, ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let cell = PNode::Adt(CELL, List::EMPTY);
    rows_world(&mut w, cell);
    let m_cell = w.ref_(Mutbl::Mut, cell);
    w.func(15, "bump", &[m_cell], PNode::Tuple(List::EMPTY));
    w.method(CELL, "bump", ItemKey(14), ItemKey(15));
    let mut b = BodyBuf::new();
    let wp = b.bind("w");
    let cells = [1u128, 2].map(|v| {
        let (v, pad) = (b.int(v, None), b.int(0, None));
        lit(&mut b, "Cell", &[("v", v), ("pad", pad)])
    });
    let s0 = let_rows(&mut b, wp, cells);
    let w1 = index_of(&mut b, wp, 1);
    let f = b.field(w1, "v");
    let seven = b.int(7, None);
    let set = b.e(Expr::Assign { lhs: f, rhs: seven });
    let w0 = index_of(&mut b, wp, 0);
    let args = b.exprs_list(&[]);
    let bump = b.e(Expr::MethodCall { receiver: w0, name: Name::of("bump"), generic_args: List::EMPTY, args, scope: ScopeId(0) });
    let a = b.bind("a");
    let (x, y) = (index_of(&mut b, wp, 0), index_of(&mut b, wp, 1));
    let (xv, yv) = (b.field(x, "v"), b.field(y, "v"));
    let a_init = b.bin(BinOp::Add, xv, yv);
    let u64_ty = b.types.intern(U64).expect("a type interns");
    let tail = tally(&mut b, wp, a);
    let root = b.block(
        &[
            s0,
            Stmt::Expr { expr: set, semi: true },
            Stmt::Expr { expr: bump, semi: true },
            Stmt::Let { pat: a, ty: Some(u64_ty), init: Some(a_init), else_: None },
        ],
        Some(tail),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert!(built.refs.iter().any(|i| i.item == INDEX_MUT_FN), "the build calls `IndexMut::index_mut`");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_rows(&built), 220108);
    let _ = built;
}
