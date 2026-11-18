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

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, FieldInit, InferCtx, ItemKey, List, Mutbl, Name, Needs, Outcome, PTy, PatId, StructBase};

use crate::{AdtFacts, CalleeFacts, Facts, FieldRegion, Form, Plant, Refusal, SigBuf, SigLife, lower_planted, read_signature};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `struct S;`, `struct B<'f, 'a> { file: &'f S, held: &'a S }`, `struct T<'a> { r: &'a S,
/// name: &'static S }`; each fn's lifetime part read from its text. `regions` false: no field's
/// declared type is known (the control).
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
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
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
        World { fx, b, t, rs, bt, tt, regions: true, misaligned: false, sigs: vec::Vec::new() }
    }

    /// The one fn of the case, `name` of `inputs` and `output`; the world is then finished and
    /// the fn's lifetime part read from `text` (as storage_end_tests.rs reads after `finish`).
    fn func(&mut self, name: &str, text: &str, inputs: &[PTy], output: PTy) -> ItemKey {
        let f = self.fx.func(name, 0, inputs, output, vec![]);
        self.fx.finish();
        let (b, t) = (self.b, self.t);
        let lifetimes = move |k: ItemKey| Some(if k == b { 2 } else if k == t { 1 } else { 0 });
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature(text, self.fx.t.view(), inputs, output, &lifetimes, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        self.sigs.push((f, buf));
        f
    }
}

impl Facts for World {
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
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: true, self_ref: false })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
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

/// The findings of `root`, the body of `owner`, with the view's `field_inits` set to `inits`;
/// or the refusal.
fn run(w: &World, b: &Bb, owner: ItemKey, root: ExprId, inits: &[FieldInit]) -> Result<vec::Vec<String>, Refusal> {
    let a = alloc();
    let mut view = b.view(owner, root);
    view.field_inits = inits;
    let mut cx = InferCtx::new_in(a);
    let c = match blaze_typeck::check(&view, &w.fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let lowered = lower_planted(&view, &c, w, Plant::None, a)?;
    let rec = check(&lowered.flow, a).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok(out)
}

fn outlives(got: &Result<vec::Vec<String>, Refusal>) -> bool {
    matches!(got, Ok(lines) if lines.iter().any(|l| l.contains("UnprovenOutlives")))
}

/// `fn get<..>(x: &<adt>) -> <out> { x.<name> }` under `regions`, its text `text`.
fn read_field(regions: bool, misaligned: bool, of_t: bool, text: &str, name: &str) -> Result<vec::Vec<String>, Refusal> {
    let mut w = World::new();
    w.regions = regions;
    w.misaligned = misaligned;
    let held = if of_t { w.tt } else { w.bt };
    let input = w.fx.r(Mutbl::Not, held);
    let rs = w.rs;
    let f = w.func("get", text, &[input], rs);
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
    let sig = "fn get<'f, 'a>(x: &B<'f, 'a>) -> &'a S { x.held }";
    let fine = read_field(true, false, false, sig, "held");
    std::println!("field_regions held: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let wrong = read_field(true, false, false, "fn get<'f, 'a>(x: &B<'f, 'a>) -> &'a S { x.file }", "file");
    std::println!("field_regions file as 'a: {wrong:?}");
    assert!(outlives(&wrong), "'f: 'a is not given: {wrong:?}");
    let control = read_field(false, false, false, sig, "held");
    std::println!("field_regions held, no row: {control:?}");
    assert!(outlives(&control), "the control finds the coarse relation: {control:?}");
}

/// `Bytes::arena` and `Task::name` in miniature: `fn get<'a>(x: &T<'a>) -> &'static S {
/// x.name }`, `name: &'static S`: accepted. NEGATIVE: `x.r` (`&'a S`) as `&'static S` asks
/// `'a: 'static`. CONTROL: no row, `x.name` stands for `'a` too and asks `'a: 'static`.
#[test]
fn a_static_field_is_static() {
    let sig = "fn get<'a>(x: &T<'a>) -> &'static S { x.name }";
    let fine = read_field(true, false, true, sig, "name");
    std::println!("field_regions name: {fine:?}");
    assert_eq!(fine, Ok(vec::Vec::new()));
    let wrong = read_field(true, false, true, "fn get<'a>(x: &T<'a>) -> &'static S { x.r }", "r");
    std::println!("field_regions r as 'static: {wrong:?}");
    assert!(outlives(&wrong), "'a: 'static is not given: {wrong:?}");
    let control = read_field(false, false, true, sig, "name");
    std::println!("field_regions name, no row: {control:?}");
    assert!(outlives(&control), "the control finds the coarse relation: {control:?}");
}

/// A row past the ADT's lifetimes is refused by name, never related another way (rule 3).
#[test]
fn a_misaligned_row_is_refused() {
    let got = read_field(true, true, false, "fn get<'f, 'a>(x: &B<'f, 'a>) -> &'a S { x.held }", "held");
    std::println!("field_regions misaligned: {got:?}");
    assert!(matches!(got, Err(ref r) if matches!(r.form, Form::FieldRegionsUnaligned { .. })), "{got:?}");
}

/// `fn mk<'f, 'a>(file: &'f S, held: &'a S) -> B<'f, 'a> { B { file: <one>, held: <other> } }`,
/// the two values swapped when `swap`.
fn make(regions: bool, swap: bool) -> Result<vec::Vec<String>, Refusal> {
    let mut w = World::new();
    w.regions = regions;
    let (rs, bt) = (w.rs, w.bt);
    let f = w.func("mk", "fn mk<'f, 'a>(file: &'f S, held: &'a S) -> B<'f, 'a> { B { file, held } }", &[rs, rs], bt);
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
    let control = make(false, false);
    std::println!("field_regions literal, no row: {control:?}");
    assert!(outlives(&control), "the control finds the coarse relation: {control:?}");
}
