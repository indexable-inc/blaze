//! A closure's requirement found in its OWNER at the closure's range (the NLL RFC, "Closures":
//! what a closure's body needs among its captures' regions and cannot prove itself is proven
//! by its creator; lane B8, NEVER COMPILED when written, 2026-11-18). The requirement is given
//! as the caller gives it ([`Facts::closure_requirements`], from the closure's own flow mapped
//! through closure.rs `requirement`); the owner emits it at the closure expression.
//!
//! `fn f<'a, 'b>(a: &'a S, b: &'b S) { let c = || two(a, b); c(); }`, with the closure's
//! body needing its captured `a`'s referent region to be `'static` (as `let _: &'static S = a;`
//! in its body would): the closure value holds, in capture order, `a` by reference
//! (`&'c1 &'x1 S`: positions 0 and 1) and `b` by reference (`&'c2 &'x3 S`: positions 2 and 3),
//! so the requirement is `Capture(1)` outlives `'static`.
//!
//! WHY NOT `Capture(3)` outlives `Capture(1)` (the first fixture, which found nothing, rightly):
//! a shared capture of `a: &'a S` is a reborrow into the closure value's field, which is
//! covariant in the referent, so it gives only `'a: 'x1` (lib.rs `borrow`, as rustc's upvar
//! subtyping in `Rvalue::Aggregate`), and `'b: 'x3`. A requirement `'x3: 'x1` among two
//! regions bounded only from above is met by both being empty: rustc accepts that owner. A
//! requirement bites only when one end is fixed: `'static`, a universal, or a position tied
//! both ways (an invariant one, `&mut &'a S`).

use blaze_typeck::fixture::Bb;
use blaze_typeck::regions::Region;

use crate::callee_value_tests::{Lane, no_clauses, world_with};
use crate::region_fixture::{FnHead, P0, P1};
use crate::tests::findings_laid;
use crate::{Need, Plant, Requirement};

/// The captured `a`'s referent outlives `'static` (the module doc).
const A_STATIC: [Requirement; 1] = [Requirement { from: Need::Capture(1), into: Need::Static }];

/// The owner `fn f<'a, 'b>(..)` cannot prove `'a: 'static`: `UnprovenOutlives` at the
/// closure's range (rustc: "lifetime may not live long enough", pointing at the closure). Its
/// positive, `fn f<'b>(a: &'static S, ..)`, gives it: no finding. CONTROL (rule 23):
/// `Plant::NoRequirements` drops the emitted requirement and the negative is accepted.
/// Prints: `requirement unstated: ["borrow UnprovenOutlives .. span: Span { lo: L, hi: H } ..`
/// (`L..H` the closure's range), `requirement stated: []` and `requirement unstated, planted
/// NoRequirements: []`.
#[test]
fn a_closure_requirement_is_the_owners_finding_at_the_closure() {
    let mut owners = None;
    let w = world_with(|fx, _st, rs, unit| {
        let two = fx.func("two", 0, &[rs, rs], unit, no_clauses());
        owners = Some((two, fx.func("f", 0, &[rs, rs], unit, no_clauses())));
    });
    let (two, f) = owners.expect("declared");
    // `fn two(_: &S, _: &S)`: two elided input lifetimes `P0`, `P1`, both late-bound.
    let two_head = FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[w.rs, w.rs], output: w.unit, slots: &[P0, P1] };
    // `fn f<'a, 'b>(a: &'a S, b: &'b S)`: `'a` is `P0`, `'b` `P1`, both late-bound (in the
    // inputs, named by no bound): `[P0, P1]`.
    let unstated = FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[w.rs, w.rs], output: w.unit, slots: &[P0, P1] };
    // `fn f<'b>(a: &'static S, b: &'b S)`: `'b` is `P0`, late-bound: `[Static, P0]`.
    let stated_head = FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rs, w.rs], output: w.unit, slots: &[Region::Static, P0] };
    for (_text, head, stated) in [
        ("fn f<'a, 'b>(a: &'a S, b: &'b S) { let c = || two(a, b); c(); }", unstated, false),
        ("fn f<'b>(a: &'static S, b: &'b S) { let c = || two(a, b); c(); }", stated_head, true),
    ] {
        let mut b = Bb::new();
        let (pa, pb) = (b.param("a"), b.param("b"));
        let callee = b.path(&["two"]);
        let (la, lb) = (b.local(pa), b.local(pb));
        let inner = b.call(callee, &[la, lb]);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let lc = b.local(pc);
        let call = b.call(lc, &[]);
        let st2 = b.semi(call);
        let root = b.block(&[st1, st2], None);
        let mut facts = Lane::of(&w);
        facts.wide.add(two, two_head);
        facts.wide.add(f, head);
        facts.closure = Some(cl);
        facts.reqs = &A_STATIC;
        let got = findings_laid(&w.fx, &b, f, root, &facts, Plant::None);
        std::println!("requirement {}: {got:?}", if stated { "stated" } else { "unstated" });
        let got = got.expect("lowered");
        if stated {
            assert!(got.is_empty(), "{got:?}");
            continue;
        }
        let range = crate::range_of(&b.view(f, root), cl);
        let mut at: blaze_own::FixedString<64> = blaze_own::FixedString::new();
        core::fmt::Write::write_fmt(&mut at, format_args!("span: Span {{ lo: {}, hi: {} }}", range.start, range.end)).expect("fits");
        let at = at.text().expect("fits");
        assert!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives") && l.contains(at)), "{got:?}; the closure at {at}");
        let planted = findings_laid(&w.fx, &b, f, root, &facts, Plant::NoRequirements);
        std::println!("requirement unstated, planted NoRequirements: {planted:?}");
        assert!(planted.as_ref().is_ok_and(|p| p.is_empty()), "{planted:?}");
    }
}
