//! Lane RB10, class "an ADT field is all of the ADT's positions" (NEVER COMPILED when written,
//! 2026-11-18). The Reference, "Generic parameters" and "Struct expressions": a field's type is
//! its declared type at the struct's arguments, so `b.held` with `b: &B<'f, 'a>` and
//! `held: &'a S` has the type `&'a S`, and `B { file, held }` coerces each value to its own
//! field's type only. Before [`Facts::field_regions`], a field place stood for every position
//! of the ADT (lib.rs `project`) and a struct literal went through `unify_call`'s one origin,
//! which tied distinct lifetime parameters: bstd's `Buffered<'f, 'a>` (`held: Bytes<'a>`,
//! fs.rs:2133), `Bytes::arena` (`arena: &'static str`, mem.rs:259), `Task::name`, `Checking {
//! given: self, .. }` (grant/made.rs) each gave a false `UnprovenOutlives` (census 1118-0027).
//!
//! Each case: the bstd shape in miniature, which rustc accepts (no finding); a negative rustc
//! refuses (a finding); and the CONTROL (rule 23): the same accepted body with no
//! `field_regions` row finds again, so the row is the cause.

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, FieldInit, ItemKey, List, Mutbl, Name, PTy, PatId, StructBase};

use crate::region_fixture::{self, Decls, FnHead, P0, P1, P2};
use crate::{AdtFacts, CalleeFacts, Facts, FieldRegion, Form, Plant, Refusal, Region};

/// `struct S;`, `struct B<'f, 'a> { file: &'f S, held: &'a S }`, `struct T<'a> { r: &'a S,
/// name: &'static S }`; each fn's head declared by hand ([`Decls`], D148). `regions` false: no
/// field's declared type is known (the control).
struct World {
    fx: Fx,
    b: ItemKey,
    t: ItemKey,
    rs: PTy,
    bt: PTy,
    tt: PTy,
    regions: bool,
    /// A row that does not line up with `B`'s `held` (a lifetime past `B`'s two).
    misaligned: bool,
    decls: Decls,
}

const B_FILE: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
const B_HELD: [FieldRegion; 1] = [FieldRegion::Lifetime(1)];
const B_PAST: [FieldRegion; 1] = [FieldRegion::Lifetime(2)];
const T_R: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
const T_NAME: [FieldRegion; 1] = [FieldRegion::Static];

impl World {
    fn new() -> World {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let b = fx.strukt("B", 0, &[("file", rs), ("held", rs)]);
        let t = fx.strukt("T", 0, &[("r", rs), ("name", rs)]);
        let bt = fx.adt(b, &[]);
        let tt = fx.adt(t, &[]);
        let mut decls = Decls::new();
        // `B<'f, 'a>` declares two lifetimes, `T<'a>` one (as `Facts::adt` below says).
        decls.adt(b, 2);
        decls.adt(t, 1);
        World { fx, b, t, rs, bt, tt, regions: true, misaligned: false, decls }
    }

    /// The one fn of the case, `name` of `inputs` and `output`, the world then finished; its
    /// head `lifetimes` (the `late` ones late-bound) and its column `slots` over `fn(inputs..)
    /// -> output`.
    fn func(&mut self, name: &str, inputs: &[PTy], output: PTy, lifetimes: u32, late: &[u32], slots: &[Region]) -> ItemKey {
        let f = self.fx.func(name, 0, inputs, output, vec![]);
        self.fx.finish();
        self.decls.func(f, FnHead { lifetimes, late, outlives: &[], inputs, output, slots });
        f
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
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = if k == self.b {
            2
        } else if k == self.t {
            1
        } else {
            0
        };
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    /// `B<'f, 'a> { file: &'f S, held: &'a S }` and `T<'a> { r: &'a S, name: &'static S }`:
    /// each lifetime only under a shared reference, so covariant (the Reference, "Subtyping and
    /// Variance"); `S` has no parameter.
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        Some(region_fixture::all_co(if k == self.b {
            2
        } else if k == self.t {
            1
        } else {
            0
        }))
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: true, self_ref: false, c_variadic: false })
    }
    fn field(&self, adt: ItemKey, _: u32, name: Name) -> Option<u32> {
        let names: &[&str] = if adt == self.b {
            &["file", "held"]
        } else if adt == self.t {
            &["r", "name"]
        } else {
            &[]
        };
        names.iter().position(|n| Name::of(n) == name).map(|i| i as u32)
    }
    fn field_count(&self, adt: ItemKey, _: u32) -> Option<u32> {
        if adt == self.b || adt == self.t { Some(2) } else { None }
    }
    fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        if !self.regions || variant != 0 {
            return None;
        }
        match (adt == self.b, adt == self.t, field) {
            (true, _, 0) => Some(&B_FILE),
            (true, _, 1) if self.misaligned => Some(&B_PAST),
            (true, _, 1) => Some(&B_HELD),
            (_, true, 0) => Some(&T_R),
            (_, true, 1) => Some(&T_NAME),
            _ => None,
        }
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// `base.name`: the fixture has no field-access builder, so it takes the slot of a read of `p`
/// (as tests.rs `field` does).
fn field(b: &mut Bb, p: PatId, base: ExprId, name: &str) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Field { base, name: Name::of(name) };
    at
}

/// `B { <inits> }`: the slot of a path expression `B` made for it (as tests.rs `update` does),
/// its written fields the view's `field_inits[0..n]`.
fn literal(b: &mut Bb, n: u32) -> ExprId {
    let at = b.path(&["B"]);
    let blaze_typeck::Expr::Path(path) = b.exprs[at.ix()] else { panic!("`Bb::path` made other than a path expression") };
    b.exprs[at.ix()] = blaze_typeck::Expr::Struct { path, fields: List { start: 0, len: n }, base: StructBase::None };
    at
}

