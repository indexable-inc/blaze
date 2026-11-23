//! Irrefutable `let` patterns beyond a binding, `_`, a tuple and an array without `..`
//! (walk/matches/irrefutable.rs, reached from walk.rs `bind_value`): a struct pattern, the
//! only variant of an enum, a reference pattern and an array pattern with `..`; built from a
//! body the checker types, run natively, the result compared.

use super::*;

/// A binding with no annotation.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn let_stmt(pat: PatId, init: ExprId) -> Stmt {
    Stmt::Let { pat, ty: None, init: Some(init), else_: None }
}

fn u32_lit(b: &mut BodyBuf, value: u128) -> ExprId {
    b.int(value, Some(IntSuffix::Uint(UintTy::U32)))
}

/// The Reference, "Patterns", "Refutability": "A pattern is said to be irrefutable when it
/// always matches the value it is being matched against"; "Struct patterns" match the named
/// fields' subpatterns; a tuple-struct pattern of an enum's only variant always matches;
/// "Reference patterns dereference the pointers that are being matched"; "Slice patterns"
/// over `[T; N]` with `..` take the prefix and the suffix at fixed places. Each binding's value
/// is a different digit of the answer, so a wrong field offset, a wrong variant row, a missed
/// dereference or a suffix taken from the front gives another number (`t` = 8 or 7 if the
/// suffix were read from the front). The library's own use (alloc VecDeque `slice_ranges`):
/// `let Range { start, end } = slice::range(range, ..len);`.
/// ```text
/// struct S { a: u32, b: (u32, u32) }
/// enum E { W(u32) }
/// fn main() -> u32 {
///     let s = S { a: 1, b: (2, 3) };
///     let S { a, b: (c, _) } = s;          // a = 1, c = 2
///     let w = W(4u32);
///     let W(x) = w;                         // 4
///     let pair = (5u32, 6u32);
///     let r = &pair;
///     let &(p, q) = r;                      // 5, 6
///     let arr = [7u32, 8, 9];
///     let [h, .., t] = arr;                 // 7, 9
///     a * 1_000_000 + c * 100_000 + x * 10_000 + p * 1_000 + q * 100 + h * 10 + t
/// }
/// ```
/// Expected: returns 1_245_679; `cargo test` prints
/// `test tests::irrefutable::let_takes_struct_variant_reference_and_rest_patterns_apart ... ok`.
/// Without the fix the build fails with "pattern N: a pattern other than a binding, `_`, a
/// tuple or an array pattern is not built to LIR yet".
#[test]
fn let_takes_struct_variant_reference_and_rest_patterns_apart() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let pair_ty = w.tuple(&[U32, U32]);
    w.strukt(9, "S", &[("a", U32), ("b", pair_ty)]);
    w.enum_(50, &[("W", CtorKind::Tuple, &[("0", U32)])]);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let s = S { a: 1, b: (2, 3) };
    let s = b.bind("s");
    let s_path = b.path("S");
    let one_ = u32_lit(&mut b, 1);
    let two = u32_lit(&mut b, 2);
    let three = u32_lit(&mut b, 3);
    let b_init = b.tuple(&[two, three]);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: one_ });
    b.field_inits.push(FieldInit { name: Name::of("b"), expr: b_init });
    let s_init = b.e(Expr::Struct { path: s_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    stmts.push(let_stmt(s, s_init));

    // let S { a, b: (c, _) } = s;
    let a = plain(&mut b, "a");
    let c = plain(&mut b, "c");
    let skip = b.pat(Pat::Wild);
    let cs = b.pats_list(&[c, skip]);
    let c_tuple = b.pat(Pat::Tuple { elems: cs, rest: None });
    let fstart = b.field_pats.len() as u32;
    b.field_pats.push(FieldPat { name: Name::of("a"), pat: a });
    b.field_pats.push(FieldPat { name: Name::of("b"), pat: c_tuple });
    let sp_path = b.path("S");
    let s_pat = b.pat(Pat::Struct { path: sp_path, fields: List { start: fstart, len: 2 }, rest: false });
    let s_use = b.local(s);
    stmts.push(let_stmt(s_pat, s_use));

    // let w = W(4u32); let W(x) = w;
    let wv = b.bind("w");
    let four = u32_lit(&mut b, 4);
    let w_init = b.call("W", &[four]);
    stmts.push(let_stmt(wv, w_init));
    let x = plain(&mut b, "x");
    let w_path = b.path("W");
    let xs = b.pats_list(&[x]);
    let w_pat = b.pat(Pat::TupleStruct { path: w_path, elems: xs, rest: None });
    let w_use = b.local(wv);
    stmts.push(let_stmt(w_pat, w_use));

    // let pair = (5u32, 6u32); let r = &pair; let &(p, q) = r;
    let pair = b.bind("pair");
    let five = u32_lit(&mut b, 5);
    let six = u32_lit(&mut b, 6);
    let pair_init = b.tuple(&[five, six]);
    stmts.push(let_stmt(pair, pair_init));
    let r = b.bind("r");
    let pair_use = b.local(pair);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: pair_use });
    stmts.push(let_stmt(r, r_init));
    let p = plain(&mut b, "p");
    let q = plain(&mut b, "q");
    let pq = b.pats_list(&[p, q]);
    let pq_tuple = b.pat(Pat::Tuple { elems: pq, rest: None });
    let r_pat = b.pat(Pat::Ref { mutbl: Mutbl::Not, inner: pq_tuple });
    let r_use = b.local(r);
    stmts.push(let_stmt(r_pat, r_use));

    // let arr = [7u32, 8, 9]; let [h, .., t] = arr;
    let arr = b.bind("arr");
    let seven = u32_lit(&mut b, 7);
    let eight = u32_lit(&mut b, 8);
    let nine = u32_lit(&mut b, 9);
    let arr_init = b.array(&[seven, eight, nine]);
    stmts.push(let_stmt(arr, arr_init));
    let h = plain(&mut b, "h");
    let dots = b.pat(Pat::Wild);
    let t = plain(&mut b, "t");
    let hdt = b.pats_list(&[h, dots, t]);
    let arr_pat = b.pat(Pat::Slice { elems: hdt, rest: Some(1) });
    let arr_use = b.local(arr);
    stmts.push(let_stmt(arr_pat, arr_use));

    // a * 1_000_000 + c * 100_000 + x * 10_000 + p * 1_000 + q * 100 + h * 10 + t
    let mut sum = b.local(t);
    for (binding, scale) in [(a, 1_000_000), (c, 100_000), (x, 10_000), (p, 1_000), (q, 100), (h, 10)] {
        let v = b.local(binding);
        let k = u32_lit(&mut b, scale);
        let term = b.bin(BinOp::Mul, v, k);
        sum = b.bin(BinOp::Add, sum, term);
    }
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(MAIN, &built)]), 1_245_679);
}

