//! A body path through a type alias whose aliased type is a projection (`Def::Alias`,
//! rules/alias_path.rs). std's `type ValidRawFd = core::num::niche_types::NotAllOnes<RawFd>;`
//! is `<i32 as NotAllOnesHelper>::Type`, and `OwnedFd::from_raw_fd` calls `ValidRawFd::new(fd)`;
//! before this rule the world could only answer such a path with the gap naming the alias.
//!
//! ```text
//! struct S;   impl S { fn mk() -> u16 { .. } }
//! trait H { type Ty; }   impl H for u8 { type Ty = S; }
//! type W = <u8 as H>::Ty;
//! fn f() { W::mk(); }      // `u16`: `mk` of `S`, the projection normalized
//! fn f() { W::nope(); }    // E0599 naming `nope`: looked up on `S`, which has none
//! ```
//!
//! A checker that read the next segment on the alias's item instead of on its normalized type
//! finds no `mk` (E0599 on the clean test); one that never asked the alias's definition stops
//! with the world's `Missing`.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Name};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world above and the owner `f`.
fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u8_, u16_, unit) = (fx.u8(), fx.u16(), fx.unit());
    let s = fx.strukt("S", 0, &[]);
    let s_ty = fx.adt(s, &[]);
    let inherent = fx.imp("impl S", 0, s_ty, None, &[], Vec::new());
    fx.impl_fn(inherent, "mk", false, &[], u16_);
    let h = fx.tr("H", 1, Vec::new(), &["Ty"]);
    fx.imp("impl H for u8", 0, u8_, Some((h, &[])), &[("Ty", s_ty)], Vec::new());
    let projected = fx.alias(h, "Ty", &[u8_]);
    fx.type_alias_item("W", 0, projected);
    let f = fx.func("f", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, f)
}

/// `W::mk()` is `S::mk()`: a `u16`, no diagnostic.
/// Expected: `test suite::type_alias_paths::a_path_through_a_projection_alias_is_the_normalized_type ... ok`.
#[test]
fn a_path_through_a_projection_alias_is_the_normalized_type() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let p = b.path(&["W", "mk"]);
    let v = b.call(p, &[]);
    let s = b.semi(v);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    let d: Vec<String> = differences(&fx, &b, &c, &Want { exprs: &[(v, "u16")], pats: &[], diags: &[] })
        .into_iter()
        .filter(|l| !l.contains("no expectation written"))
        .collect();
    assert!(d.is_empty(), "`W::mk()`:\n{}", d.join("\n"));
}

/// `W::nope()`: E0599 naming `nope`, the lookup made on `S`.
/// Expected: `test suite::type_alias_paths::a_missing_item_through_a_projection_alias_is_named ... ok`.
#[test]
fn a_missing_item_through_a_projection_alias_is_named() {
    let (fx, f) = world();
    let mut b = Bb::new();
    let p = b.path(&["W", "nope"]);
    let v = b.call(p, &[]);
    let s = b.semi(v);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    let named = c.diags.iter().any(|x| x.code == DiagCode::NoMethod && x.name == Some(Name::of("nope")));
    let got: Vec<String> = c.diags.iter().map(|x| format!("{:?} naming {:?}", x.code, x.name)).collect();
    assert!(named, "`W::nope()`: want E0599 naming `nope`, got [{}]", got.join(", "));
}