/// The findings of `root`, the body of `owner`, with the view's `field_inits` set to `inits`
/// and the world's heads laid in its record (region_fixture.rs `run`); or the refusal.
fn run(w: &World, b: &Bb, owner: ItemKey, root: ExprId, inits: &[FieldInit]) -> Result<vec::Vec<String>, Refusal> {
    let mut view = b.view(owner, root);
    view.field_inits = inits;
    region_fixture::run(&w.fx, &view, w, &w.decls, Plant::None).map(|r| r.lines)
}

fn outlives(got: &Result<vec::Vec<String>, Refusal>) -> bool {
    matches!(got, Ok(lines) if lines.iter().any(|l| l.contains("UnprovenOutlives")))
}

/// `fn get<'f, 'a>(x: &B<'f, 'a>) -> &'a S`: `'f` is `P0`, `'a` `P1`, `x`'s elided reference
/// `P2`, all late-bound; the column `[x's &, B's 'f, B's 'a, output]`.
const GET_B: (u32, &[u32], &[Region]) = (3, &[0, 1, 2], &[P2, P0, P1, P1]);
/// `fn get<'a>(x: &T<'a>) -> &'static S`: `'a` is `P0`, `x`'s elided reference `P1`, both
/// late-bound; the column `[x's &, T's 'a, output]`.
const GET_T: (u32, &[u32], &[Region]) = (2, &[0, 1], &[P1, P0, Region::Static]);

/// `fn get<..>(x: &<adt>) -> <out> { x.<name> }` under `regions`, its head `head` (`GET_B`,
/// `GET_T`: lifetimes, late-bound, column).
fn read_field(regions: bool, misaligned: bool, of_t: bool, head: (u32, &[u32], &[Region]), name: &str) -> Result<vec::Vec<String>, Refusal> {
    let mut w = World::new();
    w.regions = regions;
    w.misaligned = misaligned;
    let held = if of_t { w.tt } else { w.bt };
    let input = w.fx.r(Mutbl::Not, held);
    let rs = w.rs;
    let (lifetimes, late, slots) = head;
    let f = w.func("get", &[input], rs, lifetimes, late, slots);
    let mut b = Bb::new();
    let x = b.param("x");
    let lx = b.local(x);
    let read = field(&mut b, x, lx, name);
    let root = b.block(&[], Some(read));
    run(&w, &b, f, root, &[])
}

