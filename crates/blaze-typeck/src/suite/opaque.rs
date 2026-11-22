//! Return-position `impl Trait` seen by a caller, as the front end answers it (crates/blaze-cli/
//! tests/ladder/README.md, "Heads lane: types and bounds"): one key per occurrence; the
//! function's output is `Opaque(k, the generics in scope)`; `Predicates(k)` holds its bounds
//! with that opaque type as each clause's `Self`; the hidden type is not the caller's.
//!
//! The Reference, "Impl trait", "Abstract return types": "a caller of the function is not
//! permitted to know the concrete type"; "the caller can only use the methods declared by the
//! trait bound". So a caller proves `Tr` of the opaque type from its bounds, and normalizes
//! `<Opaque as Tr>::Out` through its bounds, at the call's own arguments for the generics.
//!
//! ```text
//! trait Tr { type Out; }
//! fn mk<T>(t: T) -> impl Tr<Out = T> { .. }
//! fn need<X: Tr>(x: X) -> <X as Tr>::Out { .. }
//! fn user() -> u8 { need(mk(1u8)) }     // clean
//! fn user16() -> u16 { need(mk(1u8)) }  // E0308: the bound says `Out = u8`
//! ```

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["Out"]);
    let p0 = fx.param(0);
    let o = fx.opaque_key("mk::{opaque#0}");
    let ps = fx.list(&[p0]);
    let opaque = fx.ty(PNode::Opaque(o, ps));
    let is_tr = fx.bound(tr, &[opaque]);
    let out_is_t = fx.proj(tr, &[opaque], "Out", p0);
    // `Sized` is among an opaque's bounds unless `?Sized` is written, as the front end's heads
    // file it (front2/heads.rs `fn_head`): without it `need`'s implied `X: Sized` is E0277.
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_tr, out_is_t, is_sized]);
    fx.func("mk", 1, &[p0], opaque, Vec::new());
    let out = fx.alias(tr, "Out", &[p0]);
    let x_tr = fx.bound(tr, &[p0]);
    fx.func("need", 1, &[p0], out, std::vec![x_tr]);
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let user = fx.func("user", 0, &[], u8_, Vec::new());
    let user16 = fx.func("user16", 0, &[], u16_, Vec::new());
    fx.finish();
    (fx, user, user16)
}

