//! Expression shapes tempo's build census found refused at the builder (lane EX, 2026-11-17),
//! each built from a body the checker types and run natively:
//! - a compound assignment to a float (core's `impl DivAssign for f32`: `*self /= other`),
//! - a slice indexed through an explicit dereference (core's `SliceIndex<[T]> for usize`:
//!   `&mut (*slice)[self]`),
//! - an overloaded dereference whose `Self` is unsized (winnow's `struct Bytes([u8])` with
//!   `impl Deref for Bytes { type Target = [u8]; }`),
//! - a volatile store and load of an aggregate (core's `ptr::write_volatile` at a struct),
//! - a match guard with `let` links (core's `dec2flt`: `None if let Some(v) = .. =>`).

use super::*;

const F64: PNode = PNode::Float(blaze_typeck::FloatTy::F64);

fn f64_lit(b: &mut BodyBuf, x: f64) -> ExprId {
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F64))));
    b.lits.push((e, u128::from(x.to_bits())));
    e
}

fn deref(b: &mut BodyBuf, p: PatId) -> ExprId {
    let e = b.local(p);
    let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e });
    b.e(Expr::Paren(star))
}

/// The Reference, "Compound assignment expressions": "If both types are primitives, then the
/// modifying operand will be evaluated first followed by the assigned operand. It will then
/// set the value of the assigned operand's place to the value of performing the operation of
/// the operator with the values of the assigned operand and modifying operand." For a float
/// the operation is its IEEE one ("Arithmetic and Logical Binary Operators").
/// ```text
/// fn main() -> u64 {
///     let mut x = 7.5f64;
///     let r = &mut x;
///     *r /= 2.0;                                 // 3.75, through a place behind a reference
///     x *= 4.0;                                  // 15
///     x -= 1.0;                                  // 14
///     x += 0.5;                                  // 14.5
///     (x == 14.5) as u64 * 100 + x as u64        // 114
/// }
/// ```
/// A build that refuses a float's compound assignment (the census's "arithmetic on this type")
/// or does it as integer arithmetic on the bits fails it.
#[test]
fn a_compound_assignment_to_a_float_is_its_ieee_operation() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], PNode::Uint(UintTy::U64));
    let mut b = BodyBuf::new();
    let u64_ty = b.types.intern(PNode::Uint(UintTy::U64)).expect("a type interns");
    let f64_ty = b.types.intern(F64).expect("a type interns");
    let (x, r) = (b.bind("x"), b.bind("r"));
    let x_init = f64_lit(&mut b, 7.5);
    let x_e = b.local(x);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: x_e });
    let star_r = {
        let e = b.local(r);
        b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e })
    };
    let two = f64_lit(&mut b, 2.0);
    let div = b.e(Expr::AssignOp { op: BinOp::Div, lhs: star_r, rhs: two });
    let mut ops = vec![Stmt::Let { pat: x, ty: Some(f64_ty), init: Some(x_init), else_: None }];
    ops.push(Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None });
    ops.push(Stmt::Expr { expr: div, semi: true });
    for (op, k) in [(BinOp::Mul, 4.0), (BinOp::Sub, 1.0), (BinOp::Add, 0.5)] {
        let lhs = b.local(x);
        let rhs = f64_lit(&mut b, k);
        let e = b.e(Expr::AssignOp { op, lhs, rhs });
        ops.push(Stmt::Expr { expr: e, semi: true });
    }
    let x_e = b.local(x);
    let want = f64_lit(&mut b, 14.5);
    let eq = b.bin(BinOp::Eq, x_e, want);
    let eq = b.e(Expr::Cast { operand: eq, ty: u64_ty });
    let hundred = b.int(100, None);
    let hi = b.bin(BinOp::Mul, eq, hundred);
    let x_e = b.local(x);
    let lo = b.e(Expr::Cast { operand: x_e, ty: u64_ty });
    let sum = b.bin(BinOp::Add, hi, lo);
    let root = b.block(&ops, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    for op in ["fdiv", "fmul", "fsub", "fadd"] {
        assert!(text.contains(op), "no `{op}` in the built function:\n{text}");
    }
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 114);
    let _ = built;
}

