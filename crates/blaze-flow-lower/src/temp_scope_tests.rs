//! Where a temporary's storage ends, from two causes of tempo's false borrow notes
//! (c2135wave69 borrow-notes.txt):
//!
//! 1. The edition of a block's OWN `{` decides its tail's temporaries (rustc `resolve_block`
//!    reads `blk.span.edition()`; the Reference, "Temporary scopes", 2024: "the tail expression
//!    of a block" is a temporary scope; before 2024 the tail's temporaries live to the end of the
//!    enclosing statement). A block a macro wrote is in the macro's crate's edition (blaze-expand
//!    def.rs `MacroDefinition::mark`), so tracing's `valueset!` block (2018) in a 2024 body
//!    keeps its tail's temporaries to the end of the call statement. Before the fix the lowering
//!    read the BODY's edition and found 696 false E0597/E0716 at tracing's macros.
//! 2. A `let` whose pattern binds by `ref`, or holds a `ref` binding under a struct, tuple,
//!    tuple-struct or slice pattern, extends its initializer's temporary to the end of the
//!    block (the Reference, "Extending based on patterns"; rustc `is_binding_pat`). Before the
//!    fix `let Fixture { ref signer, .. } = PermitFixture::new();` ended the fixture at the `;`.
//!
//! 3. The result temporary of a block, `match` or `if` (lib.rs `settle_dest`): its value drop is
//!    scheduled once its value is complete (rustc `as_temp`), so at the statement's end it is
//!    dropped BEFORE the temporaries it was made from. Before the fix it was dropped where it was
//!    made, after them: a value with drop glue (every non-`Copy` ADT is taken to have it,
//!    blaze-ladder borrows.rs `needs_drop: !copy_taken`) borrowing a 2018 block's tail
//!    temporary was drop-live past that temporary's storage end: tracing's
//!    `dispatch(&valueset!(..))`, 523 distinct false E0505/E0597 on tempo.
//!
//! The world: `struct S;` (no drop glue), `fn mk() -> S`, `fn mk1() -> (S,)`,
//! `fn make(x: &S) -> &S`, `fn look(x: &S)`, the body `fn f()`; and `struct W<'w>(&'w S)`
//! (covariant, taken to need a drop as the ladder's facts take every non-`Copy` ADT, tracing's
//! `ValueSet`), `fn vs(x: &S) -> W<'_>`, `fn look_w(v: &W<'_>)`, `fn look_w_mut(v: &mut W<'_>)`,
//! `impl W<'_> { fn peek(&self) }`.

use std::format;
use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Edition, ExprId, ItemKey, Mutbl, Name};

use crate::region_fixture::{Decls, FnHead};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Refusal, Region, Variance};

/// The facts: `W` has one lifetime, is covariant in it and needs a drop; every other ADT (`S`)
/// has no lifetime and no drop glue.
struct World {
    w: ItemKey,
}

const CO: [Variance; 1] = [Variance::Co];

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
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let w = k == self.w;
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: w, lifetimes: u32::from(w), union: false, is_enum: false, is_box: false })
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        if adt == self.w { Some(&CO) } else { None }
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

/// The world's items, declared and with their heads laid (D148): every lifetime elided, so
/// `make`'s output is its input's (the Reference, "Lifetime elision").
struct Items {
    fx: Fx,
    heads: Decls,
    f: ItemKey,
    w: ItemKey,
}

