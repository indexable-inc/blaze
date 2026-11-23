//! THE CALL CENSUS on a fixture (end-state.md D190 step 2; front2 call_census.rs): each body of
//! a small program checked under both call rules by `ProjectWorld::call_census`, the counts
//! read back. The tempo run is blaze-cli's (feature `call-census`); this pins the pieces it sums.
//!
//! - (b) OUTSIDE THE HUB IS ZERO: no call where both rules relate and the one rule's pairs miss
//!   one of the old's, not at a hub call (the gate's line).
//! - FINDINGS EQUAL ON FULLY RELATED BODIES: on the bodies whole under both rules where the
//!   one rule related every call, the codes counted under each rule are equal.
//! - A FINDING AND ITS TWIN: `too_short` keeps a borrow of a dead local through a generic
//!   call: E0597 or E0505 under the old paths, whole (the Reference, "References": a borrow may
//!   not outlive its referent); its twin `short_y` borrows only the argument the result does not
//!   hold: no finding under either rule.
//! - CALLEE KINDS: the census names a `for` loop's calls, a `?`'s and a method call by kind.
//! - DETERMINISTIC: the totals of the bodies censused in list order equal the totals in
//!   reverse order, summed per body (D179 GATHERED).
//!
//! The planted control (`OneRuleDropOne` counted as (b)) is blaze-flow-lower call_census.rs's
//! `the_census_counts_a_dropped_constraint_as_b`: the planted rule exists in that crate's
//! tests only.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_flow_lower::call_census::{CallClass, CalleeKind, WhyKind};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::FactsPlant;
use blaze_ladder::front2::call_census::{CODES, CallCensusTotals};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

fn pick<'a, T>(x: &'a T, y: &u8) -> &'a T {
    let _ = y;
    x
}

struct Cell {
    v: u8,
}

impl Cell {
    fn get(&self) -> &u8 {
        &self.v
    }
}

fn short_y() -> u8 {
    let a = 1u8;
    let ra = &a;
    let r;
    {
        let b = 2u8;
        r = pick(&ra, &b);
    }
    **r
}

fn too_short() -> u8 {
    let a = 1u8;
    let r;
    {
        let b = 2u8;
        let rb = &b;
        r = pick(&rb, &a);
    }
    **r
}

fn method(c: &Cell) -> &u8 {
    c.get()
}

fn looped() -> u8 {
    let mut n = 0u8;
    for i in 0..3u8 {
        n = n.wrapping_add(i);
    }
    n
}

fn looped_ref<'a>(v: &'a [u8], d: &'a u8) -> &'a u8 {
    let mut r = d;
    for x in v {
        r = x;
    }
    r
}

fn looped_ref_bad<'a>(v: &'a [u8], d: &'static u8) -> &'static u8 {
    let mut r = d;
    for x in v {
        r = x;
    }
    r
}

fn tried(x: Option<u8>) -> Option<u8> {
    let y = x?;
    Some(y)
}

fn mapped(x: Option<&(u8, u8)>) -> Option<&u8> {
    x.map(|p| &p.0)
}

fn mapped_bad<'a>(x: Option<&'a (u8, u8)>) -> Option<&'static u8> {
    x.map(|p| &p.0)
}

fn run_later<F: FnOnce() -> u8 + 'static>(f: F) -> u8 {
    f()
}

fn spawned<T: 'static>(t: T) -> u8 {
    run_later(move || {
        let _keep = t;
        1
    })
}

fn spawned_bad<T>(t: T) -> u8 {
    run_later(move || {
        let _keep = t;
        1
    })
}

trait Marker {}

impl Marker for &'static u8 {}

fn need<T: Marker>(t: T) -> u8 {
    let _ = t;
    0
}

fn needs_static(x: &'static u8) -> u8 {
    need(x)
}

fn needs_static_bad() -> u8 {
    let n = 1u8;
    need(&n)
}

fn id(x: &u8) -> &u8 {
    x
}

fn apply<T, U, F: FnOnce(T) -> U>(f: F, t: T) -> U {
    f(t)
}

fn applied(x: &u8) -> &u8 {
    apply(id, x)
}

fn applied_bad() -> &'static u8 {
    let n = 1u8;
    apply(id, &n)
}
";