/// `name(v)` bound by `let name(x) = <its value>;`: the binding.
fn let_variant(b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, v: u128) -> PatId {
    let local = b.bind("r");
    let lit = u32_lit(b, v);
    let init = b.call(name, &[lit]);
    stmts.push(let_stmt(local, init));
    let x = plain(b, "x");
    let path = b.path(name);
    let xs = b.pats_list(&[x]);
    let pat = b.pat(Pat::TupleStruct { path, elems: xs, rest: None });
    let use_ = b.local(local);
    stmts.push(let_stmt(pat, use_));
    x
}

/// The Reference, "Patterns", "Refutability", with empty types: a variant pattern is
/// irrefutable when every other variant of its enum is uninhabited, so `let Ok(x) = r;` over
/// `Result<T, Infallible>` is plain Rust (hybrid-array's `Array::from_fn`: `let Ok(ret) =
/// Self::try_from_fn::<Infallible>(..);`, 58 refusals in tempo's census of 04:02). Three
/// shapes of the other variants: one holding the empty enum alone (a variant the layout drops,
/// the enum laid out as its one present variant), one holding a `u32` beside it (present, with
/// bytes, still no value), one holding a tuple holding it (empty through a tuple).
/// ```text
/// enum Never {}
/// enum R { O(u32), E(Never) }
/// enum S { Q(u32), F(u32, Never), T((u32, Never)) }
/// fn main() -> u32 {
///     let r = O(7); let O(x) = r;          // 7
///     let s = Q(8); let Q(y) = s;          // 8
///     x * 10 + y
/// }
/// ```
/// Expected: 78. The control: `enum H { Y(u32), Z(u32) }`, `let Y(z) = Y(5);` stays refused
/// by name (refutable: its `Z` has values). `cargo test -p blaze-build irrefutable` prints
/// `test tests::irrefutable::let_takes_a_variant_whose_other_variants_are_uninhabited ... ok`.
/// Before: the first `let` refused with "pattern N: a variant pattern of an enum of more than
/// one variant in a `let` or parameter pattern (refutable: a `let` needs `else`)".
#[test]
fn let_takes_a_variant_whose_other_variants_are_uninhabited() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.enum_(200, &[]);
    let never = PNode::Adt(ItemKey(200), List::EMPTY);
    let with_never = w.tuple(&[U32, never]);
    w.enum_(210, &[("O", CtorKind::Tuple, &[("0", U32)]), ("E", CtorKind::Tuple, &[("0", never)])]);
    w.enum_(
        220,
        &[
            ("Q", CtorKind::Tuple, &[("0", U32)]),
            ("F", CtorKind::Tuple, &[("0", U32), ("1", never)]),
            ("T", CtorKind::Tuple, &[("0", with_never)]),
        ],
    );
    w.enum_(230, &[("Y", CtorKind::Tuple, &[("0", U32)]), ("Z", CtorKind::Tuple, &[("0", U32)])]);

    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let x = let_variant(&mut b, &mut stmts, "O", 7);
    let y = let_variant(&mut b, &mut stmts, "Q", 8);
    let xv = b.local(x);
    let ten = u32_lit(&mut b, 10);
    let tens = b.bin(BinOp::Mul, xv, ten);
    let yv = b.local(y);
    let sum = b.bin(BinOp::Add, tens, yv);
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(MAIN, &built)]), 78);

    // The control: an enum whose other variant has values.
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let z = let_variant(&mut b, &mut stmts, "Y", 5);
    let zv = b.local(z);
    let root = b.block(&stmts, Some(zv));
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("a variant pattern of an enum of more than one variant"), "the refutable `let` was not refused: {refused:?}");
}