/// `Buffered::held` in miniature: `fn get<'f, 'a>(x: &B<'f, 'a>) -> &'a S { x.held }`: the
/// field is `&'a S`, nothing asks `'f: 'a` (rustc accepts). NEGATIVE: `x.file` returned as
/// `&'a S` asks `'f: 'a`, which the signature does not give (rustc: "lifetime may not live
/// long enough"). CONTROL: no row, the field is all of `B`'s positions, `'f: 'a` again.
#[test]
fn a_field_of_one_lifetime_takes_only_that_lifetime() {
    let fine = read_field(true, false, false, GET_B, "held");
    std::println!("field_regions held: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let wrong = read_field(true, false, false, GET_B, "file");
    std::println!("field_regions file as 'a: {wrong:?}");
    assert!(outlives(&wrong), "'f: 'a is not given: {wrong:?}");
    // The control: with no row `x.held` stands for both of `B`'s positions, and relating it
    // (two positions) to the `&'a S` result (one) is the coarse relation (lib.rs `relate`,
    // noted `ValueInexact`). Its finding (`'f: 'a`) is the relation's artifact, so the body is
    // refused by the relation's name, never reported and never clean: the row is the cause.
    let control = read_field(false, false, false, GET_B, "held");
    std::println!("field_regions held, no row: {control:?}");
    assert!(
        matches!(control, Err(Refusal { form: Form::Unified { why: crate::Unified::ValueInexact }, .. })),
        "the control is refused by the coarse relation's name: {control:?}"
    );
}

/// `Bytes::arena` and `Task::name` in miniature: `fn get<'a>(x: &T<'a>) -> &'static S {
/// x.name }`, `name: &'static S`: accepted. NEGATIVE: `x.r` (`&'a S`) as `&'static S` asks
/// `'a: 'static`. CONTROL: no row, `x.name` stands for `'a` too and asks `'a: 'static`.
#[test]
fn a_static_field_is_static() {
    let fine = read_field(true, false, true, GET_T, "name");
    std::println!("field_regions name: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let wrong = read_field(true, false, true, GET_T, "r");
    std::println!("field_regions r as 'static: {wrong:?}");
    assert!(outlives(&wrong), "'a: 'static is not given: {wrong:?}");
    let control = read_field(false, false, true, GET_T, "name");
    std::println!("field_regions name, no row: {control:?}");
    assert!(outlives(&control), "the control finds the coarse relation: {control:?}");
}

/// A row past the ADT's lifetimes is refused by name, never related another way (rule 3).
#[test]
fn a_misaligned_row_is_refused() {
    let got = read_field(true, true, false, GET_B, "held");
    std::println!("field_regions misaligned: {got:?}");
    assert!(matches!(got, Err(ref r) if matches!(r.form, Form::FieldRegionsUnaligned { .. })), "{got:?}");
}

/// `fn mk<'f, 'a>(file: &'f S, held: &'a S) -> B<'f, 'a> { B { file: <one>, held: <other> } }`,
/// the two values swapped when `swap`.
fn make(regions: bool, swap: bool) -> Result<vec::Vec<String>, Refusal> {
    let mut w = World::new();
    w.regions = regions;
    let (rs, bt) = (w.rs, w.bt);
    // `fn mk<'f, 'a>(file: &'f S, held: &'a S) -> B<'f, 'a>`: `'f` is `P0`, `'a` `P1`, both
    // late-bound; the column `[file, held, B's 'f, B's 'a]`.
    let f = w.func("mk", &[rs, rs], bt, 2, &[0, 1], &[P0, P1, P0, P1]);
    let mut b = Bb::new();
    let file = b.param("file");
    let held = b.param("held");
    let (lf, lh) = (b.local(file), b.local(held));
    let (first, second) = if swap { (lh, lf) } else { (lf, lh) };
    let lit = literal(&mut b, 2);
    let root = b.block(&[], Some(lit));
    let inits = [FieldInit { name: Name::of("file"), expr: first }, FieldInit { name: Name::of("held"), expr: second }];
    run(&w, &b, f, root, &inits)
}

/// `Checking { given: self, .. }` in miniature: `B { file, held }` puts `'f` into `file`'s
/// position and `'a` into `held`'s only: accepted. NEGATIVE: `B { file: held, held: file }`
/// asks `'a: 'f` and `'f: 'a` (rustc refuses both). CONTROL: no row, the literal's one origin
/// ties `'f` and `'a`.
#[test]
fn a_struct_literal_relates_each_field_to_its_own_positions() {
    let fine = make(true, false);
    std::println!("field_regions literal: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let wrong = make(true, true);
    std::println!("field_regions literal swapped: {wrong:?}");
    assert!(outlives(&wrong), "the swapped literal asks 'a: 'f: {wrong:?}");
    // The control: with no row the literal goes through one hub (lib.rs, noted
    // `AggregateHub`: four positions), which ties `'f` and `'a`; that finding is the hub's
    // artifact, so the body is refused by its name, never reported and never clean.
    let control = make(false, false);
    std::println!("field_regions literal, no row: {control:?}");
    assert!(
        matches!(control, Err(Refusal { form: Form::Unified { why: crate::Unified::AggregateHub }, .. })),
        "the control is refused by the coarse relation's name: {control:?}"
    );
}

/// Region-erased witness lowering (`lower_async_witness`, `Erased` / `Positions` grain) must not
/// run `field_slot` against `RegionSlots`-laid `field_regions` rows: under `Positions`, an
/// associated-type field (`Role::FinalResult` in `tokio_tungstenite::handshake::handshake`) counts
/// 1 position while `RegionSlots` lays `[FieldRegion::Arg(0)]` (0 positions for `Param(0)`), which
/// previously refused with `FieldRegionsUnaligned` and prevented building the coroutine state recipe.
#[test]
fn witness_lowering_skips_region_slot_field_alignment() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(blaze_typeck::LangItem::Sized, sized);
    let future = fx.tr("Future", 1, vec![], &["Output"]);
    fx.lang(blaze_typeck::LangItem::Future, future);
    let role_tr = fx.tr("Role", 1, vec![], &["Out"]);
    let p0 = fx.param(0);
    let role_out = fx.alias(role_tr, "Out", &[p0]);
    let holder = fx.strukt("Holder", 1, &[("out", role_out)]);
    let holder_p0 = fx.adt(holder, &[p0]);
    let role_bound = fx.bound(role_tr, &[p0]);
    let unit = fx.unit();
    let f = fx.func("handshake", 1, &[holder_p0], unit, vec![role_bound]);
    fx.finish();

    let mut b = Bb::new();
    let x = b.param("x");
    let lx = b.local(x);
    let read = field(&mut b, x, lx, "out");
    let inner = b.block(&[], Some(read));
    let ab = b.async_block(inner, blaze_typeck::Capture::Move);
    let s = b.semi(ab);
    let root = b.block(&[s], None);
    let view = b.view(f, root);
    let mut cx = blaze_typeck::InferCtx::new_in(region_fixture::alloc());
    let needs = blaze_typeck::Needs::new_in(region_fixture::alloc());
    let blaze_typeck::Outcome::Done(ck) = blaze_typeck::check(&view, &fx, &mut cx, &needs, region_fixture::alloc()).expect("check") else {
        panic!("expected Done");
    };
    assert!(ck.diags.is_empty(), "diags: {:?}", ck.diags);

    const HOLDER_OUT: [FieldRegion; 1] = [FieldRegion::Arg(0)];
    struct WitnessWorld {
        holder: ItemKey,
    }
    impl Facts for WitnessWorld {
        fn regions(&self) -> &dyn crate::RegionFacts { &crate::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn adt_variances(&self, _: ItemKey) -> Option<&[crate::Variance]> { Some(&[]) }
        fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_bodies_checked(&self) -> bool { false }
        fn closure_requirements(&self, _: ExprId) -> &[crate::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { Some(0) }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { Some(0) }
        fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> { None }
        fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
            (adt == self.holder && variant == 0 && name == Name::of("out")).then_some(0)
        }
        fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
            (adt == self.holder && variant == 0).then_some(1)
        }
        fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
            (adt == self.holder && variant == 0 && field == 0).then_some(&HOLDER_OUT[..])
        }
        fn param_copy(&self, _: u32) -> bool { false }
    }

    let witness = crate::lower_async_witness(&view, &ck, ab, &WitnessWorld { holder }, region_fixture::alloc());
    assert!(witness.is_ok(), "witness lowering must not refuse RegionSlots field_regions with FieldRegionsUnaligned: {:?}", witness.err());
}

/// Associated-type ADT fields with zero region slots (the 4 remaining `PositionsNotSlots` bodies:
/// `blaze-format-core`'s `Slice::count`, `Slice::follow_as`, `Verified<Slice>::items` reading
/// `self.len: O::Len`, and `blaze-bitflags`'s `IterNames::remaining` reading `self.remaining:
/// F::Bits`) have no `field_regions` row because `PNode::Alias` is unlaid at the ADT head.
/// Under `RegionSlots`, a projected field whose instantiated type carries 0 slots is an exact
/// empty slot slice (`len = 0, exact = true`), while:
/// - Positive: passing `s.len` to a call `count_len(s.len)` succeeds cleanly (no `PositionsNotSlots`,
///   no false coupling to `Slice`'s own `'a` lifetime).
/// - Negative 1 (param outlives): passing `s.len` (`O::Len`) to `need_static<X: 'static>(x: X)`
///   without `O: 'static` is STILL rejected with `UnprovenOutlives` (`ρ_O: 'static`); with
///   `O: 'static` it succeeds.
/// - Negative 2 (reborrow): returning `&s.len` as `&'static O::Len` from `s: &'a Slice<'a, O>`
///   is STILL rejected with `UnprovenOutlives` (`'a: 'static`), proving `parent`/`deref` stay intact.
#[test]
fn associated_type_field_with_zero_slots_passes_one_rule_and_checks_outlives() {
    #[derive(Clone, Copy)]
    enum Case {
        Count,
        NeedStaticUnbounded,
        NeedStaticBounded,
        BadReborrow,
    }

    fn run_case(case: Case) -> Result<vec::Vec<String>, Refusal> {
        let mut fx = Fx::new();
        let u8t = fx.u8();
        let unit = fx.unit();
        let ru8 = fx.r(Mutbl::Not, u8t);
        let offset_tr = fx.tr("Offset", 1, vec![], &["Len"]);
        let p0 = fx.param(0);
        let o_len = fx.alias(offset_tr, "Len", &[p0]);
        let r_o_len = fx.r(Mutbl::Not, o_len);
        // `struct Slice<'a, O: Offset> { tag: &'a u8, len: O::Len }` (has BOTH a lifetime `'a`
        // so parent `pi.len == 1 > 0` AND a `PNode::Alias` field with no `field_regions` row).
        let slice = fx.strukt("Slice", 1, &[("tag", ru8), ("len", o_len)]);
        let slice_p0 = fx.adt(slice, &[p0]);
        let r_slice = fx.r(Mutbl::Not, slice_p0);
        let offset_bound = fx.bound(offset_tr, &[p0]);

        let count_fn = fx.func("count_len", 1, &[o_len], u8t, vec![offset_bound]);
        let need_static_fn = fx.func("need_static", 1, &[p0], unit, vec![]);
        let (caller_out, caller_slots): (PTy, &[Region]) = match case {
            Case::Count => (u8t, &[P0, P0]),
            Case::NeedStaticUnbounded | Case::NeedStaticBounded => (unit, &[P0, P0]),
            Case::BadReborrow => (r_o_len, &[P0, P0, Region::Static]),
        };
        let caller = fx.func("caller", 1, &[r_slice], caller_out, vec![offset_bound]);
        fx.finish();

        let mut decls = Decls::new();
        decls.adt(slice, 1);
        decls.trait_(offset_tr, 0);
        decls.alias(offset_tr, "Len", 0);
        decls.func(count_fn, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[o_len], output: u8t, slots: &[] });
        decls.func(need_static_fn, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[p0], output: unit, slots: &[] });
        decls.clause(need_static_fn, 0, 0, p0, &[]);
        decls.type_outlives(need_static_fn, 0, Region::Static);
        decls.func(caller, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[r_slice], output: caller_out, slots: caller_slots });
        if matches!(case, Case::NeedStaticBounded) {
            decls.clause(caller, 0, 0, p0, &[]);
            decls.type_outlives(caller, 0, Region::Static);
        }

        struct AliasWorld {
            slice: ItemKey,
            offset_tr: ItemKey,
            caller: ItemKey,
            param_static: bool,
        }
        impl Facts for AliasWorld {
            fn regions(&self) -> &dyn crate::RegionFacts { &crate::NoRegions }
            fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
            fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { None }
            fn constant_initializer(&self) -> bool { false }
            fn fn_binders(&self) -> bool { true }
            fn closure_bodies_checked(&self) -> bool { false }
            fn closure_requirements(&self, _: ExprId) -> &[crate::Requirement] { &[] }
            fn async_bodies_checked(&self) -> bool { false }
            fn is_future(&self, _: PTy) -> Option<bool> { None }
            fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
            fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
            fn call_bounds(&self, item: ItemKey) -> Option<crate::CallBounds<'_>> {
                const P0_STATIC: [(crate::OutlivesType, Region); 1] = [(crate::OutlivesType::Param(0), Region::Static)];
                if item == self.caller {
                    Some(crate::CallBounds {
                        outlives: if self.param_static { &P0_STATIC } else { &[] },
                        trait_bounded: &[],
                        unread_below: 0,
                    })
                } else {
                    None
                }
            }
            fn trait_lifetimes(&self, tr: ItemKey) -> Option<u32> { (tr == self.offset_tr).then_some(0) }
            fn alias_lifetimes(&self, tr: ItemKey, name: Name) -> Option<u32> {
                (tr == self.offset_tr && name == Name::of("Len")).then_some(0)
            }
            fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
            fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
            fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
            fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
            fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
                let lifetimes = u32::from(k == self.slice);
                Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
            }
            fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
                Some(region_fixture::all_co(if k == self.slice { 2 } else { 0 }))
            }
            fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
                Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
            }
            fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
                if adt != self.slice || variant != 0 {
                    return None;
                }
                if name == Name::of("tag") {
                    Some(0)
                } else if name == Name::of("len") {
                    Some(1)
                } else {
                    None
                }
            }
            fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
                (adt == self.slice && variant == 0).then_some(2)
            }
            fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
                // `tag` has a row; `len` (`O::Len`) has NO row (exact match to `borrow_fields.rs`).
                const TAG: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
                (adt == self.slice && variant == 0 && field == 0).then_some(&TAG[..])
            }
            fn param_copy(&self, _: u32) -> bool { true }
            fn alias_copy(&self, _: PTy) -> bool { true }
        }

        let mut b = Bb::new();
        let s = b.param("s");
        let ls = b.local(s);
        let read_len = field(&mut b, s, ls, "len");
        let tail = match case {
            Case::Count => {
                let bp0 = b.ty(blaze_typeck::PNode::Param(0));
                let callee = b.path_args("count_len", &[bp0]);
                b.call(callee, &[read_len])
            }
            Case::NeedStaticUnbounded | Case::NeedStaticBounded => {
                let callee = b.path(&["need_static"]);
                b.call(callee, &[read_len])
            }
            Case::BadReborrow => b.borrow(Mutbl::Not, read_len),
        };
        let root = b.block(&[], Some(tail));
        let view = b.view(caller, root);
        region_fixture::run(
            &fx,
            &view,
            &AliasWorld { slice, offset_tr, caller, param_static: matches!(case, Case::NeedStaticBounded) },
            &decls,
            Plant::None,
        )
        .map(|r| r.lines)
    }

    let ok_count = run_case(Case::Count);
    assert_eq!(ok_count, Ok(vec::Vec::new()), "POSITIVE: `count_len(s.len)` must succeed without PositionsNotSlots: {ok_count:?}");

    let bad_static = run_case(Case::NeedStaticUnbounded);
    assert!(outlives(&bad_static), "NEGATIVE: `need_static(s.len)` without `O: 'static` must report UnprovenOutlives: {bad_static:?}");

    let ok_static = run_case(Case::NeedStaticBounded);
    assert_eq!(ok_static, Ok(vec::Vec::new()), "POSITIVE: `need_static(s.len)` with `O: 'static` must succeed: {ok_static:?}");

    let bad_reborrow = run_case(Case::BadReborrow);
    assert!(outlives(&bad_reborrow), "NEGATIVE: `&s.len` returned as `&'static O::Len` must report UnprovenOutlives: {bad_reborrow:?}");
}

