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

fn ref_bind(b: &mut BodyBuf, name: &str, mutbl: Mutbl) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(mutbl), mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn struct_init_s(b: &mut BodyBuf, a_val: u128, b_val: u128) -> ExprId {
    let s_path = b.path("S");
    let a = u32_lit(b, a_val);
    let bv = u32_lit(b, b_val);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: a });
    b.field_inits.push(FieldInit { name: Name::of("b"), expr: bv });
    b.e(Expr::Struct { path: s_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None })
}

fn struct_pat_s(b: &mut BodyBuf, a_pat: PatId, b_pat: PatId) -> PatId {
    let fstart = b.field_pats.len() as u32;
    b.field_pats.push(FieldPat { name: Name::of("a"), pat: a_pat });
    b.field_pats.push(FieldPat { name: Name::of("b"), pat: b_pat });
    let sp_path = b.path("S");
    b.pat(Pat::Struct { path: sp_path, fields: List { start: fstart, len: 2 }, rest: false })
}

fn deref_local(b: &mut BodyBuf, p: PatId) -> ExprId {
    let l = b.local(p);
    b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: l })
}

/// Regression for `bind_let_place` (walk/matches/ref_bind.rs) reading the already-peeled
/// `checked.pat_ty` instead of `adjusted_ty(init)` (the `clap_derive` `Method::to_tokens`
/// miscompilation: `let Method { ref name, ref args } = self;` on `self: &Method`).
/// Exercises:
/// - `let S { ref a, ref b } = self_ref;` over a function parameter `&S` (exact `clap_derive` form)
/// - `let S { ref a, ref b } = s_ref;` over a local place `&S`
/// - `let S { ref a, ref b } = s_ref_ref;` over a double-ref local place `&&S`
/// - `let &S { ref a, ref b } = s_ref;` with an explicit `&` pattern
/// ```text
/// struct S { a: u32, b: u32 }
/// fn read_method(self_ref: &S) -> u32 {
///     let S { ref a, ref b } = self_ref;
///     *a * 10 + *b
/// }
/// fn main() -> u32 {
///     let s = S { a: 1, b: 2 };
///     let s_ref = &s;
///     let r1 = read_method(s_ref);           // 12
///     let S { ref a, ref b } = s_ref;        // 1, 2
///     let s_ref_ref = &s_ref;
///     let S { ref a: c, ref b: d } = s_ref_ref; // 1, 2
///     let &S { ref a: e, ref b: f } = s_ref;    // 1, 2
///     r1 * 1_000_000 + *a * 100_000 + *b * 10_000 + *c * 1_000 + *d * 100 + *e * 10 + *f
/// }
/// ```
/// Expected: returns 12_121_212 when executed natively.
#[test]
fn let_ref_bind_over_reference_param_local_double_ref_and_explicit_ref_pat() {
    const READ_METHOD: ItemKey = ItemKey(2);
    let mut w = TestWorld::new();
    w.strukt(9, "S", &[("a", U32), ("b", U32)]);
    let s_ty = PNode::Adt(ItemKey(9), List::EMPTY);
    let ref_s = w.ref_(Mutbl::Not, s_ty);
    w.func(1, "main", &[], U32);
    w.func(2, "read_method", &[ref_s], U32);

    // fn read_method(self_ref: &S) -> u32 { let S { ref a, ref b } = self_ref; *a * 10 + *b }
    let mut hb = BodyBuf::new();
    hb.edition = Edition::E2021;
    let self_ref = plain(&mut hb, "self_ref");
    hb.params.push(self_ref);
    let ha = ref_bind(&mut hb, "a", Mutbl::Not);
    let hb_pat = ref_bind(&mut hb, "b", Mutbl::Not);
    let hpat = struct_pat_s(&mut hb, ha, hb_pat);
    let hinit = hb.local(self_ref);
    let hstmt = let_stmt(hpat, hinit);
    let ha_val = deref_local(&mut hb, ha);
    let ten = u32_lit(&mut hb, 10);
    let ha_scaled = hb.bin(BinOp::Mul, ha_val, ten);
    let hb_val = deref_local(&mut hb, hb_pat);
    let hret = hb.bin(BinOp::Add, ha_scaled, hb_val);
    let hroot = hb.block(&[hstmt], Some(hret));
    let mut ground = PortableTys::new_in(Alloc::global());
    let helper_built = build_instance(&w, &hb, READ_METHOD, hroot, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));

    // fn main() -> u32
    let mut b = BodyBuf::new();
    b.edition = Edition::E2021;
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let s = S { a: 1, b: 2 };
    let s = b.bind("s");
    let s_init = struct_init_s(&mut b, 1, 2);
    stmts.push(let_stmt(s, s_init));

    // let s_ref = &s;
    let s_ref = plain(&mut b, "s_ref");
    let s_use = b.local(s);
    let s_ref_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_use });
    stmts.push(let_stmt(s_ref, s_ref_init));

    // let r1 = read_method(s_ref);
    let r1 = plain(&mut b, "r1");
    let s_ref_arg = b.local(s_ref);
    let r1_call = b.call("read_method", &[s_ref_arg]);
    stmts.push(let_stmt(r1, r1_call));

    // let S { ref a, ref b } = s_ref;
    let a = ref_bind(&mut b, "a", Mutbl::Not);
    let bb = ref_bind(&mut b, "b", Mutbl::Not);
    let s_pat1 = struct_pat_s(&mut b, a, bb);
    let s_ref_use1 = b.local(s_ref);
    stmts.push(let_stmt(s_pat1, s_ref_use1));

    // let s_ref_ref = &s_ref;
    let s_ref_ref = plain(&mut b, "s_ref_ref");
    let s_ref_use2 = b.local(s_ref);
    let s_ref_ref_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_ref_use2 });
    stmts.push(let_stmt(s_ref_ref, s_ref_ref_init));

    // let S { a: ref c, b: ref d } = s_ref_ref;
    let c = ref_bind(&mut b, "c", Mutbl::Not);
    let d = ref_bind(&mut b, "d", Mutbl::Not);
    let s_pat2 = struct_pat_s(&mut b, c, d);
    let s_ref_ref_use = b.local(s_ref_ref);
    stmts.push(let_stmt(s_pat2, s_ref_ref_use));

    // let &S { a: ref e, b: ref f } = s_ref;
    let e_pat = ref_bind(&mut b, "e", Mutbl::Not);
    let f_pat = ref_bind(&mut b, "f", Mutbl::Not);
    let inner_pat3 = struct_pat_s(&mut b, e_pat, f_pat);
    let s_pat3 = b.pat(Pat::Ref { mutbl: Mutbl::Not, inner: inner_pat3 });
    let s_ref_use3 = b.local(s_ref);
    stmts.push(let_stmt(s_pat3, s_ref_use3));

    // r1 * 1_000_000 + *a * 100_000 + *b * 10_000 + *c * 1_000 + *d * 100 + *e * 10 + *f
    let mut sum = deref_local(&mut b, f_pat);
    for (binding, scale) in [(a, 100_000), (bb, 10_000), (c, 1_000), (d, 100), (e_pat, 10)] {
        let star = deref_local(&mut b, binding);
        let k = u32_lit(&mut b, scale);
        let term = b.bin(BinOp::Mul, star, k);
        sum = b.bin(BinOp::Add, sum, term);
    }
    let r1_use = b.local(r1);
    let million = u32_lit(&mut b, 1_000_000);
    let r1_term = b.bin(BinOp::Mul, r1_use, million);
    sum = b.bin(BinOp::Add, r1_term, sum);

    let root = b.block(&stmts, Some(sum));
    let main_built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(READ_METHOD, &helper_built), (MAIN, &main_built)]), 12_121_212);
}

