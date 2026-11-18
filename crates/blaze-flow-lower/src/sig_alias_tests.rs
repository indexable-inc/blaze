//! Piece sig-paths (lane B9): a written type path is read for what it RESOLVES to, a type alias
//! expanded (sig/alias.rs), never lined up with the erased expansion as if it named it.
//!
//! Sources: the Reference, "Type aliases" ("A type alias defines a new name for an existing
//! type"); "Lifetime elision" (its examples, and the rules count the lifetimes a parameter's
//! type has, so an alias written bare elides its own lifetime parameters); "Lifetime bounds"
//! (`'static` outlives every lifetime: a local's borrow passed where `'static` is demanded is
//! E0597).
//!
//! No heap is made here: the readers take the shared test allocator of sig_assoc_tests.rs, and
//! the lowering runs through tests.rs `findings_with`.

use std::vec;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ItemKey, Mutbl, Name, PNode, PTy};

use crate::sig::{AliasScope, NoAliases, PathIs, PathWhy, SigWorld, read_signature_with};
use crate::sig_assoc_tests::alloc;
use crate::tests::findings_with;
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Region, SigBuf, SigLife, SigRefusal};

/// The aliases of every test, by name: what `type_path` answers for a last segment.
const ALIASES: &[(&str, &str)] = &[
    ("L", "pub type L = Hold<'static>;"),
    ("R", "type R<'a> = &'a Row<'a>;"),
    ("R2", "type R2<'b> = R<'b>;"),
    ("V", "type V<T> = Wrap<&'static T>;"),
    ("Sw", "type Sw<'p, 'q> = Two<'q, 'p>;"),
];

/// An ADT of the fixture: its name, key and lifetime count.
type AdtRow = (&'static str, ItemKey, u32);

/// A world of fixed tables: the ADTs by name and [`ALIASES`]; a name in neither is unresolved.
struct World<'s> {
    adts: &'s [AdtRow],
}

impl SigWorld for World<'_> {
    fn adt_lifetimes(&self, adt: ItemKey) -> Option<u32> {
        self.adts.iter().find(|a| a.1 == adt).map(|a| a.2)
    }
    fn type_path(&self, _: Option<AliasScope>, _: bool, segments: &[&str]) -> PathIs<'_> {
        let Some(last) = segments.last() else { return PathIs::Unresolved };
        if let Some(&(_, text)) = ALIASES.iter().find(|a| a.0 == *last) {
            return PathIs::Alias { scope: AliasScope(1), text };
        }
        match self.adts.iter().find(|a| a.0 == *last) {
            Some(&(_, k, _)) => PathIs::Adt(k),
            None => PathIs::Unresolved,
        }
    }
}

/// The fixture: `S`, `Hold<'a>`, `Row<'a>`, `Wrap<T>`, `Two<'a, 'b>`, `Pair2<T, U = ()>`.
struct Base {
    fx: Fx,
    adts: [AdtRow; 6],
    s: PTy,
    rs: PTy,
    hold: PTy,
    row: PTy,
    two: PTy,
    unit: PTy,
}

fn base() -> Base {
    let mut fx = Fx::new();
    let sk = fx.strukt("S", 0, &[]);
    let hk = fx.strukt("Hold", 0, &[]);
    let rk = fx.strukt("Row", 0, &[]);
    let wk = fx.strukt("Wrap", 1, &[]);
    let tk = fx.strukt("Two", 0, &[]);
    let pk = fx.strukt("Pair2", 2, &[]);
    let s = fx.adt(sk, &[]);
    let rs = fx.r(Mutbl::Not, s);
    let hold = fx.adt(hk, &[]);
    let row = fx.adt(rk, &[]);
    let two = fx.adt(tk, &[]);
    let unit = fx.unit();
    let adts = [("S", sk, 0), ("Hold", hk, 1), ("Row", rk, 1), ("Wrap", wk, 0), ("Two", tk, 2), ("Pair2", pk, 0)];
    Base { fx, adts, s, rs, hold, row, two, unit }
}

fn key(adts: &[AdtRow], name: &str) -> ItemKey {
    adts.iter().find(|a| a.0 == name).map(|a| a.1).expect("an ADT of the fixture")
}

/// What a reading gives: its regions and input ends.
type Read = (vec::Vec<Region>, vec::Vec<u32>);

/// `text` read in `world`: its regions and ends, or the refusal.
fn read_in(fx: &Fx, world: &dyn SigWorld, text: &str, inputs: &[PTy], output: PTy) -> Result<Read, SigRefusal> {
    let mut buf = SigBuf::new_in(alloc());
    read_signature_with(text, fx.t.view(), inputs, output, world, &mut buf, alloc())?;
    Ok((buf.regions.iter().copied().collect(), buf.ends.iter().copied().collect()))
}

