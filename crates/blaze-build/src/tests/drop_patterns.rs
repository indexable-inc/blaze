//! A by-value binding of a part that needs drop in an `if let` on a local, and a `let` whose
//! pattern leaves such a part unbound (`_`, a left-out field): the first moves that part out of
//! the local by its field flags (walk/lets.rs `let_link`, walk/moves.rs `move_out`), the second
//! leaves it with its owner (walk/matches/irrefutable.rs `bind_let_owned`).
//!
//! The Reference:
//! - "Wildcard pattern": "Unlike identifier patterns, it does not copy, move or borrow the
//!   value it matches."
//! - "Destructors": "When control flow leaves a drop scope all variables associated to that
//!   scope are dropped in reverse order of declaration"; "If a variable has been partially
//!   initialized, only its initialized fields are dropped"; "Temporary scopes": the `let`
//!   statement is the temporary scope of its initializer.
//!
//! The library's own uses: bstd's `Placed::new` (`if let Some(frame) = frame { .. }`, `frame` a
//! parameter) and `World::new` (`let Start { driver, env: _, .. } = start;`). `G { x }` logs `x`
//! when dropped; the world is tests.rs `match_world` (`enum E { A(G, G), .. }`) and a struct
//! `S { p: G, q: G }`.

use super::*;

fn let_stmt(pat: PatId, init: ExprId) -> Stmt {
    Stmt::Let { pat, ty: None, init: Some(init), else_: None }
}

/// `(G{1}, G{2}, ..)` of the given `x`s.
fn g_tuple(b: &mut BodyBuf, xs: &[u128]) -> ExprId {
    let mut elems: Vec<ExprId, Global> = Vec::new_in(Global);
    for &x in xs {
        elems.push(g_lit(b, x));
    }
    b.tuple(&elems)
}

/// Which body `drops` builds.
#[derive(Clone, Copy, Debug)]
enum Form {
    /// `let x = A(G{1}, G{2}); if let A(a, _) = x {} let k = G{9};`
    IfLetLocal,
    /// `let x = (G{1}, G{2}, G{3}); let (a, b, _) = x; let k = G{9};`
    LetLocalTuple,
    /// `let (a, _) = (G{1}, G{2}); let k = G{9};`
    LetTempTuple,
    /// `let s = S { p: G{1}, q: G{2} }; let S { p, q: _ } = s; let k = G{9};`
    LetLocalStruct,
}