/// `let S { ref mut a, ref b } = s_mut_ref;` over a `&mut S` place expression: `a` must point
/// to `s.a` (mutating `*a` updates `s.a`) and `b` must point to `s.b`.
/// ```text
/// struct S { a: u32, b: u32 }
/// fn main() -> u32 {
///     let mut s = S { a: 3, b: 4 };
///     let s_mut_ref = &mut s;
///     let S { ref mut a, ref b } = s_mut_ref;
///     *a = *a + *b;
///     s.a * 10 + s.b
/// }
/// ```
/// Expected: returns 74 when executed natively.
#[test]
fn let_ref_mut_bind_over_mut_reference_place() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.strukt(9, "S", &[("a", U32), ("b", U32)]);
    let mut b = BodyBuf::new();
    b.edition = Edition::E2021;
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let mut s = S { a: 3, b: 4 };
    let s = b.bind("s");
    let s_init = struct_init_s(&mut b, 3, 4);
    stmts.push(let_stmt(s, s_init));

    // let s_mut_ref = &mut s;
    let s_mut_ref = plain(&mut b, "s_mut_ref");
    let s_use = b.local(s);
    let s_mut_ref_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: s_use });
    stmts.push(let_stmt(s_mut_ref, s_mut_ref_init));

    // let S { ref mut a, ref b } = s_mut_ref;
    let a = ref_bind(&mut b, "a", Mutbl::Mut);
    let bb = ref_bind(&mut b, "b", Mutbl::Not);
    let pat = struct_pat_s(&mut b, a, bb);
    let s_mut_ref_use = b.local(s_mut_ref);
    stmts.push(let_stmt(pat, s_mut_ref_use));

    // *a = *a + *b;
    let lhs = deref_local(&mut b, a);
    let ra = deref_local(&mut b, a);
    let rb = deref_local(&mut b, bb);
    let rhs = b.bin(BinOp::Add, ra, rb);
    let assign = b.e(Expr::Assign { lhs, rhs });
    stmts.push(Stmt::Expr { expr: assign, semi: true });

    // s.a * 10 + s.b
    let s1 = b.local(s);
    let sa = b.e(Expr::Field { base: s1, name: Name::of("a") });
    let ten = u32_lit(&mut b, 10);
    let tens = b.bin(BinOp::Mul, sa, ten);
    let s2 = b.local(s);
    let sb = b.e(Expr::Field { base: s2, name: Name::of("b") });
    let sum = b.bin(BinOp::Add, tens, sb);

    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(MAIN, &built)]), 74);
}