/// THE CAUSE, read: `type L = Hold<'static>;`. `fn scope(x: &S) -> L` returns `'static`
/// (`[Anon(0)]` in, `[Static]` out), and `fn keep(h: L)` demands it (`[Static]`). Control (the
/// cause back: the world knows no alias, the reading before this piece): the output takes the
/// input's region and the input is a fresh one, `[Anon(0), Anon(0)]` and `[Anon(0)]`.
/// Prints `an_alias_to_a_static_type_is_static: scope [Anon(0), Static], keep [Static]; as an
/// ADT: [Anon(0), Anon(0)], [Anon(0)]`.
#[test]
fn an_alias_to_a_static_type_is_static() {
    let mut b = base();
    b.fx.finish();
    let world = World { adts: &b.adts };
    let scope = read_in(&b.fx, &world, "fn scope(x: &S) -> L { todo() }", &[b.rs], b.hold).expect("read");
    let keep = read_in(&b.fx, &world, "fn keep(h: L) {}", &[b.hold], b.unit).expect("read");
    let adts = b.adts;
    let lifetimes = move |k: ItemKey| adts.iter().find(|a| a.1 == k).map(|a| a.2);
    let old = NoAliases(&lifetimes);
    let scope_old = read_in(&b.fx, &old, "fn scope(x: &S) -> L { todo() }", &[b.rs], b.hold).expect("read");
    let keep_old = read_in(&b.fx, &old, "fn keep(h: L) {}", &[b.hold], b.unit).expect("read");
    std::println!("an_alias_to_a_static_type_is_static: scope {:?}, keep {:?}; as an ADT: {:?}, {:?}", scope.0, keep.0, scope_old.0, keep_old.0);
    assert_eq!(scope, (vec![Region::Anon(0), Region::Static], vec![1]));
    assert_eq!(keep, (vec![Region::Static], vec![1]));
    assert_eq!(scope_old.0, [Region::Anon(0), Region::Anon(0)]);
    assert_eq!(keep_old.0, [Region::Anon(0)]);
}

/// The other alias forms, each against its erased expansion:
/// - `type R<'a> = &'a Row<'a>;` (an alias with its own lifetime parameter): `fn r<'x>(a:
///   R<'x>) -> R<'x>` is `'x` at all four positions; written bare, `R` elides `'a`: ONE fresh
///   lifetime, substituted twice, and that one decides the output.
/// - `type R2<'b> = R<'b>;` (an alias of an alias): `R2<'x>` is `[Named(0), Named(0)]`.
/// - `type V<T> = Wrap<&'static T>;` (a generic alias): `V<&'x S>` is `[Static, Named(0)]`, and
///   `-> &S` takes `'x`: the alias's `'static` is no lifetime the parameter's text uses.
/// - `type Sw<'p, 'q> = Two<'q, 'p>;`: `Sw<'a, 'b>` is `Two<'b, 'a>`, `[Named(1), Named(0)]`;
///   lined up as if it named `Two` it is SWAPPED, `[Named(0), Named(1)]` (control).
/// - `Pair2<&'x S>` (its second parameter defaulted to `()`): `[Named(0)]`; `usize` beside it
///   against its primitive node is read (no item has its name).
/// - a path the world does not resolve is refused by name, `Path { why: Unresolved }`.
/// Prints `every_alias_form_reads_its_expansion: <form> <reading>`, one line per case.
#[test]
fn every_alias_form_reads_its_expansion() {
    let mut b = base();
    let (rs, row, two, unit) = (b.rs, b.row, b.two, b.unit);
    let r = b.fx.r(Mutbl::Not, row);
    let rrs = b.fx.r(Mutbl::Not, rs);
    let v = b.fx.adt(key(&b.adts, "Wrap"), &[rrs]);
    let p2 = b.fx.adt(key(&b.adts, "Pair2"), &[rs, unit]);
    let us = b.fx.usize();
    b.fx.finish();
    let world = World { adts: &b.adts };
    let n = Region::Named;
    let cases: [(&str, &[PTy], PTy, Read); 6] = [
        ("fn r<'x>(a: R<'x>) -> R<'x> { a }", &[r], r, (vec![n(0), n(0), n(0), n(0)], vec![2])),
        ("fn r(a: R) -> R { a }", &[r], r, (vec![Region::Anon(0); 4], vec![2])),
        ("fn r2<'x>(a: R2<'x>) {}", &[r], unit, (vec![n(0), n(0)], vec![2])),
        ("fn v<'x>(a: V<&'x S>) -> &S { todo() }", &[v], rs, (vec![Region::Static, n(0), n(0)], vec![2])),
        ("fn sw<'a, 'b>(t: Sw<'a, 'b>) {}", &[two], unit, (vec![n(1), n(0)], vec![2])),
        ("fn p<'x>(p: Pair2<&'x S>, k: usize) {}", &[p2, us], unit, (vec![n(0)], vec![1, 1])),
    ];
    for (text, inputs, output, want) in cases {
        let got = read_in(&b.fx, &world, text, inputs, output);
        std::println!("every_alias_form_reads_its_expansion: `{text}` {got:?}");
        assert_eq!(got.as_ref(), Ok(&want), "{text}");
    }
    let adts = b.adts;
    let lifetimes = move |k: ItemKey| adts.iter().find(|a| a.1 == k).map(|a| a.2);
    let swapped = read_in(&b.fx, &NoAliases(&lifetimes), "fn sw<'a, 'b>(t: Sw<'a, 'b>) {}", &[two], unit);
    std::println!("every_alias_form_reads_its_expansion: Sw as if it named Two {swapped:?}");
    assert_eq!(swapped.map(|g| g.0), Ok(vec![n(0), n(1)]));
    let nope = read_in(&b.fx, &world, "fn nope(x: Nope) {}", &[two], unit);
    std::println!("every_alias_form_reads_its_expansion: Nope {nope:?}");
    assert!(matches!(nope, Err(SigRefusal::Path { why: PathWhy::Unresolved, .. })), "{nope:?}");
}