/// NEGATIVE SOUNDNESS: an associated-type field whose trait carries a lifetime (`T: Tr<'a>`,
/// `out: T::Out`, `RegionSlots::count(T::Out) == 1 > 0`) with no `field_regions` row MUST NOT be
/// collapsed to `len = 0, exact = true` — passing it to a call must still refuse with
/// `PositionsNotSlots` rather than silently dropping the trait lifetime `'a`.
#[test]
fn associated_type_field_with_nonzero_slots_still_refuses_positions_not_slots() {
    let mut fx = Fx::new();
    let unit = fx.unit();
    let tr = fx.tr("Tr", 1, vec![], &["Out"]);
    let p0 = fx.param(0);
    let t_out = fx.alias(tr, "Out", &[p0]);
    let wrap = fx.strukt("Wrap", 1, &[("out", t_out)]);
    let wrap_p0 = fx.adt(wrap, &[p0]);
    let r_wrap = fx.r(Mutbl::Not, wrap_p0);
    let tr_bound = fx.bound(tr, &[p0]);

    let consume = fx.func("consume", 1, &[p0], unit, vec![]);
    let caller = fx.func("caller", 1, &[r_wrap], unit, vec![tr_bound]);
    fx.finish();

    let mut decls = Decls::new();
    decls.adt(wrap, 1);
    decls.trait_(tr, 1);
    decls.alias(tr, "Out", 1);
    decls.func(consume, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[p0], output: unit, slots: &[] });
    decls.func(caller, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[r_wrap], output: unit, slots: &[P0, P0] });

    struct NonZeroAliasWorld {
        wrap: ItemKey,
        tr: ItemKey,
    }
    impl Facts for NonZeroAliasWorld {
        fn regions(&self) -> &dyn crate::RegionFacts { &crate::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_bodies_checked(&self) -> bool { false }
        fn closure_requirements(&self, _: ExprId) -> &[crate::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, k: ItemKey) -> Option<u32> { (k == self.tr).then_some(1) }
        fn alias_lifetimes(&self, k: ItemKey, name: Name) -> Option<u32> {
            (k == self.tr && name == Name::of("Out")).then_some(1)
        }
        fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
            let lifetimes = u32::from(k == self.wrap);
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
        }
        fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
            Some(region_fixture::all_co(if k == self.wrap { 2 } else { 0 }))
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
        }
        fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
            (adt == self.wrap && variant == 0 && name == Name::of("out")).then_some(0)
        }
        fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
            (adt == self.wrap && variant == 0).then_some(1)
        }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[FieldRegion]> { None }
        fn param_copy(&self, _: u32) -> bool { true }
        fn alias_copy(&self, _: PTy) -> bool { true }
    }

    let mut b = Bb::new();
    let w = b.param("w");
    let lw = b.local(w);
    let read_out = field(&mut b, w, lw, "out");
    let callee = b.path(&["consume"]);
    let call = b.call(callee, &[read_out]);
    let root = b.block(&[], Some(call));
    let view = b.view(caller, root);
    let got = region_fixture::run(&fx, &view, &NonZeroAliasWorld { wrap, tr }, &decls, Plant::None);
    assert!(
        matches!(got, Err(Refusal { form: Form::OneRule { why: crate::OneRuleWhy::PositionsNotSlots { .. }, .. }, .. })),
        "NEGATIVE: non-zero-slot alias field with no field_regions row must still refuse PositionsNotSlots: {got:?}"
    );
}

