//! The lowering's tests: each form's dump, the desugarings against rustc's expansions, the
//! scopes, the record-equality property, the depth bound and the planted bug's control.

use std::string::String;
use std::vec::Vec as StdVec;
use std::{format, vec};

use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::columns::View;
use crate::driver::{Expansion, ItemRef, Outcome, What};
use crate::form::*;
use crate::plant::{self, Plant};
use crate::{Cfg, CfgAnswer, Edition, Expand, Lowered, Sink, lower_body, lower_file, lower_signature};

/// The tests' cfg set: `unix` and nothing else.
struct Unix;

impl Cfg for Unix {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        match (name, value) {
            ("unix", None) => CfgAnswer::Yes,
            _ => CfgAnswer::No,
        }
    }
}

fn body(text: &str) -> Lowered<'static, '_> {
    match lower_body(text, Edition::E2024, &mut Unix, &mut crate::NoModule, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

fn signature(text: &str) -> Lowered<'static, '_> {
    match lower_signature(text, Edition::E2024, &mut Unix, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

fn dump_of(l: &Lowered<'_, '_>) -> String {
    let mut out = String::new();
    crate::dump::dump(&l.view(), Alloc::global(), &mut out).expect("a String takes every write");
    out
}

/// Whether `fragments` are in `dump`, each after the one before.
fn in_order(dump: &str, fragments: &[&str]) -> Result<(), String> {
    let mut at = 0;
    for f in fragments {
        match dump[at..].find(f) {
            Some(i) => at += i + f.len(),
            None => return Err(format!("`{f}` not found after byte {at} of\n{dump}")),
        }
    }
    Ok(())
}

#[track_caller]
fn assert_body(text: &str, fragments: &[&str]) {
    let l = body(text);
    let dump = dump_of(&l);
    if let Err(why) = in_order(&dump, fragments) {
        panic!("{text}:\n{why}");
    }
    let v = l.view();
    assert!(v.unhandled.is_empty(), "{text}: unhandled forms\n{dump}");
    assert!(v.diags.is_empty(), "{text}: diagnostics\n{dump}");
}

// ---------------------------------------------------------------------------------------------
// One per form

#[test]
fn every_expression_form_dumps() {
    let cases: &[(&str, &[&str])] = &[
        ("fn f() -> u8 { 1 }", &["(body", "(block e", "(tail (lit 1))"]),
        ("fn f() { let x = 1; }", &["(let x@p0 (lit 1))"]),
        ("fn f(x: u8) -> u8 { x }", &["(body x@p0", "(tail x@p0)"]),
        ("fn f() { g(1, 2); }", &["(semi (call (path g) (lit 1) (lit 2)))"]),
        ("fn f(v: V) { v.m::<u8>(1); }", &["(method v@p0 m (path u8) (lit 1))"]),
        ("fn f(v: V) { v.a; v.0; }", &["(field v@p0 a)", "(field v@p0 0)"]),
        ("fn f(a: u8) { -a; !a; }", &["(neg a@p0)", "(not a@p0)"]),
        ("fn f(a: &u8) -> u8 { *a }", &["(deref a@p0)"]),
        ("fn f(a: u8) { a + 1 * 2; }", &["(bin Add a@p0 (bin Mul (lit 1) (lit 2)))"]),
        ("fn f(mut a: u8) { a = 1; a += 2; }", &["(assign a@p0 (lit 1))", "(assign-op Add a@p0 (lit 2))"]),
        ("fn f(a: u8) { &a; &mut a; }", &["(ref a@p0)", "(ref-mut a@p0)"]),
        ("fn f(a: u8) { &raw const a; }", &["(raw-const a@p0)"]),
        ("fn f(a: u8) { a as u32; }", &["(cast a@p0 (path u32))"]),
        ("fn f() { (1, 2); [1, 2]; [0; 4]; }", &["(tuple (lit 1) (lit 2))", "(array (lit 1) (lit 2))", "(repeat (lit 0) (lit 4))"]),
        ("fn f(s: S) { S { a: 1, ..s }; }", &["(struct (path S) (init a (lit 1)) .. s@p0)"]),
        ("fn f(v: V) { v[0]; }", &["(index v@p0 (lit 0))"]),
        ("fn f(c: bool) { if c { 1 } else { 2 }; }", &["(if c@p0 (block", "(lit 1)", "(block", "(lit 2)"]),
        ("fn f(o: O) { match o { E::A => 1, _ if true => 2 }; }", &["(match o@p0 (arm (path E A) (lit 1)) (arm _ (if (lit true)) (lit 2)))"]),
        ("fn f() { loop { break; } }", &["(loop e", "(break ->e"]),
        ("fn f() -> u8 { return 1; }", &["(return (lit 1))"]),
        ("fn f() { let g = |x: u8| -> u8 { x }; }", &["(closure (param x@p", "(path u8))", "(-> (path u8))"]),
        ("fn f() { let g = move || 1; }", &["(closure move"]),
        ("fn f() { unsafe { g() } }", &["(unsafe e"]),
        ("fn f() { const { 1 }; }", &["(const e"]),
        ("fn f() { async { 1 }; }", &["(async-block"]),
        ("fn f() { 'a: { break 'a 1; } }", &["(block e", "(break ->e"]),
        ("fn f(x: u8) { format_args!(\"{} {x}\", 1); }", &["(format-args \"{} {x}\"", "(arg (lit 1))", "(captured x x@p0)"]),
        ("fn f() { (1); }", &["(paren (lit 1))"]),
        ("fn f() { become g(); }", &["(become (call (path g)))"]),
    ];
    for (text, fragments) in cases {
        assert_body(text, fragments);
    }
}

#[test]
fn every_pattern_form_dumps() {
    let cases: &[(&str, &[&str])] = &[
        ("fn f() { let _ = 1; }", &["(let _ (lit 1))"]),
        ("fn f() { let (a, ..) = t; }", &["(tuple a@p0)"]),
        ("fn f() { let &mut a = r; }", &["(ref a@p0)"]),
        ("fn f() { let ref mut a = r; }", &["ref mut a@p0"]),
        ("fn f() { let a @ Some(_) = o; }", &["a@p", "(@ (tuple-struct (path Some) _))"]),
        ("fn f() { let S { a, b: c, .. } = s; }", &["(struct (path S) (field a a@p", "(field b c@p", ".."]),
        ("fn f() { let [a, b] = s; }", &["(slice a@p0 b@p1)"]),
        ("fn f() { match x { 1 | 2 => {} 3..=4 => {} -1 => {} _ => {} } }", &["(or (lit 1) (lit 2))", "(range= (lit 3) (lit 4))", "(neg-lit -1)"]),
        ("fn f() { let box a = b; }", &["(box a@p0)"]),
    ];
    for (text, fragments) in cases {
        let l = body(text);
        let dump = dump_of(&l);
        if let Err(why) = in_order(&dump, fragments) {
            panic!("{text}:\n{why}");
        }
    }
}

#[test]
fn every_signature_form_dumps() {
    let cases: &[(&str, &[&str])] = &[
        // The dump's order: attributes, generics, where, parameters, bounds, fields, items,
        // then the trait, the self type and the type.
        ("pub fn f<T: Clone>(x: T) -> T where T: Copy { x }", &["(sig FN f", "Pub", "(generics (type T (bound (path Clone))))", "(where (pred (path T) (bound (path Copy))))", "(params (param x@p0 (path T)))", "(ty (path T))"]),
        ("struct S { pub a: u8, b: Vec<u8> }", &["(sig STRUCT S", "(field Pub a (path u8))", "(field b (path (seg Vec (path u8))))"]),
        ("enum E { A, B(u8), C = 3 }", &["(sig ENUM E", "(variant A unit)", "(variant B () (field (path u8)))", "(variant C unit = 3)"]),
        ("trait T: Clone { fn f(&self); }", &["(sig TRAIT T", "(bounds (bound (path Clone)))", "(item FN f)"]),
        ("impl<T> Tr for S<T> { fn f(&self) {} }", &["(sig IMPL", "(item FN f)", "(trait (path Tr))", "(self (path (seg S (path T))))"]),
        ("type A = [u8; 4];", &["(sig TYPE_ALIAS A", "(ty (array (path u8) 4))"]),
        ("const C: &'static str = \"\";", &["(sig CONST C", "(ty (& ", "(path str)))"]),
        ("static S: fn(u8) -> u8 = f;", &["(sig STATIC S", "(ty (fn"]),
        ("#[cfg(windows)] fn f() {}", &["(cfg-out)"]),
    ];
    for (text, fragments) in cases {
        let l = signature(text);
        let dump = dump_of(&l);
        if let Err(why) = in_order(&dump, fragments) {
            panic!("{text}:\n{why}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The desugarings, against rustc's (rustc_ast_lowering, as `-Zunpretty=hir` prints them)

/// rustc: `match IntoIterator::into_iter(xs) { mut iter => loop { match Iterator::next(&mut
/// iter) { None => break, Some(x) => { .. } } } }`, the `None` arm first.
#[test]
fn for_is_rustcs() {
    assert_body(
        "fn f(xs: X) { for x in xs { g(x); } }",
        &[
            "(match/for (call #into_iter xs@p0)",
            "(arm mut $iter@p",
            "(loop/for e",
            "(match/for (call #next (ref-mut $iter@p",
            "(arm #None (break ->e",
            "(arm (tuple-struct #Some x@p",
            "(call (path g) x@p",
        ],
    );
    let v = body("fn f(xs: X) { for x in xs {} }");
    let langs: StdVec<_> = v.view().langs().collect();
    assert_eq!(langs, vec![LangItem::IntoIterIntoIter, LangItem::IteratorNext, LangItem::OptionSome, LangItem::OptionNone]);
}

/// rustc: `loop { if c { .. } else { break } }`.
#[test]
fn while_is_rustcs() {
    assert_body("fn f(c: bool) { while c { g(); } }", &["(loop/while e", "(tail (if c@p0 (block", "(stmt (break ->e"]);
}

/// The Reference, "Destructuring assignments": "the assignee expression is first evaluated
/// ... desugared to a `let` binding of fresh variables by the corresponding pattern, followed
/// by the assignment of those variables to the places, in order"; so the right side (`p()`,
/// `q()`) runs before any place's index (`n()`, `m()`), and the places are assigned in the
/// order written. `_` is a wildcard, `..` a rest, nested assignees nest, and a struct's `..`
/// is its pattern's rest. Before the fix every one was "unhandled BIN_EXPR".
#[test]
fn destructuring_assignment_is_a_let_then_assignments() {
    assert_body(
        "fn f(v: V, w: W) { (v[n()], w[m()]) = (p(), q()); }",
        &[
            "(let (tuple $made@p",
            "(call (path p)",
            "(call (path q)",
            "(semi (assign (index v@p0 (call (path n)",
            "(semi (assign (index w@p1 (call (path m)",
        ],
    );
    assert_body("fn f(a: A, b: B) { (a, b) = (b, a); }", &["(let (tuple $made@p", "(assign a@p0 $made@p", "(assign b@p1 $made@p"]);
    assert_body("fn f(a: A, t: T) { (a, _) = t; }", &["(let (tuple $made@p", "_", "(assign a@p0 $made@p"]);
    assert_body("fn f(x: X, y: Y, arr: R) { [x, .., y] = arr; }", &["(let (slice $made@p", "..", "(assign x@p0", "(assign y@p1"]);
    assert_body("fn f(g: G, s: S) { S { f: g, .. } = s; }", &["(let (struct (path S) (field f $made@p", "..", "(assign g@p0"]);
    assert_body(
        "fn f(a: A, b: B, c: C) { ((a, Pair(b, ..)), [c]) = z(); }",
        &["(let (tuple (tuple $made@p", "(tuple-struct (path Pair) $made@p", "(slice $made@p", "(assign a@p0", "(assign b@p1", "(assign c@p2"],
    );
    // `..base` has no place in an assignee.
    let l = body("fn f(g: G, s: S) { S { f: g, ..s } = s; }");
    let dump = dump_of(&l);
    assert!(dump.contains("diag FruInDestructuringAssignment"), "{dump}");
}

/// rustc: `if let` and `while let` chains stay `let` expressions in a condition.
#[test]
fn let_chains_stay_lets() {
    assert_body(
        "fn f(o: O) { if let Some(x) = o && x > 1 { g(x); } }",
        &["(if (bin And (let (tuple-struct (path Some) x@p", "o@p0)", "(bin Gt x@p"],
    );
    assert_body("fn f(o: O) { while let Some(x) = o { g(x); } }", &["(loop/while e", "(tail (if (let (tuple-struct (path Some) x@p"]);
}

/// rustc: `let PAT = INIT else { DIVERGE };` stays one statement.
#[test]
fn let_else_stays_a_statement() {
    assert_body("fn f(o: O) { let Some(x) = o else { return; }; }", &["(let (tuple-struct (path Some) x@p", "o@p0", "(else (block", "(return)"]);
}

/// rustc: `match Try::branch(e) { ControlFlow::Break(residual) =>
/// return FromResidual::from_residual(residual), ControlFlow::Continue(val) => val }`.
#[test]
fn question_mark_is_rustcs() {
    assert_body(
        "fn f(r: R) -> R { r?; r }",
        &[
            "(match/try (call #branch r@p0)",
            "(arm (tuple-struct #Break $residual@p",
            "(return (call #from_residual $residual@p",
            "(arm (tuple-struct #Continue $val@p",
            "$val@p",
        ],
    );
    // In a `try` block, `?` breaks to the block.
    assert_body("fn f(r: R) { try { r?; }; }", &["(try e", "(break ->e", "(call #from_residual"]);
}

/// rustc: the range structs with `start`/`end`, `RangeInclusive::new` for `a..=b`.
#[test]
fn ranges_are_rustcs() {
    assert_body(
        "fn f(a: u8, b: u8) { a..b; a..; ..b; ..; a..=b; ..=b; }",
        &[
            "(struct #Range (init $start a@p0) (init $end b@p1))",
            "(struct #RangeFrom (init $start a@p0))",
            "(struct #RangeTo (init $end b@p1))",
            "(struct #RangeFull)",
            "(call #range_inclusive_new a@p0 b@p1)",
            "(struct #RangeToInclusive (init $end b@p1))",
        ],
    );
}

/// `.await` is one node, in an async body only; an async fn's body is its block, its head
/// marked async with the written return type; an async block is a coroutine closure.
#[test]
fn await_is_one_node() {
    assert_body("async fn f() -> u8 { g().await }", &["(body (block e", "(await (call (path g)))"]);
    let l = body("async fn f() -> u8 { g().await }");
    assert!(!dump_of(&l).contains("yield"), "{}", dump_of(&l));
    assert_body("fn f() { async move { g().await }; }", &["(async-block move", "(await (call (path g)))"]);
    // E0728: outside an async body, and in a plain closure inside one, and in a `gen` block.
    for text in ["fn f() { g().await; }", "async fn f() { let c = || g().await; }", "fn f() { gen { g().await }; }"] {
        let l = body(text);
        assert!(l.view().diags().any(|d| d.is_ok_and(|d| d.kind == DiagKind::AwaitOutsideAsync)), "{text}");
        assert!(!dump_of(&l).contains("(await"), "{text}");
    }
}

/// An async fn's body converts with its coroutine output: the written return type, `()` when
/// none is written; `.await` and an async block convert to their nodes.
#[test]
fn an_async_body_converts_with_its_output() {
    use crate::typeck::{Refused, Types, to_typeck};
    use blaze_typeck::{PNode, PTy, PortableTys};
    /// Every written type is `u8`.
    struct U8;
    impl Types for U8 {
        fn ty(&mut self, _: &View<'_>, _: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
            tys.intern(PNode::Uint(blaze_typeck::UintTy::U8)).map_err(Refused::Check)
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
            Ok(0)
        }
    }
    let mut tys = PortableTys::new_in(Alloc::global());
    let u8_ = tys.intern(PNode::Uint(blaze_typeck::UintTy::U8)).expect("u8");
    let none = tys.intern_list(&[]).expect("()");
    let unit = tys.intern(PNode::Tuple(none)).expect("()");
    let cases = [
        ("async fn f() -> u8 { g().await }", Some(u8_)),
        ("async fn f() { g().await; }", Some(unit)),
        ("fn f() { let _ = async { 1 }; }", None),
    ];
    for (text, output) in cases {
        let l = body(text);
        let b = to_typeck(&l.view(), blaze_typeck::ItemKey(0), &mut tys, &mut U8, Alloc::global()).expect(text);
        assert_eq!(b.coroutine, output.map(|output| blaze_typeck::CoroutineKind::Async { output }), "{text}");
        let awaits = b.exprs.iter().filter(|e| matches!(e, blaze_typeck::Expr::Await(_))).count();
        let blocks = b.exprs.iter().filter(|e| matches!(e, blaze_typeck::Expr::AsyncBlock { .. })).count();
        assert_eq!((awaits, blocks), (usize::from(output.is_some()), usize::from(output.is_none())), "{text}");
    }
}

/// rustc's `lower_coroutine_body_with_moved_arguments`: a plain binding is moved by a `let`
/// of the same name, any other pattern through `__argN`.
#[test]
fn async_fn_moves_its_arguments() {
    assert_body(
        "async fn f(a: u8, (b, c): (u8, u8)) {}",
        &["(body a@p", "$made@p", "(let a@p", "(let mut $made@p", "(let (tuple b@p"],
    );
}

// ---------------------------------------------------------------------------------------------
// Scopes

fn binds(v: &View<'_>, name: &str) -> StdVec<PatId> {
    (0..v.pats.len() as u32)
        .map(PatId)
        .filter(|p| matches!(v.pat(*p), Ok(Pat::Bind { name: n, .. }) if v.name_text(n) == Ok(name)))
        .collect()
}

/// Every local expression's binding, in expression order.
fn locals(v: &View<'_>) -> StdVec<PatId> {
    (0..v.exprs.len() as u32)
        .filter_map(|e| match v.expr(ExprId(e)) {
            Ok(Expr::Local { pat, .. }) => Some(pat),
            _ => None,
        })
        .collect()
}

fn of_kind(v: &View<'_>, want: fn(&Expr) -> bool) -> StdVec<ExprId> {
    (0..v.exprs.len() as u32).map(ExprId).filter(|e| v.expr(*e).is_ok_and(|x| want(&x))).collect()
}

#[test]
fn shadowing_finds_the_innermost() {
    let text = "fn f() -> u8 { let x = 1; let x = x; x }";
    let l = body(text);
    let v = l.view();
    let xs = binds(&v, "x");
    assert_eq!(xs.len(), 2);
    assert_eq!(locals(&v), vec![xs[0], xs[1]], "the second let's init sees the first, the tail the second");
    assert!(matches!(v.pat(xs[1]), Ok(Pat::Bind { shadows: Some(s), .. }) if s == xs[0]));
    // A binding is not in scope in its own initializer, and a block's end ends it.
    let text = "fn f(x: u8) -> u8 { { let x = 2; } x }";
    let l = body(text);
    let v = l.view();
    let xs = binds(&v, "x");
    assert_eq!(locals(&v), vec![xs[0]]);
}

#[test]
fn labels_find_their_loop() {
    let text = "fn f() { 'a: loop { loop { break 'a; continue; } } }";
    let l = body(text);
    let v = l.view();
    let loops = of_kind(&v, |e| matches!(e, Expr::Loop { .. }));
    assert_eq!(loops.len(), 2, "post-order: the inner loop first");
    let (inner, outer) = (loops[0], loops[1]);
    let breaks: StdVec<_> = of_kind(&v, |e| matches!(e, Expr::Break { .. }));
    let conts: StdVec<_> = of_kind(&v, |e| matches!(e, Expr::Continue { .. }));
    assert!(matches!(v.expr(breaks[0]), Ok(Expr::Break { target: Some(t), .. }) if t == outer));
    assert!(matches!(v.expr(conts[0]), Ok(Expr::Continue { target: Some(t) }) if t == inner));
    // An undeclared label and a continue to a block are refused, as rustc refuses them.
    let l = body("fn f() { 'a: { continue 'a; } break 'b; }");
    let kinds: StdVec<_> = l.view().diags().filter_map(Result::ok).map(|d| d.kind).collect();
    assert!(kinds.contains(&DiagKind::ContinueToBlock), "{kinds:?}");
    assert!(kinds.contains(&DiagKind::UndeclaredLabel), "{kinds:?}");
    // A closure is a barrier to labels.
    let l = body("fn f() { 'a: loop { let c = || { break 'a; }; } }");
    assert!(l.view().diags().filter_map(Result::ok).any(|d| d.kind == DiagKind::UndeclaredLabel));
}

#[test]
fn closures_see_out_and_shadow_in() {
    let text = "fn f(x: u8, y: u8) { let g = |x: u8| x + y; x; }";
    let l = body(text);
    let v = l.view();
    let xs = binds(&v, "x");
    let ys = binds(&v, "y");
    assert_eq!(xs.len(), 2, "the fn's x and the closure's");
    assert_eq!(locals(&v), vec![xs[1], ys[0], xs[0]]);
}

#[test]
fn a_path_that_is_no_local_stays_a_path() {
    let text = "fn f() { fn g() {} g(); }";
    let l = body(text);
    let v = l.view();
    assert!(locals(&v).is_empty());
    assert_eq!(of_kind(&v, |e| matches!(e, Expr::Path(_))).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Records are functions of their item's text

struct Collect(StdVec<(ContentId, What, StdVec<u8>)>);

impl Sink for Collect {
    fn record(&mut self, _item: &ItemRef<'_>, what: What, key: ContentId, outcome: Outcome<'_, '_>) {
        match outcome {
            Outcome::Record { bytes, .. } => self.0.push((key, what, bytes.to_vec())),
            Outcome::Need(need) => panic!("needs {:?}", need.atoms),
        }
    }

    fn refused(&mut self, item: &ItemRef<'_>, refusal: crate::Refusal<'_>) {
        panic!("{}: {} refused, {}", item.item_text(), refusal.path, refusal.bound);
    }
}

struct NoMacros;

impl Expand for NoMacros {
    fn expand(&mut self, _: &str, _: &str, _: Edition, _: &mut dyn crate::Cfg, _: &mut crate::BodyOut<'_>) -> Expansion {
        Expansion::Unsupported
    }
    fn expand_body(&mut self, _: crate::BodyCall<'_>, _: &mut dyn crate::Cfg, _: &mut crate::BodyOut<'_>) -> Expansion {
        Expansion::Unsupported
    }
    fn derive(
        &mut self,
        _: &str,
        _: &blaze_expand::TokenStream<'_>,
        _: blaze_expand::DeriveFacts,
        _: Edition,
        _: &mut crate::BodyOut<'_>,
    ) -> Expansion {
        Expansion::Unsupported
    }
    fn attr(
        &mut self,
        _: &str,
        _: Option<&blaze_expand::TokenStream<'_>>,
        _: &blaze_expand::TokenStream<'_>,
        _: Edition,
        _: &mut crate::BodyOut<'_>,
    ) -> Expansion {
        Expansion::Unsupported
    }
}

const ITEMS: &[&str] = &[
    "fn a(x: u8) -> u8 { let y = x + 1; y * 2 }\n",
    "struct S { a: [u8; 4], b: Option<u8> }\n",
    "enum E { A = 1, B(u8) }\n",
    "impl S { fn m(&self) -> u8 { for x in self.a { if x > 0 { return x; } } 0 } }\n",
    "async fn b(s: &S) -> u8 { c().await? }\n",
    "const C: [u8; 2 + 1] = [0; 3];\n",
    "trait T { fn t(&self) { let _ = 1..=2; } }\n",
    "mod m { pub fn d() { format_args!(\"{}\", 1); } }\n",
];

const NOISE: &[&str] = &["\n", "// a comment\n", "    ", "/* a block */", "fn noise() {}\n", "use x::y;\n"];

fn records(text: &str) -> StdVec<(ContentId, What, StdVec<u8>)> {
    let mut sink = Collect(StdVec::new());
    lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut NoMacros, &mut sink, Alloc::global());
    sink.0
}

#[test]
fn records_are_equal_under_text_above() {
    blaze_sim::check(Config::cases(128), |tape: &Tape| {
        let mut base = String::new();
        while base.is_empty() || tape.more(3, "another item") {
            base.push_str(ITEMS[tape.index(ITEMS.len(), "item")]);
        }
        let mut above = String::new();
        while tape.more(2, "more text above") {
            above.push_str(NOISE[tape.index(NOISE.len(), "noise")]);
        }
        let before = records(&base);
        let after = records(&format!("{above}{base}"));
        for (key, what, bytes) in &before {
            let Some((_, _, moved)) = after.iter().find(|(k, w, _)| k == key && w == what) else {
                panic!("{what:?} of key {key:?} is missing under {above:?}");
            };
            assert_eq!(moved, bytes, "{what:?} of key {key:?} changed under {above:?}");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// No stack growth

/// The parse's steps with no borrowed message (the input has no error), to cross a thread.
fn steps_of(text: &str) -> StdVec<parser::OutlineStep<'static>> {
    use parser::{OutlineStep, Step};
    let pe = crate::parser_edition(Edition::E2024);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    output
        .steps()
        .map(|s| match s {
            OutlineStep::Step(Step::Token { kind, n_input_tokens }) => OutlineStep::Step(Step::Token { kind, n_input_tokens }),
            OutlineStep::Step(Step::FloatSplit { ends_in_dot }) => OutlineStep::Step(Step::FloatSplit { ends_in_dot }),
            OutlineStep::Step(Step::Enter { kind }) => OutlineStep::Step(Step::Enter { kind }),
            OutlineStep::Step(Step::Exit) => OutlineStep::Step(Step::Exit),
            OutlineStep::Step(Step::Error { msg }) => panic!("a parse error: {msg}"),
            OutlineStep::Lazy { kind, n_input_tokens } => OutlineStep::Lazy { kind, n_input_tokens },
        })
        .collect()
}

/// Lowers and dumps `text` on a thread of 256 KiB of stack, from steps parsed on a thread of
/// enough (the parser recurses on nesting; the lowering and the dump do not).
fn deep(text: &str) -> usize {
    let steps = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn_scoped(s, || steps_of(text))
            .expect("a thread for the parse")
            .join()
            .expect("the parse")
    });
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn_scoped(s, move || {
                let pe = crate::parser_edition(Edition::E2024);
                let lexed = parser::LexedStr::new(pe, text);
                let mut cfg = Unix;
                let mut module = crate::NoModule;
                let mut m = crate::machine::Machine::new(text, crate::Mode::Body, Edition::E2024, &mut cfg, Some(&mut module), Alloc::global());
                crate::feed::feed_steps(&mut m, &lexed, steps.into_iter(), 0);
                let l = m.finish().unwrap_or_else(|_| panic!("no cfg in it"));
                let mut out = String::new();
                crate::dump::dump(&l.view(), Alloc::global(), &mut out).expect("a String takes every write");
                assert!(l.view().unhandled.is_empty());
                l.view().exprs.len()
            })
            .expect("a thread for the lowering")
            .join()
            .expect("the lowering ran in 256 KiB of stack")
    })
}

#[test]
fn a_hundred_thousand_parens_lower_in_constant_stack() {
    let n = 100_000;
    let text = format!("fn f() -> u8 {{ {}1{} }}", "(".repeat(n), ")".repeat(n));
    assert!(deep(&text) > n);
}

#[test]
fn a_hundred_thousand_operators_lower_in_constant_stack() {
    let n = 100_000;
    let text = format!("fn f() -> u8 {{ 1{} }}", " + 1".repeat(n));
    assert!(deep(&text) > 2 * n);
}

// ---------------------------------------------------------------------------------------------
// The planted bug: `for` with no `break` on `None`

/// rustc's invariant of `for`: the `None` arm of the loop's `match` leaves that loop.
fn for_leaves_on_none(v: &View<'_>) -> Result<(), String> {
    for m in 0..v.exprs.len() as u32 {
        let Ok(Expr::Match { arms, source: MatchSource::ForLoop, .. }) = v.expr(ExprId(m)) else { continue };
        if arms.len != 2 {
            continue;
        }
        let rows = v.rows(arms).map_err(|b| format!("{b:?}"))?;
        let none = Arm::decode(rows[0]).map_err(|b| format!("{b:?}"))?;
        let is_none = matches!(v.pat(none.pat), Ok(Pat::Path(p)) if v.path(p).is_ok_and(|p| p.lang == Some(LangItem::OptionNone)));
        if !is_none {
            return Err(format!("e{m}: the first arm is not `None`"));
        }
        let Ok(Expr::Break { value: None, target: Some(t) }) = v.expr(none.body) else {
            return Err(format!("e{m}: the None arm does not leave its loop"));
        };
        let Ok(Expr::Loop { body, source: LoopSource::ForLoop }) = v.expr(t) else {
            return Err(format!("e{m}: the None arm's break leaves e{}, no `for` loop", t.0));
        };
        let holds = match v.expr(body) {
            Ok(Expr::Block { stmts, .. }) => v
                .ids(stmts)
                .is_ok_and(|s| s.first().is_some_and(|s| matches!(v.stmt(StmtId(*s)), Ok(Stmt::Expr { expr, .. }) if expr.0 == m))),
            _ => false,
        };
        if !holds {
            return Err(format!("e{m}: the None arm's break leaves another loop, e{}", t.0));
        }
    }
    Ok(())
}

const PATS: &[&str] = &["x", "(a, b)", "_", "Some(v)", "&x", "mut x"];
const HEADS: &[&str] = &["xs", "0..n", "v.iter()", "[1, 2, 3]"];
const BODIES: &[&str] = &["{}", "{ g(); }", "{ if c { break; } }", "{ continue; }"];

fn draw_for(tape: &Tape) -> String {
    let mut text = String::from("fn f() { ");
    let mut depth = 0;
    loop {
        if tape.coin("label") {
            text.push_str("'l: ");
        }
        text.push_str(&format!("for {} in {} ", PATS[tape.index(PATS.len(), "pat")], HEADS[tape.index(HEADS.len(), "head")]));
        depth += 1;
        if depth < 3 && tape.coin("nest") {
            text.push_str("{ ");
            continue;
        }
        text.push_str(BODIES[tape.index(BODIES.len(), "body")]);
        break;
    }
    for _ in 1..depth {
        text.push_str(" }");
    }
    text.push_str(" }");
    text
}

#[test]
fn the_oracle_catches_a_for_that_drops_the_break_on_none() {
    blaze_sim::control(Config::cases(64), Plant::Clean, Plant::ForDropsBreak, "does not leave its loop", |tape, p| {
        let text = draw_for(tape);
        plant::with(p, || {
            let l = body(&text);
            for_leaves_on_none(&l.view()).map_err(|why| Broke::Oracle(format!("{text}: {why}")))
        })
    });
}

/// `where const { COND }` is its own predicate, its condition an anonymous constant; never
/// dropped (an item would hold without its condition).
#[test]
fn a_where_const_condition_is_a_predicate() {
    let text = "impl<T> Tr for T where const { false } {}";
    let l = signature(text);
    let v = l.view();
    let rows = v.rows(v.list(crate::columns::head::WHERE)).expect("where rows");
    assert_eq!(rows.len(), 1, "{}", dump_of(&l));
    let Ok(WherePred::Const { cond }) = WherePred::decode(rows[0]) else { panic!("{}", dump_of(&l)) };
    let r = v.const_range(cond).expect("its range");
    assert_eq!(v.text_at(r).expect("its text"), "const { false }");
    assert!(dump_of(&l).contains("(const-pred const { false })"), "{}", dump_of(&l));
}

/// A format spec's trait and the arguments its width and precision name.
#[test]
fn format_specs_name_their_trait_and_counts() {
    use crate::typeck::{Count, fmt_spec};
    use blaze_typeck::FmtTrait as T;
    let cases: [(&str, T, Option<Count<'_>>, Option<Count<'_>>); 12] = [
        ("", T::Display, None, None),
        ("?", T::Debug, None, None),
        ("#x?", T::Debug, None, None),
        ("08x", T::LowerHex, None, None),
        (">8", T::Display, None, None),
        ("x>8X", T::UpperHex, None, None),
        ("+.3e", T::LowerExp, None, None),
        ("w$", T::Display, Some(Count::Name("w")), None),
        ("1$.prec$b", T::Binary, Some(Count::Index(1)), Some(Count::Name("prec"))),
        ("0$", T::Display, Some(Count::Index(0)), None),
        (".*", T::Display, None, Some(Count::Star)),
        ("-^#010.2p", T::Pointer, None, None),
    ];
    for (spec, t, width, precision) in cases {
        let s = fmt_spec(spec).unwrap_or_else(|| panic!("{spec:?} is a spec"));
        assert_eq!((s.trait_, s.width, s.precision), (t, width, precision), "{spec:?}");
    }
    for bad in ["y", ".x", "?x"] {
        assert!(fmt_spec(bad).is_none(), "{bad:?}");
    }
}

/// `format_args!` converts to one node: its arguments, and per placeholder its trait and the
/// `usize` counts its spec names.
#[test]
fn format_args_converts_with_its_uses() {
    use crate::typeck::{Refused, Types, to_typeck};
    use blaze_typeck::{FmtTrait as T, FmtUse};
    struct Infer;
    impl Types for Infer {
        fn ty(&mut self, _: &View<'_>, _: TypeId, tys: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            tys.intern(blaze_typeck::PNode::Infer).map_err(Refused::Check)
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
            Ok(0)
        }
    }
    let l = body("fn f(x: u8, w: usize) { format_args!(\"{x:>w$} {:?} {:.*} {0:#x}\", 1, 2, 3); }");
    let mut tys = blaze_typeck::PortableTys::new_in(Alloc::global());
    let b = to_typeck(&l.view(), blaze_typeck::ItemKey(0), &mut tys, &mut Infer, Alloc::global()).expect("it converts");
    // Arguments: 1, 2, 3 written, then w and x captured in the order the template reads them
    // (`w$` before the placeholder's own `x`): w is 3, x is 4.
    let uses: StdVec<FmtUse> = b.fmt_uses.iter().copied().collect();
    let want = [
        FmtUse { arg: 3, trait_: T::Count },
        FmtUse { arg: 4, trait_: T::Display },
        FmtUse { arg: 0, trait_: T::Debug },
        FmtUse { arg: 1, trait_: T::Count },
        FmtUse { arg: 2, trait_: T::Display },
        FmtUse { arg: 0, trait_: T::LowerHex },
    ];
    assert_eq!(uses, want);
    let l = body("fn f(x: u8) { format_args!(\"{x:.*}\", 1, 2); }");
    let mut tys = blaze_typeck::PortableTys::new_in(Alloc::global());
    let refused = to_typeck(&l.view(), blaze_typeck::ItemKey(0), &mut tys, &mut Infer, Alloc::global()).map(|_| ());
    assert!(matches!(refused, Err(Refused::Form { what: "`.*` after a named or numbered argument", .. })), "{refused:?}");
}

/// A doc comment on a field or variant is an attribute, never an anonymous constant (the
/// consteval census found 1118 of them read as one); a real default or discriminant still is.
#[test]
fn a_doc_comment_is_no_anonymous_constant() {
    for text in ["struct S { /// Frames a second.\n rate: u32 }", "enum E { /// doc\n A }"] {
        let l = signature(text);
        assert_eq!(l.view().consts().count(), 0, "{text}");
    }
    for text in ["struct S { /// doc\n rate: u32 = 3 }", "enum E { /// doc\n A = 1 }"] {
        let l = signature(text);
        let v = l.view();
        let consts: StdVec<&str> = v.consts().map(|c| v.text_at(c).expect("the const's text")).collect();
        assert_eq!(consts.len(), 1, "{text}: {consts:?}");
        assert!(!consts[0].contains("doc"), "{text}: {consts:?}");
    }
}

/// `..` in a tuple, tuple-struct or struct pattern is its list's rest, so the conversion takes
/// it (tempo's census: 89 bodies stopped at "a rest pattern" on `S { a, .. }`); a slice's
/// `..` is still refused, by the slice.
#[test]
fn a_rest_in_a_tuple_or_struct_pattern_converts() {
    use crate::typeck::{Refused, Types, to_typeck};
    struct NoTypes;
    impl Types for NoTypes {
        fn ty(&mut self, _: &View<'_>, _: TypeId, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            Err(Refused::Form { what: "a written type", range: Default::default() })
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
            Err(Refused::Form { what: "a repeat length", range: Default::default() })
        }
    }
    let convert = |text: &str| {
        let l = body(text);
        let mut tys = blaze_typeck::PortableTys::new_in(Alloc::global());
        to_typeck(&l.view(), blaze_typeck::ItemKey(0), &mut tys, &mut NoTypes, Alloc::global()).map(|_| ())
    };
    // A slice's `..` converts too, at its list's `rest` (typeck_tests' `slice_patterns_with_rest`).
    for text in [
        "fn f() { let (a, ..) = g(); }",
        "fn f() { let S(a, ..) = g(); }",
        "fn f() { let S { a, .. } = g(); }",
        "fn f() { let [a, ..] = g(); }",
    ] {
        assert_eq!(convert(text), Ok(()), "{text}");
    }
}

// ---------------------------------------------------------------------------------------------
// Identifier patterns

/// A module around the body: `MCall` a unit struct, `None` and `A`/`B` unit variants (the
/// prelude's, a glob's), `LIMIT` a constant, `ST` a static, `Tup` a tuple struct.
struct Module;

impl crate::ValueNames for Module {
    fn value_name(&mut self, name: &str) -> crate::ValueName {
        use crate::ValueName as V;
        match name {
            "MCall" => V::UnitStruct,
            "None" | "A" | "B" => V::UnitVariant,
            "LIMIT" => V::Const,
            "ST" => V::Static,
            "Tup" => V::TupleCtor,
            _ => V::Nothing,
        }
    }
}

fn body_in<'t>(text: &'t str, values: &mut dyn crate::ValueNames) -> Lowered<'static, 't> {
    match lower_body(text, Edition::E2024, &mut Unix, values, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

/// The Reference, "Identifier patterns": "Path patterns take precedence over identifier
/// patterns": an identifier pattern with no `ref`, `mut` or `@` that names a constant, a unit
/// struct or a unit variant in scope is a path pattern, and the arm's use of the name is that
/// path; "It is an error if identifier patterns ... shadow ... statics" and tuple structs or
/// tuple variants (rustc E0530), and a `ref`, `mut` or `@` binding of any of them is one too.
/// Each case: (body, name, binds, uses the binding, PatternShadows errors).
#[test]
fn identifier_patterns_are_paths_where_the_name_is_one() {
    let cases: [(&str, &str, usize, usize, usize); 11] = [
        ("fn f(x: MCall) -> MCall { match x { MCall => MCall } }", "MCall", 0, 0, 0),
        ("fn f(o: Option<u8>) -> u8 { match o { None => 0, Some(v) => v } }", "None", 0, 0, 0),
        ("fn f(e: E) -> u8 { match e { A => 1, B => 2 } }", "B", 0, 0, 0),
        ("fn f(n: u8) -> u8 { match n { LIMIT => 1, _ => 0 } }", "LIMIT", 0, 0, 0),
        ("fn f(n: u8) -> u8 { const LOCAL: u8 = 4; match n { LOCAL => 1, _ => 0 } }", "LOCAL", 0, 0, 0),
        ("fn f(x: &MCall) -> MCall { let MCall = x; MCall }", "MCall", 0, 0, 0),
        ("fn f(MCall: MCall) -> MCall { MCall }", "MCall", 0, 0, 0),
        ("fn f(n: u8) -> u8 { match n { other => other } }", "other", 1, 1, 0),
        ("fn f(x: MCall) -> u8 { match x { ref MCall => 0 } }", "MCall", 1, 0, 1),
        ("fn f(n: u8) -> u8 { match n { ST => 0, _ => 1 } }", "ST", 1, 0, 1),
        ("fn f(t: Tup) -> u8 { match t { Tup => 0 } }", "Tup", 1, 0, 1),
    ];
    let mut wrong = StdVec::new();
    for (text, name, want_binds, want_uses, want_errors) in cases {
        let l = body_in(text, &mut Module);
        let v = l.view();
        let bound = binds(&v, name);
        // A use of a name made a path pattern would point at a non-binding: counted as a use.
        let uses = locals(&v).iter().filter(|p| bound.contains(p) || !matches!(v.pat(**p), Ok(Pat::Bind { .. }))).count();
        let errors = v.diags().filter(|d| matches!(d, Ok(d) if d.kind == DiagKind::PatternShadows)).count();
        let paths = (0..v.pats.len() as u32).filter(|p| matches!(v.pat(PatId(*p)), Ok(Pat::Path(_)))).count();
        let got = (bound.len(), uses, errors);
        std::eprintln!("CASE {text}: binds {}, uses of the binding {uses}, PatternShadows {errors}, path patterns {paths}", bound.len());
        if got != (want_binds, want_uses, want_errors) {
            wrong.push(format!("{text}: got {got:?}, want {:?}", (want_binds, want_uses, want_errors)));
        }
    }
    assert!(wrong.is_empty(), "identifier patterns lowered wrong:\n{}", wrong.join("\n"));
}

/// A module whose `LIMIT` is a constant or not, as a test asks.
struct Limit(bool);

impl crate::ValueNames for Limit {
    fn value_name(&mut self, name: &str) -> crate::ValueName {
        if self.0 && name == "LIMIT" { crate::ValueName::Const } else { crate::ValueName::Nothing }
    }
}

/// Each module answer is in the record's read list: lowered while `LIMIT` names nothing, the
/// body binds it; once `LIMIT` is a constant the read list no longer holds, and the body
/// lowered again holds a path pattern and no binding.
#[test]
fn a_binding_that_becomes_a_constant_is_lowered_again() {
    let text = "fn f(n: u8) -> u8 { match n { LIMIT => LIMIT } }";
    let before = body_in(text, &mut Limit(false));
    let v = before.view();
    assert_eq!(binds(&v, "LIMIT").len(), 1, "no constant: a binding");
    assert_eq!(crate::value_reads_hold(&v, &mut Limit(false)), Ok(true), "the same world: the record holds");
    assert_eq!(crate::value_reads_hold(&v, &mut Limit(true)), Ok(false), "`LIMIT` became a constant: lowered again");
    let after = body_in(text, &mut Limit(true));
    let v = after.view();
    assert_eq!(binds(&v, "LIMIT").len(), 0, "a constant: no binding");
    let paths = (0..v.pats.len() as u32).filter(|p| matches!(v.pat(PatId(*p)), Ok(Pat::Path(_)))).count();
    assert_eq!(paths, 1, "a path pattern");
    // `match n` is the one local left; the arm's `LIMIT` is the constant's path.
    assert!(locals(&v).iter().all(|p| matches!(v.pat(*p), Ok(Pat::Bind { .. }))), "every local is a binding");
    assert_eq!(locals(&v).len(), 1, "`n` alone: the arm's `LIMIT` is no local");
    assert_eq!(crate::value_reads_hold(&v, &mut Limit(true)), Ok(true));
}
