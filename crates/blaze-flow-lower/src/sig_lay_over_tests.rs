//! A body's own signature laid over its normalized types (lib.rs `Lower::lay_over`): tempo's
//! 239 `SigPositionsUnaligned` (c2135wave69, first node-launcher `launch(builder:
//! TempoLaunchBuilder)`, its type naming `<TempoNode as Node<A>>::ComponentsBuilder`). The head
//! keeps the projection, ONE position; the checked body has its normalization, an ADT of its
//! own count of positions. Laid by the declared walk alone the counts differed and the body was
//! refused (`SigSite::Count`); laid over the body's type, the normalization's positions are
//! hidden placeholders, as the projection's one was.
//!
//! The world: `struct S;` (no lifetime), `trait Tr { type A; }`, the body
//! `fn f(x: (&u8, S)) { x; }` whose head declares `x: (&'a u8, <S as Tr>::A)`.

use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ItemKey, Mutbl, Name, PTy};

use crate::region_fixture::{Decls, FnHead};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Region, SigSite};

struct World;

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
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
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

#[derive(Clone, Copy, Debug)]
enum Declared {
    /// `(&'a u8, <S as Tr>::A)`: the projection the body normalized to `S`.
    Projection,
    /// `(&'a u8, S)`: as the body has it.
    Same,
    /// `(&'a u8, S, S)`: another shape (a malformed head): refused, the site named.
    Other,
}

/// The type checker's row for the signature's projection at the body's root.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// None filed.
    None,
    /// `Selected::Generic`: the row a projection over a parameter gets (`<T as Tr>::A`).
    Generic,
    /// `Selected::Unanswered`.
    Unanswered,
}

fn lowered(declared: Declared, row: Row) -> Result<vec::Vec<std::string::String>, Refusal> {
    use blaze_typeck::{List, Projected, Selected};
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let tr = fx.tr("Tr", 0, vec![], &["A"]);
    let st = fx.adt(s, &[]);
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);
    let body_ty = fx.tuple(&[ru8, st]);
    let unit = fx.unit();
    let f = fx.func("f", 0, &[body_ty], unit, vec![]);
    let head_ty: PTy = match declared {
        Declared::Projection => {
            let a = fx.alias(tr, "A", &[st]);
            fx.tuple(&[ru8, a])
        }
        Declared::Same => body_ty,
        Declared::Other => fx.tuple(&[ru8, st, st]),
    };
    fx.finish();
    let mut heads = Decls::new();
    // The one counter's facts for `<S as Tr>::A` (as sig_assoc_tests and open_projection_tests
    // declare them): `Tr` and its `A` write no lifetime, and `S` has none. So the projection's
    // subtree holds no slot both today (an alias's own slots, `alias_lifetimes`) and under
    // design B (an alias laid by its ARGUMENTS' slots: `S`'s, none): the head's column is
    // `[Param(0)]`, `&'a u8`'s alone, in both.
    heads.trait_(tr, 0);
    heads.alias(tr, "A", 0);
    let one: &[u32] = &[0];
    heads.func(f, FnHead { lifetimes: 1, late: one, outlives: &[], inputs: &[head_ty], output: unit, slots: &[Region::Param(0)] });
    let mut b = Bb::new();
    let px = b.param("x");
    let lx = b.local(px);
    let st1 = b.semi(lx);
    let root = b.block(&[st1], None);
    let view = b.view(f, root);
    let mut c = crate::region_fixture::checked(&fx, &view);
    let selected = match row {
        Row::None => None,
        Row::Generic => Some(Selected::Generic),
        Row::Unanswered => Some(Selected::Unanswered),
    };
    if let Some(selected) = selected {
        c.selected.push(Projected { expr: root, at: 0, trait_: tr, name: Some(Name::of("A")), args: List::EMPTY, global_env: false, selected });
        c.selected.sort_by_key(|r| (r.expr, r.name.is_none()));
    }
    crate::region_fixture::lower(&fx, &view, &mut c, &World, &heads, Plant::None).map(|r| r.lines)
}

