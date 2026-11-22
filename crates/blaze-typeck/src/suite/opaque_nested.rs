//! An auto trait of an opaque type reached INSIDE a goal, not as its self type
//! (rules/opaque_def.rs `opaque_ambiguity_gap`).
//!
//! The rung s9-app-03 (std census 1, class C2) stopped in `main` with E0283 "cannot satisfy
//! `Pin<Box<Opaque>>: CoerceUnsized<Pin<Box<dyn Future<Output = ()> + Send>>>`" at
//! `spawn(Box::pin(worker("a", 3, 2)))`, `worker` an `async fn`. The goal reaches
//! `Opaque: Send` through `Pin`'s and `Box`'s `CoerceUnsized` impls (library/core/src/pin.rs,
//! library/alloc/src/boxed.rs) and the unsizing to `dyn Future + Send` (the Reference, "Type
//! coercions", "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`"); that
//! leaf is ambiguous while the world has not given the opaque's hidden type (blaze-solve
//! structural.rs: "ambiguous until it does"). The gap that makes front2 define the opaque and
//! check again was raised only when the ambiguous goal's SELF type is the opaque, so this goal
//! was reported as E0283 instead. The Reference, "Impl trait", "Auto trait leakage": the auto
//! traits of the hidden type leak through the opaque type wherever it is, at any depth.
//!
//! The toy world:
//!
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "send"] auto trait Send {}
//! trait Iterator { type Item; }
//! enum Option<T> { None, Some(T) }
//! fn plain() -> impl Iterator<Item = u8>;   // its hidden type not given by the world
//! fn need_send<T: Send>(t: T);
//! fn user() { need_send(Option::Some(plain())) }
//! ```
//!
//! `Option<Opaque>: Send` holds when `Opaque: Send` does (auto traits are structural), which
//! leaks from the hidden type: the typed gap `OpaqueHiddenUnknown` naming `plain`'s opaque,
//! never E0283. Without the fix the check finishes with one CannotSatisfy finding and the test
//! panics with no `OpaqueHiddenUnknown` in its message.
//!
//! Prints `test suite::opaque_nested::send_of_an_opaque_inside_an_adt_is_the_opaques_gap ... ok`.

use std::format;
use std::vec::Vec;

use super::check_body;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    let iterator = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    let u8_ = fx.u8();
    let none = fx.list(&[]);
    let o = fx.opaque_key(&format!("{}::{{opaque#0}}", "plain"));
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_iter = fx.bound(iterator, &[opaque]);
    let item_u8 = fx.proj(iterator, &[opaque], "Item", u8_);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_iter, item_u8, is_sized]);
    fx.func("plain", 0, &[], opaque, Vec::new());
    fx.lang_def(LangItem::OptionNone, crate::world::Def::Ctor { adt: option, variant: 0, kind: crate::world::CtorKind::Unit });
    fx.lang_def(LangItem::OptionSome, crate::world::Def::Ctor { adt: option, variant: 1, kind: crate::world::CtorKind::Tuple });
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let is_send = fx.bound(send, &[p0]);
    let unit = fx.unit();
    fx.func("need_send", 1, &[p0], unit, std::vec![is_send]);
    let user = fx.func("user", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, user)
}

/// `need_send(Option::Some(plain()))`: the gap names the opaque, as `need_send(plain())` does
/// (suite/opaque.rs `send_of_an_opaque_is_a_gap_until_its_hidden_type_is_recorded`).
#[test]
#[should_panic(expected = "OpaqueHiddenUnknown")]
fn send_of_an_opaque_inside_an_adt_is_the_opaques_gap() {
    let (fx, user) = world();
    let mut b = Bb::new();
    let f = b.path(&["plain"]);
    let call = b.call(f, &[]);
    let some = b.path(&["Option", "Some"]);
    let wrapped = b.call(some, &[call]);
    let need = b.path(&["need_send"]);
    let sent = b.call(need, &[wrapped]);
    let root = b.block(&[], Some(sent));
    let _ = check_body(&fx, &b, user, root, Plant::None, InferPlant::None);
}
