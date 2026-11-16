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

fn body(text: &str) -> Lowered<'static> {
    match lower_body(text, Edition::E2024, &mut Unix, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

fn signature(text: &str) -> Lowered<'static> {
    match lower_signature(text, Edition::E2024, &mut Unix, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

fn dump_of(text: &str, l: &Lowered<'_>) -> String {
    let mut out = String::new();
    crate::dump::dump(text, &l.view(), Alloc::global(), &mut out).expect("a String takes every write");
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
    let dump = dump_of(text, &l);
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
        let dump = dump_of(text, &l);
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
        let dump = dump_of(text, &l);
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

/// rustc's `make_lowered_await`.
#[test]
fn await_is_rustcs() {
    assert_body(
        "async fn f() -> u8 { g().await }",
        &[
            "(async-fn move resume=p",
            "(match/await (call #into_future (call (path g)))",
            "(arm mut $__awaitee@p",
            "(loop e",
            "(match/await (unsafe e",
            "(call #poll (call #new_unchecked (ref-mut $__awaitee@p",
            "(call #get_context $_task_context@p",
            "(arm (tuple-struct #Ready $result@p",
            "(break ->e",
            "(arm #Pending (block e",
            "(assign $_task_context@p",
            "(yield (tuple))",
        ],
    );
    // E0728: no desugaring outside a coroutine.
    let l = body("fn f() { g().await; }");
    assert!(l.view().diags().any(|d| d.is_ok_and(|d| d.kind == DiagKind::AwaitOutsideAsync)));
    // A plain closure in an async body is no coroutine.
    let l = body("async fn f() { let c = || g().await; }");
    assert!(l.view().diags().any(|d| d.is_ok_and(|d| d.kind == DiagKind::AwaitOutsideAsync)));
}

/// rustc's `lower_coroutine_body_with_moved_arguments`: a plain binding is moved by a `let`
/// of the same name, any other pattern through `__argN`.
#[test]
fn async_fn_moves_its_arguments() {
    assert_body(
        "async fn f(a: u8, (b, c): (u8, u8)) {}",
        &["(body a@p", "$made@p", "(async-fn move", "(let a@p", "(let mut $made@p", "(let (tuple b@p"],
    );
}

// ---------------------------------------------------------------------------------------------
// Scopes

fn binds(v: &View<'_>, text: &str, name: &str) -> StdVec<PatId> {
    (0..v.pats.len() as u32)
        .map(PatId)
        .filter(|p| matches!(v.pat(*p), Ok(Pat::Bind { name: n, .. }) if v.name_text(text, n) == name))
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
    let xs = binds(&v, text, "x");
    assert_eq!(xs.len(), 2);
    assert_eq!(locals(&v), vec![xs[0], xs[1]], "the second let's init sees the first, the tail the second");
    assert!(matches!(v.pat(xs[1]), Ok(Pat::Bind { shadows: Some(s), .. }) if s == xs[0]));
    // A binding is not in scope in its own initializer, and a block's end ends it.
    let text = "fn f(x: u8) -> u8 { { let x = 2; } x }";
    let l = body(text);
    let v = l.view();
    let xs = binds(&v, text, "x");
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
    let xs = binds(&v, text, "x");
    let ys = binds(&v, text, "y");
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
}

struct NoMacros;

impl Expand for NoMacros {
    fn expand(&mut self, _: &str, _: &str, _: Edition, _: &mut blaze_own::AString<'_>) -> Expansion {
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
    lower_file(text, Edition::E2024, &mut Unix, &mut NoMacros, &mut sink, Alloc::global());
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
                let mut m = crate::machine::Machine::new(text, crate::Mode::Body, Edition::E2024, &mut cfg, Alloc::global());
                crate::feed::feed_steps(&mut m, &lexed, steps.into_iter(), 0);
                let l = m.finish().unwrap_or_else(|_| panic!("no cfg in it"));
                let mut out = String::new();
                crate::dump::dump(text, &l.view(), Alloc::global(), &mut out).expect("a String takes every write");
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