/// Shared `Facts` table for normalized-alias field tests (one `Facts` type avoids redundant
/// monomorphizations of `Lower<F, A, R>` in the test binary).
struct FieldAliasWorld {
    traits: vec::Vec<(ItemKey, u32)>,
    aliases: vec::Vec<(ItemKey, &'static str, u32)>,
    adts: vec::Vec<(ItemKey, u32, u32, &'static [&'static str])>,
    field_regions: vec::Vec<(ItemKey, u32, &'static [FieldRegion])>,
    writes_lifetimes: bool,
}

impl Facts for FieldAliasWorld {
    fn regions(&self) -> &dyn crate::RegionFacts { &crate::NoRegions }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
    fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { None }
    fn constant_initializer(&self) -> bool { false }
    fn fn_binders(&self) -> bool { true }
    fn closure_bodies_checked(&self) -> bool { false }
    fn closure_requirements(&self, _: ExprId) -> &[crate::Requirement] { &[] }
    fn async_bodies_checked(&self) -> bool { false }
    fn is_future(&self, _: PTy) -> Option<bool> { None }
    fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
    fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
    fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> { None }
    fn trait_lifetimes(&self, tr: ItemKey) -> Option<u32> {
        self.traits.iter().find(|&&(k, _)| k == tr).map(|&(_, n)| n)
    }
    fn alias_lifetimes(&self, tr: ItemKey, name: Name) -> Option<u32> {
        self.aliases.iter().find(|&&(k, s, _)| k == tr && Name::of(s) == name).map(|&(_, _, n)| n)
    }
    fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
    fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
    fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
    fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = self.adts.iter().find(|&&(a, ..)| a == k).map_or(0, |&(_, lts, ..)| lts);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        let n = self.adts.iter().find(|&&(a, ..)| a == k).map_or(0, |&(_, _, v, _)| v);
        Some(region_fixture::all_co(n))
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: self.writes_lifetimes, self_ref: false, c_variadic: false })
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        if variant != 0 {
            return None;
        }
        let &(_, _, _, names) = self.adts.iter().find(|&&(a, ..)| a == adt)?;
        names.iter().position(|n| Name::of(n) == name).map(|i| i as u32)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        if variant != 0 {
            return None;
        }
        let &(_, _, _, names) = self.adts.iter().find(|&&(a, ..)| a == adt)?;
        Some(names.len() as u32)
    }
    fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        if variant != 0 {
            return None;
        }
        self.field_regions.iter().find(|&&(a, f, _)| a == adt && f == field).map(|&(_, _, r)| r)
    }
    fn param_copy(&self, _: u32) -> bool { false }
}

