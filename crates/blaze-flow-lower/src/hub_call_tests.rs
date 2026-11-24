//! Lane RB10, class "a reference argument's loan reaches its own referent through the call's
//! hub" (call relating: RB8's to fix; this file is the repro and the diagnosis only; NEVER
//! COMPILED when written, 2026-11-18).
//!
//! The census's 33 `Invalidated` (bstd `write_keys`, `take_all`, `granted_value`, `is_empty`,
//! simtest `laid`, `refusal`, `member_said`): a call `g(&mut *w)` of a known head that writes
//! no lifetimes, whose type argument carries an origin (`T = W<'static>`, bstd's
//! `String<LaunchAlloc>` with `LaunchAlloc = SharedAlloc<'static>`, grant.rs:81), and no
//! generic signature to relate it by (`Facts::shape` or `Facts::call_bounds` missing, or
//! `call_by_generic_signature` not lining up), goes to `unify_call` (lib.rs). There the
//! reference's top origin `'r` flows INTO the hub (one way) and the hub flows into every other
//! position, the referent's `W<'?>` among them; `borrow` related that referent position to
//! `*w`'s both ways (a `&mut` is invariant in its referent), and `*w`'s is `'static`. So the
//! loan of `*w` reaches `'static`, is live for ever, and the next `&mut *w` conflicts with it.
//!
//! The rule rustc applies (RFC 2094, "Subtyping" and "Reborrow constraints"; the Reference,
//! "Lifetime bounds", implied bounds): `fn g<T>(t: &'r mut T)` relates `'r` to `T` only by
//! `T: 'r` (each of `T`'s regions outlives `'r`: a flow from `T`'s positions INTO `'r`, never
//! out of `'r`). A callee that writes no lifetime cannot store the `&'r mut T` anywhere a
//! region of `T` names (no type of its signature holds `'r` but the argument itself, and the
//! output, by elision, only when it has one input lifetime). FIX (RB8, `unify_call` or a
//! generic path before it): for a known head that writes no lifetimes, a reference argument's
//! top origin goes into the output's elided positions only, and its referent's positions are
//! related to the type arguments' as `call_by_generic_signature` relates them; never through
//! the hub that flows back into the referent.

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PatId, UnOp};

use crate::region_fixture::{self, Decls, FnHead, P0};
use blaze_own::Alloc;
use blaze_typeck::regions::{Arity, Count};

use crate::{AdtFacts, CallBounds, CallLacks, CalleeFacts, Facts, Form, Plant, Refusal, Region, Shape, ShapeBuf, Variance, read_shape};

/// `struct W<'a>;` (one lifetime), `fn g<T>(t: &mut T)`, the owner `fn f(w: &mut W<'static>)`:
/// heads declared by hand (region_fixture.rs, D148); no shape and no bounds (`unify_call`).
struct World {
    w: ItemKey,
    decls: Decls,
    /// `g`'s shape when filed (`read_shape`): then the call goes by its generic signature.
    shape: Option<(ItemKey, ShapeBuf<Alloc<'static>>)>,
}

/// The counts `read_shape` reads: `W` one lifetime, nothing else known.
struct Counts {
    w: ItemKey,
}

impl Arity for Counts {
    fn adt_lifetimes(&self, k: ItemKey) -> Count {
        Count::Known(u32::from(k == self.w))
    }
    fn opaque_captures(&self, _: ItemKey) -> Count {
        Count::NotKnown
    }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
        Count::NotKnown
    }
    fn trait_lifetimes(&self, _: ItemKey) -> Count {
        Count::NotKnown
    }
}