/// The Reference, "Array and slice indexing expressions": "For other types an index expression
/// `a[b]` is equivalent to `*std::ops::Index::index(&a, b)`", but a slice `[T]` is indexed
/// builtin, and `(*s)[i]` with `s: &[T]` (or `&mut [T]`) is the element of the place `*s`, the
/// slice behind the wide pointer, its length the pointer's (core's `SliceIndex<[T]> for usize`
/// writes exactly `&mut (*slice)[self]`).
/// ```text
/// fn main() -> usize {
///     let mut a = [10u8, 20, 30];
///     let m: &mut [u8] = &mut a;
///     (*m)[2] = 5;
///     let s: &[u8] = &a;
///     (*s)[1] as usize * 100 + (*s)[2] as usize    // 2005
/// }
/// ```
/// A build that refuses an index whose base is a written-out dereference (the census's
/// "indexing a slice not reached by a dereference of a wide pointer") fails it; one that
/// indexed the pointer's own memory reads the address's bytes, not 20 and 5.
#[test]
fn a_slice_is_indexed_through_an_explicit_dereference() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let mut_slice = b.types.intern(PNode::Ref(Mutbl::Mut, slice)).expect("a type interns");
    let su8 = Some(IntSuffix::Uint(UintTy::U8));
    let (a, m, s) = (b.bind("a"), b.bind("m"), b.bind("s"));
    let (e10, e20, e30) = (b.int(10, su8), b.int(20, su8), b.int(30, su8));
    let arr = b.array(&[e10, e20, e30]);
    let a_e = b.local(a);
    let m_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: a_e });
    let star_m = deref(&mut b, m);
    let two = b.int(2, None);
    let at = b.index(star_m, two);
    let five = b.int(5, None);
    let store = b.e(Expr::Assign { lhs: at, rhs: five });
    let a_e = b.local(a);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let elem = |b: &mut BodyBuf, i: u128| {
        let base = deref(b, s);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        b.e(Expr::Cast { operand: at, ty: usize_ty })
    };
    let one = elem(&mut b, 1);
    let hundred = b.int(100, None);
    let hi = b.bin(BinOp::Mul, one, hundred);
    let lo = elem(&mut b, 2);
    let sum = b.bin(BinOp::Add, hi, lo);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Let { pat: m, ty: Some(mut_slice), init: Some(m_init), else_: None },
            Stmt::Expr { expr: store, semi: true },
            Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(MAIN, None, &built)]), 2005);
    }
    let _ = built;
}

