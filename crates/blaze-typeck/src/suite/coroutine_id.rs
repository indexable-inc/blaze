//! A caller's auto-trait goal on an `async fn`'s opaque type, read from the hidden type the
//! defining body recorded (port27/tempo-run/ASYNC.md P3, D-A3).
//!
//! The Reference, "Impl trait", "Abstract return types": "a caller of the function is not
//! permitted to know the concrete type"; and "Auto trait leakage": "Auto traits are
//! implemented for the opaque type if they are implemented for the hidden type". The
//! Reference, "Async functions": `async fn f(x: A) -> T { body }` "is roughly equivalent to"
//! `fn f(x: A) -> impl Future<Output = T> { async move { body } }`, so the hidden type is the
//! async block's future, whose auto traits are those of what it holds (the Reference,
//! "Async blocks", "Capture modes": an `async move` block captures by value), and the library
//! (core/src/marker.rs): "`Rc` is not `Send`" (`impl<T: ?Sized, A: Allocator> !Send for
//! Rc<T, A>` in alloc/src/rc.rs).
//!
//! The world here is the defining body's record as another body reads it: the opaque's hidden
//! type is `PNode::Coroutine { owner: af, ordinal: 0, args: [] }` (the only async block of
//! `af`'s body, at `af`'s own, empty, parameters) and the coroutine's facts
//! (`World::coroutine`) are its output and its interior (the tuple of its captures by value).
//!
//! ```text
//! auto trait Send {}
//! trait Future { type Output; }
//! struct Rc<T>(T);  impl<T> !Send for Rc<T> {}
//! fn need_send<T: Send>(t: T) {}
//! async fn af(x: u32) -> u32 { x }          // hidden: async move { x }, interior (u32,)
//! async fn rf(x: Rc<u32>) -> u32 { .. }     // hidden: async move { .. }, interior (Rc<u32>,)
//! fn user() { need_send(af(1)); }           // clean
//! fn user_rc() { need_send(rf(..)); }       // E0277: `Rc<u32>` is not `Send`
//! ```

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world above. `rc_capture` picks the async fn the caller calls: `false` is `af(u32)`
/// whose future holds a `u32`, `true` is `rf(Rc<u32>)` whose future holds an `Rc<u32>`.
/// Returns the world, the caller `user`, and the async fn it calls.
fn world(rc_capture: bool) -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let u32_ = fx.u32();
    let p0 = fx.param(0);
    let rc = fx.tuple_strukt("Rc", 1, &[p0]);
    let rc_p0 = fx.adt(rc, &[p0]);
    let not_send = fx.imp("impl !Send for Rc<T>", 1, rc_p0, Some((send, &[])), &[], Vec::new());
    fx.set_polarity(not_send, crate::world::ImplPolarity::Negative);
    let rc_u32 = fx.adt(rc, &[u32_]);
    let (name, input) = if rc_capture { ("rf", rc_u32) } else { ("af", u32_) };
    let o = fx.opaque_key(&format!("{name}::{{opaque#0}}"));
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_future = fx.bound(future, &[opaque]);
    let out_u32 = fx.proj(future, &[opaque], "Output", u32_);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_future, out_u32, is_sized]);
    let f = fx.func(name, 0, &[input], opaque, Vec::new());
    // The defining body's record: the hidden type by identity, and that coroutine's facts
    // (output `u32`, interior: the one capture by value, `async move` over the parameter).
    let hidden = fx.ty(PNode::Coroutine { owner: f, ordinal: 0, args: none });
    let interior = fx.tuple(&[input]);
    fx.hidden.push((o, hidden));
    fx.coroutines.push((f, 0, u32_, interior));
    let is_send = fx.bound(send, &[p0]);
    let unit = fx.unit();
    fx.func("need_send", 1, &[p0], unit, std::vec![is_send]);
    let user = fx.func("user", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, user, f)
}

/// `{ need_send(NAME(ARG)); }` as `user`'s body; `ARG` is `1u32` for `af` and a path to a
/// local-free value for `rf` (`rf`'s input is only read for its type: the call's argument is
/// `Rc(1u32)`).
fn caller(fx: &Fx, name: &str, rc_capture: bool) -> (crate::fixture::Bb, crate::ids::ExprId, crate::ids::ExprId) {
    let _ = fx;
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let arg = if rc_capture {
        let ctor = b.path(&["Rc"]);
        b.call(ctor, &[one])
    } else {
        one
    };
    let f = b.path(&[name]);
    let call = b.call(f, &[arg]);
    let need = b.path(&["need_send"]);
    let sent = b.call(need, &[call]);
    let s = b.semi(sent);
    let root = b.block(&[s], None);
    (b, root, sent)
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// The caller's `Send` goal on `af`'s opaque holds: the hidden type is `af`'s coroutine 0,
/// whose interior `(u32,)` is `Send` ("Auto trait leakage"). Without the hidden type answered
/// by identity (the world's `opaque_hidden` `None`, as before P3) the goal is ambiguous and the
/// check stops at the gap `Unported("an auto trait of an opaque type: its hidden type is not
/// recorded yet")`; without `PNode::Coroutine` lowered through `World::coroutine` the record is
/// refused as a body type in a record (`BodyTypeInRecord`).
/// Should print: "test suite::coroutine_id::a_callers_send_goal_on_an_async_fn_leaks_from_its_future ... ok".
#[test]
fn a_callers_send_goal_on_an_async_fn_leaks_from_its_future() {
    let (fx, user, _) = world(false);
    let (b, root, sent) = caller(&fx, "af", false);
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(sent, "()")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The same caller over `rf`, whose future holds an `Rc<u32>` by value: the hidden type's
/// interior is not `Send` (`impl<T> !Send for Rc<T>`), so the caller's `Send` goal on the
/// opaque fails: E0277 (`DiagCode::Unsatisfied`), never clean and never the gap. The control
/// for the first test: a world that answered every auto trait of an opaque as holding (an
/// assumed answer) passes the first test and fails this one.
/// Should print: "test suite::coroutine_id::an_rc_captured_by_value_makes_the_callers_send_fail ... ok".
#[test]
fn an_rc_captured_by_value_makes_the_callers_send_fail() {
    let (fx, user, _) = world(true);
    let (b, root, _) = caller(&fx, "rf", true);
    let c = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
    let unsatisfied = c.diags.iter().filter(|d| d.code == DiagCode::Unsatisfied).count();
    assert_eq!(unsatisfied, 1, "one E0277 for `Rc<u32>: Send` through the opaque's hidden type: {:?}", c.diags);
}