impl Facts for World {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
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
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
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
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>> {
        self.shape.as_ref().filter(|(k, _)| *k == item).map(|(_, s)| s.shape())
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = u32::from(k == self.w);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    /// `W<'a>` is `struct W<'a>(&'a ());` (a lifetime must be used, E0392): covariant in `'a`
    /// (the Reference, "Subtyping and Variance": an ADT is as its fields are).
    fn adt_variances(&self, k: ItemKey) -> Option<&[Variance]> {
        const CO: &[Variance] = &[Variance::Co];
        (k == self.w).then_some(CO)
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        // `fn g<T>(t: &mut T)` declares no type-outlives bound and has no trait bound on `T`:
        // known and empty, as the real fact table answers it. `owes_type_outlives` asks.
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: &[] })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// `*operand`: the fixture has no deref builder, so it takes the slot of a read of `p`.
fn deref(b: &mut Bb, p: PatId, operand: ExprId) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Unary { op: UnOp::Deref, operand };
    at
}

/// `&mut *w`.
fn reborrow(b: &mut Bb, w: PatId) -> ExprId {
    let lw = b.local(w);
    let d = deref(b, w, lw);
    b.borrow(Mutbl::Mut, d)
}

/// The body of `f`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Body {
    /// `g(&mut *w);`
    One,
    /// `g(&mut *w); g(&mut *w);`
    Two,
    /// `let r = &mut *w; g(&mut *w); g(r);`
    Held,
    /// `fn g<T>(t: T)`, `fn f(w: W<'static>) { g(w); }`: a value whose only region is
    /// `'static`, no reference: nothing the one origin can make conflict.
    Value,
}

/// The findings of `fn f(w: &mut W<'static>) { <body> }` (with [`Body::Value`], `fn f(w:
/// W<'static>)` and `fn g<T>(t: T)`). `shaped`: `g`'s shape filed.
fn findings(body: Body, shaped: bool) -> Result<vec::Vec<String>, Refusal> {
    let held = body == Body::Held;
    let value = body == Body::Value;
    let mut fx = Fx::new();
    let wk = fx.strukt("W", 0, &[]);
    let wt = fx.adt(wk, &[]);
    let rw = fx.r(Mutbl::Mut, wt);
    let t0 = fx.param(0);
    let rt = fx.r(Mutbl::Mut, t0);
    let (g_in, f_in) = if value { (t0, wt) } else { (rt, rw) };
    let unit = fx.unit();
    let g = fx.func("g", 1, &[g_in], unit, vec![]);
    let f = fx.func("f", 0, &[f_in], unit, vec![]);
    fx.finish();
    let shape = shaped.then(|| {
        let mut s = ShapeBuf::new_in(Alloc::global());
        if let Err(why) = read_shape(fx.t.view(), &[g_in], unit, &Counts { w: wk }, &mut s, Alloc::global()) {
            panic!("`g`'s shape was not read: {why:?}");
        }
        (g, s)
    });
    let mut w = World { w: wk, decls: Decls::new(), shape };
    // `W<'a>` declares one lifetime (as `Facts::adt` says).
    w.decls.adt(wk, 1);
    if value {
        // `fn g<T>(t: T)`: no lifetime. `fn f(w: W<'static>)`: `W`'s `'static`.
        w.decls.func(g, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[t0], output: unit, slots: &[] });
        w.decls.func(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[wt], output: unit, slots: &[Region::Static] });
    } else {
        // `fn g<T>(t: &mut T)`: `t`'s elided lifetime `P0`, late-bound.
        w.decls.func(g, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rt], output: unit, slots: &[P0] });
        // `fn f(w: &mut W<'static>)`: `w`'s elided reference `P0` (late-bound), `W`'s `'static`.
        w.decls.func(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rw], output: unit, slots: &[P0, Region::Static] });
    }
    let mut b = Bb::new();
    let pw = b.param("w");
    if value {
        let callee = b.path(&["g"]);
        let arg = b.local(pw);
        let call = b.call(callee, &[arg]);
        let s = b.semi(call);
        let root = b.block(&[s], None);
        return region_fixture::findings(&fx, &b, f, root, &w, &w.decls, Plant::None);
    }
    let mut stmts = vec::Vec::new();
    let r = if held {
        let r = b.bind("r");
        let first = reborrow(&mut b, pw);
        stmts.push(b.let_(r, None, first));
        Some(r)
    } else {
        None
    };
    let callee = b.path(&["g"]);
    let arg = reborrow(&mut b, pw);
    let call = b.call(callee, &[arg]);
    stmts.push(b.semi(call));
    if body != Body::One {
        let callee = b.path(&["g"]);
        let arg = match r {
            Some(r) => b.local(r),
            None => reborrow(&mut b, pw),
        };
        let call = b.call(callee, &[arg]);
        stmts.push(b.semi(call));
    }
    let root = b.block(&stmts, None);
    region_fixture::findings(&fx, &b, f, root, &w, &w.decls, Plant::None)
}

