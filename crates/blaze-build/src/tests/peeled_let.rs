//! A `let` whose struct or tuple pattern peels the reference it is matched against (walk.rs
//! `bind_value_of`, walk/matches/irrefutable.rs `bind_irrefutable_at`): the check records the
//! pattern at the peeled type, the value is the reference.

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

/// The Reference, "Binding modes": "Each time a reference is matched using a non-reference
/// pattern, it will automatically dereference the value and update the default binding mode.
/// References will set the default binding mode to `ref`". blaze-core's fsevents.rs
/// `end_on_queue` (`let Ending { fw, stream } = unsafe { &*ending.cast::<Ending>() };`) and
/// bstd's `Io::write_out` (`let World { driver, hosted, .. } = &mut *world;`) are this form.
/// ```text
/// struct S { a: u32, b: u32 }
/// fn main() -> u32 {
///     let s = S { a: 3, b: 4 };
///     let S { a, b } = &s;            // a = &s.a, b = &s.b
///     let pair = (5u32, 6u32);
///     let (p, q) = &pair;             // p = &pair.0, q = &pair.1
///     *a * 1000 + *b * 100 + *p * 10 + *q
/// }
/// ```
/// Expected: returns 3456; `cargo test` prints
/// `test tests::peeled_let::let_struct_and_tuple_patterns_over_a_reference ... ok`. Without
/// `bind_value_of` the build fails with "a malformed body or record: a scalar where a place was
/// expected" (the reference stored as the peeled struct); a pattern bound at the struct's own
/// place instead of through the reference reads the pointer's bits as `a`.
#[test]
fn let_struct_and_tuple_patterns_over_a_reference() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.strukt(9, "S", &[("a", U32), ("b", U32)]);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let s = S { a: 3, b: 4 };
    let s = b.bind("s");
    let s_path = b.path("S");
    let three = u32_lit(&mut b, 3);
    let four = u32_lit(&mut b, 4);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: three });
    b.field_inits.push(FieldInit { name: Name::of("b"), expr: four });
    let s_init = b.e(Expr::Struct { path: s_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    stmts.push(let_stmt(s, s_init));

    // let S { a, b } = &s;
    let a = plain(&mut b, "a");
    let bb = plain(&mut b, "b");
    let fstart = b.field_pats.len() as u32;
    b.field_pats.push(FieldPat { name: Name::of("a"), pat: a });
    b.field_pats.push(FieldPat { name: Name::of("b"), pat: bb });
    let sp_path = b.path("S");
    let s_pat = b.pat(Pat::Struct { path: sp_path, fields: List { start: fstart, len: 2 }, rest: false });
    let s_use = b.local(s);
    let s_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_use });
    stmts.push(let_stmt(s_pat, s_ref));

    // let pair = (5u32, 6u32); let (p, q) = &pair;
    let pair = b.bind("pair");
    let five = u32_lit(&mut b, 5);
    let six = u32_lit(&mut b, 6);
    let pair_init = b.tuple(&[five, six]);
    stmts.push(let_stmt(pair, pair_init));
    let p = plain(&mut b, "p");
    let q = plain(&mut b, "q");
    let pq = b.pats_list(&[p, q]);
    let pq_tuple = b.pat(Pat::Tuple { elems: pq, rest: None });
    let pair_use = b.local(pair);
    let pair_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: pair_use });
    stmts.push(let_stmt(pq_tuple, pair_ref));

    // *a * 1000 + *b * 100 + *p * 10 + *q
    let q_use = b.local(q);
    let mut sum = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: q_use });
    for (binding, scale) in [(a, 1000), (bb, 100), (p, 10)] {
        let v = b.local(binding);
        let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: v });
        let k = u32_lit(&mut b, scale);
        let term = b.bin(BinOp::Mul, star, k);
        sum = b.bin(BinOp::Add, sum, term);
    }
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(MAIN, &built)]), 3456);
}