/// `Projection` under a `Generic` row and `Same` are lowered and checked clean (the
/// normalization's positions placeholders, exactly the signature's word); `Other` is refused
/// `SigPositionsUnaligned { site: Unlaid { input: Some(0) } }`, naming the check.
/// STRICT RELATIONS REFUSE ONLY ON A FINDING: `Projection` with no row, or an `Unanswered`
/// one, cannot be laid through an impl; its positions are placeholders noted as stricter
/// (`ProjectionUnbound` `NoRow`, `RowUnanswered`, in `Lowered::coarse`), and the body `x;` has
/// no finding under them: it stands CHECKED clean, never refused up front. (The artifact twin,
/// a finding under the placeholders refused by name, is `through_impl`'s control.)
#[test]
fn a_declared_projection_lays_over_its_normalization() {
    for (declared, row) in [(Declared::Projection, Row::Generic), (Declared::Same, Row::None)] {
        let got = lowered(declared, row);
        std::println!("lay_over {declared:?} {row:?}: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()), "{declared:?} {row:?}");
    }
    for row in [Row::None, Row::Unanswered] {
        let got = lowered(Declared::Projection, row);
        std::println!("lay_over Projection {row:?}: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()), "{row:?}: clean under the placeholders, so checked");
    }
    let got = lowered(Declared::Other, Row::None);
    std::println!("lay_over Other: {got:?}");
    assert!(
        matches!(got, Err(Refusal { form: Form::SigPositionsUnaligned { site: SigSite::Unlaid { input: Some(0) }, .. }, .. })),
        "{got:?}"
    );
}

/// Design B (head VERSION 26): a declared projection laid THROUGH the impl the type checker
/// selected for the body's own signature (lib.rs `Lower::lay_through_impl`). The world:
/// `struct Inner<'a>`, `struct Wrap<'a>`, `trait Tr { type A; }`, `impl<'i> Tr for Wrap<'i> {
/// type A = Inner<'i>; }`, and the body `get<'a>(x: <Wrap<'a> as Tr>::A) -> Inner<RET> { x }`.
/// The Reference, "Associated types": the impl's `type A = Inner<'i>;` defines `<Wrap<'a> as
/// Tr>::A` as `Inner<'a>` ("Generic implementations": `'i` is bound by the self type).
mod through_impl {
    use std::string::String;
    use std::vec;

    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{ItemKey, List, Name, Projected, Selected};

    use crate::region_fixture::{self, Decls, FnHead, ImplHead, P0};
    use crate::{AdtFacts, CalleeFacts, Facts, Plant, Refusal, Region};

    struct Heads {
        with_lifetime: vec::Vec<ItemKey>,
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
            let lifetimes = u32::from(self.with_lifetime.contains(&k));
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
        }
        /// `Inner<'a>(&'a S)` and `Wrap<'a>(&'a S)`: covariant in `'a` (the Reference,
        /// "Subtyping and Variance").
        fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
            Some(region_fixture::all_co(u32::from(self.with_lifetime.contains(&k))))
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
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

    /// `get` lowered and checked, its output's region `ret` (`P0`: `'a`; `Static`). `row`: the
    /// type checker's row for the signature's projection at the body's root (the selection the
    /// type check files for the body's own signature: until it does, this test files it, as
    /// impl_call_tests.rs reads the call's); `false` is the control, no row.
    fn get(ret: Region, row: bool) -> Result<vec::Vec<String>, Refusal> {
        let mut fx = Fx::new();
        let i = fx.strukt("Inner", 0, &[]);
        let inner = fx.adt(i, &[]);
        let w = fx.strukt("Wrap", 0, &[]);
        let wrap = fx.adt(w, &[]);
        let tr = fx.tr("Tr", 1, vec![], &["A"]);
        let imp = fx.imp("impl<'i> Tr for Wrap<'i> { type A = Inner<'i>; }", 0, wrap, Some((tr, &[])), &[("A", inner)], vec![]);
        let alias = fx.alias(tr, "A", &[wrap]);
        let f = fx.func("get", 0, &[inner], inner, vec![]);
        fx.finish();
        let mut decls = Decls::new();
        decls.adt(i, 1);
        decls.adt(w, 1);
        decls.trait_(tr, 0);
        decls.alias(tr, "A", 0);
        // `'a` is written only in the projection's arguments among the inputs: early-bound.
        // The column: the alias's own slots (none), `Wrap<'a>`'s `[P0]`; the output's `[ret]`.
        decls.func(f, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[alias], output: inner, slots: &[P0, ret] });
        decls.imp(imp, ImplHead { lifetimes: 1, outlives: &[], self_ty: wrap, self_slots: &[P0], trait_ref: Some((tr, &[], &[])) });
        decls.impl_assoc(imp, "A", inner, &[P0]);
        let heads = Heads { with_lifetime: vec![i, w] };
        let mut b = Bb::new();
        let px = b.param("x");
        let lx = b.local(px);
        let root = b.block(&[], Some(lx));
        let view = b.view(f, root);
        let mut c = region_fixture::checked(&fx, &view);
        if row {
            let selected = Selected::Impl { imp, args: List::EMPTY };
            c.selected.push(Projected { expr: root, at: 0, trait_: tr, name: Some(Name::of("A")), args: List::EMPTY, global_env: false, selected });
            c.selected.sort_by_key(|r| (r.expr, r.name.is_none()));
        }
        region_fixture::lower(&fx, &view, &mut c, &heads, &decls, Plant::None).map(|r| r.lines)
    }

    /// `-> Inner<'a> { x }`: `x` is `Inner<'a>` through the impl: CLEAN (rustc accepts).
    /// CONTROL (rule 23): without the row the projection is not laid through the impl: its
    /// position is a placeholder `'a` does not outlive (stricter), which makes a finding there
    /// (the false finding design B removes), so the body is REFUSED by the stricter step's name
    /// (`ProjectionUnbound::NoRow`), the finding dropped (rule 3: never reported).
    #[test]
    fn a_projection_input_is_laid_through_its_impl() {
        let got = get(P0, true);
        std::println!("through_impl get: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()), "`type A = Inner<'i>` makes `x` an `Inner<'a>`: clean");
        let got = get(P0, false);
        std::println!("through_impl get without the row (control): {got:?}");
        assert!(
            matches!(&got, Err(Refusal { form: crate::Form::ProjectionUnbound { why: crate::ProjectionUnbound::NoRow, .. }, .. })),
            "no row: refused by name: {got:?}"
        );
    }

    /// NEGATIVE TWIN, rustc REFUSES: `-> Inner<'static> { x }` with `x: Inner<'a>` ("lifetime
    /// may not live long enough"): with the row (the precise path) the real finding is
    /// reported (the mapping never accepts more). Without it, the finding under the
    /// placeholder cannot be told from an artifact: refused `NoRow`, never a pass.
    #[test]
    fn a_projection_input_through_its_impl_is_not_static() {
        let got = get(Region::Static, true);
        std::println!("through_impl get_static: {got:?}");
        assert!(matches!(&got, Ok(lines) if !lines.is_empty()), "`'a` does not outlive `'static`: {got:?}");
        let got = get(Region::Static, false);
        std::println!("through_impl get_static without the row: {got:?}");
        assert!(
            matches!(&got, Err(Refusal { form: crate::Form::ProjectionUnbound { why: crate::ProjectionUnbound::NoRow, .. }, .. })),
            "{got:?}"
        );
    }

    /// Regression for `blaze_own::soa::tuple_columns!` (`SigPositionsUnaligned`):
    /// `impl<T0, T1> Cols for (T0, T1) { type Ref<'a> = (&'a T0, &'a T1); }`.
    /// Popping a `PNode::Param` substitution frame (`T0`) must NOT return early from `lay_slots`
    /// and drop sibling tuple elements (`&'a T1`).
    /// Positive: `-> (&'a u8, &'a u8)` is clean.
    /// Negative twin: `-> (&'a u8, &'static u8)` reports `UnprovenOutlives` on the second slot.
    #[test]
    fn a_tuple_projection_with_multiple_param_substitutions_aligns_all_slots() {
        let run = |second_ret: Region| {
            let mut fx = Fx::new();
            let u8_ = fx.u8();
            let ru8 = fx.r(blaze_typeck::Mutbl::Not, u8_);
            let pair_u8 = fx.tuple(&[u8_, u8_]);
            let pair_ru8 = fx.tuple(&[ru8, ru8]);
            let p0 = fx.param(0);
            let p1 = fx.param(1);
            let rp0 = fx.r(blaze_typeck::Mutbl::Not, p0);
            let rp1 = fx.r(blaze_typeck::Mutbl::Not, p1);
            let pair_params = fx.tuple(&[p0, p1]);
            let pair_rparams = fx.tuple(&[rp0, rp1]);
            let tr = fx.tr("Cols", 1, vec![], &["Ref"]);
            let imp = fx.imp("impl<T0, T1> Cols for (T0, T1) { type Ref<'a> = (&'a T0, &'a T1); }", 2, pair_params, Some((tr, &[])), &[("Ref", pair_rparams)], vec![]);
            let gat0 = Region::Late { binder: u32::MAX, var: 0 };
            let alias = fx.alias(tr, "Ref", &[pair_u8]);
            let f = fx.func("get_pair", 0, &[pair_ru8], pair_ru8, vec![]);
            fx.finish();
            let mut decls = Decls::new();
            decls.trait_(tr, 0);
            decls.alias(tr, "Ref", 1);
            decls.func(f, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[alias], output: pair_ru8, slots: &[P0, P0, second_ret] });
            decls.imp(imp, ImplHead { lifetimes: 0, outlives: &[], self_ty: pair_params, self_slots: &[], trait_ref: Some((tr, &[], &[])) });
            decls.impl_assoc(imp, "Ref", pair_rparams, &[gat0, gat0]);
            let heads = Heads { with_lifetime: vec![] };
            let mut b = Bb::new();
            let px = b.param("x");
            let lx = b.local(px);
            let root = b.block(&[], Some(lx));
            let view = b.view(f, root);
            let mut c = region_fixture::checked(&fx, &view);
            let selected = Selected::Impl { imp, args: List::EMPTY };
            c.selected.push(Projected { expr: root, at: 0, trait_: tr, name: Some(Name::of("Ref")), args: List::EMPTY, global_env: false, selected });
            c.selected.sort_by_key(|r| (r.expr, r.name.is_none()));
            region_fixture::lower(&fx, &view, &mut c, &heads, &decls, Plant::None).map(|r| r.lines)
        };
        let clean = run(P0);
        assert_eq!(clean, Ok(vec::Vec::new()), "both tuple slots align with 'a: {clean:?}");
        let bad = run(Region::Static);
        assert!(matches!(&bad, Ok(lines) if !lines.is_empty()), "TWIN: second slot 'a does not outlive 'static: {bad:?}");
    }

    /// Regression for forwarding associated types in `tempo-node` (`RpcNodeCore for TempoEthApi<N>`)
    /// and `tempo-transaction-pool` (`BestTransactions for TempoBestTransactions<T>`):
    /// `impl<N> Tr for Wrap<N> { type A = <N as Tr>::A; }`, where declared `<Wrap<N> as Tr>::A`
    /// normalized in the body to `<N as Tr>::A` (same `(trait_, name)` alias head, different Self).
    /// Positive: `&'a <Wrap<N> as Tr>::A -> &'a <N as Tr>::A` aligns cleanly via `through_impl`.
    /// Negative twin: `-> &'static <N as Tr>::A` still reports the real outlives error.
    #[test]
    fn a_forwarding_alias_projection_normalizes_to_inner_alias() {
        let run = |ret_reg: Region, with_row: bool| {
            let mut fx = Fx::new();
            let wrap = fx.strukt("Wrap", 1, &[]);
            let p0 = fx.param(0);
            let wrap_p0 = fx.adt(wrap, &[p0]);
            let tr = fx.tr("Tr", 1, vec![], &["A"]);
            let inner_alias = fx.alias(tr, "A", &[p0]);
            let outer_alias = fx.alias(tr, "A", &[wrap_p0]);
            let ref_outer = fx.r(blaze_typeck::Mutbl::Not, outer_alias);
            let ref_inner = fx.r(blaze_typeck::Mutbl::Not, inner_alias);
            let tr_bound = fx.bound(tr, &[p0]);
            let imp = fx.imp("impl<N: Tr> Tr for Wrap<N> { type A = <N as Tr>::A; }", 1, wrap_p0, Some((tr, &[])), &[("A", inner_alias)], vec![tr_bound]);
            let f = fx.func("forward", 1, &[ref_inner], ref_inner, vec![tr_bound]);
            fx.finish();
            let mut decls = Decls::new();
            decls.trait_(tr, 0);
            decls.alias(tr, "A", 0);
            decls.func(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[ref_outer], output: ref_outer, slots: &[P0, ret_reg] });
            decls.imp(imp, ImplHead { lifetimes: 0, outlives: &[], self_ty: wrap_p0, self_slots: &[], trait_ref: Some((tr, &[], &[])) });
            decls.impl_assoc(imp, "A", inner_alias, &[]);
            let heads = Heads { with_lifetime: vec![] };
            let mut b = Bb::new();
            let px = b.param("x");
            let lx = b.local(px);
            let root = b.block(&[], Some(lx));
            let view = b.view(f, root);
            let mut c = region_fixture::checked(&fx, &view);
            if with_row {
                let selected = Selected::Impl { imp, args: List::EMPTY };
                c.selected.push(Projected { expr: root, at: 0, trait_: tr, name: Some(Name::of("A")), args: List::EMPTY, global_env: false, selected });
                c.selected.sort_by_key(|r| (r.expr, r.name.is_none()));
            }
            region_fixture::lower(&fx, &view, &mut c, &heads, &decls, Plant::None).map(|r| r.lines)
        };
        let clean = run(P0, true);
        assert_eq!(clean, Ok(vec::Vec::new()), "forwarding alias with impl row aligns cleanly: {clean:?}");
        let no_row = run(P0, false);
        assert_eq!(no_row, Ok(vec::Vec::new()), "forwarding alias without impl row falls back to placeholders cleanly: {no_row:?}");
        let bad = run(Region::Static, true);
        assert!(matches!(&bad, Ok(lines) if !lines.is_empty()), "TWIN: &'a does not outlive &'static: {bad:?}");
    }
}
