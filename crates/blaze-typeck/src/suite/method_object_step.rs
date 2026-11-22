//! A bound candidate (an object's principal and its supertraits, or a parameter's bounds) has
//! `Self` fixed to the type of the autoderef step it was found at (rules/method.rs `Cand::on`),
//! so it applies only where the adjusted receiver is its `self` of that type. The reduction of tempo's zone_sequencer settlement.rs:96 and
//! :101 (`impl fmt::Display for BatchSubmitError { .. Self::Other(error) => error.fmt(f) }` with
//! `error: &eyre::Report`, only the module `fmt` imported), which blaze reported as a false
//! E0034 "found `&Report`, about `fmt`":
//!
//! ```text
//! trait Deref { type Target: ?Sized; }
//! trait Debug { fn fmt(&self) -> u8; }   trait Display { fn fmt(&self) -> u8; }
//! trait Error: Debug + Display { fn source(&self) -> u8; }
//! struct Report;  impl Debug for Report {}  impl Display for Report {}
//! impl Deref for Report { type Target = dyn Error; }
//! fn show(r: &Report) -> u8 { r.fmt() }        // `Display` in scope (eyre's case: the current
//!                                              // impl's trait, rules/method.rs)
//! fn cause(r: &Report) -> u8 { r.source() }    // nothing in scope
//! struct Plain;  fn plain(p: &Plain) -> u8 { p.fmt() }
//! ```
//!
//! The Reference, "Method call expressions": for each candidate receiver type in order (`&Report`,
//! `&&Report`, `&mut &Report`, `Report`, .., `dyn Error`, `&dyn Error`, ..), "T's inherent methods"
//! then "any of the methods provided by a visible trait implemented by T"; a trait object's
//! "inherent" methods are its principal's and supertraits' (rustc probe.rs
//! `assemble_inherent_candidates_from_object`: an `ObjectCandidate` whose `xform_self_ty` has
//! `Self = dyn Error`). So at the first type, `&Report`, the object's `Debug::fmt` and
//! `Display::fmt` take `&dyn Error`, not `&Report`, and do not apply; `Display::fmt` of the
//! trait in scope does (`Self = Report`): `show` checks, calling `Display::fmt::<Report>`.
//!
//! The bug: the object's candidates were tried at every step with a fresh `Self`, in the
//! inherent group: at `&Report` both took `Self = Report` and both held, two applicable
//! inherent candidates, E0034. The twins keep the real errors: with both `Debug` and `Display`
//! in scope `r.fmt()` IS ambiguous (E0034 in rustc too); with nothing in scope it reaches the
//! object step, where both object candidates apply (E0034 again); `Plain` has no `fmt` at all
//! (E0599).

use std::vec::Vec;

use super::{Want, callee, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, List, Mutbl, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// Which of `Debug` and `Display` the world brings into scope.
#[derive(Clone, Copy)]
struct Scope {
    debug: bool,
    display: bool,
}

/// The world of the module doc, with `Display::fmt`'s key.
fn world(scope: Scope) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let u8_ = fx.u8();
    let debug = fx.tr("Debug", 1, Vec::new(), &[]);
    fx.trait_fn(debug, "fmt", true, &[r_self], u8_);
    let display = fx.tr("Display", 1, Vec::new(), &[]);
    let display_fmt = fx.trait_fn(display, "fmt", true, &[r_self], u8_);
    let (sup_debug, sup_display) = (fx.bound(debug, &[p0]), fx.bound(display, &[p0]));
    let error = fx.tr("Error", 1, std::vec![sup_debug, sup_display], &[]);
    fx.trait_fn(error, "source", true, &[r_self], u8_);
    if scope.debug {
        fx.use_trait(debug);
    }
    if scope.display {
        fx.use_trait(display);
    }
    let report_k = fx.strukt("Report", 0, &[]);
    let report = fx.adt(report_k, &[]);
    fx.imp("impl Debug for Report", 0, report, Some((debug, &[])), &[], Vec::new());
    fx.imp("impl Display for Report", 0, report, Some((display, &[])), &[], Vec::new());
    let ex_error = fx.ty(PNode::ExTrait(error, List::EMPTY));
    let dyn_error = fx.t.dynamic(&[ex_error]).expect("dyn Error");
    fx.imp("impl Deref for Report", 0, report, Some((deref, &[])), &[("Target", dyn_error)], Vec::new());
    let plain_k = fx.strukt("Plain", 0, &[]);
    let plain = fx.adt(plain_k, &[]);
    let r_report = fx.r(Mutbl::Not, report);
    let r_plain = fx.r(Mutbl::Not, plain);
    fx.func("show", 0, &[r_report], u8_, Vec::new());
    fx.func("cause", 0, &[r_report], u8_, Vec::new());
    fx.func("plain", 0, &[r_plain], u8_, Vec::new());
    fx.finish();
    (fx, display_fmt)
}