/// Exact `run_parent.rs::lowering_counts` shape:
/// `struct Lazy<'r, 'a, F: Front> { cx: &'r u8, started: Started<'a, F::Start, F::Maker> }`
/// under `fn f<'r, 'a, 'h, F: Front<Maker = RunMaker<'h>>>(self_: &Lazy<'r, 'a, F>)`:
/// `FieldAliasGuard` suppresses `field_regions(Lazy, 0, 1)` to `None` because `F::Maker`
/// normalizes to `RunMaker<'h>` in this body. `field_slot_with_aliases` resolves `self_.started`
/// from `Facts::regions().field(Lazy, 0, 1)` + owner `ExProj` clauses to exact slots `['a, 'h]`
/// (never touching `'r`), avoiding `Unified::BorrowInexact` on `&self_.started` while preserving
/// exact outlives and conflict checking:
/// - POSITIVE 1: borrowing `&self_.started` succeeds with `coarse.is_none()` and 0 findings.
/// - POSITIVE 2: reading `(&self_.started).heavy.maker.data` as `&'h u8` succeeds with `coarse.is_none()`.
/// - CONTROL (rule 23): omitting `regions().field(Lazy, 0, 1)` reproduces `Unified::BorrowInexact`.
/// - NEGATIVE 1..3: escaping `&'h u8` as `&'static u8`, `&'a u8`, or `&'r u8` reports `UnprovenOutlives`.
/// - NEGATIVE 4: conflicting `&self_.started` and `&mut self_.started` reports a borrow conflict.
#[test]
fn normalized_alias_field_under_where_projection_bound_avoids_borrow_inexact() {
    const P3: Region = Region::Param(3);

    #[derive(Clone, Copy)]
    enum Case {
        BorrowStarted,
        ReturnH,
        ControlNoFieldCol,
        EscapeStatic,
        EscapeA,
        EscapeR,
        ConflictingBorrow,
    }

    fn run_case(case: Case) -> Result<region_fixture::Run, Refusal> {
        let mut fx = Fx::new();
        let u8t = fx.u8();
        let ru8 = fx.r(Mutbl::Not, u8t);
        let front = fx.tr("Front", 1, vec![], &["Start", "Maker"]);
        let p0 = fx.param(0);
        let p1 = fx.param(1);
        let f_start = fx.alias(front, "Start", &[p0]);
        let f_maker = fx.alias(front, "Maker", &[p0]);
        let run_maker = fx.strukt("RunMaker", 0, &[("data", ru8)]);
        let run_maker_ty = fx.adt(run_maker, &[]);
        let heavy = fx.strukt("Heavy", 1, &[("maker", p0)]);
        let heavy_p1 = fx.adt(heavy, &[p1]);
        let started = fx.strukt("Started", 2, &[("tag", ru8), ("start", p0), ("heavy", heavy_p1)]);
        let started_decl = fx.adt(started, &[f_start, f_maker]);
        let started_norm = fx.adt(started, &[f_start, run_maker_ty]);
        let r_started_norm = fx.r(Mutbl::Not, started_norm);
        let lazy = fx.strukt("Lazy", 1, &[("cx", ru8), ("started", started_decl)]);
        let lazy_f = fx.adt(lazy, &[p0]);
        let self_mut = matches!(case, Case::ConflictingBorrow);
        let r_lazy = fx.r(if self_mut { Mutbl::Mut } else { Mutbl::Not }, lazy_f);

        let front_bound = fx.bound(front, &[p0]);
        let maker_proj = fx.proj(front, &[p0], "Maker", run_maker_ty);
        let use_started = fx.func("use_started", 1, &[r_started_norm], u8t, vec![front_bound, maker_proj]);

        // Owner lifetimes: P0 = 'r, P1 = 'a, P2 = 'h, P3 = 'self.
        let (ret_ty, caller_slots): (PTy, &[Region]) = match case {
            Case::BorrowStarted | Case::ControlNoFieldCol | Case::ConflictingBorrow => (u8t, &[P3, P0, P1]),
            Case::ReturnH => (ru8, &[P3, P0, P1, P2]),
            Case::EscapeStatic => (ru8, &[P3, P0, P1, Region::Static]),
            Case::EscapeA => (ru8, &[P3, P0, P1, P1]),
            Case::EscapeR => (ru8, &[P3, P0, P1, P0]),
        };
        let caller = fx.func("lowering_counts", 1, &[r_lazy], ret_ty, vec![front_bound, maker_proj]);
        let ex_maker_args = fx.list(&[p0]);
        let ex_maker = fx.ty(blaze_typeck::PNode::ExProj {
            trait_: front,
            name: Name::of("Maker"),
            args: ex_maker_args,
            term: run_maker_ty,
        });
        fx.finish();

        let mut decls = Decls::new();
        decls.adt(run_maker, 1);
        decls.adt(heavy, 0);
        decls.adt(started, 1);
        decls.adt(lazy, 2);
        decls.trait_(front, 0);
        decls.alias(front, "Start", 0);
        decls.alias(front, "Maker", 0);
        if !matches!(case, Case::ControlNoFieldCol) {
            // Declared column of `Lazy<'r, 'a, F>::started`: `Started<'a, F::Start, F::Maker>` -> `[P1]`.
            decls.field(lazy, 0, 1, started_decl, &[P1]);
        }
        decls.func(
            use_started,
            FnHead { lifetimes: 3, late: &[0, 1, 2], outlives: &[], inputs: &[r_started_norm], output: u8t, slots: &[P0, P1, P2] },
        );
        decls.func(
            caller,
            FnHead { lifetimes: 4, late: &[0, 1, 3], outlives: &[], inputs: &[r_lazy], output: ret_ty, slots: caller_slots },
        );
        decls.clause(caller, 1, 0, ex_maker, &[P2]);

        const RM_DATA: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
        const HEAVY_MAKER: [FieldRegion; 1] = [FieldRegion::Arg(0)];
        const STARTED_TAG: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
        const STARTED_START: [FieldRegion; 1] = [FieldRegion::Arg(0)];
        const STARTED_HEAVY: [FieldRegion; 1] = [FieldRegion::Arg(1)];
        const LAZY_CX: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];

        let world = FieldAliasWorld {
            traits: vec![(front, 0)],
            aliases: vec![(front, "Start", 0), (front, "Maker", 0)],
            adts: vec![
                (run_maker, 1, 1, &["data"]),
                (heavy, 0, 1, &["maker"]),
                (started, 1, 3, &["tag", "start", "heavy"]),
                (lazy, 2, 3, &["cx", "started"]),
            ],
            // `Lazy::started` (field 1) intentionally omitted (FieldAliasGuard).
            field_regions: vec![
                (run_maker, 0, &RM_DATA),
                (heavy, 0, &HEAVY_MAKER),
                (started, 0, &STARTED_TAG),
                (started, 1, &STARTED_START),
                (started, 2, &STARTED_HEAVY),
                (lazy, 0, &LAZY_CX),
            ],
            writes_lifetimes: true,
        };

        let mut b = Bb::new();
        let s = b.param("self_");
        let root = match case {
            Case::BorrowStarted | Case::ControlNoFieldCol => {
                let ls = b.local(s);
                let st = field(&mut b, s, ls, "started");
                let r_st = b.borrow(Mutbl::Not, st);
                let bp0 = b.ty(blaze_typeck::PNode::Param(0));
                let callee = b.path_args("use_started", &[bp0]);
                let call = b.call(callee, &[r_st]);
                b.block(&[], Some(call))
            }
            Case::ReturnH | Case::EscapeStatic | Case::EscapeA | Case::EscapeR => {
                let ls = b.local(s);
                let st = field(&mut b, s, ls, "started");
                let r_st = b.borrow(Mutbl::Not, st);
                let hv = field(&mut b, s, r_st, "heavy");
                let mk = field(&mut b, s, hv, "maker");
                let dt = field(&mut b, s, mk, "data");
                b.block(&[], Some(dt))
            }
            Case::ConflictingBorrow => {
                let ls1 = b.local(s);
                let st1 = field(&mut b, s, ls1, "started");
                let r1 = b.borrow(Mutbl::Not, st1);
                let rp = b.bind("r");
                let let_r = b.let_(rp, None, r1);
                let ls2 = b.local(s);
                let st2 = field(&mut b, s, ls2, "started");
                let m = b.borrow(Mutbl::Mut, st2);
                let mp = b.bind("m");
                let let_m = b.let_(mp, None, m);
                let lr = b.local(rp);
                let bp0 = b.ty(blaze_typeck::PNode::Param(0));
                let callee = b.path_args("use_started", &[bp0]);
                let call = b.call(callee, &[lr]);
                b.block(&[let_r, let_m], Some(call))
            }
        };
        let view = b.view(caller, root);
        region_fixture::run(&fx, &view, &world, &decls, Plant::None)
    }

    let ok_borrow = run_case(Case::BorrowStarted).expect("POSITIVE 1: &self.started must not refuse");
    assert!(ok_borrow.lines.is_empty(), "POSITIVE 1: expected 0 findings, got {:?}", ok_borrow.lines);
    assert!(ok_borrow.coarse.is_none(), "POSITIVE 1: expected coarse.is_none(), got {:?}", ok_borrow.coarse);

    let ok_h = run_case(Case::ReturnH).expect("POSITIVE 2: returning &'h u8 must not refuse");
    assert!(ok_h.lines.is_empty(), "POSITIVE 2: expected 0 findings, got {:?}", ok_h.lines);
    assert!(ok_h.coarse.is_none(), "POSITIVE 2: expected coarse.is_none(), got {:?}", ok_h.coarse);

    let control = run_case(Case::ControlNoFieldCol);
    assert!(
        matches!(control, Err(Refusal { form: Form::Unified { why: crate::Unified::BorrowInexact }, .. })),
        "CONTROL: without regions().field(Lazy::started), &self.started must hit BorrowInexact: {control:?}"
    );

    let bad_static = run_case(Case::EscapeStatic).expect("NEGATIVE 1: must produce finding, not refusal");
    assert!(bad_static.lines.iter().any(|l| l.contains("UnprovenOutlives")), "NEGATIVE 1: &'h as &'static must fail: {:?}", bad_static.lines);

    let bad_a = run_case(Case::EscapeA).expect("NEGATIVE 2: must produce finding, not refusal");
    assert!(bad_a.lines.iter().any(|l| l.contains("UnprovenOutlives")), "NEGATIVE 2: &'h as &'a must fail: {:?}", bad_a.lines);

    let bad_r = run_case(Case::EscapeR).expect("NEGATIVE 3: must produce finding, not refusal");
    assert!(bad_r.lines.iter().any(|l| l.contains("UnprovenOutlives")), "NEGATIVE 3: &'h as &'r must fail: {:?}", bad_r.lines);

    let bad_conflict = run_case(Case::ConflictingBorrow).expect("NEGATIVE 4: must produce finding, not refusal");
    assert!(!bad_conflict.lines.is_empty(), "NEGATIVE 4: &mut self.started while &self.started is live must fail");
}