fn items() -> Items {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let tup = fx.tuple(&[st]);
    let unit = fx.unit();
    let w = fx.strukt("W", 0, &[]);
    let wt = fx.adt(w, &[]);
    let rw = fx.r(Mutbl::Not, wt);
    let rwm = fx.r(Mutbl::Mut, wt);
    let mk = fx.func("mk", 0, &[], st, vec![]);
    let mk1 = fx.func("mk1", 0, &[], tup, vec![]);
    let make = fx.func("make", 0, &[rs], rs, vec![]);
    let look = fx.func("look", 0, &[rs], unit, vec![]);
    let vs = fx.func("vs", 0, &[rs], wt, vec![]);
    let look_w = fx.func("look_w", 0, &[rw], unit, vec![]);
    let look_w_mut = fx.func("look_w_mut", 0, &[rwm], unit, vec![]);
    let imp = fx.imp("impl W<'_>", 0, wt, None, &[], vec![]);
    let peek = fx.impl_fn(imp, "peek", true, &[rw], unit);
    let f = fx.func("f", 0, &[], unit, vec![]);
    fx.finish();
    let mut heads = Decls::new();
    heads.adt(w, 1);
    let none: &[u32] = &[];
    let one: &[u32] = &[0];
    let two: &[u32] = &[0, 1];
    let (p0, p1) = (Region::Param(0), Region::Param(1));
    heads.func(mk, FnHead { lifetimes: 0, late: none, outlives: &[], inputs: &[], output: st, slots: &[] });
    heads.func(mk1, FnHead { lifetimes: 0, late: none, outlives: &[], inputs: &[], output: tup, slots: &[] });
    heads.func(make, FnHead { lifetimes: 1, late: one, outlives: &[], inputs: &[rs], output: rs, slots: &[p0, p0] });
    heads.func(look, FnHead { lifetimes: 1, late: one, outlives: &[], inputs: &[rs], output: unit, slots: &[p0] });
    // `fn vs(x: &S) -> W<'_>`: `[Param(0), Param(0)]`.
    heads.func(vs, FnHead { lifetimes: 1, late: one, outlives: &[], inputs: &[rs], output: wt, slots: &[p0, p0] });
    // `fn look_w(v: &W<'_>)` and `fn look_w_mut(v: &mut W<'_>)`: `[Param(0), Param(1)]`.
    heads.func(look_w, FnHead { lifetimes: 2, late: two, outlives: &[], inputs: &[rw], output: unit, slots: &[p0, p1] });
    heads.func(look_w_mut, FnHead { lifetimes: 2, late: two, outlives: &[], inputs: &[rwm], output: unit, slots: &[p0, p1] });
    // `fn peek(&self)` of `impl<'w> W<'w>`: the impl's `'w` `Param(0)` (early), the elided
    // `&self` `Param(1)` (late): `&W<'w>` `[Param(1), Param(0)]` (sig_assoc_tests.rs's `get`).
    heads.func(peek, FnHead { lifetimes: 2, late: &[1], outlives: &[], inputs: &[rw], output: unit, slots: &[p1, p0] });
    heads.func(f, FnHead { lifetimes: 0, late: none, outlives: &[], inputs: &[], output: unit, slots: &[] });
    Items { fx, heads, f, w }
}

/// The findings of `root`, the body of `f`, read as written in `edition`.
fn run(it: &Items, b: &Bb, root: ExprId, edition: Edition) -> Result<vec::Vec<String>, Refusal> {
    run_planted(it, b, root, edition, Plant::None)
}

/// As [`run`], under `plant`.
fn run_planted(it: &Items, b: &Bb, root: ExprId, edition: Edition, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let mut view = b.view(it.f, root);
    view.edition = edition;
    crate::region_fixture::run(&it.fx, &view, &World { w: it.w }, &it.heads, plant).map(|r| r.lines)
}

fn found(got: &Result<vec::Vec<String>, Refusal>) -> bool {
    match got {
        Ok(lines) => lines.iter().any(|l| l.starts_with("borrow Invalidated")),
        Err(r) => panic!("refused, not checked: {r:?}"),
    }
}

/// `fn f() { look({ make(&mk()) }); }`, the block marked as written in `block` (None: not
/// marked, the body's edition), the body in `body`.
fn tail_block(body: Edition, block: Option<Edition>) -> Result<vec::Vec<String>, Refusal> {
    let it = items();
    let mut b = Bb::new();
    let (look, make, mk) = (b.path(&["look"]), b.path(&["make"]), b.path(&["mk"]));
    let temp = b.call(mk, &[]);
    let bor = b.borrow(Mutbl::Not, temp);
    let tail = b.call(make, &[bor]);
    let blk = b.block(&[], Some(tail));
    if let Some(ed) = block {
        // The runs are made against the body's edition, so it is set first (`run` sets it again).
        b.body_edition(body);
        b.expr_edition(blk, ed);
    }
    let call = b.call(look, &[blk]);
    let st = b.semi(call);
    let root = b.block(&[st], None);
    run(&it, &b, root, body)
}