/// The Reference, "The dereference operator": "On non-pointer types `*x` is equivalent to
/// `*std::ops::Deref::deref(&x)`", and `Deref`'s `Self` may be unsized (`trait Deref` has no
/// `Sized` bound on `Self`): then `&x` is a wide pointer ("Dynamically Sized Types": a struct
/// whose last field is a slice is itself a DST), passed to `deref` whole.
/// ```text
/// struct Bytes { b: [u8] }
/// impl Deref for Bytes { type Target = [u8]; fn deref(&self) -> &[u8] { transmute(self) } }
/// fn main() -> usize {
///     let a = [1u8, 2, 3, 4];
///     let s: &[u8] = &a;
///     let by: &Bytes = transmute(s);
///     by[2] as usize * 10 + by[0] as usize                     // 31
/// }
/// ```
/// Each `by[i]` autoderefs `&Bytes` builtin, then `Bytes` through `Deref::deref` with the wide
/// `&Bytes`; the slice it returns is indexed with its own length (4: a zero length would
/// panic at the bounds check). A build that refuses the
/// unsized `Self` (the census's "an overloaded dereference of an unsized `Self`"), or passes
/// only the address, fails it.
#[test]
fn an_overloaded_dereference_of_an_unsized_self_passes_the_wide_pointer() {
    const BYTES: ItemKey = ItemKey(40);
    const BYTES_DEREF: ItemKey = ItemKey(12);
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let u8_w = w.ty(U8);
    w.strukt(BYTES.0, "Bytes", &[("b", PNode::Slice(u8_w))]);
    let bytes = PNode::Adt(BYTES, List::EMPTY);
    let r_bytes = w.ref_(Mutbl::Not, bytes);
    let r_slice = w.ref_(Mutbl::Not, PNode::Slice(u8_w));
    w.func(BYTES_DEREF.0, "bytes_deref", &[r_bytes], r_slice);
    w.deref_impl(bytes, PNode::Slice(u8_w), BYTES_DEREF);

    // deref: `transmute(self)`, `&Bytes` to `&[u8]`, the same address and length.
    let mut c = BodyBuf::new();
    let p = c.bind("w");
    c.params.push(p);
    let p_e = c.local(p);
    let body = c.call("transmute", &[p_e]);
    let croot = c.block(&[], Some(body));
    let mut g = PortableTys::new_in(Alloc::global());
    let deref_built = build_instance(&w, &c, BYTES_DEREF, croot, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));

    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let bytes_b = b.types.intern(bytes).expect("a type interns");
    let ref_bytes = b.types.intern(PNode::Ref(Mutbl::Not, bytes_b)).expect("a type interns");
    let su8 = Some(IntSuffix::Uint(UintTy::U8));
    let (a, s, by) = (b.bind("a"), b.bind("s"), b.bind("by"));
    let elems = [b.int(1, su8), b.int(2, su8), b.int(3, su8), b.int(4, su8)];
    let arr = b.array(&elems);
    let a_e = b.local(a);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let s_e = b.local(s);
    let by_init = b.call("transmute", &[s_e]);
    let elem = |b: &mut BodyBuf, i: u128| {
        let base = b.local(by);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        b.e(Expr::Cast { operand: at, ty: usize_ty })
    };
    let third = elem(&mut b, 2);
    let ten = b.int(10, None);
    let hi = b.bin(BinOp::Mul, third, ten);
    let first = elem(&mut b, 0);
    let sum = b.bin(BinOp::Add, hi, first);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None },
            Stmt::Let { pat: by, ty: Some(ref_bytes), init: Some(by_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert!(built.refs.iter().any(|r| r.item == BYTES_DEREF), "`deref` is called: {:?}", built.refs);
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(BYTES_DEREF, None, &deref_built), (MAIN, None, &built)]), 31);
    }
    let _ = (built, deref_built);
}