/// Concrete impl normalization of a `PNode::Alias` field:
/// `struct Holder<'r, 'a, F: Front> { cx: &'r u8, started: Started<'a, F::Maker> }`
/// instantiated as `Holder<'r, 'a, MyFront<'x>>` with `impl<'t> Front for MyFront<'t> { type Maker = RunMaker<'t>; }`:
/// - POSITIVE: borrowing `&h.started` and reading `(&h.started).maker.data` as `&'x u8` succeeds with `coarse.is_none()`.
/// - NEGATIVE: escaping `&'x u8` as `&'a u8` reports `UnprovenOutlives`.
#[test]
fn normalized_alias_field_via_selected_impl_avoids_borrow_inexact() {
    use crate::region_fixture::ImplHead;
    const P3: Region = Region::Param(3);

    fn run_case(escape_a: bool) -> Result<region_fixture::Run, Refusal> {
        let mut fx = Fx::new();
        let u8t = fx.u8();
        let ru8 = fx.r(Mutbl::Not, u8t);
        let front = fx.tr("Front", 1, vec![], &["Maker"]);
        let p0 = fx.param(0);
        let f_maker = fx.alias(front, "Maker", &[p0]);
        let run_maker = fx.strukt("RunMaker", 0, &[("data", ru8)]);
        let run_maker_ty = fx.adt(run_maker, &[]);
        let my_front = fx.strukt("MyFront", 0, &[("tag", ru8)]);
        let my_front_ty = fx.adt(my_front, &[]);
        let imp = fx.imp("Front_MyFront", 0, my_front_ty, Some((front, &[])), &[("Maker", run_maker_ty)], vec![]);
        let started = fx.strukt("Started", 1, &[("tag", ru8), ("maker", p0)]);
        let started_decl = fx.adt(started, &[f_maker]);
        let holder = fx.strukt("Holder", 1, &[("cx", ru8), ("started", started_decl)]);
        let holder_concrete = fx.adt(holder, &[my_front_ty]);
        let r_holder = fx.r(Mutbl::Not, holder_concrete);
        // P0 = 'r, P1 = 'a, P2 = 'x, P3 = 'self.
        let ret_slot = if escape_a { P1 } else { P2 };
        let caller = fx.func("get_x", 0, &[r_holder], ru8, vec![]);
        fx.finish();

        let mut decls = Decls::new();
        decls.adt(run_maker, 1);
        decls.adt(my_front, 1);
        decls.adt(started, 1);
        decls.adt(holder, 2);
        decls.trait_(front, 0);
        decls.alias(front, "Maker", 0);
        decls.field(holder, 0, 1, started_decl, &[P1]);
        decls.imp(
            imp,
            ImplHead { lifetimes: 1, outlives: &[], self_ty: my_front_ty, self_slots: &[P0], trait_ref: Some((front, &[], &[])) },
        );
        decls.impl_assoc(imp, "Maker", run_maker_ty, &[P0]);
        decls.func(
            caller,
            FnHead { lifetimes: 4, late: &[0, 1, 2, 3], outlives: &[], inputs: &[r_holder], output: ru8, slots: &[P3, P0, P1, P2, ret_slot] },
        );

        const RM_DATA: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
        const STARTED_TAG: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];
        const STARTED_MAKER: [FieldRegion; 1] = [FieldRegion::Arg(0)];
        const HOLDER_CX: [FieldRegion; 1] = [FieldRegion::Lifetime(0)];

        let world = FieldAliasWorld {
            traits: vec![(front, 0)],
            aliases: vec![(front, "Maker", 0)],
            adts: vec![
                (run_maker, 1, 1, &["data"]),
                (my_front, 1, 1, &["tag"]),
                (started, 1, 2, &["tag", "maker"]),
                (holder, 2, 3, &["cx", "started"]),
            ],
            // `Holder::started` (field 1) intentionally omitted (FieldAliasGuard).
            field_regions: vec![
                (run_maker, 0, &RM_DATA),
                (started, 0, &STARTED_TAG),
                (started, 1, &STARTED_MAKER),
                (holder, 0, &HOLDER_CX),
            ],
            writes_lifetimes: false,
        };

        let mut b = Bb::new();
        let h = b.param("h");
        let lh = b.local(h);
        let st = field(&mut b, h, lh, "started");
        let r_st = b.borrow(Mutbl::Not, st);
        let mk = field(&mut b, h, r_st, "maker");
        let dt = field(&mut b, h, mk, "data");
        let root = b.block(&[], Some(dt));
        let view = b.view(caller, root);
        region_fixture::run(&fx, &view, &world, &decls, Plant::None)
    }

    let ok = run_case(false).expect("POSITIVE: concrete impl field alias must not refuse");
    assert!(ok.lines.is_empty(), "POSITIVE: expected 0 findings, got {:?}", ok.lines);
    assert!(ok.coarse.is_none(), "POSITIVE: expected coarse.is_none(), got {:?}", ok.coarse);

    let bad = run_case(true).expect("NEGATIVE: &'x as &'a must produce finding, not refusal");
    assert!(bad.lines.iter().any(|l| l.contains("UnprovenOutlives")), "NEGATIVE: expected UnprovenOutlives, got {:?}", bad.lines);
}