/// `fn <owner>(x: ..) -> u8 { x.<method>() }`: the local is expression 0, the call 1, the
/// block 2.
fn call_on_x(fx: &Fx, owner: &str, method: &str) -> (Bb, crate::columns::Checked<'static>) {
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let e1 = b.method(e0, method, &[]);
    let root = b.block(&[], Some(e1));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    (b, c)
}

fn codes(c: &crate::columns::Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// `show`: `r.fmt()` with `Display` in scope is `Display::fmt::<Report>`, picked by value at the
/// first step; the object's candidates do not apply there. Before the fix: E0034.
#[test]
fn a_trait_in_scope_beats_the_deref_objects_candidates_at_an_earlier_step() {
    let (fx, display_fmt) = world(Scope { debug: false, display: true });
    let (b, c) = call_on_x(&fx, "show", "fmt");
    expect(&fx, &b, &c, &Want {
        exprs: &[(ExprId(0), "&Report"), (ExprId(1), "u8"), (ExprId(2), "u8")],
        pats: &[(PatId(0), "&Report")],
        diags: &[],
    });
    let picked = c.callees.column::<0>().iter().position(|&e| e == ExprId(1)).map(|i| c.callees.column::<1>()[i]);
    assert_eq!(picked, Some(display_fmt), "the pick is not `Display::fmt` ({})", callee(&fx, &c, ExprId(1)));
    assert_eq!(callee(&fx, &c, ExprId(1)), "fmt<Report>");
}

/// An object's own method is still found at the object's step: `cause`'s `r.source()` is
/// `Error::source::<dyn Error>`, a dynamic call, reached by the overloaded deref and an autoref.
#[test]
fn an_objects_method_is_found_at_the_objects_step() {
    let (fx, _) = world(Scope { debug: false, display: false });
    let (b, c) = call_on_x(&fx, "cause", "source");
    expect(&fx, &b, &c, &Want {
        exprs: &[(ExprId(0), "&Report"), (ExprId(1), "u8"), (ExprId(2), "u8")],
        pats: &[(PatId(0), "&Report")],
        diags: &[],
    });
    assert_eq!(callee(&fx, &c, ExprId(1)), "source<dyn Error>");
}

/// Negative twin: with `Debug` and `Display` both in scope, `r.fmt()` has two applicable trait
/// candidates at `&Report` (`Self = Report` for both): E0034, as rustc says.
#[test]
fn both_traits_in_scope_is_still_ambiguous() {
    let (fx, _) = world(Scope { debug: true, display: true });
    let (_, c) = call_on_x(&fx, "show", "fmt");
    assert_eq!(codes(&c), std::vec![DiagCode::AmbiguousMethod], "{:?}", c.diags);
}

/// Negative twin: with nothing in scope `r.fmt()` reaches the object step, where `Debug::fmt`
/// and `Display::fmt` (supertraits of the principal) both apply at `&dyn Error`: E0034.
#[test]
fn the_objects_two_supertrait_methods_are_still_ambiguous() {
    let (fx, _) = world(Scope { debug: false, display: false });
    let (_, c) = call_on_x(&fx, "show", "fmt");
    assert_eq!(codes(&c), std::vec![DiagCode::AmbiguousMethod], "{:?}", c.diags);
}

/// Negative twin: `Plain` implements neither trait and derefs to nothing: `p.fmt()` is E0599
/// even with `Display` in scope.
#[test]
fn a_type_with_no_display_has_no_fmt() {
    let (fx, _) = world(Scope { debug: false, display: true });
    let (_, c) = call_on_x(&fx, "plain", "fmt");
    assert_eq!(codes(&c), std::vec![DiagCode::NoMethod], "{:?}", c.diags);
}