/// `look({ make(&mk()) })` in a 2024 body: the unmarked block's tail temporary `mk()` ends at
/// the block's `}` while `look` still reads the reference made from it (rustc E0716, a true
/// finding); the same block marked 2021 (a macro of that edition wrote it) keeps the temporary
/// to the end of the statement: no finding. CONTROL: a 2021 body, unmarked, no finding (the
/// body's edition still decides an unmarked block).
#[test]
fn a_macro_blocks_tail_temporaries_follow_the_macros_edition() {
    let unmarked = tail_block(Edition::E2024, None);
    let marked = tail_block(Edition::E2024, Some(Edition::E2021));
    let old_body = tail_block(Edition::E2021, None);
    std::println!("tail_block 2024 unmarked: {unmarked:?}\ntail_block 2024 marked 2021: {marked:?}\ntail_block 2021 unmarked: {old_body:?}");
    assert!(found(&unmarked), "a 2024 block's tail temporary ends at its `}}`: {unmarked:?}");
    assert!(!found(&marked), "a 2021 macro's block keeps its tail temporary to the statement's end: {marked:?}");
    assert!(!found(&old_body), "a 2021 body's unmarked block keeps its tail temporary: {old_body:?}");
}

#[derive(Clone, Copy, Debug)]
enum Let {
    /// `let ref a = mk(); look(a);`
    Ref,
    /// `let (ref a,) = mk1(); look(a);`
    TupleRef,
    /// `let a = make(&mk()); look(a);`: the temporary is a call's argument, not extended
    /// (rustc E0716, a true finding).
    Arg,
}

fn let_body(form: Let) -> Result<vec::Vec<String>, Refusal> {
    let it = items();
    let mut b = Bb::new();
    let look = b.path(&["look"]);
    // The pattern, the binding `a` in it, the initializer.
    let (pat, binding, init) = match form {
        Let::Ref => {
            let mk = b.path(&["mk"]);
            let a = b.bind_ref("a", Mutbl::Not);
            (a, a, b.call(mk, &[]))
        }
        Let::TupleRef => {
            let mk1 = b.path(&["mk1"]);
            let a = b.bind_ref("a", Mutbl::Not);
            (b.tuple_pat(&[a]), a, b.call(mk1, &[]))
        }
        Let::Arg => {
            let (make, mk) = (b.path(&["make"]), b.path(&["mk"]));
            let temp = b.call(mk, &[]);
            let bor = b.borrow(Mutbl::Not, temp);
            let a = b.bind("a");
            (a, a, b.call(make, &[bor]))
        }
    };
    let st1 = b.let_(pat, None, init);
    let la = b.local(binding);
    let call = b.call(look, &[la]);
    let st2 = b.semi(call);
    let root = b.block(&[st1, st2], None);
    run(&it, &b, root, Edition::E2021)
}

