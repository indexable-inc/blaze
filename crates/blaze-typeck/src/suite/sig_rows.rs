//! The body's OWN signature files its projections as `Checked::selected` rows at the body's
//! root (machine.rs `sig_rows`), so the borrow check can map `<Wrap<'a> as Tr>::A` through
//! the selected impl's associated type (blaze-flow-lower `Lower::lay_through_impl`, design B).
//! Before, rows were filed only for a callee's signature at a call (`Lower::Args`), and the
//! body's signature, lowered with `Lower::Body`, gave none.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait Tr { type A; }
//! struct W;
//! impl Tr for W { type A = u8; }
//! fn get(x: <W as Tr>::A) {}            // a row at the root: Selected::Impl of that impl
//! fn gen<T: Tr>(x: <T as Tr>::A) {}     // a row at the root: Selected::ParamEnv (`T: Tr`)
//! fn plain(x: u8) {}                    // no row
//! ```
//!
//! A projection that does not select (its arguments name the body's generics) still gets its
//! row, naming its source where one where-clause of the body applies (`Selected::ParamEnv`,
//! writeback/selected_rows.rs `source_not_ground`), else `Selected::Generic`.

use std::vec::Vec;

use super::check_body;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Name};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::selected::Selected;

struct World {
    fx: Fx,
    tr: ItemKey,
    imp: ItemKey,
    get: ItemKey,
    gen_: ItemKey,
    plain: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let (u8_, unit) = (fx.u8(), fx.unit());
    let tr = fx.tr("Tr", 1, Vec::new(), &["A"]);
    let w = fx.strukt("W", 0, &[]);
    let w_t = fx.adt(w, &[]);
    let imp = fx.imp("impl Tr for W", 0, w_t, Some((tr, &[])), &[("A", u8_)], Vec::new());
    let w_a = fx.alias(tr, "A", &[w_t]);
    let get = fx.func("get", 0, &[w_a], unit, Vec::new());
    let t_a = fx.alias(tr, "A", &[p0]);
    let t_tr = fx.bound(tr, &[p0]);
    let gen_ = fx.func("gen", 1, &[t_a], unit, std::vec![t_tr]);
    let plain = fx.func("plain", 0, &[u8_], unit, Vec::new());
    fx.finish();
    World { fx, tr, imp, get, gen_, plain }
}

/// `fn owner(x: ..) {}`: its rows at the root, as `(trait, name, selected)`, and its findings.
fn root_rows(w: &World, owner: ItemKey) -> (Vec<(ItemKey, Option<Name>, Selected)>, usize) {
    let mut b = Bb::new();
    b.param("x");
    let root = b.block(&[], None);
    let c = check_body(&w.fx, &b, owner, root, Plant::None, InferPlant::None);
    let rows = c.selected_at(root).iter().map(|r| (r.trait_, r.name, r.selected)).collect();
    (rows, c.diags.len())
}

/// `fn get(x: <W as Tr>::A)`: one row at the root naming `impl Tr for W`.
#[test]
fn a_signature_projection_that_selects_an_impl_is_a_row_at_the_root() {
    let w = world();
    let (rows, diags) = root_rows(&w, w.get);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (tr, name, sel) = rows[0];
    assert_eq!((tr, name), (w.tr, Some(Name::of("A"))));
    assert!(matches!(sel, Selected::Impl { imp, .. } if imp == w.imp), "{sel:?}");
}

/// The negative twin: `fn gen<T: Tr>(x: <T as Tr>::A)` selects no impl: its row names the
/// body's one where-clause `T: Tr` as its source (`Selected::ParamEnv`, never an impl); and
/// `fn plain(x: u8)` names no projection: no row.
#[test]
fn a_generic_signature_projection_is_a_generic_row_and_none_is_no_row() {
    let w = world();
    let (rows, diags) = root_rows(&w, w.gen_);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0, rows[0].1), (w.tr, Some(Name::of("A"))));
    assert!(matches!(rows[0].2, Selected::ParamEnv { .. }), "{:?}", rows[0].2);
    let (rows, diags) = root_rows(&w, w.plain);
    assert_eq!(diags, 0);
    assert!(rows.is_empty(), "{rows:?}");
}