#[test]
fn the_call_census_on_a_fixture() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/census/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "census", dir: "/census/", root: "/census/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies: Vec<BodyRef<'_>> = world.bodies(heap.alloc()).iter().map(|(b, _)| *b).collect();
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };

    // The totals are a small value (call_census.rs asserts at most 1 KiB): this test holds some
    // thirty of them as locals and temporaries, which overflowed the test thread's 2 MiB stack
    // at the fn's entry while the tally and the kept rows were inline (wave-1119 t68).
    assert!(core::mem::size_of::<CallCensusTotals>() <= 1024, "the totals stay small");
    // Each body alone, then summed: list order and reverse order.
    let mut forward = CallCensusTotals::new(heap.shared());
    let mut per_body: Vec<(String, CallCensusTotals)> = Vec::new();
    for b in bodies.iter() {
        let mut one = CallCensusTotals::new(heap.shared());
        world.call_census(*b, &mut one, heap.alloc());
        forward.add(&one);
        per_body.push((name_of(b), one));
    }
    let mut backward = CallCensusTotals::new(heap.shared());
    for b in bodies.iter().rev() {
        let mut one = CallCensusTotals::new(heap.shared());
        world.call_census(*b, &mut one, heap.alloc());
        backward.add(&one);
    }
    let mut report = String::new();
    forward.write(&mut report).expect("the report writes");
    println!("{report}");
    assert_eq!(forward, backward, "the census is a sum: list order and reverse order agree");

    // The gate's lines.
    assert_eq!(forward.calls().class_total(CallClass::Fewer), 0, "(b) outside the hub:\n{report}");
    assert_eq!(forward.fewer_total, 0, "no (b) row listed:\n{report}");
    // On every fully related body except `needs_static_bad` (where the one rule's empty-column
    // selection tie catches the real E0597 that the old rule misses), the two rules agree.
    for (name, t) in &per_body {
        if name == "needs_static_bad" {
            let e0597 = CODES.iter().position(|x| *x == "E0597").expect("E0597");
            assert_eq!(t.related_one[e0597], 1, "one rule catches E0597 on needs_static_bad:\n{report}");
        } else {
            assert_eq!(t.related_old, t.related_one, "findings on fully related body `{name}` agree:\n{report}");
        }
    }
    assert_eq!(forward.not_compared, [0; 3], "every fixture body compared:\n{report}");

    // A finding and its twin, read per body.
    let of = |n: &str| per_body.iter().find(|(name, _)| name == n).map(|(_, t)| t).unwrap_or_else(|| panic!("`{n}` censused"));
    let code = |c: &str| CODES.iter().position(|x| *x == c).expect("a counted code");
    let too_short = of("too_short");
    assert_eq!(too_short.whole_old, 1, "too_short checked whole under the old paths");
    assert!(too_short.findings_old[code("E0597")] + too_short.findings_old[code("E0505")] >= 1, "too_short: a dead local's borrow kept is found:\n{report}");
    let short_y = of("short_y");
    assert_eq!((short_y.whole_old, short_y.whole_one), (1, 1), "short_y checked whole under both rules:\n{report}");
    assert_eq!(short_y.findings_old.iter().sum::<u64>() + short_y.findings_one.iter().sum::<u64>(), 0, "twin: short_y clean under both rules");

    // Kinds: each written call is named by how it was written.
    let kind = |t: &CallCensusTotals, k: CalleeKind| CallClass::ALL.iter().map(|c| t.calls().by_class[c.index()][k.index()]).sum::<u64>();
    assert!(kind(of("method"), CalleeKind::Method) >= 1, "a method call is `Method`:\n{report}");
    assert!(kind(of("looped"), CalleeKind::ForDesugar) >= 1, "a `for` loop's calls are `ForDesugar`:\n{report}");
    // The gather counts the trait of every alias and of every selected row (front2 borrows.rs
    // `gather_facts_planted`): `<I as IntoIterator>::IntoIter` and `<I as Iterator>::Item`
    // name their traits as no trait reference does, and uncounted they were `HeadUnread`.
    // The counts reach the one rule through the flows' facts (borrows_nested.rs `NestedFacts`
    // forwards `trait_lifetimes` and `alias_lifetimes`): no alias of a `for` loop or a `?` is
    // refused `Count`, and no impl a row selected `HeadUnread`.
    let unread = |t: &CallCensusTotals| t.calls().why_total(WhyKind::UnselectedHeadUnread);
    let uncounted = |t: &CallCensusTotals| t.unfiled[0].iter().sum::<u64>() + t.unfiled[1].iter().sum::<u64>();
    assert_eq!(unread(of("looped")), 0, "a `for` loop's impl heads are read:\n{report}");
    assert_eq!(uncounted(of("looped")), 0, "a `for` loop's aliases are counted:\n{report}");
    assert_eq!((unread(of("tried")), uncounted(of("tried"))), (0, 0), "a `?`'s aliases are counted:\n{report}");
    // The control (rule 23): the counts left unfiled (`FactsPlant::NoTraitCounts`), the same
    // two bodies are refused by `Count` or `HeadUnread` again.
    for n in ["looped", "tried"] {
        let b = bodies.iter().find(|b| name_of(*b) == n).copied().unwrap_or_else(|| panic!("`{n}` is a body"));
        let mut planted = CallCensusTotals::new(heap.shared());
        world.call_census_planted(b, FactsPlant::NoTraitCounts, &mut planted, heap.alloc());
        let mut text = String::new();
        planted.write(&mut text).expect("the report writes");
        assert!(unread(&planted) + uncounted(&planted) >= 1, "control: `{n}` with no count filed is refused by `Count` or `HeadUnread`:\n{text}");
    }
    // A `for` loop over `&'a [u8]` relates its items through the selected impls' values
    // (`impl<'a, T> IntoIterator for &'a [T] { type Item = &'a T; .. }`, `Iter<'a, T>`'s
    // `Item`): the item is `&'a u8`. The Reference, "Iterator loops": `for PAT in EXPR` is
    // `IntoIterator::into_iter(EXPR)` then `Iterator::next` per pass, the pattern bound to
    // the `Some` value. `looped_ref` returns it at `'a`: clean. Its NEGATIVE TWIN
    // `looped_ref_bad` returns it as `&'static u8`, which rustc rejects ("lifetime may not
    // live long enough": `'a` must outlive `'static`): a finding under the one rule, whole.
    // Census wave-1119 t67 refused these calls `Malformed` (6 `for`, 2 `?`): blaze-borrow read
    // the selected impl's header without its self type while heads_regions.rs `header_trait`
    // lays it subject-first (`ExTrait(IntoIterator, [&'x [T]])`), so the alias's children
    // and the header's did not line up (now `Bad::HeaderKids` by name; its reduced test is
    // blaze-borrow `a_for_loops_into_iter_normalizes_through_a_subject_first_header`).
    let fine = of("looped_ref");
    let malformed = |t: &CallCensusTotals| t.calls().malformed_by.iter().flatten().sum::<u64>();
    assert_eq!((malformed(fine), malformed(of("looped_ref_bad")), malformed(of("tried"))), (0, 0, 0), "a `for`'s and a `?`'s impl headers line up with their aliases:\n{report}");
    assert_eq!((fine.whole_one, fine.findings_one.iter().sum::<u64>()), (1, 0), "looped_ref whole and clean under the one rule:\n{report}");
    let bad = of("looped_ref_bad");
    assert_eq!((unread(bad), uncounted(bad)), (0, 0),"looped_ref_bad's calls are read and counted:\n{report}");
    assert_eq!(bad.whole_one, 1, "looped_ref_bad checked whole under the one rule:\n{report}");
    assert!(bad.findings_one.iter().sum::<u64>() >= 1, "twin: looped_ref_bad's item returned as `'static` is a finding:\n{report}");

    // A CLOSURE ARGUMENT (census wave-1119 (b) = 191, item 1). `x.map(|p| &p.0)`: the
    // closure's signature is `Option::map`'s clause `F: FnOnce(T) -> U` (`T = &'x (u8, u8)`,
    // `U = &'y u8`), and Rust's builtin `FnOnce` candidate for a closure equates its signature
    // with the clause's arguments and `Output` (the Reference, "Closure types": a closure
    // implements `FnOnce` with its signature; rustc `confirm_closure_candidate`). The one
    // rule skipped that clause (its head column is empty) and lacked `(Arg 0, Dest 0)`;
    // one_call.rs `tie_closure_args` ties the closure's positions to the call's. `mapped`:
    // no (b), whole and clean under the one rule. Its NEGATIVE TWIN `mapped_bad` returns the
    // field as `&'static u8`, which rustc rejects ("lifetime may not live long enough"): it is
    // never accepted (a finding, or `Stricter` where the tie unified two positions).
    let fewer = |t: &CallCensusTotals| t.calls().class_total(CallClass::Fewer);
    let accepted = |t: &CallCensusTotals| t.whole_one == 1 && t.findings_one.iter().sum::<u64>() == 0;
    let mapped = of("mapped");
    assert_eq!((fewer(mapped),mapped.whole_one, mapped.findings_one.iter().sum::<u64>()), (0, 1, 0), "mapped: related, whole and clean under the one rule:\n{report}");
    assert!(!accepted(of("mapped_bad")),"twin: mapped_bad's field returned as `'static` is not accepted by the one rule:\n{report}");
    // `F: 'static` of a closure holding a `T` (`spawn_blocking`'s bound; census (b)
    // `(Param(1), Uni(Static))`): the Reference, "Closure types", a closure's outlives are its
    // captures' (rustc `compute_components` visits a closure's upvar types), so `T: 'static`
    // is asked (blaze-borrow relate.rs `components`, `Owner::body_params`). `spawned` bounds
    // `T: 'static`: clean. Its NEGATIVE TWIN `spawned_bad` does not, which rustc rejects
    // (E0310, "the parameter type `T` may not live long enough"): a finding.
    let spawned = of("spawned");
    assert_eq!((fewer(spawned), spawned.whole_one, spawned.findings_one.iter().sum::<u64>()), (0, 1, 0), "spawned: whole and clean under the one rule:\n{report}");
    let spawned_bad = of("spawned_bad");
    assert_eq!((fewer(spawned_bad), spawned_bad.whole_one), (0, 1), "spawned_bad: related and whole under the one rule:\n{report}");
    assert!(spawned_bad.findings_one.iter().sum::<u64>() >= 1, "twin: spawned_bad's `T` captured where `F: 'static` is a finding:\n{report}");
    // The control (rule 23): the tie and the captured parameters removed
    // (`FactsPlant::ClosureArgsUntied`, one_call.rs `OneRuleUntied`): `mapped` is (b) again,
    // and both twins are accepted, as before the fix.
    let mut planted_of = |n: &str| {
        let b = bodies.iter().find(|b| name_of(*b) == n).copied().unwrap_or_else(|| panic!("`{n}` is a body"));
        let mut planted = CallCensusTotals::new(heap.shared());
        world.call_census_planted(b, FactsPlant::ClosureArgsUntied, &mut planted, heap.alloc());
        let mut text = String::new();
        planted.write(&mut text).expect("the report writes");
        (planted, text)
    };
    let (untied, text) = planted_of("mapped");
    assert!(fewer(&untied) >= 1, "control: `mapped` without the tie lacks a pair again:\n{text}");
    let (untied, text) = planted_of("mapped_bad");
    assert!(accepted(&untied), "control: `mapped_bad` without the tie is accepted (the looser rule):\n{text}");
    let (untied, text) = planted_of("spawned_bad");
    assert!(accepted(&untied) && fewer(&untied) >= 1, "control: `spawned_bad` without the captured `T` is accepted and lacks `(Param, 'static)`:\n{text}");
    // THE EMPTY-COLUMN RULE (D190 step 2 (iv); blaze-borrow relate.rs `inert`, one_call.rs
    // `call_one`). A clause whose head column is empty WRITES no region, but its instance may
    // hold one, and the proof of the clause there may relate it (the Reference,
    // "Implementations": a where-clause holds for the instance the use makes). The one rule
    // skipped every such clause: LOOSER than Rust.
    // - `need(&n)` under `T: Marker`, `impl Marker for &'static u8`: rustc E0597 (`n` must be
    //   `'static`). `needs_static_bad` was accepted; now never accepted (the clause is tied
    //   through the selected impl, or refused by name where no selection is filed).
    // - `apply(id, &n)` under `F: FnOnce(T) -> U` with a fn item `F`: its builtin candidate
    //   equates `id`'s signature with `(T,)` and `U`, so `U` holds `n`'s borrow; returned as
    //   `'static`, rustc E0515. `applied_bad` was accepted; now never accepted.
    // Their TWINS (rustc accepts) are never a finding: whole and clean, or refused by name
    // until the type checker files a selection for a lifetime-less trait bound on a
    // regionful instance (scratch typeck-selection-rows-lifetimeless-bounds.md).
    // `mapped` above stays whole and clean: `Option::map`'s `F: [const] Destruct` is a trait
    // whose every proof relates nothing (`Facts::trait_relates_nothing`), and its `F:
    // FnOnce(T) -> U` is the closure's own tie.
    for n in ["needs_static_bad", "applied_bad"] {
        assert!(!accepted(of(n)), "NEGATIVE TWIN `{n}` (rustc rejects): never accepted by the one rule:\n{report}");
    }
    for n in ["needs_static", "applied"] {
        assert_eq!(of(n).findings_one.iter().sum::<u64>(), 0, "TWIN `{n}` (rustc accepts): no finding under the one rule (a refusal by name is honest):\n{report}");
    }
    assert!(kind(of("tried"), CalleeKind::TryDesugar) >= 1, "a `?`'s calls are `TryDesugar`:\n{report}");
    assert!(kind(of("short_y"), CalleeKind::Fn) >= 1, "a path call is `Fn`:\n{report}");
    assert_eq!(kind(of("short_y"), CalleeKind::Method), 0, "twin: no method call in short_y");
}