/// `let ref a = mk();` and `let (ref a,) = mk1();` extend the temporary to the block's end:
/// `look(a)` after them is no finding. The twin, `let a = make(&mk());` (the temporary an
/// argument, not extended), is E0716 at the `;`: found.
#[test]
fn a_ref_pattern_extends_its_initializers_temporary() {
    let mut wrong = vec::Vec::new();
    for form in [Let::Ref, Let::TupleRef] {
        let got = let_body(form);
        std::println!("let {form:?}: {got:?}");
        if found(&got) {
            wrong.push(format!("{form:?}: {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "a `ref` binding's temporary ended at the `;`: {wrong:?}");
    let twin = let_body(Let::Arg);
    std::println!("let Arg: {twin:?}");
    assert!(found(&twin), "a call argument's temporary ends at the `;`: {twin:?}");
}

/// A `match` arm's temporaries (lib.rs `close_arm_temps`) and their extension by a `let`
/// (`mark_extended`, `extend_temp`).
#[derive(Clone, Copy, Debug)]
enum Arm {
    /// `look(match mk() { _ => make(&mk()) });`: `mk()` dies at the arm's end, before `look`
    /// reads the reference made from it: rustc E0716, found.
    Arg,
    /// `let x = match mk() { _ => &mk() }; look(x);`: the arm of an extending `match` is
    /// extending, so `&mk()` is an extending borrow: `mk()` lives to the block's end: clean.
    LetRef,
    /// `let x = match mk() { _ => make(&mk()) }; look(x);`: a call's argument is not extending:
    /// `mk()` dies at the arm's end: found.
    LetArg,
    /// `let x = { &mk() }; look(x);`: the block's tail is extending: clean (before
    /// `extend_temp`, `mk()` joined the BLOCK's scope and died at its `}`: a false finding).
    LetBlock,
    /// `let x = { make(&mk()) }; look(x);`: not extended: found (2021: at the `let`'s `;`).
    LetBlockArg,
}

fn arm_body(form: Arm, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let it = items();
    let mut b = Bb::new();
    let (look, mk) = (b.path(&["look"]), b.path(&["mk"]));
    let temp = b.call(mk, &[]);
    let bor = b.borrow(Mutbl::Not, temp);
    let value = match form {
        Arm::Arg | Arm::LetArg | Arm::LetBlockArg => {
            let make = b.path(&["make"]);
            b.call(make, &[bor])
        }
        Arm::LetRef | Arm::LetBlock => bor,
    };
    let value = match form {
        Arm::Arg | Arm::LetRef | Arm::LetArg => {
            let mk2 = b.path(&["mk"]);
            let scrut = b.call(mk2, &[]);
            let w = b.wild();
            b.match_(scrut, &[(w, value)])
        }
        Arm::LetBlock | Arm::LetBlockArg => b.block(&[], Some(value)),
    };
    let root = match form {
        Arm::Arg => {
            let call = b.call(look, &[value]);
            let st = b.semi(call);
            b.block(&[st], None)
        }
        Arm::LetRef | Arm::LetArg | Arm::LetBlock | Arm::LetBlockArg => {
            let x = b.bind("x");
            let st1 = b.let_(x, None, value);
            let lx = b.local(x);
            let call = b.call(look, &[lx]);
            let st2 = b.semi(call);
            b.block(&[st1, st2], None)
        }
    };
    run_planted(&it, &b, root, Edition::E2021, plant)
}

/// The Reference, "Temporary scopes": "the body expression for a match arm" is a temporary
/// scope (rustc region.rs `resolve_arm`): `look(match mk() { _ => make(&mk()) })` is E0716.
/// CONTROL (rule 23): `Plant::ArmTempsToStatement` keeps the arm's temporaries to the
/// statement's end: clean, the hole as it was.
/// TWINS, "Temporary lifetime extension": `let x = match mk() { _ => &mk() };` and `let x = {
/// &mk() };` extend `mk()` to the block's end: clean; the same with `make(&mk())` (a call's
/// argument, not extending): found.
/// Prints `arm <form>: ..` per form and `arm Arg, planted ArmTempsToStatement: Ok([])`.
#[test]
fn a_match_arms_temporaries_die_at_its_end_unless_a_let_extends_them() {
    let mut wrong = vec::Vec::new();
    for (form, want) in [(Arm::Arg, true), (Arm::LetRef, false), (Arm::LetArg, true), (Arm::LetBlock, false), (Arm::LetBlockArg, true)] {
        let got = arm_body(form, Plant::None);
        std::println!("arm {form:?}: {got:?}");
        if found(&got) != want {
            wrong.push(format!("{form:?} want found {want}: {got:?}"));
        }
    }
    let control = arm_body(Arm::Arg, Plant::ArmTempsToStatement);
    std::println!("arm Arg, planted ArmTempsToStatement: {control:?}");
    if found(&control) {
        wrong.push(format!("CONTROL: the arm's temporaries kept to the statement's end still found: {control:?}"));
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// Where the value of `{ vs(&mk()) }` is used, each a place context that puts it in a
/// temporary of the enclosing statement.
#[derive(Clone, Copy, Debug)]
enum Use {
    /// `look_w(&{ vs(&mk()) });`: tracing's `dispatch(&valueset!(..))`.
    Shared,
    /// `look_w_mut(&mut { vs(&mk()) });`.
    Mut,
    /// `{ vs(&mk()) }.peek();`: the receiver auto-referenced.
    Receiver,
}

/// `fn f() { <use of { vs(&mk()) }>; }`, read as written in `edition`, under `plant`.
fn block_value(form: Use, edition: Edition, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let it = items();
    let mut b = Bb::new();
    let (vs, mk) = (b.path(&["vs"]), b.path(&["mk"]));
    let temp = b.call(mk, &[]);
    let bor = b.borrow(Mutbl::Not, temp);
    let tail = b.call(vs, &[bor]);
    let blk = b.block(&[], Some(tail));
    let used = match form {
        Use::Shared => {
            let look_w = b.path(&["look_w"]);
            let r = b.borrow(Mutbl::Not, blk);
            b.call(look_w, &[r])
        }
        Use::Mut => {
            let look_w_mut = b.path(&["look_w_mut"]);
            let r = b.borrow(Mutbl::Mut, blk);
            b.call(look_w_mut, &[r])
        }
        Use::Receiver => b.method(blk, "peek", &[]),
    };
    let st = b.semi(used);
    let root = b.block(&[st], None);
    run_planted(&it, &b, root, edition, plant)
}

/// The Reference, "Temporary scopes": before 2024 a block's tail temporaries (`mk()`) live to
/// the end of the enclosing statement, and the block's value, borrowed (`&`, `&mut`, a
/// receiver's autoref), is a temporary of that statement too: `look_w` reads a `W` whose loan
/// of `mk()` is alive. rustc `as_temp` drops that value BEFORE `mk()`. Each form clean in 2021.
/// TWIN: in 2024 `mk()` dies at the block's `}` while the `W` made from it is still used
/// (rustc E0716): found. CONTROL (rule 23): `Plant::DestDroppedLast`, the value's drop where
/// `dest_for` made it, after `mk()`'s storage end: the 2021 forms found again.
/// Prints `block value <form> 2021: Ok([])`, `.. 2024: Ok(["borrow Invalidated ..`, and
/// `.. 2021, planted DestDroppedLast: Ok(["borrow Invalidated ..`.
#[test]
fn a_borrowed_blocks_value_is_dropped_before_its_tail_temporaries() {
    let mut wrong = vec::Vec::new();
    for form in [Use::Shared, Use::Mut, Use::Receiver] {
        let old = block_value(form, Edition::E2021, Plant::None);
        let new = block_value(form, Edition::E2024, Plant::None);
        let planted = block_value(form, Edition::E2021, Plant::DestDroppedLast);
        std::println!("block value {form:?} 2021: {old:?}\nblock value {form:?} 2024: {new:?}\nblock value {form:?} 2021, planted DestDroppedLast: {planted:?}");
        if found(&old) {
            wrong.push(format!("{form:?} 2021 found: {old:?}"));
        }
        if !found(&new) {
            wrong.push(format!("TWIN {form:?} 2024 clean: {new:?}"));
        }
        if !found(&planted) {
            wrong.push(format!("CONTROL {form:?} 2021 planted clean: {planted:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// `look_w(&match &mk() { r => vs(r) });`: a `match`'s scrutinee temporaries (`mk()`, `&mk()`)
/// live to the end of the enclosing statement in every edition (the Reference, "Temporary
/// scopes": the scrutinee of a `match` is not a temporary scope), and the match's value is
/// dropped before them: clean. TWIN: `look_w(&match mk() { a => vs(&a) });` borrows the arm's
/// binding `a`, dead at the arm's end while the `W` is used (rustc E0597): found. CONTROL:
/// `Plant::DestDroppedLast` finds the clean one again.
#[test]
fn a_borrowed_matchs_value_is_dropped_before_its_scrutinee_temporaries() {
    let body = |twin: bool, plant: Plant| {
        let it = items();
        let mut b = Bb::new();
        let (look_w, vs, mk) = (b.path(&["look_w"]), b.path(&["vs"]), b.path(&["mk"]));
        let made = b.call(mk, &[]);
        let m = if twin {
            let a = b.bind("a");
            let la = b.local(a);
            let ra = b.borrow(Mutbl::Not, la);
            let arm = b.call(vs, &[ra]);
            b.match_(made, &[(a, arm)])
        } else {
            let scrut = b.borrow(Mutbl::Not, made);
            let r = b.bind("r");
            let lr = b.local(r);
            let arm = b.call(vs, &[lr]);
            b.match_(scrut, &[(r, arm)])
        };
        let bm = b.borrow(Mutbl::Not, m);
        let call = b.call(look_w, &[bm]);
        let st = b.semi(call);
        let root = b.block(&[st], None);
        run_planted(&it, &b, root, Edition::E2021, plant)
    };
    let clean = body(false, Plant::None);
    let twin = body(true, Plant::None);
    let planted = body(false, Plant::DestDroppedLast);
    std::println!("match value: {clean:?}\nmatch value, arm binding (twin): {twin:?}\nmatch value, planted DestDroppedLast: {planted:?}");
    assert!(!found(&clean), "the scrutinee's temporaries outlive the match's value: {clean:?}");
    assert!(found(&twin), "TWIN: the arm's binding dies at the arm's end: {twin:?}");
    assert!(found(&planted), "CONTROL: the value dropped after the scrutinee's temporaries: {planted:?}");
}

/// The facts of [`a_constant_initializers_temporary_is_static`]: the temp world's, with the
/// owner's kind as `Facts::constant_initializer` states it.
struct Owner {
    world: World,
    constant: bool,
}

impl Facts for Owner {
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
        self.world.adt(k)
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.world.adt_variances(adt)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.world.callee(k)
    }
    fn constant_initializer(&self) -> bool {
        self.constant
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

/// The Reference, "Destructors", "Temporary lifetime extension": "The temporary scopes for
/// expressions in `const` and `static` items ... are extended to the end of the program", by
/// the `let` rules' extending expressions (rustc region.rs `resolve_body`). `const C: &S =
/// &mk();` (its head, as blaze-ladder heads.rs lays a const's: a fn of no inputs whose output
/// is `&'static S`, the const's elided lifetime `'static`) is clean (tempo evm
/// handler/config.rs:60). NEGATIVE TWIN, the same body as `fn c() -> &'static S { &mk() }`:
/// the temporary dies at the body's end under the returned borrow (rustc E0515): a finding.
/// The only difference is `Facts::constant_initializer`, so it is the cause (rule 23).
/// Prints `const initializer: Ok([])` and `fn body (twin): Ok([..])`.
#[test]
fn a_constant_initializers_temporary_is_static() {
    let body = |constant: bool| {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let w = fx.strukt("W", 0, &[]);
        let mk = fx.func("mk", 0, &[], st, vec![]);
        let c = fx.func("c", 0, &[], rs, vec![]);
        fx.finish();
        let mut heads = Decls::new();
        heads.func(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: st, slots: &[] });
        heads.func(c, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: rs, slots: &[Region::Static] });
        let mut b = Bb::new();
        let mkp = b.path(&["mk"]);
        let made = b.call(mkp, &[]);
        let root = b.borrow(Mutbl::Not, made);
        let facts = Owner { world: World { w }, constant };
        crate::region_fixture::findings(&fx, &b, c, root, &facts, &heads, Plant::None)
    };
    let constant = body(true);
    let twin = body(false);
    std::println!("const initializer: {constant:?}\nfn body (twin): {twin:?}");
    assert_eq!(constant, Ok(vec::Vec::new()), "a constant initializer's extending borrow is 'static");
    assert!(matches!(twin, Ok(ref lines) if !lines.is_empty()), "TWIN: a fn's temporary dies under its returned borrow: {twin:?}");
}