/// The bstd shape: two reborrows of `*w` one after the other, each loan dead when its call
/// returns; rustc accepts. With `g`'s shape filed the call goes by its generic signature
/// (`T = W<'static>` related to `*w`'s type, `&'r mut` fresh): CLEAN. Without it, the call
/// goes by `unify_call`'s one origin, a STRICTER relation (noted `CallGenericUnaligned {
/// lacks: Shape }`, `Lowered::coarse`), which carries the first loan into `W`'s `'static` (the
/// census's `Invalidated`; tempo's `for x in it` and `cursor.seek(..)` pairs): that finding is
/// the relation's artifact, so the body is REFUSED by the relation's name, the finding dropped.
/// NOT YET RUN (written 2026-11-21 by a write-only lane).
#[test]
fn two_reborrows_through_a_generic_call_do_not_conflict() {
    for shaped in [true, false] {
        let got = findings(Body::Two, shaped);
        std::println!("hub_call two reborrows, shaped {shaped}: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()));
    }
}

/// STRICT RELATIONS REFUSE ONLY ON A FINDING: `g(w)` with `w: W<'static>` by value, no shape:
/// the one origin is used (stricter, `any` holds: `W<'static>` has a position) and nothing
/// conflicts under it, so the body stands CHECKED clean, never refused up front (a clean
/// result under a stricter relation is clean under Rust's). CONTROL: the same body with the
/// shape: clean too (the precise path agrees).
/// `g(&mut *w);` alone (`Body::One`) is printed, not asserted: whether the one origin makes a
/// finding there depends on the reborrow's own constraint (`*w`'s `'P0` outliving the loan),
/// which this lane did not read (UNSURE); either way it must not be an up-front refusal
/// without a finding: it is clean, or refused `CallGenericUnaligned` (shapeless), or clean
/// (shaped).
#[test]
fn a_clean_body_under_the_one_origin_stands_checked() {
    for shaped in [false, true] {
        let got = findings(Body::Value, shaped);
        std::println!("hub_call value, shaped {shaped}: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()), "shaped {shaped}");
    }
    let got = findings(Body::One, true);
    std::println!("hub_call one reborrow, shaped: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()));
    let got = findings(Body::One, false);
    std::println!("hub_call one reborrow, no shape: {got:?}");
    assert!(
        matches!(got, Ok(ref l) if l.is_empty()) || matches!(got, Err(Refusal { form: Form::CallGenericUnaligned { lacks: CallLacks::Shape, .. }, .. })),
        "{got:?}"
    );
}

/// NEGATIVE TWIN (rustc E0499): `let r = &mut *w; g(&mut *w); g(r);`: `r`'s loan is live
/// across the second reborrow: a finding by the generic signature (the precise path). Without
/// the shape the one origin also finds it, but that finding may be its artifact: refused by
/// name, never reported, never a pass.
#[test]
fn a_reborrow_held_across_a_second_is_refused() {
    for shaped in [true, false] {
        let got = findings(Body::Held, shaped);
        std::println!("hub_call held reborrow, shaped {shaped}: {got:?}");
        assert!(matches!(got, Ok(ref lines) if lines.iter().any(|l| l.contains("Invalidated"))), "{got:?}");
    }
}