/// `need(mk(1u8))`: expr 4 is the outer call; the root block is expr 5.
fn body() -> (Bb, ExprId) {
    let mut b = Bb::new();
    let need = b.path(&["need"]);
    let mk = b.path(&["mk"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let inner = b.call(mk, &[one]);
    let outer = b.call(need, &[inner]);
    let root = b.block(&[], Some(outer));
    (b, root)
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// The caller proves `impl Tr<Out = T>: Tr` and normalizes `Out` from the bounds at `T = u8`.
/// Without the generics substituted (`Out` left `T`, a parameter of `mk`), the call would not
/// be `u8`.
#[test]
fn a_caller_uses_an_opaque_types_bounds_at_its_arguments() {
    let (fx, user, _) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(3), "impl mk::{opaque#0}"), (ExprId(4), "u8"), (ExprId(5), "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The bound fixes `Out = u8` at the call: a `u16` return is E0308, so the caller does know
/// the bound's term and no more.
#[test]
fn an_opaque_types_projection_is_its_bound_and_nothing_else() {
    let (fx, _, user16) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, user16, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(4), "u8")], pats: &[], diags: &[(DiagCode::Mismatch, ExprId(4))] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// A generic fn's opaque at two instances in one body: `{ let a = need(mk(1u8)); need(mk(1u16)) }`
/// under `-> u16`. The Reference, "Impl trait", [type.impl-trait.generic-captures]: "For this
/// concrete type to use a generic parameter, that generic parameter must be captured by the
/// abstract type": `mk::<u8>`'s opaque and `mk::<u16>`'s are two types, each with its own `Out`.
#[test]
fn a_generic_opaque_at_two_instances_is_two_types() {
    let (fx, _, user16) = world();
    let mut b = Bb::new();
    let need = b.path(&["need"]);
    let mk = b.path(&["mk"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let inner = b.call(mk, &[one]);
    let first = b.call(need, &[inner]);
    let a = b.bind("a");
    let s = b.let_(a, None, first);
    let need2 = b.path(&["need"]);
    let mk2 = b.path(&["mk"]);
    let two = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let inner2 = b.call(mk2, &[two]);
    let second = b.call(need2, &[inner2]);
    let root = b.block(&[s], Some(second));
    let c = check_body(&fx, &b, user16, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(first, "u8"), (second, "u16"), (root, "u16")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The DEFINING side (rules/opaque_def.rs): `trait Tr2 {}`, `impl Tr2 for u8 {}` and
/// `fn d() -> impl Tr2`, the opaque's bounds `Tr2` and `Sized`.
fn defining_world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr2", 1, Vec::new(), &[]);
    let u8_ = fx.u8();
    fx.imp("impl Tr2 for u8", 0, u8_, Some((tr, &[])), &[], Vec::new());
    let o = fx.opaque_key("d::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_tr = fx.bound(tr, &[opaque]);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_tr, is_sized]);
    let d = fx.func("d", 0, &[], opaque, Vec::new());
    fx.finish();
    (fx, d)
}

/// `{ return 1u8; TAIL }` checked as `d`'s body.
fn defining_body(tail: fn(&mut Bb) -> ExprId) -> (Fx, Bb, ExprId, crate::columns::Checked<'static>) {
    let (fx, d) = defining_world();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let r = b.ret(Some(one));
    let s = b.semi(r);
    let t = tail(&mut b);
    let root = b.block(&[s], Some(t));
    let c = check_body(&fx, &b, d, root, Plant::None, InferPlant::None);
    (fx, b, t, c)
}

/// [type.impl-trait.return.constraint-body]: "Each possible return value from the function must
/// resolve to the same concrete type". `{ return 1u8; 2u8 }`: two return points of one type, the
/// hidden type `u8`, which has `Tr2`: clean. Without the rule the body's `u8` is compared with
/// the opaque itself, E0308.
#[test]
fn a_defining_body_with_two_returns_of_one_type_is_clean() {
    let (fx, b, t, c) = defining_body(|b| b.int(Some(IntSuffix::Uint(UintTy::U8))));
    let want = Want { exprs: &[(t, "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The planted bug: the output kept as the opaque itself (the state before rules/opaque_def.rs)
/// makes `{ 1u8 }` fail: `u8` is not the opaque type itself, E0308 in the body, so the test
/// above checks the rule. (While the checker made an opaque a NON-rigid alias the pair was
/// deferred and ended as "DiagWithoutPlace { code: Unsatisfied, name: None }", run of
/// 2026-11-16; an opaque is rigid now: infer/mod.rs `mk`.)
#[test]
fn control_opaque_not_opened() {
    let (fx, d) = defining_world();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let root = b.block(&[], Some(one));
    let c = check_body(&fx, &b, d, root, Plant::OpaqueNotOpened, InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Mismatch), "the unopened opaque was not caught: {:?}", c.diags);
}

/// The same sentence: `{ return 1u8; "a" }` returns two types: E0308 at the second.
#[test]
fn a_defining_body_with_two_returned_types_is_a_mismatch() {
    let (fx, b, t, c) = defining_body(|b| b.str_());
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::Mismatch, t)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// "the function chooses the return type, but only promises that it will implement Trait":
/// `{ "a" }` makes `&str` the hidden type, which lacks `Tr2`: E0277 at the body, naming `Tr2`.
#[test]
fn a_hidden_type_that_lacks_a_bound_is_unsatisfied() {
    let (fx, d) = defining_world();
    let mut b = Bb::new();
    let a = b.str_();
    let root = b.block(&[], Some(a));
    let c = check_body(&fx, &b, d, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::Unsatisfied, root)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `use_opt_next` of tests/front2_bounds.rs reduced: `enum Option<T> { None, Some(T) }`,
/// `trait Iterator { type Item; fn next(&mut self) -> Option<Self::Item>; }`,
/// `fn opt_iter() -> Option<impl Iterator<Item = u8>>` and `fn plain() -> impl Iterator<Item = u8>`
/// (one opaque key each, bounds `Iterator`, `Item == u8`, `Sized`), checked as
/// `fn user() -> Option<u8>`.
fn next_world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    let iterator = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    let item = fx.alias(iterator, "Item", &[p0]);
    let opt_item = fx.adt(option, &[item]);
    let m_self = fx.r(crate::ids::Mutbl::Mut, p0);
    let next = fx.trait_fn(iterator, "next", true, &[m_self], opt_item);
    fx.use_trait(iterator);
    fx.lang_def(LangItem::IteratorNext, crate::world::Def::Fn(next));
    fx.lang_def(LangItem::OptionNone, crate::world::Def::Ctor { adt: option, variant: 0, kind: crate::world::CtorKind::Unit });
    fx.lang_def(LangItem::OptionSome, crate::world::Def::Ctor { adt: option, variant: 1, kind: crate::world::CtorKind::Tuple });
    let u8_ = fx.u8();
    let none = fx.list(&[]);
    for name in ["opt_iter", "plain"] {
        let o = fx.opaque_key(&format!("{name}::{{opaque#0}}"));
        let opaque = fx.ty(PNode::Opaque(o, none));
        let is_iter = fx.bound(iterator, &[opaque]);
        let item_u8 = fx.proj(iterator, &[opaque], "Item", u8_);
        let is_sized = fx.bound(sized, &[opaque]);
        fx.set_opaque_bounds(o, std::vec![is_iter, item_u8, is_sized]);
        let out = if name == "plain" { opaque } else { fx.adt(option, &[opaque]) };
        fx.func(name, 0, &[], out, Vec::new());
    }
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let is_send = fx.bound(send, &[p0]);
    let unit = fx.unit();
    fx.func("need_send", 1, &[p0], unit, std::vec![is_send]);
    let opt_u8 = fx.adt(option, &[u8_]);
    let user = fx.func("user", 0, &[], opt_u8, Vec::new());
    fx.finish();
    (fx, user)
}

/// "the caller may only use the methods declared by the specified Trait": `next` through the
/// opaque's `Iterator` bound, the receiver bound by a pattern inside `Option`:
/// `match opt_iter() { Some(i) => i.next(), n => None }` is `Option<u8>`.
#[test]
fn a_method_on_an_opaque_bound_by_a_pattern() {
    let (fx, user) = next_world();
    let mut b = Bb::new();
    let f = b.path(&["opt_iter"]);
    let call = b.call(f, &[]);
    let i = b.bind("i");
    let some = b.lang_variant_pat(LangItem::OptionSome, &[i]);
    let recv = b.local(i);
    let next = b.method(recv, "next", &[]);
    let n = b.bind("n");
    let none = b.lang(LangItem::OptionNone);
    let m = b.match_(call, &[(some, next), (n, none)]);
    let root = b.block(&[], Some(m));
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(next, "Option<u8>"), (root, "Option<u8>")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Send` of an opaque whose bounds do not say it: an auto trait "leaks" from the hidden type,
/// which the world does not record yet, so `{ need_send(plain()); None }` is the typed gap
/// naming that, never E0283 "cannot satisfy" (what it was before rules/opaque_def.rs's gap).
#[test]
#[should_panic(expected = "OpaqueHiddenUnknown")]
fn send_of_an_opaque_is_a_gap_until_its_hidden_type_is_recorded() {
    let (fx, user) = next_world();
    let mut b = Bb::new();
    let f = b.path(&["plain"]);
    let call = b.call(f, &[]);
    let need = b.path(&["need_send"]);
    let sent = b.call(need, &[call]);
    let s = b.semi(sent);
    let none = b.lang(LangItem::OptionNone);
    let root = b.block(&[s], Some(none));
    let _ = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
}

/// The form that checks clean in tests/front2_bounds.rs (`let mut i = evens(); i.next()`), in
/// the same world: `{ let i = plain(); i.next() }`.
#[test]
fn a_method_on_an_opaque_bound_by_let() {
    let (fx, user) = next_world();
    let mut b = Bb::new();
    let f = b.path(&["plain"]);
    let call = b.call(f, &[]);
    let i = b.bind("i");
    let s = b.let_(i, None, call);
    let recv = b.local(i);
    let next = b.method(recv, "next", &[]);
    let root = b.block(&[s], Some(next));
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(next, "Option<u8>")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// [type.impl-trait.return.constraint-body]: "Each possible return value from the function must
/// resolve to the same concrete type": that ONE type is the record's, per opaque key
/// (`Checked::hidden`): `{ return 1u8; 2u8 }` as `fn d() -> impl Tr2` records one row, `u8`.
/// Without writeback's rows the list is empty and this fails at "one hidden type".
/// Should print: "test suite::opaque::the_hidden_type_is_recorded_per_opaque ... ok".
#[test]
fn the_hidden_type_is_recorded_per_opaque() {
    let (fx, _, _, c) = defining_body(|b| b.int(Some(IntSuffix::Uint(UintTy::U8))));
    assert_eq!(c.hidden.len(), 1, "one hidden type: {:?}", c.hidden);
    assert_eq!(crate::fixture::show(&fx, c.types.view(), c.hidden[0].1), "u8");
}

/// A caller's body defines nothing: `fn user() -> Option<u8>` calling `plain()` records no
/// hidden type (the opaque stays opaque to it: "a caller of the function is not permitted to
/// know the concrete type").
#[test]
fn a_caller_records_no_hidden_type() {
    let (fx, user) = next_world();
    let mut b = Bb::new();
    let f = b.path(&["plain"]);
    let call = b.call(f, &[]);
    let i = b.bind("i");
    let s = b.let_(i, None, call);
    let recv = b.local(i);
    let next = b.method(recv, "next", &[]);
    let root = b.block(&[s], Some(next));
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    assert!(c.hidden.is_empty(), "{:?}", c.hidden);
}

/// `trait Future { type Output; }` (the lang item) and `async fn NAME(x: u8) -> u8`, as the
/// front end files an async fn (front2/heads.rs): the output is opaque 0 with the bounds
/// `Future`, `Output == u8` and `Sized`.
fn async_world(name: &str) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let u8_ = fx.u8();
    let o = fx.opaque_key(&format!("{name}::{{opaque#0}}"));
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_future = fx.bound(future, &[opaque]);
    let out_u8 = fx.proj(future, &[opaque], "Output", u8_);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_future, out_u8, is_sized]);
    let f = fx.func(name, 0, &[u8_], opaque, Vec::new());
    fx.finish();
    (fx, f)
}

/// The Reference, "Async functions": "async fn example<'a>(x: &'a str) -> usize { .. } is
/// roughly equivalent to: fn example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a {
/// async move { .. } }". `async fn af(x: u8) -> u8 { x }` in the form blaze-lower makes (the
/// root is the `async move` block): the block's output is the written `u8`, its future is the
/// opaque's hidden type and has `Future<Output = u8>`: clean, one hidden type recorded.
/// Without the rule (control.rs `start_body` checking the root against the written type) the
/// future meets `u8`: E0308 at the root.
/// Should print: "test suite::opaque::an_async_fn_checks_as_its_async_move_block ... ok".
#[test]
fn an_async_fn_checks_as_its_async_move_block() {
    let (fx, af) = async_world("af");
    let mut b = Bb::new();
    let x = b.param("x");
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(u8_);
    let e0 = b.local(x);
    let inner = b.block(&[], Some(e0));
    let root = b.async_block(inner, crate::body::Capture::Move);
    let c = check_body(&fx, &b, af, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(e0, "u8"), (inner, "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
    assert_eq!(c.hidden.len(), 1, "the future is the opaque's hidden type: {:?}", c.hidden);
}

/// The same sentence: `async fn h(x: u8) -> u8 { "a" }` is `fn h(x: u8) -> impl Future<Output =
/// u8> { async move { "a" } }` whose block must return the written `u8`: ONE finding, E0308 at
/// the string (never a failed `Future<Output = u8>` bound at the block, and never two).
#[test]
fn an_async_fn_returning_the_wrong_type_is_one_mismatch_at_the_value() {
    let (fx, h) = async_world("h");
    let mut b = Bb::new();
    let _x = b.param("x");
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(u8_);
    let a = b.str_();
    let inner = b.block(&[], Some(a));
    let root = b.async_block(inner, crate::body::Capture::Move);
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    let [diag] = c.diags.as_slice() else { panic!("one error at the returned string: {:?}", c.diags) };
    assert_eq!(diag.code, DiagCode::Mismatch);
    assert_eq!(diag.node, Some(crate::diag::Node::Expr(a)));
    assert_eq!(diag.at, b.view(h, root).expr_range(a));
    let types = c.types.view();
    assert_eq!(crate::fixture::show(&fx, types, diag.expected.expect("written return type")), "u8");
    assert_eq!(crate::fixture::show(&fx, types, diag.found.expect("returned value")), "&str");
}