/// The Reference's elision examples ("Lifetime elision") through the resolving reader:
/// `fn print(s: &S)` is `[Anon(0)]`; `fn substr(s: &S, until: usize) -> &S` gives the output the
/// one input lifetime; `fn new(buf: &S) -> Hold<'_>` and `-> Hold` both take the input's;
/// `fn frob(s: &S, t: &S) -> &S` is "ILLEGAL", refused `ElisionUndecided`.
/// Prints `the_references_elision_examples: <text> <reading>`, one line per case.
#[test]
fn the_references_elision_examples() {
    let mut b = base();
    let (rs, hold, unit) = (b.rs, b.hold, b.unit);
    let us = b.fx.usize();
    b.fx.finish();
    let world = World { adts: &b.adts };
    let a = Region::Anon;
    let cases: [(&str, &[PTy], PTy, Read); 4] = [
        ("fn print(s: &S) {}", &[rs], unit, (vec![a(0)], vec![1])),
        ("fn substr(s: &S, until: usize) -> &S { s }", &[rs, us], rs, (vec![a(0), a(0)], vec![1, 1])),
        ("fn new(buf: &S) -> Hold<'_> { todo() }", &[rs], hold, (vec![a(0), a(0)], vec![1])),
        ("fn new(buf: &S) -> Hold { todo() }", &[rs], hold, (vec![a(0), a(0)], vec![1])),
    ];
    for (text, inputs, output, want) in cases {
        let got = read_in(&b.fx, &world, text, inputs, output);
        std::println!("the_references_elision_examples: `{text}` {got:?}");
        assert_eq!(got.as_ref(), Ok(&want), "{text}");
    }
    let frob = read_in(&b.fx, &world, "fn frob(s: &S, t: &S) -> &S { s }", &[rs, rs], rs);
    std::println!("the_references_elision_examples: frob {frob:?}");
    assert!(matches!(frob, Err(SigRefusal::ElisionUndecided { .. })), "{frob:?}");
}

/// The lowering's facts: the ADTs' lifetime counts and the signatures read.
struct Sigs<'s> {
    adts: &'s [AdtRow],
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
}

