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
        Some(CalleeFacts { writes_lifetimes: true, self_ref: false })
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
