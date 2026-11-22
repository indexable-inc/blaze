//! Lane RB10, class "a `Copy` impl that bounds no argument" (NEVER COMPILED when written,
//! 2026-11-18). The Reference, "Special types and traits", `Copy`: a type is `Copy` when an
//! impl says so, under that impl's bounds; `#[derive(Copy)]` bounds every type parameter by
//! `Copy`, but core's `impl<T: ?Sized> Copy for NonNull<T>` and bstd's `impl<H> Copy for
//! RingEnd<H>` (task/ring.rs:147) bound none. The lowering took the derive's rule for every
//! ADT (lib.rs `is_copy`), so `NonNull<Header>` (a non-`Copy` `Header`) was moved, not copied:
//! the census's six `UseOfMoved` (`Pool::run_now`, `reserve` twice, `run_lent`,
//! `EndRing::next`, `Shares::split`) and `RingEnd::clone`'s `MoveOut(Deref)` (`*self`).
//! [`Facts::copy_args`] says which arguments an impl bounds.
//!
//! The program: `struct S;` (not `Copy`), `struct P<T> { p: T }` with a `Copy` impl, `fn h(_:
//! P<S>)`, and `fn f(p: P<S>) { h(p); h(p); }`.

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ItemKey, Name};

use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal};

/// `P`'s `Copy` impl bounds `args` (`None`: not read, the derive's rule).
struct World {
    p: ItemKey,
    args: Option<&'static [bool]>,
}

impl Facts for World {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let copy = k == self.p;
        Some(AdtFacts { copy, drop_impl: false, needs_drop: !copy, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn copy_args(&self, adt: ItemKey) -> Option<&[bool]> {
        if adt == self.p { self.args } else { None }
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// The findings of `fn f(p: P<S>) { h(p); h(p); }` with `P`'s impl bounding `args`.
fn twice(args: Option<&'static [bool]>) -> Result<vec::Vec<String>, Refusal> {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let t0 = fx.param(0);
    let p = fx.strukt("P", 1, &[("p", t0)]);
    let ps = fx.adt(p, &[st]);
    let unit = fx.unit();
    let h = fx.func("h", 0, &[ps], unit, vec![]);
    let f = fx.func("f", 0, &[ps], unit, vec![]);
    fx.finish();
    // `fn h(_: P<S>)` and `fn f(p: P<S>)`: no lifetime (`P` and `S` declare none).
    let mut decls = Decls::new();
    for item in [h, f] {
        decls.func(item, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[ps], output: unit, slots: &[] });
    }
    let mut b = Bb::new();
    let pp = b.param("p");
    let mut stmts = vec::Vec::new();
    for _ in 0..2 {
        let h = b.path(&["h"]);
        let lp = b.local(pp);
        let call = b.call(h, &[lp]);
        stmts.push(b.semi(call));
    }
    let root = b.block(&stmts, None);
    findings(&fx, &b, f, root, &World { p, args }, &decls, Plant::None)
}

fn moved(got: &Result<vec::Vec<String>, Refusal>) -> bool {
    matches!(got, Ok(lines) if lines.iter().any(|l| l.contains("UseOfMoved")))
}

/// `NonNull<Header>` in miniature: `P`'s impl bounds nothing (`[false]`), so `P<S>` is `Copy`
/// and the second `h(p)` reads a copy: no finding. NEGATIVE: an impl that bounds `T: Copy`
/// (`[true]`, a derived `Copy`) makes `P<S>` not `Copy`: the second `h(p)` uses a moved value
/// (rustc E0382). CONTROL (rule 23): no row, the derive's rule, the finding again. A row of
/// the wrong length is refused by name.
#[test]
fn an_unbounded_copy_impl_copies_whatever_its_argument() {
    let fine = twice(Some(&[false]));
    std::println!("copy_args unbounded: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let bounded = twice(Some(&[true]));
    std::println!("copy_args bounded: {bounded:?}");
    assert!(moved(&bounded), "P<S> is not Copy under `T: Copy`: {bounded:?}");
    let control = twice(None);
    std::println!("copy_args unread: {control:?}");
    assert!(moved(&control), "the derive's rule moves: {control:?}");
    let wrong = twice(Some(&[false, false]));
    std::println!("copy_args misaligned: {wrong:?}");
    assert!(matches!(wrong, Err(ref r) if matches!(r.form, Form::CopyArgsUnaligned { .. })), "{wrong:?}");
}