/// core's intrinsics (library/core/src/intrinsics/mod.rs): `volatile_store<T>(dst: *mut T,
/// val: T)` "Performs a volatile store to the `dst` pointer" and `volatile_load<T>(src: *const
/// T) -> T` "Performs a volatile load from the `src` pointer", for any `T`, an aggregate too
/// (core's `ptr::write_volatile`, called with a struct by tempo's dependencies).
/// ```text
/// fn main() -> u32 {
///     let a = [1u32, 2, 3];
///     let v = [7u32, 8, 9];
///     let p: *mut [u32; 3] = &mut a;
///     volatile_store(p, v);
///     let q: *const [u32; 3] = &a;
///     let r = volatile_load(q);
///     r[0] + r[1] * 10 + r[2] * 100 + a[0] * 1000      // 7987
/// }
/// ```
/// The built function holds a `store volatile` per 4-byte piece of the array; a build that
/// refuses the aggregate (the census's "`volatile_store` of an aggregate") fails it, and one
/// that stored only the first word reads 2 and 3 back.
#[test]
fn a_volatile_store_and_load_of_an_aggregate_move_every_byte() {
    const VOLATILE_STORE: u128 = 7;
    const VOLATILE_LOAD: u128 = 8;
    let mut w = TestWorld::new();
    w.func(1, "main", &[], PNode::Uint(UintTy::U32));
    w.bounds_fn();
    let t = w.ty(PNode::Param(0));
    let unit = PNode::Tuple(List::EMPTY);
    w.generic_func(VOLATILE_STORE, "volatile_store", 1, &[PNode::Ptr(Mutbl::Mut, t), PNode::Param(0)], unit);
    w.generic_func(VOLATILE_LOAD, "volatile_load", 1, &[PNode::Ptr(Mutbl::Not, t)], PNode::Param(0));
    w.intrinsics.push((ItemKey(VOLATILE_STORE), Intrinsic::Table(crate::intrinsics::Which::VolatileStore)));
    w.intrinsics.push((ItemKey(VOLATILE_LOAD), Intrinsic::Table(crate::intrinsics::Which::VolatileLoad)));
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(PNode::Uint(UintTy::U32)).expect("a type interns");
    let three = b.types.usize_value(3).expect("a length interns");
    let arr_ty = b.types.intern(PNode::Array(u32_ty, three)).expect("a type interns");
    let mut_p = b.types.intern(PNode::Ptr(Mutbl::Mut, arr_ty)).expect("a type interns");
    let const_p = b.types.intern(PNode::Ptr(Mutbl::Not, arr_ty)).expect("a type interns");
    let su32 = Some(IntSuffix::Uint(UintTy::U32));
    let (a, v, p, q, r) = (b.bind("a"), b.bind("v"), b.bind("p"), b.bind("q"), b.bind("r"));
    let a_elems = [b.int(1, su32), b.int(2, su32), b.int(3, su32)];
    let a_init = b.array(&a_elems);
    let v_elems = [b.int(7, su32), b.int(8, su32), b.int(9, su32)];
    let v_init = b.array(&v_elems);
    let a_e = b.local(a);
    let p_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: a_e });
    let (p_e, v_e) = (b.local(p), b.local(v));
    let store = b.call("volatile_store", &[p_e, v_e]);
    let a_e = b.local(a);
    let q_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let q_e = b.local(q);
    let r_init = b.call("volatile_load", &[q_e]);
    let term = |b: &mut BodyBuf, of: PatId, i: u128, scale: u128| {
        let base = b.local(of);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        let k = b.int(scale, None);
        b.bin(BinOp::Mul, at, k)
    };
    let terms = [term(&mut b, r, 0, 1), term(&mut b, r, 1, 10), term(&mut b, r, 2, 100), term(&mut b, a, 0, 1000)];
    let mut sum = terms[0];
    for &t in &terms[1..] {
        sum = b.bin(BinOp::Add, sum, t);
    }
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: v, ty: None, init: Some(v_init), else_: None },
            Stmt::Let { pat: p, ty: Some(mut_p), init: Some(p_init), else_: None },
            Stmt::Expr { expr: store, semi: true },
            Stmt::Let { pat: q, ty: Some(const_p), init: Some(q_init), else_: None },
            Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.matches("volatile").count() >= 6, "three volatile stores and three volatile loads:\n{text}");
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(MAIN, None, &built)]), 7987, "{text}");
    }
    let _ = built;
}

/// A binding with no annotation, immutable: the default binding mode applies.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn tuple_pat(b: &mut BodyBuf, elems: &[PatId]) -> PatId {
    let elems = b.pats_list(elems);
    b.pat(Pat::Tuple { elems, rest: None })
}

/// `match 0u32 { 0 if let (a, 7) = (y, y + 4) => a * 10, 0 => 1, n => n }`: a scalar
/// scrutinee, its arms by a switch.
fn switch_guarded(b: &mut BodyBuf, y: u128) -> ExprId {
    let a = plain(b, "a");
    let seven = b.pat_int(7);
    let pat = tuple_pat(b, &[a, seven]);
    let (y1, y2, four) = (b.int(y, SU32), b.int(y, SU32), b.int(4, None));
    let y4 = b.bin(BinOp::Add, y2, four);
    let init = b.tuple(&[y1, y4]);
    let guard = b.e(Expr::Let { pat, init });
    let (a_e, ten) = (b.local(a), b.int(10, None));
    let taken = b.bin(BinOp::Mul, a_e, ten);
    let (zero1, zero2, n) = (b.pat_int(0), b.pat_int(0), plain(b, "n"));
    let (one_, n_e) = (b.int(1, None), b.local(n));
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: zero1, guard: Some(guard), body: taken });
    b.arms.push(Arm { pat: zero2, guard: None, body: one_ });
    b.arms.push(Arm { pat: n, guard: None, body: n_e });
    let scrutinee = b.int(0, SU32);
    b.e(Expr::Match { scrutinee, arms: List { start, len: 3 } })
}