impl Facts for Sigs<'_> {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = self.adts.iter().find(|a| a.1 == k).map_or(0, |a| a.2);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
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
/// place, no finding. Control (the cause removed: `keep` read with no alias known, as before
/// this piece, `h: L` a fresh lifetime): `bad` has NO finding, which is what blaze accepted.
/// Prints `an_alias_input_demands_static_of_its_argument: <case> <findings>`, three lines.
#[test]
fn an_alias_input_demands_static_of_its_argument() {
    let mut b = base();
    let (st, rs, hold, unit) = (b.s, b.rs, b.hold, b.unit);
    let mk = b.fx.func("mk", 0, &[rs], hold, vec![]);
    let keep = b.fx.func("keep", 0, &[hold], unit, vec![]);
    let bad = b.fx.func("bad", 0, &[st], unit, vec![]);
    b.fx.finish();
    let adts = b.adts;
    let lifetimes = move |k: ItemKey| adts.iter().find(|a| a.1 == k).map(|a| a.2);
    let old = NoAliases(&lifetimes);
    let world = World { adts: &adts };
    let read = |w: &dyn SigWorld, text: &str, inputs: &[PTy], output: PTy| -> SigBuf<Alloc<'static>> {
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature_with(text, b.fx.t.view(), inputs, output, w, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        buf
    };
    let cases: [(&str, &dyn SigWorld, &str, bool); 3] = [
        ("alias to 'static", &world, "fn keep(h: L) {}", true),
        ("Hold<'_>", &world, "fn keep(h: Hold<'_>) {}", false),
        ("control, no alias known", &old, "fn keep(h: L) {}", false),
    ];
    for (case, w, keep_text, found) in cases {
        let facts = Sigs {
            adts: &adts,
            sigs: vec![
                (mk, read(w, "fn mk(s: &S) -> Hold<'_> { todo() }", &[rs], hold)),
                (keep, read(w, keep_text, &[hold], unit)),
                (bad, read(w, "fn bad(s: S) { keep(mk(&s)); }", &[st], unit)),
            ],
        };
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
        let lines = findings_with(&b.fx, &body, bad, root, &facts, Plant::None).expect("lowered");
        std::println!("an_alias_input_demands_static_of_its_argument: {case} {lines:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow")), found, "{case}: {lines:?}");
        assert_eq!(lines.is_empty(), !found, "{case}: {lines:?}");
    }
}

/// The Grant shape, the 24 false `UnprovenOutlives` of bstd: `impl<'g> Granting<'g> { fn
/// scope(&self, kind: &'static str) -> Result<(&'g Scope<LaunchAlloc>, &'g Io), Want> }` with
/// `type LaunchAlloc = SharedAlloc<'static>;`. With the alias known, `Scope<LaunchAlloc>`'s
/// lifetime is `Static`, not the elided input's: the output demands nothing of the receiver.
/// Here as a free fn over `Hold` (`L`): `fn scope<'g>(from: &Two<'g, 'g>, kind: &'static str)
/// -> (&'g Wrap<L>, &'g S)` is in `[Anon(0), Named(0), Named(0) | Static]`, out `[Named(0),
/// Static, Named(0)]`. Control (as if `L` named `Hold`): the `Static` inside `Wrap<L>` is
/// `Anon(0)`, the only elided input's, which made the caller prove `'from: 'g`.
/// Prints `the_grant_shape_reads_static: <reading>; as an ADT <reading>`.
#[test]
fn the_grant_shape_reads_static() {
    let mut b = base();
    let (s, two, hold) = (b.s, b.two, b.hold);
    let from = b.fx.r(Mutbl::Not, two);
    let str_ = b.fx.ty(PNode::Str);
    let kind = b.fx.r(Mutbl::Not, str_);
    let wrap = b.fx.adt(key(&b.adts, "Wrap"), &[hold]);
    let rw = b.fx.r(Mutbl::Not, wrap);
    let rs2 = b.fx.r(Mutbl::Not, s);
    let out = b.fx.tuple(&[rw, rs2]);
    b.fx.finish();
    let text = "fn scope<'g>(from: &Two<'g, 'g>, kind: &'static str) -> (&'g Wrap<L>, &'g S) { todo() }";
    let world = World { adts: &b.adts };
    let got = read_in(&b.fx, &world, text, &[from, kind], out);
    let adts = b.adts;
    let lifetimes = move |k: ItemKey| adts.iter().find(|a| a.1 == k).map(|a| a.2);
    let old = read_in(&b.fx, &NoAliases(&lifetimes), text, &[from, kind], out);
    std::println!("the_grant_shape_reads_static: {got:?}; as an ADT {old:?}");
    let n = Region::Named(0);
    assert_eq!(got, Ok((vec![Region::Anon(0), n, n, Region::Static, n, Region::Static, n], vec![3, 4])));
    // The control: read as if `L` named an ADT with one unwritten lifetime, that position is
    // never `'static`. Here its elision is undecided (first run, 2026-11-18: three input
    // lifetimes and no `self`: the Reference, "Lifetime elision"); under a `&self` it takes
    // `self`'s, the false `'from: 'g` of the census.
    assert_eq!(old, Err(SigRefusal::ElisionUndecided { at: 66 }));
}
