//! Piece sig-paths (lane B9): a written type path means what it RESOLVES to, a type alias
//! expanded, never lined up with the erased expansion as if it named it. The head lowering
//! expands the alias when it lays the head's region column (D148); what it must give for each
//! alias form (an alias to a `'static` type, an alias with lifetime parameters, an alias of an
//! alias, a generic alias, swapped arguments, the Reference's elision examples) is stated in
//! blaze-ladder tests/front2_region_rules.rs. Here the lowering's half: a column holding the
//! alias's `'static` demands `'static` of the argument.
//!
//! Sources: the Reference, "Type aliases" ("A type alias defines a new name for an existing
//! type"); "Lifetime elision"; "Lifetime bounds" (`'static` outlives every lifetime: a local's
//! borrow passed where `'static` is demanded is E0597).
//!
//! The heads are declared with the shared fixture (region_fixture.rs [`Decls`], laid in the
//! checked record's own table) and the lowering runs through region_fixture.rs `findings`.

use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ItemKey, Mutbl, Name};

use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Region, Variance};

/// The lowering's facts: the ADTs' lifetime counts (`hold` declares one, every other none; the
/// heads' [`Decls::adt`] says the same).
struct Heads {
    hold: ItemKey,
}

impl Heads {
    fn lifetimes(&self, adt: ItemKey) -> u32 {
        u32::from(adt == self.hold)
    }
}

impl Facts for Heads {
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
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: self.lifetimes(k), union: false, is_enum: false, is_box: false })
    }
    /// `Hold<'a>` holds `&'a S`: covariant in `'a`; the rest have no parameter.
    fn adt_variances(&self, k: ItemKey) -> Option<&[Variance]> {
        Some(crate::region_fixture::all_co(self.lifetimes(k)))
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
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// SOUNDNESS, the negative rustc refuses (E0597): `type L = Hold<'static>; fn mk(s: &S) ->
/// Hold<'_>; fn keep(h: L); fn bad(s: S) { keep(mk(&s)); }` passes a borrow of the local `s`
/// where `'static` is demanded: a `borrow` finding. The positive: `fn keep(h: Hold<'_>)` in its
/// place, no finding. Control (the cause removed: `keep`'s `h: L` not expanded, as before
/// piece sig-paths, a fresh lifetime): `bad` has NO finding, which is what blaze accepted.
///
/// Heads: `mk` `[Param(0), Param(0)]` (one elided input lifetime, given to the output);
/// `keep(h: L)` `['static]` (the alias expanded); `keep(h: Hold<'_>)` and the control
/// `[Param(0)]`; `bad(s: S)` none.
/// Prints `an_alias_input_demands_static_of_its_argument: <case> <findings>`, three lines.
#[test]
fn an_alias_input_demands_static_of_its_argument() {
    let mut fx = Fx::new();
    let sk = fx.strukt("S", 0, &[]);
    let hk = fx.strukt("Hold", 0, &[]);
    let st = fx.adt(sk, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let hold = fx.adt(hk, &[]);
    let unit = fx.unit();
    let mk = fx.func("mk", 0, &[rs], hold, vec![]);
    let keep = fx.func("keep", 0, &[hold], unit, vec![]);
    let bad = fx.func("bad", 0, &[st], unit, vec![]);
    fx.finish();
    let p0 = Region::Param(0);
    let cases: [(&str, u32, Region, bool); 3] = [("alias to 'static", 0, Region::Static, true), ("Hold<'_>", 1, p0, false), ("control, no alias known", 1, p0, false)];
    for (case, keep_count, keep_slot, found) in cases {
        let facts = Heads { hold: hk };
        let mut decls = Decls::new();
        decls.adt(hk, 1);
        // `fn mk(s: &S) -> Hold<'_>`: `P0` late-bound (in the inputs, in no bound).
        decls.func(mk, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: hold, slots: &[p0, p0] });
        // `fn keep(h: L)` (`'static`, no lifetime of its own) or `fn keep(h: Hold<'_>)` (`P0`,
        // late-bound).
        let keep_late: &[u32] = if keep_count == 1 { &[0] } else { &[] };
        decls.func(keep, FnHead { lifetimes: keep_count, late: keep_late, outlives: &[], inputs: &[hold], output: unit, slots: &[keep_slot] });
        // `fn bad(s: S)`: no lifetime.
        decls.func(bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
        let mut body = Bb::new();
        let ps = body.param("s");
        let ls = body.local(ps);
        let bor = body.borrow(Mutbl::Not, ls);
        let mkp = body.path(&["mk"]);
        let made = body.call(mkp, &[bor]);
        let kp = body.path(&["keep"]);
        let kept = body.call(kp, &[made]);
        let k = body.semi(kept);
        let root = body.block(&[k], None);
        let lines = findings(&fx, &body, bad, root, &facts, &decls, Plant::None).expect("lowered");
        std::println!("an_alias_input_demands_static_of_its_argument: {case} {lines:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow")), found, "{case}: {lines:?}");
        assert_eq!(lines.is_empty(), !found, "{case}: {lines:?}");
    }
}