/// Nested tuple/struct peeled references and `x @ sub` subpattern bindings across `bind_value`,
/// `bind_let_value_ref`, and `bind_let_place`:
/// ```text
/// struct S { a: u32, b: u32 }
/// fn main() -> u32 {
///     let s = S { a: 2, b: 3 };
///     let s_ref = &s;
///     let (S { ref a, ref b },) = (s_ref,);          // value tuple with `ref` (2, 3)
///     let tup = (s_ref,);
///     let (S { a: ref c, b: ref d },) = tup;         // place tuple with `ref` (2, 3)
///     let (S { a: e, b: f },) = (&s,);               // value tuple without `ref` (2, 3)
///     let r @ S { a: g, b: h } = &s;                 // `x @ sub` over `&S` (r.a=2, g=2, h=3)
///     *a * 10_000_000 + *b * 1_000_000 + *c * 100_000 + *d * 10_000
///         + *e * 1_000 + *f * 100 + (*r).a * 10 + *g + *h
/// }
/// ```
/// Expected: `23_232_325` when executed natively.
#[test]
fn let_nested_tuple_struct_peeled_refs_and_at_subpattern() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.strukt(9, "S", &[("a", U32), ("b", U32)]);
    let mut b = BodyBuf::new();
    b.edition = Edition::E2021;
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let s = S { a: 2, b: 3 };
    let s = b.bind("s");
    let s_init = struct_init_s(&mut b, 2, 3);
    stmts.push(let_stmt(s, s_init));

    // let s_ref = &s;
    let s_ref = plain(&mut b, "s_ref");
    let s_use = b.local(s);
    let s_ref_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_use });
    stmts.push(let_stmt(s_ref, s_ref_init));

    // let (S { ref a, ref b },) = (s_ref,);
    let a = ref_bind(&mut b, "a", Mutbl::Not);
    let bb = ref_bind(&mut b, "b", Mutbl::Not);
    let sp1 = struct_pat_s(&mut b, a, bb);
    let tlist1 = b.pats_list(&[sp1]);
    let tpat1 = b.pat(Pat::Tuple { elems: tlist1, rest: None });
    let s_ref_1 = b.local(s_ref);
    let tinit1 = b.tuple(&[s_ref_1]);
    stmts.push(let_stmt(tpat1, tinit1));

    // let tup = (s_ref,); let (S { a: ref c, b: ref d },) = tup;
    let tup = plain(&mut b, "tup");
    let s_ref_2 = b.local(s_ref);
    let tup_init = b.tuple(&[s_ref_2]);
    stmts.push(let_stmt(tup, tup_init));
    let c = ref_bind(&mut b, "c", Mutbl::Not);
    let d = ref_bind(&mut b, "d", Mutbl::Not);
    let sp2 = struct_pat_s(&mut b, c, d);
    let tlist2 = b.pats_list(&[sp2]);
    let tpat2 = b.pat(Pat::Tuple { elems: tlist2, rest: None });
    let tup_use = b.local(tup);
    stmts.push(let_stmt(tpat2, tup_use));

    // let (S { a: e, b: f },) = (&s,);
    let e_pat = plain(&mut b, "e");
    let f_pat = plain(&mut b, "f");
    let sp3 = struct_pat_s(&mut b, e_pat, f_pat);
    let tlist3 = b.pats_list(&[sp3]);
    let tpat3 = b.pat(Pat::Tuple { elems: tlist3, rest: None });
    let s_use2 = b.local(s);
    let s_ref_tmp = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_use2 });
    let tinit3 = b.tuple(&[s_ref_tmp]);
    stmts.push(let_stmt(tpat3, tinit3));

    // let r @ S { a: g, b: h } = &s;
    let g = plain(&mut b, "g");
    let h = plain(&mut b, "h");
    let sp4 = struct_pat_s(&mut b, g, h);
    let r = b.pat(Pat::Bind {
        ann: BindAnn { by_ref: None, mutable: false },
        name: Name::of("r"),
        sub: Some(sp4),
        ty: None,
    });
    let s_use3 = b.local(s);
    let s_ref_tmp2 = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: s_use3 });
    stmts.push(let_stmt(r, s_ref_tmp2));

    let g_val = deref_local(&mut b, g);
    let h_val = deref_local(&mut b, h);
    let mut sum = b.bin(BinOp::Add, g_val, h_val);
    let r_deref = deref_local(&mut b, r);
    let r_a = b.e(Expr::Field { base: r_deref, name: Name::of("a") });
    let ten = u32_lit(&mut b, 10);
    let r_term = b.bin(BinOp::Mul, r_a, ten);
    sum = b.bin(BinOp::Add, sum, r_term);
    for (binding, scale) in [(a, 10_000_000), (bb, 1_000_000), (c, 100_000), (d, 10_000), (e_pat, 1_000), (f_pat, 100)] {
        let star = deref_local(&mut b, binding);
        let k = u32_lit(&mut b, scale);
        let term = b.bin(BinOp::Mul, star, k);
        sum = b.bin(BinOp::Add, sum, term);
    }

    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_native(&[(MAIN, &built)]), 23_232_325);
}