/// `match (0u32, y) { (0, c) if let (d, _) = (c, 0u32) && d > 2 && let 9 = d + 6 => 100 + d,
/// _ => 2 }`: a tuple scrutinee, its arms by tests; the guard a chain of two `let`s and a
/// `bool`, `d` bound by the first link for the later ones and the body.
fn tested_chain(b: &mut BodyBuf, y: u128) -> ExprId {
    let (zero, c) = (b.pat_int(0), plain(b, "c"));
    let pat = tuple_pat(b, &[zero, c]);
    let (d, w) = (plain(b, "d"), b.pat(Pat::Wild));
    let first_pat = tuple_pat(b, &[d, w]);
    let (c_e, z) = (b.local(c), b.int(0, SU32));
    let first_init = b.tuple(&[c_e, z]);
    let first = b.e(Expr::Let { pat: first_pat, init: first_init });
    let (d_e, two) = (b.local(d), b.int(2, None));
    let big = b.bin(BinOp::Gt, d_e, two);
    let nine = b.pat_int(9);
    let (d_e, six) = (b.local(d), b.int(6, None));
    let d6 = b.bin(BinOp::Add, d_e, six);
    let third = b.e(Expr::Let { pat: nine, init: d6 });
    let chain = b.bin(BinOp::And, first, big);
    let chain = b.bin(BinOp::And, chain, third);
    let (hundred, d_e) = (b.int(100, None), b.local(d));
    let taken = b.bin(BinOp::Add, hundred, d_e);
    let (wild, two_) = (b.pat(Pat::Wild), b.int(2, None));
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat, guard: Some(chain), body: taken });
    b.arms.push(Arm { pat: wild, guard: None, body: two_ });
    let (x, y_e) = (b.int(0, SU32), b.int(y, SU32));
    let scrutinee = b.tuple(&[x, y_e]);
    b.e(Expr::Match { scrutinee, arms: List { start, len: 2 } })
}

/// The Reference, "Match expressions", "Match guards" ([expr.match.guard.behavior]): "If all
/// of the guard condition operands evaluate to true and all of the `let` patterns successfully
/// match their scrutinees, the match arm is successfully matched against and the arm body is
/// executed"; otherwise "the next pattern ... is tested"; and [expr.match.guard.chains]: a
/// guard "may contain ... `let` patterns", whose bindings are in scope in the arm's body. core
/// writes one in `dec2flt`: `None if let Some(value) = parse_inf_nan(s, negative) => return
/// Ok(value),`.
/// ```text
/// fn main() -> u32 {
///     switch_guarded(3)              // (3, 7) matches: 30
///     + switch_guarded(4) * 100      // (4, 8) fails: the next arm `0`, 1
///     + tested_chain(3) * 1000       // d = 3, 3 > 2, 9 = 9: 103
///     + tested_chain(1) * 1_000_000  // d = 1, 1 > 2 fails: the next arm, 2
/// }                                  // 2_103_130
/// ```
/// A build that refuses the `let` (the census's "`let` outside an `if` or `while` condition")
/// fails it; one whose failed link went anywhere but the next arm gives other numbers.
#[test]
fn a_match_guard_with_let_links_binds_for_the_body_and_falls_to_the_next_arm() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let parts = [
        (switch_guarded(&mut b, 3), 1u128),
        (switch_guarded(&mut b, 4), 100),
        (tested_chain(&mut b, 3), 1000),
        (tested_chain(&mut b, 1), 1_000_000),
    ];
    let mut acc = None;
    for (part, scale) in parts {
        let k = b.int(scale, None);
        let scaled = b.bin(BinOp::Mul, part, k);
        acc = Some(match acc {
            Some(prev) => b.bin(BinOp::Add, prev, scaled),
            None => scaled,
        });
    }
    let root = b.block(&[], acc);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 2_103_130);
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 2_103_130);
}