/// `fn main() { <form> }` built, and run natively with the glue of `G`, `N` and `E`: the order
/// of the drops (each `G`'s `x`; 9 is `k`).
fn drops(form: Form) -> Result<Vec<i32, Global>, String> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    w.strukt(140, "S", &[("p", g), ("q", g)]);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    match form {
        Form::IfLetLocal => {
            let e_ty = b.types.intern(e).expect("a type interns");
            let x = b.bind("x");
            let value = scrut_value(&mut b, Scrut::A);
            stmts.push(Stmt::Let { pat: x, ty: Some(e_ty), init: Some(value), else_: None });
            let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
            let pa = variant_pat(&mut b, "A", &[a, w0]);
            let init = b.local(x);
            let cond = b.e(Expr::Let { pat: pa, init });
            let then = b.block(&[], None);
            let iff = b.e(Expr::If { cond, then, else_: None });
            stmts.push(Stmt::Expr { expr: iff, semi: true });
        }
        Form::LetLocalTuple => {
            let x = b.bind("x");
            let value = g_tuple(&mut b, &[1, 2, 3]);
            stmts.push(let_stmt(x, value));
            let (a, bb, w0) = (b.bind("a"), b.bind("b"), b.pat(Pat::Wild));
            let elems = b.pats_list(&[a, bb, w0]);
            let pat = b.pat(Pat::Tuple { elems, rest: None });
            let init = b.local(x);
            stmts.push(let_stmt(pat, init));
        }
        Form::LetTempTuple => {
            let value = g_tuple(&mut b, &[1, 2]);
            let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
            let elems = b.pats_list(&[a, w0]);
            let pat = b.pat(Pat::Tuple { elems, rest: None });
            stmts.push(let_stmt(pat, value));
        }
        Form::LetLocalStruct => {
            let s = b.bind("s");
            let s_path = b.path("S");
            let (g1, g2) = (g_lit(&mut b, 1), g_lit(&mut b, 2));
            let start = b.field_inits.len() as u32;
            b.field_inits.push(FieldInit { name: Name::of("p"), expr: g1 });
            b.field_inits.push(FieldInit { name: Name::of("q"), expr: g2 });
            let value = b.e(Expr::Struct { path: s_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
            stmts.push(let_stmt(s, value));
            let (p, w0) = (b.bind("p"), b.pat(Pat::Wild));
            let fstart = b.field_pats.len() as u32;
            b.field_pats.push(FieldPat { name: Name::of("p"), pat: p });
            b.field_pats.push(FieldPat { name: Name::of("q"), pat: w0 });
            let sp_path = b.path("S");
            let pat = b.pat(Pat::Struct { path: sp_path, fields: List { start: fstart, len: 2 }, rest: false });
            let init = b.local(s);
            stmts.push(let_stmt(pat, init));
        }
    }
    let k = b.bind("k");
    let k_init = g_lit(&mut b, 9);
    stmts.push(let_stmt(k, k_init));
    let root = b.block(&stmts, None);
    g_drops(&w, &b, root, [g, n, e])
}

/// Each form built (before: refused with "pattern N: a binding by value of a part that needs
/// drop, in a match by tests (a move out)" for the `if let`, "pattern N: a `_` pattern over a
/// value that needs drop" for the `let`s), and every `G` dropped once, in the Reference's order:
/// - `if let A(a, _) = x {}`: `a` (G1) moved out of `x` and dropped at the `if`'s end: 1; then
///   at the block's end `k` (9) and `x`'s rest, never G1 again: 2.
/// - `let (a, b, _) = x;`: nothing dropped at the `let`; at the block's end, in reverse order of
///   declaration, `k`, `b`, `a`, then `x`'s rest (G3, which `_` left in `x`): 9, 2, 1, 3. A
///   build that owned the bindings last part first would drop `a` before `b` (9, 1, 2, 3).
/// - `let (a, _) = (G{1}, G{2});`: the temporary's rest (G2) at the statement's end: 2; then
///   `k`, `a`: 9, 1.
/// - `let S { p, q: _ } = s;`: `k`, `p`, then `s`'s rest (its `q`): 9, 1, 2.
///
/// `cargo test -p blaze-build drop_patterns` prints
/// `test tests::drop_patterns::a_let_or_if_let_leaving_or_moving_a_part_drops_each_part_once ... ok`.
#[test]
fn a_let_or_if_let_leaving_or_moving_a_part_drops_each_part_once() {
    let cases: &[(Form, &[i32])] = &[
        (Form::IfLetLocal, &[1, 9, 2]),
        (Form::LetLocalTuple, &[9, 2, 1, 3]),
        (Form::LetTempTuple, &[2, 9, 1]),
        (Form::LetLocalStruct, &[9, 1, 2]),
    ];
    let mut failed = String::new();
    for &(form, want) in cases {
        match drops(form) {
            Ok(seen) if seen.as_slice() == want => {}
            Ok(seen) => failed.push_str(&format!("{form:?}: dropped {seen:?}, want {want:?}\n")),
            Err(e) => failed.push_str(&format!("{form:?}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
}

/// The fn `main` calls in the parameter tests.
const F: ItemKey = ItemKey(250);

/// Which parameters `param_drops` gives `f`.
#[derive(Clone, Copy, Debug)]
enum Param {
    /// `fn f(_: G) { let k = G{9}; }`, `f(G{1})`
    Wild,
    /// The Reference's own example ("Destructors", "Drop scopes"):
    /// `fn f((x, _): (G, G), (_, y): (G, G)) {}`, `f((G{0}, G{1}), (G{2}, G{3}))`
    TwoTuples,
    /// `fn f(((a, _), _): ((G, G), G)) { let k = G{9}; }`, `f(((G{1}, G{2}), G{3}))`
    Nested,
}

/// `fn main() { f(..); }` and `f` of `param` built in one table and run natively with the glue
/// of `G`, `(G, G)` and `((G, G), G)`: the order of the drops (each `G`'s `x`).
fn param_drops(param: Param) -> Result<Vec<i32, Global>, String> {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let pair = w.tuple(&[g, g]);
    let nested = w.tuple(&[pair, g]);
    let inputs: &[PNode] = match param {
        Param::Wild => &[g],
        Param::TwoTuples => &[pair, pair],
        Param::Nested => &[nested],
    };
    w.func(250, "f", inputs, UNIT);
    let mut fb = BodyBuf::new();
    let mut fstmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let mut mb = BodyBuf::new();
    let args: Vec<ExprId, Global> = match param {
        Param::Wild => {
            let w0 = fb.pat(Pat::Wild);
            fb.params.push(w0);
            let mut a = Vec::new_in(Global);
            a.push(g_lit(&mut mb, 1));
            a
        }
        Param::TwoTuples => {
            let (x, w0) = (fb.bind("x"), fb.pat(Pat::Wild));
            let first = fb.pats_list(&[x, w0]);
            let first = fb.pat(Pat::Tuple { elems: first, rest: None });
            let (w1, y) = (fb.pat(Pat::Wild), fb.bind("y"));
            let second = fb.pats_list(&[w1, y]);
            let second = fb.pat(Pat::Tuple { elems: second, rest: None });
            fb.params.extend_from_slice(&[first, second]);
            let mut a = Vec::new_in(Global);
            a.push(g_tuple(&mut mb, &[0, 1]));
            a.push(g_tuple(&mut mb, &[2, 3]));
            a
        }
        Param::Nested => {
            let (a, w0, w1) = (fb.bind("a"), fb.pat(Pat::Wild), fb.pat(Pat::Wild));
            let inner = fb.pats_list(&[a, w0]);
            let inner = fb.pat(Pat::Tuple { elems: inner, rest: None });
            let outer = fb.pats_list(&[inner, w1]);
            let outer = fb.pat(Pat::Tuple { elems: outer, rest: None });
            fb.params.push(outer);
            let inner_v = g_tuple(&mut mb, &[1, 2]);
            let third = g_lit(&mut mb, 3);
            let mut v = Vec::new_in(Global);
            v.push(mb.tuple(&[inner_v, third]));
            v
        }
    };
    if let Param::Wild | Param::Nested = param {
        let k = fb.bind("k");
        let k_init = g_lit(&mut fb, 9);
        fstmts.push(let_stmt(k, k_init));
    }
    let froot = fb.block(&fstmts, None);
    let call = mb.call("f", &args);
    let mroot = mb.block(&[Stmt::Expr { expr: call, semi: true }], None);
    let mut ground = PortableTys::new_in(Alloc::global());
    let f = build_instance(&w, &fb, F, froot, &[], &mut ground)?;
    let main = build_instance(&w, &mb, MAIN, mroot, &[], &mut ground)?;
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (f, main, g, pair, nested);
        Ok(Vec::new_in(Global))
    }
    #[cfg(target_arch = "aarch64")]
    {
        let mut gb = BodyBuf::new();
        let p = gb.bind("to_drop");
        gb.params.push(p);
        let groot = gb.block(&[], None);
        let view = gb.view(DROP_IN_PLACE, groot);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => return Err(format!("the glue's check did not finish: {:?}", other.err())),
        };
        let sig = w.sig(DROP_IN_PLACE).expect("a signature");
        // The nodes of the ground table (the world's `pair` and `nested` name its own lists):
        // `G`'s first, then `(G, G)`, then `((G, G), G)`, each glue placed after its callees'.
        let _ = (pair, nested);
        let gt = ground.intern(g).expect("intern");
        let pair = PNode::Tuple(ground.intern_list(&[gt, gt]).expect("intern"));
        let pt = ground.intern(pair).expect("intern");
        let nested = PNode::Tuple(ground.intern_list(&[pt, gt]).expect("intern"));
        let types = [g, pair, nested];
        let mut glues: Vec<(List, Built<Global>), Global> = Vec::new_in(Global);
        for &t in types.iter() {
            let ty = ground.intern(t).expect("intern");
            let args = ground.intern_list(&[ty]).expect("intern");
            let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
            let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
                .map_err(|e| format!("the glue: {e}"))?;
            glues.push((args, built));
        }
        {
            let v = ground.view();
            for built in [&f, &main] {
                for r in built.refs.iter().filter(|r| r.item == DROP_IN_PLACE) {
                    let node = v.list(r.args).ok().and_then(|l| l.first().copied()).and_then(|t| v.get(t).copied());
                    if !node.is_some_and(|n| types.contains(&n)) {
                        return Err(format!("a drop at arguments {:?} with no glue placed:\n{}", r.args, blaze_lir::print(built.func.view())));
                    }
                }
            }
        }
        let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
        for (args, built) in glues.iter() {
            fns.push((DROP_IN_PLACE, Some(*args), built));
        }
        fns.push((F, None, &f));
        fns.push((MAIN, None, &main));
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(&[(G_DROP, host_g_drop as usize as u64)], &fns);
        let seen = DROPPED.with(|d| d.borrow().clone());
        if std::env::var_os("BLAZE_TEST_PRINT").is_some() {
            println!("{param:?}: {seen:?}\n{}", blaze_lir::print(f.func.view()));
        }
        Ok(seen)
    }
}

/// A parameter pattern that leaves a part that needs drop unbound (walk/matches/irrefutable.rs
/// `bind_param`; tempo's census of 04:02: 30 refusals, "pattern N: a `_` pattern over a value
/// that needs drop", std's `impl From<NulError> for io::Error { fn from(_: NulError) .. }`).
/// The Reference, "Destructors", "Drop scopes": "All function parameters are in the scope of
/// the entire function body, so are dropped last when evaluating a function. Each actual
/// function parameter is dropped after any bindings introduced in that parameter's pattern";
/// "Wildcard pattern": `_` does not move what it matches, so the argument keeps it.
/// - `fn f(_: G) { let k = G{9}; }`: `k`, then the argument: 9, 1.
/// - `fn f((x, _): (G, G), (_, y): (G, G)) {}` (the Reference's example, "drop order is 3 2 0
///   1"): the second parameter's `y`, its rest, the first's `x`, its rest: 3, 2, 0, 1. A build
///   that dropped every rest after every binding would give 3, 0, 2, 1.
/// - `fn f(((a, _), _): ((G, G), G)) { let k = G{9}; }`: `k`, `a`, then the argument's rest
///   in declaration order (its `.0.1`, then its `.1`): 9, 1, 2, 3.
///
/// `cargo test -p blaze-build drop_patterns` prints
/// `test tests::drop_patterns::a_parameter_pattern_leaving_a_part_drops_it_after_its_bindings ... ok`.
#[test]
fn a_parameter_pattern_leaving_a_part_drops_it_after_its_bindings() {
    let cases: &[(Param, &[i32])] = &[(Param::Wild, &[9, 1]), (Param::TwoTuples, &[3, 2, 0, 1]), (Param::Nested, &[9, 1, 2, 3])];
    let mut failed = String::new();
    for &(param, want) in cases {
        match param_drops(param) {
            Ok(seen) if seen.as_slice() == want || (!cfg!(target_arch = "aarch64") && seen.is_empty()) => {}
            Ok(seen) => failed.push_str(&format!("{param:?}: dropped {seen:?}, want {want:?}\n")),
            Err(e) => failed.push_str(&format!("{param:?}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
}

/// `let (a, b): (G, G); b = G{2}; a = G{1}; let k = G{9};` (num-bigint's `let (mut q, r1, d);`;
/// tempo's census of 04:02: 39 refusals, "pattern N: a pattern other than a binding or `_`"):
/// the Reference, "let statements": the pattern's variables are declared, none initialized;
/// each assignment initializes one, and at the block's end they drop in reverse order of
/// declaration, whatever order they were assigned in: `k`, `b`, `a`: 9, 2, 1 (walk/matches/
/// irrefutable.rs `declare_let`). A build that owned them in assignment order gives 9, 1, 2.
///
/// `cargo test -p blaze-build drop_patterns` prints
/// `test tests::drop_patterns::a_let_without_an_initializer_declares_each_binding_of_its_pattern ... ok`.
#[test]
fn a_let_without_an_initializer_declares_each_binding_of_its_pattern() {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let gt = b.types.intern(g).expect("a type interns");
    let gs = b.types.intern_list(&[gt, gt]).expect("a list interns");
    let pair = b.types.intern(PNode::Tuple(gs)).expect("a type interns");
    let (a, bb) = (b.bind("a"), b.bind("b"));
    let elems = b.pats_list(&[a, bb]);
    let pat = b.pat(Pat::Tuple { elems, rest: None });
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    stmts.push(Stmt::Let { pat, ty: Some(pair), init: None, else_: None });
    for (binding, x) in [(bb, 2), (a, 1)] {
        let lhs = b.local(binding);
        let rhs = g_lit(&mut b, x);
        let expr = b.e(Expr::Assign { lhs, rhs });
        stmts.push(Stmt::Expr { expr, semi: true });
    }
    let k = b.bind("k");
    let k_init = g_lit(&mut b, 9);
    stmts.push(let_stmt(k, k_init));
    let root = b.block(&stmts, None);
    let seen = g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"));
    if cfg!(target_arch = "aarch64") {
        assert_eq!(seen.as_slice(), &[9, 2, 1]);
    }
}

/// Which body `order_drops` builds: one pattern binding every part by value.
#[derive(Clone, Copy, Debug)]
enum Order {
    /// `match (G{1}, G{2}) { (a, b) => {} }`
    MatchTuple,
    /// `if let (a, b) = (G{1}, G{2}) {}`
    IfLetTuple,
    /// `match Trio { x: G{1}, y: G{2}, z: G{3} } { Trio { z, x, y } => {} }`
    MatchStructOutOfOrder,
    /// `let (a, b) = (G{1}, G{2});`
    LetTuple,
}

/// `fn main() { <order> }` built, and run natively with the glue of `G`, `N` and `E`: the
/// order of the drops (each `G`'s `x`). The struct is named `Trio`, not `P`: tests.rs
/// `match_world` already has a variant `P`, and variants resolve by their own name.
fn order_drops(order: Order) -> Result<Vec<i32, Global>, String> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    w.strukt(141, "Trio", &[("x", g), ("y", g), ("z", g)]);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    match order {
        Order::MatchTuple | Order::IfLetTuple | Order::LetTuple => {
            let value = g_tuple(&mut b, &[1, 2]);
            let (a, bb) = (b.bind("a"), b.bind("b"));
            let elems = b.pats_list(&[a, bb]);
            let pat = b.pat(Pat::Tuple { elems, rest: None });
            match order {
                Order::MatchTuple => {
                    let body = b.block(&[], None);
                    let start = b.arms.len() as u32;
                    b.arms.push(Arm { pat, guard: None, body });
                    let m = b.e(Expr::Match { scrutinee: value, arms: List { start, len: 1 } });
                    stmts.push(Stmt::Expr { expr: m, semi: true });
                }
                Order::IfLetTuple => {
                    let cond = b.e(Expr::Let { pat, init: value });
                    let then = b.block(&[], None);
                    let iff = b.e(Expr::If { cond, then, else_: None });
                    stmts.push(Stmt::Expr { expr: iff, semi: true });
                }
                _ => stmts.push(let_stmt(pat, value)),
            }
        }
        Order::MatchStructOutOfOrder => {
            let path = b.path("Trio");
            let (g1, g2, g3) = (g_lit(&mut b, 1), g_lit(&mut b, 2), g_lit(&mut b, 3));
            let start = b.field_inits.len() as u32;
            b.field_inits.push(FieldInit { name: Name::of("x"), expr: g1 });
            b.field_inits.push(FieldInit { name: Name::of("y"), expr: g2 });
            b.field_inits.push(FieldInit { name: Name::of("z"), expr: g3 });
            let value = b.e(Expr::Struct { path, fields: List { start, len: 3 }, base: blaze_typeck::StructBase::None });
            let (z, x, y) = (b.bind("z"), b.bind("x"), b.bind("y"));
            let fstart = b.field_pats.len() as u32;
            b.field_pats.push(FieldPat { name: Name::of("z"), pat: z });
            b.field_pats.push(FieldPat { name: Name::of("x"), pat: x });
            b.field_pats.push(FieldPat { name: Name::of("y"), pat: y });
            let pat_path = b.path("Trio");
            let pat = b.pat(Pat::Struct { path: pat_path, fields: List { start: fstart, len: 3 }, rest: false });
            let body = b.block(&[], None);
            let astart = b.arms.len() as u32;
            b.arms.push(Arm { pat, guard: None, body });
            let m = b.e(Expr::Match { scrutinee: value, arms: List { start: astart, len: 1 } });
            stmts.push(Stmt::Expr { expr: m, semi: true });
        }
    }
    let root = b.block(&stmts, None);
    g_drops(&w, &b, root, [g, n, e])
}

/// The bindings of one pattern drop in reverse order of declaration, which is the order they
/// are written ("Destructors": "When control flow leaves a drop scope all variables associated
/// to that scope are dropped in reverse order of declaration"), in a match arm and an `if let`
/// (matches.rs `bind_with`) as in a plain `let` (walk.rs `own_pat`):
/// - `match (G{1}, G{2}) { (a, b) => {} }`: `b`, `a`: 2, 1.
/// - `if let (a, b) = (G{1}, G{2}) {}`: 2, 1.
/// - `match Trio { x: G{1}, y: G{2}, z: G{3} } { Trio { z, x, y } => {} }`: the reverse of the
///   order written (`z`, `x`, `y`), not of the fields': `y`, `x`, `z`: 2, 1, 3.
/// - `let (a, b) = (G{1}, G{2});` (the control, already right through `own_pat`): 2, 1.
///
/// A build that owns a tested pattern's bindings last part first gives 1, 2 for the first two
/// and 3, 1, 2 for the third.
///
/// `cargo test -p blaze-build drop_patterns` prints
/// `test tests::drop_patterns::a_patterns_bindings_drop_in_reverse_of_the_order_written ... ok`.
#[test]
fn a_patterns_bindings_drop_in_reverse_of_the_order_written() {
    let cases: &[(Order, &[i32])] = &[
        (Order::MatchTuple, &[2, 1]),
        (Order::IfLetTuple, &[2, 1]),
        (Order::MatchStructOutOfOrder, &[2, 1, 3]),
        (Order::LetTuple, &[2, 1]),
    ];
    let mut failed = String::new();
    for &(order, want) in cases {
        match order_drops(order) {
            Ok(seen) if seen.as_slice() == want => {}
            Ok(seen) => failed.push_str(&format!("{order:?}: dropped {seen:?}, want {want:?}\n")),
            Err(e) => failed.push_str(&format!("{order:?}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
}
